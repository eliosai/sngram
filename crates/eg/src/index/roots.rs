//! Search path and index-root terminology.

use std::path::{Component, Path, PathBuf};

use crate::flags::HiArgs;

use super::request::SearchRequest;

/// Normalized paths requested by one search invocation.
pub struct SearchRoots {
    roots: Vec<SearchRoot>,
    build_root: IndexRoot,
    strip_dot_prefix: bool,
}

impl SearchRoots {
    pub fn from_args(args: &HiArgs) -> anyhow::Result<Self> {
        Self::from_paths(args.cwd(), args.search_paths(), args.has_implicit_path())
    }

    pub fn from_request(request: &SearchRequest<'_>) -> anyhow::Result<Self> {
        Self::from_args(request.args())
    }

    fn from_paths(cwd: &Path, paths: &[PathBuf], strip_dot_prefix: bool) -> anyhow::Result<Self> {
        let mut roots = Vec::with_capacity(paths.len().max(1));
        for path in paths {
            if path == Path::new("-") {
                anyhow::bail!("indexed search does not support stdin yet; use --no-index");
            }
            roots.push(SearchRoot::new(path.clone(), absolute_path(cwd, path))?);
        }
        if roots.is_empty() {
            roots.push(SearchRoot::new(cwd.to_path_buf(), cwd.to_path_buf())?);
        }
        let build_root = IndexRoot::new(default_build_root(cwd, &roots)?);
        Ok(Self {
            roots,
            build_root,
            strip_dot_prefix,
        })
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
        let path = normalize_lexically(&absolute_path(cwd, path));
        self.roots.iter().any(|root| root.contains(&path))
    }

    /// Render an indexed file the way the walker would for these arguments
    pub fn display_path(&self, path: &Path) -> PathBuf {
        let normalized = normalize_lexically(path);
        let Some(root) = self.roots.iter().find(|root| root.contains(&normalized)) else {
            return path.to_path_buf();
        };
        let display = root.display_path(&normalized);
        if self.strip_dot_prefix
            && let Ok(stripped) = display.strip_prefix("./")
        {
            return stripped.to_path_buf();
        }
        display
    }

    pub fn covers_index_root(&self, index_root: &Path) -> bool {
        matches!(
            self.roots.as_slice(),
            [SearchRoot {
                path,
                kind: SearchRootKind::Directory,
                ..
            }] if path == index_root
        )
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
    given: PathBuf,
    path: PathBuf,
    kind: SearchRootKind,
}

impl SearchRoot {
    fn new(given: PathBuf, path: PathBuf) -> anyhow::Result<Self> {
        let path = normalize_lexically(&path);
        let kind = if path.is_dir() {
            SearchRootKind::Directory
        } else if path.exists() {
            SearchRootKind::File
        } else {
            anyhow::bail!("search path {} does not exist", path.display());
        };
        Ok(Self { given, path, kind })
    }

    fn contains(&self, path: &Path) -> bool {
        match self.kind {
            SearchRootKind::Directory => path.starts_with(&self.path),
            SearchRootKind::File => path == self.path,
        }
    }

    fn display_path(&self, path: &Path) -> PathBuf {
        match path.strip_prefix(&self.path) {
            Ok(relative) if !relative.as_os_str().is_empty() => self.given.join(relative),
            _ => self.given.clone(),
        }
    }

    /// Directory an index must cover to answer this root
    fn covering_dir(&self, cwd: &Path) -> PathBuf {
        match self.kind {
            SearchRootKind::Directory => self.path.clone(),
            SearchRootKind::File => self
                .path
                .parent()
                .map_or_else(|| cwd.to_path_buf(), Path::to_path_buf),
        }
    }
}

#[derive(Clone, Copy)]
enum SearchRootKind {
    Directory,
    File,
}

/// Smallest directory containing every search root.
///
/// The working directory is not it: search paths outside the working directory
/// would leave the index covering a tree the query never asks about, and every
/// requested file uncovered.
fn default_build_root(cwd: &Path, roots: &[SearchRoot]) -> anyhow::Result<PathBuf> {
    let shared = roots
        .iter()
        .map(|root| root.covering_dir(cwd))
        .reduce(|shared, dir| shared_ancestor(&shared, &dir))
        .unwrap_or_else(|| cwd.to_path_buf());
    anyhow::ensure!(
        roots.len() == 1 || shared.parent().is_some(),
        "indexed search needs the search paths to share a parent directory; use --no-index"
    );
    Ok(shared)
}

/// Longest path prefix both paths share
fn shared_ancestor(left: &Path, right: &Path) -> PathBuf {
    left.components()
        .zip(right.components())
        .take_while(|(left, right)| left == right)
        .map(|(component, _)| component)
        .collect()
}

pub fn absolute_path(cwd: &Path, path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    absolute
        .components()
        .filter(|component| !matches!(component, std::path::Component::CurDir))
        .collect()
}

/// Resolve `.` and `..` components without touching the filesystem
fn normalize_lexically(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {},
            Component::ParentDir => match normalized.components().next_back() {
                None | Some(Component::ParentDir) => normalized.push(Component::ParentDir),
                Some(Component::RootDir | Component::Prefix(_)) => {},
                Some(_) => {
                    normalized.pop();
                },
            },
            component => normalized.push(component),
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::{IndexRoot, SearchRoots};
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    fn scratch(name: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("eg-roots-{name}-"))
            .tempdir()
            .expect("scratch dir")
    }

