//! What a query sees in the window where the daemon republishes a generation.
#![allow(missing_docs, clippy::unwrap_used)]

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};

const SETTLE_WAIT: Duration = Duration::from_mins(2);
const CORPUS_FILES: usize = 200;
/// Corpus large enough that a write can land after the walk but inside the build
const RACE_CORPUS_FILES: usize = 3000;
static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

struct Fixture {
    corpus: PathBuf,
    runtime: PathBuf,
    root: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "eg-publish-window-{name}-{}-{id}",
            std::process::id()
        ));
        let corpus = root.join("corpus");
        let runtime = root.join("xdg");
        fs::create_dir_all(&corpus).unwrap();
        fs::create_dir_all(&runtime).unwrap();
        Self {
            corpus,
            runtime,
            root,
        }
    }

    /// Corpus with enough files that a rebuild takes measurable time
    fn filled(name: &str, files: usize) -> Self {
        let fixture = Self::new(name);
        for index in 0..files {
            let body = if index % 20 == 0 {
                "zz needle7 zz\nfiller\n"
            } else {
                "zz needle zz\nfiller\n"
            };
            fs::write(fixture.corpus.join(format!("f{index:04}.txt")), body).unwrap();
        }
        fixture
    }

    fn eg(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_eg"))
            .env("XDG_RUNTIME_DIR", &self.runtime)
            .current_dir(&self.corpus)
            .env("EG_INDEX_DIRT_WAIT_MS", "2000")
            .args(args)
            .output()
            .unwrap()
    }

    fn state_runtime(&self) -> PathBuf {
        self.corpus.join(".eg/runtime")
    }

    /// Build the index once and let the daemon settle on a published generation
    fn settle(&self) {
        wait_until(SETTLE_WAIT, || {
            let output = self.eg(&["-l", "needle", "./"]);
            output.status.success().then_some(())
        });
        self.quiesce();
    }

    /// Wait until the freshness proof has stood still, with no rebuild in flight
    fn quiesce(&self) {
        let clean = self.state_runtime().join("journal-clean");
        wait_until(SETTLE_WAIT, || held_for(&clean, Duration::from_millis(500)));
    }

    /// Paths the currently published generation covers
    fn published_paths(&self) -> Vec<u8> {
        fs::read(self.corpus.join(".eg/index/postings-v9/paths-v3.bin")).unwrap_or_default()
    }

    /// Process id of the daemon that owns this fixture's runtime root
    fn daemon_pid(&self) -> String {
        let owner = fs::read_to_string(self.runtime.join("eg/daemon.lock")).unwrap();
        owner.trim().split('-').next().unwrap().to_owned()
    }
}

/// A daemon held stopped, so it can neither drain the watcher nor republish
struct Halted(String);

impl Halted {
    fn new(pid: &str) -> Self {
        signal(pid, "-STOP");
        Self(pid.to_owned())
    }
}

impl Drop for Halted {
    fn drop(&mut self) {
        signal(&self.0, "-CONT");
    }
}

