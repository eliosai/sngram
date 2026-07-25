//! Whole-content storage for searchable prefixes the scanner refuses

use std::{fs::File, io::Write, path::Path};

use anyhow::Context;
use memmap2::{Mmap, MmapOptions};

pub const FILE_NAME: &str = "verbatim.bin";

/// Longest prefix kept whole: measured over linux, k8s, django and hass-core, every prefix the scanner refuses is under 1 KiB
pub const MAX_LEN: usize = 1024;

const MAGIC: [u8; 8] = *b"EGVERB1\0";
const VERSION: u32 = 1;
const HEADER_SIZE: usize = 24;
const ENTRY_HEADER_SIZE: usize = 6;
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// A document whose entire searchable content lives in the index
pub struct HeldDocument {
    ord: u32,
    bytes: Vec<u8>,
}

impl HeldDocument {
    /// Hold `bytes` for `ord`, or `None` when they exceed [`MAX_LEN`]
    pub fn new(ord: u32, bytes: &[u8]) -> Option<Self> {
        (bytes.len() <= MAX_LEN).then(|| Self {
            ord,
            bytes: bytes.to_vec(),
        })
    }
}

pub fn write_records(path: &Path, records: &mut Vec<HeldDocument>) -> anyhow::Result<()> {
    records.sort_unstable_by_key(|record| record.ord);
    let mut body = Vec::new();
    for record in records.iter() {
        body.extend_from_slice(&record.ord.to_le_bytes());
        body.extend_from_slice(&(record.bytes.len() as u16).to_le_bytes());
        body.extend_from_slice(&record.bytes);
    }
    let mut out = header(records.len(), checksum(&body)).to_vec();
    out.extend_from_slice(&body);
    let mut file =
        File::create(path).with_context(|| format!("failed to create {}", path.display()))?;
    file.write_all(&out)
        .with_context(|| format!("failed to write {}", path.display()))
}

/// The held documents of one published index
pub struct HeldIndex {
    storage: Mmap,
    count: usize,
}

impl HeldIndex {
    /// Open the store, returning `None` when it is missing or corrupt
    pub fn open(path: &Path) -> anyhow::Result<Option<Self>> {
        let file = match File::open(path) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(err).with_context(|| format!("failed to open {}", path.display()));
            },
        };
        let storage = mmap_file(&file, path)?;
        let Some(count) = validate(&storage) else {
            log::debug!("eg index: invalid verbatim file {}", path.display());
            return Ok(None);
        };
        Ok(Some(Self { storage, count }))
    }

    pub const fn len(&self) -> usize {
        self.count
    }

    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Ordinals whose held content `decide` accepts, in ascending order
    pub fn ordinals_accepted(&self, mut decide: impl FnMut(&[u8]) -> bool) -> Vec<usize> {
        let mut ords = Vec::new();
        let mut rest = self.storage.get(HEADER_SIZE..).unwrap_or_default();
        while let Some((ord, bytes, tail)) = split_entry(rest) {
            if decide(bytes) {
                ords.push(ord as usize);
            }
            rest = tail;
        }
        ords
    }
}

fn split_entry(rest: &[u8]) -> Option<(u32, &[u8], &[u8])> {
    let head = rest.get(..ENTRY_HEADER_SIZE)?;
    let ord = u32::from_le_bytes(head.get(..4)?.try_into().ok()?);
    let len = usize::from(u16::from_le_bytes(head.get(4..6)?.try_into().ok()?));
    let bytes = rest.get(ENTRY_HEADER_SIZE..ENTRY_HEADER_SIZE + len)?;
    Some((ord, bytes, rest.get(ENTRY_HEADER_SIZE + len..)?))
}

/// Entry count when the file parses whole and its body checksum holds
fn validate(bytes: &[u8]) -> Option<usize> {
    let head = bytes.get(..HEADER_SIZE)?;
    if head.get(..8)? != MAGIC || read_u32(head, 8) != VERSION {
        return None;
    }
    let body = bytes.get(HEADER_SIZE..)?;
    if checksum(body) != read_u64(head, 16) {
        return None;
    }
    let count = read_u32(head, 12) as usize;
    let mut seen = 0usize;
    let mut rest = body;
    let mut last = None;
    while let Some((ord, _, tail)) = split_entry(rest) {
        if last.is_some_and(|prev| prev >= ord) {
            return None;
        }
        last = Some(ord);
        seen += 1;
        rest = tail;
    }
    (rest.is_empty() && seen == count).then_some(count)
}

