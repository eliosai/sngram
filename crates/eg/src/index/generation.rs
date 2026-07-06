//! Ready index generation selected for a search.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use crate::flags::HiArgs;

use super::{bench, catalog::ReadyGeneration, config, location::IndexLocation, manifest, planner};

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
            || !super::index_present(args, index_dir)
    }

    pub fn bench_source(&self, cold_build: bool) -> &'static str {
        if cold_build {
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
        cold_build: bool,
        bench: Option<&mut bench::BenchReport>,
    ) -> anyhow::Result<Option<BTreeSet<usize>>> {
        super::backend_candidates(
            args,
            table_fingerprint,
            table,
            &self.index_dir(),
            snapshot,
            loaded_manifest,
            plan,
            cold_build,
            bench,
        )
    }
}
