//! Compact mmap-backed sparse n-gram postings index.

use std::{
    cmp::Ordering,
    collections::{BTreeSet, BinaryHeap, HashMap},
    fs::{self, File, TryLockError},
    io::{self, BufReader, BufWriter, Cursor, Read, Seek, SeekFrom, Write},
    mem,
    path::{Path, PathBuf},
    rc::Rc,
    sync::atomic::{AtomicUsize, Ordering as AtomicOrdering},
    time::{Duration, Instant},
};

use anyhow::Context;
use memmap2::{Mmap, MmapOptions};
use rayon::prelude::*;

use sngram::types::{DfStats, QueryExpr, QueryPlan};
use sngram_types::WeightTable;
use sngram_types::{Gram, GramSpace, HashKey, ScanError, ScanEvent};

use crate::flags::HiArgs;

use super::manifest::{
    CurrentFile, CurrentSnapshot, Manifest, ManifestBackend, changed_ordinals, is_compatible,
    manifest_for, manifest_present, read_manifest, remove_manifest, write_manifest,
};

const MANIFEST_FILE_NAME: &str = "manifest.json";
const DELTA_MANIFEST_FILE_NAME: &str = "delta-manifest.json";
const TABLE_FILE_NAME: &str = "table.bin";
const POSTINGS_FILE_NAME: &str = "postings.bin";
const DELTA_TABLE_FILE_NAME: &str = "delta-table.bin";
const DELTA_POSTINGS_FILE_NAME: &str = "delta-postings.bin";
const RUNS_DIR_NAME: &str = "runs";
const LOCK_SUFFIX: &str = ".lock";
const TEMP_SUFFIX: &str = ".rebuilding";
const OLD_SUFFIX: &str = ".old";
const SECTION_HEADER_SIZE: usize = 32;
const SECTION_FORMAT_VERSION: u32 = 4;
const TABLE_MAGIC: [u8; 8] = *b"EGTABL1\0";
const POSTINGS_MAGIC: [u8; 8] = *b"EGPOST1\0";
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
/// Table record layout: hash (8 bytes) then posting-list length (4 bytes).
/// The posting-list byte offset is not stored; it is the prefix sum of the
/// preceding lengths, reconstructed once when a segment opens.
const TABLE_RECORD_SIZE: usize = 20;
const POSTING_SIZE: usize = 4;
const RUN_PAIR_SIZE: usize = 12;
const FILES_PER_RAYON_TASK: usize = 128;
const INDEX_RAM_CAP_BYTES: usize = 512 * 1024 * 1024;
const MIN_PAIRS_PER_RUN: usize = 128 * 1024;
const MAX_PAIRS_PER_RUN: usize = 2_000_000;
const MAX_DELTA_FILES: usize = 4096;
const FORCED_CANDIDATE_HASH: u64 = u64::MAX;
/// Files scanned between index-build progress lines under `--debug`.
const BUILD_PROGRESS_EVERY: usize = 20_000;

pub(super) fn prepare_index(
    args: &HiArgs,
    table_fingerprint: u64,
    table: &WeightTable,
    index_home: &Path,
    snapshot: &CurrentSnapshot,
    loaded_manifest: Option<&Manifest>,
) -> anyhow::Result<PostingsIndex> {
    match args.index().mode {
        super::config::IndexMode::NoIndex => {
            anyhow::bail!("internal error: indexed path used with --no-index")
        },
        super::config::IndexMode::Verify | super::config::IndexMode::Repair => {
            anyhow::bail!("internal error: maintenance mode reached prepare_index")
        },
        super::config::IndexMode::Rebuild => {
            rebuild_index(args, table_fingerprint, table, index_home, snapshot)
        },
        super::config::IndexMode::Auto | super::config::IndexMode::Require => auto_index(
            args,
            table_fingerprint,
            table,
            index_home,
            snapshot,
            loaded_manifest,
        ),
    }
}

/// Corpus fraction a plan may select before the index stops paying: above
/// this, candidate verification does strictly more work than a plain scan
/// (measured 97-99 % FP on numeric/version classes selecting 46-84 %).
pub(super) const SCAN_FALLBACK_PCT: usize = 30;
const MIN_SELECTIVITY_CEILING: u64 = 32;

/// `None` means the plan is too unselective for the index — scan instead.
pub(super) fn query_index(
    index: &PostingsIndex,
    keyed: &super::planner::KeyedPlan,
) -> anyhow::Result<Option<BTreeSet<usize>>> {
    let started_at = Instant::now();
    let df = PostingsDf { index };
    let ceiling = selectivity_ceiling(index.doc_count as u64);
    let mut plan = keyed.plan.clone();
    let raw_grams = count_plan_grams(&plan);
    plan.tune(&df, ceiling);
    log::debug!(
        "eg index query: postings plan_grams={} tuned_plan_grams={}",
        raw_grams,
        count_plan_grams(&plan),
    );
    if plan.is_none() {
        log::debug!(
            "eg index query: postings candidates=0 lookup_time={:?} total_query_time={:?}",
            Duration::ZERO,
            started_at.elapsed()
        );
        return Ok(Some(BTreeSet::new()));
    }
    let estimate = estimate_with_forced(index, &plan, &df)?;
    if estimate > ceiling {
        log::debug!(
            "eg index query: estimate {estimate} of {} docs exceeds {SCAN_FALLBACK_PCT}%; rejecting indexed query without scan fallback",
            index.doc_count
        );
        return Ok(None);
    }
    let lookup_started_at = Instant::now();
    let tuned = super::planner::KeyedPlan {
        plan,
        key: keyed.key,
    };
    let mut candidates = eval_plan(index, &tuned)?;
    candidates = union_sorted(candidates, index.lookup(FORCED_CANDIDATE_HASH)?);
    log::debug!(
        "eg index query: postings candidates={} lookup_time={:?} total_query_time={:?}",
        candidates.len(),
        lookup_started_at.elapsed(),
        started_at.elapsed()
    );
    candidates
        .into_iter()
        .map(Ok)
        .collect::<anyhow::Result<BTreeSet<usize>>>()
        .map(Some)
}

pub(super) fn selectivity_ceiling(doc_count: u64) -> u64 {
    doc_count
        .saturating_mul(SCAN_FALLBACK_PCT as u64)
        .checked_div(100)
        .unwrap_or(0)
        .max(MIN_SELECTIVITY_CEILING)
        .min(doc_count)
}

/// Estimated verification set, including files deliberately forced through
/// exact matching because they have no gram postings.
fn estimate_with_forced(
    index: &PostingsIndex,
    plan: &QueryPlan,
    df: &PostingsDf<'_>,
) -> anyhow::Result<u64> {
    if plan.is_none() {
        return Ok(0);
    }
    let forced = index.posting_len(FORCED_CANDIDATE_HASH)? as u64;
    Ok(plan
        .estimate_candidates(df)
        .saturating_add(forced)
        .min(index.doc_count as u64))
}

/// Posting-list lengths as document-frequency priors.
struct PostingsDf<'a> {
    index: &'a PostingsIndex,
}

impl DfStats for PostingsDf<'_> {
    fn doc_count(&self, space: GramSpace, gram: &Gram) -> u64 {
        self.index
            .posting_len(gram.hash_keyed(hash_key_for(space)))
            .unwrap_or(0) as u64
    }

    fn total_docs(&self) -> u64 {
        self.index.doc_count as u64
    }
}

const fn hash_key_for(space: GramSpace) -> HashKey {
    match space {
        GramSpace::Primary => HashKey::UNKEYED,
        GramSpace::Folded => HashKey::UNKEYED.folded(),
    }
}

