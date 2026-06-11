"""Training corpus: which Hugging Face datasets feed the counter.

A *family* is one dataset repo; a *source* is one streamable unit inside it
(a config/language subset, or the whole repo). Sources shard by file, which
is the unit of work, retry, and resume.
"""

from __future__ import annotations

import os
from dataclasses import dataclass, field
from pathlib import Path

# Language subsets streamed from finepdfs and fineweb-2 (their config names).
WEB_LANGS = [
    # top-tier global
    # NB: finepdfs/fineweb-2 publish individual-language ISO codes, not
    # macrolanguage codes — e.g. Arabic is arb_Arab (not ara_Arab). The codes
    # below are validated against both repos' available configs.
    "eng_Latn", "cmn_Hani", "spa_Latn", "arb_Arab", "fra_Latn", "rus_Cyrl", "por_Latn",
    "deu_Latn", "jpn_Jpan", "ita_Latn", "kor_Hang", "tur_Latn", "vie_Latn", "pol_Latn",
    "nld_Latn", "ind_Latn", "fas_Arab", "ukr_Cyrl", "ces_Latn", "swe_Latn", "ron_Latn",
    "hun_Latn", "ell_Grek", "dan_Latn", "fin_Latn", "tha_Thai", "heb_Hebr", "nob_Latn",
    # South Asian
    "hin_Deva", "ben_Beng", "tam_Taml", "tel_Telu", "mar_Deva", "guj_Gujr", "kan_Knda",
    "mal_Mlym", "pan_Guru", "sin_Sinh", "urd_Arab", "npi_Deva", "asm_Beng", "ory_Orya",
    # SE Asian
    "zsm_Latn", "jav_Latn", "sun_Latn", "fil_Latn", "ceb_Latn", "khm_Khmr", "mya_Mymr",
    "lao_Laoo",
    # East Asian extras
    "yue_Hani",
    # European (more)
    "slk_Latn", "bul_Cyrl", "srp_Cyrl", "hrv_Latn", "bos_Latn", "slv_Latn", "lit_Latn",
    "lvs_Latn", "ekk_Latn", "isl_Latn", "cat_Latn", "glg_Latn", "eus_Latn", "gle_Latn",
    "cym_Latn", "mlt_Latn", "als_Latn", "mkd_Cyrl", "bel_Cyrl", "afr_Latn",
    # Caucasus / Central Asia / Middle East extras
    "kat_Geor", "hye_Armn", "azj_Latn", "kaz_Cyrl", "uzn_Latn", "kir_Cyrl", "tgk_Cyrl",
    "pbt_Arab", "ckb_Arab",
    # African
    "swh_Latn", "hau_Latn", "yor_Latn", "ibo_Latn", "amh_Ethi", "zul_Latn", "xho_Latn",
    "som_Latn", "sna_Latn",
    # misc
    "lat_Latn", "epo_Latn",
]


@dataclass(frozen=True)
class Source:
    """One streamable unit: repo + optional config, with its text field."""

    family: str
    repo: str
    text_field: str
    config: str | None = None
    # fallback for repos the standard loader can't stream (script datasets):
    # a hf:// parquet glob loaded through the generic parquet builder
    data_files: str | None = None

    @property
    def id(self) -> str:
        return f"{self.family}/{self.config}" if self.config else self.family


@dataclass(frozen=True)
class Family:
    """One dataset repo, expanded into its sources."""

    id: str
    sources: tuple[Source, ...] = field(default_factory=tuple)


def default_families() -> list[Family]:
    """The 50 TB training mix: code-heavy, blended with multilingual web text."""

    def fam(fid: str, sources: list[Source]) -> Family:
        return Family(id=fid, sources=tuple(sources))

    return [
        fam("the-stack", [Source("the-stack", "bigcode/the-stack", "content")]),
        fam(
            "finepdfs",
            [
                Source("finepdfs", "HuggingFaceFW/finepdfs", "text", config=lang)
                for lang in WEB_LANGS
            ],
        ),
        fam(
            "fineweb-2",
            [
                # fineweb-2 is the multilingual (non-English) set: no eng_Latn
                # config exists there; English web text comes via finepdfs
                Source("fineweb-2", "HuggingFaceFW/fineweb-2", "text", config=lang)
                for lang in WEB_LANGS
                if lang != "eng_Latn"
            ],
        ),
        fam(
            "starcoderdata",
            [Source("starcoderdata", "bigcode/starcoderdata", "content")],
        ),
        fam(
            "github-code",
            [
                # script-based dataset: stream its parquet files directly
                Source(
                    "github-code",
                    "codeparrot/github-code",
                    "content",
                    data_files="hf://datasets/codeparrot/github-code/data/*.parquet",
                )
            ],
        ),
    ]


def hf_token() -> str | None:
    """HF_TOKEN from the environment or a local .env file."""
    if tok := os.environ.get("HF_TOKEN"):
        return tok
    env = Path(".env")
    if env.exists():
        for line in env.read_text().splitlines():
            if line.startswith("HF_TOKEN="):
                return line.removeprefix("HF_TOKEN=").strip()
    return None
