//! Sparse n-gram index integration.

pub(crate) mod backend;
pub(crate) mod config;

mod manifest;
mod planner;
mod postings;
mod store;

use std::{
    collections::{BTreeSet, VecDeque},
    fs,
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering as AtomicOrdering},
    },
    time::Instant,
};

use anyhow::bail;

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
    ensure_supported(args, mode)?;
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
    let table_spec = sngram_weights::selected()?;
    let table = table_spec.load()?;
    let planning_started_at = Instant::now();
    let plan = planner::query_plan(args, &table)?;
    log::debug!(
        "eg index: query planning took {:?}",
        planning_started_at.elapsed()
    );
    log::debug!("eg index: query plan: {plan}");
    let index_root = index_root(args)?;
    let index_dir = index_root.join(".eg/index");
    let state_root = index_root.join(".eg");
    let (snapshot, loaded_manifest) =
        match try_fast_snapshot(args, table_spec, &index_root, &index_dir)? {
            Some(snapshot) => snapshot,
            None => {
                fs::create_dir_all(&state_root).map_err(|err| {
                    anyhow::anyhow!(
                        "failed to create index state directory {}: {err}",
                        state_root.display()
                    )
                })?;
                let walk_started_at = Instant::now();
                let collected = collect_haystacks(args, &state_root)?;
                log::debug!(
                    "eg index: collected {} haystacks and {} dirs in {:?}",
                    collected.haystacks.len(),
                    collected.dirs.len(),
                    walk_started_at.elapsed()
                );
                let manifest_started_at = Instant::now();
                let snapshot = manifest::current_snapshot(
                    args,
                    &index_root,
                    &collected.haystacks,
                    &collected.dirs,
                )?;
                log::debug!(
                    "eg index: built freshness manifest for {} files in {:?}",
                    snapshot.files.len(),
                    manifest_started_at.elapsed()
                );
                (snapshot, None)
            },
        };
    if args.has_implicit_path() && snapshot.files.is_empty() {
        crate::eprint_nothing_searched();
    }

    let prepare_started_at = Instant::now();
    let candidates = match args.index().backend {
        IndexBackend::Postings => {
            let index_home = index_dir.join("postings-v3");
            let index = postings::prepare_index(
                args,
                table_spec,
                &table,
                &index_home,
                &snapshot,
                loaded_manifest.as_ref(),
            )?;
            postings::query_index(&index, &plan)?
        },
        IndexBackend::Tantivy | IndexBackend::TantivyRam => {
            let index_home = index_dir.join("tantivy-v1");
            let (schema, fields) = store::schema();
            let index = store::prepare_index(
                args,
                table_spec,
                &table,
                schema,
                fields,
                &index_home,
                &snapshot,
                loaded_manifest.as_ref(),
            )?;
            store::query_index(&index, fields, &plan)?
        },
    };
    log::debug!(
        "eg index: backend prepare+lookup produced {} candidates in {:?}",
        candidates.len(),
        prepare_started_at.elapsed()
    );
    verify_candidates(args, mode, started_at, &snapshot, &candidates)
}

fn ensure_supported(args: &HiArgs, mode: SearchMode) -> anyhow::Result<()> {
    if args.invert_match() {
        bail!("indexed search does not support inverted matches yet; use --no-index");
    }
    if matches!(mode, SearchMode::FilesWithoutMatch) {
        bail!("indexed search does not support --files-without-match yet; use --no-index");
    }
    if matches!(mode, SearchMode::JSON) {
        bail!("indexed search does not support JSON output yet; use --no-index");
    }
    if args.passthru() {
        bail!("indexed search does not support --passthru yet; use --no-index");
    }
    if args.include_zero() && matches!(mode, SearchMode::Count | SearchMode::CountMatches) {
        bail!("indexed search does not support --include-zero with counts yet; use --no-index");
    }
    if args.non_default_regex_engine() {
        bail!("indexed search does not support PCRE2 or hybrid regex engines yet; use --no-index");
    }
    if args.explicit_encoding() {
        bail!("indexed search does not support explicit encodings yet; use --no-index");
    }
    if args.has_preprocessor() {
        bail!("indexed search does not support preprocessors yet; use --no-index");
    }
    if args.search_zip() {
        bail!("indexed search does not support compressed archive search yet; use --no-index");
    }
    if args.no_unicode() {
        bail!("indexed search does not support --no-unicode yet; use --no-index");
    }
    Ok(())
}

