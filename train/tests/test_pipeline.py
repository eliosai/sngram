from pathlib import Path

import pytest
import sngram

from sngram_train.errors import ConfigurationError
from sngram_train.pipeline import Trainer, TrainerConfig
from sngram_train.stream import CorpusMeta, CorpusRow

LINE = b"fn main() { return 42; }\n"


class ListStream:
    def __init__(self, rows, position=0):
        self.rows = rows
        self.position = position

    def __iter__(self):
        while self.position < len(self.rows):
            row = self.rows[self.position]
            self.position += 1
            yield row

    def state_dict(self):
        return {"position": self.position}


class MemoryContent:
    def __init__(self, values):
        self.values = values

    def read(self, blob_id, _max_bytes):
        return self.values[blob_id]


class InterruptingContent(MemoryContent):
    def __init__(self, values, interrupt_at):
        super().__init__(values)
        self.interrupt_at = interrupt_at
        self.calls = 0

    def read(self, blob_id, max_bytes):
        self.calls += 1
        if self.calls == self.interrupt_at:
            raise KeyboardInterrupt
        return super().read(blob_id, max_bytes)


def corpus(spec):
    """Rows and content from (group, copies, doc_bytes, weight) specs."""

    rows, content = [], {}
    for group, copies, size, weight in spec:
        for index in range(copies):
            blob = f"{group}-{index}"
            content[blob] = (LINE * (size // len(LINE) + 1))[:size]
            rows.append(CorpusRow(group, blob, "utf-8", size, weight))
    groups = {}
    for row in rows:
        groups[row.group] = groups.get(row.group, 0) + row.length * row.weight
    meta = CorpusMeta(
        "revision", len(rows), sum(r.length for r in rows), sum(groups.values()), groups
    )
    return rows, content, meta


def build(tmp_path, rows, content, meta, limit=None, resume=False, interval=3600.0):
    factory = lambda state: ListStream(rows, (state or {}).get("position", 0))
    config = TrainerConfig(
        mint_dir=tmp_path / "bins",
        workers=4,
        checkpoint_interval=interval,
        limit=limit,
        resume=resume,
    )
    return Trainer(factory, content, config, meta)


def test_full_stream_counts_every_row_and_mints_final(tmp_path: Path):
    rows, content, meta = corpus([("code", 6, 100, 4), ("docs", 3, 50, 1)])
    trainer = build(tmp_path, rows, MemoryContent(content), meta)

    trainer.run()

    assert trainer.counter.bytes_processed == meta.effective_bytes
    assert trainer.state.rows == len(rows)
    assert trainer.group_bytes() == {"code": 2400, "docs": 150}
    table = sngram.WeightTable.from_path(tmp_path / "bins" / "final_weights.bin")
    assert "stack-v2@revision" in (table.provenance or "")
    assert f"{meta.effective_bytes} effective bytes" in table.provenance


def test_missing_content_is_skipped_and_the_run_completes(tmp_path: Path):
    rows, content, meta = corpus([("code", 10, 100, 1)])
    for lost in ("code-3", "code-7"):
        del content[lost]

    class LossyContent(MemoryContent):
        def read(self, blob_id, _max_bytes):
            if blob_id not in self.values:
                raise FileNotFoundError(blob_id)
            return self.values[blob_id]

    trainer = build(tmp_path, rows, LossyContent(content), meta)
    trainer.run()

    assert trainer.skips == 2
    assert trainer.counter.bytes_processed == 800
    assert (tmp_path / "bins" / "final_weights.bin").exists()


def test_limit_stops_the_stream_early(tmp_path: Path):
    rows, content, meta = corpus([("code", 40, 100, 1)])
    trainer = build(tmp_path, rows, MemoryContent(content), meta, limit=500)

    trainer.run()

    assert 500 <= trainer.counter.bytes_processed < meta.effective_bytes
    assert (tmp_path / "bins" / "final_weights.bin").exists()


def test_interrupted_run_resumes_to_the_identical_table(tmp_path: Path):
    rows, content, meta = corpus([("code", 30, 100, 2), ("docs", 10, 60, 1)])
    interrupted = build(
        tmp_path / "run", rows, InterruptingContent(content, 17), meta, interval=0.0
    )
    with pytest.raises(KeyboardInterrupt):
        interrupted.run()

    resumed = build(
        tmp_path / "run", rows, MemoryContent(content), meta, resume=True
    )
    resumed.run()
    reference = build(tmp_path / "reference", rows, MemoryContent(content), meta)
    reference.run()

    resumed_table = (tmp_path / "run" / "bins" / "final_weights.bin").read_bytes()
    reference_table = (tmp_path / "reference" / "bins" / "final_weights.bin").read_bytes()
    assert resumed_table == reference_table
    assert resumed.counter.bytes_processed == meta.effective_bytes


def test_checkpoint_rejects_a_different_corpus_revision(tmp_path: Path):
    rows, content, meta = corpus([("code", 4, 100, 1)])
    trainer = build(tmp_path, rows, MemoryContent(content), meta)
    trainer.run()

    drifted = CorpusMeta("other", meta.rows, meta.raw_bytes, meta.effective_bytes, meta.groups)
    with pytest.raises(ConfigurationError, match="revision"):
        build(tmp_path, rows, MemoryContent(content), drifted, resume=True)


def test_no_resume_starts_a_fresh_run(tmp_path: Path):
    rows, content, meta = corpus([("code", 4, 100, 1)])
    build(tmp_path, rows, MemoryContent(content), meta).run()

    fresh = build(tmp_path, rows, MemoryContent(content), meta, resume=False)
    fresh.run()

    assert fresh.counter.bytes_processed == meta.effective_bytes
