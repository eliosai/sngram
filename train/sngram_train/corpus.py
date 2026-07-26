"""The shapes every training corpus shares, and the choice between them."""

from __future__ import annotations

from dataclasses import dataclass, field
from enum import Enum

import pyarrow as pa
import pyarrow.compute as pc

from .errors import ConfigurationError

LAYOUTS = ("stack-files", "parquet-column", "json-lines")


class CorpusName(str, Enum):
    """The corpora a training run can be pointed at"""

    BLEND = "blend"
    STACK_V3 = "stack-v3"


@dataclass(frozen=True)
class Reading:
    """How one shard yields text: its file layout and its text column"""

    layout: str
    field: str

    def __post_init__(self) -> None:
        if self.layout not in LAYOUTS:
            raise ConfigurationError(f"unknown shard layout {self.layout!r}")
        if not self.field:
            raise ConfigurationError(f"{self.layout} shards need a text field")


@dataclass(frozen=True)
class Shard:
    """One streamable file and the buckets its bytes count against"""

    path: str
    size: int
    reading: Reading
    family: str
    source: str


@dataclass(frozen=True)
class CorpusIdentity:
    """Name and content fingerprint of the corpus a run is bound to"""

    name: str
    fingerprint: str

    def stamp(self) -> str:
        return f"{self.name}@{self.fingerprint[:12]}"

    def binding(self) -> str:
        return f"{self.name}@{self.fingerprint}"


@dataclass
class Tallies:
    """Counted bytes and completed shards per family and per source"""

    family_bytes: dict[str, int] = field(default_factory=dict)
    family_done: dict[str, int] = field(default_factory=dict)
    source_bytes: dict[str, int] = field(default_factory=dict)
    source_done: dict[str, int] = field(default_factory=dict)

    def add(self, shard: Shard, amount: int) -> None:
        self.family_bytes[shard.family] = self.family_bytes.get(shard.family, 0) + amount
        self.source_bytes[shard.source] = self.source_bytes.get(shard.source, 0) + amount

    def finish(self, shard: Shard) -> None:
        self.family_done[shard.family] = self.family_done.get(shard.family, 0) + 1
        self.source_done[shard.source] = self.source_done.get(shard.source, 0) + 1


@dataclass(frozen=True)
class Quota:
    """Byte ceilings per family and per source"""

    families: dict[str, int]
    sources: dict[str, int]

    def remaining(self, shard: Shard, tallies: Tallies) -> int:
        family = self.families[shard.family] - tallies.family_bytes.get(shard.family, 0)
        source = self.sources[shard.source] - tallies.source_bytes.get(shard.source, 0)
        return max(min(family, source), 0)


@dataclass(frozen=True)
class Corpus:
    """A resolved shard list with its identity and byte ceilings"""

    identity: CorpusIdentity
    shards: tuple[Shard, ...]
    quota: Quota | None = None

    def wire_bytes(self) -> int:
        return sum(shard.size for shard in self.shards)

    def take(self, count: int) -> Corpus:
        """The first `count` shards of every source."""

        seen: dict[str, int] = {}
        kept = []
        for shard in self.shards:
            seen[shard.source] = seen.get(shard.source, 0) + 1
            if seen[shard.source] <= count:
                kept.append(shard)
        return Corpus(self.identity, tuple(kept), self.quota)


@dataclass(frozen=True)
class Sifted:
    """One decoded batch reduced to countable content"""

    content: pa.RecordBatch
    rows: int
    files: int
    # vendor flagged files, included in the counts
    vendor_files: int = 0
    lang_bytes: dict[str, int] = field(default_factory=dict)

    def head(self, limit: int) -> Sifted:
        """The longest row prefix whose content fits a byte budget."""

        lengths = pc.fill_null(pc.binary_length(self.content.column(0)), 0)
        if (pc.sum(lengths).as_py() or 0) <= limit:
            return self
        kept = _prefix_rows(lengths, limit)
        return Sifted(self.content.slice(0, kept), self.rows, kept)


def _prefix_rows(lengths: pa.Array, limit: int) -> int:
    total = 0
    for index, length in enumerate(lengths.to_pylist()):
        if total + length > limit:
            return index
        total += length
    return len(lengths)


def resolve(name: CorpusName, token: str | None, note=None) -> Corpus:
    """Pin the chosen corpus and list every shard it will stream."""

    if name is CorpusName.BLEND:
        from . import blend

        return blend.resolve(token, note)
    from . import stack

    return stack.resolve(token, note)
