//! Phase-one integration tests for the elgrep CLI port.

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicUsize, Ordering},
};

static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Fixture {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("eg-phase1-{}-{id}", std::process::id()));
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

fn eg(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_eg"))
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn no_pattern_names_elgrep() {
    let output = eg(&[]);

    assert_eq!(Some(2), output.status.code());
    assert_eq!(
        "eg: elgrep requires at least one pattern to execute a search\n",
        String::from_utf8(output.stderr).unwrap()
    );
}

#[test]
fn help_names_elgrep_and_index_flags() {
    let output = eg(&["-h"]);
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert!(output.status.success());
    assert!(stdout.contains("elgrep (eg)"));
    assert!(stdout.contains("SPARSE N-GRAM OPTIONS:"));
    assert!(stdout.contains("--index=MODE"));
    assert!(stdout.contains("--index-backend=BACKEND"));
    assert!(stdout.contains("--no-index"));
}

#[test]
fn indexed_mode_errors_without_fallback() {
    let fixture = Fixture::new();
    fs::write(fixture.path("sample.txt"), "alpha\n").unwrap();

    let output = eg(&[
        "--index=auto",
        "alpha",
        fixture.path("sample.txt").to_str().unwrap(),
    ]);

    assert_eq!(Some(2), output.status.code());
    assert_eq!(
        "eg: indexed search is not implemented yet; use --no-index\n",
        String::from_utf8(output.stderr).unwrap()
    );
}

#[test]
fn indexed_mode_errors_even_when_matches_are_impossible() {
    let output = eg(&["--index=auto", "-m0", "alpha"]);

    assert_eq!(Some(2), output.status.code());
    assert_eq!(
        "eg: indexed search is not implemented yet; use --no-index\n",
        String::from_utf8(output.stderr).unwrap()
    );
}

#[test]
fn index_backend_enables_indexed_mode() {
    let output = eg(&["--index-backend=tantivy-ram", "alpha"]);

    assert_eq!(Some(2), output.status.code());
    assert_eq!(
        "eg: indexed search is not implemented yet; use --no-index\n",
        String::from_utf8(output.stderr).unwrap()
    );
}

#[test]
fn no_index_can_override_indexed_mode() {
    let fixture = Fixture::new();
    fs::write(fixture.path("sample.txt"), "alpha\n").unwrap();

    let output = eg(&[
        "--index=rebuild",
        "--no-index",
        "alpha",
        fixture.path("sample.txt").to_str().unwrap(),
    ]);

    assert!(output.status.success());
    assert_eq!("alpha\n", String::from_utf8(output.stdout).unwrap());
}

#[test]
fn invalid_index_choice_errors() {
    let output = eg(&["--index=maybe", "alpha"]);
    let stderr = String::from_utf8(output.stderr).unwrap();

    assert_eq!(Some(2), output.status.code());
    assert!(stderr.contains("unrecognized index mode 'maybe'"));
}

#[test]
fn no_index_search_uses_copied_ripgrep_path() {
    let fixture = Fixture::new();
    fs::write(fixture.path("sample.txt"), "alpha\nbeta\n").unwrap();

    let output = eg(&[
        "--no-index",
        "alpha",
        fixture.path("sample.txt").to_str().unwrap(),
    ]);

    assert!(output.status.success());
    assert_eq!("alpha\n", String::from_utf8(output.stdout).unwrap());
}
