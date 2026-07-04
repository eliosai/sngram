//! Tantivy storage for sparse n-gram postings.

use std::{
    collections::BTreeSet,
    fs::{self, File},
    path::Path,
    sync::mpsc,
    thread,
};

use anyhow::Context;
use memmap2::{Mmap, MmapOptions};
use rayon::prelude::*;
use sngram::QueryPlan;
use sngram_types::{Content, WeightTable};
use tantivy::{
    DocId, Index, Score, Searcher, SegmentOrdinal, SegmentReader, TantivyDocument, Term,
    collector::{Collector, SegmentCollector},
    fastfield::Column,
    query::{BooleanQuery, Query, TermQuery},
    schema::{FAST, Field, INDEXED, IndexRecordOption, STORED, Schema},
};

use crate::{
    flags::HiArgs,
    index::config::{IndexBackend, IndexMode},
};

use super::{
    manifest::{
        CurrentFile, CurrentSnapshot, Manifest, ManifestBackend, changed_ordinals, manifest_for,
        manifest_present, read_manifest, write_manifest,
    },
    planner::plan_to_query,
};

const INDEX_DATA_DIR_NAME: &str = "tantivy";
const MANIFEST_FILE_NAME: &str = "manifest.json";
const FIELD_GRAM: &str = "gram";
const FIELD_DOC_ORD: &str = "doc_ord";
const FIELD_PATH_HASH: &str = "path_hash";
const FIELD_FORCED_CANDIDATE: &str = "forced_candidate";
const MIN_TANTIVY_THREAD_BUDGET: usize = 15_000_000;
const TANTIVY_THREAD_BUDGET: usize = 64 * 1024 * 1024;

pub(super) fn prepare_index(
    args: &HiArgs,
    table_fingerprint: u64,
    table: &WeightTable,
    schema: Schema,
    fields: IndexFields,
    index_home: &Path,
    snapshot: &CurrentSnapshot,
    loaded_manifest: Option<&Manifest>,
) -> anyhow::Result<Index> {
    if matches!(args.index().backend, IndexBackend::TantivyRam) {
        return build_memory_index(args, table, schema, fields, &snapshot.files);
    }
    match args.index().mode {
        IndexMode::NoIndex => anyhow::bail!("internal error: indexed path used with --no-index"),
        IndexMode::Verify | IndexMode::Repair => {
            anyhow::bail!("internal error: maintenance mode reached prepare_index")
        },
        IndexMode::Rebuild => rebuild_disk_index(
            args,
            table_fingerprint,
            table,
            schema,
            fields,
            index_home,
            snapshot,
        ),
        IndexMode::Auto | IndexMode::Require => auto_disk_index(
            args,
            table_fingerprint,
            table,
            schema,
            fields,
            index_home,
            snapshot,
            loaded_manifest,
        ),
    }
}

pub(super) fn query_index(
    index: &Index,
    fields: IndexFields,
    keyed: &super::planner::KeyedPlan,
) -> anyhow::Result<Option<BTreeSet<usize>>> {
    let reader = index
        .reader()
        .context("failed to open tantivy index reader")?;
    let searcher = reader.searcher();
    let doc_count = searcher.num_docs();
    let ceiling = super::postings::selectivity_ceiling(doc_count);
    let df = TantivyDf {
        searcher: &searcher,
        fields,
        key: keyed.key,
    };
    let mut plan = keyed.plan.clone();
    let raw_grams = count_plan_grams(&plan);
    plan.tune(&df, ceiling);
    log::debug!(
        "eg index query: tantivy plan_grams={} tuned_plan_grams={}",
        raw_grams,
        count_plan_grams(&plan),
    );
    if matches!(plan, QueryPlan::None) {
        return Ok(Some(BTreeSet::new()));
    }
    let estimate = estimate_with_forced(&searcher, fields, &plan, &df);
    if estimate > ceiling {
        log::debug!(
            "eg index query: estimate {estimate} of {doc_count} docs exceeds {}%; rejecting indexed query without scan fallback",
            super::postings::SCAN_FALLBACK_PCT
        );
        return Ok(None);
    }
    let query = forced_candidate_query(fields, plan_to_query(fields.gram, &plan, keyed.key)?);
    let ords = searcher
        .search(&*query, &DocOrdCollector)
        .context("failed to query sparse n-gram index")?;
    ords.into_iter()
        .map(u64_to_usize)
        .collect::<anyhow::Result<BTreeSet<usize>>>()
        .map(Some)
}

