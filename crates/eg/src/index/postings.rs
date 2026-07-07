//! Compact mmap-backed sparse n-gram postings index.

use std::{
    cell::RefCell,
    cmp::Ordering,
    collections::{BTreeSet, BinaryHeap, HashMap},
    fs::{self, File},
    io::{self, BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    mem,
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering as AtomicOrdering},
    },
    time::{Duration, Instant},
};

use anyhow::Context;
use memmap2::{Mmap, MmapOptions};
use rayon::prelude::*;

use sngram_types::{DfStats, GramKey, GramNeedle, PlanExpr, QueryPlan, ScanNeed, WeightTable};

use crate::flags::HiArgs;

use super::manifest::{
    CurrentFile, CurrentSnapshot, ManifestBackend, manifest_for, write_manifest, write_path_table,
};
use super::progress::{BuildPhase, BuildProgress};
use super::{
    bench,
    executor::{self, PlanBackend},
    summary::{self, SummaryIndex, SummaryRecord},
};

const MANIFEST_FILE_NAME: &str = "manifest.json";
const TABLE_FILE_NAME: &str = "table.bin";
const POSTINGS_FILE_NAME: &str = "postings.bin";
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
/// Table record layout: hash, posting-list byte offset, then posting-list length.
const TABLE_RECORD_SIZE: usize = 20;
const POSTING_SIZE: usize = 4;
const RUN_PAIR_SIZE: usize = 12;
const FILES_PER_RAYON_TASK: usize = 1024;
const INDEX_RAM_CAP_BYTES: usize = 512 * 1024 * 1024;
const MIN_PAIRS_PER_RUN: usize = 128 * 1024;
const MAX_PAIRS_PER_RUN: usize = 4_000_000;
const RUN_READER_BUFFER_BYTES: usize = 64 * 1024;
const SECTION_WRITER_BUFFER_BYTES: usize = 1024 * 1024;
const POSTING_WRITE_BUFFER_BYTES: usize = 16 * 1024;
const FORCED_CANDIDATE_HASH: u64 = u64::MAX;
/// Files scanned between index-build progress lines under `--debug`.
const BUILD_PROGRESS_EVERY: usize = 20_000;
const POSTINGS_MERGE_PROGRESS_EVERY: u64 = 1_000_000;
/// Allow one exact sparse lookup pass for mildly pessimistic estimates.
const SELECTIVITY_REFINE_MULTIPLIER: u64 = 2;

pub fn refresh_index(
    args: &HiArgs,
    table_fingerprint: u64,
    table: &WeightTable,
    index_home: &Path,
    snapshot: &CurrentSnapshot,
    progress: Option<&BuildProgress>,
) -> anyhow::Result<bench::BuildTimings> {
    rebuild_index(
        args,
        table_fingerprint,
        table,
        index_home,
        snapshot,
        progress,
    )
    .map(|(_, timings)| timings)
}

/// Corpus fraction a plan may select before the index stops paying: above
/// this, candidate verification does strictly more work than a plain scan
/// (measured 97-99 % FP on numeric/version classes selecting 46-84 %).
pub const SCAN_FALLBACK_PCT: usize = 30;
const MIN_SELECTIVITY_CEILING: u64 = 32;

