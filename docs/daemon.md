# The index daemon

`eg-indexd` owns the index lifecycle: it builds, watches, refreshes, and
deletes indexes. A foreground `eg` process reads a daemon-proofed index
and may block once for a first missing-index build; after that the daemon
keeps the index fresh in the background.

## Runtime markers

Each index root gets a runtime directory under its state root with five
markers:

- `owner`: pid and identity of the daemon that owns this index.
- `watcher-ready`: the inotify watcher covers the tree.
- `journal-clean`: no unprocessed change events; its mtime is the
  freshness watermark.
- `wake`: a foreground process asked for attention; mtime newer than
  `journal-clean` means pending work.
- `lease`: foreground processes touch this on every query; the daemon
  drops roots whose lease expires.

A global runtime directory holds the daemon owner marker and a requests
directory where foreground processes drop `<hash>.request` files naming
an index root, the eg binary, and the walk arguments.

## Freshness proof

A foreground process trusts an index only with daemon proof: the daemon
startup marker is ready, the owner marker names a live daemon, the
watcher is ready, and `journal-clean` is newer than any `wake`. A stale
index without proof is invalid, and the query takes the cold path. The
proof is a handful of stats, read fresh on every query.

## Hot and cold paths

Hot path: the index exists with proof. The query renews the lease from a
detached thread (registration costs the query nothing; measured 9.4ms on
the query before detaching, 0.05ms after) and executes immediately. If
the detached renewal fails, nothing breaks now; the proof eventually goes
stale and the next query takes the cold path, where registration is
blocking and surfaces its error.

Freshness is ordered by the watcher. Once the watcher notices a change it
drops `journal-clean` and queries wait for the refresh; a query that races
a write inside the watcher's reaction window (microseconds) may still be
served by the previous generation, the same way a scan racing a write may
read either version.

A query that arrives while the daemon is refreshing does not wait for the
tree to go fully quiet. It records the wake it registered and accepts the
index as soon as the owning daemon's `journal-clean` passes that wake, so
under continuous file churn a query waits for at most one refresh cycle
instead of starving. The foreground renders that wait as a single spinner;
the detailed phase bar only shows on a first build.

Cold path: no index or no proof. The process writes a durable request,
touches `wake`, spawns the daemon if none is live, and blocks polling for
the freshness proof. The daemon walks the tree, builds the index in a
staging directory, fsyncs, and renames it into place; a crashed rename
recovers from the `.old` sibling on the next build.

## Daemon loop

The daemon scans the requests directory, adopts roots whose index
matches the manifest on disk (a no-op refresh), and discards state for
roots whose directory vanished. Per root it installs an inotify watch,
processes change journals, rebuilds what changed, and republishes.
Missing roots and unwatchable paths are tolerated rather than fatal: a
request for a deleted directory discards that request instead of killing
the serve loop. A killed daemon restarts and re-adopts published indexes
from their manifests in about two seconds, without rebuilding.

## Ownership rules

The daemon is the only writer of index directories. Foreground processes
never delete or rebuild a published index; they request. Format version
bumps invalidate the manifest schema, so the daemon rebuilds destructively
on first contact with an old index. Concurrent builds serialize on an
advisory file lock next to the index directory.