fn estimate_with_forced(
    searcher: &Searcher,
    fields: IndexFields,
    plan: &QueryPlan,
    df: &TantivyDf<'_>,
) -> u64 {
    if matches!(plan, QueryPlan::None) {
        return 0;
    }
    let forced = searcher
        .doc_freq(&Term::from_field_u64(fields.forced_candidate, 1))
        .unwrap_or(0);
    plan.estimate_candidates(df)
        .saturating_add(forced)
        .min(searcher.num_docs())
}

struct TantivyDf<'a> {
    searcher: &'a Searcher,
    fields: IndexFields,
    key: sngram::HashKey,
}

impl sngram::DfStats for TantivyDf<'_> {
    fn doc_count(&self, gram: &sngram::Gram) -> u64 {
        self.searcher
            .doc_freq(&Term::from_field_u64(
                self.fields.gram,
                gram.hash_keyed(self.key),
            ))
            .unwrap_or(0)
    }

    fn total_docs(&self) -> u64 {
        self.searcher.num_docs()
    }
}

fn count_plan_grams(plan: &QueryPlan) -> usize {
    match plan {
        QueryPlan::All | QueryPlan::None => 0,
        QueryPlan::And { grams, sub } | QueryPlan::Or { grams, sub } => {
            grams.len() + sub.iter().map(count_plan_grams).sum::<usize>()
        },
    }
}

pub(super) fn schema() -> (Schema, IndexFields) {
    let mut builder = Schema::builder();
    let gram = builder.add_u64_field(FIELD_GRAM, INDEXED);
    let doc_ord = builder.add_u64_field(FIELD_DOC_ORD, FAST | STORED);
    let path_hash = builder.add_u64_field(FIELD_PATH_HASH, INDEXED | FAST | STORED);
    let forced_candidate = builder.add_u64_field(FIELD_FORCED_CANDIDATE, INDEXED);
    (
        builder.build(),
        IndexFields {
            gram,
            doc_ord,
            path_hash,
            forced_candidate,
        },
    )
}

fn auto_disk_index(
    args: &HiArgs,
    table_fingerprint: u64,
    table: &WeightTable,
    schema: Schema,
    fields: IndexFields,
    index_home: &Path,
    snapshot: &CurrentSnapshot,
    loaded_manifest: Option<&Manifest>,
) -> anyhow::Result<Index> {
    let data_dir = index_home.join(INDEX_DATA_DIR_NAME);
    let manifest_path = index_home.join(MANIFEST_FILE_NAME);
    if !data_dir.exists() || !manifest_present(&manifest_path) {
        return rebuild_disk_index(
            args,
            table_fingerprint,
            table,
            schema,
            fields,
            index_home,
            snapshot,
        );
    }
    let manifest_storage;
    let manifest = if let Some(manifest) = loaded_manifest {
        manifest
    } else {
        manifest_storage = match read_manifest(&manifest_path)? {
            Some(manifest) => manifest,
            None => {
                return rebuild_disk_index(
                    args,
                    table_fingerprint,
                    table,
                    schema,
                    fields,
                    index_home,
                    snapshot,
                );
            },
        };
        &manifest_storage
    };
    let expected = manifest_for(ManifestBackend::Tantivy, table_fingerprint, snapshot);
    let Some(changed_ordinals) = changed_ordinals(manifest, &expected) else {
        return rebuild_disk_index(
            args,
            table_fingerprint,
            table,
            schema,
            fields,
            index_home,
            snapshot,
        );
    };
    let index = Index::open_in_dir(&data_dir).with_context(|| {
        format!(
            "failed to open existing tantivy index at {}; remove it or use --index=rebuild",
            data_dir.display()
        )
    })?;
    if changed_ordinals.is_empty() {
        return Ok(index);
    }
    refresh_changed_files(
        args,
        table,
        &index,
        fields,
        &snapshot.files,
        &changed_ordinals,
    )?;
    write_manifest(&manifest_path, &expected)?;
    Ok(index)
}

