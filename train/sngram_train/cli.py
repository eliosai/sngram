"""The sngram command line: train, inspect, and validate weight tables."""

from __future__ import annotations

import asyncio
import os
from pathlib import Path
from typing import Optional

import typer

app = typer.Typer(
    add_completion=False,
    no_args_is_help=True,
    help="Sparse n-gram weight tables: train, inspect, validate.",
)


@app.command()
def train(
    mint_dir: Path = typer.Option(Path("./bins"), help="Where minted .bin tables land."),
    target: str = typer.Option("12TB", help="Total text to count."),
    mint_every: str = typer.Option(
        "1TB", help="Steady mint cadence (100GB/500GB bootstrap mints come first)."
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

    from .config import default_families, hf_token
    from .pipeline import Trainer, default_workers
    from .units import parse_size

    if hf_token() is None:
        typer.echo("error: HF_TOKEN is required for the production training roster")
        raise typer.Exit(2)
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

    try:
        trainer = _run_until_done(build, resume, dashboard)
    except RuntimeError as e:
        typer.echo(f"preflight failed: {e}")
        raise typer.Exit(2) from e
    typer.echo(f"done: {trainer.describe_progress()}")


def _run_until_done(build, resume: bool, dashboard: bool):
    """Run to completion, surviving crashes: rebuild from checkpoint and go on."""
    import time as _time

    attempt = 0
    while True:
        trainer = build(resume or attempt > 0)
        try:
            trainer.preflight_sources()
        except Exception:
            if events := getattr(trainer, "events", None):
                events.close()
            raise
        try:
            if dashboard:
                from .dashboard import Dashboard

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


def _show_pair(c1: int, c2: int) -> str:
    return "".join(chr(c) if 32 <= c < 127 else f"\\x{c:02x}" for c in (c1, c2))


@app.command("fs-histogram")
def fs_histogram(
    roots: list[Path] = typer.Argument(..., help="Directories/files to histogram."),
    cap: Optional[str] = typer.Option(None, help="Stop after this many text bytes (e.g. 2GB)."),
    top: int = typer.Option(25, help="How many top byte-pairs / extensions to show."),
) -> None:
    """Measure the byte-pair distribution of a real filesystem (text files only,
    binaries skipped) — the empirical target a search table should match."""
    from . import fsvalidate
    from .units import fmt_bytes, parse_size

    counts, stats = fsvalidate.filesystem_histogram(
        [str(r) for r in roots], cap=parse_size(cap) if cap else None
    )
    pairs = sum(counts) or 1
    typer.echo(
        f"text files: {stats.files}  skipped binary: {stats.skipped_binary}  "
        f"text bytes: {fmt_bytes(stats.total_bytes)}"
    )
    order = sorted(range(len(counts)), key=lambda i: counts[i], reverse=True)
    typer.echo(f"top {top} byte-pairs:")
    for i in order[:top]:
        typer.echo(f"  {_show_pair(i >> 8, i & 0xFF):8s} {counts[i] / pairs * 100:5.2f}%")
    typer.echo(f"top {top} extensions by bytes:")
    tb = stats.total_bytes or 1
    for ext, n in sorted(stats.ext_bytes.items(), key=lambda kv: kv[1], reverse=True)[:top]:
        typer.echo(f"  {ext:14s} {n / tb * 100:5.2f}%")


@app.command("fs-validate")
def fs_validate(
    table_path: Path = typer.Argument(..., help="A minted .bin weight table."),
    roots: list[Path] = typer.Argument(..., help="Directories/files to validate against."),
    cap: Optional[str] = typer.Option(None, help="Stop after this many text bytes (e.g. 2GB)."),
    top: int = typer.Option(15, help="How many over/under-weighted pairs to show."),
) -> None:
    """Score a minted table against a real filesystem: KL-divergence plus the
    byte-pairs the corpus most under- and over-represents versus the disk."""
    import sngram

    from . import fsvalidate
    from .units import fmt_bytes, parse_size

    table = sngram.WeightTable.from_path(table_path)
    counts, stats = fsvalidate.filesystem_histogram(
        [str(r) for r in roots], cap=parse_size(cap) if cap else None
    )
    report = fsvalidate.validate(counts, table, top=top)
    typer.echo(
        f"validated against {stats.files} text files "
        f"({fmt_bytes(stats.total_bytes)}, {stats.skipped_binary} binaries skipped)"
    )
    typer.echo(f"KL(filesystem || table) = {report.kl:.4f} nats  (0 = perfect match)")
    typer.echo(f"top {top} pairs the corpus UNDER-represents (raise these sources):")
    for (c1, c2), pf, qf, _score in report.under_weighted:
        typer.echo(f"  {_show_pair(c1, c2):8s} fs {pf * 100:5.2f}%  table {qf * 100:5.2f}%")
    typer.echo(f"top {top} pairs the corpus OVER-represents (lower these sources):")
    for (c1, c2), pf, qf, _score in report.over_weighted:
        typer.echo(f"  {_show_pair(c1, c2):8s} fs {pf * 100:5.2f}%  table {qf * 100:5.2f}%")


def main() -> None:
    app()


if __name__ == "__main__":
    main()