fn try_fast_snapshot(
    args: &HiArgs,
    table_spec: sngram_weights::BuiltinTable,
    index_root: &Path,
    index_dir: &Path,
) -> anyhow::Result<Option<(manifest::CurrentSnapshot, Option<manifest::Manifest>)>> {
    if !matches!(args.index().mode, config::IndexMode::Auto)
        || matches!(args.index().backend, IndexBackend::TantivyRam)
    {
        return Ok(None);
    }
    let (backend, manifest_path) = match args.index().backend {
        IndexBackend::Postings => (
            manifest::ManifestBackend::Postings,
            index_dir.join("postings-v3/manifest.json"),
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
    if !manifest::is_compatible(&old, backend, table_spec) {
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

fn verify_candidates(
    args: &HiArgs,
    mode: SearchMode,
    started_at: Instant,
    snapshot: &manifest::CurrentSnapshot,
    candidates: &BTreeSet<usize>,
) -> anyhow::Result<bool> {
    if args.threads() > 1 && candidates.len() >= 4096 {
        return verify_candidates_parallel(args, mode, started_at, snapshot, candidates);
    }
    let verify_started_at = Instant::now();
    let mut matched = false;
    let mut stats = args.stats();
    let mut searcher = args.search_worker(
        args.matcher()?,
        args.searcher()?,
        args.printer(mode, args.stdout()),
    )?;
    for &ord in candidates {
        let Some(file) = snapshot.files.get(ord) else {
            continue;
        };
        let haystack = Haystack::from_index_path(file.path.clone(), file.is_explicit());
        let search_result = match searcher.search(&haystack) {
            Ok(search_result) => search_result,
            Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => break,
            Err(err) => {
                err_message!("{}: {}", haystack.path().display(), err);
                continue;
            },
        };
        matched = matched || search_result.has_match();
        if let Some(ref mut stats) = stats {
            if let Some(search_stats) = search_result.stats() {
                *stats += search_stats;
            }
        }
        if matched && args.quit_after_match() {
            break;
        }
    }
    log::debug!(
        "eg index: verified {} candidates matched={} verify_time={:?} total_time={:?}",
        candidates.len(),
        matched,
        verify_started_at.elapsed(),
        started_at.elapsed()
    );
    if let Some(ref stats) = stats {
        let writer = searcher.printer().get_mut();
        let _ = crate::print_stats(mode, stats, started_at, writer);
    }
    Ok(matched)
}

fn verify_candidates_parallel(
    args: &HiArgs,
    mode: SearchMode,
    started_at: Instant,
    snapshot: &manifest::CurrentSnapshot,
    candidates: &BTreeSet<usize>,
) -> anyhow::Result<bool> {
    let verify_started_at = Instant::now();
    let bufwtr = args.buffer_writer();
    let stats = args.stats().map(Mutex::new);
    let matched = AtomicBool::new(false);
    let mut stats_searcher = args.search_worker(
        args.matcher()?,
        args.searcher()?,
        args.printer(mode, bufwtr.buffer()),
    )?;
    let worker_count = args.threads().min(candidates.len()).max(1);
    let queue = Mutex::new(candidates.iter().copied().collect::<VecDeque<_>>());
    std::thread::scope(|scope| -> anyhow::Result<()> {
        let mut handles = Vec::with_capacity(worker_count);
        for _ in 0..worker_count {
            let mut searcher = args.search_worker(
                args.matcher()?,
                args.searcher()?,
                args.printer(mode, bufwtr.buffer()),
            )?;
            let queue = &queue;
            let stats = &stats;
            let matched = &matched;
            let bufwtr = &bufwtr;
            handles.push(scope.spawn(move || -> std::io::Result<()> {
                loop {
                    if matched.load(AtomicOrdering::SeqCst) && args.quit_after_match() {
                        return Ok(());
                    }
                    let ord = match queue.lock().unwrap().pop_front() {
                        Some(ord) => ord,
                        None => return Ok(()),
                    };
                    let Some(file) = snapshot.files.get(ord) else {
                        continue;
                    };
                    let haystack = Haystack::from_index_path(file.path.clone(), file.is_explicit());
                    searcher.printer().get_mut().clear();
                    let search_result = match searcher.search(&haystack) {
                        Ok(search_result) => search_result,
                        Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => {
                            return Err(err);
                        },
                        Err(err) => {
                            err_message!("{}: {}", haystack.path().display(), err);
                            continue;
                        },
                    };
                    if search_result.has_match() {
                        matched.store(true, AtomicOrdering::SeqCst);
                    }
                    if let Some(locked_stats) = stats {
                        if let Some(search_stats) = search_result.stats() {
                            let mut stats = locked_stats.lock().unwrap();
                            *stats += search_stats;
                        }
                    }
                    bufwtr.print(searcher.printer().get_mut())?;
                }
            }));
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
    log::debug!(
        "eg index: verified {} candidates in parallel matched={} verify_time={:?} total_time={:?}",
        candidates.len(),
        matched.load(AtomicOrdering::SeqCst),
        verify_started_at.elapsed(),
        started_at.elapsed()
    );
    if let Some(ref locked_stats) = stats {
        let stats = locked_stats.lock().unwrap();
        let mut wtr = stats_searcher.printer().get_mut();
        let _ = crate::print_stats(mode, &stats, started_at, &mut wtr);
        let _ = bufwtr.print(&mut wtr);
    }
    Ok(matched.load(AtomicOrdering::SeqCst))
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
