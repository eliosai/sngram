use std::path::{Path, PathBuf};

use crate::counter::BigramCounter;

pub fn mint(counter: &BigramCounter, dir: &Path, label: &str) -> anyhow::Result<PathBuf> {
    let path = dir.join(format!("{label}_weights.bin"));
    let bytes = counter.to_table_bytes();
    std::fs::write(&path, &bytes)?;
    println!(
        "MINT [{label}] {} bytes -> {} ({} pairs, {} files)",
        bytes.len(),
        path.display(),
        counter.pairs_processed(),
        counter.files_processed(),
    );
    Ok(path)
}
