//! Tantivy storage for sparse n-gram postings.

use std::{collections::BTreeSet, fs, path::Path, sync::mpsc, thread};

use anyhow::Context;
use rayon::prelude::*;
use sngram_types::{DfStats, GramKey, QueryPlan, WeightTable};
use tantivy::{
    DocId, Index, Score, Searcher, SegmentOrdinal, SegmentReader, TantivyDocument, Term,
    collector::{Collector, SegmentCollector},
    fastfield::Column,
    query::TermQuery,
    schema::{FAST, Field, INDEXED, IndexRecordOption, STORED, Schema},
};

use crate::flags::HiArgs;

use super::{
    bench,
    document::IndexedDocument,
    executor::{self, PlanBackend},
    manifest::{
        CurrentFile, CurrentSnapshot, Manifest, ManifestBackend, changed_ordinals, manifest_for,
        manifest_present, read_manifest, write_manifest, write_path_table,
    },
    progress::{BuildPhase, BuildProgress},
    summary::{self, SummaryIndex},
};

const INDEX_DATA_DIR_NAME: &str = "tantivy";
const MANIFEST_FILE_NAME: &str = "manifest.json";
const FIELD_GRAM: &str = "gram";
const FIELD_DOC_ORD: &str = "doc_ord";
const FIELD_PATH_HASH: &str = "path_hash";
const FIELD_FORCED_CANDIDATE: &str = "forced_candidate";
const MIN_TANTIVY_THREAD_BUDGET: usize = 15_000_000;
const TANTIVY_THREAD_BUDGET: usize = 64 * 1024 * 1024;

pub struct TantivyIndex {
    index: Index,
    summaries: SummaryIndex,
}

pub fn refresh_index(
    args: &HiArgs,
    table_fingerprint: u64,
    table: &WeightTable,
    schema: Schema,
    fields: IndexFields,
    index_home: &Path,
    snapshot: &CurrentSnapshot,
    progress: Option<&BuildProgress>,
) -> anyhow::Result<bench::BuildTimings> {
    auto_disk_index(
        args,
        table_fingerprint,
        table,
        schema,
        fields,
        index_home,
        snapshot,
        None,
        progress,
    )
    .map(|(_, timings)| timings)
}

pub fn open_disk_index(
    index_home: &Path,
    snapshot: &CurrentSnapshot,
) -> anyhow::Result<TantivyIndex> {
    let data_dir = index_home.join(INDEX_DATA_DIR_NAME);
    let index = Index::open_in_dir(&data_dir).with_context(|| {
        format!(
            "failed to open daemon-owned tantivy index at {}",
            data_dir.display()
        )
    })?;
    let summaries = SummaryIndex::open(
        &index_home.join(summary::SUMMARY_FILE_NAME),
        snapshot.file_count(),
    )?
    .with_context(|| format!("summary index missing at {}", index_home.display()))?;
    Ok(TantivyIndex { index, summaries })
}

pub fn query_index(
    index: &TantivyIndex,
    fields: IndexFields,
    index_plan: &super::planner::IndexPlan,
    mut bench: Option<&mut bench::BenchReport>,
) -> anyhow::Result<Option<BTreeSet<usize>>> {
    let reader = index
        .index
        .reader()
        .context("failed to open tantivy index reader")?;
    let searcher = reader.searcher();
    let text_count = index.summaries.text_count() as u64;
    let ceiling = super::postings::selectivity_ceiling(text_count);
    let can_refine_estimate = index_plan.has_root_gram_constraints();
    let df = TantivyDf {
        searcher: &searcher,
        fields,
        text_count,
    };
    let mut plan = index_plan.plan.clone();
    let raw_grams = count_plan_grams(&plan);
    let tune_started_at = std::time::Instant::now();
    plan.tune(&df, ceiling);
    if let Some(report) = bench.as_deref_mut() {
        report.timing_mut().set_index_tune(tune_started_at);
        report.set_tuned_query_grams(count_plan_grams(&plan));
    }
    log::debug!(
        "eg index query: tantivy plan_grams={} tuned_plan_grams={}",
        raw_grams,
        count_plan_grams(&plan),
    );
    if plan.is_none() {
        return Ok(Some(BTreeSet::new()));
    }
    let backend = TantivyPlanBackend {
        searcher: &searcher,
        fields,
        summaries: &index.summaries,
    };
    let estimate = estimate_with_forced(&backend, &plan, &df)?;
    if estimate > ceiling {
        if !can_refine_estimate
            || estimate > super::postings::selectivity_refinement_ceiling(ceiling, text_count)
        {
            log::debug!(
                "eg index query: estimate {estimate} of {text_count} text docs exceeds {}%; rejecting indexed query without scan fallback",
                super::postings::SCAN_FALLBACK_PCT
            );
            return Ok(None);
        }
        log::debug!(
            "eg index query: refining estimate {estimate} of {text_count} text docs with bounded sparse lookup"
        );
    }
    let execute_started_at = std::time::Instant::now();
    let ords = executor::execute(&backend, &plan, executor::Precision::Doc)?;
    if let Some(report) = bench.as_deref_mut() {
        report.timing_mut().set_index_execute(execute_started_at);
    }
    if ords.len() as u64 > ceiling {
        log::debug!(
            "eg index query: actual candidates {} of {text_count} text docs exceed {}%; rejecting indexed query without scan fallback",
            ords.len(),
            super::postings::SCAN_FALLBACK_PCT
        );
        return Ok(None);
    }
    Ok(Some(ords.into_iter().collect()))
}

