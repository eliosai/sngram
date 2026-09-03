//! Streaming scan-summary collection.

use crate::{ByteSet256, EdgeBytes, SaturatingByteCounts256, ScanFlags, ScanSummary};

const EDGE: usize = EdgeBytes::CAPACITY;

// per-byte class flag bits indexed by byte value
static CLASS_FLAGS: [u64; 256] = class_flags_table();

const fn class_flags_table() -> [u64; 256] {
    let mut table = [0u64; 256];
    let mut byte = 0u8;
    loop {
        table[byte as usize] = byte_class_flags(byte);
        if byte == u8::MAX {
            break;
        }
        byte += 1;
    }
    table
}

const fn byte_class_flags(byte: u8) -> u64 {
    let mut flags = ScanFlags(0);
    if byte.is_ascii_uppercase() {
        flags = flags.with_ascii_upper();
    }
    if byte.is_ascii_lowercase() {
        flags = flags.with_ascii_lower();
    }
    if byte.is_ascii_digit() {
        flags = flags.with_ascii_digit();
    }
    if byte.is_ascii_whitespace() {
        flags = flags.with_ascii_space();
    }
    if byte.is_ascii_alphanumeric() || byte == b'_' {
        flags = flags.with_ascii_word();
    }
    if !byte.is_ascii() {
        flags = flags.with_non_ascii();
    }
    flags.bits()
}

fn next_newline(chunk: &[u8], from: usize) -> Option<usize> {
    chunk[from..]
        .iter()
        .position(|&byte| byte == b'\n')
        .map(|offset| from + offset)
}

#[derive(Debug)]
pub struct SummaryBuilder {
    byte_len: u64,
    line_breaks: u32,
    empty_line_count: u32,
    longest_line_len: u32,
    current_line_len: u32,
    current_line_has_bytes: bool,
    at_line_start: bool,
    flags: ScanFlags,
    byte_counts: SaturatingByteCounts256,
    line_start_bytes: ByteSet256,
    line_end_bytes: ByteSet256,
    prefix: EdgeBytes,
    suffix: [u8; EDGE],
    suffix_len: usize,
    suffix_pos: usize,
    last: Option<u8>,
}

struct LineSummary {
    count: u32,
    empty_count: u32,
    longest_len: u32,
}

impl Default for SummaryBuilder {
    fn default() -> Self {
        Self {
            byte_len: 0,
            line_breaks: 0,
            empty_line_count: 0,
            longest_line_len: 0,
            current_line_len: 0,
            current_line_has_bytes: false,
            at_line_start: true,
            flags: ScanFlags::default(),
            byte_counts: SaturatingByteCounts256::default(),
            line_start_bytes: ByteSet256::default(),
            line_end_bytes: ByteSet256::default(),
            prefix: EdgeBytes::default(),
            suffix: [0; EDGE],
            suffix_len: 0,
            suffix_pos: 0,
            last: None,
        }
    }
}

impl SummaryBuilder {
    pub fn observe(&mut self, chunk: &[u8]) {
        self.record_prefix(chunk);
        self.record_counts_and_classes(chunk);
        self.record_lines(chunk);
        self.record_suffix(chunk);
        self.byte_len = self.byte_len.saturating_add(chunk.len() as u64);
        if let Some(&last) = chunk.last() {
            self.last = Some(last);
        }
    }

    pub fn finish(&self, gram_count: u32) -> ScanSummary {
        let flags = self.finished_flags();
        let lines = self.line_summary();
        let line_end_bytes = self.line_end_bytes();

        ScanSummary {
            byte_len: self.byte_len,
            line_count: lines.count,
            empty_line_count: lines.empty_count,
            longest_line_len: lines.longest_len,
            gram_count,
            flags,
            byte_counts: self.byte_counts,
            line_start_bytes: self.line_start_bytes,
            line_end_bytes,
            prefix: self.prefix,
            suffix: self.suffix(),
        }
    }

    fn finished_flags(&self) -> ScanFlags {
        if self.last == Some(b'\n') {
            self.flags.with_ends_with_lf()
        } else {
            self.flags
        }
    }

    fn line_summary(&self) -> LineSummary {
        if self.byte_len == 0 {
            return LineSummary {
                count: 0,
                empty_count: 0,
                longest_len: 0,
            };
        }
        let mut lines = LineSummary {
            count: self.line_breaks,
            empty_count: self.empty_line_count,
            longest_len: self.longest_line_len.max(self.current_line_len),
        };
        if self.last != Some(b'\n') {
            lines.count = lines.count.saturating_add(1);
            if !self.current_line_has_bytes {
                lines.empty_count = lines.empty_count.saturating_add(1);
            }
        }
        lines
    }

    fn line_end_bytes(&self) -> ByteSet256 {
        let mut bytes = self.line_end_bytes;
        if self.last != Some(b'\n')
            && self.current_line_has_bytes
            && let Some(end) = self.last
        {
            bytes.insert(end);
        }
        bytes
    }

    fn record_prefix(&mut self, chunk: &[u8]) {
        let room = EDGE - self.prefix.len();
        for &byte in chunk.iter().take(room) {
            self.prefix.push(byte);
        }
    }

    fn record_counts_and_classes(&mut self, chunk: &[u8]) {
        let mut class = 0u64;
        for &byte in chunk {
            let slot = &mut self.byte_counts.counts[usize::from(byte)];
            *slot = slot.saturating_add(1);
            class |= CLASS_FLAGS[usize::from(byte)];
        }
        self.flags = self.flags.with_bits(class);
    }

