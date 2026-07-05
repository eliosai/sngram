//! Streaming document-fact collection.

use sngram_types::ScanFacts;

#[derive(Debug, Default)]
pub struct FactBuilder {
    facts: ScanFacts,
    last: Option<u8>,
}

impl FactBuilder {
    pub fn observe(&mut self, chunk: &[u8]) {
        for &byte in chunk {
            if self.last == Some(b'\r') && byte == b'\n' {
                self.facts = self.facts.with_crlf();
            }
            self.observe_byte(byte);
            self.last = Some(byte);
        }
    }

    const fn observe_byte(&mut self, byte: u8) {
        if byte == b'\n' {
            self.facts = self.facts.with_lf();
        }
        if byte.is_ascii_uppercase() {
            self.facts = self.facts.with_upper_ascii();
        }
        if byte.is_ascii_lowercase() {
            self.facts = self.facts.with_lower_ascii();
        }
        if !byte.is_ascii() {
            self.facts = self.facts.with_non_ascii();
        }
    }

    pub fn finish(&self) -> ScanFacts {
        let mut facts = self.facts;
        if self.last == Some(b'\n') {
            facts = facts.with_ends_with_newline();
        }
        facts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observes_basic_facts() {
        let mut facts = FactBuilder::default();
        facts.observe(b"Abc");
        facts.observe("é\n".as_bytes());
        let facts = facts.finish();

        assert!(facts.has_lf());
        assert!(facts.has_upper_ascii());
        assert!(facts.has_lower_ascii());
        assert!(facts.has_non_ascii());
        assert!(facts.ends_with_newline());
    }

    #[test]
    fn observes_crlf_across_chunks() {
        let mut facts = FactBuilder::default();
        facts.observe(b"abc\r");
        facts.observe(b"\ndef");

        assert!(facts.finish().has_crlf());
    }
}
