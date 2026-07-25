"""Live terminal view of a training run."""

from __future__ import annotations

import os
import threading
import time

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
        self.started_at = time.monotonic()

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
            waited = time.monotonic() - self.started_at
            lines = self.notes or ["resolving corpus"]
            body = Text("\n".join(lines) + f"\nwaiting {waited:.0f}s", style="dim")
            return Panel(body, title="sngram train", border_style="blue")


def render(trainer: Trainer):
    parts = [_header(trainer), _mix(trainer)]
    recent = _recent(trainer)
    if recent is not None:
        parts.append(recent)
    return Panel(Group(*parts), title="sngram train stack-v3", border_style="blue")


def _header(trainer: Trainer) -> Text:
    header = Text()
    header.append(f"{fmt_bytes(trainer.state.decoded)} decoded", style="bold green")
    header.append(f" ({trainer.progress():.1%}){_eta(trainer)}")
    header.append(f"   now {fmt_rate(trainer.rate_now())}", style="cyan")
    header.append(f"   avg {fmt_rate(trainer.rate_avg())}")
    header.append(
        f"   wire now {fmt_rate(trainer.wire_rate_now())}"
        f" avg {fmt_rate(trainer.wire_rate_avg())}",
        style="dim",
    )
    header.append(f"\nrepos {trainer.state.repos:,}")
    header.append(f"   files {trainer.counter.files_processed:,}")
    header.append(f"   vendor {trainer.state.vendor_files:,}", style="dim")
    header.append(f"   rss {fmt_bytes(_rss_bytes())}", style="dim")
    if trainer.retries:
        header.append(f"   retries {trainer.retries}", style="yellow")
    return header


def _eta(trainer: Trainer) -> str:
    eta = trainer.eta_seconds()
    if eta is None:
        return ""
    return f" in {int(eta // 3600)}:{int(eta % 3600 // 60):02d}:{int(eta % 60):02d}"


def _mix(trainer: Trainer) -> Text:
    ranked = trainer.language_mix(10)
    if not ranked:
        return Text("mix pending", style="dim")
    line = "  ".join(f"{language} {share:.1%}" for language, share in ranked)
    return Text(line, style="dim")


def _recent(trainer: Trainer) -> Table | None:
    if not trainer.events.tail:
        return None
    table = Table(box=None, pad_edge=False, show_header=False)
    table.add_column(style="dim", width=8)
    table.add_column()
    for event in list(trainer.events.tail)[-3:]:
        detail = ", ".join(
            f"{key}={value}"
            for key, value in event.items()
            if key not in {"ts", "kind", "languages"}
        )
        table.add_row(str(event["kind"]), detail[:110])
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
