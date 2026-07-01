# Training Data Distribution Plan

Decision date: 2026-07-01.

Target run size: **15 TB counted text bytes**. The main goal is a code-heavy
sngram table with enough config, docs, and multilingual variation to reduce
false positives without letting natural-language sources dominate the final
byte-pair distribution.

## Final Distribution

| Bucket | Target | Sources |
| --- | ---: | --- |
| Pure code | 11.25 TB / 75% | `CodedotAI/code_clippy_github`, `nick007x/github-code-2025`, high-quality Stack v2 subset, config/markup slice |
| Code/text blend | 3.00 TB / 20% | repo docs/readmes/notebooks from GitHub code corpora, `OpenCoder-LLM/opc-fineweb-code-corpus`, selected `bigcode/starcoder2data-extras`, Stack Overflow |
| English docs | 0.30 TB / 2% | `HuggingFaceFW/finepdfs`, `eng_Latn` only |
| Multilingual text | 0.45 TB / 3% | `HuggingFaceFW/fineweb-2`, hard per-language caps |

Suggested pure-code allocation:

| Source | Cap |
| --- | ---: |
| `CodedotAI/code_clippy_github` | 8.30 TB |
| `nick007x/github-code-2025` | 2.30 TB |
| `M1keR/the-stack-v2-dedup-filtered-500-stars-100-forks-contents` | 0.20 TB |
| Config/markup slice | 0.45 TB |

## Config / Markup

Primary source: `bigcode/starcoderdata`. The logged-in HF CLI can see raw
parquet folders, and a schema check confirmed raw `content` plus repo/path
metadata for the filetype slices.

Use these folders first:

`markdown`, `html`, `json`, `yaml`, `css`, `sql`, `shell`, `makefile`,
`dockerfile`, `cmake`, `restructuredtext`, `tex`, `protocol-buffer`,
`powershell`, `batchfile`, `xslt`.

Known gap: `bigcode/starcoderdata` does not expose a `toml` folder. Fill TOML,
XML, lockfiles, CI configs, and other missing config types from path-filtered
`nick007x/github-code-2025` and/or `CodedotAI/code_clippy_github`.

Small raw focused datasets can be used as high-signal extras, not as volume:

| Dataset | Notes |
| --- | --- |
| `substratusai/the-stack-yaml-k8s` | raw Kubernetes YAML, `content`, about 0.9 GB decoded |
| `loubnabnl/dockerfile_checks` | raw Dockerfile content, about 0.57 GB decoded |
| `loubnabnl/makefile_checks` | raw Makefile content, about 0.71 GB decoded |

Do **not** use `verify-ppt/marin-starcoderdata_*` for this run. Those slices are
tokenized `.ds` shards for SmolLM3, not raw byte text.

## Blend Sources

The verified standalone code/text blend sources are much smaller than 3 TB, so
the blend bucket must combine repo-derived docs with code-adjacent public sets:

| Source | Use |
| --- | --- |
| `OpenCoder-LLM/opc-fineweb-code-corpus` | code-web blend, `text`, about 265 GB decoded |
| `bigcode/starcoder2data-extras` | use `documentation`, `issues`, `stackoverflow`, and possibly `owm`; avoid `arxiv` and `wikipedia` for this bucket |
| `mikex86/stackoverflow-posts` | Stack Overflow body/title/tags, about 45 GB decoded |
| GitHub path filters | README, docs, notebooks, examples, tutorials, comments-heavy markdown/code files |

If blend runs short, fill from pure code or repo documentation. Do not fill the
shortfall with multilingual web text.

## Multilingual Rules

Use `HuggingFaceFW/fineweb-2` only with hard caps. No source or language may
borrow another source's unused budget. Recommended guardrail: CJK total <= 60 GB
inside the 450 GB multilingual bucket, with each CJK language <= 20 GB.

This is the direct fix for the previous run: when code/English exhausted, the
planner renormalized among live multilingual families and the last terabytes
became multilingual-heavy, causing Japanese/Chinese byte-pairs to dominate.

## Exclusions

Remove or exclude these:

| Dataset | Reason |
| --- | --- |
| `Cyrile/dataset-the-stack-v2-dedup-sub` | removed from plan; unnecessary overlap |
| `SKT-NRS/ST-CODEX` | do not use per plan |
| `tokyotech-llm/swallow-code` | Python-only; not useful for balanced code coverage |
| `verify-ppt/marin-starcoderdata_*` | tokenized binary `.ds`, not raw byte text |

## Operational Guardrails

- Count decoded UTF-8 text bytes, not compressed parquet bytes.
- Hard-stop each bucket at its cap; never renormalize exhausted buckets.
- Deduplicate by normalized content hash across all code/config sources.
- Mirror/re-shard large parquet sources before training to avoid high RAM and
  rate-limit retries from huge remote shards.
- If a source is incomplete or unavailable, refill only from the same family or
  from pure code, never from multilingual text.

## References

- `CodedotAI/code_clippy_github`: https://huggingface.co/datasets/CodedotAI/code_clippy_github
- `nick007x/github-code-2025`: https://huggingface.co/datasets/nick007x/github-code-2025
- `M1keR/the-stack-v2-dedup-filtered-500-stars-100-forks-contents`: https://huggingface.co/datasets/M1keR/the-stack-v2-dedup-filtered-500-stars-100-forks-contents
- `bigcode/starcoderdata`: https://huggingface.co/datasets/bigcode/starcoderdata
- `OpenCoder-LLM/opc-fineweb-code-corpus`: https://huggingface.co/datasets/OpenCoder-LLM/opc-fineweb-code-corpus
- `bigcode/starcoder2data-extras`: https://huggingface.co/datasets/bigcode/starcoder2data-extras
- `mikex86/stackoverflow-posts`: https://huggingface.co/datasets/mikex86/stackoverflow-posts
- `HuggingFaceFW/finepdfs`: https://huggingface.co/datasets/HuggingFaceFW/finepdfs
- `HuggingFaceFW/fineweb-2`: https://huggingface.co/datasets/HuggingFaceFW/fineweb-2
