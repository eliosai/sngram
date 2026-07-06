//! Corpus walk collection for index snapshot construction.

use std::path::{Path, PathBuf};

use anyhow::bail;

use crate::{flags::HiArgs, haystack::Haystack};

use super::roots::absolute_path;

pub struct CollectedHaystacks {
    pub haystacks: Vec<Haystack>,
    pub dirs: Vec<PathBuf>,
}

pub fn collect_haystacks(
    args: &HiArgs,
    index_state_root: &Path,
) -> anyhow::Result<CollectedHaystacks> {
    let haystack_builder = args.haystack_builder();
    let cwd = args.cwd().to_path_buf();
    let index_state_root = absolute_path(&cwd, index_state_root);
    let mut unsorted = Vec::new();
    let mut dirs = Vec::new();
    for result in args.walk_builder()?.build() {
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
            continue;
        };
        unsorted.push(haystack);
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