/// `None` means the plan is too unselective for the index — scan instead.
pub fn query_index(
    index: &PostingsIndex,
    index_plan: &super::planner::IndexPlan,
    mut bench: Option<&mut bench::BenchReport>,
) -> anyhow::Result<Option<BTreeSet<usize>>> {
    let started_at = Instant::now();
    let df = PostingsDf::new(index);
    let text_count = index.summaries.text_count() as u64;
    let ceiling = selectivity_ceiling(text_count);
    let can_refine_estimate = index_plan.has_root_gram_constraints();
    let mut plan = index_plan.plan.clone();
    let raw_grams = count_plan_grams(&plan);
    let tune_started_at = Instant::now();
    plan.tune(&df, ceiling);
    if let Some(report) = bench.as_deref_mut() {
        report.timing_mut().set_index_tune(tune_started_at);
        report.set_tuned_query_grams(count_plan_grams(&plan));
    }
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
        if !can_refine_estimate || estimate > selectivity_refinement_ceiling(ceiling, text_count) {
            log::debug!(
                "eg index query: estimate {estimate} of {} docs exceeds {SCAN_FALLBACK_PCT}%; rejecting indexed query without scan fallback",
                text_count
            );
            return Ok(None);
        }
        log::debug!(
            "eg index query: refining estimate {estimate} of {} docs with bounded sparse lookup",
            text_count
        );
    }
    let lookup_started_at = Instant::now();
    let candidates = execute_plan(index, &plan)?;
    if let Some(report) = bench.as_deref_mut() {
        report.timing_mut().set_index_execute(lookup_started_at);
    }
    if candidates.len() as u64 > ceiling {
        log::debug!(
            "eg index query: actual candidates {} of {} docs exceeds {SCAN_FALLBACK_PCT}%; rejecting indexed query without scan fallback",
            candidates.len(),
            text_count
        );
        return Ok(None);
    }
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

fn execute_plan(index: &PostingsIndex, plan: &QueryPlan) -> anyhow::Result<Vec<usize>> {
    if let Some(candidates) = FastAllOf::try_execute(index, plan)? {
        let forced = executor::forced_candidates(index, plan)?;
        return Ok(executor::union_sorted(candidates, forced));
    }
    executor::execute(index, plan)
}

pub fn forced_candidate_ordinals(
    index: &PostingsIndex,
    index_plan: &super::planner::IndexPlan,
) -> anyhow::Result<Vec<usize>> {
    executor::forced_candidates(index, &index_plan.plan)
}

pub fn selectivity_ceiling(doc_count: u64) -> u64 {
    doc_count
        .saturating_mul(SCAN_FALLBACK_PCT as u64)
        .checked_div(100)
        .unwrap_or(0)
        .max(MIN_SELECTIVITY_CEILING)
        .min(doc_count)
}

pub fn selectivity_refinement_ceiling(ceiling: u64, doc_count: u64) -> u64 {
    ceiling
        .saturating_mul(SELECTIVITY_REFINE_MULTIPLIER)
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
    let forced = executor::estimate_forced_candidates(index, plan)?;
    Ok(executor::estimate_candidates(index, plan, df)
        .saturating_add(forced)
        .min(index.summaries.text_count() as u64))
}

/// Posting-list lengths as document-frequency priors.
struct PostingsDf<'a> {
    index: &'a PostingsIndex,
    cache: RefCell<HashMap<u64, u64>>,
}

impl DfStats for PostingsDf<'_> {
    fn entry_count(&self, key: GramKey) -> u64 {
        let hash = key.value();
        if let Some(count) = self.cache.borrow().get(&hash).copied() {
            return count;
        }
        let count = self.index.posting_len(hash).unwrap_or(0) as u64;
        self.cache.borrow_mut().insert(hash, count);
        count
    }

    fn total_entries(&self) -> u64 {
        self.index.summaries.text_count() as u64
    }
}

impl<'a> PostingsDf<'a> {
    fn new(index: &'a PostingsIndex) -> Self {
        Self {
            index,
            cache: RefCell::new(HashMap::new()),
        }
    }
}

