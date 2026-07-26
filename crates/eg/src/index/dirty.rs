//! Paths the daemon reports changed since it published the current generation.

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

use crate::{
    flags::{HiArgs, SearchMode},
    haystack::Haystack,
};

use super::{generation::Generation, runtime, verify};

const DIRTY_FILE_NAME: &str = "dirty-paths";
const UNBOUNDED_MARKER: &str = "unbounded";
const DRAINED_PREFIX: &str = "drained=";
const PATH_PREFIX: &str = "path=";
/// Changed paths past which scanning them one by one stops beating a full scan
const MAX_DIRTY_PATHS: usize = 128;
const MAX_DIRTY_PATHS_ENV: &str = "EG_INDEX_MAX_DIRTY_PATHS";

/// Paths a published generation does not cover, and the drain that named them
pub struct DirtyLedger {
    drained_at: SystemTime,
    paths: BTreeSet<PathBuf>,
}

impl DirtyLedger {
    /// Read the daemon's change set, or `None` when it names no bounded set
    pub fn read(state_root: &Path, index_root: &Path) -> Option<Self> {
        let text =
            fs::read_to_string(runtime::runtime_dir(state_root).join(DIRTY_FILE_NAME)).ok()?;
        let mut ledger = Self {
            drained_at: SystemTime::UNIX_EPOCH,
            paths: BTreeSet::new(),
        };
        let mut stamped = false;
        for line in text.lines() {
            if line == UNBOUNDED_MARKER {
                return None;
            }
            if let Some(value) = line.strip_prefix(DRAINED_PREFIX) {
                ledger.drained_at = unix_time(value.parse::<u128>().ok()?);
                stamped = true;
            } else if let Some(value) = line.strip_prefix(PATH_PREFIX) {
                ledger.paths.insert(index_root.join(path_from_hex(value)?));
            }
        }
        (stamped && ledger.paths.len() <= max_dirty_paths()).then_some(ledger)
    }

    /// True when the daemon drained the watcher no earlier than this instant
    pub fn drained_since(&self, floor: SystemTime) -> bool {
        self.drained_at >= floor
    }

    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    pub fn contains(&self, path: &Path) -> bool {
        self.paths.contains(path)
    }

    /// Directories holding at least one changed path
    fn parents(&self) -> BTreeSet<&Path> {
        self.paths.iter().filter_map(|path| path.parent()).collect()
    }
}

/// Longest a query waits for the daemon to vouch for its change set
const DIRT_RENDEZVOUS: Duration = Duration::from_millis(30);
const DIRT_RENDEZVOUS_ENV: &str = "EG_INDEX_DIRT_WAIT_MS";
const DIRT_POLL: Duration = Duration::from_micros(250);

/// Instant a query's change set must account for, taken before the daemon wakes
pub fn change_floor() -> SystemTime {
    SystemTime::now()
}

/// What the daemon will vouch for about one generation at one instant.
pub enum Vouched {
    /// the generation covers every change that landed before the query
    Covered,
    /// these paths changed since the generation and must be searched directly
    Changed(DirtyLedger),
}

/// Settle what the daemon vouches for, or `None` when it vouches for nothing.
///
/// A freshness proof only says a walk once covered the tree. Trusting the
/// index needs the daemon to name, after this query started, everything that
/// walk no longer covers.
pub fn vouch(
    args: &HiArgs,
    mode: SearchMode,
    generation: &Generation,
    floor: SystemTime,
) -> Option<Vouched> {
    let ledger = uncovered_changes(generation, floor)?;
    if ledger.is_empty() {
        return Some(Vouched::Covered);
    }
    if verify::is_full_corpus_mode(args, mode) {
        return None;
    }
    Some(Vouched::Changed(ledger))
}

/// Changed paths this query searches directly instead of trusting the index
fn uncovered_changes(generation: &Generation, floor: SystemTime) -> Option<DirtyLedger> {
    if !runtime::daemon_watch_supported() || !runtime::daemon_watches(generation.state_root()) {
        return None;
    }
    let _hold = runtime::LeaseHold::acquire(generation.state_root());
    runtime::Lease::new(generation.index_root(), generation.state_root()).keep_alive();
    let started = Instant::now();
    let deadline = started + dirt_rendezvous();
    loop {
        let ledger = DirtyLedger::read(generation.state_root(), generation.index_root());
        match ledger {
            Some(ledger) if ledger.drained_since(floor) => return Some(ledger),
            _ if Instant::now() >= deadline => {
                log::debug!(
                    "eg index: the daemon did not vouch for its change set in {:?}",
                    started.elapsed()
                );
                return None;
            },
            _ => std::thread::sleep(DIRT_POLL),
        }
    }
}

fn dirt_rendezvous() -> Duration {
    std::env::var(DIRT_RENDEZVOUS_ENV)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map_or(DIRT_RENDEZVOUS, Duration::from_millis)
}

