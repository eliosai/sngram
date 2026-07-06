//! Freshness validation and current snapshot loading.

use std::{path::Path, time::Instant};

use crate::flags::HiArgs;

use super::{config, generation::Generation, location, manifest, runtime, walk};

pub struct LoadedSnapshot {
    current: manifest::CurrentSnapshot,
    loaded_manifest: Option<manifest::Manifest>,
    freshness_proof: &'static str,
}

impl LoadedSnapshot {
    pub fn into_parts(
        self,
    ) -> (
        manifest::CurrentSnapshot,
        Option<manifest::Manifest>,
        &'static str,
    ) {
        (self.current, self.loaded_manifest, self.freshness_proof)
    }
}

pub struct SnapshotLoader<'a> {
    args: &'a HiArgs,
    table_fingerprint: u64,
    location: &'a location::IndexLocation,
    index_dir: &'a Path,
}

impl<'a> SnapshotLoader<'a> {
    pub const fn new(
        args: &'a HiArgs,
        table_fingerprint: u64,
        location: &'a location::IndexLocation,
        index_dir: &'a Path,
    ) -> Self {
        Self {
            args,
            table_fingerprint,
            location,
            index_dir,
        }
    }

    pub fn load(&self) -> anyhow::Result<LoadedSnapshot> {
        if let Some((current, loaded_manifest)) = self.try_daemon_snapshot()? {
            return Ok(LoadedSnapshot {
                current,
                loaded_manifest: Some(loaded_manifest),
                freshness_proof: "daemon",
            });
        }
        if let Some((current, loaded_manifest)) = self.try_fast_snapshot()? {
            return Ok(LoadedSnapshot {
                current,
                loaded_manifest,
                freshness_proof: "walk",
            });
        }
        let collected = walk::collect_haystacks(self.args, &self.location.state_root)?;
        log::debug!(
            "eg index: collected {} haystacks and {} dirs",
            collected.haystacks.len(),
            collected.dirs.len()
        );
        let current = manifest::current_snapshot(
            self.args,
            &self.location.corpus_root,
            &collected.haystacks,
            &collected.dirs,
        )?;
        Ok(LoadedSnapshot {
            current,
            loaded_manifest: None,
            freshness_proof: "walk",
        })
    }

    fn try_daemon_snapshot(
        &self,
    ) -> anyhow::Result<Option<(manifest::CurrentSnapshot, manifest::Manifest)>> {
        if !matches!(
            self.args.index().mode(),
            config::IndexMode::Auto | config::IndexMode::Require
        ) || !runtime::daemon_freshness_proof(&self.location.state_root)
        {
            return Ok(None);
        }
        let (backend, manifest_path) = match self.args.index().backend() {
            config::IndexBackend::Postings => (
                manifest::ManifestBackend::Postings,
                self.index_dir.join("postings-v5/manifest.json"),
            ),
            config::IndexBackend::Tantivy => (
                manifest::ManifestBackend::Tantivy,
                self.index_dir.join("tantivy-v2/manifest.json"),
            ),
            config::IndexBackend::TantivyRam => return Ok(None),
        };
        let Some(loaded_manifest) = manifest::read_manifest(&manifest_path)? else {
            return Ok(None);
        };
        if !manifest::is_filter_compatible(
            &loaded_manifest,
            self.args,
            backend,
            self.table_fingerprint,
        ) {
            return Ok(None);
        }
        let current =
            manifest::snapshot_from_manifest(&self.location.corpus_root, &loaded_manifest);
        log::debug!(
            "eg index: loaded daemon-proofed manifest snapshot for {} files",
            current.files.len()
        );
        Ok(Some((current, loaded_manifest)))
    }

    fn try_fast_snapshot(
        &self,
    ) -> anyhow::Result<Option<(manifest::CurrentSnapshot, Option<manifest::Manifest>)>> {
        if !matches!(
            self.args.index().mode(),
            config::IndexMode::Auto | config::IndexMode::Require
        ) || matches!(
            self.args.index().backend(),
            config::IndexBackend::TantivyRam
        ) {
            return Ok(None);
        }
        let (backend, manifest_path) = match self.args.index().backend() {
            config::IndexBackend::Postings => (
                manifest::ManifestBackend::Postings,
                self.index_dir.join("postings-v5/manifest.json"),
            ),
            config::IndexBackend::Tantivy => (
                manifest::ManifestBackend::Tantivy,
                self.index_dir.join("tantivy-v2/manifest.json"),
            ),
            config::IndexBackend::TantivyRam => return Ok(None),
        };
        let manifest_read_started_at = Instant::now();
        let Some(loaded_manifest) = manifest::read_manifest(&manifest_path)? else {
            return Ok(None);
        };
        log::debug!(
            "eg index: read manifest {} in {:?}",
            manifest_path.display(),
            manifest_read_started_at.elapsed()
        );
        if !manifest::is_compatible(&loaded_manifest, backend, self.table_fingerprint) {
            return Ok(None);
        }
        let started_at = Instant::now();
        let Some(current) =
            manifest::fast_snapshot(self.args, &self.location.corpus_root, &loaded_manifest)?
        else {
            log::debug!(
                "eg index: fast freshness snapshot invalidated in {:?}",
                started_at.elapsed()
            );
            return Ok(None);
        };
        log::debug!(
            "eg index: loaded fast freshness snapshot for {} files in {:?}",
            current.files.len(),
            started_at.elapsed()
        );
        Ok(Some((current, Some(loaded_manifest))))
    }
}

pub fn generation_source(
    args: &HiArgs,
    table_fingerprint: u64,
    generation: &Generation,
    snapshot: &manifest::CurrentSnapshot,
    loaded_manifest: Option<&manifest::Manifest>,
    cold_build: bool,
) -> &'static str {
    let source = generation.bench_source(args, cold_build);
    if source == "hot" && snapshot_has_delta(args, table_fingerprint, snapshot, loaded_manifest) {
        "delta"
    } else {
        source
    }
}

fn snapshot_has_delta(
    args: &HiArgs,
    table_fingerprint: u64,
    snapshot: &manifest::CurrentSnapshot,
    loaded_manifest: Option<&manifest::Manifest>,
) -> bool {
    let Some(loaded_manifest) = loaded_manifest else {
        return false;
    };
    let backend = match args.index().backend() {
        config::IndexBackend::Postings => manifest::ManifestBackend::Postings,
        config::IndexBackend::Tantivy | config::IndexBackend::TantivyRam => {
            manifest::ManifestBackend::Tantivy
        },
    };
    let expected = manifest::manifest_for(backend, table_fingerprint, snapshot);
    manifest::changed_ordinals(loaded_manifest, &expected)
        .is_some_and(|changed| !changed.is_empty())
}
