//! A published index that cannot be read must not cost the caller its results.
//!
//! The daemon publishes a generation and a query reads it moments later. If
//! that read fails for any reason, the query owes the caller the same answer
//! `--no-index` would give, not an error and an empty result.
#![allow(missing_docs, clippy::unwrap_used)]

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

const FILES: usize = 120;
const NEEDLE: &str = "quokka";

fn eg_binary() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.join("eg")
}

fn run(root: &Path, runtime: &Path, args: &[&str]) -> (String, Option<i32>) {
    let output = Command::new(eg_binary())
        .args(args)
        .arg("./")
        .current_dir(root)
        .env("EG_INDEXD_RUNTIME_ROOT", runtime)
        .output()
        .unwrap();
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    (text, output.status.code())
}

/// Strip every published manifest of read permission
fn make_manifests_unreadable(root: &Path) -> usize {
    let mut broken = 0;
    let mut stack = vec![root.join(".eg")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path
                .file_name()
                .is_some_and(|n| n == "manifest.bin" || n == "manifest.json")
            {
                let mut perms = fs::metadata(&path).unwrap().permissions();
                std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o000);
                fs::set_permissions(&path, perms).unwrap();
                broken += 1;
            }
        }
    }
    broken
}

/// Fill a corpus where one file in twelve carries the needle
fn write_corpus(root: &Path) {
    fs::create_dir_all(root).unwrap();
    for i in 0..FILES {
        let body = if i % 12 == 0 {
            format!("a {NEEDLE} here\n")
        } else {
            "a wallaby here\n".to_string()
        };
        fs::write(root.join(format!("f{i:04}.txt")), body).unwrap();
    }
}

/// Query until the daemon has published an index for this tree
fn settle(root: &Path, runtime: &Path) {
    for _ in 0..60 {
        if run(root, runtime, &["-l", NEEDLE]).1 == Some(0) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

#[test]
fn an_unreadable_manifest_still_returns_every_match() {
    let scratch = tempfile::Builder::new()
        .prefix("eg-unreadable-manifest-")
        .tempdir()
        .unwrap();
    let root = scratch.path().join("corpus");
    let runtime = scratch.path().join("runtime");
    write_corpus(&root);
    settle(&root, &runtime);

    let scan = &["--no-index", "--sort", "path", "-l", NEEDLE];
    let (expected, _) = run(&root, &runtime, scan);
    assert!(!expected.trim().is_empty(), "fixture found no matches");
    assert!(
        make_manifests_unreadable(&root) > 0,
        "no manifest published"
    );

    let (actual, code) = run(&root, &runtime, &["--sort", "path", "-l", NEEDLE]);
    make_manifests_readable(&root);

    assert!(
        !actual.contains("is not ready"),
        "query failed instead of scanning: {actual}"
    );
    assert_eq!(code, Some(0), "query exited {code:?}: {actual}");
    assert_eq!(actual, expected, "unreadable index lost matches");
}

/// Give the manifests their permissions back so the scratch dir can be removed
fn make_manifests_readable(root: &Path) {
    let mut stack = vec![root.join(".eg")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path
                .file_name()
                .is_some_and(|n| n == "manifest.bin" || n == "manifest.json")
            {
                let mut perms = fs::metadata(&path).unwrap().permissions();
                std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o644);
                let _ = fs::set_permissions(&path, perms);
            }
        }
    }
}
