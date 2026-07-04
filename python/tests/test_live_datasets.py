"""Opt-in live checks for Stack v2 metadata access.

Run with:

    HF_TOKEN=... SNG_HF_LIVE=1 uv run pytest -q tests/test_live_datasets.py

Normal CI/unit tests skip this file so the suite stays fast and offline.
"""

from __future__ import annotations

import os

import pytest

from sngram.train.config import STACK_V2_METADATA_REPO, STACK_V2_REQUIRED_COLUMNS


pytestmark = pytest.mark.skipif(
    os.environ.get("SNG_HF_LIVE") != "1" or not os.environ.get("HF_TOKEN"),
    reason="set SNG_HF_LIVE=1 and HF_TOKEN to probe Hugging Face metadata",
)


def test_stack_v2_metadata_resolves_and_exposes_required_columns():
    from datasets import load_dataset

    ds = load_dataset(
        STACK_V2_METADATA_REPO,
        split="train",
        streaming=True,
        token=os.environ["HF_TOKEN"],
    )
    row = next(iter(ds))

    assert set(STACK_V2_REQUIRED_COLUMNS) <= set(row)
    assert row["blob_id"]
    assert row["content_id"]
