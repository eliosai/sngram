//! Compact mmap-backed sparse n-gram postings index.

use std::{
    cmp::Ordering,
    collections::{BTreeSet, BinaryHeap, HashMap},
    fs::{self, File},
    io::{self, BufReader, BufWriter, Read, Write},
    mem,
    path::{Path, PathBuf},
    rc::Rc,
    sync::atomic::{AtomicUsize, Ordering as AtomicOrdering},
    time::Instant,
};

use anyhow::Context;
use memmap2::{Mmap, MmapOptions};
use rayon::prelude::*;
use sngram::QueryPlan;
use sngram_types::{Content, WeightTable};

use crate::flags::HiArgs;

use super::manifest::{
    CurrentFile, CurrentSnapshot, Manifest, ManifestBackend, changed_ordinals, manifest_for,
    read_manifest, write_manifest,
};

const MANIFEST_FILE_NAME: &str = "manifest.json";
const DELTA_MANIFEST_FILE_NAME: &str = "delta-manifest.json";
const TABLE_FILE_NAME: &str = "table.bin";
const POSTINGS_FILE_NAME: &str = "postings.bin";
const DELTA_TABLE_FILE_NAME: &str = "delta-table.bin";
const DELTA_POSTINGS_FILE_NAME: &str = "delta-postings.bin";
const RUNS_DIR_NAME: &str = "runs";
const TABLE_RECORD_SIZE: usize = 24;
const POSTING_SIZE: usize = 4;
const RUN_PAIR_SIZE: usize = 12;
const FILES_PER_RAYON_TASK: usize = 128;
const INDEX_RAM_CAP_BYTES: usize = 512 * 1024 * 1024;
const MIN_PAIRS_PER_RUN: usize = 128 * 1024;
const MAX_PAIRS_PER_RUN: usize = 2_000_000;
const MAX_DELTA_FILES: usize = 4096;
const FORCED_CANDIDATE_HASH: u64 = u64::MAX;

pub(super) fn prepare_index(
    args: &HiArgs,
    table_spec: sngram_weights::BuiltinTable,
    table: &WeightTable,
    index_home: &Path,
    snapshot: &CurrentSnapshot,
    loaded_manifest: Option<&Manifest>,
) -> anyhow::Result<PostingsIndex> {
    match args.index().mode {
        super::config::IndexMode::NoIndex => {
            anyhow::bail!("internal error: indexed path used with --no-index")
        },
        super::config::IndexMode::Rebuild => {
            rebuild_index(args, table_spec, table, index_home, snapshot)
        },
        super::config::IndexMode::Auto => auto_index(
            args,
            table_spec,
            table,
            index_home,
            snapshot,
            loaded_manifest,
        ),
    }
}

pub(super) fn query_index(
    index: &PostingsIndex,
    plan: &QueryPlan,
) -> anyhow::Result<BTreeSet<usize>> {
    let started_at = Instant::now();
    log::debug!(
        "eg index query: postings plan_grams={}",
        count_plan_grams(plan),
    );
    let lookup_started_at = Instant::now();
    let mut candidates = eval_plan(index, plan)?;
    candidates = union_sorted(candidates, index.lookup(FORCED_CANDIDATE_HASH)?);
    log::debug!(
        "eg index query: postings candidates={} lookup_time={:?} total_query_time={:?}",
        candidates.len(),
        lookup_started_at.elapsed(),
        started_at.elapsed()
    );
    candidates.into_iter().map(Ok).collect()
}