fn rebuild_index(
    args: &HiArgs,
    table_fingerprint: u64,
    table: &WeightTable,
    index_home: &Path,
    snapshot: &CurrentSnapshot,
    progress: Option<&BuildProgress>,
) -> anyhow::Result<(PostingsIndex, bench::BuildTimings)> {
    let _lock = acquire_build_lock(index_home)?;
    recover_interrupted_rebuild(index_home)?;
    let staging = suffixed_path(index_home, TEMP_SUFFIX);
    remove_dir_all_if_exists(&staging)?;
    fs::create_dir_all(&staging)
        .with_context(|| format!("failed to create index directory {}", staging.display()))?;
    let file_refs = snapshot.eager_files().iter().collect::<Vec<_>>();
    let mut timings = build_files(
        args,
        table,
        &staging,
        &file_refs,
        TABLE_FILE_NAME,
        POSTINGS_FILE_NAME,
        summary::SUMMARY_FILE_NAME,
        progress,
    )?;
    let manifest_started_at = Instant::now();
    if let Some(progress) = progress {
        progress.phase(BuildPhase::WritingManifest);
    }
    write_manifest(
        &staging.join(MANIFEST_FILE_NAME),
        &manifest_for(ManifestBackend::Postings, table_fingerprint, snapshot),
    )?;
    write_path_table(&staging.join(MANIFEST_FILE_NAME), snapshot)?;
    timings.set_write_manifest(manifest_started_at);
    let publish_started_at = Instant::now();
    if let Some(progress) = progress {
        progress.phase(BuildPhase::Publishing);
    }
    fsync_dir(&staging)?;
    swap_in(&staging, index_home)?;
    timings.set_publish_generation(publish_started_at);
    let index = PostingsIndex::open(index_home, snapshot.file_count())?
        .with_context(|| format!("index at {} corrupt after publish", index_home.display()))?;
    Ok((index, timings))
}

pub fn open_index(index_home: &Path, snapshot: &CurrentSnapshot) -> anyhow::Result<PostingsIndex> {
    PostingsIndex::open_trusted(index_home, snapshot.file_count())?.with_context(|| {
        format!(
            "index at {} missing from daemon-owned generation",
            index_home.display()
        )
    })
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
    summary_name: &str,
    progress: Option<&BuildProgress>,
) -> anyhow::Result<bench::BuildTimings> {
    let started_at = Instant::now();
    let mut timings = bench::BuildTimings::default();
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
                progress,
                files.len(),
            )
        })?;
    let scan_elapsed = scan_started_at.elapsed();
    timings.set_scan_documents(scan_started_at);
    let run_count = next_run.load(AtomicOrdering::Relaxed);
    let mut summaries = stats.take_summaries();
    let summary_started_at = Instant::now();
    if let Some(progress) = progress {
        progress.phase(BuildPhase::WritingSummary);
    }
    summary::write_records(&index_home.join(summary_name), &mut summaries)?;
    timings.set_write_summary(summary_started_at);
    log::debug!("eg index build: scan phase done in {scan_elapsed:?}; merging {run_count} runs",);
    let merge_started_at = Instant::now();
    let pairs_total = stats.run_bytes.load(AtomicOrdering::Relaxed) as u64 / RUN_PAIR_SIZE as u64;
    if let Some(progress) = progress {
        progress.start_postings(run_count, pairs_total);
    }
    merge_runs(
        &runs_dir,
        run_count,
        &index_home.join(table_name),
        &index_home.join(postings_name),
        progress,
        pairs_total,
    )?;
    let merge_elapsed = merge_started_at.elapsed();
    timings.set_write_postings(merge_started_at);
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
    Ok(timings)
}

