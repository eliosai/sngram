/*!
Deterministic NUL handling for quit-mode binary detection.

The grep searcher's quit detection drops the whole read chunk that holds the
first NUL, so which pre-NUL bytes get searched depends on buffer growth left
over from previously searched files. Wrapping the raw stream in
[`NulQuitReader`] removes that history: every byte before the first NUL is
delivered on a clean line boundary, then the NUL alone, so binary detection
still quits and flags the file while the searched bytes are always exactly
the pre-NUL prefix.
*/

use std::io::{self, Read};

/// Head bytes that identify a stream the searcher will transcode
const DECODING_BOMS: [&[u8]; 4] = [
    &[0xFF, 0xFE],
    &[0xFE, 0xFF],
    &[0xFF, 0xFE, 0x00, 0x00],
    &[0x00, 0x00, 0xFE, 0xFF],
];

/// Longest decoding byte-order mark
const BOM_SNIFF_LEN: usize = 4;

/// Head bytes the searcher removes before the matcher sees a stream
const UTF8_BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

/// Return true when the bytes start with a UTF-16/UTF-32 byte-order mark
pub fn has_decoding_bom(bytes: &[u8]) -> bool {
    DECODING_BOMS.iter().any(|bom| bytes.starts_with(bom))
}

/// The bytes the matcher sees, with any UTF-8 byte-order mark removed
///
/// Sniffing strips this mark, so line one starts after it and anchors bind to
/// the first real byte
pub fn without_utf8_bom(bytes: &[u8]) -> &[u8] {
    bytes.strip_prefix(&UTF8_BOM).unwrap_or(bytes)
}

/// True when the head is still a proper prefix of some decoding mark
fn could_become_bom(head: &[u8]) -> bool {
    head.len() < BOM_SNIFF_LEN && DECODING_BOMS.iter().any(|bom| bom.starts_with(head))
}

/// How quit-mode reads treat the first NUL in a raw stream
#[derive(Clone, Copy, Debug)]
pub enum QuitPolicy {
    /// End streams at the first NUL, letting BOM streams transcode instead
    PrefixUnlessBom,
    /// End every stream at the first NUL
    Prefix,
    /// Keep the searcher's own chunk-based quit handling
    Searcher,
}

/// Reading phase of a wrapped stream
#[derive(Clone, Copy, Debug)]
enum State {
    Sniff,
    Scan,
    Passthrough,
    Newline,
    Nul,
    Done,
}

/// Reader that ends a raw stream at its first NUL on a line boundary
#[derive(Debug)]
pub struct NulQuitReader<R> {
    inner: R,
    sniff_bom: bool,
    held: Vec<u8>,
    served: usize,
    last: Option<u8>,
    state: State,
}

impl<R: Read> NulQuitReader<R> {
    /// Wrap a raw stream under the given quit policy
    pub fn new(inner: R, policy: QuitPolicy) -> NulQuitReader<R> {
        NulQuitReader {
            inner,
            sniff_bom: matches!(policy, QuitPolicy::PrefixUnlessBom),
            held: Vec::new(),
            served: 0,
            last: None,
            state: State::Sniff,
        }
    }

    /// True while sniffed head bytes await delivery
    fn holding(&self) -> bool {
        self.served < self.held.len()
    }

    /// Copy pending head bytes into the caller's buffer
    fn drain_held(&mut self, buf: &mut [u8]) -> usize {
        let take = (self.held.len() - self.served).min(buf.len());
        buf[..take].copy_from_slice(&self.held[self.served..self.served + take]);
        self.served += take;
        take
    }

    /// Classify the stream head, delivering it when already decisive
    fn sniff(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if !self.sniff_bom {
            self.state = State::Scan;
            return Ok(0);
        }
        let n = self.inner.read(buf)?;
        let head = &buf[..n.min(BOM_SNIFF_LEN)];
        if n > 0 && could_become_bom(head) {
            self.held.extend_from_slice(&buf[..n]);
            return self.finish_sniff();
        }
        if has_decoding_bom(head) {
            self.state = State::Passthrough;
            return Ok(n);
        }
        self.state = State::Scan;
        Ok(self.accept(&buf[..n]))
    }

    /// Grow an ambiguous head until the byte-order mark question settles
    fn finish_sniff(&mut self) -> io::Result<usize> {
        while self.held.len() < BOM_SNIFF_LEN {
            let mut tail = [0u8; BOM_SNIFF_LEN];
            let free = BOM_SNIFF_LEN - self.held.len();
            let n = self.inner.read(&mut tail[..free])?;
            if n == 0 {
                break;
            }
            self.held.extend_from_slice(&tail[..n]);
        }
        self.state = if has_decoding_bom(&self.held) {
            State::Passthrough
        } else {
            State::Scan
        };
        Ok(0)
    }

    /// Pull the next raw bytes and cap delivery at the first NUL
    fn scan(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = if self.holding() {
            self.drain_held(buf)
        } else {
            self.inner.read(buf)?
        };
        Ok(self.accept(&buf[..n]))
    }

    /// Deliverable length of freshly read bytes, quitting cleanly at a NUL
    fn accept(&mut self, chunk: &[u8]) -> usize {
        let Some(nul) = memchr::memchr(0, chunk) else {
            self.last = chunk.last().copied().or(self.last);
            return chunk.len();
        };
        if nul > 0 {
            self.last = Some(chunk[nul - 1]);
        }
        self.served = self.held.len();
        self.state = match self.last {
            Some(b'\n') | None => State::Nul,
            Some(_) => State::Newline,
        };
        nul
    }

    /// Serve a BOM stream unchanged
    fn read_through(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.holding() {
            return Ok(self.drain_held(buf));
        }
        self.inner.read(buf)
    }

