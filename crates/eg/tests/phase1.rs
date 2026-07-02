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

fn git(cwd: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .current_dir(cwd)
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
fn rebuild_index_searches_matching_files() {
    let fixture = Fixture::new();
    fs::write(fixture.path("hit.txt"), "alpha unique needle\n").unwrap();
    fs::write(fixture.path("miss.txt"), "beta only\n").unwrap();

    let output = eg(&[
        "--index=rebuild",
        "unique needle",
        fixture.root.to_str().unwrap(),
    ]);
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8(output.stderr).unwrap()
    );
    assert!(stdout.contains("hit.txt"));
    assert!(stdout.contains("alpha unique needle"));
    assert!(!stdout.contains("miss.txt"));
}

#[test]
fn default_search_builds_and_uses_index() {
    let fixture = Fixture::new();
    fs::write(fixture.path("hit.txt"), "default indexed needle\n").unwrap();

    let output = eg(&["default indexed needle", fixture.root.to_str().unwrap()]);
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8(output.stderr).unwrap()
    );
    assert!(stdout.contains("default indexed needle"));
    assert!(fixture.path(".eg/index/postings-v3/manifest.json").exists());
}

#[test]
fn default_indexed_search_supports_everything_literal() {
    let fixture = Fixture::new();
    fs::write(fixture.path("hit.txt"), "Everything initial fixture\n").unwrap();
    fs::write(fixture.path("miss.txt"), "nothing relevant\n").unwrap();

    let output = eg(&["Everything", fixture.root.to_str().unwrap()]);
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8(output.stderr).unwrap()
    );
    assert!(stdout.contains("Everything initial fixture"));
    assert!(!stdout.contains("nothing relevant"));
}