fn signal(pid: &str, signal: &str) {
    Command::new("kill").args([signal, pid]).status().unwrap();
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for _ in 0..40 {
            if fs::remove_dir_all(&self.root).is_ok() || !self.root.exists() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// Some when the marker stayed in place for the whole span
fn held_for(marker: &Path, span: Duration) -> Option<()> {
    let started = Instant::now();
    while started.elapsed() < span {
        marker.exists().then_some(())?;
        std::thread::sleep(Duration::from_millis(10));
    }
    Some(())
}

/// Sample without sleeping, for a marker that stands for only a few milliseconds
fn spin_until<T>(timeout: Duration, mut probe: impl FnMut() -> Option<T>) -> T {
    let started = Instant::now();
    loop {
        if let Some(value) = probe() {
            return value;
        }
        assert!(started.elapsed() <= timeout, "timed out waiting");
    }
}

fn wait_until<T>(timeout: Duration, mut probe: impl FnMut() -> Option<T>) -> T {
    let started = Instant::now();
    loop {
        if let Some(value) = probe() {
            return value;
        }
        assert!(started.elapsed() <= timeout, "timed out waiting");
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn a_query_landing_on_a_republished_generation_still_answers() {
    let fixture = Fixture::filled("republish", CORPUS_FILES);
    fixture.settle();

    for round in 0..12 {
        let _ = fixture.eg(&["-l", "needle", "./"]);
        let output = fixture.eg(&["--sort", "path", "-l", "needle[0-9]", "./"]);
        let stderr = String::from_utf8(output.stderr).unwrap();
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(
            output.status.success(),
            "round {round} failed: {stderr}{stdout}"
        );
        assert!(!stderr.contains("not ready"), "round {round}: {stderr}");
        assert_eq!(
            CORPUS_FILES / 20,
            stdout.lines().count(),
            "round {round}: {stdout}"
        );
    }
}

/// The freshness proof must never stand for a generation the walk lost a race to
#[test]
fn a_generation_that_lost_a_race_to_a_write_never_carries_the_proof() {
    let fixture = Fixture::filled("mid-walk", RACE_CORPUS_FILES);
    fixture.settle();
    let clean = fixture.state_runtime().join("journal-clean");
    let progress = fixture.state_runtime().join("build-progress.json");

    fs::write(fixture.corpus.join("trigger.txt"), "zz needle zz\n").unwrap();
    wait_until(SETTLE_WAIT, || (!clean.exists()).then_some(()));
    wait_until(SETTLE_WAIT, || {
        let phase = fs::read_to_string(&progress).ok()?;
        (phase.contains("scanning") || phase.contains("writing")).then_some(())
    });
    fs::write(fixture.corpus.join("mid_walk.txt"), "zz MIDWALKNEEDLE zz\n").unwrap();

    let covered = spin_until(SETTLE_WAIT, || {
        clean.exists().then(|| fixture.published_paths())
    });

    assert!(
        covered
            .windows(b"mid_walk.txt".len())
            .any(|window| window == b"mid_walk.txt"),
        "the proof stood for a generation missing the mid-walk write"
    );
}

#[test]
fn a_file_written_while_the_index_rebuilds_is_found_by_the_next_query() {
    let fixture = Fixture::filled("mid-walk-query", RACE_CORPUS_FILES);
    fixture.settle();
    let clean = fixture.state_runtime().join("journal-clean");
    let progress = fixture.state_runtime().join("build-progress.json");

    fs::write(fixture.corpus.join("trigger.txt"), "zz needle zz\n").unwrap();
    wait_until(SETTLE_WAIT, || (!clean.exists()).then_some(()));
    wait_until(SETTLE_WAIT, || {
        let phase = fs::read_to_string(&progress).ok()?;
        (phase.contains("scanning") || phase.contains("writing")).then_some(())
    });
    fs::write(fixture.corpus.join("mid_walk.txt"), "zz MIDWALKNEEDLE zz\n").unwrap();
    wait_until(SETTLE_WAIT, || clean.exists().then_some(()));

    let output = fixture.eg(&["-l", "MIDWALKNEEDLE", "./"]);
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert!(stdout.contains("mid_walk.txt"), "missed the mid-walk write");
}

/// A standing proof is not enough; the daemon must vouch for it after the query starts
#[test]
fn a_query_the_daemon_cannot_vouch_for_scans_rather_than_trusting_the_index() {
    let fixture = Fixture::filled("vouch", CORPUS_FILES);
    fixture.settle();
    let halted = Halted::new(&fixture.daemon_pid());
    fs::write(fixture.corpus.join("halted.txt"), "zz HALTEDNEEDLE zz\n").unwrap();

    let output = fixture.eg(&["-l", "HALTEDNEEDLE", "./"]);
    drop(halted);

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("halted.txt"),
        "answered from a generation no drain had vouched for: {stdout}"
    );
}

/// Modes that report on every corpus file must name each of them once
#[test]
fn a_whole_corpus_mode_matches_the_scan_after_a_file_changes() {
    let fixture = Fixture::filled("whole-corpus", CORPUS_FILES);
    fixture.settle();
    fs::write(fixture.corpus.join("f0001.txt"), "zz needle7 zz\n").unwrap();

    for flags in [
        vec!["--sort", "path", "-L", "needle7", "./"],
        vec!["--sort", "path", "-c", "--include-zero", "needle7", "./"],
    ] {
        let indexed = fixture.eg(&flags);
        let mut scanned = flags.clone();
        scanned.insert(0, "--no-index");
        let scan = fixture.eg(&scanned);

        assert_eq!(
            String::from_utf8(scan.stdout).unwrap(),
            String::from_utf8(indexed.stdout).unwrap(),
            "{flags:?} disagreed with the exact scan"
        );
    }
}

#[test]
fn a_file_written_immediately_before_a_query_is_found() {
    let fixture = Fixture::filled("immediate", CORPUS_FILES);
    fixture.settle();

    for round in 0..8 {
        let needle = format!("IMMEDIATENEEDLE{round}");
        let name = format!("immediate{round}.txt");
        fs::write(fixture.corpus.join(&name), format!("zz {needle} zz\n")).unwrap();
        let output = fixture.eg(&["-l", &needle, "./"]);
        let stdout = String::from_utf8(output.stdout).unwrap();

        assert!(stdout.contains(&name), "round {round} missed {needle}");
    }
}
