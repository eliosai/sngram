"""Filesystem validation: measure the byte-pair distribution of the real files
a regex search runs over (text only, binaries skipped) and score a minted table
against it."""

from __future__ import annotations

from pathlib import Path

import sngram
from sngram.train import fsvalidate, metrics


def test_is_binary_detects_nul():
    assert fsvalidate.is_binary(b"text\x00more") is True
    assert fsvalidate.is_binary(b"plain ascii text") is False
    assert fsvalidate.is_binary(b"") is False


def test_byte_pair_counts_match_handcount():
    counts = fsvalidate.byte_pair_counts([b"aab"])
    assert counts[(ord("a") << 8) | ord("a")] == 1
    assert counts[(ord("a") << 8) | ord("b")] == 1
    assert sum(counts) == 2


def test_byte_pairs_do_not_straddle_files():
    # two separate files must not fabricate a pair across their boundary
    counts = fsvalidate.byte_pair_counts([b"ab", b"cd"])
    assert counts[(ord("b") << 8) | ord("c")] == 0
    assert counts[(ord("a") << 8) | ord("b")] == 1
    assert counts[(ord("c") << 8) | ord("d")] == 1


def _write(p: Path, data: bytes) -> None:
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_bytes(data)


def test_filesystem_histogram_skips_binaries(tmp_path: Path):
    _write(tmp_path / "a.py", b"print('hello world')\n")
    _write(tmp_path / "b.txt", b"some plain text here\n")
    _write(tmp_path / "lib.so", b"\x7fELF\x00\x00\x00binary\x00stuff")  # NUL -> binary
    counts, stats = fsvalidate.filesystem_histogram([str(tmp_path)])
    assert stats.files == 2
    assert stats.skipped_binary == 1
    # the ELF magic bytes must not appear in the histogram
    assert counts[(0x7F << 8) | ord("E")] == 0
    # real text pairs are present
    assert counts[(ord("l") << 8) | ord("o")] > 0


def test_filesystem_histogram_counts_match_reference(tmp_path: Path):
    _write(tmp_path / "x.txt", b"the quick brown fox")
    _write(tmp_path / "y.txt", b"jumps over the lazy dog")
    counts, _ = fsvalidate.filesystem_histogram([str(tmp_path)])
    ref = sngram.BigramCounter()
    ref.process(b"the quick brown fox")
    ref.process(b"jumps over the lazy dog")
    expected = metrics.counts_from_snapshot(ref.snapshot())
    assert counts == expected


def test_validate_flags_pair_the_corpus_underrepresents(tmp_path: Path):
    # corpus (table) saw only 'aa'; the filesystem is full of 'zq' the corpus
    # barely produced -> 'zq' must surface as under-weighted, and KL > 0
    corpus = sngram.BigramCounter()
    for _ in range(500):
        corpus.process(b"aaaaaaaa")
    table = sngram.WeightTable.from_bytes(corpus.to_table_bytes())

    fs_counts = metrics.counts_from_snapshot(_snap_of(b"zq" * 500))
    report = fsvalidate.validate(fs_counts, table, top=10)
    assert report.kl > 0.0
    under_pairs = {(c1, c2) for (c1, c2), *_ in report.under_weighted}
    assert (ord("z"), ord("q")) in under_pairs


def test_validate_ranks_by_divergence_contribution_not_floor_noise(tmp_path: Path):
    # 'xy' is frequent on disk and the corpus produced only a little of it (a
    # real, actionable under-representation). 'QZ' is a single stray byte-pair
    # the corpus never saw (q at the floor) -> a raw log-ratio ranking would
    # rank 'QZ' ABOVE 'xy' (huge log-ratio from the floor), which is noise. The
    # report must rank the high-contribution 'xy' first.
    corpus = sngram.BigramCounter()
    for _ in range(1000):
        corpus.process(b"thththththth")  # 'th'/'ht' everywhere
    corpus.process(b"xyxyxy")            # 'xy' seen, but only a little
    table = sngram.WeightTable.from_bytes(corpus.to_table_bytes())

    fs = sngram.BigramCounter()
    fs.process(b"xy" * 1000)  # disk is full of 'xy'
    fs.process(b"QZ")         # one stray pair the corpus never saw
    fs_counts = metrics.counts_from_snapshot(fs.snapshot())

    report = fsvalidate.validate(fs_counts, table, top=5)
    pairs = [p for p, *_ in report.under_weighted]
    xy, yx, qz = (ord("x"), ord("y")), (ord("y"), ord("x")), (ord("Q"), ord("Z"))
    # the frequent 'xy'/'yx' (the repeated text emits both) lead; the floor-noise
    # 'QZ' is ranked below them, not above
    assert pairs[0] in {xy, yx}, f"floor-noise led the ranking: {pairs}"
    assert xy in pairs
    if qz in pairs:
        assert pairs.index(xy) < pairs.index(qz), f"floor-noise outranked xy: {pairs}"


def _snap_of(data: bytes) -> bytes:
    c = sngram.BigramCounter()
    c.process(data)
    return c.snapshot()