#[test]
fn auto_index_unchanged_uses_fast_freshness_snapshot() {
    let fixture = Fixture::new();
    fs::write(fixture.path("sample.txt"), "fast freshness needle\n").unwrap();

    let first = eg(&[
        "--index=rebuild",
        "fast freshness needle",
        fixture.root.to_str().unwrap(),
    ]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8(first.stderr).unwrap()
    );

    let second = eg(&[
        "--debug",
        "--index=auto",
        "fast freshness needle",
        fixture.root.to_str().unwrap(),
    ]);
    let stdout = String::from_utf8(second.stdout).unwrap();
    let stderr = String::from_utf8(second.stderr).unwrap();

    assert!(second.status.success(), "{stderr}");
    assert!(stdout.contains("fast freshness needle"));
    assert!(
        stderr.contains("eg index: loaded fast freshness snapshot"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("eg index: collected"),
        "unchanged auto search should not walk the tree: {stderr}"
    );
}

#[test]
fn auto_index_refreshes_changed_files_without_full_rebuild() {
    let fixture = Fixture::new();
    fs::write(fixture.path("sample.txt"), "alpha stable needle\n").unwrap();

    let first = eg(&[
        "--index=rebuild",
        "stable needle",
        fixture.root.to_str().unwrap(),
    ]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8(first.stderr).unwrap()
    );

    let marker = fixture.root.join(".eg/index/postings-v3/auto-marker");
    fs::write(&marker, "keep").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    fs::write(fixture.path("sample.txt"), "beta changed needle\n").unwrap();

    let changed = eg(&[
        "--debug",
        "--index=auto",
        "changed needle",
        fixture.root.to_str().unwrap(),
    ]);
    let stdout = String::from_utf8(changed.stdout).unwrap();
    let stderr = String::from_utf8(changed.stderr).unwrap();

    assert!(changed.status.success(), "{stderr}");
    assert!(stdout.contains("changed needle"));
    assert!(
        stderr.contains("eg index: loaded fast freshness snapshot"),
        "{stderr}"
    );
    assert!(
        marker.exists(),
        "auto mode should refresh changed files without deleting the index home"
    );

    let stale = eg(&[
        "--index=auto",
        "stable needle",
        fixture.root.to_str().unwrap(),
    ]);
    assert_eq!(Some(1), stale.status.code());
    assert_eq!("", String::from_utf8(stale.stdout).unwrap());
}

#[test]
fn auto_index_uses_git_freshness_for_git_worktrees() {
    let fixture = Fixture::new();
    if Command::new("git").arg("--version").output().is_err() {
        return;
    }
    assert!(git(&fixture.root, &["init", "-q"]).status.success());
    assert!(
        git(
            &fixture.root,
            &["config", "user.email", "eg@example.invalid"]
        )
        .status
        .success()
    );
    assert!(
        git(&fixture.root, &["config", "user.name", "eg"])
            .status
            .success()
    );
    fs::write(fixture.path("tracked.txt"), "git freshness tracked\n").unwrap();
    assert!(git(&fixture.root, &["add", "tracked.txt"]).status.success());
    assert!(
        git(&fixture.root, &["commit", "-qm", "initial"])
            .status
            .success()
    );
    fs::write(fixture.path("untracked.txt"), "git freshness untracked\n").unwrap();

    let first = eg(&[
        "--index=rebuild",
        "git freshness",
        fixture.root.to_str().unwrap(),
    ]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8(first.stderr).unwrap()
    );

    let clean = eg(&[
        "--debug",
        "--index=auto",
        "git freshness",
        fixture.root.to_str().unwrap(),
    ]);
    let clean_stderr = String::from_utf8(clean.stderr).unwrap();
    assert!(clean.status.success(), "{clean_stderr}");
    assert!(
        clean_stderr.contains("eg index: git freshness snapshot"),
        "{clean_stderr}"
    );
    assert!(
        !clean_stderr.contains("eg index: collected"),
        "git fast path should not walk the tree: {clean_stderr}"
    );

    fs::write(fixture.path("tracked.txt"), "git changed tracked\n").unwrap();
    let changed = eg(&[
        "--debug",
        "--index=auto",
        "git changed tracked",
        fixture.root.to_str().unwrap(),
    ]);
    let changed_stdout = String::from_utf8(changed.stdout).unwrap();
    let changed_stderr = String::from_utf8(changed.stderr).unwrap();
    assert!(changed.status.success(), "{changed_stderr}");
    assert!(changed_stdout.contains("git changed tracked"));
    assert!(
        changed_stderr.contains("eg index: git freshness snapshot"),
        "{changed_stderr}"
    );
}

#[test]
fn auto_index_drops_deleted_files_after_file_list_changes() {
    let fixture = Fixture::new();
    fs::write(fixture.path("old.txt"), "deleted indexed needle\n").unwrap();
    fs::write(fixture.path("kept.txt"), "kept indexed needle\n").unwrap();

    let first = eg(&[
        "--index=rebuild",
        "deleted indexed needle",
        fixture.root.to_str().unwrap(),
    ]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8(first.stderr).unwrap()
    );

    fs::remove_file(fixture.path("old.txt")).unwrap();
    let deleted = eg(&[
        "--index=auto",
        "deleted indexed needle",
        fixture.root.to_str().unwrap(),
    ]);

    assert_eq!(Some(1), deleted.status.code());
    assert_eq!("", String::from_utf8(deleted.stdout).unwrap());
}

#[test]
fn auto_index_finds_new_files_after_file_list_changes() {
    let fixture = Fixture::new();
    fs::write(fixture.path("old.txt"), "old indexed needle\n").unwrap();

    let first = eg(&[
        "--index=rebuild",
        "old indexed needle",
        fixture.root.to_str().unwrap(),
    ]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8(first.stderr).unwrap()
    );

    fs::write(fixture.path("new.txt"), "new indexed needle\n").unwrap();
    let second = eg(&[
        "--index=auto",
        "new indexed needle",
        fixture.root.to_str().unwrap(),
    ]);
    let stdout = String::from_utf8(second.stdout).unwrap();

    assert!(
        second.status.success(),
        "{}",
        String::from_utf8(second.stderr).unwrap()
    );
    assert!(stdout.contains("new.txt"));
    assert!(stdout.contains("new indexed needle"));
}

#[test]
fn indexed_empty_files_do_not_fail_mmap_indexing() {
    let fixture = Fixture::new();
    fs::write(fixture.path("empty.txt"), "").unwrap();
    fs::write(fixture.path("hit.txt"), "empty fixture needle\n").unwrap();

    let output = eg(&[
        "--index=rebuild",
        "empty fixture needle",
        fixture.root.to_str().unwrap(),
    ]);
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8(output.stderr).unwrap()
    );
    assert!(stdout.contains("empty fixture needle"));
}

