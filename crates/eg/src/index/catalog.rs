//! Disk catalog for ready index generations.

use std::path::{Path, PathBuf};

use crate::flags::HiArgs;

use super::{
    config::IndexBackend,
    location::{self, IndexLocation},
    manifest::{self, ManifestBackend},
    roots::{IndexRoot, SearchRoots},
    runtime,
};

/// Disk-first resolver for index roots and currently published generations.
pub struct GenerationCatalog<'a> {
    args: &'a HiArgs,
    table_fingerprint: u64,
}

impl<'a> GenerationCatalog<'a> {
    pub fn open(args: &'a HiArgs, table_fingerprint: u64) -> Self {
        Self {
            args,
            table_fingerprint,
        }
    }

    pub fn best_ready_generation(&self, roots: &SearchRoots) -> anyhow::Result<ReadyGeneration> {
        if self.args.index().dir().is_some() {
            return self.exact_generation(roots);
        }
        for index_root in candidate_index_roots(roots.build_root()) {
            if !roots.is_served_by(&index_root) {
                continue;
            }
            let state_root = location::local_state_root(index_root.path());
            if !state_root.is_dir() {
                continue;
            }
            let Some(manifest_path) = self.manifest_path(&state_root) else {
                continue;
            };
            if !self.manifest_is_compatible(&manifest_path)? {
                continue;
            }
            if !runtime::daemon_freshness_proof(&state_root) {
                continue;
            }
            return Ok(ReadyGeneration {
                location: IndexLocation {
                    corpus_root: index_root.path().to_path_buf(),
                    state_root,
                },
                used_parent_index: index_root.path() != roots.build_root().path(),
                source: "hot",
            });
        }
        self.exact_generation(roots)
    }

    fn exact_generation(&self, roots: &SearchRoots) -> anyhow::Result<ReadyGeneration> {
        let location = location::resolve(self.args, roots.build_root().path())?;
        let source = self.exact_generation_source(&location)?;
        Ok(ReadyGeneration {
            location,
            used_parent_index: false,
            source,
        })
    }

    fn exact_generation_source(&self, location: &IndexLocation) -> anyhow::Result<&'static str> {
        let Some(manifest_path) = self.manifest_path(&location.state_root) else {
            return Ok("missing");
        };
        if !self.manifest_is_compatible(&manifest_path)? {
            return Ok("missing");
        }
        if runtime::daemon_freshness_proof(&location.state_root) {
            Ok("hot")
        } else {
            Ok("stale")
        }
    }

    fn manifest_path(&self, state_root: &Path) -> Option<PathBuf> {
        let index_dir = state_root.join("index");
        match self.args.index().backend() {
            IndexBackend::Postings => Some(index_dir.join("postings-v9/manifest.json")),
            IndexBackend::Tantivy => Some(index_dir.join("tantivy-v2/manifest.json")),
        }
    }

    fn manifest_is_compatible(&self, manifest_path: &Path) -> anyhow::Result<bool> {
        let backend = match self.args.index().backend() {
            IndexBackend::Postings => ManifestBackend::Postings,
            IndexBackend::Tantivy => ManifestBackend::Tantivy,
        };
        manifest::manifest_path_is_filter_compatible(
            manifest_path,
            self.args,
            backend,
            self.table_fingerprint,
        )
    }
}

pub struct ReadyGeneration {
    pub location: IndexLocation,
    pub used_parent_index: bool,
    pub source: &'static str,
}

fn candidate_index_roots(build_root: &IndexRoot) -> Vec<IndexRoot> {
    let mut roots = build_root
        .path()
        .ancestors()
        .map(|path| IndexRoot::new(path.to_path_buf()))
        .collect::<Vec<_>>();
    roots.reverse();
    roots
}

#[cfg(test)]
mod tests {
    use super::candidate_index_roots;
    use crate::index::roots::IndexRoot;
    use std::path::PathBuf;

    #[test]
    fn candidate_roots_are_broadest_parent_first() {
        let roots = candidate_index_roots(&IndexRoot::new(PathBuf::from("/repo/src/parser")));
        let rendered = roots
            .iter()
            .map(|root| root.path().display().to_string())
            .collect::<Vec<_>>();

        assert_eq!(
            rendered,
            vec!["/", "/repo", "/repo/src", "/repo/src/parser"]
        );
    }
}
