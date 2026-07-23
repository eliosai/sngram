"""Stack v2 configuration discovery and area routing."""

from __future__ import annotations

import hashlib
import json
from dataclasses import dataclass

from .config import (
    AREA_FORMAT_SHARE,
    CONFIG_LANGUAGES,
    CORE_LANGUAGES,
    DATA_LANGUAGES,
    DOC_LANGUAGES,
    EXCLUDED_CONFIGS,
    STACK_V2_BUCKET_CAPS,
    WEB_LANGUAGES,
    stack_config_name,
)

TEXT_AREAS = ("docs-prose-markup", "config-build-infra", "data-query-schema")


@dataclass(frozen=True)
class FormatSpec:
    id: str
    area: str
    config: str
    cap_bytes: int


@dataclass(frozen=True)
class Catalog:
    formats: tuple[FormatSpec, ...]
    configs: tuple[str, ...]

    def format(self, format_id: str) -> FormatSpec:
        for item in self.formats:
            if item.id == format_id:
                return item
        raise KeyError(format_id)

    def roster_hash(self, revision: str) -> str:
        payload = [(item.id, item.config, item.cap_bytes) for item in self.formats]
        raw = json.dumps([revision, payload], separators=(",", ":")).encode()
        return hashlib.sha256(raw).hexdigest()


def build_catalog(configs: list[str]) -> Catalog:
    """Assign every physical Stack config to one or more logical formats."""

    available = sorted(set(configs) - {"default"} - EXCLUDED_CONFIGS)
    formats: list[FormatSpec] = []
    for config in available:
        if config == "Text":
            formats.extend(_text_formats())
            continue
        area = _config_areas().get(config, "long-tail")
        formats.append(_format(area, config))
    return Catalog(tuple(sorted(formats, key=lambda item: item.id)), tuple(available))


def _text_formats() -> list[FormatSpec]:
    return [_format(area, "Text") for area in TEXT_AREAS]


def _format(area: str, config: str) -> FormatSpec:
    area_cap = STACK_V2_BUCKET_CAPS[area]
    return FormatSpec(
        f"{area}/{config}", area, config, int(area_cap * AREA_FORMAT_SHARE[area])
    )


def _config_areas() -> dict[str, str]:
    groups = {
        "core-programming": CORE_LANGUAGES,
        "docs-prose-markup": DOC_LANGUAGES - {"Text"},
        "config-build-infra": CONFIG_LANGUAGES,
        "web-ui-templates": WEB_LANGUAGES,
        "data-query-schema": DATA_LANGUAGES,
    }
    return {
        config: area
        for area, languages in groups.items()
        for language in languages
        if (config := stack_config_name(language)) is not None
    }
