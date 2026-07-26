//! The indexed path must print the exact bytes the scan path prints.
//!
//! Each case runs one query twice over one fixture tree under `--sort path`,
//! once through the sparse index and once through `--no-index`, and requires
//! identical stdout bytes, block separators included.
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

/// Files whose line matches the digit pattern
const HIT_FILES: usize = 6;

/// Files that carry the literal but never the digit pattern
const NEAR_FILES: usize = 8;

/// Pattern that matches only the hit files and the solo file
const DIGIT: &str = "zebrafish[0-9]";

/// Pattern that matches the hit files, the near files, and the solo file
const LITERAL: &str = "zebrafish";

/// Blocks the digit pattern prints
const DIGIT_BLOCKS: usize = HIT_FILES + 1;

/// Blocks the literal pattern prints
const LITERAL_BLOCKS: usize = HIT_FILES + NEAR_FILES + 1;

struct Fixture {
    root: PathBuf,
    runtime: PathBuf,
}

impl Fixture {
    /// A tree of matching blocks, index candidates that never match, and filler
    fn blocks() -> Self {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let base = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("eg-output-bytes-{}-{id}", std::process::id()));
        let fixture = Self {
            root: base.join("corpus"),
            runtime: base.join("runtime"),
        };
        fs::create_dir_all(&fixture.root).unwrap();
        fs::create_dir_all(&fixture.runtime).unwrap();
        fixture.fill();
        fixture
    }

    fn fill(&self) {
        for n in 0..HIT_FILES {
            self.write(&format!("hit{n}.txt"), b"alpha\nzebrafish7 tail\nomega\n");
        }
        for n in 0..NEAR_FILES {
            self.write(&format!("near{n}.txt"), b"alpha\nzebrafish tail\nomega\n");
        }
        self.write("solo.txt", b"alpha\nzebrafish9 tail\nomega\n");
        for n in 0..FILLER_FILES {
            let body = format!("padding corpus file {n} keeps selectivity sane\n");
            self.write(&format!("pad{n}.txt"), body.as_bytes());
        }
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

    /// Stdout bytes of one path-sorted query, and whether the index answered it
    fn stdout(&self, indexed: bool, query: &[&str]) -> (Vec<u8>, bool) {
        let mut args = vec!["--debug", "--sort", "path"];
        args.push(if indexed {
            "--index=auto"
        } else {
            "--no-index"
        });
        args.extend_from_slice(query);
        args.push(self.root());
        let output = self.eg(&args);
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(
            matches!(output.status.code(), Some(0 | 1)),
            "eg failed: {stderr}"
        );
        (output.stdout, stderr.contains("candidates"))
    }

    /// Require byte-identical stdout from both paths, with the index engaged
    fn identical(&self, query: &[&str]) -> Vec<u8> {
        let (scan, _) = self.stdout(false, query);
        let (indexed, used_index) = self.stdout(true, query);
        assert!(used_index, "index did not answer {query:?}");
        assert_eq!(
            escape(&scan),
            escape(&indexed),
            "indexed bytes diverged for {query:?}"
        );
        scan
    }

    /// Require byte-identical stdout and at least one printed block
    fn identical_with_output(&self, query: &[&str]) -> Vec<u8> {
        let printed = self.identical(query);
        assert!(
            !printed.is_empty(),
            "fixture query printed nothing: {query:?}"
        );
        printed
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(base) = self.root.parent() {
            let _ = fs::remove_dir_all(base);
        }
    }
}

/// Readable form of raw printer output
fn escape(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).escape_debug().to_string()
}

/// Count of separator-only lines in printer output
fn separator_lines(printed: &[u8], separator: &str) -> usize {
    String::from_utf8_lossy(printed)
        .lines()
        .filter(|line| *line == separator)
        .count()
}

#[test]
fn context_blocks_carry_one_separator() {
    let fixture = Fixture::blocks();

    let printed = fixture.identical_with_output(&["-C1", DIGIT]);
    fixture.identical_with_output(&["-C2", DIGIT]);
    fixture.identical_with_output(&["-A2", DIGIT]);
    fixture.identical_with_output(&["-B2", DIGIT]);

    assert_eq!(DIGIT_BLOCKS - 1, separator_lines(&printed, "--"));
}

#[test]
fn heading_blocks_carry_one_blank_separator() {
    let fixture = Fixture::blocks();

    let printed = fixture.identical_with_output(&["--heading", DIGIT]);
    fixture.identical_with_output(&["--heading", "-C1", DIGIT]);
    fixture.identical_with_output(&["--heading", "-C2", "-n", DIGIT]);

    assert_eq!(DIGIT_BLOCKS - 1, separator_lines(&printed, ""));
}

#[test]
fn colored_blocks_carry_one_separator() {
    let fixture = Fixture::blocks();

    fixture.identical_with_output(&["--color", "always", "-C1", DIGIT]);
    fixture.identical_with_output(&["--color", "always", "--heading", "-C1", DIGIT]);
    fixture.identical_with_output(&["--color", "always", "--no-heading", "-C2", "-n", DIGIT]);
}

#[test]
fn line_and_summary_modes_match_the_scan() {
    let fixture = Fixture::blocks();

    fixture.identical_with_output(&["--no-heading", DIGIT]);
    fixture.identical_with_output(&["-n", DIGIT]);
    fixture.identical_with_output(&["-o", DIGIT]);
    fixture.identical_with_output(&["--count", DIGIT]);
    fixture.identical_with_output(&["--files-with-matches", DIGIT]);
    fixture.identical_with_output(&["--count-matches", DIGIT]);
    fixture.identical_with_output(&["--files-without-match", DIGIT]);
    fixture.identical_with_output(&["--vimgrep", DIGIT]);
    fixture.identical_with_output(&["--column", "--byte-offset", "-C1", DIGIT]);
    fixture.identical_with_output(&["--null", "-C1", DIGIT]);
    fixture.identical_with_output(&["--replace", "XX", "-C1", DIGIT]);
}

#[test]
fn every_matching_file_carries_one_separator() {
    let fixture = Fixture::blocks();

    let printed = fixture.identical_with_output(&["-C1", LITERAL]);
    fixture.identical_with_output(&["--heading", "-C1", LITERAL]);
    fixture.identical_with_output(&["--count", LITERAL]);

    assert_eq!(LITERAL_BLOCKS - 1, separator_lines(&printed, "--"));
}

#[test]
fn a_lone_matching_file_carries_no_separator() {
    let fixture = Fixture::blocks();

    let printed = fixture.identical_with_output(&["-C1", "zebrafish9"]);
    fixture.identical_with_output(&["--heading", "-C1", "zebrafish9"]);

    assert_eq!(0, separator_lines(&printed, "--"));
}

#[test]
fn an_empty_result_prints_nothing_on_either_path() {
    let fixture = Fixture::blocks();

    let printed = fixture.identical(&["-C1", "zebrafish5"]);
    fixture.identical(&["--heading", "-C1", "zebrafish5"]);

    assert!(printed.is_empty(), "expected no output, got {printed:?}");
}
