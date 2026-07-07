//! Per-document scan-summary storage for complete query-plan execution.

use std::{
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::Context;
use memmap2::{Mmap, MmapOptions};
use sngram_types::{
    ByteSet256, EdgeBytes, SaturatingByteCounts256, ScanFlags, ScanNeed, ScanSummary,
};

pub const SUMMARY_FILE_NAME: &str = "summaries.bin";

const MAGIC: [u8; 8] = *b"EGSUM1\0\0";
const VERSION: u32 = 2;
const HEADER_SIZE: usize = 32;
const RECORD_SIZE: usize = 400;
const STATUS_SKIPPED: u8 = 0;
const STATUS_UNKNOWN_TEXT: u8 = 1;
const STATUS_KNOWN: u8 = 2;
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SummaryStatus {
    Skipped,
    UnknownText,
    Known(ScanSummary),
}

impl SummaryStatus {
    pub const fn is_text(self) -> bool {
        matches!(self, Self::Known(_) | Self::UnknownText)
    }

    pub fn satisfies(self, need: &ScanNeed) -> bool {
        match self {
            Self::Known(summary) => need.satisfied_by(&summary),
            Self::UnknownText => true,
            Self::Skipped => false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SummaryRecord {
    ord: u32,
    status: SummaryStatus,
}

impl SummaryRecord {
    pub const fn new(ord: u32, status: SummaryStatus) -> Self {
        Self { ord, status }
    }

    pub const fn status(self) -> SummaryStatus {
        self.status
    }

    const fn ord(self) -> u32 {
        self.ord
    }
}

#[derive(Clone)]
pub struct SummaryIndex {
    base: SummarySegment,
    doc_count: usize,
    text_count: usize,
}

impl SummaryIndex {
    pub fn open(base_path: &Path, doc_count: usize) -> anyhow::Result<Option<Self>> {
        Self::open_with(base_path, doc_count, SummaryOpen::Strict)
    }

    pub fn open_trusted(base_path: &Path, doc_count: usize) -> anyhow::Result<Option<Self>> {
        Self::open_with(base_path, doc_count, SummaryOpen::Trusted)
    }

    fn open_with(
        base_path: &Path,
        doc_count: usize,
        mode: SummaryOpen,
    ) -> anyhow::Result<Option<Self>> {
        let Some((base, text_count)) = SummarySegment::open(base_path, doc_count, mode)? else {
            return Ok(None);
        };
        Ok(Some(Self {
            base,
            doc_count,
            text_count,
        }))
    }

    pub fn from_records(records: Vec<SummaryRecord>, doc_count: usize) -> anyhow::Result<Self> {
        let base = SummarySegment::from_records(records)?;
        if !base.covers_base(doc_count) {
            anyhow::bail!(
                "summary ordinals do not cover 0..{doc_count} (records={})",
                base.len()
            );
        }
        let text_count = base.count_text();
        Ok(Self {
            base,
            doc_count,
            text_count,
        })
    }

    pub fn status(&self, ord: usize) -> SummaryStatus {
        let Ok(ord) = u32::try_from(ord) else {
            return SummaryStatus::Skipped;
        };
        self.base
            .dense_status(ord)
            .unwrap_or(SummaryStatus::Skipped)
    }

    pub fn text_ordinals(&self) -> Vec<usize> {
        self.filter_ordinals(SummaryStatus::is_text)
    }

    pub fn text_count(&self) -> usize {
        self.text_count
    }

    pub fn ordinals_satisfying(&self, need: &ScanNeed) -> Vec<usize> {
        self.filter_ordinals(|status| status.satisfies(need))
    }

    pub fn count_satisfying(&self, need: &ScanNeed) -> usize {
        self.count_ordinals(|status| status.satisfies(need))
    }

    fn filter_ordinals(&self, keep: impl Fn(SummaryStatus) -> bool) -> Vec<usize> {
        let mut ords = Vec::new();
        for ord in 0..self.doc_count {
            if keep(self.status(ord)) {
                ords.push(ord);
            }
        }
        ords
    }

    fn count_ordinals(&self, keep: impl Fn(SummaryStatus) -> bool) -> usize {
        (0..self.doc_count)
            .filter(|&ord| keep(self.status(ord)))
            .count()
    }
}

#[derive(Clone, Copy)]
enum SummaryOpen {
    Strict,
    Trusted,
}

#[derive(Clone)]
struct SummarySegment {
    storage: SummaryStorage,
}

#[derive(Clone)]
enum SummaryStorage {
    Mmap(Arc<Mmap>),
    Bytes(Vec<u8>),
}

impl SummarySegment {
    fn open(
        path: &Path,
        doc_count: usize,
        mode: SummaryOpen,
    ) -> anyhow::Result<Option<(Self, usize)>> {
        let file = match File::open(path) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(err).with_context(|| format!("failed to open {}", path.display()));
            },
        };
        let len = file
            .metadata()
            .with_context(|| format!("failed to stat {}", path.display()))?
            .len();
        if len < HEADER_SIZE as u64 {
            log::debug!("eg index: invalid summary file {}", path.display());
            return Ok(None);
        }
        let mmap = mmap_file(&file, path)?;
        let Some(text_count) = validate_open_file(&mmap, doc_count, mode) else {
            log::debug!("eg index: invalid summary file {}", path.display());
            return Ok(None);
        };
        Ok(Some((
            Self {
                storage: SummaryStorage::Mmap(Arc::new(mmap)),
            },
            text_count,
        )))
    }

    fn from_records(mut records: Vec<SummaryRecord>) -> anyhow::Result<Self> {
        sort_records(&mut records)?;
        let mut body = Vec::with_capacity(records.len() * RECORD_SIZE);
        for record in records {
            body.extend_from_slice(&encode_record(record));
        }
        Ok(Self {
            storage: SummaryStorage::Bytes(body),
        })
    }

    fn len(&self) -> usize {
        self.body().len() / RECORD_SIZE
    }

    fn covers_base(&self, doc_count: usize) -> bool {
        body_covers_base(self.body(), doc_count)
    }

    fn count_text(&self) -> usize {
        self.body()
            .chunks_exact(RECORD_SIZE)
            .filter_map(decode_record)
            .filter(|record| record.status().is_text())
            .count()
    }

    fn dense_status(&self, ord: u32) -> Option<SummaryStatus> {
        let idx = usize::try_from(ord).ok()?;
        let record = self.record(idx)?;
        (record.ord() == ord).then_some(record.status)
    }

    fn record(&self, idx: usize) -> Option<SummaryRecord> {
        let start = idx.checked_mul(RECORD_SIZE)?;
        let end = start.checked_add(RECORD_SIZE)?;
        self.body().get(start..end).and_then(decode_record)
    }

    fn body(&self) -> &[u8] {
        match &self.storage {
            SummaryStorage::Mmap(mmap) => mmap.get(HEADER_SIZE..).unwrap_or_default(),
            SummaryStorage::Bytes(body) => body,
        }
    }
}

pub fn write_records(path: &Path, records: &mut Vec<SummaryRecord>) -> anyhow::Result<()> {
    sort_records(records)?;
    let mut body = Vec::with_capacity(records.len() * RECORD_SIZE);
    let text_count = records
        .iter()
        .filter(|record| record.status().is_text())
        .count();
    for &record in records.iter() {
        body.extend_from_slice(&encode_record(record));
    }
    let mut file = header(records.len(), checksum(&body), text_count).to_vec();
    file.extend_from_slice(&body);
    durable_write(path, &file)
}

fn durable_write(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let temp = temp_path(path);
    {
        let mut file =
            File::create(&temp).with_context(|| format!("failed to create {}", temp.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("failed to write {}", temp.display()))?;
        file.sync_all()
            .with_context(|| format!("failed to fsync {}", temp.display()))?;
    }
    fs::rename(&temp, path).with_context(|| {
        format!(
            "failed to install summary file {} from {}",
            path.display(),
            temp.display()
        )
    })?;
    if let Some(parent) = path.parent() {
        fsync_dir(parent)?;
    }
    Ok(())
}

fn temp_path(path: &Path) -> PathBuf {
    path.with_extension("tmp")
}

fn fsync_dir(dir: &Path) -> anyhow::Result<()> {
    File::open(dir)
        .and_then(|file| file.sync_all())
        .with_context(|| format!("failed to fsync directory {}", dir.display()))
}

fn sort_records(records: &mut [SummaryRecord]) -> anyhow::Result<()> {
    records.sort_by_key(|record| record.ord);
    if records.windows(2).any(|pair| pair[0].ord == pair[1].ord) {
        anyhow::bail!("duplicate summary record ordinal");
    }
    Ok(())
}

#[allow(unsafe_code)]
fn mmap_file(file: &File, path: &Path) -> anyhow::Result<Mmap> {
    unsafe { MmapOptions::new().map(file) }
        .with_context(|| format!("failed to mmap {}", path.display()))
}

fn validate_open_file(bytes: &[u8], doc_count: usize, mode: SummaryOpen) -> Option<usize> {
    let Some(body) = open_file_body(bytes) else {
        return None;
    };
    if body.len() / RECORD_SIZE != doc_count {
        return None;
    }
    if matches!(mode, SummaryOpen::Trusted) {
        return Some(usize::try_from(read_u32(bytes.get(..HEADER_SIZE)?, 12)).ok()?);
    }
    if checksum(body) != read_u64(bytes.get(..HEADER_SIZE)?, 24) {
        return None;
    }
    let text_count = body_covers_base_and_count_text(body, doc_count)?;
    (usize::try_from(read_u32(bytes.get(..HEADER_SIZE)?, 12)).ok()? == text_count)
        .then_some(text_count)
}

fn open_file_body(bytes: &[u8]) -> Option<&[u8]> {
    let header = bytes.get(..HEADER_SIZE)?;
    if header.get(..8)? != MAGIC {
        return None;
    }
    if read_u32(header, 8) != VERSION {
        return None;
    }
    let count = usize::try_from(read_u64(header, 16)).ok()?;
    let stored_checksum = read_u64(header, 24);
    let body = bytes.get(HEADER_SIZE..)?;
    if body.len() != count.checked_mul(RECORD_SIZE)? {
        return None;
    }
    if stored_checksum == 0 {
        return None;
    }
    Some(body)
}

fn body_covers_base(body: &[u8], doc_count: usize) -> bool {
    body_covers_base_and_count_text(body, doc_count).is_some()
}

fn body_covers_base_and_count_text(body: &[u8], doc_count: usize) -> Option<usize> {
    if body.len() / RECORD_SIZE != doc_count {
        return None;
    }
    let mut text_count = 0usize;
    for (expected, record) in body.chunks_exact(RECORD_SIZE).enumerate() {
        let Some(record) = decode_record(record) else {
            return None;
        };
        if usize::try_from(record.ord) != Ok(expected) {
            return None;
        }
        if record.status().is_text() {
            text_count += 1;
        }
    }
    Some(text_count)
}

fn header(count: usize, checksum: u64, text_count: usize) -> [u8; HEADER_SIZE] {
    let mut header = [0u8; HEADER_SIZE];
    header[..8].copy_from_slice(&MAGIC);
    header[8..12].copy_from_slice(&VERSION.to_le_bytes());
    header[12..16].copy_from_slice(&u32::try_from(text_count).unwrap_or(u32::MAX).to_le_bytes());
    header[16..24].copy_from_slice(&(count as u64).to_le_bytes());
    header[24..32].copy_from_slice(&checksum.to_le_bytes());
    header
}

fn encode_record(record: SummaryRecord) -> [u8; RECORD_SIZE] {
    let mut out = [0u8; RECORD_SIZE];
    write_u32(&mut out, 0, record.ord);
    match record.status {
        SummaryStatus::Skipped => out[4] = STATUS_SKIPPED,
        SummaryStatus::UnknownText => out[4] = STATUS_UNKNOWN_TEXT,
        SummaryStatus::Known(summary) => {
            out[4] = STATUS_KNOWN;
            write_summary(&mut out, summary);
        },
    }
    out
}

fn decode_record(bytes: &[u8]) -> Option<SummaryRecord> {
    if bytes.len() != RECORD_SIZE {
        return None;
    }
    let ord = read_u32(bytes, 0);
    let status = match *bytes.get(4)? {
        STATUS_SKIPPED => SummaryStatus::Skipped,
        STATUS_UNKNOWN_TEXT => SummaryStatus::UnknownText,
        STATUS_KNOWN => SummaryStatus::Known(read_summary(bytes)?),
        _ => return None,
    };
    Some(SummaryRecord { ord, status })
}

fn write_summary(out: &mut [u8], summary: ScanSummary) {
    write_u64(out, 8, summary.byte_len);
    write_u32(out, 16, summary.line_count);
    write_u32(out, 20, summary.empty_line_count);
    write_u32(out, 24, summary.longest_line_len);
    write_u32(out, 28, summary.gram_count);
    write_u64(out, 32, summary.flags.bits());
    out[40..296].copy_from_slice(&summary.byte_counts.counts);
    write_words(out, 296, summary.line_start_bytes.words);
    write_words(out, 328, summary.line_end_bytes.words);
    write_edge(out, 360, 361, summary.prefix);
    write_edge(out, 377, 378, summary.suffix);
}

fn read_summary(bytes: &[u8]) -> Option<ScanSummary> {
    let mut counts = [0u8; 256];
    counts.copy_from_slice(bytes.get(40..296)?);
    Some(ScanSummary {
        byte_len: read_u64(bytes, 8),
        line_count: read_u32(bytes, 16),
        empty_line_count: read_u32(bytes, 20),
        longest_line_len: read_u32(bytes, 24),
        gram_count: read_u32(bytes, 28),
        flags: ScanFlags(read_u64(bytes, 32)),
        byte_counts: SaturatingByteCounts256 { counts },
        line_start_bytes: ByteSet256 {
            words: read_words(bytes, 296)?,
        },
        line_end_bytes: ByteSet256 {
            words: read_words(bytes, 328)?,
        },
        prefix: read_edge(bytes, 360, 361)?,
        suffix: read_edge(bytes, 377, 378)?,
    })
}

fn write_words(out: &mut [u8], offset: usize, words: [u64; 4]) {
    for (i, word) in words.into_iter().enumerate() {
        write_u64(out, offset + i * 8, word);
    }
}

fn read_words(bytes: &[u8], offset: usize) -> Option<[u64; 4]> {
    Some([
        read_u64(bytes, offset),
        read_u64(bytes, offset + 8),
        read_u64(bytes, offset + 16),
        read_u64(bytes, offset + 24),
    ])
}

fn write_edge(out: &mut [u8], len_offset: usize, bytes_offset: usize, edge: EdgeBytes) {
    let bytes = edge.as_slice();
    out[len_offset] = u8::try_from(bytes.len()).unwrap_or(u8::MAX);
    out[bytes_offset..bytes_offset + bytes.len()].copy_from_slice(bytes);
}

fn read_edge(bytes: &[u8], len_offset: usize, bytes_offset: usize) -> Option<EdgeBytes> {
    let len = usize::from(*bytes.get(len_offset)?);
    if len > EdgeBytes::CAPACITY {
        return None;
    }
    Some(EdgeBytes::from_slice(
        bytes.get(bytes_offset..bytes_offset + len)?,
    ))
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    let mut out = [0u8; 4];
    out.copy_from_slice(&bytes[offset..offset + 4]);
    u32::from_le_bytes(out)
}

fn write_u32(out: &mut [u8], offset: usize, value: u32) {
    out[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    let mut out = [0u8; 8];
    out.copy_from_slice(&bytes[offset..offset + 8]);
    u64::from_le_bytes(out)
}

fn write_u64(out: &mut [u8], offset: usize, value: u64) {
    out[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn checksum(bytes: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET;
    for &byte in bytes {
        hash = (hash ^ u64::from(byte)).wrapping_mul(FNV_PRIME);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_records_round_trip() {
        let summary = ScanSummary {
            byte_len: 3,
            line_count: 1,
            empty_line_count: 0,
            longest_line_len: 3,
            gram_count: 2,
            flags: ScanFlags::default().with_ascii_lower(),
            byte_counts: {
                let mut counts = SaturatingByteCounts256::default();
                counts.observe(b'a');
                counts
            },
            line_start_bytes: {
                let mut set = ByteSet256::default();
                set.insert(b'a');
                set
            },
            line_end_bytes: {
                let mut set = ByteSet256::default();
                set.insert(b'c');
                set
            },
            prefix: EdgeBytes::from_slice(b"abc"),
            suffix: EdgeBytes::from_slice(b"abc"),
        };
        let record = SummaryRecord::new(7, SummaryStatus::Known(summary));

        assert_eq!(decode_record(&encode_record(record)), Some(record));
    }

    #[test]
    fn base_summary_must_cover_dense_ordinals() {
        let records = vec![
            SummaryRecord::new(1, SummaryStatus::UnknownText),
            SummaryRecord::new(2, SummaryStatus::UnknownText),
        ];
        let segment = SummarySegment::from_records(records).unwrap();

        assert_eq!(segment.len(), 2);
        assert!(!segment.covers_base(2));
    }

    #[test]
    fn text_count_excludes_skipped_entries() {
        let records = vec![
            SummaryRecord::new(0, SummaryStatus::Skipped),
            SummaryRecord::new(1, SummaryStatus::UnknownText),
            SummaryRecord::new(2, SummaryStatus::Known(empty_summary())),
        ];
        let index = SummaryIndex::from_records(records, 3).unwrap();

        assert_eq!(index.text_count(), 2);
        assert_eq!(index.text_ordinals(), vec![1, 2]);
    }

    #[test]
    fn open_rejects_corrupted_checksum() {
        let path = scratch("summary-corrupt").join(SUMMARY_FILE_NAME);
        let record = SummaryRecord::new(0, SummaryStatus::Known(empty_summary()));
        let mut bytes = summary_file(&[record]);
        bytes[HEADER_SIZE + 8] ^= 0xFF;
        fs::write(&path, bytes).unwrap();

        assert!(SummaryIndex::open(&path, 1).unwrap().is_none());
    }

    #[test]
    fn open_rejects_non_dense_ordinals() {
        let path = scratch("summary-nondense").join(SUMMARY_FILE_NAME);
        let record = SummaryRecord::new(1, SummaryStatus::Known(empty_summary()));
        fs::write(&path, summary_file(&[record])).unwrap();

        assert!(SummaryIndex::open(&path, 1).unwrap().is_none());
    }

    fn scratch(name: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("eg-summary-{name}-{stamp}"));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn summary_file(records: &[SummaryRecord]) -> Vec<u8> {
        let mut body = Vec::with_capacity(records.len() * RECORD_SIZE);
        let text_count = records
            .iter()
            .filter(|record| record.status().is_text())
            .count();
        for &record in records {
            body.extend_from_slice(&encode_record(record));
        }
        let mut file = header(records.len(), checksum(&body), text_count).to_vec();
        file.extend_from_slice(&body);
        file
    }

    fn empty_summary() -> ScanSummary {
        ScanSummary {
            byte_len: 0,
            line_count: 0,
            empty_line_count: 0,
            longest_line_len: 0,
            gram_count: 0,
            flags: ScanFlags::default(),
            byte_counts: SaturatingByteCounts256::default(),
            line_start_bytes: ByteSet256::default(),
            line_end_bytes: ByteSet256::default(),
            prefix: EdgeBytes::default(),
            suffix: EdgeBytes::default(),
        }
    }
}
