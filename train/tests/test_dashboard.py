from pathlib import Path

from rich.console import Console

from sngram_train.dashboard import RunView, render
from tests.test_pipeline import MemoryContent, build, corpus


def test_dashboard_shows_effective_rate_and_group_balance(tmp_path: Path):
    rows, content, meta = corpus([("code", 6, 100, 2), ("docs", 3, 50, 1)])
    trainer = build(tmp_path, rows, MemoryContent(content), meta)
    trainer.run()

    console = Console(record=True, width=120)
    console.print(render(trainer))
    output = console.export_text()

    assert "effective" in output
    assert "fetched" in output
    assert "code" in output and "docs" in output


def test_view_shows_notes_before_training():
    view = RunView()
    view.note("opening corpus stream")

    console = Console(record=True, width=120)
    console.print(view.render())
    output = console.export_text()

    assert "opening corpus stream" in output
