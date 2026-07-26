# Training Data Contract

Decision date: 2026-07-26.

Training reads decoded UTF-8 text streamed from the Hugging Face Hub and
nothing else. Two corpora are selectable with `--corpus`. The production
corpus is `blend`; `stack-v3` is kept and still runs.

## Why blend is production

The table shipped in `crates/lib/data/weights.bin` is a blend mint:
11.96 TB counted, 89.6% of it code. A Stack v3 table trained later lost
to it on all four measured corpora like-for-like, worst on kubernetes at
+7.17pp false positives.

The mechanism is visible in the tables. Stack v3 takes whole repositories
at their natural mix, which puts no Go in its top eight languages and
11.9% HTML and XML in them. So `<-`, Go's channel operator and also the
head of an SGML comment, lands at weight rank 3,189 in the Stack v3 table
against 6,524 in the blend table: common enough to stop being selective.
kubernetes is the Go corpus and it is the corpus that regressed.

Neither the table format nor the minting rule is implicated. A v1 table
carrying the Stack v3 weights benches identically to the v2 original, and
the count-to-weight rule is `total_pairs / count` in both.

## blend

Nine families over 38 sources, ceilings summing to 15 TB. Each family
carries a target share of counted bytes; the planner dispatches to the
family furthest below its share, so a mint holds the intended blend while
data lasts. Every family and every source stops at its ceiling.

| family | source | ceiling |
|---|---|---:|
| code-clippy | CodedotAI/code_clippy_github | 8.30 TB |
| blend-extras | bigcode/starcoder2data-extras | 2.69 TB |
| code-github-2025 | nick007x/github-code-2025 | 2.30 TB |
| config-markup | bigcode/starcoderdata, 16 languages | 450 GB |
| multilingual | HuggingFaceFW/fineweb-2, 12 languages | 450 GB |
| english-finepdfs | HuggingFaceFW/finepdfs, eng_Latn | 300 GB |
| blend-opc | OpenCoder-LLM/opc-fineweb-code-corpus | 265 GB |
| code-stack-v2-high | M1keR/the-stack-v2-dedup-filtered-500-stars-100-forks-contents | 200 GB |
| qa-stackoverflow | mikex86/stackoverflow-posts | 45 GB |

The intended split is 75% pure code, 20% code and text blend, 3%
multilingual UTF-8 coverage, 2% English prose. The realised split runs
code-heavier because the blend and markup sources exhaust their supply
first: the shipped mint filled 105 GB of the 450 GB markup ceiling and
120 GB of the 2.69 TB extras ceiling. A mint that ends short of 15 TB is
the supply running out, not a failure.

Sources differ in shape. A shard is nested stack files, a flat parquet
column, or gzipped JSON lines, and its text lives in `content`, `text`,
or `Body`. Each source declares its own layout and field.

The multilingual slice is twelve languages spanning the UTF-8 multibyte
space, 20 GB per CJK language and the rest split evenly. A real developer
filesystem is about 99.9% ASCII, so this is coverage enough to give
multibyte pairs graded weights, not a corpus in its own right.

## stack-v3

`HuggingFaceCode/stack-v3-train`, ODC-By 1.0, ungated. 8196 snappy
parquet shards, 4.71 TB on the wire and 15.9 TB decoded, 713 languages,
172.9M rows, GitHub snapshot 2025-08-07. File content rides inline in
`files[].content` and one row is one repository, taken with its whole
file mix. No ceilings apply, so the planner walks the shard list straight
through.

## Both corpora

Vendored files count. People search vendored code like any other code, so
dropping it would train on a corpus nobody indexes. The vendored share is
tracked separately for reporting.

One filter applies: a file with null content is skipped.

The trainer pins every source revision at start and stamps a fingerprint
over the whole roster into the checkpoint and the final provenance. A
checkpoint is bound to its corpus name and that fingerprint, so resuming
into a different corpus, or into a roster whose revisions moved, is
refused rather than silently trained.

## Training Flow

1. Resolve the corpus: pin revisions, list every shard with its size
2. Stream shards through parallel readers, row group by row group, with
   only the declared text column selected
3. Count byte pairs through the Rust `BigramCounter`, GIL released
4. Checkpoint the counter, every reader position, and the per-family and
   per-source tallies each minute at a consistent quiesce
5. Mint one final provenance-stamped table when the stream ends

The checkpoint is taken with the readers held still, so the counter and
every reader position describe the same instant. Transient network
failures retry in place and resume from the current position.

Cap enforcement is per batch: a batch that would overshoot a ceiling is
trimmed to the longest row prefix that fits. Workers read the tally
outside the merge lock, so concurrent workers on one source can overshoot
by up to one batch each, bounded well under 0.001% of the smallest
ceiling.

## Environment

- `HF_TOKEN`: read access to the Hub, from the environment or `train/.env`
- `SNGRAM_DATASET_REPO`: overrides the stack-v3 dataset repo

```sh
cd train
uv sync
uv run sngram train --mint-dir ./runs/r1
```
