# sngram project guidelines

## project

sngram is a sparse n-gram engine for regex prefiltering and code search indexing.

The system has two jobs:

- scan text into selective sparse grams and document metadata
- fold regex patterns into safe query plans over those grams

The index must never miss a match it could find. Query plans may return false positives; the caller verifies candidates with the real regex engine.

At a high level sngram consists of 5 parts:

### `types`

`crates/types` owns shared data shapes. Put raw cross-crate types here when more than one crate needs them.

Core types include `WeightTable`, `GramKey`, `ScanEvent`, `ScannedGram`, `ScanSummary`, `QueryPlan`, `PlanExpr`, `GramNeedle`, and the error types. Keep logic on these types as small methods. Do not add free helper functions here when a private method on the owning type reads better.

### `lib`

`crates/lib` is the public Rust API. Keep it small.

The normal API is:

```rust
sngram::scan(&table, reader, emit)?;
sngram::query(&table, pattern)?;
```

`scan` reads one `BufRead` stream, validates text input, emits `ScanEvent::Gram` values, then emits final scan metadata. `query` accepts one regex pattern and returns a `QueryPlan`. CLI concerns such as fixed-string escaping, smart case, multiple pattern OR joining, byte mode, and CRLF mode belong above `query`.

The `learn` feature exposes training counters. Keep training-specific code behind that feature.

### `weights`

The `sngram` crate embeds the trained production weight table behind the single `weights` feature (`crates/lib/src/weights.rs`, the binary at `crates/lib/data/weights.bin`). Enable `weights` and load it with `sngram::weights()`. Superseded tables live in git history and are re-mintable from training checkpoints.

Use:

```rust
let table = sngram::weights();
```

Do not expose table internals, filenames, constants, or low-level lookup helpers. The high-level return value is `WeightTable`.

### `python`

`crates/python` is the standalone `sngram` Python library. It is a maturin project: the pyo3 bindings crate, the `sngram/` wrapper package, the pyproject, and the lib tests live together in that one directory. It exposes the scan/query core, the embedded production weight table, and the GIL-free training counters. It ships no CLI and no runtime dependencies. This is the package that goes to PyPI.

`train/` is the `sngram-train` project: the corpus training pipeline and the `sngram` training CLI. It depends on the library by path and streams from the Hugging Face Hub. It trains on one of two corpora, chosen with `--corpus`:

- `blend` (default, production): nine families over 38 sources with per-family and per-source byte ceilings summing to 15 TB. A planner feeds the counter from whichever family is furthest below its target share of counted bytes, so a mint holds the intended mix rather than raw dataset sizes.
- `stack-v3`: the single-dataset path (`HuggingFaceCode/stack-v3-train`, ODC-By, ungated), file content inline in `files[].content`, one row per repository.

Sources differ by file layout (nested stack shards, flat parquet columns, gzipped JSON lines) and by text field (`content`, `text`, `Body`). The trainer counts byte pairs through Rust, checkpoints every minute at a consistent quiesce, resumes byte-exactly, and mints one provenance-stamped final table naming whichever corpus ran. A checkpoint is bound to the corpus name and its pinned revisions, so resuming into a different corpus is refused. Keep `.env` under `train/.env`; reading the data uses the Hugging Face token there.

Useful commands:

```sh
cd crates/python
uv sync
uv run pytest

cd train
uv sync
uv run pytest
uv run sngram train --shards 2 --no-dashboard
uv run sngram train --mint-dir ./runs/r1
uv run sngram train --corpus stack-v3
uv run sngram inspect runs/r1/final_weights.bin
```

The `train` options are `--mint-dir`, `--corpus`, `--workers`, `--limit`, `--shards`, `--checkpoint-every`, `--resume/--no-resume`, and `--dashboard/--no-dashboard`. `--shards N` bounds a run to the first N shards of every source.

### `eg`

`crates/eg` is the application CLI. It uses the sparse index to prefilter files, then verifies candidates through the copied ripgrep search path.

The app-level index API should stay small:

```rust
index::run(args)
index::IndexConfig
```

The foreground process resolves the search root, reads a daemon-proofed index, queries candidates, and verifies haystacks. The daemon builds, watches, refreshes, owns, and deletes indexes. A foreground process may block for the first missing-index build; after that the daemon owns the index lifecycle.

`eg-indexd` is the runtime daemon. It writes owner, watcher, clean-journal, wake, and lease markers under the index runtime directory. A stale index without daemon proof is invalid.

