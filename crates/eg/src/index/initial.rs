//! Cold-miss and explicit rebuild stage.

use std::path::Path;

use crate::flags::HiArgs;

use super::{config, manifest, postings, store};

pub struct InitialBuild<'a> {
    args: &'a HiArgs,
    table_fingerprint: u64,
    table: &'a sngram_types::WeightTable,
    index_dir: &'a Path,
}

impl<'a> InitialBuild<'a> {
    pub const fn new(
        args: &'a HiArgs,
        table_fingerprint: u64,
        table: &'a sngram_types::WeightTable,
        index_dir: &'a Path,
    ) -> Self {
        Self {
            args,
            table_fingerprint,
            table,
            index_dir,
        }
    }

    pub fn run(&self, snapshot: &manifest::CurrentSnapshot) -> anyhow::Result<InitialBuildStatus> {
        match self.args.index().backend() {
            config::IndexBackend::Postings => {
                postings::rebuild(
                    self.args,
                    self.table_fingerprint,
                    self.table,
                    &self.index_dir.join("postings-v5"),
                    snapshot,
                )?;
                Ok(InitialBuildStatus::BuiltDisk)
            },
            config::IndexBackend::Tantivy => {
                let (schema, fields) = store::schema();
                store::rebuild(
                    self.args,
                    self.table_fingerprint,
                    self.table,
                    schema,
                    fields,
                    &self.index_dir.join("tantivy-v2"),
                    snapshot,
                )?;
                Ok(InitialBuildStatus::BuiltDisk)
            },
            config::IndexBackend::TantivyRam => Ok(InitialBuildStatus::Skipped),
        }
    }
}

#[derive(Clone, Copy)]
pub enum InitialBuildStatus {
    Skipped,
    BuiltDisk,
}

impl InitialBuildStatus {
    pub const fn prebuilt_disk_index(self) -> bool {
        matches!(self, Self::BuiltDisk)
    }
}