fn write_chunk_runs(
    args: &HiArgs,
    table: &WeightTable,
    files: &[&CurrentFile],
    runs_dir: &Path,
    next_run: &AtomicUsize,
    stats: &BuildStats,
    pair_budget: usize,
    progress: Option<&BuildProgress>,
    total_files: usize,
) -> anyhow::Result<()> {
    let mut pairs = Vec::with_capacity(pair_budget.min(64 * 1024));
    for file in files {
        scan_file_pairs(
            table,
            file,
            args.index_mmap(),
            &mut pairs,
            stats,
            progress,
            total_files,
        )?;
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
    progress: Option<&BuildProgress>,
    total_files: usize,
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
    if let Some(progress) = progress {
        progress.update_scan(
            total_files,
            scanned as u64,
            stats.bytes.load(AtomicOrdering::Relaxed) as u64,
            stats.runs.load(AtomicOrdering::Relaxed) as u64,
        );
    }
    let document = super::document::scan(table, file, use_mmap)?;
    stats.push_summary(document.summary);
    if document.is_skipped() {
        return Ok(());
    }
    if document.forced_candidate {
        push_forced(pairs, document.ord, stats);
        return Ok(());
    }
    stats
        .emitted
        .fetch_add(document.emitted_grams(), AtomicOrdering::Relaxed);
    stats
        .selected
        .fetch_add(document.hashes.len(), AtomicOrdering::Relaxed);
    pairs.extend(document.hashes.into_iter().map(|hash| Pair {
        hash,
        ord: document.ord,
    }));
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
    progress: Option<&BuildProgress>,
    pairs_total: u64,
) -> anyhow::Result<()> {
    let mut table_writer = SectionWriter::create(table_path, TABLE_MAGIC)?;
    let mut postings_writer = SectionWriter::create(postings_path, POSTINGS_MAGIC)?;
    let mut merge_progress = MergeProgress::new(progress, run_count, pairs_total);
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
        merge_progress.pair_done();
        let reader = readers
            .get_mut(item.run_id)
            .context("merge run index out of range")?;
        if let Some(pair) = reader.next_pair()? {
            heap.push(HeapItem {
                pair,
                run_id: item.run_id,
            });
        } else {
            merge_progress.run_done();
        }
    }
    if let Some(hash) = current_hash {
        flush_posting(&mut table_writer, &mut postings_writer, hash, &docs)?;
    }
    table_writer.finalize(TABLE_RECORD_SIZE as u64)?;
    postings_writer.finalize(POSTING_SIZE as u64)?;
    merge_progress.finish();
    Ok(())
}

struct MergeProgress<'a> {
    progress: Option<&'a BuildProgress>,
    runs_total: usize,
    pairs_total: u64,
    runs_done: u64,
    pairs_done: u64,
    next_pair_update: u64,
}

impl<'a> MergeProgress<'a> {
    fn new(progress: Option<&'a BuildProgress>, runs_total: usize, pairs_total: u64) -> Self {
        Self {
            progress,
            runs_total,
            pairs_total,
            runs_done: 0,
            pairs_done: 0,
            next_pair_update: POSTINGS_MERGE_PROGRESS_EVERY,
        }
    }

    fn pair_done(&mut self) {
        self.pairs_done += 1;
        if self.pairs_done < self.next_pair_update && self.pairs_done < self.pairs_total {
            return;
        }
        self.emit();
        self.next_pair_update = self.pairs_done + POSTINGS_MERGE_PROGRESS_EVERY;
    }

    fn run_done(&mut self) {
        self.runs_done += 1;
    }

    fn finish(&mut self) {
        self.pairs_done = self.pairs_total;
        self.runs_done = self.runs_total as u64;
        self.emit();
    }

    fn emit(&self) {
        if let Some(progress) = self.progress {
            progress.update_postings(
                self.runs_total,
                self.runs_done,
                self.pairs_total,
                self.pairs_done,
            );
        }
    }
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
    write_posting_ordinals(postings_writer, docs)
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

fn count_plan_grams(plan: &QueryPlan) -> usize {
    plan.gram_count()
}

pub struct PostingsIndex {
    base: Segment,
    summaries: SummaryIndex,
}

impl PostingsIndex {
    /// Open the index, returning `None` when a segment is missing or corrupt.
    fn open(index_home: &Path, doc_count: usize) -> anyhow::Result<Option<Self>> {
        Self::open_with(index_home, doc_count, IndexOpen::Strict)
    }

    fn open_trusted(index_home: &Path, doc_count: usize) -> anyhow::Result<Option<Self>> {
        Self::open_with(index_home, doc_count, IndexOpen::Trusted)
    }

    fn open_with(
        index_home: &Path,
        doc_count: usize,
        mode: IndexOpen,
    ) -> anyhow::Result<Option<Self>> {
        let Some(base) = Segment::open(
            &index_home.join(TABLE_FILE_NAME),
            &index_home.join(POSTINGS_FILE_NAME),
            mode,
        )?
        else {
            return Ok(None);
        };
        let summaries = match mode {
            IndexOpen::Strict => {
                SummaryIndex::open(&index_home.join(summary::SUMMARY_FILE_NAME), doc_count)?
            },
            IndexOpen::Trusted => {
                SummaryIndex::open_trusted(&index_home.join(summary::SUMMARY_FILE_NAME), doc_count)?
            },
        };
        let Some(summaries) = summaries else {
            return Ok(None);
        };
        Ok(Some(Self { base, summaries }))
    }

