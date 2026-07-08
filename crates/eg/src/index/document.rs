//! Shared file scanning for index backends.

use std::{
    fs::{self, File},
    io::Cursor,
    path::Path,
};

use anyhow::Context;
use memmap2::{Mmap, MmapOptions};
use sngram_types::{ScanError, ScanEvent, ScanSummary, WeightTable};

use super::executor::{BLOCK_BITS, WORD_BOTH_BIT, WORD_END_BIT, WORD_START_BIT};

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

/// Maps content spans to five hashed line-bucket bits plus three word-edge bits
struct BlockMap {
    newlines: Vec<usize>,
}

const BUCKET_COUNT: usize = 5;

impl BlockMap {
    fn new(bytes: &[u8]) -> Self {
        Self {
            newlines: memchr::memchr_iter(b'\n', bytes).collect(),
        }
    }

    fn mask(&self, bytes: &[u8], span: &sngram_types::ByteRange) -> u8 {
        let first = self.line_of(span.start);
        let last = self.line_of(span.end.saturating_sub(1).max(span.start));
        let mut mask = 0u8;
        if last - first >= BUCKET_COUNT {
            mask = BLOCK_BITS;
        } else {
            for line in first..=last {
                mask |= 1 << bucket_of(line);
            }
        }
        mask | word_edges(bytes, span)
    }

    fn line_of(&self, offset: usize) -> usize {
        self.newlines.partition_point(|&newline| newline < offset)
    }
}

/// Hash a line index into a bucket so collisions stay file-size independent
fn bucket_of(line: usize) -> u8 {
    let mixed = (line as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 32;
    (mixed % BUCKET_COUNT as u64) as u8
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
    u8::from(before) * WORD_START_BIT
        | u8::from(after) * WORD_END_BIT
        | u8::from(before && after) * WORD_BOTH_BIT
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
    use super::{
        BLOCK_BITS, BlockMap, WORD_BOTH_BIT, WORD_END_BIT, WORD_START_BIT, bucket_of, word_edges,
    };
    use sngram_types::ByteRange;

    #[test]
    fn same_line_grams_share_one_bucket_bit() {
        let text = b"alpha beta\ngamma\n";
        let map = BlockMap::new(text);
        let first = map.mask(text, &ByteRange::new(0, 5)) & BLOCK_BITS;
        let second = map.mask(text, &ByteRange::new(6, 10)) & BLOCK_BITS;
        assert_eq!(first, second);
        assert_eq!(first.count_ones(), 1);
        assert_eq!(first, 1 << bucket_of(0));
    }

    #[test]
    fn bucket_is_independent_of_file_length() {
        let short = b"x\n".repeat(6);
        let long = b"x\n".repeat(60_000);
        let span = ByteRange::new(8, 9);
        assert_eq!(
            BlockMap::new(&short).mask(&short, &span) & BLOCK_BITS,
            BlockMap::new(&long).mask(&long, &span) & BLOCK_BITS,
        );
    }

    #[test]
    fn newline_spanning_gram_sets_both_line_buckets() {
        let text = b"a\nb\nc\nd\ne\nf\n";
        let map = BlockMap::new(text);
        let mask = map.mask(text, &ByteRange::new(0, 3)) & BLOCK_BITS;
        assert_eq!(mask, 1 << bucket_of(0) | 1 << bucket_of(1));
    }

    #[test]
    fn word_edges_reflect_neighbor_bytes() {
        let text = b"remains main x";
        assert_eq!(word_edges(text, &ByteRange::new(2, 6)), 0);
        assert_eq!(
            word_edges(text, &ByteRange::new(8, 12)),
            WORD_START_BIT | WORD_END_BIT | WORD_BOTH_BIT
        );
        assert_eq!(word_edges(text, &ByteRange::new(0, 6)), WORD_START_BIT);
        assert_eq!(
            word_edges(text, &ByteRange::new(13, 14)),
            WORD_START_BIT | WORD_END_BIT | WORD_BOTH_BIT
        );
    }

    #[test]
    fn split_edge_occurrences_do_not_set_both_bit() {
        let text = b"main? remain";
        assert_eq!(word_edges(text, &ByteRange::new(8, 12)) & WORD_BOTH_BIT, 0);
        assert_eq!(
            word_edges(text, &ByteRange::new(0, 4)),
            WORD_START_BIT | WORD_END_BIT | WORD_BOTH_BIT
        );
    }
}
