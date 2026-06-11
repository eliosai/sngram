"""The sngram command line: train weight tables, inspect them, bench ingest."""

from __future__ import annotations

import asyncio
import os
from pathlib import Path
from typing import Optional

import typer

app = typer.Typer(
    add_completion=False,
    no_args_is_help=True,
    help="Sparse n-gram weight tables: train, inspect, benchmark.",
)


@app.command()
def train(
    mint_dir: Path = typer.Option(Path("./bins"), help="Where minted .bin tables land."),
    target: str = typer.Option("50TB", help="Total text to count."),
    mint_every: str = typer.Option(
        "5TB", help="Steady mint cadence (100GB/500GB/1TB bootstrap mints come first)."
    ),
    workers: Optional[int] = typer.Option(
        None, help="Streaming/counting workers; default: one per physical core (4..16)."
    ),
    limit: Optional[str] = typer.Option(
        None, help="Stop after this much text (smoke runs, e.g. 1GB)."
    ),
    checkpoint_every: float = typer.Option(60.0, help="Checkpoint period, seconds."),
    resume: bool = typer.Option(True, help="Resume from the mint dir's checkpoint."),
    dashboard: bool = typer.Option(True, help="Live terminal dashboard."),
) -> None:
    """Stream the training mix from Hugging Face and mint weight tables.

    Built for unattended multi-day runs: transient HF failures back off and
    retry forever, an unexpected crash checkpoints and restarts in place, and
    Ctrl-C checkpoints so the same command resumes exactly.
    """
    # fail fast on network hangs instead of stalling silently
    os.environ.setdefault("HF_HUB_DOWNLOAD_TIMEOUT", "30")
    os.environ.setdefault("HF_HUB_ETAG_TIMEOUT", "30")

    from .train.config import default_families, hf_token
    from .train.pipeline import Trainer, default_workers
    from .train.units import parse_size

    if hf_token() is None:
        typer.echo("warning: no HF_TOKEN set (env or .env); rate limits will be tight")
    n_workers = workers or default_workers()

    def build(resume_now: bool) -> Trainer:
        return Trainer(
            families=default_families(),
            mint_dir=mint_dir,
            target=parse_size(target),
            mint_every=parse_size(mint_every),
            workers=n_workers,
            limit=parse_size(limit) if limit else None,
            checkpoint_every_s=checkpoint_every,
            resume=resume_now,
        )

    trainer = _run_until_done(build, resume, dashboard)
    typer.echo(f"done: {trainer.describe_progress()}")


def _run_until_done(build, resume: bool, dashboard: bool):
    """Run to completion, surviving crashes: rebuild from checkpoint and go on."""
    import time as _time

    attempt = 0
    while True:
        trainer = build(resume or attempt > 0)
        try:
            if dashboard:
                from .train.dashboard import Dashboard

                with Dashboard() as dash:
                    trainer.on_refresh = dash.refresh
                    asyncio.run(trainer.run())
            else:
                asyncio.run(trainer.run())
            return trainer
        except KeyboardInterrupt:
            typer.echo("\ninterrupted — checkpoint saved, resume with the same command")
            return trainer
        except Exception as e:  # noqa: BLE001 - a 10-day run must outlive surprises
            attempt += 1
            typer.echo(f"\ncrash ({e!r}) — resuming from checkpoint in 30s (attempt {attempt})")
            _time.sleep(30)


@app.command()
def inspect(
    path: Path = typer.Argument(..., help="A minted .bin weight table."),
    top: int = typer.Option(20, help="How many pairs to show per end."),
) -> None:
    """Print the commonest and rarest byte pairs of a minted table."""
    import sngram

    table = sngram.WeightTable.from_path(path)
    pairs = sorted(
        ((table.weight(c1, c2), c1, c2) for c1 in range(256) for c2 in range(256)),
    )

    def show(c1: int, c2: int) -> str:
        return "".join(chr(c) if 32 <= c < 127 else f"\\x{c:02x}" for c in (c1, c2))

    typer.echo("commonest bigrams (lowest weight):")
    for w, c1, c2 in pairs[:top]:
        typer.echo(f"  {w:<10} {show(c1, c2)}")
    typer.echo("rarest bigrams (highest weight):")
    for w, c1, c2 in pairs[-top:][::-1]:
        typer.echo(f"  {w:<10} {show(c1, c2)}")


@app.command("bench-ingest")
def bench_ingest(
    size: str = typer.Option("256MB", help="Synthetic corpus size."),
    workers: int = typer.Option(1, help="Parallel counting workers."),
) -> None:
    """Measure the offline ingest pipeline (parquet -> arrow -> count), no network."""
    import shutil
    import tempfile
    import time

    import pyarrow as pa
    import pyarrow.parquet as pq

    import sngram

    from .train.units import fmt_bytes, fmt_rate, parse_size

    total = parse_size(size)
    snippet = (
        'fn main() { let x = foo_bar(42); println!("{x}"); }\n'
        "pub async fn read(hash: Hash) -> Result<Bytes, Error> { todo!() }\n"
    )
    row = (snippet * 40)[:4096]
    rows = max(total // len(row), 1)

    tmp = Path(tempfile.mkdtemp(prefix="sngram-bench-"))
    try:
        path = tmp / "bench.parquet"
        arr = pa.array([row] * rows, type=pa.large_string())
        pq.write_table(pa.table({"content": arr}), path)
        actual = rows * len(row)
        typer.echo(f"fixture: {rows} rows, {fmt_bytes(actual)} text, {path.stat().st_size:,} B parquet")

        # pure counting ceiling: in-memory table -> count_arrow
        tbl = pq.read_table(path)
        tally = sngram.LocalTally()
        t0 = time.perf_counter()
        n = tally.count_arrow(tbl)
        pure = n / (time.perf_counter() - t0)
        typer.echo(f"count_arrow (in-memory):   {fmt_rate(pure)}")

        # full pipeline: datasets streaming -> arrow batches -> count_arrow
        from concurrent.futures import ThreadPoolExecutor

        from datasets import load_dataset

        def stream_all(_w: int) -> int:
            ds = load_dataset(
                "parquet", data_files=str(path), split="train", streaming=True
            ).with_format("arrow")
            t = sngram.LocalTally()
            got = 0
            for batch in ds.iter(batch_size=256):
                got += t.count_arrow(batch)
            return got

        t0 = time.perf_counter()
        with ThreadPoolExecutor(max_workers=workers) as pool:
            counted = sum(pool.map(stream_all, range(workers)))
        rate = counted / (time.perf_counter() - t0)
        typer.echo(f"pipeline x{workers} (streamed): {fmt_rate(rate)} total")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def main() -> None:
    app()


if __name__ == "__main__":
    main()
