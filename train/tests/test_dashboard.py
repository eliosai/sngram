from pathlib import Path

from rich.console import Console

from sngram_train.dashboard import RunView, render
from tests.test_pipeline import setup_run


def test_dashboard_shows_effective_fetched_and_format_balance(tmp_path: Path):
    trainer = setup_run(tmp_path, {"a": [20] * 4, "b": [20] * 4}, target=120)
    trainer.current_threshold = 60
    console = Console(record=True, width=120)

    console.print(render(trainer))
    output = console.export_text()

    assert "effective" in output
    assert "fetched" in output
    assert "code" in output
    assert "a" in output and "b" in output
    trainer.events.close()


def test_rescanned_configs_are_counted_once(tmp_path: Path):
    view = RunView()
    view.manifest_start(3)
    for _round in range(2):
        view.started("Python")
        view.finished("Python", accepted=10, effective=1000, seconds=1.0)

    console = Console(record=True, width=120)
    console.print(view.render())
    output = console.export_text()

    assert "manifest 1/3 configs" in output
