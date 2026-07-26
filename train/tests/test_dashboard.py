from pathlib import Path

from rich.console import Console

from sngram_train.dashboard import RunView, render
from tests.localcorpus import code_repos, write_corpus
from tests.test_pipeline import build


def test_dashboard_shows_rates_progress_and_the_mix(tmp_path: Path):
    corpus, source = write_corpus(tmp_path / "corpus", [code_repos(6)])
    trainer = build(tmp_path, corpus, source)
    trainer.run()

    console = Console(record=True, width=140)
    console.print(render(trainer))
    output = console.export_text()

    assert "decoded" in output
    assert "now" in output and "avg" in output
    assert "wire now" in output
    assert "vendor" in output and "skipped" not in output
    assert "rows" in output
    assert "Rust" in output


def test_view_shows_notes_before_training():
    view = RunView()
    view.note("resolving corpus")

    console = Console(record=True, width=120)
    console.print(view.render())
    output = console.export_text()

    assert "resolving corpus" in output
