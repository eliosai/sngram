//! Canonical Huffman coding for the posting mask column.

/// Longest permitted code, sized so one table lookup decodes any symbol
const MAX_CODE_LEN: u8 = 16;

/// Lists shorter than this store raw mask bytes instead of a bitstream
pub const HUFF_MIN_COUNT: usize = 16;

/// Distinct mask values: five line buckets plus five edge bits
pub const MASK_SYMBOLS: usize = 1 << 10;

/// Byte length of the code-length prologue at the head of postings.bin
pub const CODE_TABLE_LEN: usize = MASK_SYMBOLS;

/// Canonical code lengths for every mask symbol
#[derive(Clone)]
pub struct CodeLengths {
    lengths: [u8; MASK_SYMBOLS],
}

impl CodeLengths {
    /// Build length-limited canonical code lengths from symbol frequencies
    pub fn from_frequencies(freq: &[u64; MASK_SYMBOLS]) -> Self {
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
        let lengths: [u8; MASK_SYMBOLS] = bytes.get(..CODE_TABLE_LEN)?.try_into().ok()?;
        if lengths.iter().any(|&len| len > MAX_CODE_LEN) {
            return None;
        }
        kraft_complete(&lengths).then_some(Self { lengths })
    }

    pub const fn as_bytes(&self) -> &[u8; MASK_SYMBOLS] {
        &self.lengths
    }

