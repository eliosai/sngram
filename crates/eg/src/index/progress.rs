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
const PROGRESS_POLL: Duration = Duration::from_millis(100);
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
            Self::Walking => "walking files",
            Self::Snapshot => "building snapshot",
            Self::Scanning => "scanning files",
            Self::WritingSummary => "writing summaries",
            Self::WritingPostings => "writing postings",
            Self::WritingManifest => "writing manifest",
            Self::Publishing => "publishing index",
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
    fn message(&self) -> String {
        let phase = self.phase.map_or("building index", BuildPhase::label);
        match self.phase {
            Some(BuildPhase::Walking) => {
                return format!(
                    "{phase}: {} entries, {} files, {} dirs",
                    self.items_done, self.files_done, self.dirs_done
                );
            },
            Some(BuildPhase::Snapshot) if self.files_total > 0 => {
                return format!("{phase}: {}/{} files", self.files_done, self.files_total);
            },
            Some(BuildPhase::WritingPostings) if self.items_total > 0 => {
                return format!(
                    "{phase}: {}/{} pairs, {}/{} runs",
                    self.items_done, self.items_total, self.runs_done, self.runs_total
                );
            },
            _ => {},
        }
        if self.files_total == 0 {
            return phase.to_owned();
        }
        format!(
            "{phase}: {}/{} files, {} MiB",
            self.files_done,
            self.files_total,
            self.bytes_done / 1024 / 1024
        )
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
    enabled: bool,
}

impl BuildProgressRenderer {
    /// Create a terminal renderer when stderr can display one.
    pub fn new(enabled: bool) -> Self {
        let enabled = enabled && io::stderr().is_terminal();
        let bar = if enabled {
            let bar = ProgressBar::new(0);
            bar.set_draw_target(ProgressDrawTarget::stderr());
            bar.set_style(progress_style());
            bar.enable_steady_tick(Duration::from_millis(100));
            bar
        } else {
            ProgressBar::hidden()
        };
        Self {
            bar,
            last_poll: Instant::now() - PROGRESS_POLL,
            enabled,
        }
    }

    /// Redraw from the persisted daemon progress state.
    pub fn tick(&mut self, state_root: &Path) {
        if !self.enabled || self.last_poll.elapsed() < PROGRESS_POLL {
            return;
        }
        self.last_poll = Instant::now();
        let Ok(Some(snapshot)) = read(state_root) else {
            self.bar.set_message("building index");
            return;
        };
        if snapshot.files_total > 0 {
            self.bar.set_length(snapshot.files_total);
            self.bar
                .set_position(snapshot.files_done.min(snapshot.files_total));
        } else if snapshot.items_total > 0 {
            self.bar.set_length(snapshot.items_total);
            self.bar
                .set_position(snapshot.items_done.min(snapshot.items_total));
        } else {
            self.bar.set_length(0);
            self.bar.set_position(0);
        }
        self.bar.set_message(snapshot.message());
    }

    /// Clear the terminal progress line.
    pub fn finish(self) {
        if self.enabled {
            self.bar.finish_and_clear();
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

fn progress_style() -> ProgressStyle {
    ProgressStyle::with_template("{spinner:.green} {msg} [{bar:40.cyan/blue}] {pos}/{len}")
        .unwrap_or_else(|_| ProgressStyle::default_bar())
        .progress_chars("=> ")
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use super::{BuildPhase, BuildProgress, read};

    fn scratch(name: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("eg-progress-{}-{stamp}-{name}", std::process::id()));
        fs::create_dir_all(&root).expect("scratch dir");
        root
    }

    #[test]
    fn progress_round_trips_build_phase() {
        let root = scratch("phase");
        let progress = BuildProgress::new(&root);

        progress.phase(BuildPhase::Walking);

        let snapshot = read(&root).expect("read progress").expect("snapshot");
        assert_eq!(snapshot.phase.expect("phase").label(), "walking files");
        assert_eq!(snapshot.files_total, 0);
    }

    #[test]
    fn scan_progress_keeps_file_counts() {
        let root = scratch("scan");
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
        let root = scratch("walk");
        let progress = BuildProgress::new(&root);

        progress.update_walk(512, 400, 100);

        let snapshot = read(&root).expect("read progress").expect("snapshot");
        assert_eq!(snapshot.phase.expect("phase").label(), "walking files");
        assert_eq!(snapshot.items_done, 512);
        assert_eq!(snapshot.files_done, 400);
        assert_eq!(snapshot.dirs_done, 100);
    }

    #[test]
    fn snapshot_progress_keeps_file_counts() {
        let root = scratch("snapshot");
        let progress = BuildProgress::new(&root);

        progress.start_snapshot(1024);
        progress.update_snapshot(1024, 512);

        let snapshot = read(&root).expect("read progress").expect("snapshot");
        assert_eq!(snapshot.phase.expect("phase").label(), "building snapshot");
        assert_eq!(snapshot.files_total, 1024);
        assert_eq!(snapshot.files_done, 512);
    }

    #[test]
    fn posting_progress_keeps_pair_and_run_counts() {
        let root = scratch("postings");
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
    fn progress_clear_removes_stale_snapshot() {
        let root = scratch("clear");
        let progress = BuildProgress::new(&root);
        progress.phase(BuildPhase::Walking);

        progress.clear();

        assert!(read(&root).expect("read progress").is_none());
    }
}
