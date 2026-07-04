//! Build-time validation for embedded weight tables.

#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "build scripts fail the build by panicking with a clear error"
)]

use std::{env, fs, path::PathBuf};

struct Table {
    feature: &'static str,
    env: &'static str,
    path: &'static str,
}

const TABLES: &[Table] = &[
    Table {
        feature: "500gb",
        env: "CARGO_FEATURE_500GB",
        path: "data/500gb_weights.bin",
    },
    Table {
        feature: "1tb",
        env: "CARGO_FEATURE_1TB",
        path: "data/1tb_weights.bin",
    },
    Table {
        feature: "2tb",
        env: "CARGO_FEATURE_2TB",
        path: "data/2tb_weights.bin",
    },
    Table {
        feature: "3tb",
        env: "CARGO_FEATURE_3TB",
        path: "data/3tb_weights.bin",
    },
    Table {
        feature: "4tb",
        env: "CARGO_FEATURE_4TB",
        path: "data/4tb_weights.bin",
    },
    Table {
        feature: "5tb",
        env: "CARGO_FEATURE_5TB",
        path: "data/5tb_weights.bin",
    },
    Table {
        feature: "6tb",
        env: "CARGO_FEATURE_6TB",
        path: "data/6tb_weights.bin",
    },
    Table {
        feature: "7tb",
        env: "CARGO_FEATURE_7TB",
        path: "data/7tb_weights.bin",
    },
    Table {
        feature: "8tb",
        env: "CARGO_FEATURE_8TB",
        path: "data/8tb_weights.bin",
    },
    Table {
        feature: "9tb",
        env: "CARGO_FEATURE_9TB",
        path: "data/9tb_weights.bin",
    },
    Table {
        feature: "10tb",
        env: "CARGO_FEATURE_10TB",
        path: "data/10tb_weights.bin",
    },
    Table {
        feature: "11tb",
        env: "CARGO_FEATURE_11TB",
        path: "data/11tb_weights.bin",
    },
    Table {
        feature: "12tb",
        env: "CARGO_FEATURE_12TB",
        path: "data/final_weights.bin",
    },
];

fn main() {
    for table in TABLES {
        println!("cargo::rerun-if-changed={}", table.path);
    }

    let enabled = TABLES
        .iter()
        .filter(|table| env::var_os(table.env).is_some())
        .collect::<Vec<_>>();
    match enabled.as_slice() {
        [] => {},
        [table] => validate(table),
        tables => {
            let names = tables
                .iter()
                .map(|table| table.feature)
                .collect::<Vec<_>>()
                .join(", ");
            panic!("enable exactly one sngram weight table feature; enabled: {names}");
        },
    }
}

fn validate(table: &Table) {
    let manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by Cargo"),
    );
    let path = manifest_dir.join(table.path);
    let bytes =
        fs::read(&path).unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    sngram_types::WeightTable::from_bytes(&bytes)
        .unwrap_or_else(|err| panic!("invalid embedded table {}: {err}", path.display()));
}
