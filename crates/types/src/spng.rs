//! `SPNG` weight-table binary format.

use crate::error::TableError;

/// Format constants for the `SPNG` table binary.
pub struct WeightTableSettings;

impl WeightTableSettings {
    pub const HEADER_SIZE: usize = 16;
    pub const WEIGHTS_COUNT: usize = 65_536;
    pub const MAGIC: &[u8; 4] = b"SPNG";
    pub const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    pub const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    /// Total v1 table binary size; v2 adds a provenance block after the weights.
    pub const TABLE_BINARY_SIZE: usize = Self::HEADER_SIZE + Self::WEIGHTS_COUNT * 4;

    /// Longest accepted provenance block, in bytes.
    pub const PROVENANCE_MAX: usize = 1024;
}

/// Parsed fields of one `SPNG` table binary.
pub struct TableParts<'a> {
    pub version: u32,
    pub expected_crc: u32,
    pub body: &'a [u8],
    pub data: &'a [u8],
    pub provenance: Option<String>,
}

impl<'a> TableParts<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self, TableError> {
        if bytes.len() < WeightTableSettings::HEADER_SIZE {
            return Err(TableError::Truncated(bytes.len()));
        }
        if bytes.get(..4) != Some(WeightTableSettings::MAGIC.as_slice()) {
            return Err(TableError::InvalidMagic);
        }
        let version = read_u32_le(bytes, 4)?;
        let expected_crc = read_u32_le(bytes, 8)?;
        let body = bytes
            .get(WeightTableSettings::HEADER_SIZE..)
            .ok_or(TableError::Truncated(bytes.len()))?;
        let provenance = version_provenance(version, bytes.len(), body)?;
        let data = body
            .get(..WeightTableSettings::WEIGHTS_COUNT * 4)
            .ok_or(TableError::Truncated(bytes.len()))?;
        Ok(Self {
            version,
            expected_crc,
            body,
            data,
            provenance,
        })
    }
}

/// The body must hash to the header's stored checksum.
pub fn verify_checksum(expected: u32, body: &[u8]) -> Result<(), TableError> {
    let actual = crc32fast::hash(body);
    if expected == actual {
        Ok(())
    } else {
        Err(TableError::Checksum { expected, actual })
    }
}

pub fn fingerprint_bytes(bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .fold(WeightTableSettings::FNV_OFFSET, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(WeightTableSettings::FNV_PRIME)
        })
}

/// Version gate: v1 is the exact legacy size, v2 carries a provenance tail.
fn version_provenance(
    version: u32,
    total_len: usize,
    body: &[u8],
) -> Result<Option<String>, TableError> {
    match version {
        1 => {
            if total_len != WeightTableSettings::TABLE_BINARY_SIZE {
                return Err(TableError::InvalidSize(total_len));
            }
            Ok(None)
        },
        2 => Ok(Some(parse_provenance(body)?)),
        other => Err(TableError::InvalidVersion(other)),
    }
}

/// The v2 body is the weights, a u16 LE length, then that many UTF-8 bytes.
fn parse_provenance(body: &[u8]) -> Result<String, TableError> {
    let tail = body
        .get(WeightTableSettings::WEIGHTS_COUNT * 4..)
        .ok_or(TableError::Truncated(body.len()))?;
    let len_bytes: [u8; 2] = tail
        .get(..2)
        .and_then(|s| s.try_into().ok())
        .ok_or(TableError::Truncated(tail.len()))?;
    let len = usize::from(u16::from_le_bytes(len_bytes));
    if len > WeightTableSettings::PROVENANCE_MAX {
        return Err(TableError::InvalidProvenance);
    }
    let text = tail.get(2..2 + len).ok_or(TableError::InvalidProvenance)?;
    if tail.len() != 2 + len {
        return Err(TableError::InvalidProvenance);
    }
    String::from_utf8(text.to_vec()).map_err(|_| TableError::InvalidProvenance)
}

fn read_u32_le(bytes: &[u8], offset: usize) -> Result<u32, TableError> {
    bytes
        .get(offset..offset + 4)
        .and_then(|s| s.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or(TableError::Truncated(bytes.len()))
}

#[allow(
    clippy::expect_used,
    reason = "provenance is validated to be smaller than u16::MAX"
)]
pub fn provenance_len(provenance: &str) -> u16 {
    u16::try_from(provenance.len()).expect("valid provenance length")
}

#[allow(
    clippy::expect_used,
    reason = "the vec has exactly WEIGHTS_COUNT elements; conversion cannot fail"
)]
fn zero_weights() -> Box<[u32; WeightTableSettings::WEIGHTS_COUNT]> {
    vec![0u32; WeightTableSettings::WEIGHTS_COUNT]
        .into_boxed_slice()
        .try_into()
        .expect("WEIGHTS_COUNT elements")
}

pub fn build_weights(
    weight: &mut impl FnMut(u8, u8) -> u32,
) -> Box<[u32; WeightTableSettings::WEIGHTS_COUNT]> {
    let mut weights = zero_weights();
    for (i, w) in weights.iter_mut().enumerate() {
        let [_, c1, c2] = low_pair_bytes(i);
        *w = weight(c1, c2);
    }
    weights
}

pub fn write_weights(weights: &[u32; WeightTableSettings::WEIGHTS_COUNT], buf: &mut [u8]) {
    let data = &mut buf[WeightTableSettings::HEADER_SIZE..];
    for (i, w) in weights.iter().enumerate() {
        let off = i * 4;
        data[off..off + 4].copy_from_slice(&w.to_le_bytes());
    }
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "each value is masked to one byte before casting"
)]
const fn low_pair_bytes(index: usize) -> [u8; 3] {
    [
        ((index >> 16) & 0xFF) as u8,
        ((index >> 8) & 0xFF) as u8,
        (index & 0xFF) as u8,
    ]
}

pub fn parse_weights(data: &[u8]) -> Box<[u32; WeightTableSettings::WEIGHTS_COUNT]> {
    parse_weights_for_endian(data)
}

#[cfg(target_endian = "little")]
#[allow(
    unsafe_code,
    reason = "the table payload is little-endian u32 data copied into an aligned u32 array"
)]
fn parse_weights_for_endian(data: &[u8]) -> Box<[u32; WeightTableSettings::WEIGHTS_COUNT]> {
    let mut weights = Box::<[u32; WeightTableSettings::WEIGHTS_COUNT]>::new_uninit();
    unsafe {
        std::ptr::copy_nonoverlapping(
            data.as_ptr(),
            weights.as_mut_ptr().cast::<u8>(),
            WeightTableSettings::WEIGHTS_COUNT * 4,
        );
        weights.assume_init()
    }
}

#[cfg(not(target_endian = "little"))]
fn parse_weights_for_endian(data: &[u8]) -> Box<[u32; WeightTableSettings::WEIGHTS_COUNT]> {
    let mut weights = zero_weights();
    for (i, w) in weights.iter_mut().enumerate() {
        let off = i * 4;
        *w = u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]);
    }
    weights
}
