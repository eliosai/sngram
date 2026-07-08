//! Integration tests for indexed and unindexed elgrep CLI behavior.
#![allow(
    missing_docs,
    clippy::too_many_lines,
    clippy::use_self,
    clippy::unwrap_used
)]

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Output},
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

static NEXT_ID: AtomicUsize = AtomicUsize::new(0);
static EG_COMMAND_LOCK: Mutex<()> = Mutex::new(());

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Fixture {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("eg-index-integration-{}-{id}", std::process::id()));
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
    let _guard = EG_COMMAND_LOCK.lock().unwrap();
    let mut command = eg_command();
    command.args(args).output().unwrap()
}

fn eg_in(args: &[&str], cwd: &Path) -> Output {
    let _guard = EG_COMMAND_LOCK.lock().unwrap();
    let mut command = eg_command();
    command.current_dir(cwd).args(args).output().unwrap()
}

fn eg_with_env(args: &[&str], envs: &[(&str, &Path)]) -> Output {
    let _guard = EG_COMMAND_LOCK.lock().unwrap();
    let mut command = eg_command();
    command.args(args);
    for &(key, value) in envs {
        command.env(key, value);
    }
    command.output().unwrap()
}

fn eg_with_env_vars(args: &[&str], envs: &[(&str, &str)]) -> Output {
    let _guard = EG_COMMAND_LOCK.lock().unwrap();
    let mut command = eg_command();
    command.args(args);
    for &(key, value) in envs {
        command.env(key, value);
    }
    command.output().unwrap()
}

fn eg_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_eg"));
    command.env("XDG_RUNTIME_DIR", isolated_runtime_dir());
    command
}

fn isolated_runtime_dir() -> PathBuf {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "eg-index-integration-runtime-{}-{id}",
        std::process::id()
    ));
    fs::create_dir_all(&root).unwrap();
    root
}

fn install_daemon_fixture(dest: &Path) {
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    let staged_daemon = dest.with_extension(format!("tmp-{}", std::process::id()));
    fs::copy(env!("CARGO_BIN_EXE_eg-indexd"), &staged_daemon).unwrap();
    let permissions = fs::metadata(env!("CARGO_BIN_EXE_eg-indexd"))
        .unwrap()
        .permissions();
    fs::set_permissions(&staged_daemon, permissions).unwrap();
    fs::File::open(&staged_daemon).unwrap().sync_all().unwrap();
    fs::rename(staged_daemon, dest).unwrap();
}

struct ChildGuard {
    child: Option<Child>,
}

impl ChildGuard {
    fn spawn_daemon(runtime_root: &Path) -> Self {
        Self::spawn_daemon_binary(Path::new(env!("CARGO_BIN_EXE_eg-indexd")), runtime_root)
    }

    fn spawn_daemon_binary(binary: &Path, runtime_root: &Path) -> Self {
        let child = spawn_daemon_process(binary, runtime_root);
        wait_until(Duration::from_secs(10), || {
            runtime_root.join("startup-ready").exists().then_some(())
        });
        Self { child: Some(child) }
    }

    fn kill_and_wait(mut self) -> ExitStatus {
        let child = self.child.as_mut().unwrap();
        let _ = child.kill();
        let status = child.wait().unwrap();
        self.child.take();
        status
    }

