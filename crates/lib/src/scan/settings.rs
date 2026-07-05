//! Scanner format settings.

/// Internal constants that define the grams emitted by the scanner.
pub struct ScanSettings;

impl ScanSettings {
    pub const MIN_GRAM_LEN: usize = 3;
    pub const MAX_GRAM_LEN: usize = 100;
    pub const STACK_CAP: usize = 128;
    pub const PREFIX_RING: usize = 128;
    pub const PREFIX_RING_MASK: usize = Self::PREFIX_RING - 1;
    pub const WINDOW_CAP: usize = 1024;
    pub const WINDOW_KEEP: usize = 128;
    pub const DOCUMENT_SENTINEL: u8 = b'\n';
    pub const DOCUMENT_SENTINELS: bool = true;
    pub const CASE_FOLDED_SUPPLEMENTS: bool = true;

    pub const fn emits_len(len: usize) -> bool {
        len >= Self::MIN_GRAM_LEN && len <= Self::MAX_GRAM_LEN
    }
}
