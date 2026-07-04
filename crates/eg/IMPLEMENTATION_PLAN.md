# eg implementation plan

`eg` starts as ripgrep plus a required sparse n-gram prefilter for indexed runs. We import ripgrep crates where public APIs fit. We copy ripgrep binary internals where ripgrep keeps APIs private. We do not rewrite ripgrep behavior.

## Rules

- Keep `eg --no-index` behavior identical to `rg`.
- Copy ripgrep internals mechanically. Patch only named extension points.
- Track the ripgrep commit in source and tests.
- Use unprefixed flags: `--no-index`, `--index`, `--index-backend`.
- Use no silent fallback in indexed modes. Unsupported indexed searches return an error.
- Treat exact ripgrep verification as part of indexed search, not as fallback.
- Run performance work through CodSpeed. Do not report raw `cargo bench` timings.

## CLI modes

Bench and support these modes first:

```text
eg --no-index
eg --index=auto
eg --index=rebuild
eg --index-backend=tantivy-ram
```

`--index=rebuild` measures build plus query time. Bench reports split build, open, plan, index query, and verification times.

Initial flags:

```text
--no-index
--index=auto|rebuild
--index-dir PATH
--index-backend=tantivy|tantivy-ram
--index-table PATH
--index-table-size SIZE
--index-threads NUM
--index-memory BYTES
--max-index-filesize SIZE
--index-status
--index-stats
```

## Ripgrep integration

1. Import public crates: `grep`, `grep-searcher`, `grep-printer`, `grep-regex`, `grep-pcre2`, `ignore`, `globset`.
2. Copy ripgrep `crates/core` modules for private binary behavior: flags, haystacks, search workers, printers, messages, completion generation.
3. Add one extension point after ripgrep builds haystacks and before workers search files.
4. Keep ripgrep printers responsible for stdout, stderr, JSON, colors, stats, and exit codes.

Search flow: parse rg args, build rg haystacks, run copied rg path for `--no-index`, open or rebuild the index, plan sparse grams, query Tantivy, map doc ords to paths, verify candidates with ripgrep workers, and print with ripgrep printers.

## Index design

Use Tantivy as a numeric inverted index: one source file per Tantivy document, repeated indexed `u64` sparse n-gram hashes, fast `u64` `doc_ord`, external manifest for paths and metadata, mmap for normal runs, RAM only for `--index-backend=tantivy-ram`.

Use `sngram::scan` or `StreamScanner` for build extraction. Use `sngram::query` for query plans. Deduplicate gram hashes per file before adding the document.

If a pattern produces `QueryPlan::All`, indexed mode errors and tells the user to run `--no-index`.

## Freshness

The manifest records the table hash, root path, ripgrep commit, `eg` version, file size, mtime, and content hash policy. Indexed mode errors on stale files, missing files, new files, table mismatch, root mismatch, or schema mismatch. Later work may add explicit overlay support, but v1 keeps the rule strict.

## Build pipeline

Use ripgrep ignore traversal to list files. Rayon workers read files, extract sparse grams, sort and deduplicate hashes, then send prepared documents to one Tantivy writer. The builder commits the index and writes manifest plus stats atomically.

## Tests

- Copy ripgrep integration fixtures where useful.
- Add parity tests that compare `rg` with `eg --no-index`.
- Add indexed tests that compare `rg` with `eg --index=auto`.
- Add error tests for unsupported indexed flags and broad query plans.
- Add manifest tests for stale, missing, new, and changed files.
- Add candidate tests that prove matching files appear in the candidate set.
- Add Unicode tests for Chinese, Japanese, Hebrew, English, and mixed Python.

## CodSpeed benches

Use Divan through `codspeed-divan-compat`. Add `codspeed.yml` for whole-command benchmarks when Divan cannot express the workload. Use simulation for CPU-bound micro paths and walltime for end-to-end search, file IO, indexing, and mmap behavior.

Bench modes:

```text
eg --no-index
eg --index=auto
eg --index=rebuild
eg --index-backend=tantivy-ram
```

Metrics: wall time, build time, index open time, query plan time, Tantivy query time, ripgrep verification time, files and bytes indexed, index size, candidate files, verified files, matching files, false positives, false positive rate, bytes verified over corpus bytes, speedup against `eg --no-index`, stdout hash, and exit code equality.

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
3. Add Tantivy schema, manifest, and RAM backend tests.
4. Add sparse n-gram index builder.
5. Add query-plan to Tantivy translation.
6. Add candidate verification through ripgrep workers.
7. Add CodSpeed Divan benches and command benches.
8. Add Linux runtime clone setup and committed multilingual corpus.
