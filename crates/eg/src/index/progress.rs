//! Foreground rendering and daemon progress state for cold index builds.

use std::{
    fs,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant},
};

use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use serde::{Deserialize, Serialize};

const RUNTIME_DIR_NAME: &str = "runtime";
const PROGRESS_FILE_NAME: &str = "build-progress.json";
/// Prefix that names the wait on every cold-build progress line
const BUILDING: &str = "building index";
/// Line printed above the bar the first time a tree is indexed
const COLD_BUILD_NOTE: &str = "building a sparse n-gram index. This wait happens once per tree.";
const PROGRESS_POLL: Duration = Duration::from_millis(100);
/// Waits shorter than this paint nothing
const SHOW_AFTER: Duration = Duration::from_millis(150);
const SCAN_UPDATE_FILES: u64 = 512;
const WALK_UPDATE_ITEMS: u64 = 512;
const SNAPSHOT_UPDATE_FILES: u64 = 512;
const POSTINGS_UPDATE_PAIRS: u64 = 1_000_000;

/// Build phase reported by the daemon refresh process.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildPhase {
    /// Walking the configured search roots.
    Walking,
    /// Creating the manifest snapshot from the walk result.
    Snapshot,
    /// Scanning files into sparse grams.
    Scanning,
    /// Writing summary records.
    WritingSummary,
    /// Writing posting-list storage.
    WritingPostings,
    /// Writing the generation manifest.
    WritingManifest,
    /// Publishing the completed generation atomically.
    Publishing,
    /// The generation is daemon-proofed and ready.
    Ready,
}

impl BuildPhase {
    const fn label(self) -> &'static str {
        match self {
            Self::Walking => "walking the tree",
            Self::Snapshot => "reading file metadata",
            Self::Scanning => "scanning files",
            Self::WritingSummary => "writing summaries",
            Self::WritingPostings => "writing postings",
            Self::WritingManifest => "writing the manifest",
            Self::Publishing => "publishing",
            Self::Ready => "index ready",
        }
    }
}

/// Last-known cold build progress persisted under the index runtime directory.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct BuildSnapshot {
    phase: Option<BuildPhase>,
    files_total: u64,
    files_done: u64,
    bytes_done: u64,
    dirs_done: u64,
    items_total: u64,
    items_done: u64,
    runs_total: u64,
    runs_done: u64,
}

impl BuildSnapshot {
    /// True while the snapshot describes a build that is still running
    const fn is_building(&self) -> bool {
        !matches!(self.phase, None | Some(BuildPhase::Ready))
    }

    /// Total and completed units for the bar, or `None` for a phase with no count
    const fn bar_bounds(&self) -> Option<(u64, u64)> {
        if matches!(self.phase, Some(BuildPhase::WritingPostings)) {
            return None;
        }
        if self.files_total > 0 {
            return Some((self.files_total, self.files_done));
        }
        if self.items_total > 0 {
            return Some((self.items_total, self.items_done));
        }
        None
    }

    fn message(&self) -> String {
        let Some(phase) = self.phase else {
            return BUILDING.to_owned();
        };
        match phase {
            BuildPhase::Walking => {
                format!(
                    "{BUILDING}: walking the tree, {} files so far",
                    self.files_done
                )
            },
            BuildPhase::Scanning if self.bytes_done > 0 => {
                format!(
                    "{BUILDING}: scanning files, {} MiB",
                    self.bytes_done / 1024 / 1024
                )
            },
            other => format!("{BUILDING}: {}", other.label()),
        }
    }
}

/// Progress writer used by the daemon-owned build path.
pub struct BuildProgress {
    path: PathBuf,
    last_update: Mutex<BuildProgressCursor>,
}

impl BuildProgress {
    /// Create a progress writer for an index state root.
    pub fn new(state_root: &Path) -> Self {
        Self {
            path: progress_path(state_root),
            last_update: Mutex::new(BuildProgressCursor::default()),
        }
    }

    /// Remove stale progress from an earlier interrupted build.
    pub fn clear(&self) {
        let _ = fs::remove_file(&self.path);
    }

    /// Mark a phase that does not have file-level progress.
    pub fn phase(&self, phase: BuildPhase) {
        self.reset_update_cursor();
        let _ = self.write(&BuildSnapshot {
            phase: Some(phase),
            ..BuildSnapshot::default()
        });
    }

    /// Update corpus walk progress
    pub fn update_walk(&self, entries_done: u64, files_done: u64, dirs_done: u64) {
        if !self.should_update(ProgressKind::Walk, entries_done, WALK_UPDATE_ITEMS) {
            return;
        }
        let _ = self.write(&BuildSnapshot {
            phase: Some(BuildPhase::Walking),
            items_done: entries_done,
            files_done,
            dirs_done,
            ..BuildSnapshot::default()
        });
    }

