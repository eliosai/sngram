# Index Daemon Plan

This is the intended architecture for production indexed `eg`. It is a plan,
not an implementation.

## Goal

Keep the CLI boundary simple:

- `index::run(&HiArgs)` is the only indexed-search behavior entrypoint.
- `index::IndexConfig` is the only index configuration type visible to the CLI
  argument layer.
- All daemon spawning, runtime paths, index generation selection, refresh
  policy, and backend details stay inside `index`.

The daemon is not a search server. Search stays in the CLI: the CLI plans the
query, maps the ready index, finds candidate files, and verifies those
candidates through the copied ripgrep search worker. The daemon only maintains
ready index generations after an initial build exists.

## Process Shape

Use a second binary, tentatively `eg-indexd`.

`index::run` owns daemon startup. The user-facing CLI does not expose daemon
commands. On demand, `index::run` ensures the daemon binary is available at a
stable runtime path:

```text
$XDG_RUNTIME_DIR/eg/bin/eg-indexd
```

When `XDG_RUNTIME_DIR` is unset, use a per-user temp runtime root with the same
layout. The runtime copy is versioned by the `eg` build identity, so a stale
daemon binary cannot maintain an index with incompatible code.

The daemon code should live under:

```text
crates/eg/src/index/daemon/
```

and the binary entrypoint should be minimal, forwarding into that module. The
search CLI still calls only `index::run`.

## Scope Model

An index has a scope root. The scope root is the largest compatible directory
index that can answer the query safely.

Rules:

- A parent index can answer a child query. Searching `/repo/src` can use an
  existing compatible `/repo` index, then restrict candidate paths to `/repo/src`.
- A child index cannot answer a parent query. Searching `/repo` cannot use an
  index that only covers `/repo/src`.
- When a parent index becomes healthy, child indexes covered by that parent are
  redundant. The daemon may remove covered child runtime indexes and refresh the
  parent TTL.
- Do not merge child index files into a parent index in the first version.
  Reusing child segments would complicate ordinals, manifests, summaries,
  deletes, and freshness. Parent rebuild plus incremental maintenance is the
  simpler correct model.

Compatibility is determined by the indexed corpus root plus the walk/filter
fingerprint, weight fingerprint, index format version, scanner/query format,
and backend generation format. If any of those differ, the parent is not
compatible.

## Cold Miss Flow

When `index::run` cannot find a ready generation:

1. Resolve the scope root and runtime state root.
2. Start the per-scope daemon and wait for a watcher-ready marker.
3. The CLI performs the initial full build synchronously, as it does today.
4. Show a progress bar when stderr is a TTY; otherwise emit sparse progress
   lines under `--debug`.
5. Publish the initial generation atomically.
6. Query the generation and verify candidates.
7. Leave the daemon running to maintain incremental changes until the idle TTL.

Starting the daemon before the full build is important. Files created, removed,
or changed while the CLI is scanning must be captured by the daemon journal. For
the first query after publish, any dirty paths observed during the build should
be treated as forced candidates until the daemon has applied them to a new
generation.

## Hot Path

When a ready generation exists and the daemon lease is healthy, the CLI should
avoid a full freshness walk.

The hot path still performs cheap validation:

- generation manifest exists and is complete
- format and weight fingerprints match
- walk/filter fingerprint matches
- daemon lease is alive
- watcher state is healthy
- published generation pointer is atomic and immutable

Then the CLI mmaps the index and queries it. This is "no freshness walk", not
"trust stale files blindly."

## Runtime Protocol

Use files in the runtime state root, not sockets.

Suggested layout:

```text
runtime/
  bin/
    eg-indexd
  scopes/
    <scope-key>/
      lock
      daemon.pid
      lease
      watcher-ready
      current
      generations/
        <generation-id>/
      journal/
      requests/
```

The daemon owns:

- `daemon.pid`
- `lease`
- `watcher-ready`
- `journal/`
- generation publication after incremental updates

The CLI owns:

- initial full generation build on cold miss
- atomic first publish
- query execution and verification
- touching the lease/request file to keep the daemon alive

`current` must point to an immutable generation. Never mutate a generation in
place. New work publishes a new generation, then old generations are garbage
collected after readers can no longer hold them.

## Daemon Responsibilities

The daemon does not perform the initial full build.

It does:

- watch the scope root recursively
- debounce filesystem events
- record events while the initial build is running
- scan new and changed files
- remove deleted files
- maintain delta segments
- compact deltas into a new immutable generation when thresholds are reached
- clean up covered child runtime indexes
- refresh or expire the per-scope TTL
- shut down after the idle timeout
- optionally delete runtime-only index state on shutdown

## One Daemon Per Scope

Use one daemon per index scope for the first production version.

This keeps the model simple:

- one root watcher
- one lock
- one lease
- one journal
- one current generation pointer
- one cleanup policy

A single global daemon can be added later if many active roots become common,
but it is not the simplest correct architecture.

## TTL And Cleanup

The daemon exits after an idle timeout. A query refreshes the lease. The timeout
can be configured later, but the default should be conservative enough to keep a
developer session warm without leaving long-lived background work.

Runtime-only indexes may be deleted at daemon shutdown. A later persistent-cache
mode can keep generations across daemon exits for faster cold starts. The exact
default should be chosen after measuring build cost versus disk churn.

## Non-Goals For The First Version

- No search server.
- No socket protocol.
- No child-to-parent segment merge.
- No global daemon.
- No trusting runtime indexes without a healthy lease and watcher state.
- No public daemon CLI.