    pub fn corpus_text_bytes(&self) -> u64 {
        self.summaries.text_bytes()
    }

    fn lookup(&self, hash: u64) -> anyhow::Result<Vec<usize>> {
        self.base.lookup(hash)
    }

    fn posting_list(&self, hash: u64) -> anyhow::Result<PostingList<'_>> {
        self.base.posting_list(hash)
    }

    /// Posting-list length without decoding: the df prior for the cost model.
    fn posting_len(&self, hash: u64) -> anyhow::Result<usize> {
        self.base.posting_len(hash)
    }
}

#[derive(Clone, Copy)]
enum IndexOpen {
    Strict,
    Trusted,
}

impl PlanBackend for PostingsIndex {
    fn summaries(&self) -> &SummaryIndex {
        &self.summaries
    }

    fn lookup_gram(&self, key: GramKey) -> anyhow::Result<Vec<usize>> {
        self.lookup(key.value())
    }

    fn forced_candidates(&self) -> anyhow::Result<Vec<usize>> {
        self.lookup(FORCED_CANDIDATE_HASH)
    }
}

struct Segment {
    table: Mmap,
    postings: Mmap,
}

impl Segment {
    /// Open both files and validate their section headers and sampled checksums.
    fn open(
        table_path: &Path,
        postings_path: &Path,
        mode: IndexOpen,
    ) -> anyhow::Result<Option<Self>> {
        let strict = matches!(mode, IndexOpen::Strict);
        let Some(table) = open_section(table_path, TABLE_MAGIC, TABLE_RECORD_SIZE, strict)? else {
            return Ok(None);
        };
        let Some(postings) = open_section(postings_path, POSTINGS_MAGIC, POSTING_SIZE, strict)?
        else {
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

    fn lookup(&self, hash: u64) -> anyhow::Result<Vec<usize>> {
        self.posting_list(hash).map(|list| list.ordinals())
    }

    fn posting_list(&self, hash: u64) -> anyhow::Result<PostingList<'_>> {
        let Some((offset, len)) = find_record(self.table_body(), hash)? else {
            return Ok(PostingList::empty());
        };
        let len = usize::try_from(len).context("posting length does not fit in usize")?;
        let offset = usize::try_from(offset).context("posting offset does not fit in usize")?;
        let byte_len = len
            .checked_mul(POSTING_SIZE)
            .context("posting byte length overflow")?;
        let end = offset
            .checked_add(byte_len)
            .context("posting byte range overflow")?;
        let postings = self.postings_body();
        let Some(region) = postings.get(offset..end) else {
            anyhow::bail!("posting list points past postings file");
        };
        Ok(PostingList { bytes: region })
    }

    fn posting_len(&self, hash: u64) -> anyhow::Result<usize> {
        let table = self.table_body();
        let Some((_, len)) = find_record(table, hash)? else {
            return Ok(0);
        };
        usize::try_from(len).context("posting length does not fit in usize")
    }
}

#[derive(Clone, Copy)]
struct PostingList<'a> {
    bytes: &'a [u8],
}

impl<'a> PostingList<'a> {
    const fn empty() -> Self {
        Self { bytes: &[] }
    }

    fn len(self) -> usize {
        self.bytes.len() / POSTING_SIZE
    }

    fn ordinals(self) -> Vec<usize> {
        self.bytes
            .chunks_exact(POSTING_SIZE)
            .map(|chunk| u32::from_le_bytes(chunk.try_into().expect("four bytes")) as usize)
            .collect()
    }

