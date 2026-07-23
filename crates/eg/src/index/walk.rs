//! Corpus walk collection for index snapshot construction.

use std::path::{Path, PathBuf};

use anyhow::bail;

use crate::{flags::HiArgs, haystack::Haystack};

use super::{progress::BuildProgress, roots::absolute_path};

pub struct CollectedHaystacks {
    pub haystacks: Vec<Haystack>,
    pub dirs: Vec<PathBuf>,
}

pub fn collect_haystacks(
    args: &HiArgs,
    corpus_root: &Path,
    index_state_root: &Path,
    progress: Option<&BuildProgress>,
) -> anyhow::Result<CollectedHaystacks> {
    let haystack_builder = args.haystack_builder();
    let cwd = args.cwd().to_path_buf();
    let corpus_root = absolute_path(&cwd, corpus_root);
    let index_state_root = absolute_path(&cwd, index_state_root);
    let mut unsorted = Vec::new();
    let mut dirs = Vec::new();
    let mut entries_done = 0u64;
    for result in args.walk_builder_rooted(&corpus_root)?.build() {
        entries_done += 1;
        let dent = match result {
            Ok(dent) => dent,
            Err(err) => {
                let _ = haystack_builder.build_from_result(Err(err));
                continue;
            },
        };
        let path = absolute_path(&cwd, dent.path());
        if path.starts_with(&index_state_root) {
            continue;
        }
        if dent.file_type().is_some_and(|file_type| file_type.is_dir()) {
            dirs.push(dent.path().to_path_buf());
        }
        let Some(haystack) = haystack_builder.build_from_result(Ok(dent)) else {
            update_walk(progress, entries_done, unsorted.len(), dirs.len());
            continue;
        };
        unsorted.push(haystack);
        update_walk(progress, entries_done, unsorted.len(), dirs.len());
    }
    let mut haystacks = Vec::new();
    for haystack in args.sort(unsorted.into_iter()) {
        if haystack.is_stdin() {
            bail!("indexed search does not support stdin yet; use --no-index");
        }
        haystacks.push(haystack);
    }
    Ok(CollectedHaystacks { haystacks, dirs })
}

fn update_walk(
    progress: Option<&BuildProgress>,
    entries_done: u64,
    files_done: usize,
    dirs_done: usize,
) {
    if let Some(progress) = progress {
        progress.update_walk(entries_done, files_done as u64, dirs_done as u64);
    }
}
