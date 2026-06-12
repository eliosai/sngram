"""Live rich dashboard for a training run."""

from __future__ import annotations

import time

from rich.console import Group
from rich.live import Live
from rich.panel import Panel
from rich.table import Table
from rich.text import Text

from .units import fmt_bytes, fmt_rate, mint_label
from .pipeline import Trainer, rss_bytes


def render(t: Trainer):
    # headline = everything counted so far (durable + in-flight). It advances
    # every batch, so the number visibly moves and stays in step with the ETA.
    header = Text()
    header.append(f"  {fmt_bytes(t.total_bytes())}", style="bold green")
    header.append(f" / {fmt_bytes(t.target)}")
    header.append(f"   now {fmt_rate(t.rate_now())}", style="cyan")
    header.append(f"   avg {fmt_rate(t.rate_avg())}")
    next_label = mint_label(t.thresholds[0]) if t.thresholds else "done"
    header.append(f"   next mint {next_label} in {t.eta_next_mint()}", style="magenta")
    header.append(f"   mints {len(t.state.mints_done)}")
    if t.last_kl is not None:
        # KL from the previous mint: shrinking toward 0 means the table has
        # converged and the run can stop early
        header.append(f"   kl {t.last_kl:.4f}", style="cyan")
    header.append(f"   shards {t.counter.files_processed}")
    header.append(f"   rss {fmt_bytes(rss_bytes())}", style="dim")
    if t.errors:
        header.append(f"   errors {t.errors}", style="yellow")
    if t.failed_shards:
        header.append(f"   failed {t.failed_shards}", style="red")

    workers = Table(box=None, pad_edge=False, show_header=True, header_style="dim")
    workers.add_column("#", width=3)
    workers.add_column("shard", min_width=34)
    workers.add_column("read", justify="right", width=11)
    workers.add_column("speed", justify="right", width=11)
    workers.add_column("quiet", justify="right", width=7)
    now = time.monotonic()
    for i, ws in enumerate(t.worker_state):
        if ws.task == "idle":
            workers.add_row(str(i), Text("idle", style="dim"), "", "", "")
            continue
        elapsed = max(now - ws.started, 1e-6)
        quiet = now - ws.last_progress
        style = "red" if ws.stalled else ("yellow" if quiet > 30 else "")
        workers.add_row(
            str(i),
            Text(ws.task, style=style),
            fmt_bytes(ws.shard_bytes),
            fmt_rate(ws.shard_bytes / elapsed),
            Text(f"{quiet:.0f}s", style=style),
        )

    lines = [header, workers]
    if t.events.tail:
        recent = Table(box=None, pad_edge=False, show_header=False)
        recent.add_column(style="dim", width=9)
        recent.add_column()
        for ev in list(t.events.tail)[-5:]:
            kind = ev["kind"]
            style = {"error": "red", "warn": "yellow", "stall": "red", "mint": "bold green"}.get(
                kind, "dim"
            )
            detail = ", ".join(f"{k}={v}" for k, v in ev.items() if k not in {"ts", "kind"})
            recent.add_row(Text(kind, style=style), Text(detail[:110], style=style))
        lines.append(recent)

    return Panel(Group(*lines), title="sngram train", border_style="blue")


class Dashboard:
    """Wires a rich Live display into the trainer's refresh hook."""

    def __init__(self) -> None:
        self._live = Live(refresh_per_second=4, transient=False)

    def __enter__(self):
        self._live.__enter__()
        return self

    def __exit__(self, *exc):
        return self._live.__exit__(*exc)

    def refresh(self, trainer: Trainer) -> None:
        self._live.update(render(trainer))
