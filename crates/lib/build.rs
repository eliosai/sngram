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
    let table = sngram_types::WeightTable::from_bytes(&bytes)
        .unwrap_or_else(|err| panic!("invalid embedded table {}: {err}", path.display()));
    write_fingerprint(relative, table.fingerprint());
}

fn write_fingerprint(relative: &str, fingerprint: u64) {
    let tier = relative
        .strip_prefix("data/")
        .and_then(|name| name.strip_suffix("_weights.bin"))
        .expect("known weight path");
    let name = format!("WEIGHTS_{}_FINGERPRINT", tier.to_uppercase());
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set by Cargo"));
    let path = out_dir.join(format!("{tier}_weights.rs"));
    fs::write(
        path,
        format!("const {name}: u64 = {};\n", rust_u64(fingerprint)),
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
