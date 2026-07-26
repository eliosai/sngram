"""Training, inspection, and validation commands."""

from __future__ import annotations

import os
import time
from pathlib import Path
from typing import Optional

import typer

from .corpus import CorpusName
from .errors import ConfigurationError
from .units import parse_size

app = typer.Typer(
    add_completion=False,
    no_args_is_help=True,
    help="Sparse n-gram weight tables: train, inspect, validate.",
)


@app.command()
def train(
    mint_dir: Path = typer.Option(Path("./bins"), help="Output and durable run state."),
    corpus: CorpusName = typer.Option(CorpusName.BLEND, help="Which corpus to stream."),
    workers: int = typer.Option(10, help="Parallel shard readers."),
    limit: Optional[str] = typer.Option(None, help="Decoded-byte cap for a smoke run."),
    shards: Optional[int] = typer.Option(None, help="Only the first N shards per source."),
    checkpoint_every: float = typer.Option(60.0, help="Checkpoint period in seconds."),
    resume: bool = typer.Option(True, help="Resume from the checkpoint."),
    dashboard: bool = typer.Option(True, help="Show the live terminal dashboard."),
) -> None:
    """Stream a training corpus and mint the final weight table."""

    _tune_runtime()
    view = _run_view() if dashboard else None
    build = _trainer_factory(
        mint_dir, corpus, workers, limit, shards, checkpoint_every,
        view.note if view else None,
    )
    try:
        trainer = _dashboard_run(build, resume, view)
    except ConfigurationError as error:
        typer.echo(f"error: {error}")
        raise typer.Exit(2) from error
    typer.echo(f"done: {trainer.progress.describe()}")


def _trainer_factory(
    mint_dir: Path,
    corpus: CorpusName,
    workers: int,
    limit: Optional[str],
    shards: Optional[int],
    checkpoint_every: float,
    note=None,
):
    cap = parse_size(limit) if limit else None

    def build(resume_now: bool):
        return _production_trainer(
            mint_dir=mint_dir,
            corpus=corpus,
            workers=workers,
            limit=cap,
            shards=shards,
            checkpoint_interval=checkpoint_every,
            resume=resume_now,
            note=note,
        )

    return build


def _tune_runtime() -> None:
    os.environ.setdefault("HF_HUB_DOWNLOAD_TIMEOUT", "30")
    os.environ.setdefault("HF_HUB_ETAG_TIMEOUT", "30")
    os.environ.setdefault("ARROW_DEFAULT_MEMORY_POOL", "system")
    _pin_malloc()


# glibc mallopt selectors for mmap and trim thresholds
_M_TRIM_THRESHOLD = -1
_M_MMAP_THRESHOLD = -3


def _pin_malloc() -> None:
    """Keep big buffers on mmap so freed decode memory returns to the OS"""

    import ctypes

    try:
        libc = ctypes.CDLL("libc.so.6")
        libc.mallopt(_M_MMAP_THRESHOLD, 128 * 1024)
        libc.mallopt(_M_TRIM_THRESHOLD, 64 * 1024)
    except (OSError, AttributeError):
        pass


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
    corpus: CorpusName,
    workers: int,
    limit: Optional[int],
    shards: Optional[int],
    checkpoint_interval: float,
    resume: bool,
    note=None,
):
    from . import corpus as corpora
    from .config import hf_token
    from .hub import HubShards
    from .pipeline import Trainer, TrainerConfig

    token = hf_token()
    resolved = corpora.resolve(corpus, token, note)
    if shards is not None:
        resolved = resolved.take(shards)
    config = TrainerConfig(mint_dir, workers, checkpoint_interval, limit, resume)
    return Trainer(resolved, HubShards(token), config)


def _run_until_done(build, resume: bool, view):
    from .errors import is_transient

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
    from .units import fmt_bytes

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
