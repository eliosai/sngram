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

`crates/weights` embeds trained weight tables. Features expose released tables from `500gb` through `12tb`. The smaller bootstrap tables can exist as data, but the crate should expose only production tiers.

Use:

```rust
let table = sngram_weights::weights();
```

Do not expose table internals, filenames, constants, or low-level lookup helpers. The high-level return value is `WeightTable`.

### `python`

`python/` is the training and bindings project. It uses `uv`, `maturin`, and `pyo3`.

The Python package exposes the scan/query core and GIL-free training counters. The training CLI streams blended corpora, counts byte pairs through Rust, checkpoints, mints weight tables, and resumes from saved state. Keep `.env` under `python/.env`; training uses Hugging Face credentials there.

Useful commands:

```sh
cd python
uv sync
uv run pytest
uv run sngram train --limit 1GB
uv run sngram train --mint-dir ./bins
uv run sngram inspect bins/5tb_weights.bin
```

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

Use `sngram::learn::BigramCounter` for local counting and table bytes. Use the Python trainer for full corpus minting. The Python trainer is the source of production tables because it handles dataset streaming, worker coordination, checkpointing, mint cadence, and event logs.

Generated `.bin` weight tables load through `WeightTable::from_bytes`. Released tables move into `crates/weights/data` and get exposed through Cargo features.

## testing

Use the tightest command that covers the code you changed, then run broader checks before finishing.

Rust workspace:

```sh
cargo test -p sngram-types --offline
cargo test -p sngram --offline
cargo test -p sngram-weights --offline
cargo test -p eg --offline
cargo clippy -p eg --all-targets --offline -- -D warnings
cargo fmt --all -- --check
```

Python:

```sh
cd python
uv run pytest
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

Use raw Divan and CLI measurements for `eg` performance work.

CLI:

```sh
just eg bench
target/release/eg --bench PATTERN PATH
target/release/eg --bench
target/release/eg --no-index PATTERN PATH
rg PATTERN PATH
```

`--bench PATTERN PATH` emits one structured JSON report for the indexed path. Bare `--bench` runs the embedded high-false-positive TSV suite in `crates/eg/src/index/data/fp-queries.tsv` and compares indexed search with `--no-index` and `rg` when available.

Library benches:

```sh
cargo bench --manifest-path crates/lib/benches/Cargo.toml --bench extract
cargo bench --manifest-path crates/lib/benches/Cargo.toml --bench query
cargo bench --manifest-path crates/lib/benches/Cargo.toml --bench counter
```

Report command lines with results. For hot-path claims, compare indexed `eg`, `eg --no-index`, and `rg` on the same corpus and output mode.

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
