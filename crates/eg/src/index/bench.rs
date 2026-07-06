//! Structured benchmark reporting for indexed search.

use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    time::Instant,
};

use serde::Serialize;

use crate::flags::HiArgs;

#[derive(Serialize)]
pub struct BenchReport {
    ok: bool,
    matched: bool,
    mode: &'static str,
    backend: &'static str,
    search_roots: Vec<String>,
    index_root: String,
    generation_id: String,
    used_parent_index: bool,
    cold_build: bool,
    timings_ms: Timings,
    counts: Counts,
    false_positives: FalsePositives,
    bytes: Bytes,
    generation_source: &'static str,
    freshness_proof: &'static str,
    selectivity_rejected: bool,
    query_too_broad: bool,
    unsupported_reason: Option<String>,
}

impl BenchReport {
    pub fn new(args: &HiArgs) -> Self {
        Self {
            ok: false,
            matched: false,
            mode: args.index().mode_name(),
            backend: args.index().backend_name(),
            search_roots: args
                .search_paths()
                .iter()
                .map(|path| path.display().to_string())
                .collect(),
            index_root: String::new(),
            generation_id: String::new(),
            used_parent_index: false,
            cold_build: false,
            timings_ms: Timings::default(),
            counts: Counts::default(),
            false_positives: FalsePositives::default(),
            bytes: Bytes::default(),
            generation_source: "missing",
            freshness_proof: "missing",
            selectivity_rejected: false,
            query_too_broad: false,
            unsupported_reason: None,
        }
    }

    pub fn set_index_root(&mut self, root: &Path, used_parent: bool) {
        self.index_root = root.display().to_string();
        self.used_parent_index = used_parent;
    }

    pub fn set_generation(&mut self, generation_id: impl Into<String>, source: &'static str) {
        self.generation_id = generation_id.into();
        self.generation_source = source;
    }

    pub fn set_freshness_proof(&mut self, proof: &'static str) {
        self.freshness_proof = proof;
    }

    pub fn set_cold_build(&mut self, cold_build: bool) {
        self.cold_build = cold_build;
    }

    pub fn set_query_grams(&mut self, grams: usize) {
        self.counts.query_grams = grams as u64;
    }

    pub fn set_snapshot_counts(&mut self, total: usize, binary_skipped: usize) {
        self.counts.total_manifest_files = total as u64;
        self.counts.binary_skipped_files = binary_skipped as u64;
        self.counts.text_files = total.saturating_sub(binary_skipped) as u64;
    }

    pub fn set_candidates(&mut self, candidate_files: usize) {
        self.counts.candidate_files = candidate_files as u64;
        self.false_positives.candidate_files = candidate_files as u64;
    }

    pub fn set_parent_restricted_candidates(&mut self, restricted: usize) {
        self.counts.parent_restricted_candidates = restricted as u64;
    }

    pub fn set_verification(&mut self, verified: usize, matched: usize, bytes_verified: u64) {
        self.counts.verified_files = verified as u64;
        self.counts.matched_files = matched as u64;
        self.bytes.bytes_verified = bytes_verified;
        self.false_positives.matched_files = matched as u64;
        self.false_positives.false_positive_files = verified.saturating_sub(matched) as u64;
        self.false_positives.false_positive_pct =
            percentage(self.false_positives.false_positive_files, verified as u64);
        self.false_positives.candidate_pct_of_text_files =
            percentage(self.counts.candidate_files, self.counts.text_files);
    }

    pub fn set_index_bytes(&mut self, index_home: &Path) {
        self.bytes.index_table_bytes = file_len(index_home.join("table.bin"));
        self.bytes.index_postings_bytes = file_len(index_home.join("postings.bin"));
        self.bytes.summary_bytes = file_len(index_home.join("summaries.bin"));
        self.bytes.manifest_bytes = file_len(index_home.join("manifest.bin"))
            .saturating_add(file_len(index_home.join("manifest.json")));
        self.bytes.mmap_bytes = self
            .bytes
            .index_table_bytes
            .saturating_add(self.bytes.index_postings_bytes)
            .saturating_add(file_len(index_home.join("delta-table.bin")))
            .saturating_add(file_len(index_home.join("delta-postings.bin")));
    }

    pub fn reject_selectivity(&mut self) {
        self.selectivity_rejected = true;
    }

    pub fn reject_too_broad(&mut self) {
        self.query_too_broad = true;
    }