fn auto_index(
    args: &HiArgs,
    table_fingerprint: u64,
    table: &WeightTable,
    index_home: &Path,
    snapshot: &CurrentSnapshot,
    loaded_manifest: Option<&Manifest>,
) -> anyhow::Result<PostingsIndex> {
    sweep_orphans(index_home)?;
    let manifest_path = index_home.join(MANIFEST_FILE_NAME);
    if !index_home.join(TABLE_FILE_NAME).exists()
        || !index_home.join(POSTINGS_FILE_NAME).exists()
        || !manifest_present(&manifest_path)
    {
        return rebuild_index(args, table_fingerprint, table, index_home, snapshot);
    }
    let base_manifest_storage;
    let base_manifest = if let Some(manifest) = loaded_manifest {
        manifest
    } else {
        base_manifest_storage = match read_manifest(&manifest_path)? {
            Some(manifest) => manifest,
            None => return rebuild_index(args, table_fingerprint, table, index_home, snapshot),
        };
        &base_manifest_storage
    };
    let expected = manifest_for(ManifestBackend::Postings, table_fingerprint, snapshot);
    let Some(changed) = changed_ordinals(base_manifest, &expected) else {
        return rebuild_index(args, table_fingerprint, table, index_home, snapshot);
    };
    if changed.is_empty() {
        remove_delta(index_home)?;
        return open_or_rebuild(args, table_fingerprint, table, index_home, snapshot, false);
    }
    if changed.len() > MAX_DELTA_FILES {
        log::debug!(
            "eg index: {} changed files hit the MAX_DELTA_FILES={MAX_DELTA_FILES} cliff; full rebuild",
            changed.len()
        );
        return rebuild_index(args, table_fingerprint, table, index_home, snapshot);
    }
    if delta_should_fold(changed.len(), base_manifest.file_count()) {
        log::debug!(
            "eg index: delta {} of {} base files exceeds {DELTA_FOLD_PCT}%; folding into base",
            changed.len(),
            base_manifest.file_count()
        );
        return rebuild_index(args, table_fingerprint, table, index_home, snapshot);
    }
    build_delta_if_stale(args, table, index_home, snapshot, &changed, &expected)?;
    open_or_rebuild(args, table_fingerprint, table, index_home, snapshot, true)
}

/// Fraction of the base file count a delta may reach before it is folded into
/// a fresh base: past this, stale base postings dominate as false candidates
/// and a full rebuild is cheaper than an ever-growing delta.
const DELTA_FOLD_PCT: usize = 25;
/// Do not fold tiny deltas solely because a tiny corpus makes their percentage
/// look large; the rebuild avoidance matters more than stale-posting noise at
/// this scale.
const MIN_DELTA_FOLD_FILES: usize = 64;

/// Return true when the delta has grown past the fold-into-base threshold.
fn delta_should_fold(changed: usize, base_files: usize) -> bool {
    changed >= MIN_DELTA_FOLD_FILES
        && base_files > 0
        && changed.saturating_mul(100) > base_files.saturating_mul(DELTA_FOLD_PCT)
}

/// Build the delta segment unless a matching one is already committed.
fn build_delta_if_stale(
    args: &HiArgs,
    table: &WeightTable,
    index_home: &Path,
    snapshot: &CurrentSnapshot,
    changed: &[usize],
    expected: &Manifest,
) -> anyhow::Result<()> {
    let delta_manifest_path = index_home.join(DELTA_MANIFEST_FILE_NAME);
    let delta_ready = index_home.join(DELTA_TABLE_FILE_NAME).exists()
        && index_home.join(DELTA_POSTINGS_FILE_NAME).exists()
        && read_manifest(&delta_manifest_path)?
            .as_ref()
            .is_some_and(|manifest| changed_ordinals(manifest, expected) == Some(Vec::new()));
    if delta_ready {
        return Ok(());
    }
    let _lock = acquire_build_lock(index_home)?;
    let changed_files = changed
        .iter()
        .map(|&ord| {
            snapshot
                .files
                .get(ord)
                .with_context(|| format!("manifest changed file ordinal {ord} is out of range"))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    build_files(
        args,
        table,
        index_home,
        &changed_files,
        DELTA_TABLE_FILE_NAME,
        DELTA_POSTINGS_FILE_NAME,
    )?;
    write_manifest(&delta_manifest_path, expected)?;
    fsync_dir(index_home)?;
    Ok(())
}

/// Open the index, rebuilding it when a segment is missing or corrupt.
fn open_or_rebuild(
    args: &HiArgs,
    table_fingerprint: u64,
    table: &WeightTable,
    index_home: &Path,
    snapshot: &CurrentSnapshot,
    delta: bool,
) -> anyhow::Result<PostingsIndex> {
    let Some(index) = PostingsIndex::open(index_home, snapshot.files.len(), delta)? else {
        log::debug!(
            "eg index: corrupt index at {}, rebuilding",
            index_home.display()
        );
        return rebuild_index(args, table_fingerprint, table, index_home, snapshot);
    };
    Ok(index)
}

fn rebuild_index(
    args: &HiArgs,
    table_fingerprint: u64,
    table: &WeightTable,
    index_home: &Path,
    snapshot: &CurrentSnapshot,
) -> anyhow::Result<PostingsIndex> {
    let _lock = acquire_build_lock(index_home)?;
    recover_interrupted_rebuild(index_home)?;
    let staging = suffixed_path(index_home, TEMP_SUFFIX);
    remove_dir_all_if_exists(&staging)?;
    fs::create_dir_all(&staging)
        .with_context(|| format!("failed to create index directory {}", staging.display()))?;
    let file_refs = snapshot.files.iter().collect::<Vec<_>>();
    build_files(
        args,
        table,
        &staging,
        &file_refs,
        TABLE_FILE_NAME,
        POSTINGS_FILE_NAME,
    )?;
    write_manifest(
        &staging.join(MANIFEST_FILE_NAME),
        &manifest_for(ManifestBackend::Postings, table_fingerprint, snapshot),
    )?;
    fsync_dir(&staging)?;
    swap_in(&staging, index_home)?;
    PostingsIndex::open(index_home, snapshot.files.len(), false)?
        .with_context(|| format!("index at {} corrupt after rebuild", index_home.display()))
}

/// Rebuild the postings index in place, for `--index=repair`.
pub(super) fn rebuild(
    args: &HiArgs,
    table_fingerprint: u64,
    table: &WeightTable,
    index_home: &Path,
    snapshot: &CurrentSnapshot,
) -> anyhow::Result<()> {
    rebuild_index(args, table_fingerprint, table, index_home, snapshot).map(|_| ())
}

/// Result of an index integrity check: one pass/fail line per check.
pub(super) struct VerifyReport {
    checks: Vec<(String, bool)>,
}

impl VerifyReport {
    /// True when every check passed.
    pub(super) fn healthy(&self) -> bool {
        self.checks.iter().all(|(_, ok)| *ok)
    }

    /// One human-readable line per check.
    pub(super) fn lines(&self) -> Vec<String> {
        self.checks
            .iter()
            .map(|(desc, ok)| format!("  [{}] {desc}", if *ok { "ok" } else { "FAIL" }))
            .collect()
    }
}

/// Check the postings index for structural faults without searching: manifest
/// presence and compatibility, section headers and sampled checksums, delta
/// completeness, and leftover build artifacts.
pub(super) fn verify_index(
    index_home: &Path,
    table_fingerprint: u64,
) -> anyhow::Result<VerifyReport> {
    let mut checks = Vec::new();
    match read_manifest(&index_home.join(MANIFEST_FILE_NAME))? {
        Some(manifest) => {
            checks.push(("manifest present and parses".to_owned(), true));
            checks.push((
                "manifest matches the selected weight table".to_owned(),
                is_compatible(&manifest, ManifestBackend::Postings, table_fingerprint),
            ));
        },
        None => checks.push(("manifest present and parses".to_owned(), false)),
    }
    let base_ok = Segment::open(
        &index_home.join(TABLE_FILE_NAME),
        &index_home.join(POSTINGS_FILE_NAME),
    )?
    .is_some();
    checks.push((
        "base sections verify (magic, version, checksum, layout)".to_owned(),
        base_ok,
    ));
    verify_delta(index_home, &mut checks)?;
    checks.push((
        "no orphaned run directory".to_owned(),
        !index_home.join(RUNS_DIR_NAME).exists(),
    ));
    checks.push((
        "no interrupted rebuild staging".to_owned(),
        !suffixed_path(index_home, TEMP_SUFFIX).exists(),
    ));
    Ok(VerifyReport { checks })
}

/// Add delta-segment checks: complete when all three files are present and the
/// sections verify, absent when none are, torn otherwise.
fn verify_delta(index_home: &Path, checks: &mut Vec<(String, bool)>) -> anyhow::Result<()> {
    let manifest = manifest_present(&index_home.join(DELTA_MANIFEST_FILE_NAME));
    let table = index_home.join(DELTA_TABLE_FILE_NAME).exists();
    let postings = index_home.join(DELTA_POSTINGS_FILE_NAME).exists();
    if !manifest && !table && !postings {
        return Ok(());
    }
    if !(manifest && table && postings) {
        checks.push(("delta segment is complete".to_owned(), false));
        return Ok(());
    }
    let delta_ok = Segment::open(
        &index_home.join(DELTA_TABLE_FILE_NAME),
        &index_home.join(DELTA_POSTINGS_FILE_NAME),
    )?
    .is_some();
    checks.push(("delta sections verify".to_owned(), delta_ok));
    Ok(())
}

/// Advisory exclusive lock so concurrent builds serialize; freed on drop.
struct BuildLock {
    _file: File,
}

/// Acquire the advisory build lock, blocking until it is available.
fn acquire_build_lock(index_home: &Path) -> anyhow::Result<BuildLock> {
    let file = open_lock_file(index_home)?;
    file.lock()
        .with_context(|| format!("failed to lock index build for {}", index_home.display()))?;
    Ok(BuildLock { _file: file })
}

/// Acquire the build lock without blocking, returning `None` if it is held.
fn try_build_lock(index_home: &Path) -> anyhow::Result<Option<BuildLock>> {
    let file = open_lock_file(index_home)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(BuildLock { _file: file })),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(err)) => Err(err)
            .with_context(|| format!("failed to lock index build for {}", index_home.display())),
    }
}

