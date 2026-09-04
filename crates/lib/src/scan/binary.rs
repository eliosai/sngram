//! The binary rule the index shares with ripgrep's verifier

/// Bytes the rule inspects, the first buffer ripgrep's searcher reads
const SNIFF_BYTES: usize = 8192;

/// True when a NUL byte sits in the first 8 KiB, the rule ripgrep's binary detection quits on
#[must_use]
pub fn is_binary(content: &[u8]) -> bool {
    content[..content.len().min(SNIFF_BYTES)].contains(&0)
}

#[cfg(test)]
mod tests {
    use super::{SNIFF_BYTES, is_binary};

    #[test]
    fn a_nul_in_the_window_is_binary() {
        assert!(is_binary(b"\x7fELF\x00\x00\x00rest"));
        assert!(is_binary(b"text\0tail"));
    }

    #[test]
    fn text_and_empty_content_are_not_binary() {
        assert!(!is_binary(b""));
        assert!(!is_binary(b"fn main() {\n    let x = 42;\n}\n"));
        assert!(!is_binary(b"BZh91AY&SY\x1b[0m escape codes stay text"));
    }

    #[test]
    fn a_nul_past_the_window_is_not_binary() {
        let mut content = vec![b'a'; SNIFF_BYTES + 1];
        content[SNIFF_BYTES] = 0;
        assert!(!is_binary(&content));
        content[SNIFF_BYTES - 1] = 0;
        assert!(is_binary(&content));
    }
}
