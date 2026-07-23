"""Training, inspection, and validation commands."""

from __future__ import annotations

import os
import time
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
    mint_dir: Path = typer.Option(Path("./bins"), help="Output and durable run state."),
    workers: Optional[int] = typer.Option(None, help="Concurrent bounded content reads."),
    limit: Optional[str] = typer.Option(None, help="Effective-byte cap for a smoke run."),
    checkpoint_every: float = typer.Option(60.0, help="Checkpoint period in seconds."),
    resume: bool = typer.Option(True, help="Resume from the checkpoint."),
    dashboard: bool = typer.Option(True, help="Show the live terminal dashboard."),
) -> None:
    """Stream the published corpus and mint the final weight table."""

    from .errors import ConfigurationError

    _tune_runtime()
    view = _run_view() if dashboard else None
    build = _trainer_factory(mint_dir, workers, limit, checkpoint_every, view)
    try:
        trainer = _dashboard_run(build, resume, view)
    except ConfigurationError as error:
        typer.echo(f"error: {error}")
        raise typer.Exit(2) from error
    typer.echo(f"done: {trainer.describe_progress()}")


def _trainer_factory(
    mint_dir: Path,
    workers: Optional[int],
    limit: Optional[str],
    checkpoint_every: float,
    view,
):
    from .units import parse_size

    cap = parse_size(limit) if limit else None
    concurrency = workers or _default_workers()

    def build(resume_now: bool):
        return _production_trainer(
            mint_dir=mint_dir,
            workers=concurrency,
            limit=cap,
            checkpoint_interval=checkpoint_every,
            resume=resume_now,
        )

    return build


def _tune_runtime() -> None:
    import sys

    sys.setswitchinterval(0.002)
    os.environ.setdefault("HF_HUB_DOWNLOAD_TIMEOUT", "30")
    os.environ.setdefault("HF_HUB_ETAG_TIMEOUT", "30")


def _default_workers() -> int:
    return min(max((os.cpu_count() or 4) * 16, 64), 256)


def _run_view():
    from .dashboard import RunView

    return RunView()


def _dashboard_run(build, resume: bool, view):
    if view is None:
        return _run_until_done(build, resume, None)
    from .dashboard import Dashboard

    with Dashboard(view):
        return _run_until_done(build, resume, view)


def _production_trainer(
    *,
    mint_dir: Path,
    workers: int,
    limit: Optional[int],
    checkpoint_interval: float,
    resume: bool,
):
    from .config import hf_token
    from .content import SwhContent
    from .pipeline import Trainer, TrainerConfig
    from .stream import CorpusStream, corpus_meta

    token = hf_token()
    corpus = corpus_meta(token)
    config = TrainerConfig(mint_dir, workers, checkpoint_interval, limit, resume)
    factory = lambda state: CorpusStream.open(token, state)
    return Trainer(factory, SwhContent(workers=workers), config, corpus)


def _run_until_done(build, resume: bool, view):
    from .errors import ConfigurationError, is_transient

    attempt = 0
    delay = 5.0
    while True:
        trainer = None
        try:
            trainer = build(resume or attempt > 0)
            if view is not None:
                view.training(trainer)
            trainer.run()
            return trainer
        except KeyboardInterrupt:
            if trainer is None:
                raise
            typer.echo("\ninterrupted; checkpoint saved")
            return trainer
        except ConfigurationError:
            raise
        except Exception as error:
            if not is_transient(error):
                raise
            attempt += 1
            delay = _transport_pause(error, delay)


def _transport_pause(error: Exception, delay: float) -> float:
    typer.echo(f"\ntransport failure ({error!r}); resuming in {delay:.0f}s")
    time.sleep(delay)
    return min(delay * 2, 300.0)


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
    typer.echo(
        f"text files: {stats.files}  skipped binary: {stats.skipped_binary}  "
        f"text bytes: {fmt_bytes(stats.total_bytes)}"
    )
    _echo_top_pairs(counts, top)
    _echo_extensions(stats, top)


def _echo_top_pairs(counts: list[int], top: int) -> None:
    pairs = sum(counts) or 1
    order = sorted(range(len(counts)), key=counts.__getitem__, reverse=True)
    for index in order[:top]:
        pair = _show_pair(index >> 8, index & 255)
        typer.echo(f"  {pair:8s} {counts[index] / pairs * 100:5.2f}%")


def _echo_extensions(stats, top: int) -> None:
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
