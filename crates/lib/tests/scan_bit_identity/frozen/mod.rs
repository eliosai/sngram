//! Frozen copy of the baseline document scanner used as the identity oracle

mod facts;
mod space;

use std::io::BufRead;

use sngram_types::{Content, HashKey, ScanError, ScanEvent, WeightTable};

use facts::SummaryBuilder;
use space::{EmitPolicy, SpaceScanner, Transform};

const SNIFF_BYTES: usize = 8192;

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

    fn begin_document(&mut self, emit: &mut impl for<'event> FnMut(ScanEvent<'event>)) {
        self.push_sentinel(emit);
    }

    fn push_content(&mut self, chunk: &[u8], emit: &mut impl for<'event> FnMut(ScanEvent<'event>)) {
        if chunk.is_empty() {
            return;
        }

        self.summary.observe(chunk);
        self.content_bytes += chunk.len();
        self.push_to_spaces(chunk, emit);
    }

    fn finish_document(&mut self, emit: &mut impl for<'event> FnMut(ScanEvent<'event>)) {
        self.push_sentinel(emit);
        let summary = self.summary.finish(self.gram_count);
        emit(ScanEvent::Finish(&summary));
    }

    fn push_sentinel(&mut self, emit: &mut impl for<'event> FnMut(ScanEvent<'event>)) {
        self.push_to_spaces(&[ScanSettings::DOCUMENT_SENTINEL], emit);
    }

    fn push_to_spaces(
        &mut self,
        chunk: &[u8],
        emit: &mut impl for<'event> FnMut(ScanEvent<'event>),
    ) {
        let content_bytes = self.content_bytes;
        let gram_count = &mut self.gram_count;
        self.primary.push_bytes(chunk, content_bytes, &mut |gram| {
            *gram_count = gram_count.saturating_add(1);
            emit(ScanEvent::Gram(gram));
        });

        self.folded.push_bytes(chunk, content_bytes, &mut |gram| {
            *gram_count = gram_count.saturating_add(1);
            emit(ScanEvent::Gram(gram));
        });
    }
}

fn read_validated<R>(mut input: R) -> Result<(Vec<u8>, R), ScanError>
where
    R: BufRead,
{
    let mut bytes = Vec::new();

    while bytes.len() < SNIFF_BYTES {
        let chunk = input.fill_buf()?;
        if chunk.is_empty() {
            break;
        }

        let take = chunk.len().min(SNIFF_BYTES - bytes.len());
        bytes.extend_from_slice(&chunk[..take]);
        input.consume(take);
    }

    let content = Content::new(&bytes);
    if content.has_binary_signature() || content.is_likely_binary() {
        return Err(ScanError::Binary);
    }
    Ok((bytes, input))
}

/// Frozen copy of the public scan entry point
pub fn scan<R>(
    table: &WeightTable,
    input: R,
    mut emit: impl for<'event> FnMut(ScanEvent<'event>),
) -> Result<(), ScanError>
where
    R: BufRead,
{
    let (prefix, mut input) = read_validated(input)?;
    let mut scanner = DocumentScanner::new(table);
    scanner.begin_document(&mut emit);
    scanner.push_content(&prefix, &mut emit);
    loop {
        let chunk = input.fill_buf()?;
        if chunk.is_empty() {
            break;
        }
        let len = chunk.len();
        scanner.push_content(chunk, &mut emit);
        input.consume(len);
    }
    scanner.finish_document(&mut emit);
    Ok(())
}
