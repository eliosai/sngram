//! Streaming input validation before scan events are emitted.

use std::io::BufRead;

use sngram_types::{Content, ScanError};

const SNIFF_BYTES: usize = 8192;

#[derive(Debug, Default)]
pub struct ValidatedPrefix {
    bytes: Vec<u8>,
}

impl ValidatedPrefix {
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

pub fn read<R>(mut input: R) -> Result<(ValidatedPrefix, R), ScanError>
where
    R: BufRead,
{
    let mut bytes = Vec::new();

    while bytes.len() < SNIFF_BYTES {
        let chunk = input.fill_buf()?;
        if chunk.is_empty() {
            break;
        }

        let take = chunk.len().min(SNIFF_BYTES - bytes.len());
        bytes.extend_from_slice(&chunk[..take]);
        input.consume(take);
    }

    let prefix = ValidatedPrefix { bytes };
    validate(prefix.bytes())?;
    Ok((prefix, input))
}

fn validate(prefix: &[u8]) -> Result<(), ScanError> {
    let content = Content::new(prefix);
    if content.has_binary_signature() || content.is_likely_binary() {
        return Err(ScanError::Binary);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::{self, Cursor, Read};

    use super::*;

    #[test]
    fn accepts_text_prefix() {
        let (prefix, mut rest) = read(Cursor::new(b"fn main() {}\n")).expect("valid text");

        assert_eq!(prefix.bytes(), b"fn main() {}\n");
        assert!(rest.fill_buf().expect("read rest").is_empty());
    }

    #[test]
    fn rejects_known_binary_signature() {
        let err = read(Cursor::new(b"\x7fELF\x00\x00\x00rest")).unwrap_err();

        assert!(matches!(err, ScanError::Binary));
    }

    #[test]
    fn rejects_dense_control_byte_prefix() {
        let data = vec![0; 100];
        let err = read(Cursor::new(data)).unwrap_err();

        assert!(matches!(err, ScanError::Binary));
    }

    #[test]
    fn leaves_remaining_stream_after_sniff_cap() {
        let data = vec![b'a'; SNIFF_BYTES + 7];
        let (prefix, mut rest) = read(Cursor::new(data)).expect("valid text");

        assert_eq!(prefix.bytes().len(), SNIFF_BYTES);
        assert_eq!(rest.fill_buf().expect("read rest").len(), 7);
    }

    #[derive(Debug)]
    struct FailingReader;

    impl Read for FailingReader {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("boom"))
        }
    }

    impl BufRead for FailingReader {
        fn fill_buf(&mut self) -> io::Result<&[u8]> {
            Err(io::Error::other("boom"))
        }

        fn consume(&mut self, _amt: usize) {}
    }

    #[test]
    fn returns_io_errors_before_validation_finishes() {
        let err = read(FailingReader).unwrap_err();

        assert!(matches!(err, ScanError::Io(_)));
    }
}
