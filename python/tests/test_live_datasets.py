"""Opt-in HF availability checks for the production training roster.

Run with:

    HF_TOKEN=... SNG_HF_LIVE=1 uv run pytest -q tests/test_live_datasets.py

Normal CI/unit tests skip this file so the suite stays fast and offline.
"""

from __future__ import annotations

import os
from pathlib import Path

import pytest

from sngram.train.config import default_families
from sngram.train.pipeline import Trainer


pytestmark = pytest.mark.skipif(
    os.environ.get("SNG_HF_LIVE") != "1" or not os.environ.get("HF_TOKEN"),
    reason="set SNG_HF_LIVE=1 and HF_TOKEN to probe Hugging Face datasets",
)


def test_all_training_sources_resolve_and_expose_text_column(tmp_path: Path):
    trainer = Trainer(
        families=default_families(),
        mint_dir=tmp_path / "bins",
        target=1,
        mint_every=1,
        workers=1,
        limit=None,
        checkpoint_every_s=3600.0,
        resume=False,
    )
    try:
        trainer.preflight_sources()
    finally:
        trainer.events.close()
