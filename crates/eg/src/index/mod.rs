//! Sparse n-gram index integration.

mod bench;
mod catalog;
mod classify;
mod config;
mod document;
mod executor;
mod generation;
mod location;
mod manifest;
mod planner;
mod postings;
mod request;
mod roots;
mod runtime;
mod store;
mod summary;
mod verify;

use std::{
    collections::BTreeSet,
    fmt::{self, Write as FmtWrite},
    path::{Path, PathBuf},
    time::Instant,
};

use anyhow::bail;
use catalog::GenerationCatalog;
use generation::Generation;
use request::{SearchRequest, Unsupported, unsupported};
use roots::{SearchRoots, absolute_path};
use sngram_types::QueryPlan;

use crate::{
    flags::{HiArgs, Mode},
    haystack::Haystack,
};

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
            run_daemon_refresh(self.args)?;
            return Ok(true);
        }
        if self.args.index().bench() {
            return run_bench(self.args);
        }
        run_inner(self.args, None)
    }
}

fn run_daemon_refresh(args: &HiArgs) -> anyhow::Result<()> {
    let Mode::Search(_) = args.mode() else {
        return Ok(());
    };
    if args.index().is_no_index()
        || args.index().is_maintenance()
        || request::searches_stdin(args)
        || matches!(args.index().backend(), config::IndexBackend::TantivyRam)
    {
        return Ok(());
    }

    let table = sngram_weights::weights();
    let table_fingerprint = table.fingerprint();
    let roots = SearchRoots::from_args(args)?;
    let catalog = GenerationCatalog::open(args, table_fingerprint);
    let generation = catalog.best_ready_generation(&roots)?;
    let location = generation.location;
    runtime::clear_journal_clean(&location.state_root);

    let collected = collect_haystacks(args, &location.state_root)?;
    let snapshot = manifest::current_snapshot(
        args,
        &location.corpus_root,
        &collected.haystacks,
        &collected.dirs,
    )?;
    let index_dir = location.index_dir();
    match args.index().backend() {
        config::IndexBackend::Postings => {
            postings::refresh_index(
                args,
                table_fingerprint,
                &table,
                &index_dir.join("postings-v5"),
                &snapshot,
            )?;
        },
        config::IndexBackend::Tantivy => {
            let (schema, fields) = store::schema();
            store::refresh_index(
                args,
                table_fingerprint,
                &table,
                schema,
                fields,
                &index_dir.join("tantivy-v2"),
                &snapshot,
            )?;
        },
        config::IndexBackend::TantivyRam => unreachable!("filtered above"),
    }
    runtime::mark_journal_clean(&location.state_root)?;
    Ok(())
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
        return maintenance(args);
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
    let roots = SearchRoots::from_args(args)?;
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
            generation.bench_source(cold_build),
        );
    }
    let validate_started_at = Instant::now();
    runtime::Lease::new(generation.index_root(), generation.state_root()).refresh_best_effort();
    let (snapshot, loaded_manifest, freshness_proof) =
        load_snapshot(args, table_fingerprint, generation.location(), &index_dir)?;
    if let Some(report) = bench.as_deref_mut() {
        report
            .timing_mut()
            .set_generation_validate(validate_started_at);
        let binary_skipped = snapshot
            .files
            .iter()
            .filter(|file| file.is_skipped_binary())
            .count();
        report.set_snapshot_counts(snapshot.files.len(), binary_skipped);
        report.set_freshness_proof(freshness_proof);
    }
    if args.has_implicit_path() && snapshot.files.is_empty() {
        crate::eprint_nothing_searched();
    }
    warn_large_implicit_build(args, generation.index_root(), &index_dir, &snapshot);
    let query_started_at = Instant::now();
    let Some(mut candidates) = generation.query(
        args,
        table_fingerprint,
        &table,
        &snapshot,
        loaded_manifest.as_ref(),
        &plan,
        cold_build,
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

/// Load a freshness snapshot, reusing a fast snapshot when the index is fresh.
fn load_snapshot(
    args: &HiArgs,
    table_fingerprint: u64,
    location: &location::IndexLocation,
    index_dir: &Path,
) -> anyhow::Result<(
    manifest::CurrentSnapshot,
    Option<manifest::Manifest>,
    &'static str,
)> {
    if let Some(snapshot) = try_daemon_snapshot(
        args,
        table_fingerprint,
        &location.corpus_root,
        &location.state_root,
        index_dir,
    )? {
        return Ok((snapshot.0, Some(snapshot.1), "daemon"));
    }
    if let Some(snapshot) =
        try_fast_snapshot(args, table_fingerprint, &location.corpus_root, index_dir)?
    {
        return Ok((snapshot.0, snapshot.1, "walk"));
    }
    let collected = collect_haystacks(args, &location.state_root)?;
    log::debug!(
        "eg index: collected {} haystacks and {} dirs",
        collected.haystacks.len(),
        collected.dirs.len()
    );
    let snapshot = manifest::current_snapshot(
        args,
        &location.corpus_root,
        &collected.haystacks,
        &collected.dirs,
    )?;
    Ok((snapshot, None, "walk"))
}

/// Prepare the selected backend and return the candidate document ordinals.
fn backend_candidates(
    args: &HiArgs,
    table_fingerprint: u64,
    table: &sngram_types::WeightTable,
    index_dir: &Path,
    snapshot: &manifest::CurrentSnapshot,
    loaded_manifest: Option<&manifest::Manifest>,
    plan: &planner::IndexPlan,
    cold_build: bool,
    mut bench: Option<&mut bench::BenchReport>,
) -> anyhow::Result<Option<BTreeSet<usize>>> {
    let prepare_started_at = Instant::now();
    let candidates = match args.index().backend() {
        config::IndexBackend::Postings => {
            let index_home = index_dir.join("postings-v5");
            let open_started_at = Instant::now();
            let index = postings::prepare_index(
                args,
                table_fingerprint,
                table,
                &index_home,
                snapshot,
                loaded_manifest,
            )?;
            if let Some(report) = bench.as_deref_mut() {
                report.timing_mut().set_index_mmap(open_started_at);
                if cold_build {
                    report.timing_mut().set_initial_build(open_started_at);
                }
                report.set_index_bytes(&index_home);
            }
            let Some(candidates) = postings::query_index(&index, plan)? else {
                return Ok(None);
            };
            candidates
        },
        config::IndexBackend::Tantivy | config::IndexBackend::TantivyRam => {
            let index_home = index_dir.join("tantivy-v2");
            let (schema, fields) = store::schema();
            let open_started_at = Instant::now();
            let index = store::prepare_index(
                args,
                table_fingerprint,
                table,
                schema,
                fields,
                &index_home,
                snapshot,
                loaded_manifest,
            )?;
            if let Some(report) = bench.as_deref_mut() {
                report.timing_mut().set_index_mmap(open_started_at);
                if cold_build {
                    report.timing_mut().set_initial_build(open_started_at);
                }
            }
            let Some(candidates) = store::query_index(&index, fields, plan)? else {
                return Ok(None);
            };
            candidates
        },
    };
    log::debug!(
        "eg index: backend prepare+lookup produced {} candidates in {:?}",
        candidates.len(),
        prepare_started_at.elapsed()
    );
    Ok(Some(candidates))
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
    if !args.has_implicit_path() || index_present(args, index_dir) {
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

/// Return true when a compatible-form index manifest already exists.
fn index_present(args: &HiArgs, index_dir: &Path) -> bool {
    let manifest = match args.index().backend() {
        config::IndexBackend::Postings => index_dir.join("postings-v5/manifest.json"),
        config::IndexBackend::Tantivy => index_dir.join("tantivy-v2/manifest.json"),
        config::IndexBackend::TantivyRam => return false,
    };
    manifest::manifest_present(&manifest)
}

/// Return true when the path resolves to the user's home directory.
fn is_home_dir(path: &Path) -> bool {
    let Some(home) = std::env::var_os("HOME") else {
        return false;
    };
    let canon = |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    canon(path) == canon(&PathBuf::from(home))
}

/// Run `--index=verify` or `--index=repair`: check the index and report, and
/// under repair rebuild it when a fault is found. Returns whether it is healthy.
fn maintenance(args: &HiArgs) -> anyhow::Result<bool> {
    let table = sngram_weights::weights();
    let table_fingerprint = table.fingerprint();
    let location = location::resolve(args, &index_root(args)?)?;
    let index_dir = location.index_dir();
    if !matches!(args.index().backend(), config::IndexBackend::Postings) {
        report_line(
            "eg index verify: only the postings backend is verifiable (tantivy is experimental)",
        );
        return Ok(false);
    }
    let index_home = index_dir.join("postings-v5");
    let report = postings::verify_index(&index_home, table_fingerprint)?;
    for line in report.lines() {
        report_line(&line);
    }
    if report.healthy() {
        report_line("eg index verify: index is healthy");
        return Ok(true);
    }
    if matches!(args.index().mode(), config::IndexMode::Repair) {
        report_line("eg index repair: fault found, rebuilding");
        rebuild_for_repair(args, table_fingerprint, &table, &location, &index_home)?;
        report_line("eg index repair: rebuild complete");
        return Ok(true);
    }
    report_line("eg index verify: index is unhealthy (run --index=repair to rebuild)");
    Ok(false)
}

/// Rebuild the postings index from a fresh corpus snapshot for repair.
fn rebuild_for_repair(
    args: &HiArgs,
    table_fingerprint: u64,
    table: &sngram_types::WeightTable,
    location: &location::IndexLocation,
    index_home: &Path,
) -> anyhow::Result<()> {
    let collected = collect_haystacks(args, &location.state_root)?;
    let snapshot = manifest::current_snapshot(
        args,
        &location.corpus_root,
        &collected.haystacks,
        &collected.dirs,
    )?;
    postings::rebuild(args, table_fingerprint, table, index_home, &snapshot)?;
    Ok(())
}

/// Print a maintenance report line to stdout, ignoring write errors.
fn report_line(line: &str) {
    use std::io::Write;
    let _ = writeln!(std::io::stdout().lock(), "{line}");
}

fn try_daemon_snapshot(
    args: &HiArgs,
    table_fingerprint: u64,
    index_root: &Path,
    state_root: &Path,
    index_dir: &Path,
) -> anyhow::Result<Option<(manifest::CurrentSnapshot, manifest::Manifest)>> {
    if !matches!(
        args.index().mode(),
        config::IndexMode::Auto | config::IndexMode::Require
    ) || !runtime::daemon_freshness_proof(state_root)
    {
        return Ok(None);
    }
    let (backend, manifest_path) = match args.index().backend() {
        config::IndexBackend::Postings => (
            manifest::ManifestBackend::Postings,
            index_dir.join("postings-v5/manifest.json"),
        ),
        config::IndexBackend::Tantivy => (
            manifest::ManifestBackend::Tantivy,
            index_dir.join("tantivy-v2/manifest.json"),
        ),
        config::IndexBackend::TantivyRam => return Ok(None),
    };
    let Some(manifest) = manifest::read_manifest(&manifest_path)? else {
        return Ok(None);
    };
    if !manifest::is_filter_compatible(&manifest, args, backend, table_fingerprint) {
        return Ok(None);
    }
    let snapshot = manifest::snapshot_from_manifest(index_root, &manifest);
    log::debug!(
        "eg index: loaded daemon-proofed manifest snapshot for {} files",
        snapshot.files.len()
    );
    Ok(Some((snapshot, manifest)))
}

fn try_fast_snapshot(
    args: &HiArgs,
    table_fingerprint: u64,
    index_root: &Path,
    index_dir: &Path,
) -> anyhow::Result<Option<(manifest::CurrentSnapshot, Option<manifest::Manifest>)>> {
    if !matches!(
        args.index().mode(),
        config::IndexMode::Auto | config::IndexMode::Require
    ) || matches!(args.index().backend(), config::IndexBackend::TantivyRam)
    {
        return Ok(None);
    }
    let (backend, manifest_path) = match args.index().backend() {
        config::IndexBackend::Postings => (
            manifest::ManifestBackend::Postings,
            index_dir.join("postings-v5/manifest.json"),
        ),
        config::IndexBackend::Tantivy => (
            manifest::ManifestBackend::Tantivy,
            index_dir.join("tantivy-v2/manifest.json"),
        ),
        config::IndexBackend::TantivyRam => return Ok(None),
    };
    let manifest_read_started_at = Instant::now();
    let Some(old) = manifest::read_manifest(&manifest_path)? else {
        return Ok(None);
    };
    log::debug!(
        "eg index: read manifest {} in {:?}",
        manifest_path.display(),
        manifest_read_started_at.elapsed()
    );
    if !manifest::is_compatible(&old, backend, table_fingerprint) {
        return Ok(None);
    }
    let started_at = Instant::now();
    let Some(snapshot) = manifest::fast_snapshot(args, index_root, &old)? else {
        log::debug!(
            "eg index: fast freshness snapshot invalidated in {:?}",
            started_at.elapsed()
        );
        return Ok(None);
    };
    log::debug!(
        "eg index: loaded fast freshness snapshot for {} files in {:?}",
        snapshot.files.len(),
        started_at.elapsed()
    );
    Ok(Some((snapshot, Some(old))))
}

fn collect_haystacks(args: &HiArgs, index_state_root: &Path) -> anyhow::Result<CollectedHaystacks> {
    let haystack_builder = args.haystack_builder();
    let cwd = args.cwd().to_path_buf();
    let index_state_root = absolute_path(&cwd, index_state_root);
    let mut unsorted = Vec::new();
    let mut dirs = Vec::new();
    for result in args.walk_builder()?.build() {
        let dent = match result {
            Ok(dent) => dent,
            Err(err) => {
                let _ = haystack_builder.build_from_result(Err(err));
                continue;
            },
        };
        let path = absolute_path(&cwd, dent.path());
        if path.starts_with(&index_state_root) {
            continue;
        }
        if dent.file_type().is_some_and(|file_type| file_type.is_dir()) {
            dirs.push(dent.path().to_path_buf());
        }
        let Some(haystack) = haystack_builder.build_from_result(Ok(dent)) else {
            continue;
        };
        unsorted.push(haystack);
    }
    let mut haystacks = Vec::new();
    for haystack in args.sort(unsorted.into_iter()) {
        if haystack.is_stdin() {
            bail!("indexed search does not support stdin yet; use --no-index");
        }
        haystacks.push(haystack);
    }
    Ok(CollectedHaystacks { haystacks, dirs })
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

fn index_root(args: &HiArgs) -> anyhow::Result<PathBuf> {
    let cwd = args.cwd();
    let Some(path) = args.search_paths().first() else {
        return Ok(cwd.to_path_buf());
    };
    if path == Path::new("-") {
        bail!("indexed search does not support stdin yet; use --no-index");
    }
    if args.search_paths().len() > 1 {
        return Ok(cwd.to_path_buf());
    }
    let absolute = absolute_path(cwd, path);
    if absolute.is_dir() {
        Ok(absolute)
    } else {
        Ok(absolute
            .parent()
            .map_or_else(|| cwd.to_path_buf(), Path::to_path_buf))
    }
}

struct CollectedHaystacks {
    haystacks: Vec<Haystack>,
    dirs: Vec<PathBuf>,
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