    fn wait_for_exit(mut self, timeout: Duration) -> ExitStatus {
        let started = Instant::now();
        loop {
            let child = self.child.as_mut().unwrap();
            if let Some(status) = child.try_wait().unwrap() {
                self.child.take();
                return status;
            }
            assert!(
                started.elapsed() <= timeout,
                "timed out waiting for daemon exit"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn spawn_daemon_process(binary: &Path, runtime_root: &Path) -> Child {
    wait_until(Duration::from_secs(2), || {
        match Command::new(binary)
            .arg("--runtime-root")
            .arg(runtime_root)
            .spawn()
        {
            Ok(child) => Some(Ok(child)),
            Err(err) if err.raw_os_error() == Some(26) => None,
            Err(err) => Some(Err(err)),
        }
    })
    .unwrap()
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn wait_until<T>(timeout: Duration, mut f: impl FnMut() -> Option<T>) -> T {
    let started = Instant::now();
    loop {
        if let Some(value) = f() {
            return value;
        }
        assert!(
            started.elapsed() <= timeout,
            "timed out waiting for condition"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn assert_bench_schema(report: &serde_json::Value) {
    for field in [
        "ok",
        "matched",
        "mode",
        "backend",
        "search_roots",
        "index_root",
        "generation_id",
        "used_parent_index",
        "cold_build",
        "timings_ms",
        "counts",
        "false_positives",
        "bytes",
        "generation_source",
        "freshness_proof",
        "selectivity_rejected",
        "query_too_broad",
        "unsupported_reason",
    ] {
        assert!(report.get(field).is_some(), "missing bench field {field}");
    }
    for field in [
        "request_validate",
        "parse_request",
        "plan_query",
        "resolve_root",
        "catalog_probe",
        "daemon_register",
        "daemon_start",
        "cold_build_total",
        "daemon_ready",
        "daemon_proof",
        "manifest_open",
        "walk_collect",
        "snapshot_build",
        "scan_documents",
        "write_postings",
        "write_summary",
        "write_manifest",
        "publish_generation",
        "index_open",
        "index_tune",
        "index_execute",
        "index_lookup",
        "candidate_restrict",
        "verify_haystacks",
        "total",
    ] {
        assert!(
            report["timings_ms"].get(field).is_some(),
            "missing timing {field}"
        );
    }
    for field in [
        "total_manifest_files",
        "text_files",
        "binary_skipped_files",
        "query_grams",
        "tuned_query_grams",
        "candidate_files",
        "verified_files",
        "matched_files",
        "forced_candidate_files",
        "parent_restricted_candidates",
    ] {
        assert!(
            report["counts"].get(field).is_some(),
            "missing count {field}"
        );
    }
    for field in [
        "candidate_files",
        "matched_files",
        "false_positive_files",
        "false_positive_pct",
        "candidate_pct_of_text_files",
    ] {
        assert!(
            report["false_positives"].get(field).is_some(),
            "missing false positive field {field}"
        );
    }
    for field in [
        "index_table_bytes",
        "index_postings_bytes",
        "summary_bytes",
        "manifest_bytes",
        "mmap_bytes",
        "bytes_verified",
    ] {
        assert!(
            report["bytes"].get(field).is_some(),
            "missing bytes {field}"
        );
    }
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
    assert!(stdout.contains("--bench"));
    assert!(!stdout.contains("--bench-suite"));
    assert!(stdout.contains("--no-index"));
}

#[test]
fn auto_index_searches_matching_files() {
    let fixture = Fixture::new();
    fs::write(fixture.path("hit.txt"), "alpha unique needle\n").unwrap();
    fs::write(fixture.path("miss.txt"), "beta only\n").unwrap();

    let output = eg(&[
        "--index=auto",
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
    assert!(fixture.path(".eg/index/postings-v9/manifest.bin").exists());
}

#[test]
fn index_bench_emits_structured_json_without_match_output() {
    let fixture = Fixture::new();
    fs::write(fixture.path("hit.txt"), "bench unique needle\n").unwrap();
    fs::write(fixture.path("miss.txt"), "nothing here\n").unwrap();

    let output = eg(&[
        "--bench",
        "bench unique needle",
        fixture.root.to_str().unwrap(),
    ]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8(output.stderr).unwrap()
    );
    assert_bench_schema(&report);
    assert_eq!(Some(true), report["ok"].as_bool());
    assert_eq!(Some(true), report["matched"].as_bool());
    assert_eq!(Some("postings"), report["backend"].as_str());
    assert_eq!(Some(true), report["cold_build"].as_bool());
    assert_eq!(Some("cold_build"), report["generation_source"].as_str());
    assert!(report["timings_ms"]["total"].as_f64().unwrap() >= 0.0);
    assert!(report["counts"]["candidate_files"].as_u64().unwrap() >= 1);
    assert!(report["false_positives"]["false_positive_files"].is_u64());
    assert!(!stdout.contains("bench unique needle\n"));
}

#[test]
fn index_bench_emits_suite_table_and_summary() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.path("crates/eg/src/index")).unwrap();
    fs::write(
        fixture.path("crates/eg/src/index/mod.rs"),
        r#"
use std::collections::HashMap;
#[derive(Debug, Clone)]
pub mod bench_data;
fn main() {
    const MAX_FILE_SIZE: usize = 4096;
    let path = "crates/eg/src/index/mod.rs";
    let timeout = 250ms;
    let color = 0xDEADBEEF;
    // TODO: keep index fast
    println!("{path} {color} {timeout}");
}
fn helper_result() -> Result<(), Error> { Ok(()) }
"#,
    )
    .unwrap();
    fs::write(fixture.path("hit.txt"), "foo123bar\n").unwrap();
    fs::write(fixture.path("false-positive.txt"), "foo\nbar\n").unwrap();

    let output = eg_in(&["--bench"], &fixture.root);
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8(output.stderr).unwrap()
    );
    assert!(stdout.contains("idx_wall"));
    assert!(stdout.contains("scan_wall"));
    assert!(stdout.contains("rg_wall"));
    assert!(stdout.contains("lit_rare"));
    assert!(stdout.contains("summary regexes="));
    assert!(stdout.contains("warm_wall_ms="));
    assert!(stdout.contains("speedup_scan="));
    assert!(stdout.contains("speedup_rg="));
    assert!(stdout.contains("false_positive_pct="));
    assert!(!stdout.trim_start().starts_with('{'));
}

#[test]
fn index_bench_reports_cold_daemon_build_source() {
    let fixture = Fixture::new();
    fs::write(fixture.path("hit.txt"), "bench cold build needle\n").unwrap();

    let output = eg(&[
        "--bench",
        "--index=auto",
        "bench cold build needle",
        fixture.root.to_str().unwrap(),
    ]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8(output.stderr).unwrap()
    );
    assert_bench_schema(&report);
    assert_eq!(Some(true), report["cold_build"].as_bool());
    assert_eq!(Some("cold_build"), report["generation_source"].as_str());
    assert_eq!(Some(true), report["matched"].as_bool());
    for field in [
        "walk_collect",
        "snapshot_build",
        "scan_documents",
        "write_postings",
        "write_summary",
        "write_manifest",
        "publish_generation",
    ] {
        assert!(
            report["timings_ms"][field]
                .as_f64()
                .is_some_and(|ms| ms > 0.0),
            "expected cold build timing for {field}: {}",
            report["timings_ms"][field]
        );
    }
}

#[test]
fn index_bench_reports_hot_source_after_daemon_refresh() {
    let fixture = Fixture::new();
    let runtime_fixture = Fixture::new();
    fs::write(fixture.path("sample.txt"), "bench daemon old needle\n").unwrap();
    let runtime_parent = runtime_fixture.path("xdg");
    let runtime_parent_str = runtime_parent.to_str().unwrap();
    let root_str = fixture.root.to_str().unwrap();
    let envs = [("XDG_RUNTIME_DIR", runtime_parent_str)];

    let first = eg_with_env_vars(&["--index=auto", "bench daemon old", root_str], &envs);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8(first.stderr).unwrap()
    );
    let state_runtime = fixture.path(".eg/runtime");
    let initial_clean = wait_until(Duration::from_secs(10), || {
        fs::metadata(state_runtime.join("journal-clean"))
            .and_then(|meta| meta.modified())
            .ok()
    });

    std::thread::sleep(Duration::from_millis(20));
    fs::write(fixture.path("sample.txt"), "bench daemon changed needle\n").unwrap();
    wait_until(Duration::from_secs(10), || {
        let modified = fs::metadata(state_runtime.join("journal-clean"))
            .and_then(|meta| meta.modified())
            .ok()?;
        (modified > initial_clean).then_some(modified)
    });

    let output = eg_with_env_vars(
        &["--bench", "--index=auto", "bench daemon changed", root_str],
        &envs,
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8(output.stderr).unwrap()
    );
    assert_bench_schema(&report);
    assert_eq!(Some(false), report["cold_build"].as_bool());
    assert_eq!(Some("hot"), report["generation_source"].as_str());
    assert_eq!(Some(true), report["matched"].as_bool());
}

#[test]
fn index_bench_reports_forced_candidate_count_on_rejection() {
    let fixture = Fixture::new();
    fs::write(fixture.path("hit.txt"), "rare forced candidate needle\n").unwrap();
    for i in 0..80 {
        fs::write(
            fixture.path(format!("encoded_{i:02}.txt")),
            [0xFF, 0xFE, b'x', b'y', b'z'],
        )
        .unwrap();
    }

    let output = eg(&[
        "--bench",
        "--index=auto",
        "--files-with-matches",
        "rare forced candidate needle",
        fixture.root.to_str().unwrap(),
    ]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(Some(2), output.status.code());
    assert_bench_schema(&report);
    assert_eq!(Some(false), report["ok"].as_bool());
    assert_eq!(Some(true), report["selectivity_rejected"].as_bool());
    assert_eq!(
        Some(80),
        report["counts"]["forced_candidate_files"].as_u64()
    );
}

#[test]
fn index_bench_reports_forced_candidates_after_republish() {
    let fixture = Fixture::new();
    fs::write(fixture.path("hit.txt"), "dirty forced candidate needle\n").unwrap();
    fs::write(fixture.path("encoded.txt"), [0xFF, 0xFE, b'a']).unwrap();

    let build = eg(&[
        "--index=auto",
        "dirty forced candidate",
        fixture.root.to_str().unwrap(),
    ]);
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8(build.stderr).unwrap()
    );

    std::thread::sleep(Duration::from_millis(20));
    fs::write(fixture.path("encoded.txt"), [0xFF, 0xFE, b'b']).unwrap();

    let output = eg(&[
        "--bench",
        "--index=auto",
        "dirty forced candidate",
        fixture.root.to_str().unwrap(),
    ]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8(output.stderr).unwrap()
    );
    assert_bench_schema(&report);
    assert_eq!(Some(1), report["counts"]["forced_candidate_files"].as_u64());
    assert_eq!(Some(true), report["matched"].as_bool());
}

#[test]
fn index_bench_reports_structured_errors() {
    let broad = eg(&["--bench", ".*"]);
    let broad_stdout = String::from_utf8(broad.stdout).unwrap();
    let broad_report: serde_json::Value = serde_json::from_str(&broad_stdout).unwrap();

    assert_eq!(Some(2), broad.status.code());
    assert_bench_schema(&broad_report);
    assert_eq!(Some(false), broad_report["ok"].as_bool());
    assert_eq!(Some(true), broad_report["query_too_broad"].as_bool());
    assert!(
        broad_report["unsupported_reason"]
            .as_str()
            .unwrap()
            .contains("too broad")
    );

    let no_index = eg(&["--bench", "--no-index", "needle"]);
    let no_index_stdout = String::from_utf8(no_index.stdout).unwrap();
    let no_index_report: serde_json::Value = serde_json::from_str(&no_index_stdout).unwrap();

    assert_eq!(Some(2), no_index.status.code());
    assert_bench_schema(&no_index_report);
    assert_eq!(Some(false), no_index_report["ok"].as_bool());
    assert!(
        no_index_report["unsupported_reason"]
            .as_str()
            .unwrap()
            .contains("--no-index")
    );

    let non_search = eg(&["--bench", "--files"]);
    let non_search_stdout = String::from_utf8(non_search.stdout).unwrap();
    let non_search_report: serde_json::Value = serde_json::from_str(&non_search_stdout).unwrap();

    assert_eq!(Some(2), non_search.status.code());
    assert_bench_schema(&non_search_report);
    assert_eq!(Some(false), non_search_report["ok"].as_bool());
    assert!(
        non_search_report["unsupported_reason"]
            .as_str()
            .unwrap()
            .contains("only supports search")
    );
}

#[test]
fn index_bench_reports_false_positive_counts_on_controlled_corpus() {
    let fixture = Fixture::new();
    fs::write(fixture.path("hit.txt"), "foo123bar\n").unwrap();
    fs::write(fixture.path("false-positive.txt"), "bar then foo\n").unwrap();

    let output = eg(&["--bench", r"foo.*bar", fixture.root.to_str().unwrap()]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8(output.stderr).unwrap()
    );
    assert_bench_schema(&report);
    assert_eq!(Some(2), report["counts"]["candidate_files"].as_u64());
    assert_eq!(Some(2), report["counts"]["verified_files"].as_u64());
    assert_eq!(Some(1), report["counts"]["matched_files"].as_u64());
    assert_eq!(
        Some(1),
        report["false_positives"]["false_positive_files"].as_u64()
    );
    assert_eq!(
        Some(50.0),
        report["false_positives"]["false_positive_pct"].as_f64()
    );
}

#[test]
fn parent_index_serves_child_search_root() {
    let fixture = Fixture::new();
    let runtime_fixture = Fixture::new();
    let runtime_parent = runtime_fixture.path("xdg");
    let envs = [("XDG_RUNTIME_DIR", runtime_parent.as_path())];
    fs::create_dir_all(fixture.path("src")).unwrap();
    fs::create_dir_all(fixture.path("other")).unwrap();
    fs::write(fixture.path("src/hit.txt"), "shared parent child needle\n").unwrap();
    fs::write(
        fixture.path("other/hit.txt"),
        "shared parent sibling needle\n",
    )
    .unwrap();

    let build = eg_with_env(
        &[
            "--index=auto",
            "shared parent",
            fixture.root.to_str().unwrap(),
        ],
        &envs,
    );
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8(build.stderr).unwrap()
    );

    let output = eg_with_env(
        &[
            "--bench",
            "shared parent",
            fixture.path("src").to_str().unwrap(),
        ],
        &envs,
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8(output.stderr).unwrap()
    );
    assert_eq!(Some(true), report["used_parent_index"].as_bool());
    assert_eq!(
        Some(1),
        report["counts"]["parent_restricted_candidates"].as_u64()
    );
    assert_eq!(Some(1), report["counts"]["verified_files"].as_u64());
}

#[test]
fn child_index_is_ignored_for_parent_search_root() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.path("src")).unwrap();
    fs::write(fixture.path("src/hit.txt"), "child index parent needle\n").unwrap();

    let child_build = eg(&[
        "--index=auto",
        "child index parent",
        fixture.path("src").to_str().unwrap(),
    ]);
    assert!(
        child_build.status.success(),
        "{}",
        String::from_utf8(child_build.stderr).unwrap()
    );
    assert!(fixture.path("src/.eg/index").exists());

    let output = eg(&[
        "--bench",
        "child index parent",
        fixture.root.to_str().unwrap(),
    ]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8(output.stderr).unwrap()
    );
    assert_bench_schema(&report);
    assert_eq!(Some(false), report["used_parent_index"].as_bool());
    assert_eq!(Some(true), report["cold_build"].as_bool());
    assert_eq!(Some(true), report["matched"].as_bool());
}

#[test]
fn incompatible_parent_index_identity_is_rejected() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.path("src")).unwrap();
    fs::write(fixture.path("src/hit.txt"), "identity child needle\n").unwrap();

    let parent_build = eg(&[
        "--index=auto",
        "--index-backend=tantivy",
        "identity child",
        fixture.root.to_str().unwrap(),
    ]);
    assert!(
        parent_build.status.success(),
        "{}",
        String::from_utf8(parent_build.stderr).unwrap()
    );

    let output = eg(&[
        "--bench",
        "identity child",
        fixture.path("src").to_str().unwrap(),
    ]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8(output.stderr).unwrap()
    );
    assert_bench_schema(&report);
    assert_eq!(Some(false), report["used_parent_index"].as_bool());
    assert_eq!(Some(true), report["cold_build"].as_bool());
    assert_eq!(Some(true), report["matched"].as_bool());
}

#[test]
fn daemon_ready_marker_enables_manifest_only_hot_snapshot() {
    let fixture = Fixture::new();
    fs::write(fixture.path("hit.txt"), "daemon proof needle\n").unwrap();

    let build = eg(&["daemon proof needle", fixture.root.to_str().unwrap()]);
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8(build.stderr).unwrap()
    );
    let runtime = fixture.path(".eg/runtime");
    fs::create_dir_all(&runtime).unwrap();
    fs::write(runtime.join("watcher-ready"), "ready").unwrap();
    fs::write(runtime.join("journal-clean"), "clean").unwrap();

