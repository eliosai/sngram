# elgrep

`elgrep` is an indexed ripgrep alternative. The `eg` command carries
ripgrep's search path and adds a sparse n-gram prefilter: the index
narrows each query to candidate files, and the real regex engine
verifies them, so results match a plain scan exactly. The `eg-indexd`
daemon builds and maintains indexes in the background.

## Installation

```sh
cargo install elgrep
```

This installs `eg` for searches and `eg-indexd` next to it for
background index maintenance.

## Usage

The CLI adds a few index-specific flags. By default, searches use the index.
The first query for a new root blocks while the index is built; after that,
the daemon keeps the index fresh in the background.

```sh
eg 'max_\w+_size' ~/src/linux
eg --no-index 'max_\w+_size' ~/src/linux   # plain scan, no index used
```

Every ripgrep invocation works. When the index cannot serve a query, `eg`
searches without it and returns the same results a scan would, just
slower. That covers flags the sparse grams cannot express (`-v`,
`--passthru`, `-a`, `-z`, `--pre`, `-E`, `-P`, `--null-data`), stdin
pipes, patterns with no selective n-gram, and patterns that select so
much of the corpus that verifying candidates costs more than scanning.
`--debug` names the reason; an invocation that named `--index` explicitly
gets one line on stderr. Only real errors, an invalid regex or a missing
path, still fail.

## Upgrading to 0.7

0.7 moves the postings schema from 16 to 21. The daemon rebuilds any
index it finds at an older schema on first contact, destructively and
without asking, so the first query against an existing root after the
upgrade pays for one build. Nothing else about the CLI changes.

## Benchmarks

The embedded 296-query suite runs every query through the index and again
through `eg --no-index`, on the same tree with files-with-matches output.
Measured 2026-07-25 on isolated corpus copies, each with a hot
daemon-owned index:

| Corpus | Index build | Suite vs scan | False positives | False negatives |
|---|---:|---:|---:|---:|
| linux (1.615 GB) | 13,101 ms | 7.87x | 26.56% | 0 |
| k8s | 2,611 ms | 7.59x | 39.64% | 0 |
| hass-core | 1,649 ms | 7.47x | 44.65% | 0 |
| django | 678 ms | 4.21x | 26.58% | 0 |

On linux the suite finishes in about 3,630 ms indexed against about
28,600 ms scanning, and the index is 1,467,104,135 bytes, 0.91x the
corpus text. The run fails if any indexed hit set diverges from its scan
hit set, so the zero false-negative column is enforced, not observed.

Those columns compare elgrep against itself. `--bench` adds a ripgrep leg
when it finds an `rg` binary on PATH, and the numbers above were taken on
a machine without one.

```sh
eg --bench 'max_\w+_size' ~/src/linux   # one indexed query, JSON report
cd ~/src/linux && eg --bench            # the whole suite
```

The hand comparison, run from inside the corpus:

```sh
eg --files-with-matches --color never --no-heading -e PATTERN ./
eg --no-index --files-with-matches --color never --no-heading -e PATTERN ./
grep -rIl --exclude-dir=.git --exclude-dir=.eg -e PATTERN ./
```

## License

MIT. The command line facade and the search path it drives are copied from
[ripgrep](https://github.com/BurntSushi/ripgrep) by Andrew Gallant, at the
revision `eg --version` reports, and are used under ripgrep's MIT license.
See `LICENSE-RIPGREP-MIT`.
