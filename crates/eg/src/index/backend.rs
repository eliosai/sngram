//! Backend-specific index open and sparse lookup.

use std::{collections::BTreeSet, path::Path, time::Instant};

use crate::flags::HiArgs;

use super::{bench, config, manifest, planner, postings, store};

pub struct CandidateQuery<'a> {
    args: &'a HiArgs,
    table_fingerprint: u64,
    table: &'a sngram_types::WeightTable,
    index_dir: &'a Path,
    snapshot: &'a manifest::CurrentSnapshot,
    loaded_manifest: Option<&'a manifest::Manifest>,
    plan: &'a planner::IndexPlan,
    prebuilt_disk_index: bool,
    bench: Option<&'a mut bench::BenchReport>,
}

impl<'a> CandidateQuery<'a> {
    pub const fn new(
        args: &'a HiArgs,
        table_fingerprint: u64,
        table: &'a sngram_types::WeightTable,
        index_dir: &'a Path,
        snapshot: &'a manifest::CurrentSnapshot,
        loaded_manifest: Option<&'a manifest::Manifest>,
        plan: &'a planner::IndexPlan,
        prebuilt_disk_index: bool,
        bench: Option<&'a mut bench::BenchReport>,
    ) -> Self {
        Self {
            args,
            table_fingerprint,
            table,
            index_dir,
            snapshot,
            loaded_manifest,
            plan,
            prebuilt_disk_index,
            bench,
        }
    }

    pub fn run(mut self) -> anyhow::Result<Option<BTreeSet<usize>>> {
        let prepare_started_at = Instant::now();
        let candidates = match self.args.index().backend() {
            config::IndexBackend::Postings => self.query_postings()?,
            config::IndexBackend::Tantivy | config::IndexBackend::TantivyRam => {
                self.query_tantivy()?
            },
        };
        log::debug!(
            "eg index: backend prepare+lookup produced {} candidates in {:?}",
            candidates.as_ref().map_or(0, BTreeSet::len),
            prepare_started_at.elapsed()
        );
        Ok(candidates)
    }

    fn query_postings(&mut self) -> anyhow::Result<Option<BTreeSet<usize>>> {
        let index_home = self.index_dir.join("postings-v5");
        let open_started_at = Instant::now();
        let index = if self.prebuilt_disk_index {
            postings::open_index(&index_home, self.snapshot)?
        } else {
            postings::prepare_index(
                self.args,
                self.table_fingerprint,
                self.table,
                &index_home,
                self.snapshot,
                self.loaded_manifest,
            )?
        };
        if let Some(report) = self.bench.as_deref_mut() {
            let forced = postings::forced_candidate_ordinals(&index, self.plan)?;
            let dirty_forced = dirty_forced_candidates(
                self.args,
                self.table_fingerprint,
                self.snapshot,
                self.loaded_manifest,
                &forced,
                manifest::ManifestBackend::Postings,
            );
            report.timing_mut().set_index_mmap(open_started_at);
            report.set_index_bytes(&index_home);
            report.set_forced_candidate_files(u64::try_from(forced.len()).unwrap_or(u64::MAX));
            report.set_dirty_forced_candidates(dirty_forced);
        }
        postings::query_index(&index, self.plan)
    }

    fn query_tantivy(&mut self) -> anyhow::Result<Option<BTreeSet<usize>>> {
        let index_home = self.index_dir.join("tantivy-v2");
        let (schema, fields) = store::schema();
        let open_started_at = Instant::now();
        let index = if self.prebuilt_disk_index {
            store::open_disk_index(&index_home, self.snapshot)?
        } else {
            store::prepare_index(
                self.args,
                self.table_fingerprint,
                self.table,
                schema,
                fields,
                &index_home,
                self.snapshot,
                self.loaded_manifest,
            )?
        };
        if let Some(report) = self.bench.as_deref_mut() {
            let forced = store::forced_candidate_ordinals(&index, fields, self.plan)?;
            let dirty_forced = dirty_forced_candidates(
                self.args,
                self.table_fingerprint,
                self.snapshot,
                self.loaded_manifest,
                &forced,
                manifest::ManifestBackend::Tantivy,
            );
            report.timing_mut().set_index_mmap(open_started_at);
            report.set_forced_candidate_files(u64::try_from(forced.len()).unwrap_or(u64::MAX));
            report.set_dirty_forced_candidates(dirty_forced);
        }
        store::query_index(&index, fields, self.plan)
    }
}

fn dirty_forced_candidates(
    args: &HiArgs,
    table_fingerprint: u64,
    snapshot: &manifest::CurrentSnapshot,
    loaded_manifest: Option<&manifest::Manifest>,
    forced: &[usize],
    backend: manifest::ManifestBackend,
) -> u64 {
    let Some(loaded_manifest) = loaded_manifest else {
        return 0;
    };
    let expected = manifest::manifest_for(backend, table_fingerprint, snapshot);
    let Some(changed) = manifest::changed_ordinals(loaded_manifest, &expected) else {
        return 0;
    };
    if !manifest::is_filter_compatible(loaded_manifest, args, backend, table_fingerprint) {
        return 0;
    }
    forced
        .iter()
        .filter(|ord| changed.binary_search(ord).is_ok())
        .count()
        .try_into()
        .unwrap_or(u64::MAX)
}
