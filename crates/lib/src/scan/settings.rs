//! Private scan-format and engine settings.

/// Frozen scan settings for the standard index format.
pub struct ScanSettings;

impl ScanSettings {
    /// Shortest gram emitted or matched; a sparse gram spans at least one bigram.
    pub const MIN_LEN: usize = 3;
    /// Longest gram emitted; bounds index entries and covering-set members.
    pub const MAX_LEN: usize = 100;
    /// Monotonic stack entries retained per space.
    pub const STACK_CAP: usize = 128;
    /// Prefix-hash ring slots.
    pub const RING: usize = 128;
    /// Prefix-hash ring mask.
    pub const RING_MASK: usize = Self::RING - 1;
    /// Streaming byte window per gram space.
    pub const WINDOW_CAP: usize = 1024;
    /// Bytes kept when compacting the window.
    pub const WINDOW_KEEP: usize = 128;
    /// Virtual document boundary byte.
    pub const SENTINEL: u8 = b'\n';
    /// Number of virtual sentinel bytes in the standard scanned stream.
    pub const SENTINELS_PER_DOCUMENT: usize = 2;
    /// The standard index format stores document-boundary grams.
    pub const LINE_SENTINELS: bool = true;
    /// The standard index format stores a folded twin gram space.
    pub const FOLDED_SPACE: bool = true;
}
