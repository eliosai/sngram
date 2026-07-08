//! Canonical Huffman coding for the posting mask column.

/// Longest permitted code, sized so one table lookup decodes any symbol
const MAX_CODE_LEN: u8 = 16;

/// Lists shorter than this store raw mask bytes instead of a bitstream
pub const HUFF_MIN_COUNT: usize = 16;

/// Byte length of the code-length prologue at the head of postings.bin
pub const CODE_TABLE_LEN: usize = 256;

/// Canonical code lengths for all 256 mask symbols
#[derive(Clone)]
pub struct CodeLengths {
    lengths: [u8; 256],
}

impl CodeLengths {
    /// Build length-limited canonical code lengths from symbol frequencies
    pub fn from_frequencies(freq: &[u64; 256]) -> Self {
        let mut scaled = *freq;
        if scaled.iter().filter(|&&count| count > 0).count() <= 1 {
            scaled[0] += 1;
            scaled[1] += 1;
        }
        loop {
            let lengths = huffman_lengths(&scaled);
            if lengths.iter().all(|&len| len <= MAX_CODE_LEN) {
                return Self { lengths };
            }
            for count in &mut scaled {
                *count = *count / 2 + u64::from(*count > 0);
            }
        }
    }

    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        let lengths: [u8; 256] = bytes.get(..CODE_TABLE_LEN)?.try_into().ok()?;
        if lengths.iter().any(|&len| len > MAX_CODE_LEN) {
            return None;
        }
        kraft_complete(&lengths).then_some(Self { lengths })
    }

    pub const fn as_bytes(&self) -> &[u8; 256] {
        &self.lengths
    }

    /// MSB-first canonical codes ordered by (length, symbol)
    fn codes(&self) -> [(u16, u8); 256] {
        let mut codes = [(0u16, 0u8); 256];
        let mut code = 0u32;
        for len in 1..=MAX_CODE_LEN {
            for symbol in 0..256 {
                if self.lengths[symbol] == len {
                    codes[symbol] = (code as u16, len);
                    code += 1;
                }
            }
            code <<= 1;
        }
        codes
    }
}

/// True when the lengths form a complete prefix code
fn kraft_complete(lengths: &[u8; 256]) -> bool {
    let total: u64 = lengths
        .iter()
        .filter(|&&len| len > 0)
        .map(|&len| 1u64 << (MAX_CODE_LEN - len))
        .sum();
    total == 1 << MAX_CODE_LEN
}

/// Plain Huffman code lengths by pairwise merging; zero-frequency symbols
/// get zero length, single-symbol inputs get length one
fn huffman_lengths(freq: &[u64; 256]) -> [u8; 256] {
    let mut lengths = [0u8; 256];
    let mut heap: std::collections::BinaryHeap<std::cmp::Reverse<(u64, Vec<usize>)>> = freq
        .iter()
        .enumerate()
        .filter(|&(_, &count)| count > 0)
        .map(|(symbol, &count)| std::cmp::Reverse((count, vec![symbol])))
        .collect();
    if heap.len() == 1 {
        let std::cmp::Reverse((_, symbols)) = heap.pop().expect("one entry");
        lengths[symbols[0]] = 1;
        return lengths;
    }
    while heap.len() > 1 {
        let std::cmp::Reverse((left_count, left)) = heap.pop().expect("two entries");
        let std::cmp::Reverse((right_count, mut symbols)) = heap.pop().expect("two entries");
        for &symbol in &left {
            lengths[symbol] += 1;
        }
        for &symbol in &symbols {
            lengths[symbol] += 1;
        }
        symbols.extend(left);
        heap.push(std::cmp::Reverse((left_count + right_count, symbols)));
    }
    lengths
}

/// Symbol encoder from code lengths
pub struct Encoder {
    codes: [(u16, u8); 256],
}

impl Encoder {
    pub fn new(lengths: &CodeLengths) -> Self {
        Self {
            codes: lengths.codes(),
        }
    }

    /// Append the bitstream for `masks`, padded to a whole byte
    pub fn encode_into(&self, masks: impl Iterator<Item = u8>, out: &mut Vec<u8>) {
        let mut acc = 0u32;
        let mut bits = 0u8;
        for mask in masks {
            let (code, len) = self.codes[usize::from(mask)];
            acc = (acc << len) | u32::from(code);
            bits += len;
            while bits >= 8 {
                bits -= 8;
                out.push((acc >> bits) as u8);
            }
        }
        if bits > 0 {
            out.push((acc << (8 - bits)) as u8);
        }
    }
}