    fn record_lines(&mut self, chunk: &[u8]) {
        let mut from = 0usize;
        while let Some(newline) = next_newline(chunk, from) {
            self.open_line(&chunk[from..newline]);
            let before = if newline == 0 {
                self.last
            } else {
                Some(chunk[newline - 1])
            };
            self.flags = self.flags.with_lf();
            if before == Some(b'\r') {
                self.flags = self.flags.with_crlf();
            }
            self.close_line(before);
            from = newline + 1;
        }
        self.open_line(&chunk[from..]);
    }

    fn open_line(&mut self, segment: &[u8]) {
        let Some(&first) = segment.first() else {
            return;
        };
        if self.at_line_start {
            self.line_start_bytes.insert(first);
            self.at_line_start = false;
        }
        let added = u32::try_from(segment.len()).unwrap_or(u32::MAX);
        self.current_line_len = self.current_line_len.saturating_add(added);
        self.current_line_has_bytes = true;
    }

    fn close_line(&mut self, before: Option<u8>) {
        self.line_breaks = self.line_breaks.saturating_add(1);
        self.longest_line_len = self.longest_line_len.max(self.current_line_len);
        if self.current_line_len == 0 {
            self.empty_line_count = self.empty_line_count.saturating_add(1);
        }
        if self.current_line_has_bytes
            && let Some(end) = before
        {
            self.line_end_bytes.insert(end);
        }
        self.current_line_len = 0;
        self.current_line_has_bytes = false;
        self.at_line_start = true;
    }

    fn record_suffix(&mut self, chunk: &[u8]) {
        if chunk.len() >= EDGE {
            self.suffix.copy_from_slice(&chunk[chunk.len() - EDGE..]);
            self.suffix_pos = 0;
            self.suffix_len = EDGE;
            return;
        }
        for &byte in chunk {
            self.suffix[self.suffix_pos] = byte;
            self.suffix_pos = (self.suffix_pos + 1) % EDGE;
        }
        self.suffix_len = (self.suffix_len + chunk.len()).min(EDGE);
    }

    fn suffix(&self) -> EdgeBytes {
        if self.suffix_len == 0 {
            return EdgeBytes::default();
        }
        let mut bytes = [0; EDGE];
        let start = if self.suffix_len == EDGE {
            self.suffix_pos
        } else {
            0
        };
        for (i, byte) in bytes.iter_mut().enumerate().take(self.suffix_len) {
            *byte = self.suffix[(start + i) % EDGE];
        }
        EdgeBytes::from_slice(&bytes[..self.suffix_len])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observes_summary_facts() {
        let mut facts = SummaryBuilder::default();
        facts.observe(b"Ab\r\n");
        facts.observe("é\n".as_bytes());
        let summary = facts.finish(9);

        assert_eq!(summary.byte_len, 7);
        assert_eq!(summary.line_count, 2);
        assert_eq!(summary.gram_count, 9);
        assert!(summary.flags.has_lf());
        assert!(summary.flags.has_crlf());
        assert!(summary.flags.has_ascii_upper());
        assert!(summary.flags.has_ascii_lower());
        assert!(summary.flags.has_non_ascii());
        assert!(summary.flags.ends_with_lf());
        assert!(summary.line_start_bytes.contains_any(byte_set(b'A')));
        assert!(summary.line_end_bytes.contains_any(byte_set(b'\r')));
    }

    #[test]
    fn suffix_keeps_last_edge_bytes() {
        let mut facts = SummaryBuilder::default();
        facts.observe(b"0123456789abcdefghijklmnop");
        let summary = facts.finish(0);

        assert_eq!(summary.prefix.as_slice(), b"0123456789abcdef");
        assert_eq!(summary.suffix.as_slice(), b"abcdefghijklmnop");
    }

    #[test]
    fn suffix_wraps_across_small_chunks() {
        let mut facts = SummaryBuilder::default();
        for chunk in b"0123456789abcdefghijklmnop".chunks(3) {
            facts.observe(chunk);
        }
        let summary = facts.finish(0);

        assert_eq!(summary.suffix.as_slice(), b"abcdefghijklmnop");
    }

    #[test]
    fn crlf_split_across_chunks_is_detected() {
        let mut facts = SummaryBuilder::default();
        facts.observe(b"line\r");
        facts.observe(b"\nnext");
        let summary = facts.finish(0);

        assert!(summary.flags.has_crlf());
        assert!(summary.line_end_bytes.contains_any(byte_set(b'\r')));
    }

    #[test]
    fn eof_line_records_its_last_byte() {
        let mut facts = SummaryBuilder::default();
        facts.observe(b"a\nbc");
        let summary = facts.finish(0);

        assert_eq!(summary.line_count, 2);
        assert!(summary.line_end_bytes.contains_any(byte_set(b'a')));
        assert!(summary.line_end_bytes.contains_any(byte_set(b'c')));
    }

    #[test]
    fn empty_lines_do_not_reuse_previous_newline_as_line_end() {
        let mut facts = SummaryBuilder::default();
        facts.observe(b"\n\n");
        let summary = facts.finish(0);

        assert_eq!(summary.line_count, 2);
        assert_eq!(summary.empty_line_count, 2);
        assert!(summary.line_end_bytes.is_empty());
    }

    #[test]
    fn line_spanning_chunks_counts_full_length() {
        let mut facts = SummaryBuilder::default();
        facts.observe(b"abc");
        facts.observe(b"defg\nx");
        let summary = facts.finish(0);

        assert_eq!(summary.longest_line_len, 7);
        assert!(summary.line_start_bytes.contains_any(byte_set(b'a')));
        assert!(summary.line_start_bytes.contains_any(byte_set(b'x')));
        assert!(summary.line_end_bytes.contains_any(byte_set(b'g')));
    }

    fn byte_set(byte: u8) -> ByteSet256 {
        let mut set = ByteSet256::default();
        set.insert(byte);
        set
    }
}