    let output = eg(&[
        "--debug",
        "--bench",
        "daemon proof needle",
        fixture.root.to_str().unwrap(),
    ]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert!(output.status.success(), "{stderr}");
    assert_bench_schema(&report);
    assert_eq!(Some("daemon"), report["freshness_proof"].as_str());
    assert!(
        !stderr.contains("eg index: collected"),
        "daemon-proofed hot path should not walk: {stderr}"
    );
    assert_eq!(Some(true), report["matched"].as_bool());
}

#[test]
fn stale_index_without_live_daemon_is_rejected() {
    let fixture = Fixture::new();
    fs::write(fixture.path("hit.txt"), "daemon stale old needle\n").unwrap();

    let build = eg(&["daemon stale old", fixture.root.to_str().unwrap()]);
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8(build.stderr).unwrap()
    );
    assert!(fixture.path(".eg/index").exists());

    let stale = eg_with_env_vars(
        &[
            "--bench",
            "daemon stale old",
            fixture.root.to_str().unwrap(),
        ],
        &[("EG_INDEXD_DISABLE_AUTOSPAWN", "1")],
    );
    let stderr = String::from_utf8(stale.stderr).unwrap();

    assert!(!stale.status.success());
    assert!(
        stderr.contains("indexed search needs eg-indexd"),
        "{stderr}"
    );
}

