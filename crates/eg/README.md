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

A file carrying a NUL is searched up to that NUL rather than skipped
whole, so `eg` reports matches in the text head of a compiled catalog or
an archive where ripgrep reports nothing. On a django checkout that is
1,353 extra files for `-l '.*'`, almost all of them `.mo` and `.png`.
The indexed and `--no-index` paths agree; the difference is against
ripgrep, and it only ever adds files.

## Upgrading to 1.0

1.0 moves the postings schema from 21 to 22 and the tantivy schema from 6
to 7. Indexing no longer runs a signature list and a control-byte density
sniff over the bytes before the first NUL, so a file that sniff refused, an
archive header or a console log, is indexed for its grams instead of being
forced into every candidate set. No file that was indexed is skipped now,
and `--files-with-matches` still names a file exactly when a plain scan
does. The daemon rebuilds any index at an older schema on first contact.

## Upgrading to 0.7

0.7 moves the postings schema from 16 to 21. The daemon rebuilds any
index it finds at an older schema on first contact, destructively and
without asking, so the first query against an existing root after the
upgrade pays for one build. Nothing else about the CLI changes.

## Benchmarks

The embedded 296-query suite, run from the shell through all three tools
with files-with-matches output. A pattern counts toward a row only when
elgrep, ripgrep, and grep agree on the hit set. Measured 2026-07-26 on
isolated corpus copies, each with a hot daemon-owned index, against
ripgrep 15.2.0 and GNU grep 3.11:

| Corpus | Index build | elgrep | ripgrep | grep |
|---|---:|---:|---:|---:|
| linux (1.615 GB) | 11,880 ms | 6,521 ms | 22,507 ms (3.45x) | 506,745 ms (77.7x) |
| k8s (272 MB) | 2,659 ms | 1,518 ms | 8,311 ms (5.48x) | 100,842 ms (66.5x) |
| hass-core (179 MB) | 1,697 ms | 1,315 ms | 7,044 ms (5.36x) | 76,065 ms (57.9x) |
| django (38 MB) | 765 ms | 1,083 ms | 3,283 ms (3.03x) | 21,625 ms (20.0x) |

Every query there pays process startup, which is what a person pays at a
shell and which costs the fastest tool proportionally most. In process,
`eg --bench` puts the same suite at 8.45x a plain scan on linux, 3,312 ms
against 27,991 ms, and the index is 1,467,104,135 bytes, 0.91x the corpus
text. The run fails if any indexed hit set diverges from its scan hit
set, so a lost match fails the build rather than reaching a user.

Per pattern on linux, p50 of 9 runs, each hit set checked against
ripgrep's before it was timed:

| Pattern | Matched files | elgrep | ripgrep | grep |
|---|---:|---:|---:|---:|
| `linus tor` | 0 | 5.9 ms | 84.0 ms (14.2x) | 821.2 ms (139.3x) |
| `EXPORT_SYMBOL_GPL` | 3627 | 16.6 ms | 86.5 ms (5.2x) | 671.7 ms (40.6x) |
| `copy_from_user` | 1221 | 7.0 ms | 82.9 ms (11.9x) | 677.9 ms (97.1x) |
| `schedule_timeout` | 418 | 9.3 ms | 86.1 ms (9.2x) | 684.2 ms (73.4x) |

The spread is the index working as designed. A pattern that matches
nothing is answered from posting lists alone; a pattern that matches
3,627 files still costs the verifier 3,627 file reads.

```sh
eg --bench 'max_\w+_size' ~/src/linux   # one indexed query, JSON report
cd ~/src/linux && eg --bench            # the whole suite
```

The hand comparison, run from inside the corpus:

```sh
eg -l -e PATTERN ./
eg --no-index -l -e PATTERN ./
rg -l -e PATTERN ./
grep -rl --binary-files=without-match --exclude-dir=.git -e PATTERN ./
```

## License

MIT. The command line facade and the search path it drives are copied from
[ripgrep](https://github.com/BurntSushi/ripgrep) by Andrew Gallant, at the
revision `eg --version` reports, and are used under ripgrep's MIT license.
See `LICENSE-RIPGREP-MIT`.
