//! Build-time validation for embedded weight tier tables

#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "build scripts fail the build by panicking with a clear error"
)]

use std::{env, fs, path::PathBuf};

const TIERS: &[&str] = &["12tb"];

fn main() {
    for tier in TIERS {
        let path = format!("data/{tier}_weights.bin");
        println!("cargo::rerun-if-changed={path}");
        if env::var_os(format!("CARGO_FEATURE_{}", tier.to_uppercase())).is_some() {
            validate(&path);
        }
    }
}

fn validate(relative: &str) {
    let manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by Cargo"),
    );
    let path = manifest_dir.join(relative);
    let bytes =
        fs::read(&path).unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    sngram_types::WeightTable::from_bytes(&bytes)
        .unwrap_or_else(|err| panic!("invalid embedded table {}: {err}", path.display()));
}