    pub fn finish_ok(&mut self, matched: bool) {
        self.ok = true;
        self.matched = matched;
    }

    pub fn finish_error(&mut self, err: &anyhow::Error) {
        self.ok = false;
        self.unsupported_reason = Some(err.to_string());
    }

    pub fn print(&self) -> anyhow::Result<()> {
        let stdout = io::stdout();
        let mut lock = stdout.lock();
        serde_json::to_writer_pretty(&mut lock, self)?;
        lock.write_all(b"\n")?;
        Ok(())
    }

    pub fn timing_mut(&mut self) -> &mut Timings {
        &mut self.timings_ms
    }
}

#[derive(Default, Serialize)]
pub struct Timings {
    parse_request: f64,
    plan_query: f64,
    resolve_roots: f64,
    catalog_open: f64,
    generation_validate: f64,
    initial_build: f64,
    index_mmap: f64,
    index_query: f64,
    verify_candidates: f64,
    total: f64,
}

impl Timings {
    pub fn set_parse_request(&mut self, started: Instant) {
        self.parse_request = elapsed_ms(started);
    }

    pub fn set_plan_query(&mut self, started: Instant) {
        self.plan_query = elapsed_ms(started);
    }

    pub fn set_resolve_roots(&mut self, started: Instant) {
        self.resolve_roots = elapsed_ms(started);
    }

    pub fn set_catalog_open(&mut self, started: Instant) {
        self.catalog_open = elapsed_ms(started);
    }

    pub fn set_generation_validate(&mut self, started: Instant) {
        self.generation_validate = elapsed_ms(started);
    }

    pub fn set_initial_build(&mut self, started: Instant) {
        self.initial_build = elapsed_ms(started);
    }

    pub fn set_index_mmap(&mut self, started: Instant) {
        self.index_mmap = elapsed_ms(started);
    }

    pub fn set_index_query(&mut self, started: Instant) {
        self.index_query = elapsed_ms(started);
    }

    pub fn set_verify_candidates(&mut self, started: Instant) {
        self.verify_candidates = elapsed_ms(started);
    }

    pub fn set_total(&mut self, started: Instant) {
        self.total = elapsed_ms(started);
    }
}

#[derive(Default, Serialize)]
struct Counts {
    total_manifest_files: u64,
    text_files: u64,
    binary_skipped_files: u64,
    query_grams: u64,
    candidate_files: u64,
    verified_files: u64,
    matched_files: u64,
    forced_candidate_files: u64,
    parent_restricted_candidates: u64,
    dirty_forced_candidates: u64,
}

#[derive(Default, Serialize)]
struct FalsePositives {
    candidate_files: u64,
    matched_files: u64,
    false_positive_files: u64,
    false_positive_pct: f64,
    candidate_pct_of_text_files: f64,
}

#[derive(Default, Serialize)]
struct Bytes {
    index_table_bytes: u64,
    index_postings_bytes: u64,
    summary_bytes: u64,
    manifest_bytes: u64,
    mmap_bytes: u64,
    bytes_verified: u64,
}

fn elapsed_ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

fn file_len(path: PathBuf) -> u64 {
    fs::metadata(path).map_or(0, |metadata| metadata.len())
}

fn percentage(part: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        (part as f64 / total as f64) * 100.0
    }
}

#[cfg(test)]
mod tests {
    use super::BenchReport;

    #[test]
    fn false_positive_stats_are_totaled_from_verified_matches() {
        let mut report = BenchReport {
            ok: false,
            matched: false,
            mode: "auto",
            backend: "postings",
            search_roots: Vec::new(),
            index_root: String::new(),
            generation_id: String::new(),
            used_parent_index: false,
            cold_build: false,
            timings_ms: Default::default(),
            counts: Default::default(),
            false_positives: Default::default(),
            bytes: Default::default(),
            generation_source: "hot",
            freshness_proof: "daemon",
            selectivity_rejected: false,
            query_too_broad: false,
            unsupported_reason: None,
        };
        report.set_snapshot_counts(100, 10);
        report.set_candidates(20);
        report.set_verification(20, 5, 1234);

        assert_eq!(15, report.false_positives.false_positive_files);
        assert_eq!(75.0, report.false_positives.false_positive_pct);
        assert_eq!(
            100.0 * 20.0 / 90.0,
            report.false_positives.candidate_pct_of_text_files
        );
    }
}