fn auto_index(
    args: &HiArgs,
    table_spec: sngram_weights::BuiltinTable,
    table: &WeightTable,
    index_home: &Path,
    snapshot: &CurrentSnapshot,
    loaded_manifest: Option<&Manifest>,
) -> anyhow::Result<PostingsIndex> {
    let manifest_path = index_home.join(MANIFEST_FILE_NAME);
    if !index_home.join(TABLE_FILE_NAME).exists()
        || !index_home.join(POSTINGS_FILE_NAME).exists()
        || !manifest_path.exists()
    {
        return rebuild_index(args, table_spec, table, index_home, snapshot);
    }
    let base_manifest_storage;
    let base_manifest = if let Some(manifest) = loaded_manifest {
        manifest
    } else {
        base_manifest_storage = match read_manifest(&manifest_path)? {
            Some(manifest) => manifest,
            None => return rebuild_index(args, table_spec, table, index_home, snapshot),
        };
        &base_manifest_storage
    };
    let expected = manifest_for(ManifestBackend::Postings, table_spec, snapshot);
    let Some(changed) = changed_ordinals(base_manifest, &expected) else {
        return rebuild_index(args, table_spec, table, index_home, snapshot);
    };
    if changed.is_empty() {
        remove_delta(index_home)?;
        return PostingsIndex::open(index_home, snapshot.files.len(), false);
    }
    if changed.len() > MAX_DELTA_FILES {
        return rebuild_index(args, table_spec, table, index_home, snapshot);
    }
    let delta_manifest_path = index_home.join(DELTA_MANIFEST_FILE_NAME);
    let delta_ready = index_home.join(DELTA_TABLE_FILE_NAME).exists()
        && index_home.join(DELTA_POSTINGS_FILE_NAME).exists()
        && read_manifest(&delta_manifest_path)?
            .as_ref()
            .is_some_and(|manifest| changed_ordinals(manifest, &expected) == Some(Vec::new()));
    if !delta_ready {
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
        write_manifest(&delta_manifest_path, &expected)?;
    }
    PostingsIndex::open(index_home, snapshot.files.len(), true)
}

