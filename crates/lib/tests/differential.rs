//! Differential tests: production `scan` and `StreamScanner` against a frozen
//! reference implementation of the sparse-gram hull.
//!
//! The reference is the original three-pass monotonic-stack algorithm
//! (drain-emit, top-emit, dedup, capped push), written for clarity and kept
//! independent of the production code. Any optimization of the production
//! scanner must reproduce the reference's emission sequence byte for byte,
//! for every table and input below — including plateau-heavy tables, runs
//! longer than `MAX_LEN`, and inputs that overflow the bounded stack.
#![allow(missing_docs, clippy::unwrap_used, clippy::indexing_slicing)]

use sngram::StreamScanner;
use sngram_types::{Content, TABLE_BINARY_SIZE, WeightTable};

// Frozen algorithm parameters; must mirror crates/lib/src/extract.rs.
const MIN_LEN: usize = 3;
const MAX_LEN: usize = 100;
const STACK_CAP: usize = 128;

/// Reference hull: emit every (start, end) where the border pairs outweigh
/// all interior pairs, lengths within MIN_LEN..=MAX_LEN, with the same
/// bounded-stack eviction as production.
fn reference_scan(table: &WeightTable, content: &[u8], emit: &mut impl FnMut(usize, usize)) {
    if content.len() < MIN_LEN {
        return;
    }
    let try_emit = |start: usize, end: usize, emit: &mut dyn FnMut(usize, usize)| {
        if (MIN_LEN..=MAX_LEN).contains(&(end - start)) {
            emit(start, end);
        }
    };
    let mut stack: Vec<(usize, u32)> = Vec::new();
    for i in 0..content.len() - 1 {
        let w = table.weight(content[i], content[i + 1]);
        let end = i + 2;
        // drain: pop strictly lighter entries, emitting each span
        while let Some(&(start, sw)) = stack.last() {
            if sw >= w {
                break;
            }
            stack.pop();
            try_emit(start, end, emit);
        }
        // top: the surviving top also spans a hull gram ending here
        if let Some(&(start, _)) = stack.last() {
            try_emit(start, end, emit);
        }
        // dedup: equal weights collapse into the new entry
        while let Some(&(_, sw)) = stack.last() {
            if sw != w {
                break;
            }
            stack.pop();
        }
        // capped push: the oldest entry is beyond MAX_LEN once depth hits cap
        if stack.len() == STACK_CAP {
            stack.remove(0);
        }
        stack.push((i, w));
    }
}

/// Deterministic LCG (Knuth MMIX constants) so failures reproduce exactly.
struct Lcg(u64);

impl Lcg {
    fn next_u32(&mut self) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) as u32
    }
}

fn build_table(f: impl Fn(u8, u8) -> u32) -> WeightTable {
    let mut buf = vec![0u8; TABLE_BINARY_SIZE];
    buf[..4].copy_from_slice(b"SPNG");
    buf[4..8].copy_from_slice(&1u32.to_le_bytes());
    for c1 in 0u16..256 {
        for c2 in 0u16..256 {
            let idx = (c1 as usize) << 8 | c2 as usize;
            let off = 16 + idx * 4;
            #[allow(clippy::cast_possible_truncation)]
            buf[off..off + 4].copy_from_slice(&f(c1 as u8, c2 as u8).to_le_bytes());
        }
    }
    let crc = crc32fast::hash(&buf[16..]);
    buf[8..12].copy_from_slice(&crc.to_le_bytes());
    WeightTable::from_bytes(&buf).unwrap()
}