/// Search every changed path directly rather than trusting the generation
pub fn fold_into(
    args: &HiArgs,
    snapshot: &mut super::manifest::CurrentSnapshot,
    candidates: &mut BTreeSet<usize>,
    ledger: &DirtyLedger,
) {
    if ledger.is_empty() {
        return;
    }
    candidates.retain(|ord| {
        snapshot
            .file(*ord)
            .is_none_or(|file| !ledger.contains(&file.path))
    });
    for haystack in uncovered_haystacks(args, ledger) {
        let ord = snapshot.add_uncovered(haystack.path().to_path_buf(), haystack.is_explicit());
        candidates.insert(ord);
    }
    log::debug!(
        "eg index: folded {} changed paths into the candidate set",
        ledger.paths.len()
    );
}

/// Changed files the walk that built the generation would have collected
fn uncovered_haystacks(args: &HiArgs, ledger: &DirtyLedger) -> Vec<Haystack> {
    let builder = args.haystack_builder();
    let mut found = Vec::new();
    for parent in ledger.parents() {
        let Ok(mut walk) = args.walk_builder_rooted(parent) else {
            continue;
        };
        for result in walk.max_depth(Some(1)).build() {
            let Some(haystack) = builder.build_from_result(result) else {
                continue;
            };
            if ledger.contains(haystack.path()) {
                found.push(haystack);
            }
        }
    }
    found
}

fn max_dirty_paths() -> usize {
    std::env::var(MAX_DIRTY_PATHS_ENV)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(MAX_DIRTY_PATHS)
}

fn unix_time(nanos: u128) -> SystemTime {
    let secs = u64::try_from(nanos / 1_000_000_000).unwrap_or(u64::MAX);
    let rest = u32::try_from(nanos % 1_000_000_000).unwrap_or_default();
    SystemTime::UNIX_EPOCH + Duration::new(secs, rest)
}

fn path_from_hex(value: &str) -> Option<PathBuf> {
    if !value.len().is_multiple_of(2) {
        return None;
    }
    let mut bytes = Vec::with_capacity(value.len() / 2);
    for pair in value.as_bytes().chunks_exact(2) {
        bytes.push((hex_nibble(pair[0])? << 4) | hex_nibble(pair[1])?);
    }
    Some(PathBuf::from(os_string_from_bytes(bytes)))
}

const fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

#[cfg(unix)]
fn os_string_from_bytes(bytes: Vec<u8>) -> std::ffi::OsString {
    use std::os::unix::ffi::OsStringExt;
    std::ffi::OsString::from_vec(bytes)
}

#[cfg(not(unix))]
fn os_string_from_bytes(bytes: Vec<u8>) -> std::ffi::OsString {
    std::ffi::OsString::from(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::{DIRTY_FILE_NAME, DirtyLedger};
    use std::{fs, path::Path, time::SystemTime};

    fn scratch(name: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("eg-dirty-{name}-"))
            .tempdir()
            .expect("scratch dir")
    }

    fn write_ledger(state_root: &Path, body: &str) {
        let runtime = state_root.join("runtime");
        fs::create_dir_all(&runtime).expect("runtime");
        fs::write(runtime.join(DIRTY_FILE_NAME), body).expect("ledger");
    }

    #[test]
    fn a_named_change_set_resolves_against_the_corpus_root() {
        let guard = scratch("named");
        let state_root = guard.path();
        write_ledger(state_root, "drained=1000000000\npath=7372632f612e7273\n");

        let ledger = DirtyLedger::read(state_root, Path::new("/repo")).expect("ledger");

        assert!(ledger.contains(Path::new("/repo/src/a.rs")));
        assert!(!ledger.is_empty());
        assert!(ledger.drained_since(SystemTime::UNIX_EPOCH));
    }

    #[test]
    fn an_unbounded_change_set_is_refused() {
        let guard = scratch("unbounded");
        let state_root = guard.path();
        write_ledger(state_root, "drained=1000000000\nunbounded\n");

        assert!(DirtyLedger::read(state_root, Path::new("/repo")).is_none());
    }

    #[test]
    fn a_ledger_without_a_drain_stamp_is_refused() {
        let guard = scratch("unstamped");
        let state_root = guard.path();
        write_ledger(state_root, "path=61\n");

        assert!(DirtyLedger::read(state_root, Path::new("/repo")).is_none());
    }

    #[test]
    fn a_missing_ledger_is_refused() {
        let guard = scratch("missing");

        assert!(DirtyLedger::read(guard.path(), Path::new("/repo")).is_none());
    }

    #[test]
    fn a_stale_drain_stamp_does_not_answer_for_a_later_floor() {
        let guard = scratch("floor");
        let state_root = guard.path();
        write_ledger(state_root, "drained=1000000000\n");

        let ledger = DirtyLedger::read(state_root, Path::new("/repo")).expect("ledger");

        assert!(!ledger.drained_since(SystemTime::now()));
        assert!(ledger.is_empty());
    }
}