fn rebuild_index(
    args: &HiArgs,
    table_spec: sngram_weights::BuiltinTable,
    table: &WeightTable,
    index_home: &Path,
    snapshot: &CurrentSnapshot,
) -> anyhow::Result<PostingsIndex> {
    if index_home.exists() {
        fs::remove_dir_all(index_home)
            .with_context(|| format!("failed to remove old index at {}", index_home.display()))?;
    }
    fs::create_dir_all(index_home)
        .with_context(|| format!("failed to create index directory {}", index_home.display()))?;
    let file_refs = snapshot.files.iter().collect::<Vec<_>>();
    build_files(
        args,
        table,
        index_home,
        &file_refs,
        TABLE_FILE_NAME,
        POSTINGS_FILE_NAME,
    )?;
    write_manifest(
        &index_home.join(MANIFEST_FILE_NAME),
        &manifest_for(ManifestBackend::Postings, table_spec, snapshot),
    )?;
    PostingsIndex::open(index_home, snapshot.files.len(), false)
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
    let merge_started_at = Instant::now();
    merge_runs(
        &runs_dir,
        next_run.load(AtomicOrdering::Relaxed),
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
    stats.files.fetch_add(1, AtomicOrdering::Relaxed);
    stats
        .bytes
        .fetch_add(metadata.len() as usize, AtomicOrdering::Relaxed);
    if metadata.len() == 0 {
        return Ok(());
    }
    let ord = u32::try_from(file.ord).context("indexed document ordinal does not fit in u32")?;
    let bytes = read_file(&file.path, use_mmap)?;
    let bytes = bytes.as_ref();
    if has_decoding_bom(bytes) {
        pairs.push(Pair {
            hash: FORCED_CANDIDATE_HASH,
            ord,
        });
        stats.forced.fetch_add(1, AtomicOrdering::Relaxed);
    }
    let mut hashes = Vec::new();
    let emitted = collect_sparse_hashes(table, bytes, &mut hashes);
    hashes.sort_unstable();
    hashes.dedup();
    let selected = hashes.len();
    pairs.extend(hashes.into_iter().map(|hash| Pair { hash, ord }));
    stats.emitted.fetch_add(emitted, AtomicOrdering::Relaxed);
    stats.selected.fetch_add(selected, AtomicOrdering::Relaxed);
    Ok(())
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

fn merge_runs(
    runs_dir: &Path,
    run_count: usize,
    table_path: &Path,
    postings_path: &Path,
) -> anyhow::Result<()> {
    let table_file = File::create(table_path)
        .with_context(|| format!("failed to create table {}", table_path.display()))?;
    let postings_file = File::create(postings_path)
        .with_context(|| format!("failed to create postings {}", postings_path.display()))?;
    let mut table_writer = BufWriter::new(table_file);
    let mut postings_writer = BufWriter::new(postings_file);
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
    let mut postings_offset = 0u64;
    while let Some(item) = heap.pop() {
        if current_hash != Some(item.pair.hash) {
            if let Some(hash) = current_hash {
                flush_posting(
                    &mut table_writer,
                    &mut postings_writer,
                    hash,
                    &docs,
                    &mut postings_offset,
                )?;
                docs.clear();
            }
            current_hash = Some(item.pair.hash);
        }
        if docs.last().copied() != Some(item.pair.ord) {
            docs.push(item.pair.ord);
        }
        if let Some(pair) = readers[item.run_id].next_pair()? {
            heap.push(HeapItem {
                pair,
                run_id: item.run_id,
            });
        }
    }
    if let Some(hash) = current_hash {
        flush_posting(
            &mut table_writer,
            &mut postings_writer,
            hash,
            &docs,
            &mut postings_offset,
        )?;
    }
    table_writer
        .flush()
        .with_context(|| format!("failed to flush table {}", table_path.display()))?;
    postings_writer
        .flush()
        .with_context(|| format!("failed to flush postings {}", postings_path.display()))?;
    Ok(())
}

fn flush_posting(
    table_writer: &mut BufWriter<File>,
    postings_writer: &mut BufWriter<File>,
    hash: u64,
    docs: &[u32],
    postings_offset: &mut u64,
) -> anyhow::Result<()> {
    let len = u32::try_from(docs.len()).context("posting list length does not fit in u32")?;
    write_table_record(table_writer, hash, *postings_offset, len)?;
    for &doc in docs {
        postings_writer.write_all(&doc.to_le_bytes())?;
    }
    *postings_offset += u64::from(len) * POSTING_SIZE as u64;
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
    for name in [
        DELTA_MANIFEST_FILE_NAME,
        DELTA_TABLE_FILE_NAME,
        DELTA_POSTINGS_FILE_NAME,
    ] {
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

fn has_decoding_bom(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0xFF, 0xFE])
        || bytes.starts_with(&[0xFE, 0xFF])
        || bytes.starts_with(&[0xFF, 0xFE, 0x00, 0x00])
        || bytes.starts_with(&[0x00, 0x00, 0xFE, 0xFF])
}

fn collect_sparse_hashes(table: &WeightTable, bytes: &[u8], hashes: &mut Vec<u64>) -> usize {
    let mut count = 0usize;
    sngram::scan(table, &Content::new(bytes), |_, _, hash| {
        count += 1;
        hashes.push(hash);
    });
    count
}

fn count_plan_grams(plan: &QueryPlan) -> usize {
    match plan {
        QueryPlan::All | QueryPlan::None => 0,
        QueryPlan::And { grams, sub } | QueryPlan::Or { grams, sub } => {
            grams.len() + sub.iter().map(count_plan_grams).sum::<usize>()
        },
    }
}

/// Posting lists shared between plan nodes: case-folded plans repeat the
/// same gram across many OR branches, so each unique gram is fetched and
/// decoded once per query.
type PostingCache = HashMap<u64, Rc<Vec<usize>>>;

fn eval_plan(index: &PostingsIndex, plan: &QueryPlan) -> anyhow::Result<Vec<usize>> {
    let mut cache = PostingCache::new();
    eval_plan_cached(index, plan, &mut cache)
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
    cache: &mut PostingCache,
) -> anyhow::Result<Vec<usize>> {
    match plan {
        QueryPlan::All => {
            anyhow::bail!("indexed query has no sparse n-gram constraints; use --no-index")
        },
        QueryPlan::None => Ok(Vec::new()),
        QueryPlan::And { grams, sub } => {
            let mut lists = Vec::with_capacity(grams.len() + sub.len());
            for gram in grams {
                lists.push(lookup_cached(index, cache, gram.hash())?);
            }
            for plan in sub {
                lists.push(Rc::new(eval_plan_cached(index, plan, cache)?));
            }
            intersect_all_sorted(index.doc_count, lists)
        },
        QueryPlan::Or { grams, sub } => {
            let mut acc = Vec::new();
            for gram in grams {
                acc = union_sorted_ref(&acc, &lookup_cached(index, cache, gram.hash())?);
            }
            for plan in sub {
                acc = union_sorted_ref(&acc, &eval_plan_cached(index, plan, cache)?);
            }
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
    fn open(index_home: &Path, doc_count: usize, delta: bool) -> anyhow::Result<Self> {
        Ok(Self {
            base: Segment::open(
                &index_home.join(TABLE_FILE_NAME),
                &index_home.join(POSTINGS_FILE_NAME),
            )?,
            delta: if delta {
                Some(Segment::open(
                    &index_home.join(DELTA_TABLE_FILE_NAME),
                    &index_home.join(DELTA_POSTINGS_FILE_NAME),
                )?)
            } else {
                None
            },
            doc_count,
        })
    }

    fn lookup(&self, hash: u64) -> anyhow::Result<Vec<usize>> {
        let mut out = self.base.lookup(hash)?;
        if let Some(delta) = &self.delta {
            out = union_sorted(out, delta.lookup(hash)?);
        }
        Ok(out)
    }
}

struct Segment {
    table: Option<Mmap>,
    postings: Option<Mmap>,
}

impl Segment {
    fn open(table_path: &Path, postings_path: &Path) -> anyhow::Result<Self> {
        Ok(Self {
            table: mmap_optional(table_path)?,
            postings: mmap_optional(postings_path)?,
        })
    }

    fn lookup(&self, hash: u64) -> anyhow::Result<Vec<usize>> {
        let Some(table) = &self.table else {
            return Ok(Vec::new());
        };
        let Some(postings) = &self.postings else {
            return Ok(Vec::new());
        };
        let Some((offset, len)) = find_record(table, hash)? else {
            return Ok(Vec::new());
        };
        let offset = usize::try_from(offset).context("posting offset does not fit in usize")?;
        let len = usize::try_from(len).context("posting length does not fit in usize")?;
        let byte_len = len
            .checked_mul(POSTING_SIZE)
            .context("posting byte length overflow")?;
        let end = offset
            .checked_add(byte_len)
            .context("posting byte range overflow")?;
        if end > postings.len() {
            anyhow::bail!("posting list points past postings file");
        }
        let mut docs = Vec::with_capacity(len);
        for chunk in postings[offset..end].chunks_exact(POSTING_SIZE) {
            docs.push(u32::from_le_bytes(chunk.try_into().expect("exact chunk")) as usize);
        }
        Ok(docs)
    }
}

#[allow(unsafe_code)]
fn mmap_optional(path: &Path) -> anyhow::Result<Option<Mmap>> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    if file
        .metadata()
        .with_context(|| format!("failed to stat {}", path.display()))?
        .len()
        == 0
    {
        return Ok(None);
    }
    unsafe { MmapOptions::new().map(&file) }
        .map(Some)
        .with_context(|| format!("failed to mmap {}", path.display()))
}

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
                let base = mid * TABLE_RECORD_SIZE;
                return Ok(Some((
                    read_u64_at(table, base + 8)?,
                    read_u32_at(table, base + 16)?,
                )));
            },
        }
    }
    Ok(None)
}

fn write_table_record(
    writer: &mut BufWriter<File>,
    hash: u64,
    offset: u64,
    len: u32,
) -> anyhow::Result<()> {
    writer.write_all(&hash.to_le_bytes())?;
    writer.write_all(&offset.to_le_bytes())?;
    writer.write_all(&len.to_le_bytes())?;
    writer.write_all(&0u32.to_le_bytes())?;
    Ok(())
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
