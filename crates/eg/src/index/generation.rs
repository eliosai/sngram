//! Ready index generation selected for a search.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use crate::flags::HiArgs;

use super::{
    backend::CandidateQuery, bench, catalog::ReadyGeneration, config, location::IndexLocation,
    manifest, planner,
};

pub struct Generation {
    location: IndexLocation,
    used_parent_index: bool,
    source: &'static str,
}

impl Generation {
    pub fn from_ready(ready: ReadyGeneration) -> Self {
        Self {
            location: ready.location,
            used_parent_index: ready.used_parent_index,
            source: ready.source,
        }
    }

    pub const fn location(&self) -> &IndexLocation {
        &self.location
    }

    pub fn index_root(&self) -> &Path {
        &self.location.corpus_root
    }

    pub fn state_root(&self) -> &Path {
        &self.location.state_root
    }

    pub fn index_dir(&self) -> PathBuf {
        self.location.index_dir()
    }

    pub const fn used_parent_index(&self) -> bool {
        self.used_parent_index
    }

    pub fn is_cold_build(&self, args: &HiArgs, index_dir: &Path) -> bool {
        matches!(args.index().mode(), config::IndexMode::Rebuild)
            || self.source == "cold_build"
            || !index_present(args, index_dir)
    }

    pub fn bench_source(&self, args: &HiArgs, cold_build: bool) -> &'static str {
        if matches!(args.index().mode(), config::IndexMode::Rebuild) {
            "rebuild"
        } else if cold_build {
            "cold_build"
        } else {
            self.source
        }
    }

    pub fn query(
        &self,
        args: &HiArgs,
        table_fingerprint: u64,
        table: &sngram_types::WeightTable,
        snapshot: &manifest::CurrentSnapshot,
        loaded_manifest: Option<&manifest::Manifest>,
        plan: &planner::IndexPlan,
        prebuilt_disk_index: bool,
        bench: Option<&mut bench::BenchReport>,
    ) -> anyhow::Result<Option<BTreeSet<usize>>> {
        CandidateQuery::new(
            args,
            table_fingerprint,
            table,
            &self.index_dir(),
            snapshot,
            loaded_manifest,
            plan,
            prebuilt_disk_index,
            bench,
        )
        .run()
    }
}

pub fn index_present(args: &HiArgs, index_dir: &Path) -> bool {
    let manifest = match args.index().backend() {
        config::IndexBackend::Postings => index_dir.join("postings-v5/manifest.json"),
        config::IndexBackend::Tantivy => index_dir.join("tantivy-v2/manifest.json"),
        config::IndexBackend::TantivyRam => return false,
    };
    manifest::manifest_present(&manifest)
}
