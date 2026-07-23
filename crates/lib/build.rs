//! Build-time validation for the embedded weight table

#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "build scripts fail the build by panicking with a clear error"
)]

use std::{env, fs, path::PathBuf};

const TABLE: &str = "data/weights.bin";

fn main() {
    println!("cargo::rerun-if-changed={TABLE}");
    if env::var_os("CARGO_FEATURE_WEIGHTS").is_some() {
        validate(TABLE);
    }
}

fn validate(relative: &str) {
    let manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by Cargo"),
    );
    let path = manifest_dir.join(relative);
    let bytes =
        fs::read(&path).unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    let table = sngram_types::WeightTable::from_bytes(&bytes)
        .unwrap_or_else(|err| panic!("invalid embedded table {}: {err}", path.display()));
    write_fingerprint(table.fingerprint());
}

fn write_fingerprint(fingerprint: u64) {
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set by Cargo"));
    fs::write(
        out_dir.join("weights.rs"),
        format!(
            "const WEIGHTS_FINGERPRINT: u64 = {};\n",
            rust_u64(fingerprint)
        ),
    )
    .expect("write fingerprint");
}

fn rust_u64(value: u64) -> String {
    let raw = value.to_string();
    let mut out = String::with_capacity(raw.len() + raw.len() / 3);
    for (i, ch) in raw.chars().enumerate() {
        if i > 0 && (raw.len() - i).is_multiple_of(3) {
            out.push('_');
        }
        out.push(ch);
    }
    out
}