fn open_lock_file(index_home: &Path) -> anyhow::Result<File> {
    let lock_path = suffixed_path(index_home, LOCK_SUFFIX);
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create index directory {}", parent.display()))?;
    }
    File::create(&lock_path)
        .with_context(|| format!("failed to open build lock {}", lock_path.display()))
}

/// Restore an intact index after a crash during the rename swap.
fn recover_interrupted_rebuild(index_home: &Path) -> anyhow::Result<()> {
    let old = suffixed_path(index_home, OLD_SUFFIX);
    if !index_home.exists() && old.exists() {
        fs::rename(&old, index_home).with_context(|| {
            format!(
                "failed to restore index {} from {}",
                index_home.display(),
                old.display()
            )
        })?;
    }
    remove_dir_all_if_exists(&suffixed_path(index_home, TEMP_SUFFIX))?;
    remove_dir_all_if_exists(&old)
}

/// Swap a freshly built staging directory into place without losing the old index.
fn swap_in(staging: &Path, index_home: &Path) -> anyhow::Result<()> {
    let old = suffixed_path(index_home, OLD_SUFFIX);
    remove_dir_all_if_exists(&old)?;
    if index_home.exists() {
        fs::rename(index_home, &old)
            .with_context(|| format!("failed to move old index {}", index_home.display()))?;
    }
    fs::rename(staging, index_home).with_context(|| {
        format!(
            "failed to install new index {} from {}",
            index_home.display(),
            staging.display()
        )
    })?;
    if let Some(parent) = index_home.parent() {
        fsync_dir(parent)?;
    }
    remove_dir_all_if_exists(&old)
}

/// Remove orphaned run and temp directories and torn delta files on open.
fn sweep_orphans(index_home: &Path) -> anyhow::Result<()> {
    let Some(_lock) = try_build_lock(index_home)? else {
        return Ok(());
    };
    recover_interrupted_rebuild(index_home)?;
    remove_dir_all_if_exists(&index_home.join(RUNS_DIR_NAME))?;
    let present = u8::from(manifest_present(&index_home.join(DELTA_MANIFEST_FILE_NAME)))
        + u8::from(index_home.join(DELTA_TABLE_FILE_NAME).exists())
        + u8::from(index_home.join(DELTA_POSTINGS_FILE_NAME).exists());
    if present != 0 && present != 3 {
        log::debug!("eg index: sweeping torn delta in {}", index_home.display());
        remove_delta(index_home)?;
    }
    Ok(())
}

/// Return a sibling path formed by appending a suffix to the index directory.
fn suffixed_path(index_home: &Path, suffix: &str) -> PathBuf {
    let name = index_home.file_name().map_or_else(
        || String::from("index"),
        |name| name.to_string_lossy().into_owned(),
    );
    index_home.with_file_name(format!("{name}{suffix}"))
}

fn remove_dir_all_if_exists(path: &Path) -> anyhow::Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("failed to remove {}", path.display())),
    }
}

/// Flush a directory's metadata so renamed entries survive a crash.
#[cfg(unix)]
fn fsync_dir(dir: &Path) -> anyhow::Result<()> {
    File::open(dir)
        .and_then(|file| file.sync_all())
        .with_context(|| format!("failed to fsync directory {}", dir.display()))
}

#[cfg(not(unix))]
fn fsync_dir(_dir: &Path) -> anyhow::Result<()> {
    Ok(())
}

