//! Sparse n-gram index integration.

mod backend;
mod bench;
mod bench_suite;
mod catalog;
mod classify;
mod config;
mod daemon_refresh;
mod document;
mod executor;
mod generation;
mod initial;
mod location;
mod maintenance;
mod manifest;
mod planner;
mod postings;
mod request;
mod roots;
mod runtime;
mod snapshot;
mod store;
mod summary;
mod verify;
mod walk;

use std::{
    collections::BTreeSet,
    fmt::{self, Write as FmtWrite},
    path::Path,
    time::Instant,
};

use anyhow::bail;
use catalog::GenerationCatalog;
use generation::Generation;
use initial::InitialBuild;
use request::{SearchRequest, Unsupported, unsupported};
use roots::{SearchRoots, absolute_path};
use sngram_types::QueryPlan;

use crate::flags::HiArgs;

pub use config::IndexConfig;

/// Run an indexed search.
pub fn run(args: &HiArgs) -> anyhow::Result<bool> {
    IndexedSearch::from_args(args)?.run()
}

struct IndexedSearch<'a> {
    args: &'a HiArgs,
}

impl<'a> IndexedSearch<'a> {
    fn from_args(args: &'a HiArgs) -> anyhow::Result<Self> {
        Ok(Self { args })
    }

    fn run(self) -> anyhow::Result<bool> {
        if runtime::is_daemon_refresh() {
            daemon_refresh::run(self.args)?;
            return Ok(true);
        }
        if self.args.index().bench_suite() {
            return bench_suite::run(self.args);
        }
        if self.args.index().bench() {
            return run_bench(self.args);
        }
        run_inner(self.args, None)
    }
}

fn run_bench(args: &HiArgs) -> anyhow::Result<bool> {
    let total_started_at = Instant::now();
    let mut report = bench::BenchReport::new(args);
    let result = run_inner(args, Some(&mut report));
    report.timing_mut().set_total(total_started_at);
    match result {
        Ok(matched) => {
            report.finish_ok(matched);
            report.print()?;
            Ok(matched)
        },
        Err(err) => {
            report.finish_error(&err);
            report.print()?;
            Err(err)
        },
    }
}

fn run_inner(args: &HiArgs, mut bench: Option<&mut bench::BenchReport>) -> anyhow::Result<bool> {
    let parse_started_at = Instant::now();
    let request = if args.index().is_maintenance() {
        return maintenance::run(args);
    } else {
        SearchRequest::from_args(args)
    };
    if let Some(report) = bench.as_deref_mut() {
        report.timing_mut().set_parse_request(parse_started_at);
    }
    let request = request?;
    if !request.matches_possible() {
        return Ok(false);
    }
    let args = request.args();
    let mode = request.mode();
    let started_at = request.started_at();
    log::debug!(
        "eg index: mode={:?} backend={:?} threads={}",
        args.index().mode(),
        args.index().backend(),
        args.threads()
    );
    if !matches!(args.index().backend(), config::IndexBackend::Postings) {
        log::debug!(
            "eg index: the {:?} backend is experimental and unsupported; prefer --index-backend postings",
            args.index().backend()
        );
    }
    let table = sngram_weights::weights();
    let table_fingerprint = table.fingerprint();
    let plan_started_at = Instant::now();
    let plan = match planner::QueryPlanner::new(args, &table).plan() {
        Ok(plan) => plan,
        Err(planner::PlanError::InvalidRegex(err)) => bail!("{err}"),
        Err(planner::PlanError::Unsupported(reason)) => {
            if matches!(reason, Unsupported::TooBroadPattern)
                && let Some(report) = bench.as_deref_mut()
            {
                report.reject_too_broad();
            }
            return unsupported(reason);
        },
    };
    if let Some(report) = bench.as_deref_mut() {
        report.timing_mut().set_plan_query(plan_started_at);
        report.set_query_grams(plan.plan.gram_count());
    }
    log::debug!("eg index: query plan: {}", debug_plan(&plan.plan));
    let roots_started_at = Instant::now();
    let roots = SearchRoots::from_request(&request)?;
    debug_assert!(roots.is_served_by(roots.build_root()));
    let catalog_started_at = Instant::now();
    let catalog = GenerationCatalog::open(args, table_fingerprint);
    let generation = Generation::from_ready(catalog.best_ready_generation(&roots)?);
    let index_dir = generation.index_dir();
    if let Some(report) = bench.as_deref_mut() {
        report.timing_mut().set_resolve_roots(roots_started_at);
        report.timing_mut().set_catalog_open(catalog_started_at);
        report.set_index_root(generation.index_root(), generation.used_parent_index());
    }
    let cold_build = generation.is_cold_build(args, &index_dir);
    if let Some(report) = bench.as_deref_mut() {
        report.set_cold_build(cold_build);
        report.set_generation(
            index_dir.display().to_string(),
            generation.bench_source(args, cold_build),
        );
    }
    let validate_started_at = Instant::now();
    let (snapshot, loaded_manifest, freshness_proof) =
        snapshot::SnapshotLoader::new(args, table_fingerprint, generation.location(), &index_dir)
            .load()?
            .into_parts();
    if let Some(report) = bench.as_deref_mut() {
        report
            .timing_mut()
            .set_generation_validate(validate_started_at);
        let binary_skipped = snapshot
            .files
            .iter()
            .filter(|file| file.is_skipped_binary())
            .count();
        report.set_generation(
            index_dir.display().to_string(),
            snapshot::generation_source(
                args,
                table_fingerprint,
                &generation,
                &snapshot,
                loaded_manifest.as_ref(),
                cold_build,
            ),
        );
        report.set_snapshot_counts(snapshot.files.len(), binary_skipped);
        report.set_freshness_proof(freshness_proof);
    }
    if args.has_implicit_path() && snapshot.files.is_empty() {
        crate::eprint_nothing_searched();
    }
    warn_large_implicit_build(args, generation.index_root(), &index_dir, &snapshot);
    let prebuilt_disk_index = if cold_build {
        let build_started_at = Instant::now();
        let status =
            InitialBuild::new(args, table_fingerprint, &table, &index_dir).run(&snapshot)?;
        if let Some(report) = bench.as_deref_mut() {
            report.timing_mut().set_initial_build(build_started_at);
        }
        status.prebuilt_disk_index()
    } else {
        false
    };
    runtime::Lease::new(generation.index_root(), generation.state_root()).refresh_best_effort();
    let query_started_at = Instant::now();
    let Some(mut candidates) = generation.query(
        args,
        table_fingerprint,
        &table,
        &snapshot,
        loaded_manifest.as_ref(),
        &plan,
        prebuilt_disk_index,
        bench.as_deref_mut(),
    )?
    else {
        if let Some(report) = bench.as_deref_mut() {
            report.reject_selectivity();
        }
        return unsupported(Unsupported::TooManyCandidates);
    };
    let unrestricted_candidates = candidates.len();
    restrict_candidates(args, &roots, &snapshot, &mut candidates);
    if let Some(report) = bench.as_deref_mut() {
        report.timing_mut().set_index_query(query_started_at);
        report.set_candidates(candidates.len());
        report.set_parent_restricted_candidates(unrestricted_candidates - candidates.len());
    }
    let matched =
        verify::CandidateVerifier::new(args, mode, started_at, &snapshot, &candidates, bench)
            .verify()?;
    Ok(matched)
}