    /// Mark the start of snapshot metadata collection
    pub fn start_snapshot(&self, files_total: usize) {
        self.update_snapshot_inner(files_total, 0, true);
    }

    /// Update snapshot metadata progress
    pub fn update_snapshot(&self, files_total: usize, files_done: u64) {
        self.update_snapshot_inner(files_total, files_done, false);
    }

    /// Mark the start of a known-size file scan.
    pub fn start_scan(&self, files_total: usize) {
        self.update_scan_inner(files_total, 0, 0, 0, true);
    }

    /// Update file-scan progress, rate-limited by scanned file count.
    pub fn update_scan(
        &self,
        files_total: usize,
        files_done: u64,
        bytes_done: u64,
        runs_done: u64,
    ) {
        self.update_scan_inner(files_total, files_done, bytes_done, runs_done, false);
    }

    /// Mark the start of posting-list merge progress
    pub fn start_postings(&self, runs_total: usize, pairs_total: u64) {
        self.update_postings_inner(runs_total, 0, pairs_total, 0, true);
    }

    /// Update posting-list merge progress
    pub fn update_postings(
        &self,
        runs_total: usize,
        runs_done: u64,
        pairs_total: u64,
        pairs_done: u64,
    ) {
        self.update_postings_inner(runs_total, runs_done, pairs_total, pairs_done, false);
    }

    fn update_snapshot_inner(&self, files_total: usize, files_done: u64, force: bool) {
        if !force && !self.should_update(ProgressKind::Snapshot, files_done, SNAPSHOT_UPDATE_FILES)
        {
            return;
        }
        let _ = self.write(&BuildSnapshot {
            phase: Some(BuildPhase::Snapshot),
            files_total: files_total as u64,
            files_done,
            ..BuildSnapshot::default()
        });
    }

    fn update_scan_inner(
        &self,
        files_total: usize,
        files_done: u64,
        bytes_done: u64,
        runs_done: u64,
        force: bool,
    ) {
        if !force
            && !self.should_update(ProgressKind::Scan, files_done, SCAN_UPDATE_FILES)
            && files_done < files_total as u64
        {
            return;
        }
        let _ = self.write(&BuildSnapshot {
            phase: Some(BuildPhase::Scanning),
            files_total: files_total as u64,
            files_done,
            bytes_done,
            runs_done,
            ..BuildSnapshot::default()
        });
    }

    fn update_postings_inner(
        &self,
        runs_total: usize,
        runs_done: u64,
        pairs_total: u64,
        pairs_done: u64,
        force: bool,
    ) {
        if !force && !self.should_update(ProgressKind::Postings, pairs_done, POSTINGS_UPDATE_PAIRS)
        {
            return;
        }
        let _ = self.write(&BuildSnapshot {
            phase: Some(BuildPhase::WritingPostings),
            items_total: pairs_total,
            items_done: pairs_done,
            runs_total: runs_total as u64,
            runs_done,
            ..BuildSnapshot::default()
        });
    }

    fn should_update(&self, kind: ProgressKind, value: u64, step: u64) -> bool {
        let Ok(mut cursor) = self.last_update.lock() else {
            return false;
        };
        if cursor.kind != kind {
            *cursor = BuildProgressCursor { kind, value };
            return true;
        }
        if value.saturating_sub(cursor.value) < step {
            return false;
        }
        cursor.value = value;
        true
    }

    fn reset_update_cursor(&self) {
        if let Ok(mut cursor) = self.last_update.lock() {
            *cursor = BuildProgressCursor::default();
        }
    }