    /// Emit one injected byte and move to the next phase
    fn inject(&mut self, buf: &mut [u8], byte: u8, next: State) -> usize {
        buf[0] = byte;
        self.state = next;
        1
    }
}

impl<R: Read> Read for NulQuitReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            match self.state {
                State::Sniff => {
                    let n = self.sniff(buf)?;
                    if n > 0 {
                        return Ok(n);
                    }
                },
                State::Scan => {
                    let n = self.scan(buf)?;
                    if n > 0 || matches!(self.state, State::Scan) {
                        return Ok(n);
                    }
                },
                State::Passthrough => return self.read_through(buf),
                State::Newline => return Ok(self.inject(buf, b'\n', State::Nul)),
                State::Nul => return Ok(self.inject(buf, 0, State::Done)),
                State::Done => return Ok(0),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Read};

    use super::{NulQuitReader, QuitPolicy, has_decoding_bom, without_utf8_bom};

    fn drain(bytes: &[u8], policy: QuitPolicy) -> Vec<u8> {
        let mut reader = NulQuitReader::new(Cursor::new(bytes.to_vec()), policy);
        let mut out = Vec::new();
        reader.read_to_end(&mut out).expect("read wrapped stream");
        out
    }

    fn drain_slowly(bytes: &[u8], policy: QuitPolicy, step: usize) -> Vec<u8> {
        let mut reader = NulQuitReader::new(Cursor::new(bytes.to_vec()), policy);
        let mut out = Vec::new();
        let mut buf = vec![0u8; step];
        loop {
            let n = reader.read(&mut buf).expect("read wrapped stream");
            if n == 0 {
                return out;
            }
            out.extend_from_slice(&buf[..n]);
        }
    }

    #[test]
    fn utf8_bom_is_stripped_like_the_searcher_strips_it() {
        assert_eq!(
            b"alpha",
            without_utf8_bom(&[0xEF, 0xBB, 0xBF, b'a', b'l', b'p', b'h', b'a'])
        );
        assert_eq!(b"alpha", without_utf8_bom(b"alpha"));
        assert_eq!(&[0xEF, 0xBB], without_utf8_bom(&[0xEF, 0xBB]));
        assert_eq!(
            &[0xFF, 0xFE, b'a'],
            without_utf8_bom(&[0xFF, 0xFE, b'a']),
            "transcoding marks are not stripped here"
        );
    }

    #[test]
    fn bom_prefixes() {
        assert!(has_decoding_bom(&[0xFF, 0xFE, b'a']));
        assert!(has_decoding_bom(&[0xFE, 0xFF, b'a']));
        assert!(has_decoding_bom(&[0x00, 0x00, 0xFE, 0xFF, b'a']));
        assert!(!has_decoding_bom(b"no bom"));
        assert!(!has_decoding_bom(&[0xEF, 0xBB, 0xBF, b'a']));
    }

    #[test]
    fn nul_free_streams_pass_unchanged() {
        assert_eq!(
            b"plain\ntext\n".to_vec(),
            drain(b"plain\ntext\n", QuitPolicy::Prefix)
        );
        assert_eq!(
            b"no newline",
            drain(b"no newline", QuitPolicy::PrefixUnlessBom).as_slice()
        );
        assert_eq!(Vec::<u8>::new(), drain(b"", QuitPolicy::PrefixUnlessBom));
    }

    #[test]
    fn nul_ends_the_stream_on_a_line_boundary() {
        assert_eq!(
            b"pre\n\0".to_vec(),
            drain(b"pre\n\0post", QuitPolicy::Prefix)
        );
        assert_eq!(
            b"pre\npartial\n\0".to_vec(),
            drain(b"pre\npartial\0post", QuitPolicy::Prefix),
            "a partial final line gains a newline before the NUL"
        );
        assert_eq!(b"\0".to_vec(), drain(b"\0post", QuitPolicy::Prefix));
    }

    #[test]
    fn every_prefix_byte_survives_any_chunking() {
        let bytes = b"alpha\nbeta gamma\ndelta\0trailing junk";
        for step in [1, 2, 3, 5, 64] {
            assert_eq!(
                b"alpha\nbeta gamma\ndelta\n\0".to_vec(),
                drain_slowly(bytes, QuitPolicy::PrefixUnlessBom, step),
                "chunk size {step}"
            );
        }
    }

    #[test]
    fn utf16_bom_streams_pass_through_when_transcoding_may_apply() {
        let mut bytes = vec![0xFF, 0xFE];
        for unit in "text\n".encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        assert_eq!(bytes, drain(&bytes, QuitPolicy::PrefixUnlessBom));
        assert_eq!(
            vec![0xFF, 0xFE, b't', b'\n', 0x00],
            drain(&bytes, QuitPolicy::Prefix),
            "raw reads end at the first NUL even after a BOM"
        );
    }

    #[test]
    fn utf16_bom_streams_pass_through_tiny_read_buffers() {
        let mut bytes = vec![0xFF, 0xFE];
        for unit in "ab\n".encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        assert_eq!(bytes, drain_slowly(&bytes, QuitPolicy::PrefixUnlessBom, 1));
    }

    #[test]
    fn ambiguous_heads_resolve_without_losing_bytes() {
        assert_eq!(vec![0xFF], drain(&[0xFF], QuitPolicy::PrefixUnlessBom));
        assert_eq!(
            b"\0".to_vec(),
            drain(&[0x00, 0x00, b'a', b'b'], QuitPolicy::PrefixUnlessBom),
            "a near-BOM head that ends up binary quits at its leading NUL"
        );
        assert_eq!(
            vec![0x00, 0x00, 0xFE, 0xFF, b'a'],
            drain(&[0x00, 0x00, 0xFE, 0xFF, b'a'], QuitPolicy::PrefixUnlessBom)
        );
    }
}
