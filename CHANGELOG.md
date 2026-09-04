# Changelog

## 1.0.0 (2026-09-04)


### Features
- Fixed training?
- train: Code-dominant scan distribution and hf dataset publishing
- eg: Pin anchored patterns to line-edge posting bits
- train: Train from The Stack v3 parquet shards on the Hub
- train: Stream stack-v3 parquet and mint from inline content
- train: Count vendored files instead of skipping them
- daemon: Give watches to the tree someone is searching
- train: Restore the blended production corpus, keep stack-v3
- lib: Add async sparse scan
- lib: Add incremental text scanner
- Trim the API to scan, is_binary, query and weights


### Fixes
- Cap stack source share per area
- train: Survive hub stream network blips
- train: Honest resumed averages and corpus-identity checkpoints
- train: Show trained share against target per group
- index: Cover the searchable prefix of binary files
- eg: Search the pre-NUL prefix of binary files deterministically
- index: Render indexed paths from the current search argument
- index: Decide tiny binary prefixes from held bytes
- train: Honest eta from run averages, not windowed rates
- train: Bound and retry the shard listing
- query: Repin anchored plans after the line-edge bits
- index: Report every match the line-by-line searcher finds
- daemon: Bound the watch budget and give watches back
- eg: Credit the right author and say what the index is doing
- index: Answer from a scan rather than a stale index or an error
- eg: Only draw a progress bar for a wait worth reporting
- eg: Print one block separator on the indexed path
- eg: Scan when the published index cannot be read
- index: Union the index with the paths changed since it was published
- ci: Test the feature that exists and lint the whole workspace clean


### Performance
- scan: Collect the scan summary chunk-wise
- scan: Stream the hull scan without a copy window
- scan: Start the folded space at the first uppercase byte
- scan: Reserve the sniff buffer up front
- scan: Build the folded space lazily from the primary state
- index: Balance scan chunks and parallelize the postings merge
- train: Bound decode memory while staying link-bound
- query: Cut plan construction from milliseconds to microseconds
- index: Cut index build work by about 45 percent
- eg: Compile the query pattern once and size verify to the work
- index: Decode posting lists about 1.55 times faster
- query: Cut plan construction by about a tenth
- index: Walk filter lists with a cursor instead of researching them
- bench: Standardize on CodSpeed Divan


### Refactoring
- train: Reduce training to the published hub dataset
- One weights feature and provenance-stamped mints
- Split oversized modules and tighten the api surface
- train: Stream the corpus from the hub, no local manifest
- lib: Separate core responsibilities
- lib: Share scanner lifecycle
- lib: Fold sngram-types into sngram
- Move paired modules to mod.rs and drop scoped visibility
- lib: LearnError in learn, one line per public doc
- lib: Drop the checksum-skipping table constructor
- index: Drop the unreachable held-document path


### Documentation
- train: Describe the hub-dataset training flow
- Rewrite for release around the published corpus and one weights feature
- Retell the training story for stack v3 and cut to 0.7.0
- Record the doubled heading separator in indexed output
- Carry ripgrep's license with the code copied from it
- Record the 0.7 release numbers and the schema it lands on
- Record the fixes merged since the release notes were written
- Measure against ripgrep and grep, and correct the release numbers
- Cut the suite table from the front page
- Fold the speedups into the ripgrep and grep columns
- Describe the single crate, the gate and the release
- List the public API and the open work
- Correct the binary policy, the index format and the counts
- Cleaned up main rust docs


### Housekeeping
- training: Track minted weight tables and training state in bins
- Add a changelog and point the guard recipe at a real corpus
- Lint both python projects with ruff in CI
- bench: Cover streaming scan paths
- Bump the workspace to 0.8.0
- Adopt the just, prek, nextest and deny tooling
- skills: Copy the shared agent skills
- Gate, size, semver, release and bench workflows
- Retry elgrep's timing tests twice in nextest
- index: Cover the files the old binary sniff refused
- scripts: Fail the doc scan on an uncompiled rust block
- Pin the toolchain, drop write scopes, gate before publishing
- Run every gate step through a recipe and steady the daemon tests
- Release 1.0.0
- lib: Split plan tuning out of the structure tests
- Run the python benches in the ci recipe
- Add walltime benches on CodSpeed macro runners
- Run all benches on CodSpeed macro runners
- Build benches with the instrument each job runs