    fn write(&self, snapshot: &BuildSnapshot) -> io::Result<()> {
        let Some(parent) = self.path.parent() else {
            return Ok(());
        };
        fs::create_dir_all(parent)?;
        let tmp = self
            .path
            .with_extension(format!("tmp-{}", std::process::id()));
        let mut file = fs::File::create(&tmp)?;
        serde_json::to_writer(&mut file, snapshot).map_err(io::Error::other)?;
        file.write_all(b"\n")?;
        fs::rename(tmp, &self.path)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum ProgressKind {
    #[default]
    None,
    Walk,
    Snapshot,
    Scan,
    Postings,
}

#[derive(Clone, Copy, Debug, Default)]
struct BuildProgressCursor {
    kind: ProgressKind,
    value: u64,
}

/// Foreground progress renderer for cold waits.
pub struct BuildProgressRenderer {
    bar: ProgressBar,
    last_poll: Instant,
    started: Instant,
    enabled: bool,
    spinner: bool,
    shown: bool,
}

impl BuildProgressRenderer {
    /// Create a terminal renderer: phase progress, or one steady spinner
    pub fn new(enabled: bool, spinner: bool) -> Self {
        Self {
            bar: ProgressBar::hidden(),
            last_poll: Instant::now() - PROGRESS_POLL,
            started: Instant::now(),
            enabled: enabled && io::stderr().is_terminal(),
            spinner,
            shown: false,
        }
    }

    /// Redraw from the persisted daemon progress state.
    pub fn tick(&mut self, state_root: &Path) {
        if !self.enabled {
            return;
        }
        if !self.shown {
            if self.started.elapsed() < SHOW_AFTER {
                return;
            }
            self.reveal();
        }
        if self.spinner || self.last_poll.elapsed() < PROGRESS_POLL {
            return;
        }
        self.last_poll = Instant::now();
        match read(state_root) {
            Ok(Some(snapshot)) if snapshot.is_building() => self.draw(&snapshot),
            _ => self.draw_waiting(),
        }
    }

    /// Put a live bar on stderr once the wait has earned one
    fn reveal(&mut self) {
        self.bar = new_bar(self.spinner);
        self.shown = true;
        if !self.spinner {
            self.announce_cold_build();
        }
    }

    /// Clear the terminal progress line.
    pub fn finish(self) {
        if self.shown {
            self.bar.finish_and_clear();
        }
    }

    /// Say once why the query is waiting, above the live bar
    fn announce_cold_build(&self) {
        if crate::messages::messages() {
            self.bar.println(format!("eg: {COLD_BUILD_NOTE}"));
        }
    }

    /// Show the phase-only line used before the daemon reports progress
    fn draw_waiting(&self) {
        self.bar.set_message(BUILDING);
        self.bar.set_style(phase_style());
        self.bar.set_length(0);
        self.bar.set_position(0);
    }

    fn draw(&self, snapshot: &BuildSnapshot) {
        self.bar.set_message(snapshot.message());
        match snapshot.bar_bounds() {
            Some((total, done)) => {
                self.bar.set_style(progress_style());
                self.bar.set_length(total);
                self.bar.set_position(done.min(total));
            },
            None => {
                self.bar.set_style(phase_style());
                self.bar.set_length(0);
                self.bar.set_position(0);
            },
        }
    }
}

/// Read the latest daemon progress snapshot.
pub fn read(state_root: &Path) -> io::Result<Option<BuildSnapshot>> {
    let file = match fs::File::open(progress_path(state_root)) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    serde_json::from_reader(file)
        .map(Some)
        .map_err(io::Error::other)
}

fn progress_path(state_root: &Path) -> PathBuf {
    state_root.join(RUNTIME_DIR_NAME).join(PROGRESS_FILE_NAME)
}

fn new_bar(spinner: bool) -> ProgressBar {
    let bar = ProgressBar::new(0);
    bar.set_draw_target(ProgressDrawTarget::stderr());
    if spinner {
        bar.set_style(spinner_style());
        bar.set_message("indexing changes");
    } else {
        bar.set_style(phase_style());
        bar.set_message(BUILDING);
    }
    bar.enable_steady_tick(Duration::from_millis(100));
    bar
}

fn progress_style() -> ProgressStyle {
    ProgressStyle::with_template("{spinner:.green} {msg} [{bar:20.cyan/blue}] {pos}/{len}")
        .unwrap_or_else(|_| ProgressStyle::default_bar())
        .progress_chars("=> ")
}

fn spinner_style() -> ProgressStyle {
    ProgressStyle::with_template("{spinner:.green} {msg} ({elapsed})")
        .unwrap_or_else(|_| ProgressStyle::default_spinner())
}

fn phase_style() -> ProgressStyle {
    ProgressStyle::with_template("{spinner:.green} {msg}")
        .unwrap_or_else(|_| ProgressStyle::default_spinner())
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use indicatif::ProgressBar;

    use super::{
        BuildPhase, BuildProgress, BuildProgressRenderer, PROGRESS_POLL, SHOW_AFTER, read,
    };

    fn renderer(waited: Duration) -> BuildProgressRenderer {
        BuildProgressRenderer {
            bar: ProgressBar::hidden(),
            last_poll: Instant::now() - PROGRESS_POLL,
            started: Instant::now() - waited,
            enabled: true,
            spinner: true,
            shown: false,
        }
    }

    #[test]
    fn a_short_wait_paints_nothing() {
        let root_guard = scratch("short");
        let mut renderer = renderer(Duration::ZERO);

        renderer.tick(root_guard.path());

        assert!(!renderer.shown, "a wait under the threshold drew a bar");
    }

    #[test]
    fn a_long_wait_earns_a_bar() {
        let root_guard = scratch("long");
        let mut renderer = renderer(SHOW_AFTER + Duration::from_millis(10));

        renderer.tick(root_guard.path());

        assert!(renderer.shown, "a wait over the threshold drew nothing");
    }

    fn scratch(name: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("eg-progress-{name}-"))
            .tempdir()
            .expect("scratch dir")
    }

    #[test]
    fn progress_round_trips_build_phase() {
        let root_guard = scratch("phase");
        let root = root_guard.path().to_path_buf();
        let progress = BuildProgress::new(&root);

        progress.phase(BuildPhase::Walking);

        let snapshot = read(&root).expect("read progress").expect("snapshot");
        assert_eq!(snapshot.phase.expect("phase").label(), "walking the tree");
        assert_eq!(snapshot.files_total, 0);
    }

    #[test]
    fn scan_progress_keeps_file_counts() {
        let root_guard = scratch("scan");
        let root = root_guard.path().to_path_buf();
        let progress = BuildProgress::new(&root);

        progress.start_scan(10);
        progress.update_scan(10, 10, 4096, 3);

        let snapshot = read(&root).expect("read progress").expect("snapshot");
        assert_eq!(snapshot.phase.expect("phase").label(), "scanning files");
        assert_eq!(snapshot.files_total, 10);
        assert_eq!(snapshot.files_done, 10);
        assert_eq!(snapshot.bytes_done, 4096);
        assert_eq!(snapshot.runs_done, 3);
    }

    #[test]
    fn walk_progress_keeps_entry_file_and_dir_counts() {
        let root_guard = scratch("walk");
        let root = root_guard.path().to_path_buf();
        let progress = BuildProgress::new(&root);

        progress.update_walk(512, 400, 100);

        let snapshot = read(&root).expect("read progress").expect("snapshot");
        assert_eq!(snapshot.phase.expect("phase").label(), "walking the tree");
        assert_eq!(snapshot.items_done, 512);
        assert_eq!(snapshot.files_done, 400);
        assert_eq!(snapshot.dirs_done, 100);
    }

    #[test]
    fn snapshot_progress_keeps_file_counts() {
        let root_guard = scratch("snapshot");
        let root = root_guard.path().to_path_buf();
        let progress = BuildProgress::new(&root);

        progress.start_snapshot(1024);
        progress.update_snapshot(1024, 512);

        let snapshot = read(&root).expect("read progress").expect("snapshot");
        assert_eq!(
            snapshot.phase.expect("phase").label(),
            "reading file metadata"
        );
        assert_eq!(snapshot.files_total, 1024);
        assert_eq!(snapshot.files_done, 512);
    }

    #[test]
    fn posting_progress_keeps_pair_and_run_counts() {
        let root_guard = scratch("postings");
        let root = root_guard.path().to_path_buf();
        let progress = BuildProgress::new(&root);

        progress.start_postings(8, 2_000_000);
        progress.update_postings(8, 4, 2_000_000, 1_000_000);

        let snapshot = read(&root).expect("read progress").expect("snapshot");
        assert_eq!(snapshot.phase.expect("phase").label(), "writing postings");
        assert_eq!(snapshot.items_total, 2_000_000);
        assert_eq!(snapshot.items_done, 1_000_000);
        assert_eq!(snapshot.runs_total, 8);
        assert_eq!(snapshot.runs_done, 4);
    }

    #[test]
    fn posting_progress_message_stays_indeterminate() {
        let root_guard = scratch("postings-message");
        let root = root_guard.path().to_path_buf();
        let progress = BuildProgress::new(&root);

        progress.start_postings(198, 198);

        let snapshot = read(&root).expect("read progress").expect("snapshot");
        assert_eq!(snapshot.message(), "building index: writing postings");
    }

    #[test]
    fn ready_snapshot_from_an_earlier_build_is_not_building() {
        let root_guard = scratch("ready");
        let root = root_guard.path().to_path_buf();
        let progress = BuildProgress::new(&root);

        progress.phase(BuildPhase::Ready);

        let snapshot = read(&root).expect("read progress").expect("snapshot");
        assert!(!snapshot.is_building());
    }

    #[test]
    fn every_build_message_names_the_wait() {
        let root_guard = scratch("message");
        let root = root_guard.path().to_path_buf();
        let progress = BuildProgress::new(&root);

        progress.start_scan(10);
        progress.update_scan(10, 10, 4 * 1024 * 1024, 1);

        let snapshot = read(&root).expect("read progress").expect("snapshot");
        assert_eq!(snapshot.message(), "building index: scanning files, 4 MiB");
    }

    #[test]
    fn progress_clear_removes_stale_snapshot() {
        let root_guard = scratch("clear");
        let root = root_guard.path().to_path_buf();
        let progress = BuildProgress::new(&root);
        progress.phase(BuildPhase::Walking);

        progress.clear();

        assert!(read(&root).expect("read progress").is_none());
    }
}
