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