    #[test]
    fn absolute_path_drops_current_dir_components() {
        let cwd = Path::new("/repo");
        assert_eq!(
            PathBuf::from("/repo/vendor"),
            super::absolute_path(cwd, Path::new("./vendor"))
        );
        assert_eq!(
            PathBuf::from("/repo"),
            super::absolute_path(cwd, Path::new("."))
        );
        assert_eq!(
            PathBuf::from("/other"),
            super::absolute_path(cwd, Path::new("/other/."))
        );
    }

    #[test]
    fn explicit_directory_is_its_own_build_root() {
        let dir_guard = scratch("dir");
        let dir = dir_guard.path().to_path_buf();
        let roots =
            SearchRoots::from_paths(Path::new("/tmp"), &[dir.clone()], false).expect("roots");

        assert_eq!(roots.build_root().path(), dir.as_path());
        assert!(roots.contains(Path::new("/tmp"), &dir.join("file.rs")));
    }

    #[test]
    fn implicit_cwd_is_the_build_root() {
        let cwd_guard = scratch("implicit");
        let cwd = cwd_guard.path().to_path_buf();
        let roots = SearchRoots::from_paths(&cwd, &[], true).expect("roots");

        assert_eq!(roots.build_root().path(), cwd.as_path());
        assert!(roots.contains(&cwd, &cwd.join("src/lib.rs")));
    }

    #[test]
    fn missing_search_path_is_an_error() {
        let dir_guard = scratch("missing");
        let dir = dir_guard.path().to_path_buf();
        let missing = dir.join("gone");
        let err = SearchRoots::from_paths(Path::new("/tmp"), &[missing], false)
            .err()
            .expect("missing path must fail");

        assert!(err.to_string().contains("does not exist"));
    }

    #[test]
    fn explicit_file_builds_from_parent_and_matches_only_that_file() {
        let dir_guard = scratch("file");
        let dir = dir_guard.path().to_path_buf();
        let file = dir.join("main.rs");
        fs::write(&file, "fn main() {}\n").expect("write file");
        let roots =
            SearchRoots::from_paths(Path::new("/tmp"), &[file.clone()], false).expect("roots");

        assert_eq!(roots.build_root().path(), dir.as_path());
        assert!(roots.contains(Path::new("/tmp"), &file));
        assert!(!roots.contains(Path::new("/tmp"), &dir.join("lib.rs")));
    }

    #[test]
    fn multiple_paths_build_from_their_shared_parent() {
        let cwd_guard = scratch("multi");
        let cwd = cwd_guard.path().to_path_buf();
        let crates = cwd.join("crates");
        fs::create_dir_all(crates.join("eg")).expect("eg dir");
        fs::create_dir_all(crates.join("lib")).expect("lib dir");
        let roots = SearchRoots::from_paths(
            &cwd,
            &[PathBuf::from("crates/eg"), PathBuf::from("crates/lib")],
            false,
        )
        .expect("roots");

        assert_eq!(roots.build_root().path(), crates.as_path());
        assert!(roots.is_served_by(roots.build_root()));
    }

    #[test]
    fn multiple_paths_outside_the_cwd_are_still_served() {
        let cwd_guard = scratch("outside-cwd");
        let elsewhere_guard = scratch("outside-target");
        let cwd = cwd_guard.path().to_path_buf();
        let elsewhere = elsewhere_guard.path().to_path_buf();
        fs::create_dir_all(elsewhere.join("sub")).expect("sub dir");
        let roots =
            SearchRoots::from_paths(&cwd, &[elsewhere.clone(), elsewhere.join("sub")], false)
                .expect("roots");

        assert_eq!(roots.build_root().path(), elsewhere.as_path());
        assert!(roots.is_served_by(roots.build_root()));
    }

