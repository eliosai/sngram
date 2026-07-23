from pathlib import Path

from sngram_train import cli
from sngram_train.pipeline import Trainer, TrainerConfig
from tests.test_pipeline import ListStream, MemoryContent, build, corpus


class FlakyStream(ListStream):
    """Raises the closed-client error once, mid-stream, like a DNS blip."""

    def __init__(self, rows, position, fail_at, armed):
        super().__init__(rows, position)
        self.fail_at = fail_at
        self.armed = armed

    def __iter__(self):
        while self.position < len(self.rows):
            if self.armed[0] and self.position == self.fail_at:
                self.armed[0] = False
                raise RuntimeError(
                    "Cannot send a request, as the client has been closed."
                )
            row = self.rows[self.position]
            self.position += 1
            yield row


def test_a_network_blip_mid_stream_resumes_and_completes(tmp_path: Path, monkeypatch):
    rows, content, meta = corpus([("code", 30, 100, 2), ("docs", 10, 60, 1)])
    armed = [True]

    def builder(resume_now: bool):
        factory = lambda state: FlakyStream(
            rows, (state or {}).get("position", 0), fail_at=13, armed=armed
        )
        config = TrainerConfig(
            mint_dir=tmp_path / "bins",
            workers=4,
            checkpoint_interval=0.0,
            resume=resume_now,
        )
        return Trainer(factory, MemoryContent(content), config, meta)

    monkeypatch.setattr("time.sleep", lambda _seconds: None)
    trainer = cli._run_until_done(builder, resume=True, view=None)

    reference = build(tmp_path / "reference", rows, MemoryContent(content), meta)
    reference.run()

    assert trainer.counter.bytes_processed == meta.effective_bytes
    recovered = (tmp_path / "bins" / "final_weights.bin").read_bytes()
    expected = (tmp_path / "reference" / "bins" / "final_weights.bin").read_bytes()
    assert recovered == expected
