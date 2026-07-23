"""Live terminal view of a training run."""

from __future__ import annotations

import os
import threading

from rich.console import Group
from rich.live import Live
from rich.panel import Panel
from rich.table import Table
from rich.text import Text

from .pipeline import Trainer
from .units import fmt_bytes, fmt_rate


class RunView:
    """Shared render state for one run."""

    def __init__(self) -> None:
        self.lock = threading.Lock()
        self.trainer: Trainer | None = None
        self.notes: list[str] = []

    def note(self, message: str) -> None:
        with self.lock:
            self.notes = (self.notes + [message])[-6:]

    def training(self, trainer: Trainer) -> None:
        with self.lock:
            self.trainer = trainer

    def render(self):
        with self.lock:
            if self.trainer is not None:
                return render(self.trainer)
            body = Text("\n".join(self.notes) or "preparing corpus manifest", style="dim")
            return Panel(body, title="sngram train", border_style="blue")


def render(trainer: Trainer):
    goals = trainer.current_goals()
    parts = [_header(trainer), _areas(trainer), _formats(trainer, goals)]
    recent = _recent(trainer)
    if recent is not None:
        parts.append(recent)
    return Panel(Group(*parts), title="sngram train", border_style="blue")


def _header(trainer: Trainer) -> Text:
    header = Text()
    header.append(
        f"{fmt_bytes(trainer.committed_bytes)} effective", style="bold green"
    )
    header.append(f" / {fmt_bytes(trainer.effective_target)}{_eta(trainer)}")
    header.append(f"   {fmt_bytes(trainer.fetched_bytes())} fetched", style="cyan")
    header.append(f"   now {fmt_rate(trainer.rate_now())}", style="cyan")
    header.append(f"   avg {fmt_rate(trainer.meter.rate_avg(trainer.committed_bytes))}")
    header.append(f"   rss {fmt_bytes(_rss_bytes())}", style="dim")
    if trainer.skips:
        header.append(f"   skips {trainer.skips}", style="yellow")
    if trainer.clamped:
        header.append("   clamped", style="bold red")
    return header


def _eta(trainer: Trainer) -> str:
    rate = trainer.rate_now()
    if rate <= 0:
        return ""
    eta = max(trainer.effective_target - trainer.committed_bytes, 0) / rate
    return f" in {int(eta // 3600)}:{int(eta % 3600 // 60):02d}:{int(eta % 60):02d}"


def _areas(trainer: Trainer) -> Table:
    targets = trainer.area_targets()
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
    """Wires a run view into one live display."""

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