fn build_files(
    args: &HiArgs,
    table: &WeightTable,
    index_home: &Path,
    files: &[&CurrentFile],
    table_name: &str,
    postings_name: &str,
) -> anyhow::Result<()> {
    let started_at = Instant::now();
    let pair_budget = pairs_per_run(args.threads());
    log::debug!(
        "eg index build: postings files={} ram_cap={}MiB pair_budget_per_worker={} mmap={} table={} postings={}",
        files.len(),
        INDEX_RAM_CAP_BYTES / 1024 / 1024,
        pair_budget,
        args.index_mmap(),
        table_name,
        postings_name
    );
    let runs_dir = index_home.join(RUNS_DIR_NAME);
    if runs_dir.exists() {
        fs::remove_dir_all(&runs_dir).with_context(|| {
            format!("failed to remove old run directory {}", runs_dir.display())
        })?;
    }
    fs::create_dir_all(&runs_dir)
        .with_context(|| format!("failed to create run directory {}", runs_dir.display()))?;
    let next_run = AtomicUsize::new(0);
    let stats = BuildStats::default();
    let scan_started_at = Instant::now();
    files
        .par_chunks(FILES_PER_RAYON_TASK)
        .try_for_each(|chunk| {
            write_chunk_runs(
                args,
                table,
                chunk,
                &runs_dir,
                &next_run,
                &stats,
                pair_budget,
            )
        })?;
    let scan_elapsed = scan_started_at.elapsed();
    let run_count = next_run.load(AtomicOrdering::Relaxed);
    log::debug!("eg index build: scan phase done in {scan_elapsed:?}; merging {run_count} runs",);
    let merge_started_at = Instant::now();
    merge_runs(
        &runs_dir,
        run_count,
        &index_home.join(table_name),
        &index_home.join(postings_name),
    )?;
    let merge_elapsed = merge_started_at.elapsed();
    fs::remove_dir_all(&runs_dir)
        .with_context(|| format!("failed to remove run directory {}", runs_dir.display()))?;
    let table_bytes = file_len(&index_home.join(table_name))?;
    let postings_bytes = file_len(&index_home.join(postings_name))?;
    log::debug!(
        "eg index build: postings done files={} bytes={} emitted={} selected={} forced={} runs={} run_bytes={} table_bytes={} postings_bytes={} scan_write_time={:?} merge_time={:?} total_time={:?}",
        stats.files.load(AtomicOrdering::Relaxed),
        stats.bytes.load(AtomicOrdering::Relaxed),
        stats.emitted.load(AtomicOrdering::Relaxed),
        stats.selected.load(AtomicOrdering::Relaxed),
        stats.forced.load(AtomicOrdering::Relaxed),
        stats.runs.load(AtomicOrdering::Relaxed),
        stats.run_bytes.load(AtomicOrdering::Relaxed),
        table_bytes,
        postings_bytes,
        scan_elapsed,
        merge_elapsed,
        started_at.elapsed()
    );
    Ok(())
}

fn write_chunk_runs(
    args: &HiArgs,
    table: &WeightTable,
    files: &[&CurrentFile],
    runs_dir: &Path,
    next_run: &AtomicUsize,
    stats: &BuildStats,
    pair_budget: usize,
) -> anyhow::Result<()> {
    let mut pairs = Vec::with_capacity(pair_budget.min(64 * 1024));
    for file in files {
        scan_file_pairs(table, file, args.index_mmap(), &mut pairs, stats)?;
        if pairs.len() >= pair_budget {
            write_run(runs_dir, next_run, &mut pairs, stats)?;
        }
    }
    if !pairs.is_empty() {
        write_run(runs_dir, next_run, &mut pairs, stats)?;
    }
    Ok(())
}

fn scan_file_pairs(
    table: &WeightTable,
    file: &CurrentFile,
    use_mmap: bool,
    pairs: &mut Vec<Pair>,
    stats: &BuildStats,
) -> anyhow::Result<()> {
    let metadata = fs::metadata(&file.path)
        .with_context(|| format!("failed to stat {} for indexing", file.path.display()))?;
    let len = metadata.len();
    let scanned = stats.files.fetch_add(1, AtomicOrdering::Relaxed) + 1;
    if scanned.is_multiple_of(BUILD_PROGRESS_EVERY) {
        log::debug!("eg index build: scanned {scanned} files");
    }
    stats
        .bytes
        .fetch_add(usize_len(len), AtomicOrdering::Relaxed);
    if len == 0 {
        return Ok(());
    }
    let ord = u32::try_from(file.ord).context("indexed document ordinal does not fit in u32")?;
    if super::classify::is_oversized(len) {
        log::debug!(
            "eg index: forcing oversized file {} ({len} bytes) as candidate",
            file.path.display()
        );
        push_forced(pairs, ord, stats);
        return Ok(());
    }
    let bytes = read_file(&file.path, use_mmap)?;
    classify_and_collect(table, bytes.as_ref(), ord, pairs, stats)?;
    Ok(())
}

/// Emit grams for a readable text file, forcing encoded/high-entropy files.
fn classify_and_collect(
    table: &WeightTable,
    bytes: &[u8],
    ord: u32,
    pairs: &mut Vec<Pair>,
    stats: &BuildStats,
) -> anyhow::Result<()> {
    if super::classify::is_binary(bytes) {
        return Ok(());
    }
    let mut forced = super::classify::has_decoding_bom(bytes);
    if forced {
        push_forced(pairs, ord, stats);
        return Ok(());
    }
    let mut emitted = 0usize;
    let mut primary = Vec::new();
    let mut folded = Vec::new();
    let scan = sngram::scan(table, Cursor::new(bytes), |event| {
        if let ScanEvent::Gram(gram) = event {
            emitted += 1;
            match gram.space {
                GramSpace::Primary => primary.push(gram.hash),
                GramSpace::Folded => folded.push(gram.hash),
            }
        }
    });
    if matches!(scan, Err(ScanError::Binary)) {
        return Ok(());
    }
    scan?;
    primary.sort_unstable();
    primary.dedup();
    if super::classify::is_high_entropy(bytes.len(), primary.len()) {
        forced = true;
    }
    let mut hashes = if forced {
        push_forced(pairs, ord, stats);
        Vec::new()
    } else {
        folded.sort_unstable();
        primary.extend(folded);
        primary
    };
    if !forced {
        hashes.dedup();
    }
    let selected = hashes.len();
    pairs.extend(hashes.into_iter().map(|hash| Pair { hash, ord }));
    stats.emitted.fetch_add(emitted, AtomicOrdering::Relaxed);
    stats.selected.fetch_add(selected, AtomicOrdering::Relaxed);
    Ok(())
}

/// Record a forced-candidate posting for a file whose grams are not indexed.
fn push_forced(pairs: &mut Vec<Pair>, ord: u32, stats: &BuildStats) {
    pairs.push(Pair {
        hash: FORCED_CANDIDATE_HASH,
        ord,
    });
    stats.forced.fetch_add(1, AtomicOrdering::Relaxed);
}

/// Convert a file length to `usize` for stats without overflow on 32-bit.
fn usize_len(len: u64) -> usize {
    usize::try_from(len).unwrap_or(usize::MAX)
}

fn write_run(
    runs_dir: &Path,
    next_run: &AtomicUsize,
    pairs: &mut Vec<Pair>,
    stats: &BuildStats,
) -> anyhow::Result<()> {
    pairs.sort_unstable();
    pairs.dedup();
    let pair_count = pairs.len();
    let id = next_run.fetch_add(1, AtomicOrdering::Relaxed);
    let path = run_path(runs_dir, id);
    let file =
        File::create(&path).with_context(|| format!("failed to create run {}", path.display()))?;
    let mut writer = BufWriter::new(file);
    for pair in pairs.drain(..) {
        write_pair(&mut writer, pair)?;
    }
    writer
        .flush()
        .with_context(|| format!("failed to flush run {}", path.display()))?;
    stats.runs.fetch_add(1, AtomicOrdering::Relaxed);
    stats
        .run_bytes
        .fetch_add(pair_count * RUN_PAIR_SIZE, AtomicOrdering::Relaxed);
    log::trace!(
        "eg index build: wrote postings run id={} pairs={} bytes={}",
        id,
        pair_count,
        pair_count * RUN_PAIR_SIZE
    );
    Ok(())
}

