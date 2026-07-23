"""Production corpus access."""

from __future__ import annotations

import os
from collections.abc import Iterable
from pathlib import Path

STACK_V2_CONTENT_PREFIX = "s3://softwareheritage/content/"


def hf_token() -> str | None:
    """Read the Hugging Face token from the environment or train/.env."""

    if token := os.environ.get("HF_TOKEN"):
        return token
    return _env_file_token((Path(".env"), _project_env_path()))


def _project_env_path() -> Path:
    return Path(__file__).resolve().parents[1] / ".env"


def _env_file_token(paths: Iterable[Path]) -> str | None:
    seen: set[Path] = set()
    for path in paths:
        resolved = path.resolve()
        if resolved in seen or not path.exists():
            continue
        seen.add(resolved)
        for line in path.read_text().splitlines():
            value = line.strip().removeprefix("export ").strip()
            if value.startswith("HF_TOKEN="):
                return value.removeprefix("HF_TOKEN=").strip().strip("\"'")
    return None
