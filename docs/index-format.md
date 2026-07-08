# Index format: postings-v9

The frozen on-disk format under `<root>/.eg/index/postings-v9/`. Five
files: `table.bin`, `postings.bin`, `summaries.bin`, `manifest.bin`,
`paths-v1.bin`. Format changes bump a version and rebuild destructively;
there are no migration readers.

Sizes on the Linux kernel reference corpus (93,610 docs, 1.59GB text):
table 307MB, postings 1019MB, summaries 21MB, manifests 16MB. Total
1.42GB, 0.90x the corpus.

## Sections

`table.bin` and `postings.bin` share a 32-byte section header: magic,
format version, body byte count, and a sampled FNV checksum (4MB head and
tail windows, so a 1GB file verifies in milliseconds while still catching
truncation and torn tails).

## table.bin

Maps a gram's hash to its posting list. Body layout:

```
[records][directory: n × 16B][inline bitmaps: n × 32B][block count u32]
```

Records are delta-coded in blocks of 256. Each directory entry holds the
block's first hash32, its byte offset in the records region, and the
postings-byte offset where the block's first stored list begins. Lookup
binary-searches the directory, then decodes at most 256 records
sequentially, accumulating hash gaps and list sizes.

A record is `[hash-gap uvarint]` followed by one of two payloads. The
per-block bitmap says which: for a df=1 gram (68% of all grams) the
posting inlines as `[ordinal uvarint][mask byte]` and never touches
postings.bin; otherwise the record carries `[count uvarint][size uvarint]`
and the list lives in postings.bin. Counts are exact. The largest df on
the reference corpus is 90,031, which is why counts are varints and not
u16.

Gram keys are 64-bit hashes truncated to 32 bits. Collisions merge
posting lists, which is a sound superset; measured cost is +31 candidates
per 848k.

## postings.bin

Body layout: `[256B Huffman code lengths][lists]`. Each stored list is

```
[count ordinal gaps, uvarint][mask column]
```

Ordinals are ascending document numbers, delta-coded. The measured gap
stream sits within 14% of the Elias-Fano bound; Roaring and PEF lose on
the df=1 majority, so plain varints stay.

The mask column holds one byte per posting. Lists of 16 or more postings
encode it as a canonical Huffman bitstream over the global code table in
the prologue; shorter lists store raw bytes, because byte padding costs
more than Huffman saves on them. The reader builds a 16-bit lookup table
from the code lengths at open, one probe per symbol.

### The mask byte

```
bit 7  WORD_END    some occurrence is followed by a non-word byte
bit 6  WORD_START  some occurrence is preceded by a non-word byte
bit 5  WORD_BOTH   one single occurrence has both word edges
bits 0-4           hashed line buckets: hash(line) % 5
```

Buckets are hashed so collision probability does not grow with file
size. Intersection ANDs masks and drops a document when no bucket bit
survives; a gram on five or more distinct lines saturates all buckets.
`-U` multiline queries widen every mask to all buckets, keeping only the
word bits. Whole-literal `-w` plans demand WORD_BOTH, which split
START/END bits from different occurrences cannot fake.

One reserved hash (`u32::MAX`) lists forced candidates: files too large,
BOM-encoded, or too high-entropy to index, always handed to the verifier.

## summaries.bin

One 240-byte record per document, indexed by ordinal:

```
status u8 | byte_len u64 | longest_line_len u32 | byte counts 128B
| line-start byte set 32B | line-end byte set 32B
| prefix 1+16B | suffix 1+16B | pad
```

Byte counts are 4-bit saturating nibbles; 15 means fifteen or more, and
the reader widens it to unbounded, which over-includes and stays sound.
These records answer the plan's `ScanNeed`s without touching file
content. Status distinguishes indexed text, unindexable text (forced),
and skipped binaries.

## manifest.bin and paths-v1.bin

The binary manifest is the commit point: per-file relative path, display
path (empty when equal to the relative path), path hash, length, and
timestamps, used for freshness comparison. `paths-v1.bin` is a flat
offset-indexed path table the daemon reads without parsing the manifest.
A JSON manifest is written only under `--debug` or
`EG_INDEX_JSON_MANIFEST`, for tooling.

## Rejected designs

Recorded in [fp-optimization-plan.md](fp-optimization-plan.md): exact
per-gram line lists (+1.5-3GB), Roaring/PEF/SIMD-BP128, minimal perfect
hashing (order-preservation compresses offsets better), 40-bit keys,
wider bucket masks, and mask deletion (the masks bought the precision).