pub fn forced_candidate_ordinals(
    index: &TantivyIndex,
    fields: IndexFields,
    index_plan: &super::planner::IndexPlan,
) -> anyhow::Result<Vec<usize>> {
    let reader = index
        .index
        .reader()
        .context("failed to open tantivy index reader")?;
    let searcher = reader.searcher();
    let backend = TantivyPlanBackend {
        searcher: &searcher,
        fields,
        summaries: &index.summaries,
    };
    executor::forced_candidates(&backend, &index_plan.plan)
}

fn estimate_with_forced(
    backend: &TantivyPlanBackend<'_>,
    plan: &QueryPlan,
    df: &TantivyDf<'_>,
) -> anyhow::Result<u64> {
    if plan.is_none() {
        return Ok(0);
    }
    let forced = executor::estimate_forced_candidates(backend, plan)?;
    Ok(executor::estimate_candidates(backend, plan, df)
        .saturating_add(forced)
        .min(backend.summaries.text_count() as u64))
}

struct TantivyDf<'a> {
    searcher: &'a Searcher,
    fields: IndexFields,
    text_count: u64,
}

impl DfStats for TantivyDf<'_> {
    fn entry_count(&self, key: GramKey) -> u64 {
        self.searcher
            .doc_freq(&Term::from_field_u64(self.fields.gram, key.value()))
            .unwrap_or(0)
    }

    fn total_entries(&self) -> u64 {
        self.text_count
    }
}

struct TantivyPlanBackend<'a> {
    searcher: &'a Searcher,
    fields: IndexFields,
    summaries: &'a SummaryIndex,
}

impl PlanBackend for TantivyPlanBackend<'_> {
    fn summaries(&self) -> &SummaryIndex {
        self.summaries
    }

    fn lookup_gram(&self, key: GramKey) -> anyhow::Result<Vec<executor::Posting>> {
        let ords = self.lookup_term(Term::from_field_u64(self.fields.gram, key.value()))?;
        Ok(ords.into_iter().map(executor::Posting::full).collect())
    }

    fn forced_candidates(&self) -> anyhow::Result<Vec<usize>> {
        self.lookup_term(Term::from_field_u64(self.fields.forced_candidate, 1))
    }
}

impl TantivyPlanBackend<'_> {
    fn lookup_term(&self, term: Term) -> anyhow::Result<Vec<usize>> {
        let query = TermQuery::new(term, IndexRecordOption::Basic);
        let mut ords = self
            .searcher
            .search(&query, &DocOrdCollector)
            .context("failed to query sparse n-gram index")?
            .into_iter()
            .map(u64_to_usize)
            .collect::<anyhow::Result<Vec<_>>>()?;
        ords.sort_unstable();
        ords.dedup();
        Ok(ords)
    }
}

fn count_plan_grams(plan: &QueryPlan) -> usize {
    plan.gram_count()
}

pub fn schema() -> (Schema, IndexFields) {
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
    progress: Option<&BuildProgress>,
) -> anyhow::Result<(TantivyIndex, bench::BuildTimings)> {
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
            progress,
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
                    progress,
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
            progress,
        );
    };
    let index = Index::open_in_dir(&data_dir).with_context(|| {
        format!(
            "failed to open daemon-owned tantivy index at {}",
            data_dir.display()
        )
    })?;
    if changed_ordinals.is_empty() {
        if let Some(summaries) = SummaryIndex::open(
            &index_home.join(summary::SUMMARY_FILE_NAME),
            snapshot.file_count(),
        )? {
            return Ok((
                TantivyIndex { index, summaries },
                bench::BuildTimings::default(),
            ));
        }
    }
    rebuild_disk_index(
        args,
        table_fingerprint,
        table,
        schema,
        fields,
        index_home,
        snapshot,
        progress,
    )
}

