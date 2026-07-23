"""Byte-size parsing and formatting in decimal units."""

from __future__ import annotations

_UNITS = {
    "B": 1,
    "KB": 10**3,
    "MB": 10**6,
    "GB": 10**9,
    "TB": 10**12,
    "KIB": 2**10,
    "MIB": 2**20,
    "GIB": 2**30,
    "TIB": 2**40,
}


def parse_size(text: str) -> int:
    """Parse "5TB" / "500GB" / "1000000" into bytes (decimal units)."""
    s = text.strip().upper().replace(" ", "")
    for unit in ("TIB", "GIB", "MIB", "KIB", "TB", "GB", "MB", "KB", "B"):
        if s.endswith(unit):
            return int(float(s[: -len(unit)]) * _UNITS[unit])
    return int(s)


def fmt_bytes(n: float) -> str:
    """Render a byte count with a sensible decimal unit."""
    for unit, scale in (("TB", 10**12), ("GB", 10**9), ("MB", 10**6), ("KB", 10**3)):
        if n >= scale:
            return f"{n / scale:.2f} {unit}"
    return f"{n:.0f} B"


def fmt_rate(bytes_per_s: float) -> str:
    return f"{bytes_per_s / 10**6:,.0f} MB/s"