#[test]
fn daemon_refreshes_changed_index_and_hot_path_uses_daemon_proof() {
    let fixture = Fixture::new();
    let runtime_fixture = Fixture::new();
    fs::write(fixture.path("hit.txt"), "daemon old needle\n").unwrap();
    let runtime_parent = runtime_fixture.path("xdg");
    let runtime_parent_str = runtime_parent.to_str().unwrap();
    let root_str = fixture.root.to_str().unwrap();
    let envs = [
        ("XDG_RUNTIME_DIR", runtime_parent_str),
        ("EG_INDEXD_DISABLE_AUTOSPAWN", "1"),
    ];

    let runtime_root = runtime_parent.join("eg");
    let daemon = ChildGuard::spawn_daemon(&runtime_root);
    let build = eg_with_env_vars(&["daemon old needle", root_str], &envs);
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8(build.stderr).unwrap()
    );

    let state_runtime = fixture.path(".eg/runtime");
    let initial_clean = wait_until(Duration::from_secs(10), || {
        fs::metadata(state_runtime.join("journal-clean"))
            .and_then(|meta| meta.modified())
            .ok()
    });

    fs::write(fixture.path("hit.txt"), "daemon new needle\n").unwrap();
    wait_until(Duration::from_secs(10), || {
        let modified = fs::metadata(state_runtime.join("journal-clean"))
            .and_then(|meta| meta.modified())
            .ok()?;
        (modified > initial_clean).then_some(modified)
    });

    let output = eg_with_env_vars(&["--bench", "daemon new needle", root_str], &envs);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    drop(daemon);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8(output.stderr).unwrap()
    );
    assert_bench_schema(&report);
    assert_eq!(Some("daemon"), report["freshness_proof"].as_str());
    assert_eq!(Some(true), report["matched"].as_bool());
}

#[test]
fn daemon_invalidates_changed_file_before_hot_search() {
    let fixture = Fixture::new();
    let runtime_fixture = Fixture::new();
    fs::write(fixture.path("hit.txt"), "old daemon stale window needle\n").unwrap();
    let runtime_parent = runtime_fixture.path("xdg");
    let runtime_parent_str = runtime_parent.to_str().unwrap();
    let root_str = fixture.root.to_str().unwrap();
    let envs = [
        ("XDG_RUNTIME_DIR", runtime_parent_str),
        ("EG_INDEXD_DISABLE_AUTOSPAWN", "1"),
    ];

    let runtime_root = runtime_parent.join("eg");
    let daemon = ChildGuard::spawn_daemon(&runtime_root);
    let build = eg_with_env_vars(&["old daemon stale window", root_str], &envs);
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8(build.stderr).unwrap()
    );
    wait_until(Duration::from_secs(10), || {
        fixture
            .path(".eg/runtime/journal-clean")
            .exists()
            .then_some(())
    });

    fs::write(fixture.path("hit.txt"), "new daemon stale window needle\n").unwrap();
    let output = eg_with_env_vars(&["--bench", "new daemon stale window", root_str], &envs);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    drop(daemon);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8(output.stderr).unwrap()
    );
    assert_bench_schema(&report);
    assert_eq!(Some(true), report["matched"].as_bool());
    assert!(report["counts"]["candidate_files"].as_u64().unwrap_or(0) > 0);
}

#[test]
fn runtime_installed_daemon_refreshes_changed_index() {
    let fixture = Fixture::new();
    let runtime_fixture = Fixture::new();
    fs::write(fixture.path("hit.txt"), "runtime copy old needle\n").unwrap();
    let runtime_parent = runtime_fixture.path("xdg");
    let runtime_parent_str = runtime_parent.to_str().unwrap();
    let root_str = fixture.root.to_str().unwrap();
    let envs = [
        ("XDG_RUNTIME_DIR", runtime_parent_str),
        ("EG_INDEXD_DISABLE_AUTOSPAWN", "1"),
    ];

    let runtime_root = runtime_parent.join("eg");
    let installed_daemon = runtime_root.join("bin/eg-indexd");
    install_daemon_fixture(&installed_daemon);
    let daemon = ChildGuard::spawn_daemon_binary(&installed_daemon, &runtime_root);
    let build = eg_with_env_vars(&["runtime copy old", root_str], &envs);
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8(build.stderr).unwrap()
    );

    let state_runtime = fixture.path(".eg/runtime");
    let initial_clean = wait_until(Duration::from_secs(10), || {
        fs::metadata(state_runtime.join("journal-clean"))
            .and_then(|meta| meta.modified())
            .ok()
    });

    fs::write(fixture.path("hit.txt"), "runtime copy new needle\n").unwrap();
    wait_until(Duration::from_secs(10), || {
        let modified = fs::metadata(state_runtime.join("journal-clean"))
            .and_then(|meta| meta.modified())
            .ok()?;
        (modified > initial_clean).then_some(modified)
    });

    let output = eg_with_env_vars(&["--bench", "runtime copy new", root_str], &envs);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    drop(daemon);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8(output.stderr).unwrap()
    );
    assert_bench_schema(&report);
    assert_eq!(Some("daemon"), report["freshness_proof"].as_str());
    assert_eq!(Some(true), report["matched"].as_bool());
}

#[test]
fn daemon_graceful_idle_exit_deletes_index_markers_and_request() {
    let fixture = Fixture::new();
    let runtime_fixture = Fixture::new();
    fs::write(fixture.path("hit.txt"), "daemon graceful needle\n").unwrap();
    let runtime_parent = runtime_fixture.path("xdg");
    let runtime_parent_str = runtime_parent.to_str().unwrap();
    let runtime_root = runtime_parent.join("eg");
    let root_str = fixture.root.to_str().unwrap();
    let envs = [
        ("XDG_RUNTIME_DIR", runtime_parent_str),
        ("EG_INDEXD_DISABLE_AUTOSPAWN", "1"),
    ];

    let daemon = ChildGuard::spawn_daemon(&runtime_root);
    let build = eg_with_env_vars(&["daemon graceful needle", root_str], &envs);
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8(build.stderr).unwrap()
    );
    assert!(fixture.path(".eg/index").exists());
    assert!(runtime_root.join("startup-ready").exists());
    assert!(has_request_file(&runtime_root));

    fs::remove_file(fixture.path(".eg/runtime/lease")).unwrap();
    let status = daemon.wait_for_exit(Duration::from_secs(10));

    assert!(status.success());
    assert!(!fixture.path(".eg/index").exists());
    assert!(!fixture.path(".eg/runtime/journal-clean").exists());
    assert!(!fixture.path(".eg/runtime/watcher-ready").exists());
    assert!(!fixture.path(".eg/runtime/daemon-owner").exists());
    assert!(!runtime_root.join("startup-ready").exists());
    assert!(!has_request_file(&runtime_root));
}