/// Merge sorted runs into the final table and postings sections.
///
/// This is a single-threaded k-way heap merge on purpose: it streams one
/// monotonic key sequence into two append-only sections, so it is I/O-bound
/// rather than CPU-bound. A per-shard parallel merge would need the scan phase
/// to range-partition runs by hash so shards are disjoint and concatenable;
/// the current runs span the whole hash space, so parallelizing here would add
/// a repartition pass and coordination that outweighs the streaming win at
/// present scale. The scan phase, which is CPU-bound, is already parallel.
fn merge_runs(
    runs_dir: &Path,
    run_count: usize,
    table_path: &Path,
    postings_path: &Path,
) -> anyhow::Result<()> {
    let mut table_writer = SectionWriter::create(table_path, TABLE_MAGIC)?;
    let mut postings_writer = SectionWriter::create(postings_path, POSTINGS_MAGIC)?;
    let mut readers = Vec::with_capacity(run_count);
    let mut heap = BinaryHeap::new();
    for run_id in 0..run_count {
        let path = run_path(runs_dir, run_id);
        let mut reader = RunReader::open(&path)?;
        if let Some(pair) = reader.next_pair()? {
            heap.push(HeapItem { pair, run_id });
        }
        readers.push(reader);
    }

    let mut current_hash = None;
    let mut docs = Vec::<u32>::new();
    while let Some(item) = heap.pop() {
        if current_hash != Some(item.pair.hash) {
            if let Some(hash) = current_hash {
                flush_posting(&mut table_writer, &mut postings_writer, hash, &docs)?;
                docs.clear();
            }
            current_hash = Some(item.pair.hash);
        }
        if docs.last().copied() != Some(item.pair.ord) {
            docs.push(item.pair.ord);
        }
        let reader = readers
            .get_mut(item.run_id)
            .context("merge run index out of range")?;
        if let Some(pair) = reader.next_pair()? {
            heap.push(HeapItem {
                pair,
                run_id: item.run_id,
            });
        }
    }
    if let Some(hash) = current_hash {
        flush_posting(&mut table_writer, &mut postings_writer, hash, &docs)?;
    }
    table_writer.finalize(TABLE_RECORD_SIZE as u64)?;
    postings_writer.finalize(POSTING_SIZE as u64)?;
    Ok(())
}

fn flush_posting(
    table_writer: &mut SectionWriter,
    postings_writer: &mut SectionWriter,
    hash: u64,
    docs: &[u32],
) -> anyhow::Result<()> {
    let len = u32::try_from(docs.len()).context("posting list length does not fit in u32")?;
    let offset = postings_writer.body_len;
    write_table_record(table_writer, hash, offset, len)?;
    for &doc in docs {
        postings_writer.write_all(&doc.to_le_bytes())?;
    }
    Ok(())
}

fn run_path(runs_dir: &Path, id: usize) -> PathBuf {
    runs_dir.join(format!("{id:08}.run"))
}

fn pairs_per_run(threads: usize) -> usize {
    let threads = threads.max(1);
    (INDEX_RAM_CAP_BYTES / threads / mem::size_of::<Pair>())
        .clamp(MIN_PAIRS_PER_RUN, MAX_PAIRS_PER_RUN)
}

fn file_len(path: &Path) -> anyhow::Result<u64> {
    fs::metadata(path)
        .map(|metadata| metadata.len())
        .with_context(|| format!("failed to stat {}", path.display()))
}

fn remove_delta(index_home: &Path) -> anyhow::Result<()> {
    remove_manifest(&index_home.join(DELTA_MANIFEST_FILE_NAME))?;
    for name in [DELTA_TABLE_FILE_NAME, DELTA_POSTINGS_FILE_NAME] {
        let path = index_home.join(name);
        match fs::remove_file(&path) {
            Ok(()) => {},
            Err(err) if err.kind() == io::ErrorKind::NotFound => {},
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("failed to remove delta file {}", path.display()));
            },
        }
    }
    Ok(())
}

fn read_file(path: &Path, use_mmap: bool) -> anyhow::Result<FileBytes> {
    if use_mmap {
        mmap_file(path).map(FileBytes::Mmap)
    } else {
        fs::read(path)
            .map(FileBytes::Owned)
            .with_context(|| format!("failed to read {} for indexing", path.display()))
    }
}

#[allow(unsafe_code)]
fn mmap_file(path: &Path) -> anyhow::Result<Mmap> {
    let file = File::open(path)
        .with_context(|| format!("failed to open {} for mmap indexing", path.display()))?;
    unsafe { MmapOptions::new().map(&file) }
        .with_context(|| format!("failed to mmap {} for indexing", path.display()))
}

enum FileBytes {
    Mmap(Mmap),
    Owned(Vec<u8>),
}

impl AsRef<[u8]> for FileBytes {
    fn as_ref(&self) -> &[u8] {
        match self {
            Self::Mmap(bytes) => bytes,
            Self::Owned(bytes) => bytes,
        }
    }
}

fn count_plan_grams(plan: &QueryPlan) -> usize {
    plan.gram_count()
}

/// Posting lists shared between plan nodes: case-folded plans repeat the
/// same gram across many OR branches, so each unique gram is fetched and
/// decoded once per query.
type PostingCache = HashMap<u64, Rc<Vec<usize>>>;

fn eval_plan(
    index: &PostingsIndex,
    keyed: &super::planner::KeyedPlan,
) -> anyhow::Result<Vec<usize>> {
    let mut cache = PostingCache::new();
    eval_plan_cached(index, &keyed.plan, keyed.key, &mut cache)
}

fn lookup_cached(
    index: &PostingsIndex,
    cache: &mut PostingCache,
    hash: u64,
) -> anyhow::Result<Rc<Vec<usize>>> {
    if let Some(list) = cache.get(&hash) {
        return Ok(Rc::clone(list));
    }
    let list = Rc::new(index.lookup(hash)?);
    cache.insert(hash, Rc::clone(&list));
    Ok(list)
}

fn eval_plan_cached(
    index: &PostingsIndex,
    plan: &QueryPlan,
    key: HashKey,
    cache: &mut PostingCache,
) -> anyhow::Result<Vec<usize>> {
    eval_expr_cached(index, plan.expr(), key, cache)
}

fn eval_expr_cached(
    index: &PostingsIndex,
    expr: &QueryExpr,
    key: HashKey,
    cache: &mut PostingCache,
) -> anyhow::Result<Vec<usize>> {
    match expr {
        QueryExpr::All => {
            anyhow::bail!("indexed query has no sparse n-gram constraints; use --no-index")
        },
        QueryExpr::None => Ok(Vec::new()),
        QueryExpr::And { grams, sub } => {
            let mut lists = Vec::with_capacity(grams.len() + sub.len());
            for gram in grams {
                lists.push(lookup_cached(index, cache, gram.hash_keyed(key))?);
            }
            for expr in sub {
                lists.push(Rc::new(eval_expr_cached(index, expr, key, cache)?));
            }
            intersect_all_sorted(index.doc_count, lists)
        },
        QueryExpr::Or { grams, sub } => {
            // Concatenate every branch then sort+dedup once: folding the
            // union pairwise re-walks the accumulator per branch, quadratic
            // in branch count for the wide ORs case-folded plans build.
            let mut acc = Vec::new();
            for gram in grams {
                acc.extend_from_slice(&lookup_cached(index, cache, gram.hash_keyed(key))?);
            }
            for expr in sub {
                acc.extend(eval_expr_cached(index, expr, key, cache)?);
            }
            acc.sort_unstable();
            acc.dedup();
            Ok(acc)
        },
    }
}