/// File count above which a first-time implicit build gets a warning.
const GUARDRAIL_FILES: usize = 100_000;

/// Warn once, before a first-time index build over an implicit, very large
/// tree or the home directory, since that silently indexes everything.
fn warn_large_implicit_build(
    args: &HiArgs,
    corpus_root: &Path,
    index_dir: &Path,
    snapshot: &manifest::CurrentSnapshot,
) {
    if !args.has_implicit_path() || generation::index_present(args, index_dir) {
        return;
    }
    if snapshot.files.len() <= GUARDRAIL_FILES && !is_home_dir(corpus_root) {
        return;
    }
    message!(
        "indexing {} files under {} on first use; pass --no-index to skip the index \
         or --index-dir DIR to store it elsewhere",
        snapshot.files.len(),
        corpus_root.display()
    );
}

/// Return true when the path resolves to the user's home directory.
fn is_home_dir(path: &Path) -> bool {
    let Some(home) = std::env::var_os("HOME") else {
        return false;
    };
    let canon = |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    canon(path) == canon(&std::path::PathBuf::from(home))
}

const DEBUG_PLAN_PREVIEW_BYTES: usize = 4096;

fn debug_plan(plan: &QueryPlan) -> String {
    let mut preview = PlanPreview::new(DEBUG_PLAN_PREVIEW_BYTES);
    let _ = write!(&mut preview, "{plan}");
    if preview.truncated {
        return format!(
            "{}... [truncated plan: preview_bytes={} grams={}]",
            preview.buf,
            preview.buf.len(),
            plan_gram_count(plan)
        );
    }
    preview.buf
}

struct PlanPreview {
    buf: String,
    limit: usize,
    truncated: bool,
}

impl PlanPreview {
    fn new(limit: usize) -> Self {
        Self {
            buf: String::with_capacity(limit),
            limit,
            truncated: false,
        }
    }
}

impl fmt::Write for PlanPreview {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let remaining = self.limit.saturating_sub(self.buf.len());
        if s.len() <= remaining {
            self.buf.push_str(s);
            return Ok(());
        }
        let mut end = remaining;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        self.buf.push_str(&s[..end]);
        self.truncated = true;
        Err(fmt::Error)
    }
}

fn plan_gram_count(plan: &QueryPlan) -> usize {
    plan.gram_count()
}

fn restrict_candidates(
    args: &HiArgs,
    roots: &SearchRoots,
    snapshot: &manifest::CurrentSnapshot,
    candidates: &mut BTreeSet<usize>,
) {
    candidates.retain(|ord| {
        snapshot
            .files
            .get(*ord)
            .is_some_and(|file| roots.contains(args.cwd(), &file.path))
    });
}

#[cfg(test)]
mod tests {
    use sngram_types::{QueryPlan, WeightTable};

    use super::{DEBUG_PLAN_PREVIEW_BYTES, debug_plan};

    fn table() -> WeightTable {
        WeightTable::from_weight_fn(|c1, c2| {
            u32::from(c1).wrapping_mul(257).wrapping_add(u32::from(c2))
        })
    }

    fn plan(pattern: &str) -> QueryPlan {
        sngram::query(&table(), pattern).expect("pattern plans")
    }

    #[test]
    fn debug_plan_keeps_small_plans_verbatim() {
        let plan = plan("needle_value");

        assert_eq!(debug_plan(&plan), plan.to_string());
    }

    #[test]
    fn debug_plan_truncates_large_plans_at_utf8_boundary() {
        let pattern = (0..300)
            .map(|i| format!("gram_{i:03}_µ"))
            .collect::<Vec<_>>()
            .join("|");
        let plan = plan(&pattern);
        let rendered = debug_plan(&plan);

        assert!(rendered.len() > DEBUG_PLAN_PREVIEW_BYTES);
        assert!(rendered.contains("[truncated plan:"));
        assert!(rendered.contains("grams="));
    }
}
