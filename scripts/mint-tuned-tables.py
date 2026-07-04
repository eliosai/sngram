#!/usr/bin/env python3
"""Mint tuned v2 weight tables from existing v1 bins.

The valley tuning is a pure weight transform (weight = total/count, discount =
weight/D floored), so re-minting needs no training run: boundary-class pairs
(identifier separators, the lowercase-to-uppercase seam, line terminators) get
their weight divided toward a low floor, landing them interior to grams so
compound identifiers and line edges yield bridging grams. Byte-for-byte the
same rule as sngram::learn::tune_weight.

Usage: mint-tuned-tables.py [--discount D] [--floor F] [--in-place | --out-dir DIR] BIN...
Writes into --out-dir (default /tmp/sngram-mint) unless --in-place; never leaves
experiment artifacts inside crates/weights/data, whose *.bin glob ships to crates.io.
"""

import argparse
import datetime
import struct
import sys
import zlib
from pathlib import Path

HEADER = 16
WEIGHTS = 65536
V1_SIZE = HEADER + WEIGHTS * 4
SEPARATORS = frozenset(b"_./-:")
TERMINATORS = frozenset(b"\n\r")


def is_boundary_pair(c1: int, c2: int) -> bool:
    if c1 in SEPARATORS or c2 in SEPARATORS:
        return True
    if c1 in TERMINATORS or c2 in TERMINATORS:
        return True
    return ord("a") <= c1 <= ord("z") and ord("A") <= c2 <= ord("Z")


def tune(raw: int, c1: int, c2: int, discount: int, floor: int) -> int:
    if discount <= 1 or not is_boundary_pair(c1, c2):
        return raw
    return max(raw // discount, floor)


def mint(path: Path, discount: int, floor: int, in_place: bool, out_dir: Path) -> Path:
    data = path.read_bytes()
    if len(data) != V1_SIZE or data[:4] != b"SPNG":
        raise SystemExit(f"{path}: not a v1 SPNG table")
    version = struct.unpack_from("<I", data, 4)[0]
    if version != 1:
        raise SystemExit(f"{path}: version {version}, expected 1")
    stored_crc = struct.unpack_from("<I", data, 8)[0]
    if zlib.crc32(data[HEADER:]) != stored_crc:
        raise SystemExit(f"{path}: checksum mismatch")

    weights = list(struct.unpack_from(f"<{WEIGHTS}I", data, HEADER))
    for i, raw in enumerate(weights):
        weights[i] = tune(raw, i >> 8, i & 0xFF, discount, floor)

    date = datetime.date.today().isoformat()
    prov = (
        f"src={path.name};src-crc={stored_crc:08x};"
        f"tuning=boundary/{discount}/{floor};date={date}"
    ).encode()
    body = struct.pack(f"<{WEIGHTS}I", *weights)
    body += struct.pack("<H", len(prov)) + prov
    out = bytearray(b"SPNG")
    out += struct.pack("<I", 2)
    out += struct.pack("<I", zlib.crc32(bytes(body)))
    out += b"\x00" * 4
    out += body

    target = path if in_place else out_dir / path.name
    target.write_bytes(bytes(out))
    return target


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--discount", type=int, default=16)
    ap.add_argument("--floor", type=int, default=1)
    ap.add_argument("--in-place", action="store_true")
    ap.add_argument("--out-dir", type=Path, default=Path("/tmp/sngram-mint"))
    ap.add_argument("bins", nargs="+", type=Path)
    args = ap.parse_args()
    if not args.in_place:
        args.out_dir.mkdir(parents=True, exist_ok=True)
    for path in args.bins:
        target = mint(path, args.discount, args.floor, args.in_place, args.out_dir)
        print(f"minted {target} (discount={args.discount} floor={args.floor})")


if __name__ == "__main__":
    main()