#[test]
fn killed_daemon_leaves_index_unusable_until_next_startup_deletes_it() {
    let fixture = Fixture::new();
    let runtime_fixture = Fixture::new();
    fs::write(fixture.path("hit.txt"), "daemon crash needle\n").unwrap();
    let runtime_parent = runtime_fixture.path("xdg");
    let runtime_parent_str = runtime_parent.to_str().unwrap();
    let runtime_root = runtime_parent.join("eg");
    let root_str = fixture.root.to_str().unwrap();
    let envs = [
        ("XDG_RUNTIME_DIR", runtime_parent_str),
        ("EG_INDEXD_DISABLE_AUTOSPAWN", "1"),
    ];

    let daemon = ChildGuard::spawn_daemon(&runtime_root);
    let build = eg_with_env_vars(&["daemon crash needle", root_str], &envs);
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8(build.stderr).unwrap()
    );
    assert!(fixture.path(".eg/index").exists());
    assert!(has_request_file(&runtime_root));

    let _ = daemon.kill_and_wait();
    let stale = eg_with_env_vars(&["--bench", "daemon crash needle", root_str], &envs);
    let stderr = String::from_utf8(stale.stderr).unwrap();
    assert!(!stale.status.success());
    assert!(
        stderr.contains("indexed search needs eg-indexd"),
        "{stderr}"
    );
    assert!(fixture.path(".eg/index").exists());

    let _ = fs::remove_file(fixture.path(".eg/runtime/lease"));
    let mut restart =
        spawn_daemon_process(Path::new(env!("CARGO_BIN_EXE_eg-indexd")), &runtime_root);
    let status = wait_until(Duration::from_secs(10), || restart.try_wait().unwrap());

    assert!(status.success());
    assert!(!fixture.path(".eg/index").exists());
    assert!(!fixture.path(".eg/runtime/journal-clean").exists());
    assert!(!fixture.path(".eg/runtime/watcher-ready").exists());
    assert!(!fixture.path(".eg/runtime/daemon-owner").exists());
    assert!(!runtime_root.join("startup-ready").exists());
    assert!(!has_request_file(&runtime_root));
}

fn has_request_file(runtime_root: &Path) -> bool {
    fs::read_dir(runtime_root.join("requests"))
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .any(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("request"))
}

#[test]
fn daemon_consolidates_child_index_after_parent_build() {
    let fixture = Fixture::new();
    let runtime_fixture = Fixture::new();
    fs::create_dir_all(fixture.path("src")).unwrap();
    fs::create_dir_all(fixture.path("other")).unwrap();
    fs::write(fixture.path("src/hit.txt"), "daemon merge child needle\n").unwrap();
    fs::write(
        fixture.path("other/hit.txt"),
        "daemon merge child sibling needle\n",
    )
    .unwrap();
    let runtime_parent = runtime_fixture.path("xdg");
    let runtime_parent_str = runtime_parent.to_str().unwrap();
    let root_str = fixture.root.to_str().unwrap();
    let src = fixture.path("src");
    let src_str = src.to_str().unwrap();
    let envs = [
        ("XDG_RUNTIME_DIR", runtime_parent_str),
        ("EG_INDEXD_DISABLE_AUTOSPAWN", "1"),
    ];

    let runtime_root = runtime_parent.join("eg");
    let daemon = ChildGuard::spawn_daemon(&runtime_root);
    let child = eg_with_env_vars(&["--index=auto", "daemon merge child", src_str], &envs);
    assert!(
        child.status.success(),
        "{}",
        String::from_utf8(child.stderr).unwrap()
    );
    assert!(fixture.path("src/.eg/index").exists());

    let parent = eg_with_env_vars(&["--index=auto", "daemon merge child", root_str], &envs);
    assert!(
        parent.status.success(),
        "{}",
        String::from_utf8(parent.stderr).unwrap()
    );
    assert!(fixture.path(".eg/index").exists());
    let _ = fs::remove_file(fixture.path("src/.eg/runtime/lease"));

    wait_until(Duration::from_secs(10), || {
        (!fixture.path("src/.eg/index").exists()).then_some(())
    });

    let output = eg_with_env_vars(&["--bench", "daemon merge child", src_str], &envs);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8(output.stderr).unwrap()
    );
    assert_bench_schema(&report);
    assert_eq!(Some(true), report["used_parent_index"].as_bool());
    assert_eq!(Some(true), report["matched"].as_bool());
    assert_eq!(Some(1), report["counts"]["verified_files"].as_u64());
    drop(daemon);
}

#[cfg(unix)]
#[test]
fn read_only_local_index_dir_falls_back_to_xdg_cache() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    fs::write(fixture.path("hit.txt"), "xdg fallback needle\n").unwrap();
    fs::create_dir_all(fixture.path(".eg")).unwrap();
    fs::set_permissions(fixture.path(".eg"), fs::Permissions::from_mode(0o555)).unwrap();

    let cache = fixture.path("cache");
    let output = eg_with_env(
        &["xdg fallback needle", fixture.root.to_str().unwrap()],
        &[("XDG_CACHE_HOME", &cache)],
    );
    fs::set_permissions(fixture.path(".eg"), fs::Permissions::from_mode(0o755)).unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8(output.stderr).unwrap()
    );
    assert!(stdout.contains("xdg fallback needle"), "{stdout}");
    assert!(cache.join("eg").exists());
}