### Merge
- Deterministic pre-NUL binary search
- Render indexed paths from the current search argument
- Faster scan
- Balanced scan chunking and parallel postings merge
- Index correctness, faster scan, faster index build
- Decide tiny binary prefixes from held bytes
- Line-anchor mask bits
- Held binary prefixes and line-anchor mask bits
- Stack-v3 parquet trainer with bounded memory
- Cut plan construction from milliseconds to microseconds

## 0.7.0

Indexes built by earlier versions are rebuilt on first use: the postings
schema moved from 16 to 21.

### Correctness

Nine ways an indexed search could return fewer matches than a plain scan:

- A tree being written to could be served from a generation built before the
  last writes. Roughly one query in eight returned in milliseconds having
  missed every new file. A query whose index lost its freshness proof now
  answers from the same walk and workers `--no-index` uses.
- Any file holding a NUL was dropped whole, so a 3.4 MB protobuf whose only
  NUL sat 82 bytes from the end never matched. The bytes before the first NUL
  are indexed.
- Binary files were searched differently depending on how wide a directory
  tree the search covered.
- A UTF-8 byte order mark was indexed as content, so an anchored query whose
  only hit sat on line one of such a file was lost.
- `\A` and `\z` were planned as file edges, but search hands the matcher one
  line at a time. `os\z` on django returned nothing where a scan returned 202
  files.
- A query reaching a generation the daemon had just republished failed with
  "daemon-owned index is not ready" and returned nothing, in 11 of 24 first
  queries against a fresh index. An index a query cannot read now falls back
  to the exact scan. A `--bench` run still fails, because it exists to measure
  the indexed path.
- A build that raced a write still published a generation carrying the
  freshness proof, and for the 35 to 46 ms until the daemon drained the events
  the build had outrun, a query trusted it. Twenty files written during a
  rebuild of a 7,117 file tree were missed by every query in that window. The
  daemon now writes the proof itself, only once a drain after the build finds
  nothing changed since the walk began.
- A tree that lost its watches to a more recently queried tree kept its
  freshness proof file, so re-watching it restored the proof over a generation
  that missed every change made while it was unwatched. An eviction now
  withdraws the proof and the change set with the watches.
- A freshness proof was trusted on its own, so a query that beat the daemon to
  the change it was about to search answered from a generation that missed it.
  A save followed within about 13 ms by a search returned nothing where a scan
  returned the file. Serving from the index now needs the daemon to name, after
  the query started, everything the generation no longer covers.

Also:

- `eg PATTERN ../` returned nothing whenever an index existed, and a
  multi-path search built its index at the working directory rather than
  anywhere containing the requested paths.
- `-w` and `-x` under `--crlf` asked for a line ending in the last pattern
  byte while the verifier saw the carriage return.
- Anchored patterns carry line-start and line-end requirements into the plan.
- A sorted indexed search doubled every block separator: `--` between context
  blocks, a blank line between `--heading` blocks. Sorting runs single
  threaded, where the standard printer owns the separator, but the indexed
  path also handed one to the buffer writer.

### Behaviour

- The `stream` feature exposes `scan_async` for `AsyncBufRead` inputs. It
  emits the same key set and scan summary as synchronous `scan` without
  materializing the complete document.
- `TextScanner` accepts externally classified chunks while preserving the
  same gram and summary format, so storage engines can verify one stream pass.
- Queries the index cannot express, an inverted match, `--passthru`, binary
  flags, a stdin pipe, or a pattern too broad to prefilter, now scan instead
  of exiting 2. Invalid patterns and missing paths still fail.
- The daemon claims at most a third of the system inotify limit, refuses a
  tree whole when it does not fit, and says which knob to turn. It previously
  took as many watches as it wanted, which starved editors and language
  servers, and then hung for an hour when the kernel refused.
- The daemon releases watches for trees without a live lease, and reclaims
  staging from an interrupted build, generations a schema change retired, and
  the state shell of a corpus that is gone.
- A tree that does not fit the watch budget takes the watches of the least
  recently queried tree instead of being refused until capacity frees. Query
  recency comes from the request a foreground query rewrites, so a build
  directory nobody searches does not count as used. A tree whose lease a
  query holds is never evicted, a tree keeps its watches for at least 30
  seconds, and an evicted tree loses its freshness proof before it loses its
  watches, so the next query on it rebuilds or scans.