    /// MSB-first canonical codes ordered by (length, symbol)
    fn codes(&self) -> [(u16, u8); MASK_SYMBOLS] {
        let mut codes = [(0u16, 0u8); MASK_SYMBOLS];
        let mut code = 0u32;
        for len in 1..=MAX_CODE_LEN {
            for symbol in 0..MASK_SYMBOLS {
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
fn kraft_complete(lengths: &[u8; MASK_SYMBOLS]) -> bool {
    let total: u64 = lengths
        .iter()
        .filter(|&&len| len > 0)
        .map(|&len| 1u64 << (MAX_CODE_LEN - len))
        .sum();
    total == 1 << MAX_CODE_LEN
}

/// Plain Huffman code lengths by pairwise merging; zero-frequency symbols
/// get zero length, single-symbol inputs get length one
fn huffman_lengths(freq: &[u64; MASK_SYMBOLS]) -> [u8; MASK_SYMBOLS] {
    let mut lengths = [0u8; MASK_SYMBOLS];
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
    codes: [(u16, u8); MASK_SYMBOLS],
}

impl Encoder {
    pub fn new(lengths: &CodeLengths) -> Self {
        Self {
            codes: lengths.codes(),
        }
    }

    /// Append the bitstream for `masks`, padded to a whole byte
    pub fn encode_into(&self, masks: impl Iterator<Item = u16>, out: &mut Vec<u8>) {
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

/// Slots in the decode table, one per peeked sixteen-bit window
const TABLE_SLOTS: usize = 1 << MAX_CODE_LEN;

/// Low bits of a packed table entry holding the code length
const LEN_BITS: u16 = 5;

/// Mask selecting the code length out of a packed table entry
const LEN_MASK: u16 = (1 << LEN_BITS) - 1;

/// Bytes a peek needs beyond the current byte to hold sixteen shifted bits
const PEEK_BYTES: usize = 4;

/// One-lookup decoder: sixteen peeked bits map to a packed symbol and length
pub struct Decoder {
    table: Box<[u16; TABLE_SLOTS]>,
}

impl Decoder {
    pub fn new(lengths: &CodeLengths) -> Self {
        let mut table = vec![0u16; TABLE_SLOTS].into_boxed_slice();
        for (symbol, &(code, len)) in lengths.codes().iter().enumerate() {
            if len == 0 {
                continue;
            }
            let base = usize::from(code) << (MAX_CODE_LEN - len);
            let span = 1usize << (MAX_CODE_LEN - len);
            let entry = (symbol as u16) << LEN_BITS | u16::from(len);
            table[base..base + span].fill(entry);
        }
        Self {
            table: table.try_into().expect("table sized by construction"),
        }
    }

    /// The packed entry a sixteen-bit window selects
    fn entry(&self, window: u16) -> u16 {
        self.table[usize::from(window)]
    }

    /// Decode `count` symbols from a byte-padded bitstream into `emit`;
    /// false means the stream ran out or carried an unknown code
    pub fn decode_each(&self, bytes: &[u8], count: usize, mut emit: impl FnMut(u16)) -> bool {
        let mut bit = 0usize;
        let mut done = 0usize;
        while done < count {
            let Some(window) = bytes
                .get(bit >> 3..)
                .and_then(<[u8]>::first_chunk::<PEEK_BYTES>)
            else {
                break;
            };
            let word = u32::from_be_bytes(*window);
            let entry = self.entry((word >> (MAX_CODE_LEN - (bit & 7) as u8)) as u16);
            let len = usize::from(entry & LEN_MASK);
            if len == 0 {
                return false;
            }
            bit += len;
            emit(entry >> LEN_BITS);
            done += 1;
        }
        self.decode_tail(bytes, count - done, bit, &mut emit)
    }

    /// Finish symbols whose peek window would run past the end of the stream
    fn decode_tail(
        &self,
        bytes: &[u8],
        count: usize,
        mut bit: usize,
        emit: &mut impl FnMut(u16),
    ) -> bool {
        let total = bytes.len().saturating_mul(8);
        for _ in 0..count {
            let mut word = 0u32;
            for offset in 0..PEEK_BYTES {
                let byte = bytes.get((bit >> 3) + offset).copied().unwrap_or(0);
                word = (word << 8) | u32::from(byte);
            }
            let entry = self.entry((word >> (MAX_CODE_LEN - (bit & 7) as u8)) as u16);
            let len = usize::from(entry & LEN_MASK);
            if len == 0 || bit + len > total {
                return false;
            }
            bit += len;
            emit(entry >> LEN_BITS);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Collect `count` decoded symbols, or `None` when the stream fails
    fn decode(decoder: &Decoder, bytes: &[u8], count: usize) -> Option<Vec<u16>> {
        let mut out = Vec::with_capacity(count);
        decoder
            .decode_each(bytes, count, |symbol| out.push(symbol))
            .then_some(out)
    }

    fn round_trip(masks: &[u16]) {
        let mut freq = [0u64; MASK_SYMBOLS];
        for &mask in masks {
            freq[usize::from(mask)] += 1;
        }
        let lengths = CodeLengths::from_frequencies(&freq);
        let encoder = Encoder::new(&lengths);
        let decoder = Decoder::new(&lengths);
        let mut bytes = Vec::new();
        encoder.encode_into(masks.iter().copied(), &mut bytes);

        assert_eq!(decode(&decoder, &bytes, masks.len()), Some(masks.to_vec()));
    }

    #[test]
    fn skewed_masks_round_trip_below_byte_parity() {
        let mut masks = vec![0b0010_0001u16; 5000];
        masks.extend(std::iter::repeat_n(0x3FF, 300));
        masks.extend((0..MASK_SYMBOLS as u16).cycle().take(700));
        round_trip(&masks);

        let mut freq = [0u64; MASK_SYMBOLS];
        for &mask in &masks {
            freq[usize::from(mask)] += 1;
        }
        let lengths = CodeLengths::from_frequencies(&freq);
        let mut bytes = Vec::new();
        Encoder::new(&lengths).encode_into(masks.iter().copied(), &mut bytes);
        assert!(bytes.len() < masks.len() * 2);
    }

    #[test]
    fn single_symbol_stream_round_trips() {
        round_trip(&[0x1F; 64]);
    }

    /// Every stream length crosses the windowed loop into the tail differently
    #[test]
    fn every_stream_length_round_trips_across_the_tail() {
        let pattern = [0x21u16, 0x21, 0x3FF, 0x21, 0x1F, 0x21, 0x200, 0x21];
        for len in 0..=64usize {
            let masks: Vec<u16> = pattern.iter().copied().cycle().take(len).collect();
            round_trip(&masks);
        }
    }

    /// A stream cut short of its promised symbols must not invent any
    #[test]
    fn truncated_streams_fail_instead_of_padding() {
        let masks = vec![0x21u16, 0x3FF, 0x1F, 0x200, 0x21, 0x21, 0x3FF, 0x21];
        let mut freq = [0u64; MASK_SYMBOLS];
        for &mask in &masks {
            freq[usize::from(mask)] += 1;
        }
        let lengths = CodeLengths::from_frequencies(&freq);
        let decoder = Decoder::new(&lengths);
        let mut bytes = Vec::new();
        Encoder::new(&lengths).encode_into(masks.iter().copied(), &mut bytes);
        for cut in 0..bytes.len() {
            assert_eq!(decode(&decoder, &bytes[..cut], masks.len()), None);
        }
    }

    #[test]
    fn uniform_all_symbols_round_trip() {
        let masks: Vec<u16> = (0..MASK_SYMBOLS as u16).collect();
        round_trip(&masks);
    }

    #[test]
    fn code_table_round_trips_through_bytes() {
        let mut freq = [1u64; MASK_SYMBOLS];
        freq[0x21] = 1_000_000;
        let lengths = CodeLengths::from_frequencies(&freq);
        let parsed = CodeLengths::from_bytes(lengths.as_bytes()).expect("valid table");
        assert_eq!(parsed.as_bytes(), lengths.as_bytes());
    }

    #[test]
    fn invalid_code_tables_are_rejected() {
        assert!(CodeLengths::from_bytes(&[0u8; MASK_SYMBOLS]).is_none());
        let mut over = [0u8; MASK_SYMBOLS];
        over[0] = MAX_CODE_LEN + 1;
        assert!(CodeLengths::from_bytes(&over).is_none());
        assert!(CodeLengths::from_bytes(&[0u8; 100]).is_none());
    }

    #[test]
    fn truncated_bitstreams_fail_closed() {
        let mut freq = [0u64; MASK_SYMBOLS];
        freq[7] = 10;
        freq[9] = 3;
        let lengths = CodeLengths::from_frequencies(&freq);
        let decoder = Decoder::new(&lengths);
        assert_eq!(decode(&decoder, &[], 4), None);
    }
}
