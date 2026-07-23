//! Sparse n-gram index integration.

mod backend;
mod bench;
mod catalog;
mod classify;
mod config;
mod daemon_refresh;
mod document;
mod executor;
mod generation;
mod huffman;
mod location;
mod manifest;
mod planner;
mod postings;
mod progress;
mod request;
mod roots;
mod runtime;
mod snapshot;
mod store;
mod suite;
mod summary;
mod verify;
mod walk;

use std::{
    collections::BTreeSet,
    fmt::{self, Write as FmtWrite},
    time::Duration,
    time::Instant,
};

use anyhow::bail;
use catalog::GenerationCatalog;
use generation::Generation;
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
        if self.args.index().bench()
            && self.args.patterns().is_empty()
            && matches!(self.args.mode(), crate::flags::Mode::Search(_))
        {
            return suite::run(self.args);
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
            if std::env::var_os(suite::NO_COMPARE_ENV).is_none() {
                let (scan_wall, rg_wall) = suite::compare_unindexed()?;
                report.set_comparison(scan_wall, rg_wall);
            }
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
    let request = SearchRequest::from_args(args);
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
        "eg index: mode={} backend={:?} threads={}",
        args.index().mode_name(),
        args.index().backend(),
        args.threads()
    );
    if !matches!(args.index().backend(), config::IndexBackend::Postings) {
        log::debug!(
            "eg index: the {:?} backend is experimental and unsupported; prefer --index-backend postings",
            args.index().backend()
        );
    }
    let table = sngram::weights();
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
    if let Some(report) = bench.as_deref_mut() {
        report.timing_mut().set_resolve_root(roots_started_at);
    }
    debug_assert!(roots.is_served_by(roots.build_root()));
    let catalog_started_at = Instant::now();
    let catalog = GenerationCatalog::open(args, table_fingerprint);
    let generation = Generation::from_ready(catalog.best_ready_generation(&roots)?);
    let index_dir = generation.index_dir();
    if let Some(report) = bench.as_deref_mut() {
        report.timing_mut().set_catalog_probe(catalog_started_at);
        report.set_index_root(generation.index_root(), generation.used_parent_index());
    }
    let cold_build = generation.source() != "hot";
    if let Some(report) = bench.as_deref_mut() {
        report.set_cold_build(cold_build);
        report.set_generation(
            index_dir.display().to_string(),
            if cold_build { "cold_build" } else { "hot" },
        );
    }
    let lease = runtime::Lease::new(generation.index_root(), generation.state_root());
    if !cold_build {
        let register_started_at = Instant::now();
        lease.keep_alive_detached();
        if let Some(report) = bench.as_deref_mut() {
            report.timing_mut().set_daemon_register(register_started_at);
        }
    }
    if cold_build {
        let register_started_at = Instant::now();
        lease.request_refresh()?;
        if let Some(report) = bench.as_deref_mut() {
            report.timing_mut().set_daemon_register(register_started_at);
            report.timing_mut().set_daemon_start(register_started_at);
        }
        let build_started_at = Instant::now();
        ensure_daemon_index_ready(&generation, !args.index().bench())?;
        if let Some(report) = bench.as_deref_mut() {
            if let Some(build_timings) = bench::read_build_timings(generation.state_root())? {
                report.timing_mut().merge_build(&build_timings);
            }
            report.timing_mut().set_cold_build_total(build_started_at);
            report.timing_mut().set_daemon_ready(build_started_at);
        }
    }
    if let Some(report) = bench.as_deref_mut() {
        let proof_started_at = Instant::now();
        let _ = runtime::daemon_freshness_proof(generation.state_root());
        report.timing_mut().set_daemon_proof(proof_started_at);
    }
    let validate_started_at = Instant::now();
    let (snapshot, freshness_proof) =
        snapshot::SnapshotLoader::new(args, table_fingerprint, generation.location(), &index_dir)
            .load(true)?
            .into_parts();
    if let Some(report) = bench.as_deref_mut() {
        report.timing_mut().set_manifest_open(validate_started_at);
        report.set_snapshot_counts(snapshot.file_count(), snapshot.binary_skipped_count());
        report.set_freshness_proof(freshness_proof);
    }
    if args.has_implicit_path() && snapshot.is_empty() {
        crate::eprint_nothing_searched();
    }
    let query_started_at = Instant::now();
    let Some(mut candidates) = generation.query(args, &snapshot, &plan, bench.as_deref_mut())?
    else {
        if let Some(report) = bench.as_deref_mut() {
            report.reject_selectivity();
        }
        return unsupported(Unsupported::TooManyCandidates);
    };
    if let Some(report) = bench.as_deref_mut() {
        report.timing_mut().set_index_lookup(query_started_at);
    }
    let restrict_started_at = Instant::now();
    let unrestricted_candidates = candidates.len();
    restrict_candidates(
        args,
        &roots,
        generation.index_root(),
        &snapshot,
        &mut candidates,
    );
    if let Some(report) = bench.as_deref_mut() {
        report
            .timing_mut()
            .set_candidate_restrict(restrict_started_at);
        report.set_candidates(candidates.len());
        report.set_parent_restricted_candidates(unrestricted_candidates - candidates.len());
    }
    let matched =
        verify::CandidateVerifier::new(args, mode, started_at, &snapshot, &candidates, bench)
            .verify()?;
    Ok(matched)
}

