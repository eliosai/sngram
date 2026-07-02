//! Ignored local performance smoke tests for indexed search.
//!
//! These are not the official Divan/CodSpeed benchmarks. They exist to make
//! rebuild, unchanged-auto, and small-change-auto timings easy to inspect while
//! developing the CLI indexer.

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};

static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Fixture {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("eg-index-perf-{}-{id}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        Fixture { root }
    }

    fn path(&self, path: impl AsRef<Path>) -> PathBuf {
        self.root.join(path)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn eg(args: &[&str]) -> (Output, Duration) {
    let started = Instant::now();
    let output = Command::new(env!("CARGO_BIN_EXE_eg"))
        .args(args)
        .output()
        .unwrap();
    (output, started.elapsed())
}

#[test]
#[ignore = "local perf smoke; run with `cargo test -p eg --test index_perf -- --ignored --nocapture`"]
fn auto_reuse_and_one_file_refresh_timing_smoke() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.path("pkg")).unwrap();
    for i in 0..300 {
        let mut text = String::new();
        for j in 0..48 {
            text.push_str(&format!(
                "def function_{i}_{j}(): return 'module {i} line {j} sparse grams benchmark corpus'\n"
            ));
        }
        fs::write(fixture.path(format!("pkg/module_{i:03}.py")), text).unwrap();
    }
    fs::write(
        fixture.path("pkg/hit.py"),
        "def target(): return 'initial perf needle'\n",
    )
    .unwrap();

    let root = fixture.root.to_str().unwrap();
    let (rebuild, rebuild_elapsed) = eg(&["--index=rebuild", "initial perf needle", root]);
    assert!(
        rebuild.status.success(),
        "{}",
        String::from_utf8(rebuild.stderr).unwrap()
    );

    let (reuse, reuse_elapsed) = eg(&["--index=auto", "initial perf needle", root]);
    assert!(
        reuse.status.success(),
        "{}",
        String::from_utf8(reuse.stderr).unwrap()
    );

    std::thread::sleep(Duration::from_millis(20));
    fs::write(
        fixture.path("pkg/hit.py"),
        "def target(): return 'changed perf needle'\n",
    )
    .unwrap();
    let (refresh, refresh_elapsed) = eg(&["--index=auto", "changed perf needle", root]);
    assert!(
        refresh.status.success(),
        "{}",
        String::from_utf8(refresh.stderr).unwrap()
    );

    eprintln!(
        "eg index perf smoke: files=301 rebuild={rebuild_elapsed:?} auto_unchanged={reuse_elapsed:?} auto_one_file_changed={refresh_elapsed:?}"
    );
}