fn tables() -> Vec<(String, WeightTable)> {
    let mut out = vec![(
        "crc32".to_owned(),
        build_table(|a, b| crc32fast::hash(&[a, b])),
    )];
    for seed in [1u64, 7, 42] {
        out.push((
            format!("rand{seed}"),
            build_table(|a, b| {
                let mut r =
                    Lcg(seed ^ (u64::from(a) << 32) ^ (u64::from(b) << 16) ^ 0x9E37_79B9_7F4A_7C15);
                r.next_u32();
                r.next_u32()
            }),
        ));
    }
    // heavy plateaus: only 4 distinct weights
    out.push((
        "plateau4".to_owned(),
        build_table(|a, b| crc32fast::hash(&[a, b]) % 4),
    ));
    // extreme plateaus: only 2 distinct weights
    out.push((
        "plateau2".to_owned(),
        build_table(|a, b| crc32fast::hash(&[a, b]) % 2),
    ));
    // constant table: every weight equal — pure plateau
    out.push(("const".to_owned(), build_table(|_, _| 7)));
    // strictly decreasing weights along an ascending byte run: the scan stack
    // grows past STACK_CAP and forces eviction
    out.push((
        "monotonic".to_owned(),
        build_table(|a, b| {
            if b == a.wrapping_add(1) {
                1_000_000 - u32::from(a)
            } else {
                u32::from(a) ^ u32::from(b)
            }
        }),
    ));
    out
}

fn inputs() -> Vec<(String, Vec<u8>)> {
    let mut out: Vec<(String, Vec<u8>)> = Vec::new();
    let mut rng = Lcg(0xDEAD_BEEF);
    // random byte strings, lengths 0..200 (covers MIN_LEN edges densely)
    for len in 0..200usize {
        out.push((
            format!("rand_{len}"),
            (0..len).map(|_| rng.next_u32() as u8).collect(),
        ));
    }
    // longer random strings
    for len in [501usize, 999, 2000] {
        out.push((
            format!("rand_{len}"),
            (0..len).map(|_| rng.next_u32() as u8).collect(),
        ));
    }
    // small-alphabet random (drives plateaus + deep pops)
    for len in [300usize, 2000] {
        out.push((
            format!("alpha4_{len}"),
            (0..len).map(|_| b'a' + (rng.next_u32() % 4) as u8).collect(),
        ));
    }
    // uniform bytes, runs longer than MAX_LEN
    out.push(("uniform_300".to_owned(), vec![b'a'; 300]));
    // two long runs
    let mut runs = vec![b'a'; 150];
    runs.extend(vec![b'b'; 150]);
    out.push(("runs_150_150".to_owned(), runs));
    // strictly ascending bytes (with the monotonic table: stack overflow)
    out.push((
        "ascending_1000".to_owned(),
        (0..1000usize).map(|i| (i % 256) as u8).collect(),
    ));
    out.push(("ascending_1_200".to_owned(), (1u8..=200).collect()));
    // plateau-heavy periodic content
    out.push(("abab_400".to_owned(), b"ab".repeat(200)));
    out.push(("aabb_400".to_owned(), b"aabb".repeat(100)));
    // code-like
    let src = b"fn main() { let x = foo_bar(42); println!(\"{x}\"); }\n";
    out.push((
        "code_2000".to_owned(),
        (0..2000usize).map(|i| src[i % src.len()]).collect(),
    ));
    out
}

/// Independent recomputation of the rolling gram hash: fold the gram's bytes
/// through the published recurrence, then the splitmix64 finalizer. Mirrors
/// the scheme in `src/hashing.rs` without sharing any code with it.
fn direct_hash(bytes: &[u8]) -> u64 {
    const BASE: u64 = 0x9E37_79B9_7F4A_7C15;
    // seed 1: the implicit leading sentinel that disambiguates gram length
    let mut h = 1u64;
    for &b in bytes {
        h = h.wrapping_mul(BASE).wrapping_add(u64::from(b));
    }
    let mut z = h;
    z ^= z >> 30;
    z = z.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z ^= z >> 27;
    z = z.wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    z
}

#[test]
fn scan_matches_reference_exactly() {
    let mut cases = 0usize;
    for (tname, table) in tables() {
        for (iname, content) in inputs() {
            let mut expected = Vec::new();
            reference_scan(&table, &content, &mut |s, e| {
                expected.push((s, e, direct_hash(&content[s..e])));
            });
            let mut got = Vec::new();
            sngram::scan(&table, &Content::new(&content), |s, e, h| got.push((s, e, h)));
            assert_eq!(got, expected, "scan diverged on table={tname} input={iname}");
            cases += 1;
        }
    }
    eprintln!("scan differential cases passed: {cases}");
}

