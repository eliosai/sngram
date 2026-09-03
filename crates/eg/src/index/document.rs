//! Shared file scanning for index backends.

use std::{
    fs::{self, File},
    io::Cursor,
    path::Path,
};

use anyhow::Context;
use memmap2::{Mmap, MmapOptions};
use sngram::{ScanError, ScanEvent, ScanSummary, WeightTable};

use super::executor::{
    BLOCK_BITS, LINE_END_BIT, LINE_START_BIT, WORD_BOTH_BIT, WORD_END_BIT, WORD_START_BIT,
};

use super::{
    grams::{PackedGram, collapse},
    manifest::CurrentFile,
    summary::{SummaryRecord, SummaryStatus},
    verbatim::HeldDocument,
};

pub struct IndexedDocument {
    pub ord: u32,
    pub path_hash: u64,
    pub forced_candidate: bool,
    pub held: Option<HeldDocument>,
    pub hashes: Vec<PackedGram>,
    pub summary: SummaryRecord,
}

impl IndexedDocument {
    pub const fn is_skipped(&self) -> bool {
        matches!(self.summary.status(), SummaryStatus::Skipped)
    }

    /// Decide this document from stored bytes instead of forcing it everywhere
    fn hold(&mut self, prefix: &[u8]) {
        let Some(held) = HeldDocument::new(self.ord, prefix) else {
            return;
        };
        self.held = Some(held);
        self.forced_candidate = false;
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
    if crate::nulquit::has_decoding_bom(bytes) {
        return Ok(document(
            ord,
            path_hash,
            true,
            Vec::new(),
            SummaryStatus::UnknownText,
        ));
    }
    let bytes = crate::nulquit::without_utf8_bom(bytes);
    let prefix = super::classify::searchable_prefix(bytes);
    if file.is_explicit() && prefix.len() < bytes.len() {
        return Ok(document(
            ord,
            path_hash,
            true,
            Vec::new(),
            SummaryStatus::UnknownText,
        ));
    }
    if prefix.is_empty() {
        return Ok(document(
            ord,
            path_hash,
            false,
            Vec::new(),
            SummaryStatus::Skipped,
        ));
    }
    let Some((hashes, summary)) = scan_bytes(table, prefix)? else {
        let mut refused = document(ord, path_hash, true, Vec::new(), SummaryStatus::UnknownText);
        refused.hold(prefix);
        return Ok(refused);
    };
    let mut hashes = hashes;
    collapse(&mut hashes);
    let forced_candidate = super::classify::is_high_entropy(prefix.len(), hashes.len());
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
) -> anyhow::Result<Option<(Vec<PackedGram>, ScanSummary)>> {
    let mut blocks = BlockMap::new(bytes);
    let mut hashes = Vec::with_capacity(bytes.len().min(MAX_GRAM_PREALLOC));
    let mut summary = None;
    let scan = sngram::scan(table, Cursor::new(bytes), |event| match event {
        ScanEvent::Gram(gram) => {
            let mask = blocks.mask(bytes, &gram.span);
            hashes.push(PackedGram::new(gram.key.value(), mask));
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

/// Maps content spans to five hashed line-bucket bits plus the line and
/// word edge bits
struct BlockMap {
    newlines: Vec<usize>,
    cursor: LineCursor,
}

/// Last resolved span end and its line index
#[derive(Clone, Copy, Default)]
struct LineCursor {
    offset: usize,
    line: usize,
}

const BUCKET_COUNT: usize = 5;

/// Cap on the emitted-gram preallocation for one file
const MAX_GRAM_PREALLOC: usize = 8 * 1024 * 1024;

impl BlockMap {
    fn new(bytes: &[u8]) -> Self {
        Self {
            newlines: memchr::memchr_iter(b'\n', bytes).collect(),
            cursor: LineCursor::default(),
        }
    }

    fn mask(&mut self, bytes: &[u8], span: &sngram::ByteRange) -> u16 {
        let last = self.line_of_end(span.end.saturating_sub(1).max(span.start));
        let first = self.line_of_start(last, span.start);
        let mut mask = 0u16;
        if last - first >= BUCKET_COUNT {
            mask = BLOCK_BITS;
        } else {
            for line in first..=last {
                mask |= 1 << bucket_of(line);
            }
        }
        mask | edge_bits(bytes, span)
    }

    /// Line index of a span end, which the scanner walks forward
    fn line_of_end(&mut self, offset: usize) -> usize {
        if offset < self.cursor.offset {
            self.cursor.line = self.newlines.partition_point(|&newline| newline < offset);
        } else {
            while let Some(&newline) = self.newlines.get(self.cursor.line) {
                if newline >= offset {
                    break;
                }
                self.cursor.line += 1;
            }
        }
        self.cursor.offset = offset;
        self.cursor.line
    }

    /// Line index of a span start, walking back over the span's own newlines
    fn line_of_start(&self, last: usize, start: usize) -> usize {
        let mut first = last;
        while first > 0 && self.newlines[first - 1] >= start {
            first -= 1;
        }
        first
    }
}

/// Hash a line index into a bucket so collisions stay file-size independent
fn bucket_of(line: usize) -> u32 {
    let mixed = (line as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 32;
    (mixed % BUCKET_COUNT as u64) as u32
}

/// Word and line edge bits for one occurrence, read from the two bytes
/// bracketing the span; `\r\n` counts as a line end so a CRLF verifier never
/// loses a line its anchors still match
fn edge_bits(bytes: &[u8], span: &sngram::ByteRange) -> u16 {
    let before = span
        .start
        .checked_sub(1)
        .and_then(|at| bytes.get(at).copied());
    let after = bytes.get(span.end).copied();
    let word_start = before.is_none_or(|byte| !is_word_byte(byte));
    let word_end = after.is_none_or(|byte| !is_word_byte(byte));
    let line_start = before.is_none_or(|byte| byte == b'\n');
    let line_end = match after {
        None | Some(b'\n') => true,
        Some(b'\r') => bytes.get(span.end + 1).is_none_or(|&byte| byte == b'\n'),
        Some(_) => false,
    };
    u16::from(word_start) * WORD_START_BIT
        | u16::from(word_end) * WORD_END_BIT
        | u16::from(word_start && word_end) * WORD_BOTH_BIT
        | u16::from(line_start) * LINE_START_BIT
        | u16::from(line_end) * LINE_END_BIT
}

const fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn document(
    ord: u32,
    path_hash: u64,
    forced_candidate: bool,
    hashes: Vec<PackedGram>,
    status: SummaryStatus,
) -> IndexedDocument {
    IndexedDocument {
        ord,
        path_hash,
        forced_candidate,
        held: None,
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
        BLOCK_BITS, BlockMap, LINE_END_BIT, LINE_START_BIT, WORD_BOTH_BIT, WORD_END_BIT,
        WORD_START_BIT, bucket_of, edge_bits,
    };
    use sngram::ByteRange;

    const WORD_BITS: u16 = WORD_START_BIT | WORD_END_BIT | WORD_BOTH_BIT;
    const LINE_BITS: u16 = LINE_START_BIT | LINE_END_BIT;

    fn word_edges(bytes: &[u8], span: &ByteRange) -> u16 {
        edge_bits(bytes, span) & WORD_BITS
    }

    fn line_edges(bytes: &[u8], span: &ByteRange) -> u16 {
        edge_bits(bytes, span) & LINE_BITS
    }

    #[test]
    fn same_line_grams_share_one_bucket_bit() {
        let text = b"alpha beta\ngamma\n";
        let mut map = BlockMap::new(text);
        let first = map.mask(text, &ByteRange::new(0, 5)) & BLOCK_BITS;
        let second = map.mask(text, &ByteRange::new(6, 10)) & BLOCK_BITS;
        assert_eq!(first, second);
        assert_eq!(first.count_ones(), 1);
        assert_eq!(first, 1 << bucket_of(0));
    }

    #[test]
    fn cursor_line_lookup_matches_binary_search_in_any_order() {
        let text = b"aa\nbb\ncc\ndd\nee\nff\ngg\nhh\n";
        let mut map = BlockMap::new(text);
        let fresh = BlockMap::new(text);
        let offsets = [0, 5, 3, 23, 11, 0, 22, 7, 7, 1, 23];
        for &offset in &offsets {
            let expected = fresh.newlines.partition_point(|&newline| newline < offset);
            assert_eq!(map.line_of_end(offset), expected, "offset {offset}");
        }
    }

    #[test]
    fn span_start_line_walks_back_over_the_spanned_newlines() {
        let text = b"aa\nbb\ncc\ndd\nee\n";
        let map = BlockMap::new(text);
        for start in 0..text.len() {
            for end in start..text.len() {
                let last = map.newlines.partition_point(|&newline| newline < end);
                let expected = map.newlines.partition_point(|&newline| newline < start);
                assert_eq!(map.line_of_start(last, start), expected, "{start}..{end}");
            }
        }
    }

    #[test]
    fn masks_are_stable_across_query_order() {
        let text: Vec<u8> = (0..64)
            .flat_map(|i| format!("line number {i} with words\n").into_bytes())
            .collect();
        let spans = [(3, 9), (30, 41), (700, 712), (100, 130), (5, 6)];
        let mut ordered = BlockMap::new(&text);
        for &(start, end) in &spans {
            let mut fresh = BlockMap::new(&text);
            let span = ByteRange::new(start, end);
            assert_eq!(ordered.mask(&text, &span), fresh.mask(&text, &span));
        }
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
        let mut map = BlockMap::new(text);
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
    fn line_edges_reflect_terminators_and_text_edges() {
        let both = LINE_START_BIT | LINE_END_BIT;
        let lines = b"alpha\nbeta gamma\ndelta";
        let cases: [(&[u8], usize, usize, u16); 7] = [
            (lines, 0, 5, both),
            (lines, 6, 10, LINE_START_BIT),
            (lines, 11, 16, LINE_END_BIT),
            (lines, 7, 9, 0),
            (lines, 17, 22, both),
            (b"value\r\nnext", 0, 5, both),
            (b"a\rb", 0, 1, LINE_START_BIT),
        ];
        for (text, start, end, want) in cases {
            let span = ByteRange::new(start, end);
            assert_eq!(line_edges(text, &span), want, "{start}..{end}");
        }
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
