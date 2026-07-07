# eg implementation plan

`eg` starts as ripgrep plus a required sparse n-gram prefilter for indexed runs. We import ripgrep crates where public APIs fit. We copy ripgrep binary internals where ripgrep keeps APIs private. We do not rewrite ripgrep behavior.

## Rules

- Keep `eg --no-index` behavior identical to `rg`.
- Copy ripgrep internals mechanically. Patch only named extension points.
- Track the ripgrep commit in source and tests.
- Use unprefixed flags: `--no-index`, `--index`, `--index-backend`.
- Use no silent fallback in indexed modes. Unsupported indexed searches return an error.
- Treat exact ripgrep verification as part of indexed search, not as fallback.
- Use raw CLI and Divan benchmarks for performance work. Report command lines,
  corpus, output mode, and cold/hot daemon state with the timing numbers.

## CLI modes

Bench and support these modes:

```text
eg --no-index
eg
eg --bench
```

The default path uses a daemon-owned sparse n-gram index. `--no-index` runs the
copied ripgrep scan path. `--bench` runs the real indexed path and reports the
daemon cold/hot phases separately.

Supported index flags:

```text
--no-index
--index-dir PATH
--index-backend=postings|tantivy
--bench
--bench-suite
```

There is no foreground rebuild, verify, repair, or require mode. If no
daemon-proofed index exists, foreground `eg` may block once while the daemon
builds and publishes one. After that, every usable index must be daemon-owned.

## Ripgrep integration

1. Import public crates: `grep`, `grep-searcher`, `grep-printer`, `grep-regex`, `grep-pcre2`, `ignore`, `globset`.
2. Copy ripgrep `crates/core` modules for private binary behavior: flags, haystacks, search workers, printers, messages, completion generation.
3. Add one extension point after ripgrep builds haystacks and before workers search files.
4. Keep ripgrep printers responsible for stdout, stderr, JSON, colors, stats, and exit codes.

Search flow: parse rg args, run copied rg path for `--no-index`, otherwise plan
sparse grams, resolve the best daemon-proofed generation from disk, mmap/open
the manifest and backend, query candidate ordinals, restrict candidates to the
requested roots, verify candidates with ripgrep workers, and print with ripgrep
printers.

## Index design

The production backend is eg's compact mmap-backed postings index. Disk Tantivy
remains experimental. In-memory index backends are not part of the daemon-owned
model because foreground search must only read a published on-disk generation.

Use `sngram::scan` for build extraction and `sngram::query` for query plans. Deduplicate gram hashes per file before adding the document.

If a pattern produces an unconstrained plan, indexed mode errors and tells the user to run `--no-index`.

## Freshness

Foreground `eg` does not prove freshness by walking the tree. It accepts an
index only when disk proves daemon ownership:

- compatible backend manifest exists
- manifest identity matches the weights, index format, scanner/query format,
  and walk/filter fingerprint
- a live daemon lock owner matches the state root owner token
- watcher-ready exists
- journal-clean exists
- no newer wake/dirty epoch invalidates the clean marker
- the lease is live

If proof fails, the index is invalid for foreground search. The daemon deletes
stale generations on startup before publishing readiness and deletes maintained
generations on graceful shutdown.

## Build pipeline

Daemon refresh uses ripgrep ignore traversal to list files. Workers read files,
extract sparse grams, sort and deduplicate hashes, write backend data, summaries,
and manifest, then publish a clean marker only after the generation is complete.

## Tests

- Copy ripgrep integration fixtures where useful.
- Add parity tests that compare `rg` with `eg --no-index`.
- Add indexed tests that compare `rg` with `eg --index=auto`.
- Add error tests for unsupported indexed flags and broad query plans.
- Add manifest tests for stale, missing, new, and changed files.
- Add candidate tests that prove matching files appear in the candidate set.
- Add Unicode tests for Chinese, Japanese, Hebrew, English, and mixed Python.

## Benchmarks

Use Divan for micro paths and raw CLI timing for end-to-end search, file IO,
indexing, mmap behavior, and daemon hot/cold behavior. Compare identical output
modes when checking `eg` against `rg`.

Bench modes:

```text
eg --no-index
eg --bench
eg --bench-suite
```

Metrics: wall time, daemon proof time, cold daemon wait/build time, manifest
open time, index mmap/open time, sparse lookup time, candidate restriction time,
ripgrep verification time, files and bytes indexed, index size, candidate files,
verified files, matching files, false positives, false-positive rate, bytes
verified over corpus bytes, speedup against `eg --no-index`, `rg` comparison,
stdout hash, and exit code equality.

## Bench corpora

Linux corpus: clone `https://github.com/torvalds/linux` at bench setup time, pin a commit in bench config, and place it under `/tmp/eg-bench/linux`.

Multilingual corpus:

- commit deterministic files under `crates/eg/benches/data/multilingual`
- include 50 to 80 Python files
- include 20 to 30 Markdown files per language
- cover English, Chinese, Japanese, Hebrew, Arabic, Korean, Spanish, French, German, Portuguese, Russian, and Hindi
- cover code, law, medicine, finance, security, education, history, science, product docs, contracts, CLI manuals, and policy docs
- generate files once with small subagents, review them, then commit them
- never generate corpus files during CI

## Step order

1. Vendor/import ripgrep and make `eg --no-index` pass parity tests.
2. Add unprefixed index flags with hard errors for indexed mode.
3. Add daemon-owned manifest and backend tests.
4. Add sparse n-gram index builder.
5. Add query-plan to backend translation.
6. Add candidate verification through ripgrep workers.
7. Add Divan benches and raw CLI command benchmarks.
8. Add Linux runtime clone setup and committed multilingual corpus.
