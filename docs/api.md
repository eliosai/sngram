# The public API

The crate root holds the four functions, every type they take or return, and `DfStats`, the trait
`QueryPlan::tune` reads. `learn` is the one public module, for the counter that trains a table.
cargo-semver-checks compares each release against the one before it, and a breaking change bumps
the major, which before 1.0 moves the minor.

## Functions

- `weights() -> &'static WeightTable` returns the embedded production table, parsed and
  checksummed on first use (feature `weights`, on by default)
- `is_binary(content: &[u8]) -> bool` is true when a NUL byte sits in the first 8 KiB, the rule
  ripgrep's binary detection quits on
- `scan(table: &WeightTable, content: &[u8], emit: impl FnMut(ScannedGram)) -> ScanSummary` hands
  every gram of one document to `emit` and returns the summary mined in the same pass; it applies
  no binary policy
- `query(table: &WeightTable, pattern: &str) -> Result<QueryPlan, QueryError>` folds one regex
  into a plan every matching document satisfies

## `WeightTable`

The 256 by 256 grid of byte-pair weights. `from_weight_fn(f)` builds one from a function over
every pair, `from_bytes(bytes)` loads the `SPNG` binary and verifies its checksum, `to_bytes()`
writes it back, and `with_provenance(record)` stamps a record of at most 1024 bytes. `weight(c1,
c2)` reads one pair, `matrix()` the whole grid indexed by `c1 << 8 | c2`, `fingerprint()` the
FNV-1a identity of the bytes, `version()` the format version, and `provenance()` the record if the
table carries one. `WeightTable` is `Debug` and `Clone`, and a clone copies 256 KiB.

## `TableError`

`InvalidSize(usize)`, `InvalidMagic`, `Checksum { expected, actual }`, `Truncated(usize)`,
`InvalidVersion(u32)` and `InvalidProvenance`. The enum is `#[non_exhaustive]`, `Debug` and
`std::error::Error`.

## `ScannedGram`, `GramKey` and `ByteRange`

`ScannedGram { key, span }` is what `scan` emits; `new(key, span)` builds one and
`content_span()` returns the span as a `Range<usize>`. `GramKey` wraps the 64-bit index key,
`new(value)` and `value()`; it is the key to store, and it may cover a virtual sentinel or a
case-folded twin of the span, so re-hashing the span bytes is not equivalent. `ByteRange { start,
end }` is the half-open span in the original content, `new(start, end)` and `as_range()`.
`ScannedGram` and `ByteRange` are `#[non_exhaustive]`. All three are `Debug`, `Clone`, `Copy`,
`PartialEq` and `Eq`; `GramKey` and `ByteRange` add `Default` and `Hash`, and `GramKey` `Ord`.

## `ScanSummary`

The open record of one document: `byte_len` (`u64`), `line_count`, `empty_line_count`,
`longest_line_len` and `gram_count` (`u32`), `flags` (`ScanFlags`), `byte_counts`
(`SaturatingByteCounts256`), `line_start_bytes` and `line_end_bytes` (`ByteSet256`), `prefix` and
`suffix` (`EdgeBytes`). The struct is `#[non_exhaustive]`, so a caller starts from
`ScanSummary::default()` and sets fields. It is `Debug`, `Clone`, `Copy`, `Default`, `PartialEq`,
`Eq` and `Hash`.

## `ScanFlags`

Nine boolean facts behind `from_bits(u64)` and `bits()`: `with_lf`/`has_lf`,
`with_crlf`/`has_crlf`, `with_ends_with_lf`/`ends_with_lf`, `with_ascii_upper`/`has_ascii_upper`,
`with_ascii_lower`/`has_ascii_lower`, `with_ascii_digit`/`has_ascii_digit`,
`with_ascii_space`/`has_ascii_space`, `with_ascii_word`/`has_ascii_word` and
`with_non_ascii`/`has_non_ascii`. `Debug`, `Clone`, `Copy`, `Default`, `PartialEq`, `Eq`, `Hash`.