    #[test]
    fn paths_without_a_shared_parent_are_an_error() {
        let cwd_guard = scratch("unshared");
        let cwd = cwd_guard.path().to_path_buf();
        let err = SearchRoots::from_paths(&cwd, &[PathBuf::from("/"), cwd.clone()], false)
            .err()
            .expect("unshared paths must fail");

        assert!(err.to_string().contains("share a parent directory"));
    }

    #[test]
    fn parent_index_root_serves_child_search_root() {
        let repo_guard = scratch("parent");
        let repo = repo_guard.path().to_path_buf();
        let src = repo.join("src");
        fs::create_dir_all(&src).expect("src dir");
        let roots = SearchRoots::from_paths(&repo, &[PathBuf::from("src")], false).expect("roots");

        assert!(roots.is_served_by(&IndexRoot::new(repo)));
    }

    #[test]
    fn child_index_root_does_not_serve_parent_search_root() {
        let repo_guard = scratch("child");
        let repo = repo_guard.path().to_path_buf();
        let src = repo.join("src");
        fs::create_dir_all(&src).expect("src dir");
        let roots = SearchRoots::from_paths(&repo, &[PathBuf::from("./")], false).expect("roots");

        assert!(!roots.is_served_by(&IndexRoot::new(src)));
    }

    #[test]
    fn display_path_prefixes_files_with_the_argument_as_typed() {
        let repo_guard = scratch("display");
        let repo = repo_guard.path().to_path_buf();
        fs::create_dir_all(repo.join("src")).expect("src dir");
        let file = repo.join("src/lib.rs");

        for (arg, want) in [
            ("./", "./src/lib.rs"),
            (".", "./src/lib.rs"),
            ("src", "src/lib.rs"),
            ("src/", "src/lib.rs"),
        ] {
            let roots =
                SearchRoots::from_paths(&repo, &[PathBuf::from(arg)], false).expect("roots");
            assert_eq!(roots.display_path(&file), Path::new(want), "arg {arg}");
        }
        let absolute =
            SearchRoots::from_paths(Path::new("/tmp"), &[repo.clone()], false).expect("roots");
        assert_eq!(absolute.display_path(&file), file);
    }

    #[test]
    fn display_path_strips_the_dot_prefix_for_implicit_searches() {
        let repo_guard = scratch("implicit-display");
        let repo = repo_guard.path().to_path_buf();
        let roots = SearchRoots::from_paths(&repo, &[PathBuf::from("./")], true).expect("roots");

        assert_eq!(
            roots.display_path(&repo.join("src/lib.rs")),
            Path::new("src/lib.rs")
        );
    }

    #[test]
    fn display_path_resolves_dot_components_in_the_stored_path() {
        let repo_guard = scratch("dotted");
        let repo = repo_guard.path().to_path_buf();
        fs::create_dir_all(repo.join("sub")).expect("sub dir");
        let dotted = repo.join("./sub/../top.txt");
        let roots = SearchRoots::from_paths(&repo, &[PathBuf::from(".")], false).expect("roots");

        assert_eq!(roots.display_path(&dotted), Path::new("./top.txt"));
    }

    #[test]
    fn display_path_renders_an_explicit_file_argument_as_typed() {
        let dir_guard = scratch("file-display");
        let dir = dir_guard.path().to_path_buf();
        let file = dir.join("main.rs");
        fs::write(&file, "fn main() {}\n").expect("write file");
        let roots =
            SearchRoots::from_paths(&dir, &[PathBuf::from("main.rs")], false).expect("roots");

        assert_eq!(roots.display_path(&file), Path::new("main.rs"));
    }

    #[test]
    fn parent_dir_argument_contains_and_displays_sibling_files() {
        let repo_guard = scratch("parentdir");
        let repo = repo_guard.path().to_path_buf();
        let sub = repo.join("sub");
        fs::create_dir_all(&sub).expect("sub dir");
        let roots = SearchRoots::from_paths(&sub, &[PathBuf::from("../")], false).expect("roots");

        assert!(roots.contains(&sub, &repo.join("top.txt")));
        assert_eq!(
            roots.display_path(&repo.join("top.txt")),
            Path::new("../top.txt")
        );
    }
}
