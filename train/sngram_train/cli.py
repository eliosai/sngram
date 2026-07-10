"""Training, inspection, and validation commands."""

from __future__ import annotations

import os
import time
from pathlib import Path
from typing import Optional

import typer

from .config import CANONICAL_TARGET_BYTES, hf_token

app = typer.Typer(
    add_completion=False,
    no_args_is_help=True,
    help="Sparse n-gram weight tables: train, inspect, validate.",
)


@app.command()
def train(
    mint_dir: Path = typer.Option(Path("./bins"), help="Output and durable run state."),
    target: str = typer.Option("10TB", help="Effective bytes to train."),
    mint_every: str = typer.Option("1TB", help="Mint cadence after bootstrap mints."),
    workers: Optional[int] = typer.Option(None, help="Concurrent bounded content reads."),
    limit: Optional[str] = typer.Option(None, help="Override target for a smoke run."),
    checkpoint_every: float = typer.Option(60.0, help="Checkpoint period in seconds."),
    resume: bool = typer.Option(True, help="Resume the manifest and checkpoint."),
    dashboard: bool = typer.Option(True, help="Show the live terminal dashboard."),
) -> None:
    """Build a balanced Stack manifest and mint durable weight tables."""

    from .units import parse_size

    os.environ.setdefault("HF_HUB_DOWNLOAD_TIMEOUT", "30")
    os.environ.setdefault("HF_HUB_ETAG_TIMEOUT", "30")
    token = hf_token()
    if token is None:
        typer.echo("error: HF_TOKEN is required for the production corpus")
        raise typer.Exit(2)
    effective_target = parse_size(limit or target)
    n_workers = workers or default_workers()
    build = lambda resume_now: _production_trainer(
        mint_dir=mint_dir,
        target=effective_target,
        mint_cadence=parse_size(mint_every),
        workers=n_workers,
        checkpoint_interval=checkpoint_every,
        resume=resume_now,
        token=token,
    )
    from .errors import ConfigurationError, CorpusExhausted

    try:
        trainer = _run_until_done(build, resume, dashboard)
    except (ConfigurationError, CorpusExhausted) as error:
        typer.echo(f"error: {error}")
        raise typer.Exit(2) from error
    typer.echo(f"done: {trainer.describe_progress()}")


def default_workers() -> int:
    return min(max((os.cpu_count() or 4) * 4, 16), 64)


def _production_trainer(
    *,
    mint_dir: Path,
    target: int = CANONICAL_TARGET_BYTES,
    mint_cadence: int,
    workers: int,
    checkpoint_interval: float,
    resume: bool,
    token: str,
):
    from .catalog import build_catalog
    from .config import STACK_V2_BUCKET_CAPS
    from .content import SwhContent
    from .errors import ConfigurationError
    from .pipeline import Trainer, TrainerConfig
    from .resources import manifest_disk_budget
    from .stack import HuggingFaceRows

    rows = HuggingFaceRows(token)
    catalog = build_catalog(rows.configs())
    roster_hash = catalog.roster_hash(rows.revision, target)
    path = mint_dir / ".manifest.sqlite3"
    config_counts = {}
    for item in catalog.formats:
        config_counts[item.config] = config_counts.get(item.config, 0) + 1
    extra_capacity = sum(
        item.cap_bytes for item in catalog.formats if config_counts[item.config] > 1
    )
    budget = manifest_disk_budget(path, target, extra_capacity)
    if not budget.sufficient:
        raise ConfigurationError(
            f"insufficient disk for manifest: need {budget.required_bytes} bytes, "
            f"have {budget.free_bytes} bytes"
        )
    manifest = _prepare_manifest(
        path,
        catalog,
        rows,
        roster_hash,
        target,
        STACK_V2_BUCKET_CAPS,
        min(workers, 4),
    )
    config = TrainerConfig(
        mint_dir, target, mint_cadence, workers, checkpoint_interval, resume
    )
    return Trainer(catalog, manifest, SwhContent(workers=workers), config, STACK_V2_BUCKET_CAPS)


def _prepare_manifest(
    path, catalog, rows, roster_hash, target, area_weights, workers
):
    from .events import EventLog
    from .manifest import open_manifest
    from .stack import build_stack_manifest

    if path.exists():
        return open_manifest(path, roster_hash)
    total = len(catalog.configs)
    typer.echo(f"building sampled manifest for {total} Stack formats ({workers} readers)")
    events = EventLog(path.parent / "train-events.jsonl")
    events.log(
        "manifest_start",
        revision=rows.revision,
        configs=len(catalog.configs),
        formats=len(catalog.formats),
    )

    completed = 0

    def report(fields):
        nonlocal completed
        completed += 1
        events.log(
            "manifest_config",
            config=fields[0],
            candidates=fields[1],
            effective_bytes=fields[2],
            seconds=round(fields[3], 3),
        )
        typer.echo(
            f"manifest {completed}/{total}: {fields[0]} "
            f"({fields[1]} objects, {fields[3]:.1f}s)"
        )

    try:
        build_stack_manifest(
            path,
            catalog,
            rows,
            report,
            target=target,
            area_weights=area_weights,
            workers=workers,
        )
        events.log("manifest_done", bytes=path.stat().st_size)
    finally:
        events.close()
    return open_manifest(path, roster_hash)


