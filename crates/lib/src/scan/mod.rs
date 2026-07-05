//! Sparse n-gram extraction for the standard index format.

use std::io::BufRead;

mod cover;
mod engine;
mod facts;
mod settings;
mod validate;

use sngram_types::{ScanError, ScanEvent, WeightTable};

/// Extract sparse n-grams and scan metadata from one byte stream.
///
/// The scanner reads the input once, emits primary and ASCII-folded gram
/// spaces, and brackets the document with virtual line sentinels so anchored
/// patterns can be planned against real boundary grams.
///
/// # Errors
///
/// Returns [`ScanError::Io`] when reading from `input` fails, or
/// [`ScanError::Binary`] when the leading content sample is rejected as binary.
pub fn scan<R>(
    table: &WeightTable,
    input: R,
    mut emit: impl for<'a> FnMut(ScanEvent<'a>),
) -> Result<(), ScanError>
where
    R: BufRead,
{
    let (prefix, mut input) = validate::read(input)?;
    let mut scanner = engine::DocumentScanner::new(table);
    scanner.begin_document(&mut emit);
    scanner.push_content(prefix.bytes(), &mut emit);
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

pub fn minimal_cover(table: &WeightTable, literal: &[u8]) -> Vec<sngram_types::Gram> {
    cover::minimal_cover(table, literal)
}

pub fn guaranteed_cover(table: &WeightTable, literal: &[u8]) -> Vec<sngram_types::Gram> {
    cover::guaranteed_cover(table, literal)
}

pub const fn min_len() -> usize {
    settings::ScanSettings::MIN_LEN
}

pub const fn line_sentinels() -> bool {
    settings::ScanSettings::LINE_SENTINELS
}

pub const fn folded_space() -> bool {
    settings::ScanSettings::FOLDED_SPACE
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use sngram_types::{ScanError, WeightTable};

    use super::scan;

    fn table() -> WeightTable {
        WeightTable::from_weight_fn(|c1, c2| crc32fast::hash(&[c1, c2]))
    }

    #[test]
    fn binary_input_is_rejected_before_any_event() {
        let mut events = 0usize;
        let err = scan(&table(), Cursor::new(b"\x7fELF\x00\x00\x00rest"), |_| {
            events += 1;
        })
        .unwrap_err();

        assert!(matches!(err, ScanError::Binary));
        assert_eq!(events, 0);
    }
}
