"""The live dashboard must render without error and surface the KL convergence
signal once a second mint has produced one."""

from __future__ import annotations

import asyncio
from pathlib import Path

import pyarrow as pa
import pyarrow.parquet as pq
from rich.console import Console

from sngram_train import dashboard
from sngram_train.config import Family, Source
from sngram_train.pipeline import Trainer
from sngram_train.units import parse_size


def _run(tmp_path: Path) -> Trainer:
    d = tmp_path / "alpha"
    d.mkdir(parents=True)
    rows = ["x" * 1000] * 100  # 100 KB/shard
    for i in range(4):
        pq.write_table(
            pa.table({"content": pa.array(rows, type=pa.large_string())}),
            d / f"alpha-{i}.parquet",
        )
    fam = Family(
        id="alpha",
        sources=(Source("alpha", "local", "content", data_files=str(d / "alpha-*.parquet")),),
    )
    trainer = Trainer(
        families=[fam],
        mint_dir=tmp_path / "bins",
        target=parse_size("300KB"),
        mint_every=parse_size("100KB"),
        workers=1,
        limit=None,
        checkpoint_every_s=3600.0,
        resume=False,
    )
    asyncio.run(trainer.run())
    return trainer


def test_dashboard_renders_with_kl(tmp_path: Path):
    trainer = _run(tmp_path)
    assert trainer.last_kl is not None
    console = Console(width=200, file=open("/dev/null", "w"))
    with console.capture() as cap:
        console.print(dashboard.render(trainer))
    out = cap.get()
    assert "kl" in out
    assert "sngram train" in out
