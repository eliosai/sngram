//! Build-time validation for the embedded weight table.

#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "build scripts fail the build by panicking with a clear error"
)]

use std::{env, fs, path::PathBuf};

const TABLE_PATH: &str = "data/final_weights.bin";

fn main() {
    println!("cargo::rerun-if-changed={TABLE_PATH}");
    if env::var_os("CARGO_FEATURE_PRODUCTION").is_some() {
        validate();
    }
}

fn validate() {
    let manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by Cargo"),
    );
    let path = manifest_dir.join(TABLE_PATH);
    let bytes =
        fs::read(&path).unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    sngram_types::WeightTable::from_bytes(&bytes)
        .unwrap_or_else(|err| panic!("invalid embedded table {}: {err}", path.display()));
}
