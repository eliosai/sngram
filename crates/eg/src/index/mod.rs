//! Sparse n-gram index integration.

pub(crate) mod backend;
pub(crate) mod config;

mod classify;
mod location;
mod manifest;
mod planner;
mod postings;
mod store;

use std::{
    collections::BTreeSet,
    fmt::{self, Write as FmtWrite},
    mem,
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering},
    },
    time::Instant,
};

use anyhow::bail;
use sngram_types::{PlanExpr, QueryError, QueryPlan};

use crate::{
    flags::{HiArgs, Mode, SearchMode},
    haystack::Haystack,
    index::config::IndexBackend,
};

/// Run an indexed search.
pub(crate) fn run(args: &HiArgs) -> anyhow::Result<bool> {
    let Mode::Search(mode) = args.mode() else {
        bail!("indexed mode only supports search");
    };
    if args.index().mode.is_maintenance() {
        return maintenance(args);
    }
    if let Some(reason) = unsupported_reason(args, mode) {
        return unsupported(reason);
    }
    if searches_stdin(args) {
        return unsupported(Unsupported::Stdin);
    }
    if !args.matches_possible() {
        return Ok(false);
    }
    let started_at = Instant::now();
    log::debug!(
        "eg index: mode={:?} backend={:?} threads={}",
        args.index().mode,
        args.index().backend,
        args.threads()
    );
    if !matches!(args.index().backend, IndexBackend::Postings) {
        log::debug!(
            "eg index: the {:?} backend is experimental and unsupported; prefer --index-backend postings",
            args.index().backend
        );
    }
    let table = sngram_weights::weights();
    let table_fingerprint = table.fingerprint();
    let plan = match planner::query_plan(args, &table) {
        Ok(plan) => plan,
        Err(err) => {
            if let Some(query_err) = err.downcast_ref::<QueryError>() {
                match query_err {
                    QueryError::PatternTooLong { .. } => {
                        log::debug!("eg index: planner rejected query: {query_err}");
                        return unsupported(Unsupported::PlannerError);
                    },
                    QueryError::InvalidRegex(_) => bail!("{query_err}"),
                    _ => return unsupported(Unsupported::PlannerError),
                }
            }
            log::debug!("eg index: planner rejected query: {err:#}");
            return unsupported(Unsupported::PlannerError);
        },
    };
    log::debug!("eg index: query plan: {}", debug_plan(&plan.plan));
    match plan.plan.root() {
        PlanExpr::All => return unsupported(Unsupported::TooBroadPattern),
        PlanExpr::None => return unsupported(Unsupported::ImpossiblePattern),
        PlanExpr::AllOf { .. } | PlanExpr::AnyOf { .. } => {},
    }
    let location = location::resolve(args, &index_root(args)?)?;
    let index_dir = location.index_dir();
    let (snapshot, loaded_manifest) =
        load_snapshot(args, table_fingerprint, &location, &index_dir)?;
    if args.has_implicit_path() && snapshot.files.is_empty() {
        crate::eprint_nothing_searched();
    }
    warn_large_implicit_build(args, &location.corpus_root, &index_dir, &snapshot);
    let Some(candidates) = backend_candidates(
        args,
        table_fingerprint,
        &table,
        &index_dir,
        &snapshot,
        loaded_manifest.as_ref(),
        &plan,
    )?
    else {
        return unsupported(Unsupported::TooManyCandidates);
    };
    let matched = verify_candidates(args, mode, started_at, &snapshot, &candidates)?;
    Ok(matched)
}