/// One-lookup decoder: sixteen peeked bits map to a symbol and its length
pub struct Decoder {
    table: Vec<(u8, u8)>,
}

impl Decoder {
    pub fn new(lengths: &CodeLengths) -> Self {
        let mut table = vec![(0u8, 0u8); 1 << MAX_CODE_LEN];
        for (symbol, &(code, len)) in lengths.codes().iter().enumerate() {
            if len == 0 {
                continue;
            }
            let shift = MAX_CODE_LEN - len;
            let base = u32::from(code) << shift;
            for fill in 0..(1u32 << shift) {
                table[(base | fill) as usize] = (symbol as u8, len);
            }
        }
        Self { table }
    }

    /// Decode `count` symbols from a byte-padded bitstream
    pub fn decode(&self, bytes: &[u8], count: usize) -> Option<Vec<u8>> {
        let mut out = Vec::with_capacity(count);
        let mut acc = 0u32;
        let mut bits = 0u8;
        let mut at = 0usize;
        for _ in 0..count {
            while bits < MAX_CODE_LEN && at < bytes.len() {
                acc = (acc << 8) | u32::from(bytes[at]);
                at += 1;
                bits += 8;
            }
            if bits == 0 {
                return None;
            }
            let index = if bits >= MAX_CODE_LEN {
                ((acc >> (bits - MAX_CODE_LEN)) & 0xFFFF) as usize
            } else {
                ((acc << (MAX_CODE_LEN - bits)) & 0xFFFF) as usize
            };
            let (symbol, len) = self.table[index];
            if len == 0 || len > bits {
                return None;
            }
            bits -= len;
            acc &= (1 << bits) - 1;
            out.push(symbol);
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(masks: &[u8]) {
        let mut freq = [0u64; 256];
        for &mask in masks {
            freq[usize::from(mask)] += 1;
        }
        let lengths = CodeLengths::from_frequencies(&freq);
        let encoder = Encoder::new(&lengths);
        let decoder = Decoder::new(&lengths);
        let mut bytes = Vec::new();
        encoder.encode_into(masks.iter().copied(), &mut bytes);

        assert_eq!(decoder.decode(&bytes, masks.len()), Some(masks.to_vec()));
    }

    #[test]
    fn skewed_masks_round_trip_below_byte_parity() {
        let mut masks = vec![0b0010_0001u8; 5000];
        masks.extend(std::iter::repeat_n(0xFF, 300));
        masks.extend((0..=255u8).cycle().take(700));
        round_trip(&masks);

        let mut freq = [0u64; 256];
        for &mask in &masks {
            freq[usize::from(mask)] += 1;
        }
        let lengths = CodeLengths::from_frequencies(&freq);
        let mut bytes = Vec::new();
        Encoder::new(&lengths).encode_into(masks.iter().copied(), &mut bytes);
        assert!(bytes.len() < masks.len());
    }

    #[test]
    fn single_symbol_stream_round_trips() {
        round_trip(&[0x1F; 64]);
    }

    #[test]
    fn uniform_all_symbols_round_trip() {
        let masks: Vec<u8> = (0..=255u8).collect();
        round_trip(&masks);
    }

    #[test]
    fn code_table_round_trips_through_bytes() {
        let mut freq = [1u64; 256];
        freq[0x21] = 1_000_000;
        let lengths = CodeLengths::from_frequencies(&freq);
        let parsed = CodeLengths::from_bytes(lengths.as_bytes()).expect("valid table");
        assert_eq!(parsed.as_bytes(), lengths.as_bytes());
    }

    #[test]
    fn invalid_code_tables_are_rejected() {
        assert!(CodeLengths::from_bytes(&[0u8; 256]).is_none());
        let mut over = [0u8; 256];
        over[0] = MAX_CODE_LEN + 1;
        assert!(CodeLengths::from_bytes(&over).is_none());
        assert!(CodeLengths::from_bytes(&[0u8; 100]).is_none());
    }

    #[test]
    fn truncated_bitstreams_fail_closed() {
        let mut freq = [0u64; 256];
        freq[7] = 10;
        freq[9] = 3;
        let lengths = CodeLengths::from_frequencies(&freq);
        let decoder = Decoder::new(&lengths);
        assert_eq!(decoder.decode(&[], 4), None);
    }
}
