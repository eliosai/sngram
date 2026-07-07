//! Daemon-proofed manifest snapshot loading.

use std::path::{Path, PathBuf};

use crate::flags::HiArgs;

use super::{config, location, manifest, runtime};

pub struct LoadedSnapshot {
    current: manifest::CurrentSnapshot,
    freshness_proof: &'static str,
}

impl LoadedSnapshot {
    pub fn into_parts(self) -> (manifest::CurrentSnapshot, &'static str) {
        (self.current, self.freshness_proof)
    }
}

pub struct SnapshotLoader<'a> {
    args: &'a HiArgs,
    table_fingerprint: u64,
    location: &'a location::IndexLocation,
    manifest_path: PathBuf,
}

impl<'a> SnapshotLoader<'a> {
    pub fn new(
        args: &'a HiArgs,
        table_fingerprint: u64,
        location: &'a location::IndexLocation,
        index_dir: &'a Path,
    ) -> Self {
        let manifest_path = match args.index().backend() {
            config::IndexBackend::Postings => index_dir.join("postings-v6/manifest.json"),
            config::IndexBackend::Tantivy => index_dir.join("tantivy-v2/manifest.json"),
        };
        Self {
            args,
            table_fingerprint,
            location,
            manifest_path,
        }
    }

    pub fn load(self) -> anyhow::Result<LoadedSnapshot> {
        if let Some(current) = self.try_daemon_snapshot()? {
            return Ok(LoadedSnapshot {
                current,
                freshness_proof: "daemon",
            });
        }
        anyhow::bail!(
            "daemon-owned index is not ready for {}",
            self.location.corpus_root.display()
        )
    }

    fn try_daemon_snapshot(&self) -> anyhow::Result<Option<manifest::CurrentSnapshot>> {
        if !runtime::daemon_freshness_proof(&self.location.state_root) {
            return Ok(None);
        }
        let backend = match self.args.index().backend() {
            config::IndexBackend::Postings => manifest::ManifestBackend::Postings,
            config::IndexBackend::Tantivy => manifest::ManifestBackend::Tantivy,
        };
        let Some(current) = manifest::read_current_snapshot(
            &self.manifest_path,
            &self.location.corpus_root,
            self.args,
            backend,
            self.table_fingerprint,
        )?
        else {
            return Ok(None);
        };
        log::debug!(
            "eg index: loaded daemon-proofed manifest snapshot for {} files",
            current.file_count()
        );
        Ok(Some(current))
    }
}
