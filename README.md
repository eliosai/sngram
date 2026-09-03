# sngram

Sparse n-gram extraction for regular-expression search indexing, and
elgrep, an indexed ripgrep alternative built on it.

## Sparse n-grams

A classic regex index cuts every document into trigrams: each
overlapping three-byte window becomes a key in an inverted index, a
query regex decomposes into the trigrams a match must contain, and
intersecting their posting lists yields the candidate documents.
Trigrams are a compromise. Two-byte windows make posting lists too
large to intersect quickly, four-byte windows make the key space too
large to store, and every trigram repeats two bytes of its neighbor,
so most of the index is redundancy.

Sparse n-grams replace the fixed window with a weighted one. A weight
table assigns a weight to every byte pair, high for rare pairs and low
for common ones. The scanner extracts every substring whose two border
pairs weigh strictly more than every pair between them. The extracted
grams vary in length and land on the distinctive parts of the text.

Because the weights are deterministic, the index side and the query
side stay in agreement. Indexing extracts every sparse gram a document
contains. Querying extracts far fewer: a covering algorithm derives a
minimal set of grams a match must contain, so a regex folds into a
small boolean plan with fewer posting lookups and fewer candidates
than trigram decomposition. The plan matches a superset of what the
regex matches. A prefilter built from it never misses a match, and the
real regex verifies the candidates it admits.

The weight table is a byte-pair frequency table measured over terabytes
of real source repositories, whole trees with their natural mix of code,
config, markup, and docs, so rarity, and with it selectivity, comes from
the kind of text people search. The trained production table ships inside
the library.

## elgrep

```sh
cargo install elgrep
```

`eg` carries ripgrep's search path and adds the sparse index in front:
the index narrows each query to candidate files, the regex engine
verifies them, and results match a plain scan exactly. The `eg-indexd`
daemon builds, watches, and refreshes indexes in the background, so
every query after the first build hits a warm index.

```sh
eg 'max_\w+_size' ~/src/linux
eg --no-index 'max_\w+_size' ~/src/linux   # plain scan for comparison
```

On the Linux kernel tree, with a hot daemon-owned index and
files-with-matches output, p50 of 9 runs, measured 2026-07-26 against
ripgrep 15.2.0 and GNU grep 3.11. Every hit set is compared against
ripgrep's before the pattern is timed, so these rows are the same answer
at different speeds:

| Pattern | Matched files | elgrep | ripgrep | grep |
|---|---:|---:|---:|---:|
| `linus tor` | 0 | 5.9 ms | 84.0 ms (14.2x) | 821.2 ms (139.3x) |
| `EXPORT_SYMBOL_GPL` | 3627 | 16.6 ms | 86.5 ms (5.2x) | 671.7 ms (40.6x) |
| `copy_from_user` | 1221 | 7.0 ms | 82.9 ms (11.9x) | 677.9 ms (97.1x) |
| `schedule_timeout` | 418 | 9.3 ms | 86.1 ms (9.2x) | 684.2 ms (73.4x) |

A pattern with no matches is where the index earns most: it answers from
posting lists without opening a file. A pattern matching 3,627 files
gains least, because the verifier still reads all 3,627.

The index is a prefilter, so it may hand the verifier a file the regex
then rejects; that costs time and never costs correctness. It may never
lose a match, and the embedded 296-query suite fails the run if any
indexed hit set diverges from its scan hit set. On linux the index is
1,467,104,135 bytes, 0.91x the corpus, built in 11,880 ms.

[crates/eg/README.md](crates/eg/README.md) covers the CLI, the daemon,
and the benchmark modes.

## The Rust crate

```sh
cargo add sngram
```

The crate embeds the trained production table. The whole API is four
calls:

```rust
let table = sngram::weights();

sngram::is_binary(bytes);                 // ripgrep's rule: a NUL in the first 8 KiB
let summary = sngram::scan(table, bytes, emit);   // index side: text to grams
let plan = sngram::query(table, pattern)?;        // query side: regex to a plan
```

`scan` takes one document and hands back the grams to store, keyed
exactly as `query` will look them up, and the summary mined in the same
pass. It runs at about 208 MiB/s on code. The README in
[crates/lib](crates/lib) covers the plan structure, the tuning hook, and
training behind the `learn` feature.

## The Python package

```sh
pip install sngram
```

The same Rust core, built with maturin. No runtime dependencies, and
scan and training work release the GIL.

```python
import sngram

table = sngram.weights()
result = sngram.scan(table, b"fn main() {}")
plan = sngram.query(table, r"max_\w+_size")
```

[crates/python/README.md](crates/python/README.md) documents the full
surface, including plan tuning and a worked inverted-index example.

## The trainer

`train/` mints weight tables, streaming everything from the Hugging Face
Hub. The production corpus is `blend`: nine families over 38 sources,
15 TB of family ceilings across code, config and markup, technical text,
English prose, and a twelve-language UTF-8 coverage slice. A planner
feeds the counter from whichever family sits furthest below its target
share, so a mint holds the intended mix instead of the raw dataset
sizes. `--corpus stack-v3` selects the single-dataset path instead.

The table shipped in the library is a blend mint: 11.96 TB counted,
89.6% of it code, over about ten hours.

```sh
cd train
uv sync
uv run sngram train --shards 2      # smoke run
uv run sngram train --mint-dir ./runs/r1
uv run sngram inspect runs/r1/final_weights.bin
```

[docs/training.md](docs/training.md) specifies the production run and
its acceptance gates.

## Documentation

- [docs/architecture.md](docs/architecture.md) the system in one page
- [docs/index-format.md](docs/index-format.md) postings-v9 on disk
- [docs/query-planning.md](docs/query-planning.md) regex to plan to candidates
- [docs/daemon.md](docs/daemon.md) who builds and owns indexes
- [docs/benchmarking.md](docs/benchmarking.md) how to measure claims
- [docs/training.md](docs/training.md) the production training run
- [docs/training-data.md](docs/training-data.md) the corpus contract

## License

[MIT](LICENSE)
