//! Shared file scanning for index backends.

use std::{
    fs::{self, File},
    io::Cursor,
    path::Path,
};

use anyhow::Context;
use memmap2::{Mmap, MmapOptions};
use sngram_types::{ScanError, ScanEvent, ScanSummary, WeightTable};

use super::{
    manifest::CurrentFile,
    summary::{SummaryRecord, SummaryStatus},
};

pub struct IndexedDocument {
    pub ord: u32,
    pub path_hash: u64,
    pub forced_candidate: bool,
    pub hashes: Vec<u64>,
    pub summary: SummaryRecord,
}

impl IndexedDocument {
    pub const fn is_skipped(&self) -> bool {
        matches!(self.summary.status(), SummaryStatus::Skipped)
    }

    pub const fn emitted_grams(&self) -> usize {
        match self.summary.status() {
            SummaryStatus::Known(summary) => summary.gram_count as usize,
            SummaryStatus::Skipped | SummaryStatus::UnknownText => 0,
        }
    }
}

pub fn scan(
    table: &WeightTable,
    file: &CurrentFile,
    use_mmap: bool,
) -> anyhow::Result<IndexedDocument> {
    let ord = u32::try_from(file.ord).context("indexed document ordinal does not fit in u32")?;
    let path_hash = file.path_hash();
    let len = fs::metadata(&file.path)
        .with_context(|| format!("failed to stat {} for indexing", file.path.display()))?
        .len();
    if super::classify::is_oversized(len) {
        return Ok(document(
            ord,
            path_hash,
            true,
            Vec::new(),
            SummaryStatus::UnknownText,
        ));
    }
    let bytes = read_file(&file.path, use_mmap, len)?;
    let bytes = bytes.as_ref();
    if super::classify::is_binary(bytes) {
        return Ok(document(
            ord,
            path_hash,
            false,
            Vec::new(),
            SummaryStatus::Skipped,
        ));
    }
    if super::classify::has_decoding_bom(bytes) {
        return Ok(document(
            ord,
            path_hash,
            true,
            Vec::new(),
            SummaryStatus::UnknownText,
        ));
    }
    let Some((mut hashes, summary)) = scan_bytes(table, bytes)? else {
        return Ok(document(
            ord,
            path_hash,
            false,
            Vec::new(),
            SummaryStatus::Skipped,
        ));
    };
    hashes.sort_unstable();
    hashes.dedup();
    let forced_candidate = super::classify::is_high_entropy(bytes.len(), hashes.len());
    if forced_candidate {
        hashes.clear();
    }
    Ok(document(
        ord,
        path_hash,
        forced_candidate,
        hashes,
        SummaryStatus::Known(summary),
    ))
}

fn scan_bytes(
    table: &WeightTable,
    bytes: &[u8],
) -> anyhow::Result<Option<(Vec<u64>, ScanSummary)>> {
    let mut hashes = Vec::new();
    let mut summary = None;
    let scan = sngram::scan(table, Cursor::new(bytes), |event| match event {
        ScanEvent::Gram(gram) => hashes.push(gram.key.value()),
        ScanEvent::Finish(facts) => summary = Some(*facts),
    });
    if matches!(scan, Err(ScanError::Binary)) {
        return Ok(None);
    }
    scan?;
    let summary = summary.context("scanner finished without emitting a summary")?;
    Ok(Some((hashes, summary)))
}

fn document(
    ord: u32,
    path_hash: u64,
    forced_candidate: bool,
    hashes: Vec<u64>,
    status: SummaryStatus,
) -> IndexedDocument {
    IndexedDocument {
        ord,
        path_hash,
        forced_candidate,
        hashes,
        summary: SummaryRecord::new(ord, status),
    }
}

fn read_file(path: &Path, use_mmap: bool, len: u64) -> anyhow::Result<FileBytes> {
    if use_mmap && len != 0 {
        mmap_file(path).map(FileBytes::Mmap)
    } else {
        fs::read(path)
            .map(FileBytes::Owned)
            .with_context(|| format!("failed to read {} for indexing", path.display()))
    }
}

#[allow(unsafe_code)]
fn mmap_file(path: &Path) -> anyhow::Result<Mmap> {
    let file = File::open(path)
        .with_context(|| format!("failed to open {} for mmap indexing", path.display()))?;
    unsafe { MmapOptions::new().map(&file) }
        .with_context(|| format!("failed to mmap {} for indexing", path.display()))
}

enum FileBytes {
    Mmap(Mmap),
    Owned(Vec<u8>),
}

impl AsRef<[u8]> for FileBytes {
    fn as_ref(&self) -> &[u8] {
        match self {
            Self::Mmap(bytes) => bytes,
            Self::Owned(bytes) => bytes,
        }
    }
}
