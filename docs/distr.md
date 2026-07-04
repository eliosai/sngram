# Stack v2 / SWH Distribution Contract

Decision date: 2026-07-03.

Target run size: **12 TB counted UTF-8 text bytes**.

Only production data source:

- Metadata: `bigcode/the-stack-v2-dedup`, pinned to one Hugging Face revision.
- Content: `s3://softwareheritage/content/{blob_id}`, decoded with each row's
  `src_encoding`.

No CodeClippy, GitHub2025, FineWeb, FinePDFs, Wikipedia, Stack v1, or cloned
Stack subset sources are part of this run.

## Enforced Buckets

| Bucket | Cap | Share | Row routing |
| --- | ---: | ---: | --- |
| Core programming | 5.20 TB | 43.33% | C, C++, C#, Java, JavaScript, TypeScript, Python, PHP, Go, Rust, Ruby, Swift, Kotlin, Scala, Dart, Shell, Lua, R, Perl, Objective-C, Fortran, Pascal, Visual Basic, F#, Haskell, Clojure, Elixir, Erlang, OCaml, Julia, MATLAB, PowerShell |
| Docs / prose / markup | 2.30 TB | 19.17% | `Text`, Markdown, reStructuredText, TeX, Roff/manpages, Org, Wikitext, AsciiDoc, RMarkdown, Jupyter Notebook, BibTeX, docs/readme path overrides |
| Config / build / infra | 1.50 TB | 12.50% | JSON, YAML, XML, TOML, INI, Dockerfile, Makefile, CMake, Gradle, Maven POM, HCL, Nix, Git config/attrs/ignore, EditorConfig, lockfiles, workflow/config path overrides |
| Web / UI / templates | 1.20 TB | 10.00% | HTML, CSS, SCSS, Sass, Less, Vue, Svelte, Blade, EJS, JSP, ERB, Razor, Twig, Liquid, Handlebars, Pug, Haml, Astro, TSX |
| Data / query / schema | 1.00 TB | 8.33% | SQL, CSV, TSV, GraphQL, Protocol Buffer, Thrift, ASN.1, Avro IDL, Turtle/RDF/OWL, SPARQL, PLpgSQL, TSQL, data extension overrides |
| Long-tail floor | 0.80 TB | 6.67% | Every Stack v2 language not matched above |

Total hard cap: **12.00 TB**. The trainer does not mint `final` if the capped
roster exhausts below 12 TB.

## Row Filters

Rows are rejected before S3 fetch when:

- `is_vendor == true`
- `is_generated == true`
- required metadata fields are missing:
  `blob_id`, `content_id`, `src_encoding`, `language`
- `length_bytes <= 0`
- `length_bytes > 2 MiB`, except docs/prose rows which allow `4 MiB`
- classifier bucket does not match the active bucket source

The Stack v2 dedup dataset supplies near-deduplicated metadata. Runtime does not
keep a global `content_id` set because that would grow linearly across a 12 TB
run; distribution is enforced by source revision, bucket caps, row filters, and
exact counted-byte caps.

## Runtime Algorithm

1. Pin the Hugging Face metadata revision once and store it in checkpoints.
2. Resolve the default Stack v2 metadata parquet files. Bucket names are local
   distribution buckets, not Hugging Face dataset configs.
3. Planner round-robins the six bucket families by byte deficit.
4. Each bucket scans metadata shards, classifies rows by `language`,
   `extension`, and `path`, and fetches SWH content only for matching rows.
5. Content is gz-read from SWH, decoded with `src_encoding`, counted as UTF-8,
   and trimmed on the final row prefix if needed to land exactly on the cap.
6. Completed metadata shards and counted bytes are checkpointed with the roster
   hash, so resume cannot switch distribution or revision silently.
7. Object-level S3/fetch/decode failures are logged and skipped; metadata shard
   read failures use the existing shard retry path.

## Required Environment

- `HF_TOKEN`: Hugging Face token with access to `bigcode/the-stack-v2-dedup`.
- `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`: Software Heritage S3 access.
- `AWS_SESSION_TOKEN`: only if the issued SWH credentials are temporary.
- `AWS_REGION` / `AWS_DEFAULT_REGION`: optional; boto3 default chain applies.

Run command:

```bash
uv run --project python sngram train --mint-dir ../bins/
```

Defaults are `--target 12TB` and `--mint-every 1TB`.

## Telemetry

JSONL events emitted for SWH debugging:

- `swh_manifest_start`: source, bucket, metadata URL, pinned revision, content
  prefix, metadata field contract.
- `swh_manifest_done`: scanned metadata rows and seconds per shard.
- `swh_bucket_progress`: accepted bytes/objects, scanned rows, fetched and
  decoded bytes, skip counts by reason, fetch/decode errors.
- `s3_batch`: object counts, accepted bytes, decoded/fetched bytes, skip/error
  counts, latency p50/p95/max.
- `s3_object_error`: blob id, bucket, fetch/decode stage, error kind, bounded
  error text.
- `s3_slow_object`: sampled blobs slower than `SNG_SWH_SLOW_OBJECT_S`
  (default 15s).

References:

- Stack v2 dedup: https://huggingface.co/datasets/bigcode/the-stack-v2-dedup
- Stack v2 / StarCoder2 paper: https://arxiv.org/abs/2402.19173
- Software Heritage content prefix: `s3://softwareheritage/content/`
