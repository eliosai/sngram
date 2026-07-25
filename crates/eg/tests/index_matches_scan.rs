//! The indexed path must report every match the scan path reports.
//!
//! Each case runs the same query twice over one fixture tree, once through the
//! sparse index and once through `--no-index`, and requires identical output.
#![allow(missing_docs, clippy::unwrap_used)]

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicUsize, Ordering},
};

static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

/// Filler files that keep a fixture query below the selectivity ceiling
const FILLER_FILES: usize = 48;

struct Fixture {
    root: PathBuf,
    runtime: PathBuf,
}

impl Fixture {
    /// A tree of text shapes the searcher treats specially
    fn tricky() -> Self {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let base = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("eg-matches-scan-{}-{id}", std::process::id()));
        let fixture = Self {
            root: base.join("corpus"),
            runtime: base.join("runtime"),
        };
        fs::create_dir_all(fixture.root.join("sub")).unwrap();
        fs::create_dir_all(&fixture.runtime).unwrap();
        fixture.write("plain.txt", b"alpha beta\ngamma delta\nalpha end\n");
        fixture.write("crlf.txt", b"alpha beta\r\ngamma delta\r\nalpha end\r\n");
        fixture.write("late.txt", b"zero\none\ntwo\nthree\nalpha beta\nfive\n");
        fixture.write("sub/nested.txt", b"one\ntwo\nthree\nfour\nalpha beta\n");
        fixture.write(
            "bom_zeta.txt",
            b"\xef\xbb\xbfzeta head\nmiddle\ntail zeta\n",
        );
        fixture.write("bom_crlf.txt", b"\xef\xbb\xbfgamma delta\r\nalpha end\r\n");
        for pad in 0..FILLER_FILES {
            let body = format!("padding corpus file {pad} keeps selectivity sane\n");
            fixture.write(&format!("pad{pad}.txt"), body.as_bytes());
        }
        fixture
    }

    fn write(&self, name: &str, bytes: &[u8]) {
        fs::write(self.root.join(name), bytes).unwrap();
    }

    fn root(&self) -> &str {
        self.root.to_str().unwrap()
    }

    fn eg(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_eg"))
            .env("EG_INDEXD_RUNTIME_ROOT", &self.runtime)
            .env("XDG_RUNTIME_DIR", &self.runtime)
            .args(args)
            .output()
            .unwrap()
    }

    /// Sorted match lines from one query, and whether the index answered it
    fn matches(&self, indexed: bool, query: &[&str]) -> (Vec<String>, bool) {
        let mut args = vec!["--debug"];
        args.push(if indexed {
            "--index=auto"
        } else {
            "--no-index"
        });
        args.extend_from_slice(query);
        args.push(self.root());
        let output = self.eg(&args);
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(
            matches!(output.status.code(), Some(0 | 1)),
            "eg failed: {stderr}"
        );
        let mut lines: Vec<String> = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect();
        lines.sort();
        (lines, stderr.contains("candidates"))
    }

    /// Require identical output from both paths, with the index truly engaged
    fn agree(&self, query: &[&str]) -> Vec<String> {
        let (scan, _) = self.matches(false, query);
        let (indexed, used_index) = self.matches(true, query);
        assert_eq!(scan, indexed, "indexed output diverged for {query:?}");
        assert!(used_index, "index did not answer {query:?}");
        assert!(!scan.is_empty(), "fixture query matched nothing: {query:?}");
        scan
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(base) = self.root.parent() {
            let _ = fs::remove_dir_all(base);
        }
    }
}

#[test]
fn line_anchors_see_past_a_utf8_byte_order_mark() {
    let fixture = Fixture::tricky();

    let hits = fixture.agree(&["^zeta"]);

    assert!(hits.iter().any(|line| line.contains("bom_zeta.txt")));
}

#[test]
fn whole_line_matches_keep_crlf_line_ends() {
    let fixture = Fixture::tricky();

    let hits = fixture.agree(&["-x", "--crlf", "gamma delta"]);

    assert!(hits.iter().any(|line| line.contains("crlf.txt")));
}

#[test]
fn content_start_anchor_matches_every_line_start() {
    let fixture = Fixture::tricky();

    let hits = fixture.agree(&[r"\Aalpha"]);

    assert!(hits.iter().any(|line| line.contains("late.txt")));
    assert!(hits.iter().any(|line| line.contains("nested.txt")));
}

#[test]
fn content_end_anchor_matches_every_line_end() {
    let fixture = Fixture::tricky();

    let hits = fixture.agree(&[r"end\z"]);

    assert!(hits.iter().any(|line| line.contains("plain.txt")));
}

#[test]
fn multiline_content_anchors_still_bind_to_the_file() {
    let fixture = Fixture::tricky();

    let hits = fixture.agree(&["-U", r"\Azeta"]);

    assert!(hits.iter().any(|line| line.contains("bom_zeta.txt")));
    assert!(!hits.iter().any(|line| line.contains("late.txt")));
}

#[test]
fn overlapping_roots_name_each_file_once() {
    let fixture = Fixture::tricky();
    let sub = fixture.root.join("sub");
    let sub = sub.to_str().unwrap();

    let scan = fixture.eg(&["--no-index", "-l", "alpha", fixture.root(), sub]);
    let indexed = fixture.eg(&["--index=auto", "-l", "alpha", fixture.root(), sub]);
    let scan = String::from_utf8(scan.stdout).unwrap();
    let indexed = String::from_utf8(indexed.stdout).unwrap();

    let nested = |text: &str| text.lines().filter(|line| line.contains("nested")).count();
    assert_eq!(2, nested(&scan), "the walker visits an overlap twice");
    assert_eq!(1, nested(&indexed), "the index names each ordinal once");
    for line in scan.lines() {
        let name = line.rsplit('/').next().unwrap();
        assert!(
            indexed.lines().any(|hit| hit.ends_with(name)),
            "indexed output dropped {name}"
        );
    }
}
