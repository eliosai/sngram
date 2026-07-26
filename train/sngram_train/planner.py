"""Shard dispatch order: straight for one source, weighted for a blend."""

from __future__ import annotations

from .corpus import Corpus, Quota, Tallies


def schedule(corpus: Corpus, saved: dict | None, tallies: Tallies):
    """The dispatcher for a corpus: weighted when it carries a quota."""

    if corpus.quota is None:
        return Ordered(len(corpus.shards), saved)
    return Weighted(corpus, saved, tallies)


class Ordered:
    """Straight walk over the shard list."""

    def __init__(self, count: int, saved: dict | None) -> None:
        self._count = count
        self._cursor = int((saved or {}).get("cursor", 0))

    def next(self, tallies: Tallies) -> int | None:
        if self._cursor >= self._count:
            return None
        self._cursor += 1
        return self._cursor - 1

    def state(self) -> dict:
        return {"cursor": self._cursor}


class Weighted:
    """Hands out the family furthest below its target share of counted bytes."""

    def __init__(self, corpus: Corpus, saved: dict | None, tallies: Tallies) -> None:
        self._quota: Quota = corpus.quota
        self._indices = _source_indices(corpus)
        self._families = _family_sources(corpus)
        self._weights = _weights(self._families, self._quota)
        restored = saved or {}
        self._cursors = {source: 0 for source in self._indices}
        self._cursors.update(restored.get("cursors", {}))
        self._turns = dict(restored.get("turns", {}))
        self._dispatched = {f: tallies.family_done.get(f, 0) for f in self._families}
        self._taken = {s: tallies.source_done.get(s, 0) for s in self._indices}

    def next(self, tallies: Tallies) -> int | None:
        families = estimated_bytes(
            tallies.family_bytes, tallies.family_done, self._dispatched
        )
        sources = estimated_bytes(
            tallies.source_bytes, tallies.source_done, self._taken
        )
        live = [
            family
            for family in self._families
            if self._ready(family, families, sources)
        ]
        if not live:
            return None
        return self._take(pick_family(live, self._weights, families), sources)

    def state(self) -> dict:
        return {"cursors": dict(self._cursors), "turns": dict(self._turns)}

    def _ready(
        self, family: str, families: dict[str, float], sources: dict[str, float]
    ) -> bool:
        if families.get(family, 0.0) >= self._quota.families[family]:
            return False
        return any(self._pending(source, sources) for source in self._families[family])

    def _pending(self, source: str, sources: dict[str, float]) -> bool:
        return (
            self._cursors[source] < len(self._indices[source])
            and sources.get(source, 0.0) < self._quota.sources[source]
        )

    def _take(self, family: str, sources: dict[str, float]) -> int:
        names = self._families[family]
        start = self._turns.get(family, 0)
        for step in range(len(names)):
            source = names[(start + step) % len(names)]
            if not self._pending(source, sources):
                continue
            self._turns[family] = (start + step + 1) % len(names)
            return self._claim(family, source)
        raise RuntimeError(f"{family} was picked with no dispatchable source")

    def _claim(self, family: str, source: str) -> int:
        ordinal = self._cursors[source]
        self._cursors[source] = ordinal + 1
        self._dispatched[family] += 1
        self._taken[source] += 1
        return self._indices[source][ordinal]


def estimated_bytes(
    counted: dict[str, int], completed: dict[str, int], dispatched: dict[str, int]
) -> dict[str, float]:
    """Counted bytes plus in-flight shards priced at each bucket's mean size."""

    finished = sum(completed.values())
    mean = (sum(counted.values()) / finished) if finished else 1.0
    estimate: dict[str, float] = {}
    for bucket, sent in dispatched.items():
        done = completed.get(bucket, 0)
        rate = (counted.get(bucket, 0) / done) if done else mean
        estimate[bucket] = counted.get(bucket, 0) + rate * max(sent - done, 0)
    return estimate


def pick_family(
    live: list[str], weights: dict[str, float], estimate: dict[str, float]
) -> str:
    """The live family furthest below its target share of the estimated blend."""

    total = sum(estimate.values())
    if total <= 0:
        return max(live, key=lambda family: weights[family])
    return max(
        live, key=lambda family: weights[family] - estimate.get(family, 0.0) / total
    )


def _weights(
    families: dict[str, tuple[str, ...]], quota: Quota
) -> dict[str, float]:
    total = sum(quota.families[family] for family in families) or 1
    return {family: quota.families[family] / total for family in families}


def _source_indices(corpus: Corpus) -> dict[str, tuple[int, ...]]:
    grouped: dict[str, list[int]] = {}
    for index, shard in enumerate(corpus.shards):
        grouped.setdefault(shard.source, []).append(index)
    return {source: tuple(indices) for source, indices in grouped.items()}


def _family_sources(corpus: Corpus) -> dict[str, tuple[str, ...]]:
    grouped: dict[str, list[str]] = {}
    for shard in corpus.shards:
        sources = grouped.setdefault(shard.family, [])
        if shard.source not in sources:
            sources.append(shard.source)
    return {family: tuple(sources) for family, sources in grouped.items()}