/// Load a freshness snapshot, reusing a fast snapshot when the index is fresh.
fn load_snapshot(
    args: &HiArgs,
    table_fingerprint: u64,
    location: &location::IndexLocation,
    index_dir: &Path,
) -> anyhow::Result<(manifest::CurrentSnapshot, Option<manifest::Manifest>)> {
    if let Some(snapshot) =
        try_fast_snapshot(args, table_fingerprint, &location.corpus_root, index_dir)?
    {
        return Ok(snapshot);
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
    Ok((snapshot, None))
}

/// Prepare the selected backend and return the candidate document ordinals.
fn backend_candidates(
    args: &HiArgs,
    table_fingerprint: u64,
    table: &sngram_types::WeightTable,
    index_dir: &Path,
    snapshot: &manifest::CurrentSnapshot,
    loaded_manifest: Option<&manifest::Manifest>,
    plan: &planner::KeyedPlan,
) -> anyhow::Result<Option<BTreeSet<usize>>> {
    let prepare_started_at = Instant::now();
    let candidates = match args.index().backend {
        IndexBackend::Postings => {
            let index_home = index_dir.join("postings-v4");
            let index = postings::prepare_index(
                args,
                table_fingerprint,
                table,
                &index_home,
                snapshot,
                loaded_manifest,
            )?;
            let Some(candidates) = postings::query_index(&index, plan)? else {
                return Ok(None);
            };
            candidates
        },
        IndexBackend::Tantivy | IndexBackend::TantivyRam => {
            let index_home = index_dir.join("tantivy-v1");
            let (schema, fields) = store::schema();
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

/// Return an unindexable-query reason, or `None` when the index can serve it.
fn unsupported_reason(args: &HiArgs, _mode: SearchMode) -> Option<Unsupported> {
    if args.invert_match() {
        return Some(Unsupported::Feature {
            what: "inverted matches",
            why: "`-v/--invert-match` can make every non-matching file relevant, so sparse positive grams cannot safely narrow the search",
        });
    }
    if args.passthru() {
        return Some(Unsupported::Feature {
            what: "`--passthru`",
            why: "passthru prints non-matching lines too, so the index cannot reduce the output to matching candidate files",
        });
    }
    if args.non_default_regex_engine() {
        return Some(Unsupported::Feature {
            what: "PCRE2 or hybrid regex engines",
            why: "the sparse planner currently proves constraints for the default Rust regex semantics only",
        });
    }
    if args.explicit_encoding() {
        return Some(Unsupported::Feature {
            what: "explicit text encodings",
            why: "the index stores byte n-grams from the raw corpus and cannot yet plan over decoded alternate encodings",
        });
    }
    if args.has_preprocessor() {
        return Some(Unsupported::Feature {
            what: "preprocessors",
            why: "the index is built over stored files, not transformed preprocessor output",
        });
    }
    if args.search_zip() {
        return Some(Unsupported::Feature {
            what: "compressed archive search",
            why: "archive members are not present as stable files in the sparse n-gram index",
        });
    }
    if args.null_data() {
        return Some(Unsupported::Feature {
            what: "`--null-data`",
            why: "NUL-delimited line semantics use different boundaries than the newline sentinels stored in the sparse n-gram index",
        });
    }
    if args.index_rejects_binary_mode() {
        return Some(Unsupported::Feature {
            what: "binary search flags",
            why: "indexed eg does not search binary data; remove `--binary`/`--text` or pass `--no-index` for an explicit unindexed run",
        });
    }
    None
}

#[derive(Clone, Copy)]
enum Unsupported {
    Feature {
        what: &'static str,
        why: &'static str,
    },
    Stdin,
    PlannerError,
    TooBroadPattern,
    ImpossiblePattern,
    TooManyCandidates,
}

/// Report an indexed-search request that cannot be served safely.
fn unsupported<T>(reason: Unsupported) -> anyhow::Result<T> {
    match reason {
        Unsupported::Feature { what, why } => bail!(
            "indexed search cannot run with {what}.\n\nwhy: {why}.\nwhat works: remove the unsupported option, or pass `--no-index` when you intentionally want an exact unindexed scan."
        ),
        Unsupported::Stdin => bail!(
            "indexed search cannot read stdin.\n\nwhy: stdin is a stream, but the sparse n-gram index only covers stable files in the indexed corpus.\nwhat works: write the input to a file and search that path, or pass `--no-index` for an exact stream scan."
        ),
        Unsupported::PlannerError | Unsupported::TooBroadPattern => bail!(
            "indexed search cannot use this pattern because it is too broad for the sparse n-gram index.\n\nwhy: the pattern has no required byte n-gram that can narrow candidate files.\nwhat works: add a literal substring of at least 3 bytes, narrow wide character classes or repetitions, or pass `--no-index` for an exact unindexed scan."
        ),
        Unsupported::ImpossiblePattern => bail!(
            "indexed search cannot use this pattern because it cannot match any text under the current regex options.\n\nwhy: contradictory anchors, boundaries, or character classes made the planner prove the language empty.\nwhat works: check anchors like `$`/`^`, word boundaries like `\\b`/`\\B`, and impossible classes; use `--no-index` only if you want to double-check with the regex engine."
        ),
        Unsupported::TooManyCandidates => bail!(
            "indexed search cannot use this pattern efficiently because it selects too much of the corpus.\n\nwhy: the sparse n-gram estimate is above the indexed-search selectivity ceiling, so verifying candidates would be slower than a scan.\nwhat works: add a rarer literal, narrow numeric or wide character classes, split the search into a more selective pattern, or pass `--no-index` for an exact unindexed scan."
        ),
    }
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
    let manifest = match args.index().backend {
        IndexBackend::Postings => index_dir.join("postings-v4/manifest.json"),
        IndexBackend::Tantivy => index_dir.join("tantivy-v1/manifest.json"),
        IndexBackend::TantivyRam => return false,
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

/// Return true when any haystack to search is stdin.
fn searches_stdin(args: &HiArgs) -> bool {
    args.search_paths()
        .iter()
        .any(|path| path == Path::new("-"))
}

/// Run `--index=verify` or `--index=repair`: check the index and report, and
/// under repair rebuild it when a fault is found. Returns whether it is healthy.
fn maintenance(args: &HiArgs) -> anyhow::Result<bool> {
    let table = sngram_weights::weights();
    let table_fingerprint = table.fingerprint();
    let location = location::resolve(args, &index_root(args)?)?;
    let index_dir = location.index_dir();
    if !matches!(args.index().backend, IndexBackend::Postings) {
        report_line(
            "eg index verify: only the postings backend is verifiable (tantivy is experimental)",
        );
        return Ok(true);
    }
    let index_home = index_dir.join("postings-v4");
    let report = postings::verify_index(&index_home, table_fingerprint)?;
    for line in report.lines() {
        report_line(&line);
    }
    if report.healthy() {
        report_line("eg index verify: index is healthy");
        return Ok(true);
    }
    if matches!(args.index().mode, config::IndexMode::Repair) {
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

fn try_fast_snapshot(
    args: &HiArgs,
    table_fingerprint: u64,
    index_root: &Path,
    index_dir: &Path,
) -> anyhow::Result<Option<(manifest::CurrentSnapshot, Option<manifest::Manifest>)>> {
    if !matches!(
        args.index().mode,
        config::IndexMode::Auto | config::IndexMode::Require
    ) || matches!(args.index().backend, IndexBackend::TantivyRam)
    {
        return Ok(None);
    }
    let (backend, manifest_path) = match args.index().backend {
        IndexBackend::Postings => (
            manifest::ManifestBackend::Postings,
            index_dir.join("postings-v4/manifest.json"),
        ),
        IndexBackend::Tantivy => (
            manifest::ManifestBackend::Tantivy,
            index_dir.join("tantivy-v1/manifest.json"),
        ),
        IndexBackend::TantivyRam => return Ok(None),
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

/// Candidate document ordinals in the manifest's requested output order.
fn ordered_candidates(
    snapshot: &manifest::CurrentSnapshot,
    candidates: &BTreeSet<usize>,
) -> Vec<usize> {
    snapshot
        .files
        .iter()
        .enumerate()
        .filter_map(|(ord, _)| candidates.contains(&ord).then_some(ord))
        .collect()
}

/// Every document ordinal in the manifest's requested output order.
fn all_ordered(snapshot: &manifest::CurrentSnapshot) -> Vec<usize> {
    (0..snapshot.files.len()).collect()
}

/// Smallest candidate set that a multi-threaded verify is worth spawning for.
const PARALLEL_VERIFY_MIN: usize = 4096;

/// Return true when the mode reports on the whole corpus, not just matches.
fn is_full_corpus_mode(args: &HiArgs, mode: SearchMode) -> bool {
    matches!(mode, SearchMode::FilesWithoutMatch)
        || (args.include_zero() && matches!(mode, SearchMode::Count | SearchMode::CountMatches))
}

/// Worker count for verify: single-threaded unless the candidate set is large.
fn verify_worker_count(args: &HiArgs, ordered: usize) -> usize {
    if args.threads() > 1 && ordered >= PARALLEL_VERIFY_MIN {
        args.threads().min(ordered).max(1)
    } else {
        1
    }
}

fn verify_candidates(
    args: &HiArgs,
    mode: SearchMode,
    started_at: Instant,
    snapshot: &manifest::CurrentSnapshot,
    candidates: &BTreeSet<usize>,
) -> anyhow::Result<bool> {
    if is_full_corpus_mode(args, mode) {
        return verify_full_corpus(args, mode, started_at, snapshot, candidates);
    }
    let ordered = ordered_candidates(snapshot, candidates);
    verify_buffered(args, mode, started_at, snapshot, &ordered)
}

/// Report on every corpus file for modes that print zero-match files too.
///
/// Files the index ruled out have no matches by soundness, so they are emitted
/// through the printer with an empty search — the exact zero-count or
/// without-match line — while the candidate set is searched for real. Output is
/// path-ordered and single-threaded, matching ripgrep's per-file summary lines.
fn verify_full_corpus(
    args: &HiArgs,
    mode: SearchMode,
    started_at: Instant,
    snapshot: &manifest::CurrentSnapshot,
    candidates: &BTreeSet<usize>,
) -> anyhow::Result<bool> {
    let ordered = all_ordered(snapshot);
    let mut matched = false;
    let mut stats = args.stats();
    let mut searcher = args.search_worker(
        args.matcher()?,
        args.searcher()?,
        args.printer(mode, args.stdout()),
    )?;
    for &ord in &ordered {
        let Some(file) = snapshot.files.get(ord) else {
            continue;
        };
        let haystack = Haystack::from_index_path(file.path.clone(), file.is_explicit());
        let search_result = if candidates.contains(&ord) {
            searcher.search(&haystack)
        } else if file.is_skipped_binary() {
            continue;
        } else {
            searcher.search_absent(&file.path)
        };
        let search_result = match search_result {
            Ok(search_result) => search_result,
            Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => break,
            Err(err) => {
                err_message!("{}: {}", haystack.path().display(), err);
                continue;
            },
        };
        matched = matched || search_result.has_match();
        if let Some(ref mut stats) = stats
            && let Some(search_stats) = search_result.stats()
        {
            *stats += search_stats;
        }
    }
    if let Some(ref stats) = stats {
        let writer = searcher.printer().get_mut();
        let _ = crate::print_stats(mode, stats, started_at, writer);
    }
    Ok(matched)
}

/// Reorder buffer that releases per-file output strictly in path order.
struct Reorder {
    next_emit: usize,
    slots: Vec<Option<termcolor::Buffer>>,
}

impl Reorder {
    fn new(len: usize) -> Self {
        Self {
            next_emit: 0,
            slots: (0..len).map(|_| None).collect(),
        }
    }

    /// Store one file's buffered output, then flush the completed prefix.
    fn record_and_flush(
        &mut self,
        pos: usize,
        buffer: termcolor::Buffer,
        bufwtr: &termcolor::BufferWriter,
    ) -> std::io::Result<()> {
        if let Some(slot) = self.slots.get_mut(pos) {
            *slot = Some(buffer);
        }
        while self.slots.get(self.next_emit).is_some_and(Option::is_some) {
            if let Some(Some(ready)) = self.slots.get_mut(self.next_emit).map(Option::take) {
                bufwtr.print(&ready)?;
            }
            self.next_emit += 1;
        }
        Ok(())
    }
}

/// Shared state for the parallel verify workers.
struct Verify<'a> {
    args: &'a HiArgs,
    snapshot: &'a manifest::CurrentSnapshot,
    ordered: &'a [usize],
    next_pos: &'a AtomicUsize,
    matched: &'a AtomicBool,
    stats: Option<&'a Mutex<grep::printer::Stats>>,
    reorder: &'a Mutex<Reorder>,
    bufwtr: &'a termcolor::BufferWriter,
}

/// Verify a path-ordered candidate set through per-file buffers.
///
/// Output is buffered per file and released in path order by the reorder
/// buffer, so results are deterministic and the buffer writer inserts context
/// separators between adjacent printed files exactly as ripgrep's parallel path
/// does. A worker count of one still routes through the buffer writer, which is
/// why single-threaded indexed output gets the same separators as parallel.
fn verify_buffered(
    args: &HiArgs,
    mode: SearchMode,
    started_at: Instant,
    snapshot: &manifest::CurrentSnapshot,
    ordered: &[usize],
) -> anyhow::Result<bool> {
    let bufwtr = args.buffer_writer();
    let stats = args.stats().map(Mutex::new);
    let matched = AtomicBool::new(false);
    let next_pos = AtomicUsize::new(0);
    let reorder = Mutex::new(Reorder::new(ordered.len()));
    let mut stats_searcher = args.search_worker(
        args.matcher()?,
        args.searcher()?,
        args.printer(mode, bufwtr.buffer()),
    )?;
    let ctx = Verify {
        args,
        snapshot,
        ordered,
        next_pos: &next_pos,
        matched: &matched,
        stats: stats.as_ref(),
        reorder: &reorder,
        bufwtr: &bufwtr,
    };
    let worker_count = verify_worker_count(args, ordered.len());
    std::thread::scope(|scope| -> anyhow::Result<()> {
        let mut handles = Vec::with_capacity(worker_count);
        for _ in 0..worker_count {
            let searcher = args.search_worker(
                args.matcher()?,
                args.searcher()?,
                args.printer(mode, bufwtr.buffer()),
            )?;
            let ctx = &ctx;
            handles.push(scope.spawn(move || verify_worker(ctx, searcher)));
        }
        for handle in handles {
            match handle.join() {
                Ok(Ok(())) => {},
                Ok(Err(err)) if err.kind() == std::io::ErrorKind::BrokenPipe => {},
                Ok(Err(err)) => return Err(err.into()),
                Err(_) => bail!("indexed search worker thread panicked"),
            }
        }
        Ok(())
    })?;
    if let Some(ref locked_stats) = stats {
        let stats = locked_stats
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let wtr = stats_searcher.printer().get_mut();
        let _ = crate::print_stats(mode, &stats, started_at, &mut *wtr);
        let _ = bufwtr.print(wtr);
    }
    Ok(matched.load(AtomicOrdering::SeqCst))
}

/// One verify worker: pull path-ordered candidates and emit through the reorder buffer.
fn verify_worker(
    ctx: &Verify,
    mut searcher: crate::search::SearchWorker<termcolor::Buffer>,
) -> std::io::Result<()> {
    loop {
        if ctx.matched.load(AtomicOrdering::SeqCst) && ctx.args.quit_after_match() {
            return Ok(());
        }
        let pos = ctx.next_pos.fetch_add(1, AtomicOrdering::Relaxed);
        let Some(&ord) = ctx.ordered.get(pos) else {
            return Ok(());
        };
        let buffer = match ctx.snapshot.files.get(ord) {
            Some(file) => {
                verify_one(ctx, &mut searcher, file)?;
                mem::replace(searcher.printer().get_mut(), ctx.bufwtr.buffer())
            },
            None => ctx.bufwtr.buffer(),
        };
        ctx.reorder
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .record_and_flush(pos, buffer, ctx.bufwtr)?;
    }
}

/// Search one candidate file, updating the shared match flag and stats.
fn verify_one(
    ctx: &Verify,
    searcher: &mut crate::search::SearchWorker<termcolor::Buffer>,
    file: &manifest::CurrentFile,
) -> std::io::Result<()> {
    let haystack = Haystack::from_index_path(file.path.clone(), file.is_explicit());
    let search_result = match searcher.search(&haystack) {
        Ok(search_result) => search_result,
        Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => return Err(err),
        Err(err) => {
            err_message!("{}: {}", haystack.path().display(), err);
            return Ok(());
        },
    };
    if search_result.has_match() {
        ctx.matched.store(true, AtomicOrdering::SeqCst);
    }
    if let Some(locked_stats) = ctx.stats
        && let Some(search_stats) = search_result.stats()
    {
        *locked_stats
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) += search_stats;
    }
    Ok(())
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

fn absolute_path(cwd: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
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
