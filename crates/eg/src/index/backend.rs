//! Backend-specific index open and sparse lookup.

use std::{collections::BTreeSet, path::Path, time::Instant};

use crate::flags::HiArgs;

use super::{bench, config, manifest, planner, postings, store};

pub struct CandidateQuery<'a> {
    args: &'a HiArgs,
    index_dir: &'a Path,
    snapshot: &'a manifest::CurrentSnapshot,
    plan: &'a planner::IndexPlan,
    bench: Option<&'a mut bench::BenchReport>,
}

impl<'a> CandidateQuery<'a> {
    pub const fn new(
        args: &'a HiArgs,
        index_dir: &'a Path,
        snapshot: &'a manifest::CurrentSnapshot,
        plan: &'a planner::IndexPlan,
        bench: Option<&'a mut bench::BenchReport>,
    ) -> Self {
        Self {
            args,
            index_dir,
            snapshot,
            plan,
            bench,
        }
    }

    pub fn run(mut self) -> anyhow::Result<Option<BTreeSet<usize>>> {
        let prepare_started_at = Instant::now();
        let candidates = match self.args.index().backend() {
            config::IndexBackend::Postings => self.query_postings()?,
            config::IndexBackend::Tantivy => self.query_tantivy()?,
        };
        log::debug!(
            "eg index: backend prepare+lookup produced {} candidates in {:?}",
            candidates.as_ref().map_or(0, BTreeSet::len),
            prepare_started_at.elapsed()
        );
        Ok(candidates)
    }

    fn query_postings(&mut self) -> anyhow::Result<Option<BTreeSet<usize>>> {
        let index_home = self.index_dir.join("postings-v6");
        let open_started_at = Instant::now();
        let index = postings::open_index(&index_home, self.snapshot)?;
        if let Some(report) = self.bench.as_deref_mut() {
            let forced = postings::forced_candidate_ordinals(&index, self.plan)?;
            report.timing_mut().set_index_open(open_started_at);
            report.set_index_bytes(&index_home);
            report.set_corpus_text_bytes(index.corpus_text_bytes());
            report.set_forced_candidate_files(u64::try_from(forced.len()).unwrap_or(u64::MAX));
        }
        postings::query_index(&index, self.plan, self.bench.as_deref_mut())
    }

    fn query_tantivy(&mut self) -> anyhow::Result<Option<BTreeSet<usize>>> {
        let index_home = self.index_dir.join("tantivy-v2");
        let (_schema, fields) = store::schema();
        let open_started_at = Instant::now();
        let index = store::open_disk_index(&index_home, self.snapshot)?;
        if let Some(report) = self.bench.as_deref_mut() {
            let forced = store::forced_candidate_ordinals(&index, fields, self.plan)?;
            report.timing_mut().set_index_open(open_started_at);
            report.set_forced_candidate_files(u64::try_from(forced.len()).unwrap_or(u64::MAX));
        }
        store::query_index(&index, fields, self.plan, self.bench.as_deref_mut())
    }
}
