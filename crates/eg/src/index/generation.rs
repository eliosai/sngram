//! Ready index generation selected for a search.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use crate::flags::HiArgs;

use super::{
    backend::CandidateQuery, bench, catalog::ReadyGeneration, location::IndexLocation, manifest,
    planner,
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

    pub const fn source(&self) -> &'static str {
        self.source
    }

    pub fn query(
        &self,
        args: &HiArgs,
        snapshot: &manifest::CurrentSnapshot,
        plan: &planner::IndexPlan,
        bench: Option<&mut bench::BenchReport>,
    ) -> anyhow::Result<Option<BTreeSet<usize>>> {
        CandidateQuery::new(args, &self.index_dir(), snapshot, plan, bench).run()
    }
}