fn rebuild_disk_index(
    args: &HiArgs,
    table_fingerprint: u64,
    table: &WeightTable,
    schema: Schema,
    fields: IndexFields,
    index_home: &Path,
    snapshot: &CurrentSnapshot,
    progress: Option<&BuildProgress>,
) -> anyhow::Result<(TantivyIndex, bench::BuildTimings)> {
    let mut timings = bench::BuildTimings::default();
    if index_home.exists() {
        fs::remove_dir_all(index_home)
            .with_context(|| format!("failed to remove old index at {}", index_home.display()))?;
    }
    let data_dir = index_home.join(INDEX_DATA_DIR_NAME);
    fs::create_dir_all(&data_dir)
        .with_context(|| format!("failed to create index directory {}", data_dir.display()))?;
    let index = Index::create_in_dir(&data_dir, schema)
        .with_context(|| format!("failed to create tantivy index at {}", data_dir.display()))?;
    let scan_started_at = std::time::Instant::now();
    let summaries = add_all_documents(
        args,
        table,
        &index,
        fields,
        snapshot.eager_files(),
        progress,
    )?;
    timings.set_scan_documents(scan_started_at);
    let summary_started_at = std::time::Instant::now();
    if let Some(progress) = progress {
        progress.phase(BuildPhase::WritingSummary);
    }
    let mut records = summaries.clone();
    summary::write_records(&index_home.join(summary::SUMMARY_FILE_NAME), &mut records)?;
    timings.set_write_summary(summary_started_at);
    let manifest_started_at = std::time::Instant::now();
    if let Some(progress) = progress {
        progress.phase(BuildPhase::WritingManifest);
    }
    write_manifest(
        &index_home.join(MANIFEST_FILE_NAME),
        &manifest_for(ManifestBackend::Tantivy, table_fingerprint, snapshot),
    )?;
    write_path_table(&index_home.join(MANIFEST_FILE_NAME), snapshot)?;
    timings.set_write_manifest(manifest_started_at);
    if let Some(progress) = progress {
        progress.phase(BuildPhase::Publishing);
    }
    Ok((
        TantivyIndex {
            index,
            summaries: SummaryIndex::from_records(summaries, snapshot.file_count())?,
        },
        timings,
    ))
}

fn add_all_documents(
    args: &HiArgs,
    table: &WeightTable,
    index: &Index,
    fields: IndexFields,
    files: &[CurrentFile],
    progress: Option<&BuildProgress>,
) -> anyhow::Result<Vec<summary::SummaryRecord>> {
    let writer = index_writer(args, index)?;
    let (writer, summaries) = add_documents(args, table, writer, fields, files, progress)?;
    commit_writer(writer)?;
    Ok(summaries)
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
    progress: Option<&BuildProgress>,
) -> anyhow::Result<(
    tantivy::IndexWriter<TantivyDocument>,
    Vec<summary::SummaryRecord>,
)> {
    let use_mmap = args.index_mmap();
    let (sender, receiver) = mpsc::sync_channel(args.threads().clamp(1, 128) * 2);
    let writer_thread = thread::spawn(move || add_received_documents(writer, receiver));
    let summaries = std::sync::Mutex::new(Vec::with_capacity(files.len()));
    let scanned = std::sync::atomic::AtomicU64::new(0);
    let scan_result = files
        .par_iter()
        .try_for_each_with(sender.clone(), |sender, file| {
            let document = super::document::scan(table, file, use_mmap)?;
            let done = scanned.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            if let Some(progress) = progress {
                progress.update_scan(files.len(), done, 0, 0);
            }
            summaries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(document.summary);
            if !document.is_skipped() {
                sender
                    .send(file_document(fields, &document))
                    .context("tantivy index writer stopped while receiving scanned documents")?;
            }
            anyhow::Ok(())
        });
    drop(sender);
    let writer = writer_thread
        .join()
        .map_err(|_| anyhow::anyhow!("tantivy index writer thread panicked"))??;
    scan_result?;
    let summaries = summaries
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Ok((writer, summaries))
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

fn file_document(fields: IndexFields, file: &IndexedDocument) -> TantivyDocument {
    let mut document = TantivyDocument::default();
    document.add_u64(fields.doc_ord, u64::from(file.ord));
    document.add_u64(fields.path_hash, file.path_hash);
    if file.forced_candidate {
        document.add_u64(fields.forced_candidate, 1);
    }
    for &(hash, _mask) in &file.hashes {
        document.add_u64(fields.gram, hash);
    }
    document
}

fn u64_to_usize(value: u64) -> anyhow::Result<usize> {
    usize::try_from(value).context("indexed document ordinal does not fit in usize")
}

#[derive(Clone, Copy)]
pub struct IndexFields {
    gram: Field,
    doc_ord: Field,
    path_hash: Field,
    forced_candidate: Field,
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