    fn contains(self, ord: usize) -> bool {
        let Ok(ord) = u32::try_from(ord) else {
            return false;
        };
        let mut lo = 0usize;
        let mut hi = self.len();
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            match self.ord_at(mid).cmp(&ord) {
                Ordering::Less => lo = mid + 1,
                Ordering::Greater => hi = mid,
                Ordering::Equal => return true,
            }
        }
        false
    }

    fn ord_at(self, idx: usize) -> u32 {
        let start = idx * POSTING_SIZE;
        let bytes = self
            .bytes
            .get(start..start + POSTING_SIZE)
            .expect("posting index in range");
        u32::from_le_bytes(bytes.try_into().expect("four bytes"))
    }
}

struct FastAllOf<'a> {
    index: &'a PostingsIndex,
    driver: FastNeedle<'a>,
    filters: Vec<FastNeedle<'a>>,
    needs: &'a [ScanNeed],
}

impl<'a> FastAllOf<'a> {
    fn try_execute(
        index: &'a PostingsIndex,
        plan: &'a QueryPlan,
    ) -> anyhow::Result<Option<Vec<usize>>> {
        let Some(query) = Self::from_plan(index, plan)? else {
            return Ok(None);
        };
        Ok(Some(query.execute()))
    }

    fn from_plan(index: &'a PostingsIndex, plan: &'a QueryPlan) -> anyhow::Result<Option<Self>> {
        let PlanExpr::AllOf {
            grams,
            needs,
            children,
        } = plan.root()
        else {
            return Ok(None);
        };
        if grams.is_empty() || !children.is_empty() {
            return Ok(None);
        }
        let mut needles = Self::needles(index, grams)?;
        needles.sort_by_key(FastNeedle::len);
        let driver = needles.remove(0);
        Ok(Some(Self {
            index,
            driver,
            filters: needles,
            needs,
        }))
    }

    fn needles(
        index: &'a PostingsIndex,
        grams: &'a [GramNeedle],
    ) -> anyhow::Result<Vec<FastNeedle<'a>>> {
        grams
            .iter()
            .map(|needle| FastNeedle::open(index, needle))
            .collect()
    }

    fn execute(self) -> Vec<usize> {
        let mut candidates = self.driver.ordinals();
        candidates.retain(|&ord| self.keeps(ord));
        candidates
    }

    fn keeps(&self, ord: usize) -> bool {
        let status = self.index.summaries.status(ord);
        status.is_text()
            && self.filters.iter().all(|needle| needle.contains(ord))
            && self.needs.iter().all(|need| status.satisfies(need))
    }
}

struct FastNeedle<'a> {
    lists: Vec<PostingList<'a>>,
    len: usize,
}

impl<'a> FastNeedle<'a> {
    fn open(index: &'a PostingsIndex, needle: &GramNeedle) -> anyhow::Result<Self> {
        let lists = needle
            .keys()
            .map(|key| index.posting_list(key.value()))
            .collect::<anyhow::Result<Vec<_>>>()?;
        let len = lists.iter().map(|list| list.len()).sum();
        Ok(Self { lists, len })
    }

    const fn len(&self) -> usize {
        self.len
    }

    fn ordinals(&self) -> Vec<usize> {
        let mut ords = Vec::with_capacity(self.len);
        for list in &self.lists {
            ords.extend(list.ordinals());
        }
        ords.sort_unstable();
        ords.dedup();
        ords
    }

    fn contains(&self, ord: usize) -> bool {
        self.lists.iter().any(|list| list.contains(ord))
    }
}

/// Memory-map one section file and verify its header, magic, length, and optional sampled checksum.
#[allow(unsafe_code)]
fn open_section(
    path: &Path,
    magic: [u8; 8],
    record_size: usize,
    verify_checksum: bool,
) -> anyhow::Result<Option<Mmap>> {
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
    if let Err(reason) = verify_section_with_checksum(&mmap, magic, record_size, verify_checksum) {
        log::debug!("eg index: {} failed verification: {reason}", path.display());
        return Ok(None);
    }
    Ok(Some(mmap))
}

