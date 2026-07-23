import gzip
from pathlib import Path

from sngram_train.content import SwhContent


def test_local_content_reader_decompresses_one_bounded_object(tmp_path: Path):
    payload = b"def main():\n    return 42\n"
    with gzip.open(tmp_path / "blob", "wb") as handle:
        handle.write(payload)

    reader = SwhContent(f"file://{tmp_path}", workers=2)

    assert reader.read("blob", len(payload)) == payload


def test_content_reader_rejects_decompression_past_the_declared_length(tmp_path: Path):
    with gzip.open(tmp_path / "blob", "wb") as handle:
        handle.write(b"0123456789")

    reader = SwhContent(f"file://{tmp_path}", workers=1)

    try:
        reader.read("blob", 5)
    except ValueError as error:
        assert "declared" in str(error)
    else:
        raise AssertionError("oversized decompressed content should be rejected")


def test_bounded_gunzip_rejects_oversize_and_truncated_streams():
    import pytest

    from sngram_train.content import _gunzip_bounded

    payload = gzip.compress(b"x" * 100)
    assert _gunzip_bounded(payload, 100) == b"x" * 100
    with pytest.raises(ValueError, match="declared metadata length"):
        _gunzip_bounded(payload, 99)
    with pytest.raises(ValueError, match="complete gzip stream"):
        _gunzip_bounded(payload[:-5], 100)
    with pytest.raises(ValueError, match="complete gzip stream"):
        _gunzip_bounded(b"not gzip at all", 100)