## `ByteSet256`, `SaturatingByteCounts256` and `EdgeBytes`

`ByteSet256` is the set of byte values seen: `from_words([u64; 4])`, `words()`, `insert(byte)`,
`contains(byte)`, `contains_any(other)`, `union(other)`, `len()` and `is_empty()`.
`SaturatingByteCounts256` is the histogram with one saturating `u8` per byte value:
`from_counts([u8; 256])`, `counts()`, `observe(byte)`, `contains_at_least(&need)` and
`is_empty()`. `EdgeBytes` holds up to `CAPACITY` (16) leading or trailing bytes:
`from_slice(bytes)`, `push(byte)`, `len()`, `is_empty()` and `as_slice()`. All three are
`Debug`, `Clone`, `Copy`, `Default`, `PartialEq`, `Eq` and `Hash`.

## `QueryPlan` and `PlanExpr`

`QueryPlan::new(root)` wraps an expression, `root()` returns it, `is_all()` and `is_none()` name
the two trivial plans, `gram_count()` counts the needles in the tree, and `tune(&dyn DfStats,
stop_df)` reorders alternatives by document frequency and drops bags too common to narrow
anything. `PlanExpr` is `All`, `None`, `AllOf { grams, needs, children }` or `AnyOf { grams,
needs, children }` with the same three accessors. Both are `Debug`, `Clone`, `PartialEq`, `Eq`
and `Display`. `PlanExpr` is exhaustive on purpose: an executor must handle every variant, so a
new variant is a breaking change.

## `GramNeedle`

One gram requirement lowered to index keys: `Key(GramKey)`, `AnyKey(Vec<GramKey>)`,
`AtWordEdge { keys, starts, ends, whole }` and `AtLineEdge { keys, starts, ends }`. `keys()`
iterates every concrete key the needle may match. `Debug`, `Clone`, `PartialEq`, `Eq`, and
exhaustive on purpose.

## `ScanNeed`

One necessary condition over a `ScanSummary`: `MinByteLen(u64)`, `MinLongestLineLen(u32)`,
`ContainsAnyByte(ByteSet256)`, `MinByteCounts(Box<SaturatingByteCounts256>)`,
`LineStartsWithAnyByte(ByteSet256)`, `LineEndsWithAnyByte(ByteSet256)`, `StartsWith(EdgeBytes)`
and `EndsWith(EdgeBytes)`. `satisfied_by(&summary)` evaluates it. `Debug`, `Clone`, `PartialEq`,
`Eq`, `Display`, and exhaustive on purpose.

## `DfStats`

The trait an index implements so `QueryPlan::tune` sees its document frequencies:
`entry_count(&self, key: GramKey) -> u64` and `total_entries(&self) -> u64`.

## `QueryError`

`PatternTooLong { len, max }` and `InvalidRegex(Box<dyn std::error::Error + Send + Sync>)`, whose
`Display` carries the parser's message. The enum is `#[non_exhaustive]`, `Debug` and
`std::error::Error`.

## `learn` (feature `learn`)

`BigramCounter` counts byte pairs from many threads at once: `new()` and `Default`,
`process(content)`, `process_batch(values) -> u64` over an iterator of byte slices, `merge(&other)`,
`add_files(n)`, `pairs_processed()`, `bytes_processed()`, `files_processed()`, `count(c1, c2)`,
`snapshot() -> Vec<u8>`, `restore(snapshot, pairs, bytes, files) -> Result<(), LearnError>` into
a counter that has seen nothing, and `to_table_bytes()` in the `SPNG` format
`WeightTable::from_bytes` loads.

`learn::LearnError` is `InvalidSnapshotLen { expected, actual }` or `NotFresh`. The enum is
`#[non_exhaustive]`, `Debug`, `PartialEq`, `Eq` and `std::error::Error`.

## Features

`weights` (on by default) embeds the production table behind `weights()`. `learn` adds the
`learn` module.
