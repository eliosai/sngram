//! Sparse n-gram extraction for the standard index format.

use std::io::BufRead;

pub mod cover;
mod engine;
mod facts;
mod settings;
mod validate;

use sngram_types::{ScanError, ScanEvent, WeightTable};

pub use settings::ScanSettings;

/// Extract sparse n-grams and scan metadata from one byte stream.
///
/// The scanner reads the input once, emits raw gram keys plus case-folded
/// supplement keys when needed, and brackets the document with virtual line
/// sentinels so anchored patterns can be planned against boundary grams.
///
/// # Errors
///
/// Returns [`ScanError::Io`] when reading from `input` fails, or
/// [`ScanError::Binary`] when the leading content sample is rejected as binary.
pub fn scan<R>(
    table: &WeightTable,
    input: R,
    mut emit: impl for<'event> FnMut(ScanEvent<'event>),
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
