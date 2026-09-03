//! Sparse n-gram extraction for the standard index format

pub mod binary;
pub mod cover;
mod engine;
mod facts;
pub mod flags;
pub mod output;
pub mod settings;
mod space;

use crate::{ScanSummary, ScannedGram, WeightTable};

/// Extract the sparse grams and the summary of one document, bracketed by virtual line sentinels
pub fn scan(table: &WeightTable, content: &[u8], mut emit: impl FnMut(ScannedGram)) -> ScanSummary {
    let mut scanner = engine::DocumentScanner::new(table);
    scanner.begin_document(&mut emit);
    scanner.push_content(content, &mut emit);
    scanner.finish_document(&mut emit)
}

#[cfg(test)]
mod tests {
    use super::scan;
    use crate::WeightTable;

    fn table() -> WeightTable {
        WeightTable::from_weight_fn(|c1, c2| crc32fast::hash(&[c1, c2]))
    }

    #[test]
    fn scan_applies_no_binary_policy() {
        let mut grams = 0usize;
        let summary = scan(&table(), b"text\0tail", |_| grams += 1);

        assert_eq!(summary.byte_len, 9);
        assert_eq!(usize::try_from(summary.gram_count).unwrap(), grams);
    }

    #[test]
    fn an_empty_document_scans_to_an_empty_summary() {
        let summary = scan(&table(), b"", |gram| panic!("unexpected gram {gram:?}"));

        assert_eq!((summary.byte_len, summary.line_count), (0, 0));
    }
}