#[test]
fn default_indexed_search_supports_everything_literal() {
    let fixture = Fixture::new();
    fs::write(fixture.path("hit.txt"), "Everything initial fixture\n").unwrap();
    fs::write(fixture.path("miss.txt"), "nothing relevant\n").unwrap();

    let output = eg(&["Everything initial", fixture.root.to_str().unwrap()]);
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
fn auto_index_unchanged_uses_daemon_proofed_snapshot() {
    let fixture = Fixture::new();
    fs::write(fixture.path("sample.txt"), "fast freshness needle\n").unwrap();

    let first = eg(&[
        "--index=auto",
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
        stderr.contains("eg index: loaded daemon-proofed manifest snapshot"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("eg index: collected"),
        "unchanged auto search should not walk the tree: {stderr}"
    );
}

#[test]
fn auto_index_refreshes_changed_files_through_daemon_republish() {
    let fixture = Fixture::new();
    fs::write(fixture.path("sample.txt"), "alpha stable needle\n").unwrap();

    let first = eg(&[
        "--index=auto",
        "stable needle",
        fixture.root.to_str().unwrap(),
    ]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8(first.stderr).unwrap()
    );

    let marker = fixture.root.join(".eg/index/postings-v9/auto-marker");
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
        stderr.contains("eg index: loaded daemon-proofed manifest snapshot"),
        "{stderr}"
    );
    assert!(
        !marker.exists(),
        "daemon republish should replace the stale index home"
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
fn auto_index_ignores_stale_base_postings_for_changed_files() {
    let fixture = Fixture::new();
    for i in 0..40 {
        let bootstrap = if i == 0 { " bootstrap_unique" } else { "" };
        fs::write(
            fixture.path(format!("sample-{i}.txt")),
            format!("zzzz_obsolete_token_zzzz file {i}{bootstrap}\n"),
        )
        .unwrap();
    }

    let first = eg(&[
        "--index=auto",
        "bootstrap_unique",
        fixture.root.to_str().unwrap(),
    ]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8(first.stderr).unwrap()
    );

    std::thread::sleep(std::time::Duration::from_millis(20));
    for i in 0..40 {
        fs::write(
            fixture.path(format!("sample-{i}.txt")),
            format!("yyyy_fresh_token_yyyy file {i}\n"),
        )
        .unwrap();
    }

    let stale = eg(&[
        "--debug",
        "--index=auto",
        "zzzz_obsolete_token_zzzz file 0",
        fixture.root.to_str().unwrap(),
    ]);
    let stderr = String::from_utf8(stale.stderr).unwrap();

    assert_eq!(Some(1), stale.status.code(), "{stderr}");
    assert_eq!("", String::from_utf8(stale.stdout).unwrap());
    assert!(
        stderr.contains("backend prepare+lookup produced 0 candidates"),
        "stale postings should not inflate the candidate set: {stderr}"
    );
}

#[test]
fn indexed_byte_count_needs_prune_short_repetition_candidates() {
    let fixture = Fixture::new();
    for i in 0..40 {
        fs::write(fixture.path(format!("short_{i:02}.txt")), "ababa\n").unwrap();
    }
    fs::write(fixture.path("hit.txt"), "ababababab\n").unwrap();

    let output = eg(&[
        "--debug",
        "--index=auto",
        "--files-with-matches",
        "(ab){5}",
        fixture.root.to_str().unwrap(),
    ]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();

    assert!(output.status.success(), "{stderr}");
    assert!(stdout.contains("hit.txt"), "{stdout}");
    assert!(!stdout.contains("short_"), "{stdout}");
    assert!(
        stderr.contains("backend prepare+lookup produced 1 candidates"),
        "summary byte-count needs should reject short near-misses before verification: {stderr}"
    );
}

#[test]
fn auto_index_uses_daemon_proof_for_git_worktrees() {
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
        "--index=auto",
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
        clean_stderr.contains("eg index: loaded daemon-proofed manifest snapshot"),
        "{clean_stderr}"
    );
    assert!(
        !clean_stderr.contains("eg index: collected"),
        "daemon-proofed path should not walk the tree: {clean_stderr}"
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
        changed_stderr.contains("eg index: loaded daemon-proofed manifest snapshot"),
        "{changed_stderr}"
    );
}

#[test]
fn auto_index_drops_deleted_files_after_file_list_changes() {
    let fixture = Fixture::new();
    fs::write(fixture.path("old.txt"), "deleted indexed needle\n").unwrap();
    fs::write(fixture.path("kept.txt"), "kept indexed needle\n").unwrap();

    let first = eg(&[
        "--index=auto",
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
        "--index=auto",
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
        "--index=auto",
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
fn indexed_passthru_errors_with_help() {
    let fixture = Fixture::new();
    fs::write(fixture.path("sample.txt"), "alpha constrained needle\n").unwrap();
    let root = fixture.root.to_str().unwrap();

    let output = eg(&["--index=auto", "--passthru", "constrained needle", root]);
    let stderr = String::from_utf8(output.stderr).unwrap();

    assert_eq!(Some(2), output.status.code());
    assert!(stderr.contains("indexed search cannot run with `--passthru`"));
    assert!(stderr.contains("--no-index"), "{stderr}");
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
            stderr.contains("indexed search cannot run with"),
            "{stderr}"
        );
        assert!(stderr.contains("--no-index"), "{stderr}");
    }
}

#[test]
fn indexed_null_data_errors_with_help() {
    let fixture = Fixture::new();
    fs::write(fixture.path("sample.txt"), b"alpha\0beta").unwrap();
    let root = fixture.root.to_str().unwrap();

    let output = eg(&["--index=auto", "--null-data", "alpha", root]);
    let stderr = String::from_utf8(output.stderr).unwrap();

    assert_eq!(Some(2), output.status.code());
    assert!(stderr.contains("indexed search cannot run with `--null-data`"));
    assert!(stderr.contains("--no-index"), "{stderr}");
}

#[test]
fn indexed_invalid_regex_reports_parse_error() {
    let fixture = Fixture::new();
    fs::write(fixture.path("sample.txt"), "alpha\n").unwrap();

    let output = eg(&["[", fixture.root.to_str().unwrap()]);
    let stderr = String::from_utf8(output.stderr).unwrap();

    assert_eq!(Some(2), output.status.code());
    assert!(stderr.contains("invalid regex"), "{stderr}");
    assert!(!stderr.contains("too broad"), "{stderr}");
}

#[test]
fn indexed_many_patterns_past_planner_limit_errors_with_no_index_help() {
    let fixture = Fixture::new();
    fs::write(fixture.path("sample.txt"), "needle_420\n").unwrap();
    let mut args = vec!["--index=auto".to_owned()];
    for i in 0..450 {
        args.push("-e".to_owned());
        args.push(format!("needle_{i:03}"));
    }
    args.push(fixture.root.to_string_lossy().into_owned());
    let refs = args.iter().map(String::as_str).collect::<Vec<_>>();

    let output = eg(&refs);
    let stderr = String::from_utf8(output.stderr).unwrap();

    assert_eq!(Some(2), output.status.code());
    assert!(stderr.contains("--no-index"), "{stderr}");
}

#[test]
fn indexed_invert_match_errors_with_help() {
    let fixture = Fixture::new();
    fs::write(fixture.path("sample.txt"), "alpha\n").unwrap();

    let output = eg(&[
        "--index=auto",
        "-v",
        "absent",
        fixture.root.to_str().unwrap(),
    ]);
    let stderr = String::from_utf8(output.stderr).unwrap();

    assert_eq!(Some(2), output.status.code());
    assert!(stderr.contains("inverted matches"), "{stderr}");
    assert!(stderr.contains("--no-index"), "{stderr}");
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
fn indexed_crlf_anchor_matches_through_the_index() {
    let fixture = Fixture::new();
    fs::write(fixture.path("sample.txt"), b"crlf constrained needle\r\n").unwrap();
    let root = fixture.root.to_str().unwrap();

    let output = eg(&["--index=auto", "--crlf", r"crlf constrained needle$", root]);
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8(output.stderr).unwrap()
    );
    assert!(stdout.contains("crlf constrained needle"), "{stdout}");
}

#[test]
fn indexed_no_mmap_search_uses_read_path() {
    let fixture = Fixture::new();
    fs::write(fixture.path("sample.txt"), "read path indexed needle\n").unwrap();

    let output = eg(&[
        "--index=auto",
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

    let output = eg(&[".*", fixture.root.to_str().unwrap()]);
    let stderr = String::from_utf8(output.stderr).unwrap();

    assert_eq!(Some(2), output.status.code());
    assert!(stderr.contains("indexed search cannot use this pattern"));
    assert!(stderr.contains("too broad"));
    assert!(stderr.contains("--no-index"));
}

#[test]
fn indexed_metadata_only_patterns_error_without_scan_fallback() {
    let fixture = Fixture::new();
    fs::write(fixture.path("sample.txt"), "alpha beta ab\n").unwrap();

    for pattern in [".", "[a-z]+", "ab"] {
        let output = eg(&["--index=auto", pattern, fixture.root.to_str().unwrap()]);
        let stderr = String::from_utf8(output.stderr).unwrap();

        assert_eq!(Some(2), output.status.code(), "{stderr}");
        assert!(stderr.contains("indexed search cannot use this pattern"));
        assert!(stderr.contains("too broad"));
        assert!(stderr.contains("--no-index"));
        assert_eq!("", String::from_utf8(output.stdout).unwrap());
    }
}

#[test]
fn indexed_impossible_regex_errors_with_help() {
    let fixture = Fixture::new();
    fs::write(fixture.path("sample.txt"), "foo bar\n").unwrap();

    let output = eg(&["foo$bar", fixture.root.to_str().unwrap()]);
    let stderr = String::from_utf8(output.stderr).unwrap();

    assert_eq!(Some(2), output.status.code());
    assert!(stderr.contains("indexed search cannot use this pattern"));
    assert!(stderr.contains("cannot match"));
    assert!(stderr.contains("anchors"));
}

#[test]
fn indexed_common_literal_over_ceiling_errors_without_verify() {
    assert_common_literal_over_ceiling(&[]);
}

#[test]
fn tantivy_common_literal_over_ceiling_errors_without_verify() {
    assert_common_literal_over_ceiling(&["--index-backend=tantivy"]);
}

fn assert_common_literal_over_ceiling(backend_args: &[&str]) {
    let fixture = Fixture::new();
    for i in 0..80 {
        fs::write(
            fixture.path(format!("sample_{i:02}.txt")),
            format!("static int sample_{i:02};\n"),
        )
        .unwrap();
    }

    let mut args = vec!["--debug", "--index=auto", "--files-with-matches"];
    args.extend_from_slice(backend_args);
    args.extend_from_slice(&["static int", fixture.root.to_str().unwrap()]);
    let output = eg(&args);
    let stderr = String::from_utf8(output.stderr).unwrap();

    assert_eq!(Some(2), output.status.code(), "{stderr}");
    assert_eq!("", String::from_utf8(output.stdout).unwrap());
    assert!(
        stderr.contains("selects too much of the corpus"),
        "common gram-backed plans should reject before verification: {stderr}"
    );
}

#[test]
fn postings_selectivity_ceiling_uses_text_entries_not_skipped_files() {
    let fixture = Fixture::new();
    for i in 0..40 {
        fs::write(
            fixture.path(format!("hit_{i:02}.txt")),
            format!("static int selected_{i:02};\n"),
        )
        .unwrap();
        fs::write(
            fixture.path(format!("miss_{i:02}.txt")),
            format!("plain text selected_{i:02};\n"),
        )
        .unwrap();
    }
    for i in 0..200 {
        fs::write(
            fixture.path(format!("binary_{i:03}.bin")),
            [0, 159, 146, 150],
        )
        .unwrap();
    }

    let output = eg(&[
        "--debug",
        "--index=auto",
        "--files-with-matches",
        "static int",
        fixture.root.to_str().unwrap(),
    ]);
    let stderr = String::from_utf8(output.stderr).unwrap();

    assert_eq!(Some(2), output.status.code(), "{stderr}");
    assert_eq!("", String::from_utf8(output.stdout).unwrap());
    assert!(
        stderr.contains("actual candidates 40 of 80 docs exceeds 30%"),
        "selectivity should be based on text entries, not skipped binary files: {stderr}"
    );
}

#[test]
fn indexed_broad_numeric_class_over_ceiling_errors_without_verify() {
    let fixture = Fixture::new();
    for i in 0..80 {
        fs::write(
            fixture.path(format!("sample_{i:02}.txt")),
            format!("mask value 0x12345678 sample {i:02}\n"),
        )
        .unwrap();
    }

    let output = eg(&[
        "--debug",
        "--index=auto",
        "--files-with-matches",
        r"0x[0-9a-fA-F]{8}",
        fixture.root.to_str().unwrap(),
    ]);
    let stderr = String::from_utf8(output.stderr).unwrap();

    assert_eq!(Some(2), output.status.code(), "{stderr}");
    assert_eq!("", String::from_utf8(output.stdout).unwrap());
    assert!(
        stderr.contains("selects too much of the corpus"),
        "class-expanded numeric plans should stay estimate-rejected: {stderr}"
    );
}

#[test]
fn forced_candidates_count_toward_selectivity_rejection() {
    let fixture = Fixture::new();
    fs::write(fixture.path("hit.txt"), "rare forced candidate needle\n").unwrap();
    for i in 0..80 {
        fs::write(
            fixture.path(format!("encoded_{i:02}.txt")),
            [0xFF, 0xFE, b'x', b'y', b'z'],
        )
        .unwrap();
    }

    let output = eg(&[
        "--debug",
        "--index=auto",
        "--files-with-matches",
        "rare forced candidate needle",
        fixture.root.to_str().unwrap(),
    ]);
    let stderr = String::from_utf8(output.stderr).unwrap();

    assert_eq!(Some(2), output.status.code(), "{stderr}");
    assert!(
        stderr.contains("selects too much of the corpus"),
        "high estimates should reject the indexed path: {stderr}"
    );
    assert!(
        stderr.contains("--no-index"),
        "rejection should explain the exact-scan escape hatch: {stderr}"
    );
}

#[test]
fn tantivy_forced_candidates_count_toward_selectivity_rejection() {
    let fixture = Fixture::new();
    fs::write(fixture.path("hit.txt"), "rare forced candidate needle\n").unwrap();
    for i in 0..80 {
        fs::write(
            fixture.path(format!("encoded_{i:02}.txt")),
            [0xFF, 0xFE, b'x', b'y', b'z'],
        )
        .unwrap();
    }

    let output = eg(&[
        "--debug",
        "--index=auto",
        "--index-backend=tantivy",
        "--files-with-matches",
        "rare forced candidate needle",
        fixture.root.to_str().unwrap(),
    ]);
    let stderr = String::from_utf8(output.stderr).unwrap();

    assert_eq!(Some(2), output.status.code(), "{stderr}");
    assert!(
        stderr.contains("selects too much of the corpus"),
        "high estimates should reject the indexed path in tantivy backend: {stderr}"
    );
    assert!(
        stderr.contains("--no-index"),
        "rejection should explain the exact-scan escape hatch: {stderr}"
    );
}

#[test]
fn indexed_search_skips_binary_files_instead_of_forcing_them() {
    let fixture = Fixture::new();
    fs::write(fixture.path("blob.bin"), b"rare binary needle\0tail\n").unwrap();
    for i in 0..10 {
        fs::write(
            fixture.path(format!("text_{i:02}.txt")),
            "plain text miss\n",
        )
        .unwrap();
    }

    let output = eg(&[
        "--debug",
        "--index=auto",
        "--files-with-matches",
        "rare binary needle",
        fixture.root.to_str().unwrap(),
    ]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();

    assert_eq!(Some(1), output.status.code(), "{stderr}");
    assert_eq!("", stdout);
    assert!(
        stderr.contains("backend prepare+lookup produced 0 candidates"),
        "binary files should not enter the forced-candidate set: {stderr}"
    );
    assert!(
        !stderr.contains("found binary data"),
        "binary files should not be searched by indexed verification: {stderr}"
    );
}

#[test]
fn indexed_search_skips_late_nul_binary_files() {
    let fixture = Fixture::new();
    let mut bytes = b"late binary needle\n".to_vec();
    bytes.extend(std::iter::repeat_n(b'a', 16 * 1024));
    bytes.push(0);
    fs::write(fixture.path("late.bin"), bytes).unwrap();
    fs::write(fixture.path("text.txt"), "plain text miss\n").unwrap();

    let output = eg(&[
        "--debug",
        "--index=auto",
        "--files-with-matches",
        "late binary needle",
        fixture.root.to_str().unwrap(),
    ]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();

    assert_eq!(Some(1), output.status.code(), "{stderr}");
    assert_eq!("", stdout);
    assert!(
        stderr.contains("backend prepare+lookup produced 0 candidates"),
        "late-NUL binary files should not be gram-indexed: {stderr}"
    );
}

#[test]
fn indexed_full_corpus_modes_do_not_synthesize_binary_no_matches() {
    let fixture = Fixture::new();
    fs::write(fixture.path("hit.txt"), "full corpus needle\n").unwrap();
    fs::write(fixture.path("miss.txt"), "plain text miss\n").unwrap();
    fs::write(fixture.path("blob.bin"), b"binary miss\0tail\n").unwrap();

    let output = eg(&[
        "--index=auto",
        "--files-without-match",
        "full corpus needle",
        fixture.root.to_str().unwrap(),
    ]);
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8(output.stderr).unwrap()
    );
    assert!(stdout.contains("miss.txt"), "{stdout}");
    assert!(!stdout.contains("hit.txt"), "{stdout}");
    assert!(!stdout.contains("blob.bin"), "{stdout}");
}

#[test]
fn indexed_search_rejects_binary_modes() {
    let fixture = Fixture::new();
    fs::write(fixture.path("sample.txt"), "plain text needle\n").unwrap();

    for flag in ["--binary", "--text"] {
        let output = eg(&[flag, "plain text needle", fixture.root.to_str().unwrap()]);
        let stderr = String::from_utf8(output.stderr).unwrap();

        assert_eq!(Some(2), output.status.code(), "{flag}: {stderr}");
        assert!(stderr.contains("binary search flags"), "{flag}: {stderr}");
        assert!(
            stderr.contains("does not search binary data"),
            "{flag}: {stderr}"
        );
        assert!(stderr.contains("--no-index"), "{flag}: {stderr}");
    }
}

#[test]
fn indexed_search_honors_reverse_path_sort() {
    let fixture = Fixture::new();
    fs::write(fixture.path("a.txt"), "sort needle\n").unwrap();
    fs::write(fixture.path("z.txt"), "sort needle\n").unwrap();
    for i in 0..10 {
        fs::write(fixture.path(format!("miss_{i:02}.txt")), "no match here\n").unwrap();
    }

    let output = eg(&[
        "--debug",
        "--index=auto",
        "--sortr",
        "path",
        "sort needle",
        fixture.root.to_str().unwrap(),
    ]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();

    assert!(output.status.success(), "{stderr}");
    assert!(
        !stderr.contains("falling back to unindexed scan"),
        "{stderr}"
    );
    let z = stdout.find("z.txt").expect("z.txt in output");
    let a = stdout.find("a.txt").expect("a.txt in output");
    assert!(z < a, "{stdout}");
}

#[test]
fn indexed_sparse_regex_uses_query_plan() {
    let fixture = Fixture::new();
    fs::write(fixture.path("hit.txt"), "alpha sparse-regex needle\n").unwrap();
    fs::write(fixture.path("miss.txt"), "alpha sparseXregex needle\n").unwrap();

    let output = eg(&[
        "--index=auto",
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
        "--index=auto",
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
        "--index=auto",
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
        "--index=auto",
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
        "--index=auto",
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
        "--index=auto",
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
fn indexed_utf16_bom_file_is_not_searched_as_binary() {
    let fixture = Fixture::new();
    let mut bytes = vec![0xFF, 0xFE];
    for unit in "unicode sparse needle\n".encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    fs::write(fixture.path("utf16.txt"), bytes).unwrap();

    let output = eg(&[
        "--debug",
        "--index=auto",
        "--files-with-matches",
        "unicode sparse needle",
        fixture.root.to_str().unwrap(),
    ]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();

    assert_eq!(Some(0), output.status.code(), "{stderr}");
    assert!(stdout.contains("utf16.txt"), "{stdout}");
    assert!(
        stderr.contains("backend prepare+lookup produced 1 candidates"),
        "{stderr}"
    );
    assert!(!stderr.contains("found binary data"), "{stderr}");
}

#[test]
fn indexed_fixed_string_treats_regex_syntax_literally() {
    let fixture = Fixture::new();
    fs::write(fixture.path("literal.txt"), "call a.b[1] exactly\n").unwrap();
    fs::write(fixture.path("regexish.txt"), "call axb1 as regex bait\n").unwrap();

    let output = eg(&[
        "--index=auto",
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
        "--index-backend=tantivy",
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
        "--index=auto",
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

#[test]
fn block_masks_prune_cross_line_gap_candidates() {
    let fixture = Fixture::new();
    let far = format!(
        "alpha_gap_needle here\n{}omega_gap_needle there\n",
        "filler line of text\n".repeat(200)
    );
    fs::write(fixture.path("far.txt"), far).unwrap();
    fs::write(
        fixture.path("near.txt"),
        "alpha_gap_needle and omega_gap_needle share a line\n",
    )
    .unwrap();
    for pad in 0..40 {
        fs::write(
            fixture.path(format!("pad{pad}.txt")),
            format!("padding corpus file {pad} keeps selectivity ceilings sane\n"),
        )
        .unwrap();
    }
    let root = fixture.root.to_str().unwrap();

    let bench = eg(&["--bench", "alpha_gap_needle.*omega_gap_needle", root]);
    let stdout = String::from_utf8(bench.stdout).unwrap();
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(Some(true), report["ok"].as_bool());
    assert_eq!(Some(1), report["counts"]["candidate_files"].as_u64());
    assert_eq!(Some(1), report["counts"]["matched_files"].as_u64());

    let multiline = eg(&[
        "--bench",
        "-U",
        "alpha_gap_needle(?s:.)*omega_gap_needle",
        root,
    ]);
    let stdout = String::from_utf8(multiline.stdout).unwrap();
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(Some(true), report["ok"].as_bool());
    assert_eq!(Some(2), report["counts"]["candidate_files"].as_u64());
    assert_eq!(Some(2), report["counts"]["matched_files"].as_u64());
}

#[test]
fn word_boundary_queries_prune_midword_candidates() {
    let fixture = Fixture::new();
    fs::write(
        fixture.path("midword.txt"),
        "the domain remains chained explained\n",
    )
    .unwrap();
    fs::write(fixture.path("word.txt"), "fn main() { start here }\n").unwrap();
    for pad in 0..40 {
        fs::write(
            fixture.path(format!("pad{pad}.txt")),
            format!("padding corpus file {pad} keeps selectivity ceilings sane\n"),
        )
        .unwrap();
    }
    let root = fixture.root.to_str().unwrap();

    let bench = eg(&["--bench", "-w", "main", root]);
    let stdout = String::from_utf8(bench.stdout).unwrap();
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(Some(true), report["ok"].as_bool());
    assert_eq!(Some(1), report["counts"]["candidate_files"].as_u64());
    assert_eq!(Some(1), report["counts"]["matched_files"].as_u64());

    let plain = eg(&["--bench", "main", root]);
    let stdout = String::from_utf8(plain.stdout).unwrap();
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(Some(2), report["counts"]["candidate_files"].as_u64());
    assert_eq!(Some(2), report["counts"]["matched_files"].as_u64());
}
