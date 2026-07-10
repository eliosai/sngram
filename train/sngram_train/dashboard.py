"""Live terminal view of durable training progress."""

from __future__ import annotations

import os

from rich.console import Group
from rich.live import Live
from rich.panel import Panel
from rich.table import Table
from rich.text import Text

from .distribution import apportion
from .pipeline import Trainer
from .units import fmt_bytes, fmt_rate, mint_label


def render(trainer: Trainer):
    goals = trainer.current_goals()
    threshold = trainer.current_threshold or trainer.config.target
    parts = [_header(trainer, threshold), _areas(trainer, threshold), _formats(trainer, goals)]
    recent = _recent(trainer)
    if recent is not None:
        parts.append(recent)
    return Panel(Group(*parts), title="sngram train", border_style="blue")


def _header(trainer: Trainer, threshold: int) -> Text:
    effective = trainer.counter.bytes_processed
    fetched = trainer.fetched_bytes()
    header = Text()
    header.append(f"{fmt_bytes(effective)} effective", style="bold green")
    header.append(f" / {fmt_bytes(trainer.config.target)}")
    header.append(f"   {fmt_bytes(fetched)} fetched", style="cyan")
    header.append(f"   {fmt_rate(trainer.rate_avg())}")
    header.append(f"   mint {mint_label(threshold)}", style="magenta")
    header.append(f"   rss {fmt_bytes(_rss_bytes())}", style="dim")
    if trainer.last_kl is not None:
        header.append(f"   kl {trainer.last_kl:.5f}", style="cyan")
    return header


def _areas(trainer: Trainer, threshold: int) -> Table:
    targets = apportion(threshold, trainer.area_weights)
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
        table.add_row(format_id, fmt_bytes(progress[format_id]), fmt_bytes(goals[format_id]), label)
    return table


def _ratio(value: int, target: int) -> float:
    return value / target if target else 1.0


def _recent(trainer: Trainer) -> Table | None:
    if not trainer.events.tail:
        return None
    table = Table(box=None, pad_edge=False, show_header=False)
    table.add_column(style="dim", width=14)
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
    def __init__(self) -> None:
        self._live = Live(refresh_per_second=4, transient=False)

    def __enter__(self):
        self._live.__enter__()
        return self

    def __exit__(self, *exc):
        return self._live.__exit__(*exc)

    def refresh(self, trainer: Trainer) -> None:
        self._live.update(render(trainer))
