//! Structured benchmark reporting for indexed search.

use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    time::Instant,
};

use serde::{Deserialize, Serialize};

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

    pub fn set_tuned_query_grams(&mut self, grams: usize) {
        self.counts.tuned_query_grams = grams as u64;
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

    pub fn set_forced_candidate_files(&mut self, forced: u64) {
        self.counts.forced_candidate_files = forced;
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
            .saturating_add(self.bytes.summary_bytes)
            .saturating_add(self.bytes.manifest_bytes);
    }

    pub fn set_corpus_text_bytes(&mut self, corpus_text_bytes: u64) {
        self.bytes.corpus_text_bytes = corpus_text_bytes;
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
    request_validate: f64,
    parse_request: f64,
    plan_query: f64,
    resolve_root: f64,
    catalog_probe: f64,
    daemon_register: f64,
    daemon_start: f64,
    cold_build_total: f64,
    daemon_ready: f64,
    daemon_proof: f64,
    manifest_open: f64,
    walk_collect: f64,
    snapshot_build: f64,
    scan_documents: f64,
    write_postings: f64,
    write_summary: f64,
    write_manifest: f64,
    publish_generation: f64,
    index_open: f64,
    index_tune: f64,
    index_execute: f64,
    index_lookup: f64,
    candidate_restrict: f64,
    verify_haystacks: f64,
    total: f64,
}

impl Timings {
    pub fn set_parse_request(&mut self, started: Instant) {
        let elapsed = elapsed_ms(started);
        self.request_validate = elapsed;
        self.parse_request = elapsed;
    }

    pub fn set_plan_query(&mut self, started: Instant) {
        self.plan_query = elapsed_ms(started);
    }

    pub fn set_resolve_root(&mut self, started: Instant) {
        self.resolve_root = elapsed_ms(started);
    }

    pub fn set_catalog_probe(&mut self, started: Instant) {
        self.catalog_probe = elapsed_ms(started);
    }

    pub fn set_daemon_register(&mut self, started: Instant) {
        self.daemon_register = elapsed_ms(started);
    }

    pub fn set_daemon_start(&mut self, started: Instant) {
        self.daemon_start = elapsed_ms(started);
    }

    pub fn set_cold_build_total(&mut self, started: Instant) {
        self.cold_build_total = elapsed_ms(started);
    }

    pub fn set_daemon_ready(&mut self, started: Instant) {
        self.daemon_ready = elapsed_ms(started);
    }

    pub fn set_daemon_proof(&mut self, started: Instant) {
        self.daemon_proof = elapsed_ms(started);
    }

    pub fn set_manifest_open(&mut self, started: Instant) {
        self.manifest_open = elapsed_ms(started);
    }

    pub fn set_index_open(&mut self, started: Instant) {
        self.index_open = elapsed_ms(started);
    }

    pub fn set_index_tune(&mut self, started: Instant) {
        self.index_tune = elapsed_ms(started);
    }

    pub fn set_index_execute(&mut self, started: Instant) {
        self.index_execute = elapsed_ms(started);
    }

    pub fn set_index_lookup(&mut self, started: Instant) {
        self.index_lookup = elapsed_ms(started);
    }

    pub fn set_candidate_restrict(&mut self, started: Instant) {
        self.candidate_restrict = elapsed_ms(started);
    }

    pub fn set_verify_haystacks(&mut self, started: Instant) {
        self.verify_haystacks = elapsed_ms(started);
    }

    pub fn set_total(&mut self, started: Instant) {
        self.total = elapsed_ms(started);
    }

    pub fn merge_build(&mut self, build: &BuildTimings) {
        self.walk_collect = build.walk_collect;
        self.snapshot_build = build.snapshot_build;
        self.scan_documents = build.scan_documents;
        self.write_postings = build.write_postings;
        self.write_summary = build.write_summary;
        self.write_manifest = build.write_manifest;
        self.publish_generation = build.publish_generation;
    }
}

#[derive(Default, Serialize, Deserialize)]
pub struct BuildTimings {
    walk_collect: f64,
    snapshot_build: f64,
    scan_documents: f64,
    write_postings: f64,
    write_summary: f64,
    write_manifest: f64,
    publish_generation: f64,
}

impl BuildTimings {
    pub fn set_walk_collect(&mut self, started: Instant) {
        self.walk_collect = elapsed_ms(started);
    }

    pub fn set_snapshot_build(&mut self, started: Instant) {
        self.snapshot_build = elapsed_ms(started);
    }

    pub fn set_scan_documents(&mut self, started: Instant) {
        self.scan_documents = elapsed_ms(started);
    }

    pub fn set_write_postings(&mut self, started: Instant) {
        self.write_postings = elapsed_ms(started);
    }

    pub fn set_write_summary(&mut self, started: Instant) {
        self.write_summary = elapsed_ms(started);
    }

    pub fn set_write_manifest(&mut self, started: Instant) {
        self.write_manifest = elapsed_ms(started);
    }

    pub fn set_publish_generation(&mut self, started: Instant) {
        self.publish_generation = elapsed_ms(started);
    }

    pub fn absorb_backend(&mut self, backend: BuildTimings) {
        self.scan_documents = backend.scan_documents;
        self.write_postings = backend.write_postings;
        self.write_summary = backend.write_summary;
        self.write_manifest = backend.write_manifest;
        self.publish_generation = backend.publish_generation;
    }
}

pub fn clear_build_timings(state_root: &Path) {
    let _ = fs::remove_file(build_timings_path(state_root));
}

pub fn write_build_timings(state_root: &Path, timings: &BuildTimings) -> anyhow::Result<()> {
    let path = build_timings_path(state_root);
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("build timing path has no parent"))?;
    fs::create_dir_all(parent)?;
    let mut file = fs::File::create(&path)?;
    serde_json::to_writer(&mut file, timings)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

pub fn read_build_timings(state_root: &Path) -> anyhow::Result<Option<BuildTimings>> {
    let path = build_timings_path(state_root);
    let file = match fs::File::open(&path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    Ok(Some(serde_json::from_reader(file)?))
}

fn build_timings_path(state_root: &Path) -> PathBuf {
    state_root.join("runtime").join("build-bench.json")
}

#[derive(Default, Serialize)]
struct Counts {
    total_manifest_files: u64,
    text_files: u64,
    binary_skipped_files: u64,
    query_grams: u64,
    tuned_query_grams: u64,
    candidate_files: u64,
    verified_files: u64,
    matched_files: u64,
    forced_candidate_files: u64,
    parent_restricted_candidates: u64,
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
    corpus_text_bytes: u64,
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
