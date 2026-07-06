//! Disk catalog for ready index generations.

use std::path::{Path, PathBuf};

use crate::flags::HiArgs;

use super::{
    config::IndexBackend,
    location::{self, IndexLocation},
    manifest::{self, ManifestBackend},
    roots::{IndexRoot, SearchRoots},
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
            let Some(manifest_path) = self.manifest_path(&state_root) else {
                continue;
            };
            let Some(manifest) = manifest::read_manifest(&manifest_path)? else {
                continue;
            };
            if !self.manifest_is_compatible(&manifest) {
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
        Ok(ReadyGeneration {
            location,
            used_parent_index: false,
            source: "cold_build",
        })
    }

    fn manifest_path(&self, state_root: &Path) -> Option<PathBuf> {
        let index_dir = state_root.join("index");
        match self.args.index().backend() {
            IndexBackend::Postings => Some(index_dir.join("postings-v5/manifest.json")),
            IndexBackend::Tantivy => Some(index_dir.join("tantivy-v2/manifest.json")),
            IndexBackend::TantivyRam => None,
        }
    }

    fn manifest_is_compatible(&self, manifest: &manifest::Manifest) -> bool {
        let backend = match self.args.index().backend() {
            IndexBackend::Postings => ManifestBackend::Postings,
            IndexBackend::Tantivy => ManifestBackend::Tantivy,
            IndexBackend::TantivyRam => return false,
        };
        manifest::is_filter_compatible(manifest, self.args, backend, self.table_fingerprint)
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
