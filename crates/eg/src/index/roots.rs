//! Search path and index-root terminology.

use std::path::{Path, PathBuf};

use crate::flags::HiArgs;

use super::request::SearchRequest;

/// Normalized paths requested by one search invocation.
pub struct SearchRoots {
    roots: Vec<SearchRoot>,
    build_root: IndexRoot,
}

impl SearchRoots {
    pub fn from_args(args: &HiArgs) -> anyhow::Result<Self> {
        Self::from_paths(args.cwd(), args.search_paths())
    }

    pub fn from_request(request: &SearchRequest<'_>) -> anyhow::Result<Self> {
        Self::from_args(request.args())
    }

    fn from_paths(cwd: &Path, paths: &[PathBuf]) -> anyhow::Result<Self> {
        let mut roots = Vec::with_capacity(paths.len().max(1));
        for path in paths {
            if path == Path::new("-") {
                anyhow::bail!("indexed search does not support stdin yet; use --no-index");
            }
            roots.push(SearchRoot::new(absolute_path(cwd, path)));
        }
        if roots.is_empty() {
            roots.push(SearchRoot::new(cwd.to_path_buf()));
        }
        let build_root = IndexRoot::new(default_build_root(cwd, &roots));
        Ok(Self { roots, build_root })
    }

    pub fn build_root(&self) -> &IndexRoot {
        &self.build_root
    }

    pub fn is_served_by(&self, index_root: &IndexRoot) -> bool {
        self.roots
            .iter()
            .all(|root| root.path.starts_with(index_root.path()))
    }

    pub fn contains(&self, cwd: &Path, path: &Path) -> bool {
        let path = absolute_path(cwd, path);
        self.roots.iter().any(|root| root.contains(&path))
    }
}

/// Directory covered by one index generation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexRoot {
    path: PathBuf,
}

impl IndexRoot {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

struct SearchRoot {
    path: PathBuf,
    kind: SearchRootKind,
}

impl SearchRoot {
    fn new(path: PathBuf) -> Self {
        let kind = if path.is_dir() {
            SearchRootKind::Directory
        } else {
            SearchRootKind::File
        };
        Self { path, kind }
    }

    fn contains(&self, path: &Path) -> bool {
        match self.kind {
            SearchRootKind::Directory => path.starts_with(&self.path),
            SearchRootKind::File => path == self.path,
        }
    }
}

#[derive(Clone, Copy)]
enum SearchRootKind {
    Directory,
    File,
}

fn default_build_root(cwd: &Path, roots: &[SearchRoot]) -> PathBuf {
    if roots.len() != 1 {
        return cwd.to_path_buf();
    }
    match roots[0].kind {
        SearchRootKind::Directory => roots[0].path.clone(),
        SearchRootKind::File => roots[0]
            .path
            .parent()
            .map_or_else(|| cwd.to_path_buf(), Path::to_path_buf),
    }
}

pub fn absolute_path(cwd: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::{IndexRoot, SearchRoots};
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    fn scratch(name: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("eg-roots-{}-{stamp}-{name}", std::process::id()));
        fs::create_dir_all(&root).expect("scratch dir");
        root
    }

    #[test]
    fn explicit_directory_is_its_own_build_root() {
        let dir = scratch("dir");
        let roots = SearchRoots::from_paths(Path::new("/tmp"), &[dir.clone()]).expect("roots");

        assert_eq!(roots.build_root().path(), dir.as_path());
        assert!(roots.contains(Path::new("/tmp"), &dir.join("file.rs")));
    }

    #[test]
    fn implicit_cwd_is_the_build_root() {
        let cwd = scratch("implicit");
        let roots = SearchRoots::from_paths(&cwd, &[]).expect("roots");

        assert_eq!(roots.build_root().path(), cwd.as_path());
        assert!(roots.contains(&cwd, &cwd.join("src/lib.rs")));
    }

    #[test]
    fn explicit_file_builds_from_parent_and_matches_only_that_file() {
        let dir = scratch("file");
        let file = dir.join("main.rs");
        fs::write(&file, "fn main() {}\n").expect("write file");
        let roots = SearchRoots::from_paths(Path::new("/tmp"), &[file.clone()]).expect("roots");

        assert_eq!(roots.build_root().path(), dir.as_path());
        assert!(roots.contains(Path::new("/tmp"), &file));
        assert!(!roots.contains(Path::new("/tmp"), &dir.join("lib.rs")));
    }

    #[test]
    fn multiple_paths_build_from_cwd() {
        let cwd = scratch("multi");
        let roots = SearchRoots::from_paths(&cwd, &[PathBuf::from("src"), PathBuf::from("tests")])
            .expect("roots");

        assert_eq!(roots.build_root().path(), cwd.as_path());
    }

    #[test]
    fn parent_index_root_serves_child_search_root() {
        let repo = scratch("parent");
        let src = repo.join("src");
        fs::create_dir_all(&src).expect("src dir");
        let roots = SearchRoots::from_paths(&repo, &[PathBuf::from("src")]).expect("roots");

        assert!(roots.is_served_by(&IndexRoot::new(repo)));
    }

    #[test]
    fn child_index_root_does_not_serve_parent_search_root() {
        let repo = scratch("child");
        let src = repo.join("src");
        fs::create_dir_all(&src).expect("src dir");
        let roots = SearchRoots::from_paths(&repo, &[PathBuf::from("./")]).expect("roots");

        assert!(!roots.is_served_by(&IndexRoot::new(src)));
    }
}