fn intersect_all_sorted(
    doc_count: usize,
    mut lists: Vec<Rc<Vec<usize>>>,
) -> anyhow::Result<Vec<usize>> {
    lists.sort_by_key(|list| list.len());
    let mut iter = lists.into_iter();
    let Some(first) = iter.next() else {
        return Ok((0..doc_count).collect());
    };
    let mut acc = first.as_ref().clone();
    for list in iter {
        acc = intersect_sorted(&acc, &list);
        if acc.is_empty() {
            break;
        }
    }
    Ok(acc)
}

fn intersect_sorted(left: &[usize], right: &[usize]) -> Vec<usize> {
    let mut out = Vec::new();
    let mut i = 0;
    let mut j = 0;
    while i < left.len() && j < right.len() {
        match left[i].cmp(&right[j]) {
            Ordering::Less => i += 1,
            Ordering::Greater => j += 1,
            Ordering::Equal => {
                out.push(left[i]);
                i += 1;
                j += 1;
            },
        }
    }
    out
}

fn union_sorted(left: Vec<usize>, right: Vec<usize>) -> Vec<usize> {
    union_sorted_ref(&left, &right)
}

fn union_sorted_ref(left: &[usize], right: &[usize]) -> Vec<usize> {
    let mut out = Vec::with_capacity(left.len() + right.len());
    let mut i = 0;
    let mut j = 0;
    while i < left.len() && j < right.len() {
        match left[i].cmp(&right[j]) {
            Ordering::Less => {
                out.push(left[i]);
                i += 1;
            },
            Ordering::Greater => {
                out.push(right[j]);
                j += 1;
            },
            Ordering::Equal => {
                out.push(left[i]);
                i += 1;
                j += 1;
            },
        }
    }
    out.extend_from_slice(&left[i..]);
    out.extend_from_slice(&right[j..]);
    out
}

pub(super) struct PostingsIndex {
    base: Segment,
    delta: Option<Segment>,
    doc_count: usize,
}

impl PostingsIndex {
    /// Open the index, returning `None` when a segment is missing or corrupt.
    fn open(index_home: &Path, doc_count: usize, delta: bool) -> anyhow::Result<Option<Self>> {
        let Some(base) = Segment::open(
            &index_home.join(TABLE_FILE_NAME),
            &index_home.join(POSTINGS_FILE_NAME),
        )?
        else {
            return Ok(None);
        };
        let delta = if delta {
            match Segment::open(
                &index_home.join(DELTA_TABLE_FILE_NAME),
                &index_home.join(DELTA_POSTINGS_FILE_NAME),
            )? {
                Some(segment) => Some(segment),
                None => return Ok(None),
            }
        } else {
            None
        };
        Ok(Some(Self {
            base,
            delta,
            doc_count,
        }))
    }

    fn lookup(&self, hash: u64) -> anyhow::Result<Vec<usize>> {
        let mut out = self.base.lookup(hash)?;
        if let Some(delta) = &self.delta {
            out = union_sorted(out, delta.lookup(hash)?);
        }
        Ok(out)
    }

    /// Posting-list length without decoding: the df prior for the cost model.
    fn posting_len(&self, hash: u64) -> anyhow::Result<usize> {
        let mut len = self.base.posting_len(hash)?;
        if let Some(delta) = &self.delta {
            len += delta.posting_len(hash)?;
        }
        Ok(len)
    }
}

struct Segment {
    table: Mmap,
    postings: Mmap,
}

impl Segment {
    /// Open and verify both files, returning `None` on any structural fault.
    fn open(table_path: &Path, postings_path: &Path) -> anyhow::Result<Option<Self>> {
        let Some(table) = open_section(table_path, TABLE_MAGIC, TABLE_RECORD_SIZE)? else {
            return Ok(None);
        };
        let Some(postings) = open_section(postings_path, POSTINGS_MAGIC, POSTING_SIZE)? else {
            return Ok(None);
        };
        Ok(Some(Self { table, postings }))
    }

    fn table_body(&self) -> &[u8] {
        self.table.get(SECTION_HEADER_SIZE..).unwrap_or_default()
    }

    fn postings_body(&self) -> &[u8] {
        self.postings.get(SECTION_HEADER_SIZE..).unwrap_or_default()
    }

    /// List length from the table record alone, no posting decode.
    fn posting_len(&self, hash: u64) -> anyhow::Result<usize> {
        let table = self.table_body();
        match find_record(table, hash)? {
            Some((_, len)) => usize::try_from(len).context("posting length does not fit in usize"),
            None => Ok(0),
        }
    }

    fn lookup(&self, hash: u64) -> anyhow::Result<Vec<usize>> {
        let table = self.table_body();
        let postings = self.postings_body();
        let Some((offset, len)) = find_record(table, hash)? else {
            return Ok(Vec::new());
        };
        let len = usize::try_from(len).context("posting length does not fit in usize")?;
        let offset = usize::try_from(offset).context("posting offset does not fit in usize")?;
        let byte_len = len
            .checked_mul(POSTING_SIZE)
            .context("posting byte length overflow")?;
        let end = offset
            .checked_add(byte_len)
            .context("posting byte range overflow")?;
        let Some(region) = postings.get(offset..end) else {
            anyhow::bail!("posting list points past postings file");
        };
        let mut docs = Vec::with_capacity(len);
        for chunk in region.chunks_exact(POSTING_SIZE) {
            let bytes: [u8; POSTING_SIZE] = chunk
                .try_into()
                .map_err(|_| anyhow::anyhow!("posting chunk is not {POSTING_SIZE} bytes"))?;
            docs.push(u32::from_le_bytes(bytes) as usize);
        }
        Ok(docs)
    }
}

/// Memory-map one section file and verify its header, magic, and checksum.
#[allow(unsafe_code)]
fn open_section(path: &Path, magic: [u8; 8], record_size: usize) -> anyhow::Result<Option<Mmap>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(err).with_context(|| format!("failed to open {}", path.display()));
        },
    };
    let len = file
        .metadata()
        .with_context(|| format!("failed to stat {}", path.display()))?
        .len();
    if len < SECTION_HEADER_SIZE as u64 {
        log::debug!(
            "eg index: {} shorter than a header, rebuilding",
            path.display()
        );
        return Ok(None);
    }
    let mmap = unsafe { MmapOptions::new().map(&file) }
        .with_context(|| format!("failed to mmap {}", path.display()))?;
    if let Err(reason) = verify_section(&mmap, magic, record_size) {
        log::debug!("eg index: {} failed verification: {reason}", path.display());
        return Ok(None);
    }
    Ok(Some(mmap))
}

/// Verify a section's magic, version, length, and body checksum.
fn verify_section(mmap: &[u8], magic: [u8; 8], record_size: usize) -> Result<(), String> {
    let header = mmap.get(..SECTION_HEADER_SIZE).ok_or("missing header")?;
    if header.get(..8) != Some(&magic[..]) {
        return Err("bad magic".to_owned());
    }
    let version = read_header_u32(header, 8);
    if version != SECTION_FORMAT_VERSION {
        return Err(format!("unsupported version {version}"));
    }
    let count = read_header_u64(header, 16);
    let checksum = read_header_u64(header, 24);
    let body = mmap.get(SECTION_HEADER_SIZE..).unwrap_or_default();
    if body.len() as u64 != count.saturating_mul(record_size as u64) {
        return Err("length does not match record count".to_owned());
    }
    if sampled_checksum(body) != checksum {
        return Err("checksum mismatch".to_owned());
    }
    Ok(())
}

fn read_header_u32(header: &[u8], offset: usize) -> u32 {
    header
        .get(offset..offset + 4)
        .and_then(|slice| slice.try_into().ok())
        .map_or(0, u32::from_le_bytes)
}

