# Index Daemon Plan

This is the production shape for indexed `eg`. The daemon is a cache
maintainer, not a search server.

## Public Boundary

The CLI-facing index API stays small:

- `index::run(&HiArgs)` runs indexed search, maintenance, structured bench, and
  internal daemon refresh.
- `index::IndexConfig` is the only index configuration type visible to flag
  parsing.
- `index` keeps daemon spawning, runtime files, generation selection, refresh,
  backend details, and candidate verification internal.

Search remains in the foreground CLI process. The CLI plans the query, selects a
ready generation from disk, maps the index, queries candidates, and verifies
them through the copied ripgrep worker. The daemon only keeps generations fresh
after requests have registered them.

## Terms

- `SearchRoots`: the user-requested haystacks for this invocation.
- `IndexRoot`: the directory whose index can cover a set of `SearchRoots`.
- `StateRoot`: the `.eg` or XDG cache directory holding index and runtime state.
- `Generation`: a compatible published index backend plus manifest.
- `Walk`: the filtered filesystem traversal used to build a generation.
- `Haystack`: a file accepted by the walk and eligible for indexing/search.

Do not use `scope` in new code. `IndexRoot` and `SearchRoots` are more precise.

## Runtime Model

Use one global `eg-indexd` daemon per user runtime root. The CLI never asks the
daemon questions and never blocks on daemon RPC. It only touches files:

- `StateRoot/runtime/lease`
- `StateRoot/runtime/wake`
- `$RUNTIME/eg/requests/<key>.request`

The request file carries:

- `cwd`
- `index_root`
- `state_root`
- replayable CLI argv

The daemon reads requests, watches their `IndexRoot`s, and replays `eg` with an
internal refresh environment. That refresh path builds from a fresh walk and
does not plan or verify the original regex.

## Disk Proof

A CLI hot path may skip the full freshness walk only when disk proves the
generation is daemon-maintained:

1. The backend manifest exists.
2. The manifest identity matches the requested backend, weights fingerprint,
   index format, scanner/query format, and walk/filter fingerprint.
3. `runtime/watcher-ready` exists.
4. `runtime/journal-clean` exists.
5. `runtime/lease` is live.

If any check fails, the CLI falls back to the normal manifest freshness path or
a cold build. The daemon proof is file-based; there is no socket and no blocking
daemon call.

## Daemon Responsibilities

The daemon:

- watches active `IndexRoot`s recursively
- clears `journal-clean` when the watched tree changes
- refreshes requested generations by replaying `eg` in internal refresh mode
- writes `journal-clean` only after refresh succeeds
- removes child index directories covered by a live parent index
- exits after all leases expire

The current implementation uses Linux inotify for watcher-backed clean markers.
Non-Linux builds do not publish `watcher-ready`, so the CLI does not trust a
daemon freshness proof there.

## Parent And Child Indexes

A parent `IndexRoot` can answer child `SearchRoots`; candidates are restricted
to the requested roots before verification. A child index cannot answer a parent
search.

When a parent generation exists, covered child index directories are redundant.
The daemon may delete the child `StateRoot/index` and keep the parent lease
alive. Do not merge child backend files into the parent in this version; ordinals,
manifests, summaries, deletes, and freshness make that the wrong first cut.

## Cold And Hot Paths

Cold miss:

1. Resolve `SearchRoots`.
2. Choose the broadest compatible ready generation from disk, or the exact
   `IndexRoot` for a build.
3. Touch lease/wake/request files.
4. Walk and build synchronously in the CLI when no usable generation exists.
5. Query and verify.

Hot daemon path:

1. Resolve `SearchRoots`.
2. Select a compatible generation from disk.
3. Validate the daemon disk proof.
4. Snapshot from the manifest without a full freshness walk.
5. Map/query/verify.

Progress output belongs only to cold build, rebuild, repair, or explicit debug
paths. Hot daemon search should be quiet unless `--bench` is active.

## Bench Mode

`--bench` is valid only for indexed search. It suppresses normal match output
and emits one JSON object to stdout for success and failure. It should remain a
real indexed run, not a synthetic benchmark harness.

The report includes timings, counts, false-positive rates, backend byte sizes,
generation source, freshness proof, and unsupported reasons. Missing fields are
bugs because comparison scripts need a stable schema.

## Remaining Work

- Refactor `index::run` into `SearchRequest`, `QueryPlanner`, `Generation`,
  `InitialBuild`, `Lease`, and `CandidateVerifier` so the top-level function is
  the intended linear story.
- Move from the legacy mutable backend directory to explicit immutable
  `Generation` directories with an atomic `current` pointer.
- Add integration coverage for daemon refresh through the `eg-indexd` binary.
- Add benchmark targets for hot catalog/mmap, query-only, verify-only, cold
  build, parent restriction, and bench overhead.