const COLD_BUILD_WAIT: Duration = Duration::from_secs(60 * 60);
const COLD_PROGRESS_POLL: Duration = Duration::from_millis(100);
const DAEMON_GONE_GRACE: Duration = Duration::from_secs(5);

fn ensure_daemon_index_ready(generation: &Generation, show_progress: bool) -> anyhow::Result<()> {
    if !runtime::daemon_watch_supported() {
        bail!("indexed daemon search requires Linux filesystem watch support; use --no-index");
    }
    let started = Instant::now();
    let lease = runtime::Lease::new(generation.index_root(), generation.state_root());
    let catching_up = generation.source() == "stale";
    let wake_floor = runtime::wake_mtime(generation.state_root());
    let mut progress = progress::BuildProgressRenderer::new(show_progress, catching_up);
    let mut daemon_gone_since = None;
    loop {
        progress.tick(generation.state_root());
        check_daemon_available(&mut daemon_gone_since)?;
        if catching_up
            && let Some(floor) = wake_floor
            && runtime::daemon_caught_up_since(generation.state_root(), floor)
        {
            progress.finish();
            return Ok(());
        }
        if wait_one_proof_poll(generation, &lease, started)? {
            progress.finish();
            return Ok(());
        }
    }
}

/// Tolerate a daemon-liveness misread briefly before failing the query
fn check_daemon_available(gone_since: &mut Option<Instant>) -> anyhow::Result<()> {
    if !runtime::daemon_autospawn_disabled() || runtime::daemon_running() {
        *gone_since = None;
        return Ok(());
    }
    let since = gone_since.get_or_insert_with(Instant::now);
    if since.elapsed() > DAEMON_GONE_GRACE {
        bail!("indexed search needs eg-indexd when daemon autospawn is disabled");
    }
    std::thread::sleep(COLD_PROGRESS_POLL);
    Ok(())
}

/// One bounded proof poll; true means the index is ready to serve
fn wait_one_proof_poll(
    generation: &Generation,
    lease: &runtime::Lease<'_>,
    started: Instant,
) -> anyhow::Result<bool> {
    let remaining = COLD_BUILD_WAIT.saturating_sub(started.elapsed());
    let poll = remaining.min(COLD_PROGRESS_POLL);
    match runtime::wait_for_freshness_proof(generation.state_root(), poll) {
        runtime::ProofWait::Ready => return Ok(true),
        runtime::ProofWait::DaemonStopped if !runtime::daemon_autospawn_disabled() => {
            if started.elapsed() < COLD_BUILD_WAIT {
                lease.request_refresh()?;
                return Ok(false);
            }
        },
        runtime::ProofWait::DaemonStopped => return Ok(false),
        runtime::ProofWait::TimedOut if started.elapsed() < COLD_BUILD_WAIT => return Ok(false),
        runtime::ProofWait::TimedOut => {},
    }
    bail!(
        "timed out waiting for daemon-owned index at {}",
        generation.index_dir().display()
    );
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
    index_root: &std::path::Path,
    snapshot: &manifest::CurrentSnapshot,
    candidates: &mut BTreeSet<usize>,
) {
    if roots.covers_index_root(index_root) {
        return;
    }
    candidates.retain(|ord| {
        snapshot
            .file(*ord)
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
