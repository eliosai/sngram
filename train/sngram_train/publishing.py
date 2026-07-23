"""Manifest build and publish commands over the assets repo."""

from __future__ import annotations

import os
from pathlib import Path
from typing import Optional

import typer

from .assets import assets_repo
from .config import CANONICAL_TARGET_BYTES, hf_token

manifest_app = typer.Typer(
    add_completion=False,
    no_args_is_help=True,
    help="Build and publish the sampled corpus manifest.",
)


def hub_timeouts() -> None:
    os.environ.setdefault("HF_HUB_DOWNLOAD_TIMEOUT", "30")
    os.environ.setdefault("HF_HUB_ETAG_TIMEOUT", "30")


def open_trained_manifest(path: Path):
    """Open a manifest for training, adopting a legacy roster in place."""

    from .catalog import build_catalog, legacy_roster_hash
    from .config import STACK_V2_REVISION
    from .errors import ConfigurationError
    from .manifest import adopt_manifest, open_manifest, stored_format_ids

    configs = sorted({fid.split("/", 1)[1] for fid in stored_format_ids(path)})
    catalog = build_catalog(configs)
    roster = catalog.roster_hash(STACK_V2_REVISION)
    try:
        return catalog, open_manifest(path, roster)
    except ConfigurationError:
        legacy = legacy_roster_hash(catalog, STACK_V2_REVISION, CANONICAL_TARGET_BYTES)
        if not adopt_manifest(path, roster, legacy, CANONICAL_TARGET_BYTES):
            raise
        return catalog, open_manifest(path, roster)


@manifest_app.command("build")
def manifest_build(
    mint_dir: Path = typer.Option(Path("./bins"), help="Manifest directory."),
    target: str = typer.Option("10TB", help="Corpus bytes the manifest must cover."),
    revision: Optional[str] = typer.Option(None, help="Stack revision; defaults to the pin."),
    workers: Optional[int] = typer.Option(None, help="Concurrent metadata readers."),
    publish: bool = typer.Option(False, help="Upload to the assets repo afterwards."),
    dashboard: bool = typer.Option(True, help="Show live scan progress."),
) -> None:
    """Scan Stack metadata into a sampled manifest (one-time, slow)."""

    from .catalog import build_catalog
    from .config import STACK_V2_BUCKET_CAPS, STACK_V2_REVISION
    from .resources import default_workers, scan_workers
    from .stackrows import HuggingFaceRows
    from .units import parse_size

    token = _require_token()
    path = mint_dir / ".manifest.sqlite3"
    if path.exists():
        typer.echo("manifest already built")
    else:
        rows = HuggingFaceRows(token, revision or STACK_V2_REVISION)
        catalog = build_catalog(rows.configs())
        parsed = parse_size(target)
        _check_disk_budget(path, catalog, parsed)
        readers = scan_workers(workers or default_workers())
        _build_with_view(
            path, catalog, rows, parsed, STACK_V2_BUCKET_CAPS, readers, dashboard
        )
    if publish:
        _publish(mint_dir)


@manifest_app.command("publish")
def manifest_publish(
    mint_dir: Path = typer.Option(Path("./bins"), help="Manifest directory."),
) -> None:
    """Upload the local manifest to the assets repo."""

    _publish(mint_dir)


def _require_token() -> str:
    hub_timeouts()
    token = hf_token()
    if token is None:
        typer.echo("error: HF_TOKEN is required for the production corpus")
        raise typer.Exit(2)
    return token


def _publish(mint_dir: Path) -> None:
    from .assets import publish_dataset
    from .config import STACK_V2_REVISION

    token = _require_token()
    path = mint_dir / ".manifest.sqlite3"
    if not path.exists():
        typer.echo("error: no local manifest to publish")
        raise typer.Exit(2)
    _catalog, manifest = open_trained_manifest(path)
    manifest.close()
    if manifest.revision != STACK_V2_REVISION:
        typer.echo(f"error: manifest revision {manifest.revision[:12]} is not the pin")
        raise typer.Exit(2)
    typer.echo(f"exporting and uploading the manifest dataset to {assets_repo()}")
    repo = publish_dataset(path, token)
    typer.echo(f"published dataset to {repo}")


def _build_with_view(path, catalog, rows, target, area_weights, workers, dashboard):
    total = len(catalog.configs)
    if not dashboard:
        typer.echo(f"building sampled manifest for {total} Stack configs ({workers} readers)")
        _build_manifest(path, catalog, rows, target, area_weights, workers, None, total)
        return
    from .dashboard import Dashboard, RunView

    view = RunView()
    view.manifest_start(total)
    with Dashboard(view):
        _build_manifest(path, catalog, rows, target, area_weights, workers, view, total)


def _build_manifest(path, catalog, rows, target, area_weights, workers, view, total):
    from .events import EventLog
    from .stack import build_stack_manifest

    events = EventLog(path.parent / "train-events.jsonl")
    events.log(
        "manifest_start",
        revision=rows.revision,
        configs=total,
        formats=len(catalog.formats),
    )
    try:
        build_stack_manifest(
            path,
            catalog,
            rows,
            _ManifestReport(view, events, total),
            target=target,
            area_weights=area_weights,
            workers=workers,
        )
        events.log("manifest_done", bytes=path.stat().st_size)
    finally:
        events.close()


def _check_disk_budget(path: Path, catalog, target: int) -> None:
    from .errors import ConfigurationError
    from .resources import manifest_disk_budget

    config_counts: dict[str, int] = {}
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


class _ManifestReport:
    """Routes scan progress to the dashboard view or plain lines."""

    def __init__(self, view, events, total: int) -> None:
        self.view = view
        self.events = events
        self.total = total
        self.done: set[str] = set()

    def started(self, config: str) -> None:
        if self.view is not None:
            self.view.started(config)

    def scanned(self, config: str, rows: int, accepted_bytes: int) -> None:
        if self.view is not None:
            self.view.scanned(config, rows, accepted_bytes)

    def finished(self, config: str, accepted: int, effective: int, seconds: float) -> None:
        self.done.add(config)
        self.events.log(
            "manifest_config",
            config=config,
            candidates=accepted,
            effective_bytes=effective,
            seconds=round(seconds, 3),
        )
        if self.view is not None:
            self.view.finished(config, accepted, effective, seconds)
        else:
            typer.echo(
                f"manifest {len(self.done)}/{self.total}: {config} "
                f"({accepted} objects, {seconds:.1f}s)"
            )