/// Chunk `content` into pieces whose sizes are drawn from `sizes` round-robin.
fn chunkings(content: &[u8], rng: &mut Lcg) -> Vec<(String, Vec<Vec<u8>>)> {
    let mut out = Vec::new();
    for &fixed in &[1usize, 7, 64, 4096] {
        out.push((
            format!("fixed_{fixed}"),
            content.chunks(fixed).map(<[u8]>::to_vec).collect(),
        ));
    }
    out.push(("whole".to_owned(), vec![content.to_vec()]));
    // three random chunkings with sizes in 1..=97
    for round in 0..3 {
        let mut chunks = Vec::new();
        let mut pos = 0;
        while pos < content.len() {
            let take = (rng.next_u32() as usize % 97 + 1).min(content.len() - pos);
            chunks.push(content[pos..pos + take].to_vec());
            pos += take;
        }
        out.push((format!("random_{round}"), chunks));
    }
    out
}

#[test]
fn stream_scanner_matches_reference_under_any_chunking() {
    let mut rng = Lcg(0xC0FF_EE00);
    let mut cases = 0usize;
    for (tname, table) in tables() {
        // a representative input subset: streaming is O(per-byte), the full
        // matrix above would make this test slow without adding coverage
        for (iname, content) in inputs().into_iter().filter(|(_, c)| c.len() >= 50) {
            let mut expected: Vec<(Vec<u8>, u64)> = Vec::new();
            reference_scan(&table, &content, &mut |s, e| {
                let bytes = content[s..e].to_vec();
                let h = direct_hash(&bytes);
                expected.push((bytes, h));
            });
            for (cname, chunks) in chunkings(&content, &mut rng) {
                let mut got: Vec<(Vec<u8>, u64)> = Vec::new();
                let mut scanner = StreamScanner::new(&table);
                for chunk in &chunks {
                    scanner.push(chunk, |g, h| got.push((g.to_vec(), h)));
                }
                scanner.finish();
                assert_eq!(
                    got, expected,
                    "stream diverged on table={tname} input={iname} chunking={cname}"
                );
                cases += 1;
            }
        }
    }
    eprintln!("stream differential cases passed: {cases}");
}

#[test]
fn stream_scanner_resets_between_documents() {
    let table = build_table(|a, b| crc32fast::hash(&[a, b]));
    let doc = b"fn main() { let x = foo_bar(42); }";

    let mut first: Vec<(Vec<u8>, u64)> = Vec::new();
    let mut scanner = StreamScanner::new(&table);
    scanner.push(doc, |g, h| first.push((g.to_vec(), h)));
    scanner.finish();

    let mut second: Vec<(Vec<u8>, u64)> = Vec::new();
    scanner.push(doc, |g, h| second.push((g.to_vec(), h)));
    scanner.finish();

    assert_eq!(first, second, "finish() must fully reset scanner state");
}

#[test]
fn eviction_path_is_exercised() {
    // sanity: the monotonic table + ascending input must overflow the stack,
    // so the differential suite genuinely covers the eviction branch
    let table = build_table(|a, b| {
        if b == a.wrapping_add(1) {
            1_000_000 - u32::from(a)
        } else {
            u32::from(a) ^ u32::from(b)
        }
    });
    let content: Vec<u8> = (0u8..=200).collect();
    let mut depth = 0usize;
    let mut max_depth = 0usize;
    let mut stack: Vec<(usize, u32)> = Vec::new();
    for i in 0..content.len() - 1 {
        let w = table.weight(content[i], content[i + 1]);
        while let Some(&(_, sw)) = stack.last() {
            if sw > w {
                break;
            }
            stack.pop();
            if sw == w {
                break;
            }
        }
        stack.push((i, w));
        depth = stack.len();
        max_depth = max_depth.max(depth);
    }
    assert!(
        max_depth > STACK_CAP,
        "test table must overflow the {STACK_CAP}-entry stack (got {max_depth})"
    );
}