fn rebuild_disk_index(
    args: &HiArgs,
    table_fingerprint: u64,
    table: &WeightTable,
    schema: Schema,
    fields: IndexFields,
    index_home: &Path,
    snapshot: &CurrentSnapshot,
) -> anyhow::Result<Index> {
    if index_home.exists() {
        fs::remove_dir_all(index_home)
            .with_context(|| format!("failed to remove old index at {}", index_home.display()))?;
    }
    let data_dir = index_home.join(INDEX_DATA_DIR_NAME);
    fs::create_dir_all(&data_dir)
        .with_context(|| format!("failed to create index directory {}", data_dir.display()))?;
    let index = Index::create_in_dir(&data_dir, schema)
        .with_context(|| format!("failed to create tantivy index at {}", data_dir.display()))?;
    add_all_documents(args, table, &index, fields, &snapshot.files)?;
    write_manifest(
        &index_home.join(MANIFEST_FILE_NAME),
        &manifest_for(ManifestBackend::Tantivy, table_fingerprint, snapshot),
    )?;
    Ok(index)
}

fn build_memory_index(
    args: &HiArgs,
    table: &WeightTable,
    schema: Schema,
    fields: IndexFields,
    files: &[CurrentFile],
) -> anyhow::Result<Index> {
    let index = Index::create_in_ram(schema);
    add_all_documents(args, table, &index, fields, files)?;
    Ok(index)
}

fn add_all_documents(
    args: &HiArgs,
    table: &WeightTable,
    index: &Index,
    fields: IndexFields,
    files: &[CurrentFile],
) -> anyhow::Result<()> {
    let writer = index_writer(args, index)?;
    let writer = add_documents(args, table, writer, fields, files)?;
    commit_writer(writer)
}

