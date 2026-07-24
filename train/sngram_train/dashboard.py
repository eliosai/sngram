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
            body = Text("\n".join(self.notes) or "opening corpus stream", style="dim")
            return Panel(body, title="sngram train", border_style="blue")


def render(trainer: Trainer):
    parts = [_header(trainer), _groups(trainer)]
    recent = _recent(trainer)
    if recent is not None:
        parts.append(recent)
    return Panel(Group(*parts), title="sngram train", border_style="blue")


def _header(trainer: Trainer) -> Text:
    done = trainer.committed_bytes / max(trainer.effective_target, 1)
    header = Text()
    header.append(
        f"{fmt_bytes(trainer.committed_bytes)} effective", style="bold green"
    )
    header.append(f" / {fmt_bytes(trainer.effective_target)} ({done:.1%}){_eta(trainer)}")
    header.append(f"   {fmt_bytes(trainer.state.fetched)} fetched", style="cyan")
    header.append(f"   now {fmt_rate(trainer.rate_now())}", style="cyan")
    header.append(f"   avg {fmt_rate(trainer.meter.rate_avg(trainer.committed_bytes))}")
    header.append(f"   rows {trainer.state.rows:,}", style="dim")
    header.append(f"   rss {fmt_bytes(_rss_bytes())}", style="dim")
    if trainer.skips:
        header.append(f"   skips {trainer.skips}", style="yellow")
    return header


def _eta(trainer: Trainer) -> str:
    rate = trainer.rate_now()
    if rate <= 0:
        return ""
    eta = max(trainer.effective_target - trainer.committed_bytes, 0) / rate
    return f" in {int(eta // 3600)}:{int(eta % 3600 // 60):02d}:{int(eta % 60):02d}"


def _groups(trainer: Trainer) -> Table:
    corpus = trainer.corpus.groups
    total = sum(corpus.values()) or 1
    committed = trainer.group_bytes()
    trained = sum(committed.values()) or 1
    table = Table(box=None, pad_edge=False, header_style="dim")
    table.add_column("group", min_width=10)
    table.add_column("effective", justify="right")
    table.add_column("share", justify="right")
    table.add_column("target", justify="right")
    for group, target in sorted(corpus.items(), key=lambda item: -item[1]):
        amount = committed.get(group, 0)
        table.add_row(
            group,
            fmt_bytes(amount),
            f"{amount / trained:6.1%}",
            f"{100 * target / total:4.1f}%",
        )
    return table


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