fn verify_section_with_checksum(
    mmap: &[u8],
    magic: [u8; 8],
    record_size: usize,
    verify_checksum: bool,
) -> Result<(), String> {
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
    if verify_checksum && sampled_checksum(body) != checksum {
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

fn write_posting_ordinals(writer: &mut SectionWriter, docs: &[u32]) -> anyhow::Result<()> {
    let mut buffer = [0u8; POSTING_WRITE_BUFFER_BYTES];
    let mut len = 0usize;
    for &doc in docs {
        if len + POSTING_SIZE > buffer.len() {
            writer.write_all(&buffer[..len])?;
            len = 0;
        }
        buffer[len..len + POSTING_SIZE].copy_from_slice(&doc.to_le_bytes());
        len += POSTING_SIZE;
    }
    if len > 0 {
        writer.write_all(&buffer[..len])?;
    }
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
            writer: BufWriter::with_capacity(SECTION_WRITER_BUFFER_BYTES, file),
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
    let mut bytes = [0u8; RUN_PAIR_SIZE];
    bytes[..8].copy_from_slice(&pair.hash.to_le_bytes());
    bytes[8..].copy_from_slice(&pair.ord.to_le_bytes());
    writer.write_all(&bytes)?;
    Ok(())
}

fn read_pair(reader: &mut BufReader<File>) -> anyhow::Result<Option<Pair>> {
    if reader.fill_buf()?.is_empty() {
        return Ok(None);
    }
    let mut bytes = [0u8; RUN_PAIR_SIZE];
    reader.read_exact(&mut bytes)?;
    Ok(Some(Pair {
        hash: u64::from_le_bytes(bytes[..8].try_into().expect("eight bytes")),
        ord: u32::from_le_bytes(bytes[8..].try_into().expect("four bytes")),
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
            reader: BufReader::with_capacity(
                RUN_READER_BUFFER_BYTES,
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
    summaries: Mutex<Vec<SummaryRecord>>,
}

impl BuildStats {
    fn push_summary(&self, record: SummaryRecord) {
        self.summaries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(record);
    }

    fn take_summaries(&self) -> Vec<SummaryRecord> {
        std::mem::take(
            &mut *self
                .summaries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }
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
        FNV_OFFSET, IndexOpen, MAX_PAIRS_PER_RUN, MIN_PAIRS_PER_RUN, POSTING_SIZE,
        POSTING_WRITE_BUFFER_BYTES, POSTINGS_MAGIC, Pair, PostingList, RUN_PAIR_SIZE,
        SECTION_HEADER_SIZE, SectionWriter, Segment, TABLE_MAGIC, TABLE_RECORD_SIZE, find_record,
        fnv1a_state, pairs_per_run, read_pair, sampled_checksum, section_header, suffixed_path,
        verify_section_with_checksum, write_pair, write_posting_ordinals,
    };
    use std::{
        fs,
        fs::File,
        io::{BufReader, BufWriter, Write},
        path::{Path, PathBuf},
    };

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
    fn pair_budget_is_bounded_by_ram_cap_and_minimum() {
        assert_eq!(pairs_per_run(1), MAX_PAIRS_PER_RUN);
        assert_eq!(pairs_per_run(usize::MAX), MIN_PAIRS_PER_RUN);
        assert!(pairs_per_run(16) > MIN_PAIRS_PER_RUN);
    }

    #[test]
    fn run_pair_io_round_trips_and_stops_at_eof() {
        let dir = scratch("pair-io");
        let path = dir.join("run.bin");
        let pairs = [
            Pair { hash: 3, ord: 1 },
            Pair {
                hash: u64::MAX - 1,
                ord: u32::MAX,
            },
        ];
        {
            let mut writer = BufWriter::new(File::create(&path).unwrap());
            for pair in pairs {
                write_pair(&mut writer, pair).unwrap();
            }
            writer.flush().unwrap();
        }

        assert_eq!(
            fs::metadata(&path).unwrap().len(),
            (2 * RUN_PAIR_SIZE) as u64
        );
        let mut reader = BufReader::new(File::open(path).unwrap());
        assert_eq!(read_pair(&mut reader).unwrap(), Some(pairs[0]));
        assert_eq!(read_pair(&mut reader).unwrap(), Some(pairs[1]));
        assert_eq!(read_pair(&mut reader).unwrap(), None);
    }

    #[test]
    fn posting_ordinal_writer_batches_across_buffer_boundary() {
        let dir = scratch("posting-ordinals");
        let path = dir.join("postings.bin");
        let docs = (0..(POSTING_WRITE_BUFFER_BYTES / POSTING_SIZE + 3) as u32)
            .map(|ord| ord.wrapping_mul(7))
            .collect::<Vec<_>>();
        let mut writer = SectionWriter::create(&path, POSTINGS_MAGIC).unwrap();

        write_posting_ordinals(&mut writer, &docs).unwrap();
        writer.finalize(POSTING_SIZE as u64).unwrap();

        let bytes = fs::read(path).unwrap();
        let body = &bytes[SECTION_HEADER_SIZE..];
        assert_eq!(body.len(), docs.len() * POSTING_SIZE);
        for (chunk, expected) in body.chunks_exact(POSTING_SIZE).zip(docs) {
            assert_eq!(
                u32::from_le_bytes(chunk.try_into().expect("four bytes")),
                expected
            );
        }
    }

    #[test]
    fn posting_list_membership_uses_sorted_ordinals() {
        let mut bytes = Vec::new();
        for ord in [3u32, 8, 13, 21] {
            bytes.extend_from_slice(&ord.to_le_bytes());
        }
        let postings = PostingList { bytes: &bytes };

        assert!(postings.contains(3));
        assert!(postings.contains(13));
        assert!(postings.contains(21));
        assert!(!postings.contains(2));
        assert!(!postings.contains(22));
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
        assert!(verify_section_with_checksum(&file, TABLE_MAGIC, TABLE_RECORD_SIZE, true).is_ok());
        let empty = framed(POSTINGS_MAGIC, POSTING_SIZE, 0);
        assert!(verify_section_with_checksum(&empty, POSTINGS_MAGIC, POSTING_SIZE, true).is_ok());
    }

    #[test]
    fn section_detects_body_corruption() {
        let mut file = framed(POSTINGS_MAGIC, POSTING_SIZE, 4);
        if let Some(byte) = file.get_mut(SECTION_HEADER_SIZE + 1) {
            *byte ^= 0xFF;
        }
        assert!(verify_section_with_checksum(&file, POSTINGS_MAGIC, POSTING_SIZE, true).is_err());
    }

    #[test]
    fn actual_open_rejects_corrupted_section_body() {
        let dir = scratch("corrupt-open");
        let table = dir.join("table.bin");
        let postings = dir.join("postings.bin");

        write_section(
            &table,
            TABLE_MAGIC,
            TABLE_RECORD_SIZE,
            &table_body(&[(1, 0, 1)]),
        );
        write_section(&postings, POSTINGS_MAGIC, POSTING_SIZE, &1u32.to_le_bytes());
        let mut corrupted = fs::read(&table).unwrap();
        corrupted[SECTION_HEADER_SIZE + 1] ^= 0xFF;
        fs::write(&table, corrupted).unwrap();

        assert!(
            Segment::open(&table, &postings, IndexOpen::Strict)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn section_detects_bad_magic_and_length() {
        let file = framed(TABLE_MAGIC, TABLE_RECORD_SIZE, 2);
        assert!(verify_section_with_checksum(&file, POSTINGS_MAGIC, POSTING_SIZE, true).is_err());

        let body = vec![1u8; TABLE_RECORD_SIZE * 2];
        let mut lying = section_header(TABLE_MAGIC, 3, sampled_checksum(&body)).to_vec();
        lying.extend_from_slice(&body);
        assert!(
            verify_section_with_checksum(&lying, TABLE_MAGIC, TABLE_RECORD_SIZE, true).is_err()
        );
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

    fn scratch(name: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("eg-postings-{name}-{stamp}"));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn write_section(path: &Path, magic: [u8; 8], record_size: usize, body: &[u8]) {
        let mut file = section_header(
            magic,
            (body.len() / record_size) as u64,
            sampled_checksum(body),
        )
        .to_vec();
        file.extend_from_slice(body);
        fs::write(path, file).unwrap();
    }
}