def _run_until_done(build, resume: bool, dashboard: bool):
    from .errors import ConfigurationError, CorpusExhausted

    attempt = 0
    while True:
        trainer = None
        try:
            trainer = build(resume or attempt > 0)
            if dashboard:
                from .dashboard import Dashboard

                with Dashboard() as live:
                    trainer.on_refresh = live.refresh
                    trainer.run()
            else:
                trainer.run()
            return trainer
        except KeyboardInterrupt:
            if trainer is None:
                raise
            typer.echo("\ninterrupted; checkpoint saved")
            return trainer
        except (ConfigurationError, CorpusExhausted):
            raise
        except Exception as error:
            attempt += 1
            typer.echo(f"\ntransport failure ({error!r}); resuming in 30s")
            time.sleep(30)


@app.command()
def inspect(
    path: Path = typer.Argument(..., help="A minted weight table."),
    top: int = typer.Option(20, help="Pairs to show per end."),
) -> None:
    """Print the commonest and rarest byte pairs."""

    import sngram

    table = sngram.WeightTable.from_path(path)
    pairs = sorted(
        (table.weight(c1, c2), c1, c2) for c1 in range(256) for c2 in range(256)
    )
    typer.echo("commonest bigrams (lowest weight):")
    for weight, c1, c2 in pairs[:top]:
        typer.echo(f"  {weight:<10} {_show_pair(c1, c2)}")
    typer.echo("rarest bigrams (highest weight):")
    for weight, c1, c2 in pairs[-top:][::-1]:
        typer.echo(f"  {weight:<10} {_show_pair(c1, c2)}")


def _show_pair(c1: int, c2: int) -> str:
    return "".join(chr(value) if 32 <= value < 127 else f"\\x{value:02x}" for value in (c1, c2))


@app.command("fs-histogram")
def fs_histogram(
    roots: list[Path] = typer.Argument(..., help="Directories or files."),
    cap: Optional[str] = typer.Option(None, help="Maximum text bytes."),
    top: int = typer.Option(25, help="Pairs and extensions to show."),
) -> None:
    """Measure the byte-pair distribution of text files."""

    from . import fsvalidate
    from .units import fmt_bytes, parse_size

    counts, stats = fsvalidate.filesystem_histogram(
        [str(root) for root in roots], cap=parse_size(cap) if cap else None
    )
    pairs = sum(counts) or 1
    typer.echo(
        f"text files: {stats.files}  skipped binary: {stats.skipped_binary}  "
        f"text bytes: {fmt_bytes(stats.total_bytes)}"
    )
    order = sorted(range(len(counts)), key=counts.__getitem__, reverse=True)
    for index in order[:top]:
        pair = _show_pair(index >> 8, index & 255)
        typer.echo(f"  {pair:8s} {counts[index] / pairs * 100:5.2f}%")
    total = stats.total_bytes or 1
    extensions = sorted(
        stats.ext_bytes.items(), key=lambda item: item[1], reverse=True
    )[:top]
    for extension, size in extensions:
        typer.echo(f"  {extension:14s} {size / total * 100:5.2f}%")


@app.command("fs-validate")
def fs_validate(
    table_path: Path = typer.Argument(..., help="A minted weight table."),
    roots: list[Path] = typer.Argument(..., help="Directories or files."),
    cap: Optional[str] = typer.Option(None, help="Maximum text bytes."),
    top: int = typer.Option(15, help="Pairs to show."),
) -> None:
    """Compare a table with the byte-pair distribution on disk."""

    import sngram

    from . import fsvalidate
    from .units import parse_size

    table = sngram.WeightTable.from_path(table_path)
    counts, _stats = fsvalidate.filesystem_histogram(
        [str(root) for root in roots], cap=parse_size(cap) if cap else None
    )
    report = fsvalidate.validate(counts, table, top=top)
    typer.echo(f"KL(filesystem || table) = {report.kl:.4f} nats")
    for label, rows in (("under-represented", report.under_weighted),
                        ("over-represented", report.over_weighted)):
        typer.echo(f"{label} pairs:")
        for (c1, c2), filesystem, trained, _score in rows:
            typer.echo(
                f"  {_show_pair(c1, c2):8s} fs {filesystem * 100:5.2f}%  "
                f"table {trained * 100:5.2f}%"
            )


def main() -> None:
    app()


if __name__ == "__main__":
    main()
