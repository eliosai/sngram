//! Shared file scanning for index backends.

use std::{
    fs::{self, File},
    io::Cursor,
    path::Path,
};

use anyhow::Context;
use memmap2::{Mmap, MmapOptions};
use sngram_types::{ScanError, ScanEvent, ScanSummary, WeightTable};

use super::executor::{BLOCK_BITS, WORD_END_BIT, WORD_START_BIT};

use super::{
    manifest::CurrentFile,
    summary::{SummaryRecord, SummaryStatus},
};

pub struct IndexedDocument {
    pub ord: u32,
    pub path_hash: u64,
    pub forced_candidate: bool,
    pub hashes: Vec<(u64, u8)>,
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
    let Some((hashes, summary)) = scan_bytes(table, bytes)? else {
        return Ok(document(
            ord,
            path_hash,
            false,
            Vec::new(),
            SummaryStatus::Skipped,
        ));
    };
    let mut hashes = hashes;
    hashes.sort_unstable();
    hashes.dedup_by(|next, kept| {
        if next.0 == kept.0 {
            kept.1 |= next.1;
            true
        } else {
            false
        }
    });
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
) -> anyhow::Result<Option<(Vec<(u64, u8)>, ScanSummary)>> {
    let blocks = BlockMap::new(bytes);
    let mut hashes = Vec::new();
    let mut summary = None;
    let scan = sngram::scan(table, Cursor::new(bytes), |event| match event {
        ScanEvent::Gram(gram) => {
            hashes.push((gram.key.value(), blocks.mask(bytes, &gram.span)));
        },
        ScanEvent::Finish(facts) => summary = Some(*facts),
    });
    if matches!(scan, Err(ScanError::Binary)) {
        return Ok(None);
    }
    scan?;
    let summary = summary.context("scanner finished without emitting a summary")?;
    Ok(Some((hashes, summary)))
}

/// Maps content spans to six scaled line-block bits plus two word-edge bits
struct BlockMap {
    newlines: Vec<usize>,
    line_count: usize,
}

const BLOCK_COUNT: usize = 6;

impl BlockMap {
    fn new(bytes: &[u8]) -> Self {
        let newlines: Vec<usize> = memchr::memchr_iter(b'\n', bytes).collect();
        let trailing = bytes.last().is_some_and(|&byte| byte != b'\n');
        let line_count = (newlines.len() + usize::from(trailing)).max(1);
        Self {
            newlines,
            line_count,
        }
    }

    fn mask(&self, bytes: &[u8], span: &sngram_types::ByteRange) -> u8 {
        let first = self.block_of(self.line_of(span.start));
        let last = self.block_of(self.line_of(span.end.saturating_sub(1).max(span.start)));
        let mut mask = 0u8;
        for block in first..=last {
            mask |= 1 << block;
        }
        if mask == 0 {
            mask = BLOCK_BITS;
        }
        mask | word_edges(bytes, span)
    }

    fn line_of(&self, offset: usize) -> usize {
        self.newlines.partition_point(|&newline| newline < offset)
    }

    fn block_of(&self, line: usize) -> u8 {
        let block = line.min(self.line_count - 1) * BLOCK_COUNT / self.line_count;
        u8::try_from(block.min(BLOCK_COUNT - 1)).unwrap_or(5)
    }
}

/// Word-edge bits for one occurrence: set when a non-word byte or the text
/// edge borders the span
fn word_edges(bytes: &[u8], span: &sngram_types::ByteRange) -> u8 {
    let before = span
        .start
        .checked_sub(1)
        .and_then(|at| bytes.get(at))
        .is_none_or(|&byte| !is_word_byte(byte));
    let after = bytes.get(span.end).is_none_or(|&byte| !is_word_byte(byte));
    u8::from(before) * WORD_START_BIT | u8::from(after) * WORD_END_BIT
}

const fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn document(
    ord: u32,
    path_hash: u64,
    forced_candidate: bool,
    hashes: Vec<(u64, u8)>,
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

#[cfg(test)]
mod tests {
    use super::{BLOCK_BITS, BlockMap, WORD_END_BIT, WORD_START_BIT, word_edges};
    use sngram_types::ByteRange;

    #[test]
    fn six_line_doc_maps_lines_to_distinct_blocks() {
        let map = BlockMap::new(b"a\nb\nc\nd\ne\nf\n");
        assert_eq!(
            map.mask(b"a\nb\nc\nd\ne\nf\n", &ByteRange::new(0, 1)) & BLOCK_BITS,
            0b00_0001
        );
        assert_eq!(
            map.mask(b"a\nb\nc\nd\ne\nf\n", &ByteRange::new(10, 11)) & BLOCK_BITS,
            0b10_0000
        );
    }

    #[test]
    fn newline_spanning_gram_sets_both_blocks() {
        let map = BlockMap::new(b"a\nb\nc\nd\ne\nf\n");
        assert_eq!(
            map.mask(b"a\nb\nc\nd\ne\nf\n", &ByteRange::new(0, 3)) & BLOCK_BITS,
            0b00_0011
        );
    }

    #[test]
    fn long_doc_scales_lines_across_blocks() {
        let content = b"x\n".repeat(60);
        let map = BlockMap::new(&content);
        assert_eq!(
            map.mask(&content, &ByteRange::new(0, 1)) & BLOCK_BITS,
            0b00_0001
        );
        assert_eq!(
            map.mask(&content, &ByteRange::new(118, 119)) & BLOCK_BITS,
            0b10_0000
        );
    }

    #[test]
    fn word_edges_reflect_neighbor_bytes() {
        let text = b"remains main x";
        assert_eq!(word_edges(text, &ByteRange::new(2, 6)), 0);
        assert_eq!(
            word_edges(text, &ByteRange::new(8, 12)),
            WORD_START_BIT | WORD_END_BIT
        );
        assert_eq!(word_edges(text, &ByteRange::new(0, 6)), WORD_START_BIT);
        assert_eq!(
            word_edges(text, &ByteRange::new(13, 14)),
            WORD_START_BIT | WORD_END_BIT
        );
    }
}
