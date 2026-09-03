//! Frozen copy of the baseline document scanner used as the identity oracle

mod facts;
mod hashing;
mod space;

use sngram::{ScanSummary, ScannedGram, WeightTable};

use facts::SummaryBuilder;
use hashing::HashKey;
use space::{EmitPolicy, SpaceScanner, Transform};

/// Copy of the baseline scanner format settings
pub struct ScanSettings;

impl ScanSettings {
    pub const MIN_GRAM_LEN: usize = 3;
    pub const MAX_GRAM_LEN: usize = 16;
    pub const STACK_CAP: usize = 128;
    pub const PREFIX_RING: usize = 128;
    pub const PREFIX_RING_MASK: usize = Self::PREFIX_RING - 1;
    pub const WINDOW_CAP: usize = 1024;
    pub const WINDOW_KEEP: usize = 128;
    pub const DOCUMENT_SENTINEL: u8 = b'\n';

    pub const fn emits_len(len: usize) -> bool {
        len >= Self::MIN_GRAM_LEN && len <= Self::MAX_GRAM_LEN
    }
}

struct DocumentScanner<'t> {
    primary: SpaceScanner<'t>,
    folded: SpaceScanner<'t>,
    summary: SummaryBuilder,
    content_bytes: usize,
    gram_count: u32,
}

impl<'t> DocumentScanner<'t> {
    fn new(table: &'t WeightTable) -> Self {
        Self {
            primary: SpaceScanner::new(table, HashKey::UNKEYED, Transform::Raw, EmitPolicy::All),
            folded: SpaceScanner::new(
                table,
                HashKey::UNKEYED.folded(),
                Transform::Folded,
                EmitPolicy::ChangedOnly,
            ),
            summary: SummaryBuilder::default(),
            content_bytes: 0,
            gram_count: 0,
        }
    }

    fn begin_document(&mut self, emit: &mut impl FnMut(ScannedGram)) {
        self.push_sentinel(emit);
    }

    fn push_content(&mut self, chunk: &[u8], emit: &mut impl FnMut(ScannedGram)) {
        if chunk.is_empty() {
            return;
        }

        self.summary.observe(chunk);
        self.content_bytes += chunk.len();
        self.push_to_spaces(chunk, emit);
    }

    fn finish_document(&mut self, emit: &mut impl FnMut(ScannedGram)) -> ScanSummary {
        self.push_sentinel(emit);
        self.summary.finish(self.gram_count)
    }

    fn push_sentinel(&mut self, emit: &mut impl FnMut(ScannedGram)) {
        self.push_to_spaces(&[ScanSettings::DOCUMENT_SENTINEL], emit);
    }

    fn push_to_spaces(&mut self, chunk: &[u8], emit: &mut impl FnMut(ScannedGram)) {
        let content_bytes = self.content_bytes;
        let gram_count = &mut self.gram_count;
        self.primary.push_bytes(chunk, content_bytes, &mut |gram| {
            *gram_count = gram_count.saturating_add(1);
            emit(gram);
        });

        self.folded.push_bytes(chunk, content_bytes, &mut |gram| {
            *gram_count = gram_count.saturating_add(1);
            emit(gram);
        });
    }
}

/// Frozen copy of the scan entry point
pub fn scan(table: &WeightTable, content: &[u8], mut emit: impl FnMut(ScannedGram)) -> ScanSummary {
    let mut scanner = DocumentScanner::new(table);
    scanner.begin_document(&mut emit);
    scanner.push_content(content, &mut emit);
    scanner.finish_document(&mut emit)
}
