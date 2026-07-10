from pathlib import Path
from types import SimpleNamespace

from sngram_train.resources import manifest_disk_budget


def test_manifest_budget_accounts_for_existing_partial_file(tmp_path: Path, monkeypatch):
    path = tmp_path / "manifest.sqlite3"
    partial = path.with_suffix(path.suffix + ".tmp")
    partial.write_bytes(b"x" * 1000)
    monkeypatch.setattr(
        "sngram_train.resources.shutil.disk_usage",
        lambda _path: SimpleNamespace(total=10_000, used=9_000, free=8_000),
    )

    budget = manifest_disk_budget(path, target=16 * 1024, extra_capacity=0)

    assert budget.required_bytes == 5_000_000_000
    assert budget.free_bytes == 8_000
    assert budget.sufficient is False