#[test]
fn indexed_full_corpus_output_modes_error_without_scan_fallback() {
    let fixture = Fixture::new();
    fs::write(fixture.path("sample.txt"), "alpha constrained needle\n").unwrap();
    let root = fixture.root.to_str().unwrap();

    for args in [
        vec!["--index=auto", "--passthru", "constrained needle", root],
        vec!["--index=auto", "--json", "constrained needle", root],
        vec![
            "--index=auto",
            "--count",
            "--include-zero",
            "constrained needle",
            root,
        ],
    ] {
        let output = eg(&args);
        let stderr = String::from_utf8(output.stderr).unwrap();

        assert_eq!(Some(2), output.status.code(), "{args:?}");
        assert!(
            stderr.contains("indexed search does not support"),
            "{stderr}"
        );
        assert!(stderr.contains("use --no-index"), "{stderr}");
    }
}

#[test]
fn indexed_transformed_input_modes_error_without_scan_fallback() {
    let fixture = Fixture::new();
    fs::write(fixture.path("sample.txt"), "alpha constrained needle\n").unwrap();
    let root = fixture.root.to_str().unwrap();

    for args in [
        vec!["--index=auto", "--engine=auto", "constrained needle", root],
        vec!["--index=auto", "--engine=pcre2", "constrained needle", root],
        vec![
            "--index=auto",
            "--encoding=utf-16",
            "constrained needle",
            root,
        ],
        vec!["--index=auto", "--pre=cat", "constrained needle", root],
        vec!["--index=auto", "-z", "constrained needle", root],
    ] {
        let output = eg(&args);
        let stderr = String::from_utf8(output.stderr).unwrap();

        assert_eq!(Some(2), output.status.code(), "{args:?}");
        assert!(
            stderr.contains("indexed search does not support"),
            "{stderr}"
        );
        assert!(stderr.contains("use --no-index"), "{stderr}");
    }
}

#[test]
fn non_search_modes_ignore_the_index() {
    // --files and --type-list never search content; the default index mode
    // must not reject them.
    let fixture = Fixture::new();
    fs::write(fixture.path("sample.txt"), "alpha\n").unwrap();
    let root = fixture.root.to_str().unwrap();

    let output = eg(&["--files", root]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(Some(0), output.status.code());
    assert!(stdout.contains("sample.txt"), "{stdout}");

    let output = eg(&["--type-list"]);
    assert_eq!(Some(0), output.status.code());
}

#[test]
fn indexed_no_unicode_matches_through_the_index() {
    // The planner parses with the verifier's unicode mode, so --no-unicode
    // is supported rather than banned.
    let fixture = Fixture::new();
    fs::write(fixture.path("sample.txt"), "alpha constrained needle\n").unwrap();
    let root = fixture.root.to_str().unwrap();

    let output = eg(&["--index=auto", "--no-unicode", "constrained needle", root]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(Some(0), output.status.code());
    assert!(stdout.contains("constrained needle"), "{stdout}");
}

#[test]
fn indexed_no_mmap_search_uses_read_path() {
    let fixture = Fixture::new();
    fs::write(fixture.path("sample.txt"), "read path indexed needle\n").unwrap();

    let output = eg(&[
        "--index=rebuild",
        "--no-mmap",
        "read path indexed needle",
        fixture.root.to_str().unwrap(),
    ]);
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8(output.stderr).unwrap()
    );
    assert!(stdout.contains("read path indexed needle"));
}

#[test]
fn indexed_broad_regex_errors_without_scan_fallback() {
    let fixture = Fixture::new();
    fs::write(fixture.path("sample.txt"), "alpha\n").unwrap();

    let output = eg(&["--index=auto", ".", fixture.root.to_str().unwrap()]);
    let stderr = String::from_utf8(output.stderr).unwrap();

    assert_eq!(Some(2), output.status.code());
    assert!(stderr.contains("indexed query has no sparse n-gram constraints"));
    assert!(stderr.contains("use --no-index"));
}

#[test]
fn indexed_sparse_regex_uses_query_plan() {
    let fixture = Fixture::new();
    fs::write(fixture.path("hit.txt"), "alpha sparse-regex needle\n").unwrap();
    fs::write(fixture.path("miss.txt"), "alpha sparseXregex needle\n").unwrap();

    let output = eg(&[
        "--index=rebuild",
        "sparse[-_]regex needle",
        fixture.root.to_str().unwrap(),
    ]);
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8(output.stderr).unwrap()
    );
    assert!(stdout.contains("hit.txt"));
    assert!(stdout.contains("alpha sparse-regex needle"));
    assert!(!stdout.contains("miss.txt"));
}

#[test]
fn indexed_case_insensitive_search_has_no_case_false_negative() {
    let fixture = Fixture::new();
    fs::write(fixture.path("sample.txt"), "Mixed Case Sparse Needle\n").unwrap();

    let output = eg(&[
        "--index=rebuild",
        "-i",
        "mixed case sparse needle",
        fixture.root.to_str().unwrap(),
    ]);
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8(output.stderr).unwrap()
    );
    assert!(stdout.contains("Mixed Case Sparse Needle"));
}