- A query on a tree with unindexed changes no longer waits for the rebuild.
  The daemon publishes the paths changed since the generation it published,
  and a query searches those directly while trusting the index for the rest.
  A search after a save took 108 ms on a 7,102 file tree and now takes 5 ms,
  and every such query used to abandon the index for a full scan where none
  does now. Above 128 changed paths, or after a directory moves, the query
  falls back to waiting or scanning.
- A tree under continuous writes waits for quiet instead of rebuilding once
  per event, and keeps draining its watcher while the rebuild runs.
- A query waiting on the daemon drew a progress bar the moment it started, so
  every indexed query flashed "indexing changes (0s)". The bar now appears
  only once a wait passes 150ms. Cold builds report progress as before.

### Performance

Measured on isolated corpus copies, same weight table throughout.

| corpus | index build | speedup vs scan | false positives |
|---|---|---|---|
| linux 1.6 GB | 88.5 s to 11.9 s | 6.83x to 8.45x | 27.75% to 26.56% |
| kubernetes | 6.8 s to 2.7 s | 6.72x to 8.31x | 39.69% to 39.64% |
| django | 1.6 s to 0.8 s | 3.83x to 4.34x | 28.04% to 26.58% |
| home-assistant | 3.8 s to 1.7 s | 7.20x to 8.30x | 44.65% |

- `sngram::scan` runs at about 208 MiB/s on code, up from 90.
- Query plan construction drops from 65 ms to 4.4 ms on its worst pattern.
- Posting decode runs at 174 million postings per second, up from 113.
- Index build stopped chunking scan work by file count, which left one thread
  with thousands of generated headers while the rest idled.

### Training

- Two corpora, chosen with `--corpus`. `blend` is the default and the one
  the shipped table was minted from: nine families over 38 sources with
  per-family and per-source byte ceilings summing to 15 TB, dispatched to
  whichever family sits furthest below its target share. `stack-v3` is the
  single-dataset path, kept and still runnable.
- A Stack v3 table was measured against the shipped one and rejected. It
  lost on all four corpora like-for-like, worst on kubernetes at +7.17pp
  false positives, because taking whole repositories at their natural mix
  puts no Go in the top eight languages and 11.9% HTML and XML in them.
  The mint is kept under `train/runs/v3`.
- Shards are read by declared layout and text field, so nested stack
  files, flat parquet columns, and gzipped JSON lines all count.
- A checkpoint is bound to its corpus name and roster fingerprint, so
  resuming into a different corpus is refused rather than silently
  trained. The checkpoint schema moved to 9; existing checkpoints do not
  resume.
- Sampling counts vendored files.
- A stalled shard listing used to hang startup with no output. Each attempt is
  bounded and transport failures retry.

### Attribution

`eg --help` credited ripgrep's author as this project's. The byline now comes
from package metadata. ripgrep keeps every credit it had and gains an explicit
one in the help description and the man page, and its license now ships with
the code copied from it. See `LICENSE-RIPGREP-MIT`.

## Known issues

- `--stats` reports fewer files and bytes searched than the same query under
  `--no-index`, because the index proves most files cannot match and never
  opens them. Match, line, and file-with-match counts agree.
- With about 6,000 watches per large repository and a third of a 65,536 limit,
  roughly four large trees can be watched at once. A fifth takes the watches
  of the least recently queried tree, so the trees in use are the watched
  ones, but a working set larger than the budget still costs one exact scan
  per tree that lost its watches.
- Every indexed answer now depends on the daemon replying within 30ms. A
  daemon that is alive but wedged turns every query into an exact scan. That
  is the right direction to fail, but it is a tighter coupling than before.
- Publishing a generation renames the postings directory into place in two
  steps, so a query can read a manifest from one generation and postings from
  the next. Document counts are checked and any mismatch scans instead, but
  equal counts could still pass. `renameat2(RENAME_EXCHANGE)` would close it.
- Under `--max-depth`, a changed file is collected from its parent directory
  and can surface where the depth limit would have excluded it. That adds a
  candidate the verifier rejects; it never loses one.
