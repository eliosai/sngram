//! Bit-identity tests: production `scan` against the frozen baseline scanner.
//!
//! Every test asserts the grams match exactly, keys, spans and order, and that
//! the final summary matches.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod frozen;

use std::path::{Path, PathBuf};

use sngram::{ScanSummary, ScannedGram, WeightTable};

type Scan = (Vec<ScannedGram>, ScanSummary);

fn production_scan(table: &WeightTable, data: &[u8]) -> Scan {
    let mut grams = Vec::new();
    let summary = sngram::scan(table, data, |gram| grams.push(gram));
    (grams, summary)
}

fn frozen_scan(table: &WeightTable, data: &[u8]) -> Scan {
    let mut grams = Vec::new();
    let summary = frozen::scan(table, data, |gram| grams.push(gram));
    (grams, summary)
}

fn assert_identical(name: &str, table: &WeightTable, data: &[u8]) {
    let (expected, expected_summary) = frozen_scan(table, data);
    let (got, got_summary) = production_scan(table, data);
    assert_grams_match(name, &expected, &got);
    assert_eq!(expected_summary, got_summary, "summary differs on {name}");
}

fn assert_grams_match(name: &str, expected: &[ScannedGram], got: &[ScannedGram]) {
    let first_diff = expected
        .iter()
        .zip(got)
        .position(|(expected_gram, got_gram)| expected_gram != got_gram);
    if let Some(at) = first_diff {
        assert_eq!(expected[at], got[at], "gram {at} differs on {name}");
    }
    assert_eq!(expected.len(), got.len(), "gram count differs on {name}");
}

/// Deterministic LCG so failures reproduce exactly
struct Lcg(u64);

impl Lcg {
    const fn next_u32(&mut self) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) as u32
    }

    #[allow(clippy::cast_possible_truncation, reason = "masked to one byte")]
    const fn next_byte(&mut self) -> u8 {
        (self.next_u32() & 0xFF) as u8
    }

    fn next_text_byte(&mut self) -> u8 {
        const ALPHABET: &[u8] = b"abcdefgXYZ0129_-/.:;{}()[]<> =+\"\r\n\t\x7f\xc3\xa9";
        ALPHABET[(self.next_u32() as usize) % ALPHABET.len()]
    }
}

fn tables() -> Vec<(String, WeightTable)> {
    vec![
        (
            "crc32".to_owned(),
            WeightTable::from_weight_fn(|a, b| crc32fast::hash(&[a, b])),
        ),
        (
            "plateau4".to_owned(),
            WeightTable::from_weight_fn(|a, b| crc32fast::hash(&[a, b]) % 4),
        ),
        ("const".to_owned(), WeightTable::from_weight_fn(|_, _| 7)),
        (
            "monotonic".to_owned(),
            WeightTable::from_weight_fn(|a, b| {
                if b == a.wrapping_add(1) {
                    1_000_000 - u32::from(a)
                } else {
                    u32::from(a) ^ u32::from(b)
                }
            }),
        ),
    ]
}

#[test]
fn tiny_inputs_of_every_length_match() {
    let mut rng = Lcg(11);
    for (tname, table) in tables() {
        for len in 0..64usize {
            let text: Vec<u8> = (0..len).map(|_| rng.next_text_byte()).collect();
            let raw: Vec<u8> = (0..len).map(|_| rng.next_byte()).collect();
            assert_identical(&format!("{tname}/tiny_text_{len}"), &table, &text);
            assert_identical(&format!("{tname}/tiny_raw_{len}"), &table, &raw);
        }
    }
}

fn adversarial_inputs() -> Vec<(String, Vec<u8>)> {
    let mut out: Vec<(String, Vec<u8>)> = vec![
        ("uniform_5000".into(), vec![b'a'; 5000]),
        ("uniform_upper_5000".into(), vec![b'A'; 5000]),
        ("no_newline_9000".into(), b"xY".repeat(4500)),
        ("nul_small".into(), b"a\x00b".to_vec()),
        ("nul_dense".into(), vec![0u8; 64]),
        (
            "invalid_utf8".into(),
            b"\xc3\x28\xa0\xa1\xe2\x28\xa1ok\xf0\x90\x28\xbc".repeat(40),
        ),
        (
            "crlf_mix".into(),
            b"line one\r\nline two\rline three\n\n\r\n".repeat(50),
        ),
    ];
    out.extend(byte_range_inputs());
    out.extend(periodic_inputs());
    out
}