fn read_header_u64(header: &[u8], offset: usize) -> u64 {
    header
        .get(offset..offset + 8)
        .and_then(|slice| slice.try_into().ok())
        .map_or(0, u64::from_le_bytes)
}

/// Recompute the sampled checksum of a just-written body by re-reading its
/// head and tail windows through a fresh read handle.
fn checksum_windows(path: &Path, body_len: u64) -> io::Result<u64> {
    let mut file = File::open(path)?;
    let mut hash = fnv1a_state(FNV_OFFSET, &body_len.to_le_bytes());
    let window = CHECKSUM_WINDOW as u64;
    let mut buf = vec![0u8; CHECKSUM_WINDOW];
    let start = SECTION_HEADER_SIZE as u64;
    if body_len <= 2 * window {
        file.seek(SeekFrom::Start(start))?;
        let mut body = vec![0u8; usize::try_from(body_len).unwrap_or(usize::MAX)];
        file.read_exact(&mut body)?;
        return Ok(fnv1a_state(hash, &body));
    }
    file.seek(SeekFrom::Start(start))?;
    file.read_exact(&mut buf)?;
    hash = fnv1a_state(hash, &buf);
    file.seek(SeekFrom::Start(start + body_len - window))?;
    file.read_exact(&mut buf)?;
    Ok(fnv1a_state(hash, &buf))
}

/// Bytes hashed from each end of a section body by the sampled checksum.
const CHECKSUM_WINDOW: usize = 4 * 1024 * 1024;