#[test]
fn indexed_smart_case_lowercase_pattern_is_case_insensitive() {
    let fixture = Fixture::new();
    fs::write(fixture.path("sample.txt"), "Smart Case Sparse Needle\n").unwrap();

    let output = eg(&[
        "--index=rebuild",
        "-S",
        "smart case sparse needle",
        fixture.root.to_str().unwrap(),
    ]);
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8(output.stderr).unwrap()
    );
    assert!(stdout.contains("Smart Case Sparse Needle"));
}

#[test]
fn indexed_smart_case_uppercase_pattern_stays_case_sensitive() {
    let fixture = Fixture::new();
    fs::write(fixture.path("sample.txt"), "smart case sparse needle\n").unwrap();

    let output = eg(&[
        "--index=rebuild",
        "-S",
        "Smart Case Sparse Needle",
        fixture.root.to_str().unwrap(),
    ]);

    assert_eq!(Some(1), output.status.code());
    assert_eq!("", String::from_utf8(output.stdout).unwrap());
}

#[test]
fn indexed_inline_case_insensitive_regex_has_no_case_false_negative() {
    let fixture = Fixture::new();
    fs::write(fixture.path("sample.txt"), "Inline Case Sparse Needle\n").unwrap();

    let output = eg(&[
        "--index=rebuild",
        "(?i:inline case sparse needle)",
        fixture.root.to_str().unwrap(),
    ]);
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8(output.stderr).unwrap()
    );
    assert!(stdout.contains("Inline Case Sparse Needle"));
}

#[test]
fn indexed_multiple_patterns_match_any_pattern() {
    let fixture = Fixture::new();
    fs::write(fixture.path("alpha.txt"), "alpha branch\n").unwrap();
    fs::write(fixture.path("beta.txt"), "beta branch\n").unwrap();
    fs::write(fixture.path("gamma.txt"), "gamma branch\n").unwrap();

    let output = eg(&[
        "--index=rebuild",
        "-e",
        "alpha branch",
        "-e",
        "beta branch",
        fixture.root.to_str().unwrap(),
    ]);
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8(output.stderr).unwrap()
    );
    assert!(stdout.contains("alpha.txt"));
    assert!(stdout.contains("beta.txt"));
    assert!(!stdout.contains("gamma.txt"));
}

#[test]
fn indexed_utf16_bom_file_is_forced_into_exact_verification() {
    let fixture = Fixture::new();
    let mut bytes = vec![0xFF, 0xFE];
    for unit in "unicode sparse needle\n".encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    fs::write(fixture.path("utf16.txt"), bytes).unwrap();

    let output = eg(&[
        "--index=rebuild",
        "unicode sparse needle",
        fixture.root.to_str().unwrap(),
    ]);
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8(output.stderr).unwrap()
    );
    assert!(stdout.contains("unicode sparse needle"));
}

#[test]
fn indexed_fixed_string_treats_regex_syntax_literally() {
    let fixture = Fixture::new();
    fs::write(fixture.path("literal.txt"), "call a.b[1] exactly\n").unwrap();
    fs::write(fixture.path("regexish.txt"), "call axb1 as regex bait\n").unwrap();

    let output = eg(&[
        "--index=rebuild",
        "-F",
        "a.b[1]",
        fixture.root.to_str().unwrap(),
    ]);
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8(output.stderr).unwrap()
    );
    assert!(stdout.contains("literal.txt"));
    assert!(stdout.contains("a.b[1]"));
    assert!(!stdout.contains("regexish.txt"));
}

#[test]
fn indexed_mode_honors_impossible_match_settings() {
    let output = eg(&["--index=auto", "-m0", "alpha"]);

    assert_eq!(Some(1), output.status.code());
    assert_eq!("", String::from_utf8(output.stderr).unwrap());
    assert_eq!("", String::from_utf8(output.stdout).unwrap());
}

#[test]
fn index_backend_enables_indexed_mode() {
    let fixture = Fixture::new();
    fs::write(fixture.path("sample.txt"), "alpha via ram\n").unwrap();

    let output = eg(&[
        "--index-backend=tantivy-ram",
        "alpha via ram",
        fixture.root.to_str().unwrap(),
    ]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8(output.stderr).unwrap()
    );
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("alpha via ram")
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
