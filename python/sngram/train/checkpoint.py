"""Atomic checkpoint of counts + completed shards, so a run resumes exactly.

One JSON file, written tmp+rename, holding the counts (base64) *and* the
completed-shard state together — a kill at any instant leaves either the old
checkpoint or the new one, never a torn pair. The caller must serialize
`save` against concurrent merges/mark_done (the trainer's merge lock); under
that lock the snapshot is a true consistent cut: the counter holds exactly
the recorded completed shards.
"""

from __future__ import annotations

import base64
import json
import os
from dataclasses import dataclass, field
from pathlib import Path

import sngram


@dataclass
class RunState:
    """Everything a resumed run needs besides the raw counts."""

    # source id -> {"n_shards": int, "revision": str|None, "done": [int, ...]}
    completed: dict[str, dict] = field(default_factory=dict)
    mints_done: list[str] = field(default_factory=list)
    # repo -> pinned commit sha, fixed for the whole run (and its restarts)
    revisions: dict[str, str] = field(default_factory=dict)

    def is_done(
        self, source_id: str, n_shards: int, shard: int, revision: str | None
    ) -> bool:
        entry = self.completed.get(source_id)
        if not entry or entry["n_shards"] != n_shards:
            return False
        if entry.get("revision") != revision:
            return False  # the data behind the shard indices changed
        return shard in entry["_done_set"]

    def mark_done(
        self, source_id: str, n_shards: int, shard: int, revision: str | None
    ) -> None:
        entry = self.completed.get(source_id)
        if not entry or entry["n_shards"] != n_shards or entry.get("revision") != revision:
            entry = {
                "n_shards": n_shards,
                "revision": revision,
                "done": [],
                "_done_set": set(),
            }
            self.completed[source_id] = entry
        if shard not in entry["_done_set"]:
            entry["_done_set"].add(shard)
            entry["done"].append(shard)


def _attach_sets(state: RunState) -> RunState:
    for entry in state.completed.values():
        entry["_done_set"] = set(entry["done"])
    return state


def save(directory: Path, counter: sngram.BigramCounter, state: RunState) -> None:
    """Write one atomic checkpoint file (caller holds the merge lock)."""
    directory.mkdir(parents=True, exist_ok=True)
    payload = {
        "version": 2,
        "counts_b64": base64.b64encode(counter.snapshot()).decode(),
        "pairs": counter.pairs_processed,
        "bytes": counter.bytes_processed,
        "files": counter.files_processed,
        "completed": {
            sid: {
                "n_shards": e["n_shards"],
                "revision": e.get("revision"),
                "done": sorted(e["_done_set"]),
            }
            for sid, e in state.completed.items()
        },
        "mints_done": list(state.mints_done),
        "revisions": dict(state.revisions),
    }
    tmp = directory / "state.json.tmp"
    tmp.write_text(json.dumps(payload))
    os.replace(tmp, directory / "state.json")


def load(directory: Path, counter: sngram.BigramCounter) -> RunState | None:
    """Restore a checkpoint into a fresh `counter`; None when there is none."""
    state_path = directory / "state.json"
    if not state_path.exists():
        return None
    if counter.pairs_processed != 0 or counter.bytes_processed != 0:
        raise ValueError("checkpoint restore requires a fresh counter")

    payload = json.loads(state_path.read_text())
    if "counts_b64" in payload:
        counts = base64.b64decode(payload["counts_b64"])
    else:
        # legacy v1 layout: counts in a sibling counts.bin
        counts_path = directory / "counts.bin"
        if not counts_path.exists():
            return None
        counts = counts_path.read_bytes()
    counter.restore(counts, payload["pairs"], payload["bytes"], payload["files"])
    state = RunState(
        completed={
            sid: {
                "n_shards": e["n_shards"],
                "revision": e.get("revision"),
                "done": list(e["done"]),
            }
            for sid, e in payload["completed"].items()
        },
        mints_done=list(payload["mints_done"]),
        revisions=dict(payload.get("revisions", {})),
    )
    return _attach_sets(state)