fn refresh_changed_files(
    args: &HiArgs,
    table: &WeightTable,
    index: &Index,
    fields: IndexFields,
    files: &[CurrentFile],
    changed_ordinals: &[usize],
) -> anyhow::Result<()> {
    let changed_files = changed_ordinals
        .iter()
        .map(|&ord| {
            files
                .get(ord)
                .with_context(|| format!("manifest changed file ordinal {ord} is out of range"))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let writer = index_writer(args, index)?;
    for &ord in changed_ordinals {
        let Some(file) = files.get(ord) else {
            anyhow::bail!("manifest changed file ordinal {ord} is out of range");
        };
        writer.delete_term(Term::from_field_u64(fields.path_hash, file.path_hash()));
    }
    let writer = add_document_refs(args, table, writer, fields, &changed_files)?;
    commit_writer(writer)
}

fn index_writer(
    args: &HiArgs,
    index: &Index,
) -> anyhow::Result<tantivy::IndexWriter<TantivyDocument>> {
    let writer_threads = args.threads().clamp(1, 8);
    let memory_budget = writer_threads * TANTIVY_THREAD_BUDGET.max(MIN_TANTIVY_THREAD_BUDGET);
    index
        .writer_with_num_threads::<TantivyDocument>(writer_threads, memory_budget)
        .context("failed to create tantivy index writer")
}

fn commit_writer(mut writer: tantivy::IndexWriter<TantivyDocument>) -> anyhow::Result<()> {
    writer.commit().context("failed to commit tantivy index")?;
    writer
        .wait_merging_threads()
        .context("failed while waiting for tantivy merge threads")?;
    Ok(())
}

fn add_documents(
    args: &HiArgs,
    table: &WeightTable,
    writer: tantivy::IndexWriter<TantivyDocument>,
    fields: IndexFields,
    files: &[CurrentFile],
) -> anyhow::Result<tantivy::IndexWriter<TantivyDocument>> {
    let use_mmap = args.index_mmap();
    let (sender, receiver) = mpsc::sync_channel(args.threads().clamp(1, 128) * 2);
    let writer_thread = thread::spawn(move || add_received_documents(writer, receiver));
    let scan_result = files
        .par_iter()
        .try_for_each_with(sender.clone(), |sender, file| {
            if let Some(file_grams) = scan_file(table, file, use_mmap)? {
                sender
                    .send(file_document(fields, &file_grams))
                    .context("tantivy index writer stopped while receiving scanned documents")?;
            }
            anyhow::Ok(())
        });
    drop(sender);
    let writer = writer_thread
        .join()
        .map_err(|_| anyhow::anyhow!("tantivy index writer thread panicked"))??;
    scan_result?;
    Ok(writer)
}

fn add_document_refs(
    args: &HiArgs,
    table: &WeightTable,
    writer: tantivy::IndexWriter<TantivyDocument>,
    fields: IndexFields,
    files: &[&CurrentFile],
) -> anyhow::Result<tantivy::IndexWriter<TantivyDocument>> {
    let use_mmap = args.index_mmap();
    let (sender, receiver) = mpsc::sync_channel(args.threads().clamp(1, 128) * 2);
    let writer_thread = thread::spawn(move || add_received_documents(writer, receiver));
    let scan_result = files
        .par_iter()
        .try_for_each_with(sender.clone(), |sender, file| {
            if let Some(file_grams) = scan_file(table, file, use_mmap)? {
                sender
                    .send(file_document(fields, &file_grams))
                    .context("tantivy index writer stopped while receiving scanned documents")?;
            }
            anyhow::Ok(())
        });
    drop(sender);
    let writer = writer_thread
        .join()
        .map_err(|_| anyhow::anyhow!("tantivy index writer thread panicked"))??;
    scan_result?;
    Ok(writer)
}

fn add_received_documents(
    writer: tantivy::IndexWriter<TantivyDocument>,
    receiver: mpsc::Receiver<TantivyDocument>,
) -> anyhow::Result<tantivy::IndexWriter<TantivyDocument>> {
    for document in receiver {
        writer.add_document(document)?;
    }
    Ok(writer)
}

fn scan_file(
    table: &WeightTable,
    file: &CurrentFile,
    use_mmap: bool,
) -> anyhow::Result<Option<FileGrams>> {
    let len = fs::metadata(&file.path)
        .with_context(|| format!("failed to stat {} for indexing", file.path.display()))?
        .len();
    if len == 0 {
        return Ok(Some(file_grams(file, false, Vec::new())));
    }
    if super::classify::is_oversized(len) {
        // TODO(lib): try_scan so oversized files scan instead of force-candidate
        return Ok(Some(file_grams(file, true, Vec::new())));
    }
    let bytes = read_file(&file.path, use_mmap)?;
    let bytes = bytes.as_ref();
    if super::classify::is_binary(bytes) {
        return Ok(None);
    }
    let mut forced_candidate = super::classify::has_decoding_bom(bytes);
    let content = Content::new(bytes);
    let mut hashes = Vec::new();
    sngram::scan_with(table, &content, super::postings::SCAN_PRIMARY, |_, hash| {
        hashes.push(hash);
    });
    hashes.sort_unstable();
    hashes.dedup();
    if super::classify::is_high_entropy(bytes.len(), hashes.len()) {
        forced_candidate = true;
        hashes.clear();
    } else {
        let primary_unique = hashes.len();
        sngram::scan_with(table, &content, super::postings::SCAN_FOLDED, |_, hash| {
            hashes.push(hash);
        });
        if let Some(folded) = hashes.get_mut(primary_unique..) {
            folded.sort_unstable();
        }
        hashes.dedup();
    }
    Ok(Some(file_grams(file, forced_candidate, hashes)))
}

fn file_grams(file: &CurrentFile, forced_candidate: bool, hashes: Vec<u64>) -> FileGrams {
    FileGrams {
        ord: file.ord,
        path_hash: file.path_hash(),
        forced_candidate,
        hashes,
    }
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
    let metadata = file
        .metadata()
        .with_context(|| format!("failed to stat {} for mmap indexing", path.display()))?;
    if metadata.len() == 0 {
        anyhow::bail!("indexed search cannot mmap empty file {}", path.display());
    }
    // SAFETY: The map is read-only and scoped to this indexing worker. eg never
    // mutates the mapped file; concurrent external truncation has the same OS
    // caveat as ripgrep mmap search and is treated as an indexing-time failure.
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

fn forced_candidate_query(fields: IndexFields, query: Box<dyn Query>) -> Box<dyn Query> {
    let forced: Box<dyn Query> = Box::new(TermQuery::new(
        Term::from_field_u64(fields.forced_candidate, 1),
        IndexRecordOption::Basic,
    ));
    Box::new(BooleanQuery::union(vec![query, forced]))
}

fn file_document(fields: IndexFields, file: &FileGrams) -> TantivyDocument {
    let mut document = TantivyDocument::default();
    document.add_u64(fields.doc_ord, file.ord as u64);
    document.add_u64(fields.path_hash, file.path_hash);
    if file.forced_candidate {
        document.add_u64(fields.forced_candidate, 1);
    }
    for &hash in &file.hashes {
        document.add_u64(fields.gram, hash);
    }
    document
}

fn u64_to_usize(value: u64) -> anyhow::Result<usize> {
    usize::try_from(value).context("indexed document ordinal does not fit in usize")
}

#[derive(Clone, Copy)]
pub(super) struct IndexFields {
    gram: Field,
    doc_ord: Field,
    path_hash: Field,
    forced_candidate: Field,
}

struct FileGrams {
    ord: usize,
    path_hash: u64,
    forced_candidate: bool,
    hashes: Vec<u64>,
}

struct DocOrdCollector;

impl Collector for DocOrdCollector {
    type Fruit = Vec<u64>;
    type Child = SegmentDocOrdCollector;

    fn for_segment(
        &self,
        _segment_local_id: SegmentOrdinal,
        segment: &SegmentReader,
    ) -> tantivy::Result<Self::Child> {
        Ok(SegmentDocOrdCollector {
            column: segment.fast_fields().u64(FIELD_DOC_ORD)?,
            ords: Vec::new(),
        })
    }

    fn requires_scoring(&self) -> bool {
        false
    }

    fn merge_fruits(&self, segment_fruits: Vec<Vec<u64>>) -> tantivy::Result<Vec<u64>> {
        let len = segment_fruits.iter().map(Vec::len).sum();
        let mut ords = Vec::with_capacity(len);
        for fruit in segment_fruits {
            ords.extend(fruit);
        }
        Ok(ords)
    }
}

struct SegmentDocOrdCollector {
    column: Column<u64>,
    ords: Vec<u64>,
}

impl SegmentCollector for SegmentDocOrdCollector {
    type Fruit = Vec<u64>;

    fn collect(&mut self, doc: DocId, _score: Score) {
        if let Some(ord) = self.column.values_for_doc(doc).next() {
            self.ords.push(ord);
        }
    }

    fn collect_block(&mut self, docs: &[DocId]) {
        self.ords.reserve(docs.len());
        for &doc in docs {
            if let Some(ord) = self.column.values_for_doc(doc).next() {
                self.ords.push(ord);
            }
        }
    }

    fn harvest(self) -> Self::Fruit {
        self.ords
    }
}