fn byte_range_inputs() -> Vec<(String, Vec<u8>)> {
    let ascending: Vec<u8> = (0..3000usize)
        .map(|i| 32 + u8::try_from(i % 95).unwrap())
        .collect();
    let high: Vec<u8> = (0..2000usize)
        .map(|i| 128 + u8::try_from(i % 128).unwrap())
        .collect();
    vec![
        ("ascending_printable".into(), ascending),
        ("high_bytes".into(), high),
    ]
}

fn periodic_inputs() -> Vec<(String, Vec<u8>)> {
    let mut out: Vec<(String, Vec<u8>)> = Vec::new();
    let src = b"pub fn ExampleValue42() {\n    let HTTP_ID = Some(\"AlphaBeta\");\n}\n";
    out.push((
        "code_mixed_case_9000".into(),
        (0..9000usize).map(|i| src[i % src.len()]).collect(),
    ));
    for len in [127usize, 128, 129, 1023, 1024, 1025, 8191, 8192, 8193] {
        let mut rng = Lcg(len as u64);
        out.push((
            format!("boundary_{len}"),
            (0..len).map(|_| rng.next_text_byte()).collect(),
        ));
    }
    out
}

#[test]
fn adversarial_inputs_match() {
    for (tname, table) in tables() {
        for (iname, data) in adversarial_inputs() {
            assert_identical(&format!("{tname}/{iname}"), &table, &data);
        }
    }
}

#[test]
fn random_fuzz_inputs_match() {
    let mut rng = Lcg(0xF00D);
    let tables = tables();
    for round in 0..300usize {
        let len = (rng.next_u32() as usize) % 4000;
        let data: Vec<u8> = if round % 3 == 0 {
            (0..len).map(|_| rng.next_byte()).collect()
        } else {
            (0..len).map(|_| rng.next_text_byte()).collect()
        };
        let (tname, table) = &tables[round % tables.len()];
        assert_identical(&format!("{tname}/fuzz_{round}_{len}"), table, &data);
    }
}

fn source_files(root: &Path, ext: &str, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() && !path.ends_with(".git") {
            source_files(&path, ext, out);
        } else if kind.is_file() && path.extension().is_some_and(|found| found == ext) {
            out.push(path);
        }
    }
}

#[test]
fn own_source_corpus_matches() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let table = WeightTable::from_weight_fn(|a, b| crc32fast::hash(&[a, b]));
    let mut files = Vec::new();
    source_files(&manifest.join("src"), "rs", &mut files);
    assert!(files.len() > 10, "expected repo sources as fixtures");
    for path in files {
        let data = std::fs::read(&path).expect("read source fixture");
        let name = path.display().to_string();
        assert_identical(&name, &table, &data);
    }
}

fn walk_budget(root: &Path, budget: &mut u64, table: &WeightTable, stats: &mut (u64, u64)) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        if *budget == 0 {
            return;
        }
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() && !path.ends_with(".git") {
            walk_budget(&path, budget, table, stats);
        } else if kind.is_file() {
            compare_real_file(&path, budget, table, stats);
        }
    }
}

fn compare_real_file(path: &Path, budget: &mut u64, table: &WeightTable, stats: &mut (u64, u64)) {
    let Ok(data) = std::fs::read(path) else {
        return;
    };
    if data.len() as u64 > 2_000_000 {
        return;
    }
    *budget = budget.saturating_sub(data.len() as u64);
    let name = path.display().to_string();
    assert_identical(&name, table, &data);
    stats.0 += 1;
    stats.1 += data.len() as u64;
}

/// Heavy fixture run over real repositories; run with --release --ignored
#[test]
#[ignore = "multi hundred MB corpus, run explicitly in release"]
fn real_corpus_matches() {
    let table = WeightTable::from_weight_fn(|a, b| crc32fast::hash(&[a, b]));
    let mut stats = (0u64, 0u64);
    for root in [
        "/home/mike/repos/linux",
        "/home/mike/repos/django",
        "/home/mike/repos/k8s",
    ] {
        let path = Path::new(root);
        if !path.is_dir() {
            eprintln!("skipping missing corpus root {root}");
            continue;
        }
        let mut budget = 120_000_000u64;
        walk_budget(path, &mut budget, &table, &mut stats);
    }
    eprintln!(
        "real corpus identical: {} files, {} bytes",
        stats.0, stats.1
    );
    assert!(stats.0 > 0, "no corpus files compared");
}
