# Changelog

## 1.0.0 (2026-09-04)


### Features
- Sparse n-gram extraction library and types
- CLI modules — counter, resources, session, commands
- Checkpoint save/restore via redb
- Complete CLI — HF streaming, learn, resume, progress
- Final dataset config — 4 datasets, ~49TB total
- Added bins for minted weight tables
- Dead-simple sequential learn loop, retry forever, never crash
- Tracing-based logging with file output and extensive debug sites
- Split stdout (compact human) from file (firehose tracing)
- Minted the 5TB weights
- 10tb weight table
- Sngram-weights crate + license & publishing metadata
- query: Redo regex->plan as 1:1 codesearch port over sparse n-grams
- Added 15tb weights
- Added 15tb weights and made them default
- Streaming StreamScanner extraction, release 0.4.2
- 0.5.0 — fused rolling-hash core, Python trainer, retire 0.4 tables
- Dashboard ready — segmented event log, bounded-memory streaming, total-bytes ETA
- 10TB Linux-filesystem corpus, weighted blend, KL convergence, FS validation
- trained: New bins and distribution5;5u
- Delted old bins
- 15TB run which ended at 12TB
- A new the stack dedup v2 distribution
- Harden sparse ngram indexed search
- eg: Harden fp bench suite
- lib: Derive class, anchor, and edge scan needs
- eg: Delta-varint postings and a 16-byte gram cap
- eg: Pack table records into 12 bytes
- eg: Line-block posting masks (postings-v7)
- lib: Pin edge windows and center grams for oversized crosses
- Word-edge posting bits prune mid-word candidates (postings-v8)
- Demand longest-line length for single-line patterns
- Postings-v9 final index format
- Huffman mask column and planner residual pruning
- Inline-record bitmap, detached lease renewal, bench comparison
- Production CI, semver gate, embedded table in python package
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
- Address all reviewer findings for CLI orchestration
- Datasets reviewer findings
- Recursion limit for opendal type nesting
- HF token from .env, recursive file listing, progress labels
- Fast HF file listing via tree API + reqwest
- Use FineWeb (has content) instead of the-stack-v2-dedup (metadata-only)
- Use codeparrot/github-code (ungated, 1TB code with content)
- Eliminate false negatives in sparse n-gram extraction
- Per-row-group timeout to break silent stalls; timestamp every log line
- Aligned column names to actual hf column names
- Stream shards by row group with mid-file resume + read heartbeat
- eg: Survive daemon restarts without index loss
- train: Remove credential missing error
- trainig: Made MB/s processed goto 34MB/s from 0MB/s
- training: Fix training low throughput
- trainig: Fix minting bug
- Cap stack source share per area
- eg: Sound index ownership and a daemon that never blocks the hot path
- Hide postings progress counters
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
- 2-file prefetch pipeline + decode overlap + bigger HTTP chunks
- Bigger prefetch buffer + bigger opendal gap + correct decode timer
- Reduce indexed benchmark overhead
- Speed up hot indexed startup
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
- types,weights: Cleaned up weights and types crates
- Clean up lib api
- lib: Move everything into right position and cleanup scan api
- lib: Cleanesd lib and introduced better logic for fuller index with more meta
- sngram: Cleanup scan
- eg: Embed fp bench suite and consolidate index runtime
- python: Split the sngram library from the training pipeline
- train: Reduce training to the published hub dataset
- lib,python: One library per language with per-tier embedded weights
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
- Update READMEs for the 0.3 QueryPlan API
- Add fp optimization plan
- Record phase 2 results
- Record phase 3 results
- Record phase 4 mint sweep verdict
- Record phase 5b word-edge results
- Replace historical plans with a current system doc set
- Sync endline and commands with the frozen state
- train: Describe the hub-dataset training flow
- Rewrite the READMEs around the four dev surfaces
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
- Freeze postings-v9 and ship a single production weight tier
- training: Track minted weight tables and training state in bins
- Prepare elgrep 0.6 release
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
- lib: Split plan tuning out of the structure tests
- Run the python benches in the ci recipe
- Add walltime benches on CodSpeed macro runners
- Run all benches on CodSpeed macro runners
- Build benches with the instrument each job runs


### Eg
- Add sparse index baseline
- Memoize postings lookups within a query
- Plan through PlanOptions; support --no-unicode and non-search modes
- Batch OR unions; corpus rows for reviewer findings and flag modes


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


### Plan
- Sparse-native windows, scan covers, bounded repetition
- Harden the analyzer and add verifier-semantics PlanOptions
- Document load-bearing invariants; dedup flush paths; spec tests
- Stop analyzing once the gram budget saturates
- Exact flush accounting, final-flush reserve, x{2,} split, look filtering
- Full-exact bounded reps + plus-base seam tracking
- Boundary-byte seams for wide classes; reject two-class contiguity


### Scripts
- Add fp-rate harness and 130-query corpus
- Differential false-negative check
- Audit sentinel rows for the fp corpus