/// FNV-1a over a running state.
fn fnv1a_state(mut hash: u64, bytes: &[u8]) -> u64 {
    for &byte in bytes {
        hash = (hash ^ u64::from(byte)).wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Sampled body checksum: length, head window, tail window (whole body when
/// small). Catches the real crash artifacts — truncation, torn tails, zeroed
/// pages at either end — in O(window), not O(body): a full-body hash cost
/// 11 s per invocation on a 1 GB index.
fn sampled_checksum(body: &[u8]) -> u64 {
    let mut hash = fnv1a_state(FNV_OFFSET, &(body.len() as u64).to_le_bytes());
    if body.len() <= 2 * CHECKSUM_WINDOW {
        return fnv1a_state(hash, body);
    }
    hash = fnv1a_state(hash, body.get(..CHECKSUM_WINDOW).unwrap_or_default());
    let tail_start = body.len() - CHECKSUM_WINDOW;
    fnv1a_state(hash, body.get(tail_start..).unwrap_or_default())
}

/// Binary search the table for `hash`, returning the posting byte offset and
/// list length stored in the record — no per-open reconstruction, because a
/// process-per-query CLI pays any open-time work on every invocation.
fn find_record(table: &[u8], hash: u64) -> anyhow::Result<Option<(u64, u32)>> {
    if !table.len().is_multiple_of(TABLE_RECORD_SIZE) {
        anyhow::bail!("index table has invalid length");
    }
    let mut lo = 0usize;
    let mut hi = table.len() / TABLE_RECORD_SIZE;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let mid_hash = read_u64_at(table, mid * TABLE_RECORD_SIZE)?;
        match mid_hash.cmp(&hash) {
            Ordering::Less => lo = mid + 1,
            Ordering::Greater => hi = mid,
            Ordering::Equal => {
                return Ok(Some((
                    read_u64_at(table, mid * TABLE_RECORD_SIZE + 8)?,
                    read_u32_at(table, mid * TABLE_RECORD_SIZE + 16)?,
                )));
            },
        }
    }
    Ok(None)
}

fn write_table_record(
    writer: &mut SectionWriter,
    hash: u64,
    offset: u64,
    len: u32,
) -> anyhow::Result<()> {
    writer.write_all(&hash.to_le_bytes())?;
    writer.write_all(&offset.to_le_bytes())?;
    writer.write_all(&len.to_le_bytes())?;
    Ok(())
}

/// Streams a section body under a placeholder header, then finalizes with a
/// magic, record count, and body checksum before flushing to disk.
struct SectionWriter {
    writer: BufWriter<File>,
    body_len: u64,
    magic: [u8; 8],
    path: PathBuf,
}

impl SectionWriter {
    fn create(path: &Path, magic: [u8; 8]) -> anyhow::Result<Self> {
        let mut file =
            File::create(path).with_context(|| format!("failed to create {}", path.display()))?;
        file.write_all(&[0u8; SECTION_HEADER_SIZE])
            .with_context(|| format!("failed to reserve header in {}", path.display()))?;
        Ok(Self {
            writer: BufWriter::new(file),
            body_len: 0,
            magic,
            path: path.to_path_buf(),
        })
    }

    fn write_all(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        self.writer
            .write_all(bytes)
            .with_context(|| format!("failed to write {}", self.path.display()))?;
        self.body_len += bytes.len() as u64;
        Ok(())
    }

    /// Backfill the header and fsync the file so the body is durable.
    fn finalize(self, record_size: u64) -> anyhow::Result<()> {
        let SectionWriter {
            writer,
            body_len,
            magic,
            path,
        } = self;
        let mut file = writer
            .into_inner()
            .with_context(|| format!("failed to flush {}", path.display()))?;
        let checksum = checksum_windows(&path, body_len)
            .with_context(|| format!("failed to checksum {}", path.display()))?;
        let count = body_len / record_size;
        file.seek(SeekFrom::Start(0))
            .with_context(|| format!("failed to seek {}", path.display()))?;
        file.write_all(&section_header(magic, count, checksum))
            .with_context(|| format!("failed to write header {}", path.display()))?;
        file.sync_all()
            .with_context(|| format!("failed to fsync {}", path.display()))
    }
}

/// Encode a 32-byte section header: magic, version, count, and checksum.
fn section_header(magic: [u8; 8], count: u64, checksum: u64) -> [u8; SECTION_HEADER_SIZE] {
    let mut header = [0u8; SECTION_HEADER_SIZE];
    if let Some(slot) = header.get_mut(..8) {
        slot.copy_from_slice(&magic);
    }
    if let Some(slot) = header.get_mut(8..12) {
        slot.copy_from_slice(&SECTION_FORMAT_VERSION.to_le_bytes());
    }
    if let Some(slot) = header.get_mut(16..24) {
        slot.copy_from_slice(&count.to_le_bytes());
    }
    if let Some(slot) = header.get_mut(24..32) {
        slot.copy_from_slice(&checksum.to_le_bytes());
    }
    header
}

fn write_pair(writer: &mut BufWriter<File>, pair: Pair) -> anyhow::Result<()> {
    writer.write_all(&pair.hash.to_le_bytes())?;
    writer.write_all(&pair.ord.to_le_bytes())?;
    Ok(())
}

fn read_pair(reader: &mut BufReader<File>) -> anyhow::Result<Option<Pair>> {
    let mut first = [0u8; 1];
    match reader.read(&mut first)? {
        0 => return Ok(None),
        1 => {},
        _ => unreachable!(),
    }
    let mut rest = [0u8; 11];
    reader.read_exact(&mut rest)?;
    let mut hash = [0u8; 8];
    hash[0] = first[0];
    hash[1..].copy_from_slice(&rest[..7]);
    let mut ord = [0u8; 4];
    ord.copy_from_slice(&rest[7..]);
    Ok(Some(Pair {
        hash: u64::from_le_bytes(hash),
        ord: u32::from_le_bytes(ord),
    }))
}

fn read_u64_at(bytes: &[u8], offset: usize) -> anyhow::Result<u64> {
    let end = offset.checked_add(8).context("u64 read offset overflow")?;
    let Some(slice) = bytes.get(offset..end) else {
        anyhow::bail!("u64 read past end of table");
    };
    Ok(u64::from_le_bytes(slice.try_into().expect("eight bytes")))
}

fn read_u32_at(bytes: &[u8], offset: usize) -> anyhow::Result<u32> {
    let end = offset.checked_add(4).context("u32 read offset overflow")?;
    let Some(slice) = bytes.get(offset..end) else {
        anyhow::bail!("u32 read past end of table");
    };
    Ok(u32::from_le_bytes(slice.try_into().expect("four bytes")))
}

struct RunReader {
    reader: BufReader<File>,
}

impl RunReader {
    fn open(path: &Path) -> anyhow::Result<Self> {
        Ok(Self {
            reader: BufReader::new(
                File::open(path)
                    .with_context(|| format!("failed to open run {}", path.display()))?,
            ),
        })
    }

    fn next_pair(&mut self) -> anyhow::Result<Option<Pair>> {
        read_pair(&mut self.reader)
    }
}

#[derive(Default)]
struct BuildStats {
    files: AtomicUsize,
    bytes: AtomicUsize,
    emitted: AtomicUsize,
    selected: AtomicUsize,
    forced: AtomicUsize,
    runs: AtomicUsize,
    run_bytes: AtomicUsize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Pair {
    hash: u64,
    ord: u32,
}

impl Ord for Pair {
    fn cmp(&self, other: &Self) -> Ordering {
        self.hash.cmp(&other.hash).then(self.ord.cmp(&other.ord))
    }
}

impl PartialOrd for Pair {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct HeapItem {
    pair: Pair,
    run_id: usize,
}

impl Ord for HeapItem {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .pair
            .cmp(&self.pair)
            .then(other.run_id.cmp(&self.run_id))
    }
}

impl PartialOrd for HeapItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        FNV_OFFSET, POSTING_SIZE, POSTINGS_MAGIC, SECTION_HEADER_SIZE, TABLE_MAGIC,
        TABLE_RECORD_SIZE, delta_should_fold, find_record, fnv1a_state, intersect_sorted,
        sampled_checksum, section_header, suffixed_path, union_sorted_ref, verify_section,
    };
    use std::path::Path;

    fn table_body(records: &[(u64, u64, u32)]) -> Vec<u8> {
        let mut body = Vec::new();
        for &(hash, offset, len) in records {
            body.extend_from_slice(&hash.to_le_bytes());
            body.extend_from_slice(&offset.to_le_bytes());
            body.extend_from_slice(&len.to_le_bytes());
        }
        body
    }

    #[test]
    fn find_record_returns_stored_offset_and_len() {
        let p = POSTING_SIZE as u64;
        let records = [(10u64, 0u64, 3u32), (20, 3 * p, 0), (30, 3 * p, 5)];
        let body = table_body(&records);
        assert_eq!(find_record(&body, 10).unwrap(), Some((0, 3)));
        assert_eq!(find_record(&body, 20).unwrap(), Some((3 * p, 0)));
        assert_eq!(find_record(&body, 30).unwrap(), Some((3 * p, 5)));
        assert_eq!(find_record(&body, 25).unwrap(), None);
    }

    #[test]
    fn delta_fold_threshold() {
        assert!(!delta_should_fold(0, 100));
        assert!(!delta_should_fold(1, 1), "tiny deltas stay incremental");
        assert!(
            !delta_should_fold(63, 100),
            "small deltas stay incremental even above the percentage threshold"
        );
        assert!(!delta_should_fold(64, 256), "exactly 25% does not fold");
        assert!(delta_should_fold(65, 256));
        assert!(!delta_should_fold(5, 0), "no base means no fold");
    }

    #[test]
    fn sorted_list_ops_handle_empty_inputs_and_tail_boundaries() {
        assert_eq!(intersect_sorted(&[], &[1, 2]), Vec::<usize>::new());
        assert_eq!(intersect_sorted(&[1, 2, 4, 8], &[0, 2, 8, 9]), vec![2, 8]);
        assert_eq!(union_sorted_ref(&[], &[1, 3]), vec![1, 3]);
        assert_eq!(union_sorted_ref(&[1, 4], &[]), vec![1, 4]);
        assert_eq!(
            union_sorted_ref(&[1, 2, 8], &[0, 2, 9]),
            vec![0, 1, 2, 8, 9]
        );
    }

    fn framed(magic: [u8; 8], record_size: usize, records: usize) -> Vec<u8> {
        let body = vec![0xABu8; record_size * records];
        let mut file = section_header(magic, records as u64, sampled_checksum(&body)).to_vec();
        file.extend_from_slice(&body);
        file
    }

    #[test]
    fn section_roundtrip_verifies() {
        let file = framed(TABLE_MAGIC, TABLE_RECORD_SIZE, 3);
        assert!(verify_section(&file, TABLE_MAGIC, TABLE_RECORD_SIZE).is_ok());
        let empty = framed(POSTINGS_MAGIC, POSTING_SIZE, 0);
        assert!(verify_section(&empty, POSTINGS_MAGIC, POSTING_SIZE).is_ok());
    }

    #[test]
    fn section_detects_body_corruption() {
        let mut file = framed(POSTINGS_MAGIC, POSTING_SIZE, 4);
        if let Some(byte) = file.get_mut(SECTION_HEADER_SIZE + 1) {
            *byte ^= 0xFF;
        }
        assert!(verify_section(&file, POSTINGS_MAGIC, POSTING_SIZE).is_err());
    }

    #[test]
    fn section_detects_bad_magic_and_length() {
        let file = framed(TABLE_MAGIC, TABLE_RECORD_SIZE, 2);
        assert!(verify_section(&file, POSTINGS_MAGIC, POSTING_SIZE).is_err());

        let body = vec![1u8; TABLE_RECORD_SIZE * 2];
        let mut lying = section_header(TABLE_MAGIC, 3, sampled_checksum(&body)).to_vec();
        lying.extend_from_slice(&body);
        assert!(verify_section(&lying, TABLE_MAGIC, TABLE_RECORD_SIZE).is_err());
    }

    #[test]
    fn fnv_is_stable() {
        assert_eq!(fnv1a_state(FNV_OFFSET, b""), FNV_OFFSET);
        assert_eq!(
            fnv1a_state(FNV_OFFSET, b"abc"),
            fnv1a_state(FNV_OFFSET, b"abc")
        );
        assert_ne!(
            fnv1a_state(FNV_OFFSET, b"abc"),
            fnv1a_state(FNV_OFFSET, b"abd")
        );
    }

    #[test]
    fn sampled_checksum_sees_truncation_and_tail_damage() {
        let big = vec![0x5Au8; 9 * 1024 * 1024];
        let full = sampled_checksum(&big);
        let mut tail_damaged = big.clone();
        let at = tail_damaged.len() - 7;
        tail_damaged[at] ^= 0xFF;
        assert_ne!(full, sampled_checksum(&tail_damaged));
        assert_ne!(full, sampled_checksum(&big[..big.len() - 1]));
        let mut head_damaged = big;
        head_damaged[3] ^= 0xFF;
        assert_ne!(full, sampled_checksum(&head_damaged));
    }

    #[test]
    fn suffixed_path_appends_sibling() {
        assert_eq!(
            suffixed_path(Path::new("/a/postings-v3"), ".lock"),
            Path::new("/a/postings-v3.lock")
        );
    }
}