## training

Rust training lives behind `sngram`'s `learn` feature:

```toml
sngram = { path = "crates/lib", features = ["learn"] }
```

Use `sngram::learn::BigramCounter` for local counting and table bytes. The Python trainer mints production tables: it owns shard streaming, worker coordination, checkpointing, provenance stamping, and event logs.

Generated `.bin` weight tables load through `WeightTable::from_bytes`. Released tables move into `crates/lib/data` and get exposed through Cargo features.

## testing

Use the tightest command that covers the code you changed, then run broader checks before finishing.

Rust workspace:

```sh
cargo test -p sngram-types --offline
cargo test -p sngram --offline
cargo test -p sngram --features weights --offline
cargo test -p elgrep --offline
cargo clippy -p elgrep --all-targets --offline -- -D warnings
cargo fmt --all -- --check
```

Python:

```sh
cd crates/python && uv run pytest
cd train && uv run pytest
```

`eg` helper commands:

```sh
just eg check
just eg test
just eg clippy
just eg release
```

Unit tests belong in `mod tests {}` inside the source file that owns the logic. Integration tests for the CLI belong under `crates/eg/tests/` with names that describe the behavior. Do not create catch-all test files with vague names.

## benchmarking

`eg` benches and the library benches in `crates/lib/benches` run on Divan, through the CodSpeed compatibility layer. The Python bindings are benchmarked with `pytest-codspeed` in `crates/python/benchmarks`. Every push and pull request runs both suites under CodSpeed CPU simulation (`.github/workflows/codspeed.yml`).

CLI:

```sh
just eg bench
target/release/eg --bench PATTERN PATH
target/release/eg --bench
target/release/eg --no-index PATTERN PATH
rg PATTERN PATH
```

`--bench PATTERN PATH` emits one structured JSON report for the indexed path, ending with a `comparison` block that re-runs the query through `--no-index`, and through `rg` when a ripgrep binary is on PATH. Bare `--bench` runs the embedded 296-query TSV suite in `crates/eg/src/index/data/fp-queries.tsv` and compares indexed search with `--no-index`, adding the `rg` leg under the same condition.

Library benches, from the `sngram-benches` crate at `crates/lib/benches`:

```sh
cargo bench -p sngram-benches --bench extract
cargo bench -p sngram-benches --bench query
cargo bench -p sngram-benches --bench counter
```

Under CodSpeed, locally, matching what CI does. `RUSTFLAGS=""` is required: the repo-local `target-cpu=native` codegen is not executable under valgrind.

```sh
RUSTFLAGS="" cargo codspeed build -p sngram-benches
RUSTFLAGS="" codspeed run --mode simulation -- cargo codspeed run -p sngram-benches
```

Python binding benches, from `crates/python`:

```sh
RUSTFLAGS="" uv sync
RUSTFLAGS="" codspeed run --mode simulation -- uv run --no-sync pytest benchmarks --codspeed
```

Report command lines with results. For hot-path claims, compare indexed `eg` against `eg --no-index` on the same corpus and output mode, and add `rg` when a real ripgrep binary is on PATH.

## code quality

Code should read like a story, each module a chapter, each exposing as little as possible for the higher level module to consume.

Keep functions and methods at 25 lines or less. Keep files under 400 lines. Split by semantic domain before a file turns into a dumping ground.

Expose the least API each module can expose. Use private items by default. If another module needs a concept, put that concept in its owning module and make the item public. Do not duplicate it.

Never re-export as a bridge. Do not keep old and new paths alive together. Move a thing once, then update callers. `pub use x;` is forbidden outside crate `lib.rs`, and should stay rare there.

Never use `pub(crate)`, `pub(super)`, or `pub(in ...)`. An item is private or public.

Prefer a small direct function over a flexible abstraction. Remove duplication when it makes the code easier to read.

Everything in public API must be typed. Do not pass loose strings, integers, bools, vectors, or byte buffers when a domain type fits. Use checked constructors when a value has rules.

Do not mutate public interfaces of `interp`, `readline`, `builtins`, or `lib` crates without explicit approval.

## comments

Never write comments longer than one line.
Never end comments with a period.
Never explain who uses a thing.
Never explain why a thing exists in comments.
Write what the item is, in plain words.
Use `///` or `//` only.
Follow `.agents/skills/stop-slop` when writing comments and docs.

## committing

Use conventional commits. Make commit messages direct and human. Never self-attribute or mention assistant tooling.
