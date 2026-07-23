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

## Benchmarks

These results are from the Linux kernel source tree on a hot daemon-owned
index. Each row reports 9 wall-time runs per tool, using files-with-matches
output and identical normalized hit sets.

| Pattern | Matched files | elgrep p50 / p95 | ripgrep p50 / p95 | grep p50 / p95 | Speedup vs ripgrep | Speedup vs grep |
|---|---:|---:|---:|---:|---:|---:|
| `linus tor` | 0 | 10.2 / 11.6 ms | 185.9 / 201.5 ms | 1345.8 / 1360.9 ms | 18.2x | 131.6x |
| `EXPORT_SYMBOL_GPL` | 3610 | 45.4 / 48.2 ms | 202.6 / 209.5 ms | 1093.1 / 1107.6 ms | 4.5x | 24.1x |
| `copy_from_user` | 1224 | 19.2 / 21.3 ms | 199.3 / 203.1 ms | 1121.3 / 1162.0 ms | 10.4x | 58.5x |
| `schedule_timeout` | 418 | 13.6 / 15.3 ms | 177.4 / 183.7 ms | 963.2 / 989.6 ms | 13.0x | 70.8x |

Benchmark commands:

```sh
eg --files-with-matches --color never --no-heading -e PATTERN ./
rg --files-with-matches --color never --no-heading -e PATTERN ./
grep -rIl --exclude-dir=.git --exclude-dir=.eg -e PATTERN ./
```

You can also use `--bench` to inspect one indexed invocation or run the
embedded false-positive suite.

```sh
eg --bench 'max_\w+_size' ~/src/linux
cd ~/src/linux && eg --bench
```
