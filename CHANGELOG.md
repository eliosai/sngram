# Changelog

## 0.7.0

Indexes built by earlier versions are rebuilt on first use: the postings
schema moved from 16 to 21.

### Correctness

Six ways an indexed search could return fewer matches than a plain scan:

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

Also:

- `eg PATTERN ../` returned nothing whenever an index existed, and a
  multi-path search built its index at the working directory rather than
  anywhere containing the requested paths.
- `-w` and `-x` under `--crlf` asked for a line ending in the last pattern
  byte while the verifier saw the carriage return.
- Anchored patterns carry line-start and line-end requirements into the plan.
- Tiny binary prefixes are decided from bytes held in the index rather than
  forced into every candidate set.
- A sorted indexed search doubled every block separator: `--` between context
  blocks, a blank line between `--heading` blocks. Sorting runs single
  threaded, where the standard printer owns the separator, but the indexed
  path also handed one to the buffer writer.

### Behaviour

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
- A tree under continuous writes waits for quiet instead of rebuilding once
  per event.
- A query waiting on the daemon drew a progress bar the moment it started, so
  every indexed query flashed "indexing changes (0s)". The bar now appears
  only once a wait passes 150ms. Cold builds report progress as before.

### Performance

Measured on isolated corpus copies, same weight table throughout.

| corpus | index build | speedup vs scan | false positives |
|---|---|---|---|
| linux 1.6 GB | 88.5 s to 13.1 s | 6.83x to 7.87x | 27.75% to 26.56% |
| kubernetes | 6.8 s to 2.6 s | 6.72x to 7.59x | 39.69% to 39.64% |
| django | 1.6 s to 0.7 s | 3.83x to 4.21x | 28.04% to 26.58% |
| home-assistant | 3.8 s to 1.6 s | 7.20x to 7.47x | 44.65% |

- `sngram::scan` runs at about 208 MiB/s on code, up from 90.
- Query plan construction drops from 65 ms to 4.4 ms on its worst pattern.
- Posting decode runs at 174 million postings per second, up from 113.
- Index build stopped chunking scan work by file count, which left one thread
  with thousands of generated headers while the rest idled.

### Training

- The corpus is The Stack v3, read directly from parquet with file content
  inline. The separate object store fetch is gone.
- Sampling takes whole repositories with their natural file mix. Vendored
  files are counted.
- A full pass over 15.9 TB of decoded source takes about 13 hours at roughly
  340 MB/s, holding about 2.2 GB of memory.
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
- A query landing in the window right after a rebuild publishes can still
  trust a slightly old generation. Closing it means ordering the walk against
  the watcher event stream.
- With about 6,000 watches per large repository and a third of a 65,536 limit,
  roughly four large trees can be watched at once. A fifth takes the watches
  of the least recently queried tree, so the trees in use are the watched
  ones, but a working set larger than the budget still costs one exact scan
  per tree that lost its watches.
