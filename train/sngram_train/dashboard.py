"""Live terminal view of the manifest and training phases."""

from __future__ import annotations

import os
import threading
import time
from collections import deque

from rich.console import Group
from rich.live import Live
from rich.panel import Panel
from rich.table import Table
from rich.text import Text

from .pipeline import Trainer
from .units import fmt_bytes, fmt_rate, mint_label


class RunView:
    """Shared render state fed by the manifest scan and the trainer."""

    def __init__(self) -> None:
        self.lock = threading.Lock()
        self.trainer: Trainer | None = None
        self.total_configs = 0
        self.done_configs = 0
        self.accepted_bytes = 0
        self.started_at = time.monotonic()
        self.active: dict[str, tuple[int, int, float]] = {}
        self.recent: deque[str] = deque(maxlen=4)

    def manifest_start(self, total: int) -> None:
        with self.lock:
            self.total_configs = total
            self.started_at = time.monotonic()

    def started(self, config: str) -> None:
        with self.lock:
            self.active[config] = (0, 0, time.monotonic())

    def scanned(self, config: str, rows: int, accepted_bytes: int) -> None:
        with self.lock:
            started = self.active.get(config, (0, 0, time.monotonic()))[2]
            self.active[config] = (rows, accepted_bytes, started)

    def finished(self, config: str, accepted: int, effective: int, seconds: float) -> None:
        with self.lock:
            self.active.pop(config, None)
            self.done_configs += 1
            self.accepted_bytes += effective
            self.recent.append(
                f"{config}: {accepted} objects, {fmt_bytes(effective)}, {seconds:.1f}s"
            )

    def training(self, trainer: Trainer) -> None:
        with self.lock:
            self.trainer = trainer

    def render(self):
        with self.lock:
            if self.trainer is not None:
                return render(self.trainer)
            return self._render_manifest()

    def _render_manifest(self) -> Panel:
        elapsed = time.monotonic() - self.started_at
        header = Text()
        header.append(
            f"manifest {self.done_configs}/{self.total_configs} configs",
            style="bold green",
        )
        header.append(f"   {fmt_bytes(self.accepted_bytes)} inventoried", style="cyan")
        header.append(f"   {elapsed:,.0f}s elapsed", style="dim")
        parts = [header, self._active_table()]
        if self.recent:
            parts.append(Text("\n".join(self.recent), style="dim"))
        return Panel(Group(*parts), title="sngram train", border_style="blue")

    def _active_table(self) -> Table:
        table = Table(box=None, pad_edge=False, header_style="dim")
        table.add_column("scanning", min_width=32)
        table.add_column("rows", justify="right")
        table.add_column("accepted", justify="right")
        table.add_column("elapsed", justify="right")
        now = time.monotonic()
        for config, (rows, accepted, started) in sorted(self.active.items()):
            table.add_row(
                config, f"{rows:,}", fmt_bytes(accepted), f"{now - started:.0f}s"
            )
        return table


def render(trainer: Trainer):
    goals = trainer.current_goals()
    threshold = trainer.current_threshold or trainer.effective_target
    parts = [_header(trainer, threshold), _areas(trainer, threshold), _formats(trainer, goals)]
    recent = _recent(trainer)
    if recent is not None:
        parts.append(recent)
    return Panel(Group(*parts), title="sngram train", border_style="blue")


def _header(trainer: Trainer, threshold: int) -> Text:
    header = Text()
    header.append(
        f"{fmt_bytes(trainer.counter.bytes_processed)} effective", style="bold green"
    )
    header.append(f" / {fmt_bytes(trainer.effective_target)}")
    header.append(f"   {fmt_bytes(trainer.fetched_bytes())} fetched", style="cyan")
    header.append(f"   now {fmt_rate(trainer.rate_now())}", style="cyan")
    header.append(f"   avg {fmt_rate(trainer.rate_avg())}")
    header.append(f"   mint {mint_label(threshold)}{_eta(trainer)}", style="magenta")
    header.append(f"   rss {fmt_bytes(_rss_bytes())}", style="dim")
    if trainer.skips:
        header.append(f"   skips {trainer.skips}", style="yellow")
    if trainer.last_kl is not None:
        header.append(f"   kl {trainer.last_kl:.5f}", style="cyan")
    if trainer.clamped:
        header.append("   clamped", style="bold red")
    return header


def _eta(trainer: Trainer) -> str:
    eta = trainer.eta_next_mint()
    if eta is None:
        return ""
    return f" in {int(eta // 3600)}:{int(eta % 3600 // 60):02d}:{int(eta % 60):02d}"


def _areas(trainer: Trainer, threshold: int) -> Table:
    targets = trainer.area_targets(threshold)
    actual = trainer.area_bytes()
    table = Table(box=None, pad_edge=False, header_style="dim")
    table.add_column("area", min_width=24)
    table.add_column("effective", justify="right")
    table.add_column("target", justify="right")
    table.add_column("fill", justify="right")
    for area, target in targets.items():
        amount = actual.get(area, 0)
        fill = amount / target if target else 1.0
        table.add_row(area, fmt_bytes(amount), fmt_bytes(target), f"{fill:6.1%}")
    return table


def _formats(trainer: Trainer, goals: dict[str, int]) -> Table:
    progress = trainer.format_bytes()
    order = sorted(goals, key=lambda key: (_ratio(progress[key], goals[key]), key))[:14]
    table = Table(box=None, pad_edge=False, header_style="dim")
    table.add_column("format", min_width=32)
    table.add_column("effective", justify="right")
    table.add_column("goal", justify="right")
    table.add_column("state", justify="right")
    for format_id in order:
        state = trainer.state.progress(format_id)
        fill = _ratio(progress[format_id], goals[format_id])
        label = "exhausted" if state.exhausted else f"{fill:.1%}"
        table.add_row(
            format_id, fmt_bytes(progress[format_id]), fmt_bytes(goals[format_id]), label
        )
    return table


def _ratio(value: int, target: int) -> float:
    return value / target if target else 1.0


def _recent(trainer: Trainer) -> Table | None:
    if not trainer.events.tail:
        return None
    table = Table(box=None, pad_edge=False, show_header=False)
    table.add_column(style="dim", width=16)
    table.add_column()
    for event in list(trainer.events.tail)[-4:]:
        detail = ", ".join(
            f"{key}={value}" for key, value in event.items() if key not in {"ts", "kind"}
        )
        table.add_row(str(event["kind"]), detail[:100])
    return table


def _rss_bytes() -> int:
    try:
        with open("/proc/self/statm", encoding="ascii") as handle:
            pages = int(handle.read().split()[1])
        return pages * os.sysconf("SC_PAGE_SIZE")
    except (OSError, ValueError, IndexError):
        return 0


class Dashboard:
    """Wires a run view into one live display across both phases."""

    def __init__(self, view: RunView) -> None:
        self.view = view
        self._live = Live(
            get_renderable=view.render, refresh_per_second=4, transient=False
        )

    def __enter__(self):
        self._live.__enter__()
        return self

    def __exit__(self, *exc):
        return self._live.__exit__(*exc)
