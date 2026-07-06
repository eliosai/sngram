//! End-to-end benchmarks for the indexed `eg` CLI path.

use std::{
    fmt::{Display, Write as _},
    fs,
    path::PathBuf,
    process::{Command, Output},
    sync::{
        OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
};

fn main() {
    divan::main();
}

#[divan::bench]
fn hot_catalog_generation_mmap() {
    let corpus = Corpus::shared();
    let output = corpus.eg(["--bench", "rare_hot_needle", corpus.root_str()]);
    assert_success(&output);
    assert_contains(&output.stdout, "\"freshness_proof\": \"daemon\"");
}

#[divan::bench]
fn sparse_query_execution() {
    let corpus = Corpus::shared();
    let output = corpus.eg(["--bench", "module_149 shared_token", corpus.root_str()]);
    assert_success(&output);
    assert_contains(&output.stdout, "\"candidate_files\"");
}

#[divan::bench]
fn candidate_verification() {
    let corpus = Corpus::shared();
    let output = corpus.eg(["--bench", "candidate_bridge_needle", corpus.root_str()]);
    assert_success(&output);
    assert_contains(&output.stdout, "\"verified_files\"");
}

#[divan::bench]
fn cold_build() {
    let corpus = Corpus::scratch("cold");
    corpus.populate(48);
    let output = corpus.eg(["--index=rebuild", "rare_hot_needle", corpus.root_str()]);
    assert_success(&output);
}

#[divan::bench]
fn parent_index_child_restriction() {
    let corpus = Corpus::shared();
    let output = corpus.eg(["--bench", "rare_hot_needle", corpus.child_str()]);
    assert_success(&output);
    assert_contains(&output.stdout, "\"used_parent_index\": true");
}

#[divan::bench]
fn normal_hot_run() {
    let corpus = Corpus::shared();
    let output = corpus.eg(["rare_hot_needle", corpus.root_str()]);
    assert_success(&output);
}

#[divan::bench]
fn bench_hot_run() {
    let corpus = Corpus::shared();
    let output = corpus.eg(["--bench", "rare_hot_needle", corpus.root_str()]);
    assert_success(&output);
}

struct Corpus {
    root: PathBuf,
    runtime: PathBuf,
    child: PathBuf,
}

impl Corpus {
    fn shared() -> &'static Self {
        static SHARED: OnceLock<Corpus> = OnceLock::new();
        SHARED.get_or_init(|| {
            let corpus = Self::scratch("shared");
            corpus.populate(150);
            let output = corpus.eg(["--index=rebuild", "rare_hot_needle", corpus.root_str()]);
            assert_success(&output);
            corpus.mark_daemon_ready();
            corpus
        })
    }

    fn scratch(name: &str) -> Self {
        static NEXT_ID: AtomicUsize = AtomicUsize::new(0);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("eg-index-bench-{}-{id}-{name}", std::process::id()));
        let runtime = std::env::temp_dir().join(format!(
            "eg-index-bench-runtime-{}-{id}-{name}",
            std::process::id()
        ));
        let child = root.join("src");
        must(fs::create_dir_all(&child), "create benchmark corpus");
        must(fs::create_dir_all(&runtime), "create benchmark runtime");
        Self {
            root,
            runtime,
            child,
        }
    }

    fn populate(&self, files: usize) {
        must(fs::create_dir_all(&self.child), "create child dir");
        for i in 0..files {
            let path = self.child.join(format!("module_{i:03}.txt"));
            must(fs::write(path, module_text(i)), "write benchmark module");
        }
        must(
            fs::write(
                self.child.join("hit.txt"),
                "rare_hot_needle candidate_bridge_needle module_149 shared_token\n",
            ),
            "write benchmark hit",
        );
    }

    fn root_str(&self) -> &str {
        must_some(self.root.to_str(), "utf8 benchmark path")
    }

    fn child_str(&self) -> &str {
        must_some(self.child.to_str(), "utf8 benchmark path")
    }

    fn eg<const N: usize>(&self, args: [&str; N]) -> Output {
        must(
            Command::new(eg_binary())
                .args(args)
                .env("XDG_RUNTIME_DIR", &self.runtime)
                .env("EG_INDEXD_DISABLE_AUTOSPAWN", "1")
                .output(),
            "run eg benchmark command",
        )
    }

    fn mark_daemon_ready(&self) {
        let runtime = self.root.join(".eg/runtime");
        must(fs::create_dir_all(&runtime), "create state runtime");
        must(
            fs::write(runtime.join("watcher-ready"), "ready"),
            "watcher marker",
        );
        must(
            fs::write(runtime.join("journal-clean"), "clean"),
            "clean marker",
        );
    }
}

fn module_text(i: usize) -> String {
    let mut text = String::new();
    for j in 0..24 {
        must(
            writeln!(
                &mut text,
                "module_{i:03} shared_token candidate_bridge_{j:02} filler text for sparse grams"
            ),
            "write benchmark text",
        );
    }
    text
}

fn eg_binary() -> PathBuf {
    if let Some(path) = option_env!("CARGO_BIN_EXE_eg") {
        return PathBuf::from(path);
    }
    let current = must(std::env::current_exe(), "bench exe");
    let deps = must_some(current.parent(), "bench deps dir");
    let profile = deps.parent().unwrap_or(deps);
    profile.join("eg")
}

fn assert_contains(haystack: &[u8], needle: &str) {
    assert!(
        String::from_utf8_lossy(haystack).contains(needle),
        "missing {needle} in {}",
        String::from_utf8_lossy(haystack)
    );
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn must<T, E: Display>(result: Result<T, E>, action: &str) -> T {
    result.unwrap_or_else(|err| fatal(format_args!("{action}: {err}")))
}

fn must_some<T>(value: Option<T>, action: &str) -> T {
    value.unwrap_or_else(|| fatal(action))
}

fn fatal(message: impl Display) -> ! {
    eprintln!("{message}");
    std::process::abort();
}