fn header(count: usize, checksum: u64) -> [u8; HEADER_SIZE] {
    let mut header = [0u8; HEADER_SIZE];
    header[..8].copy_from_slice(&MAGIC);
    header[8..12].copy_from_slice(&VERSION.to_le_bytes());
    header[12..16].copy_from_slice(&u32::try_from(count).unwrap_or(u32::MAX).to_le_bytes());
    header[16..24].copy_from_slice(&checksum.to_le_bytes());
    header
}

#[allow(unsafe_code)]
fn mmap_file(file: &File, path: &Path) -> anyhow::Result<Mmap> {
    unsafe { MmapOptions::new().map(file) }
        .with_context(|| format!("failed to mmap {}", path.display()))
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    let mut out = [0u8; 4];
    out.copy_from_slice(&bytes[offset..offset + 4]);
    u32::from_le_bytes(out)
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    let mut out = [0u8; 8];
    out.copy_from_slice(&bytes[offset..offset + 8]);
    u64::from_le_bytes(out)
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
    use std::fs;

    use super::{FILE_NAME, HEADER_SIZE, HeldDocument, HeldIndex, MAX_LEN};

    fn scratch(name: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::Builder::new()
            .prefix(&format!("eg-verbatim-{name}-"))
            .tempdir()
            .expect("scratch dir");
        let path = dir.path().join(FILE_NAME);
        (dir, path)
    }

    fn store(name: &str, records: &[(u32, &[u8])]) -> (tempfile::TempDir, std::path::PathBuf) {
        let (dir, path) = scratch(name);
        let mut held: Vec<HeldDocument> = records
            .iter()
            .map(|&(ord, bytes)| HeldDocument::new(ord, bytes).expect("held"))
            .collect();
        super::write_records(&path, &mut held).expect("write");
        (dir, path)
    }

    #[test]
    fn prefixes_longer_than_the_bound_are_not_held() {
        assert!(HeldDocument::new(0, &vec![b'a'; MAX_LEN]).is_some());
        assert!(HeldDocument::new(0, &vec![b'a'; MAX_LEN + 1]).is_none());
    }

    #[test]
    fn held_content_round_trips_in_ordinal_order() {
        let (_dir, path) = store("round-trip", &[(9, b"\x89PNG\r\n\x1a\n"), (2, b"GIF89a")]);
        let index = HeldIndex::open(&path).expect("open").expect("present");

        assert!(!index.is_empty());
        assert_eq!(index.ordinals_accepted(|_| true), vec![2, 9]);
        assert_eq!(
            index.ordinals_accepted(|bytes| bytes.starts_with(b"GIF")),
            vec![2]
        );
        assert_eq!(index.ordinals_accepted(|_| false), Vec::<usize>::new());
    }

    #[test]
    fn an_empty_store_opens_and_holds_nothing() {
        let (_dir, path) = store("empty", &[]);
        let index = HeldIndex::open(&path).expect("open").expect("present");

        assert!(index.is_empty());
        assert_eq!(index.ordinals_accepted(|_| true), Vec::<usize>::new());
    }

    #[test]
    fn a_missing_store_opens_as_none() {
        let (_dir, path) = scratch("missing");

        assert!(HeldIndex::open(&path).expect("open").is_none());
    }

    #[test]
    fn a_corrupt_body_is_rejected() {
        let (_dir, path) = store("corrupt", &[(1, b"abcd")]);
        let mut bytes = fs::read(&path).expect("read");
        bytes[HEADER_SIZE + 5] ^= 0xFF;
        fs::write(&path, bytes).expect("write");

        assert!(HeldIndex::open(&path).expect("open").is_none());
    }

    #[test]
    fn a_truncated_file_is_rejected() {
        let (_dir, path) = store("truncated", &[(1, b"abcd")]);
        let bytes = fs::read(&path).expect("read");
        fs::write(&path, &bytes[..bytes.len() - 1]).expect("write");

        assert!(HeldIndex::open(&path).expect("open").is_none());
    }
}
