//! Private learning settings.

pub struct LearnSettings;

impl LearnSettings {
    pub const PAIR_COUNT: usize = 256 * 256;
    pub const SNAPSHOT_BYTES: usize = Self::PAIR_COUNT * 8;
    pub const MINT_OFF_BOUNDARY_DISCOUNT: u32 = 1;
    pub const MINT_OFF_BOUNDARY_FLOOR: u32 = 1;
    pub const MINT_DEFAULT_BOUNDARY_DISCOUNT: u32 = 16;
    pub const MINT_DEFAULT_BOUNDARY_FLOOR: u32 = 1;

    pub const fn pair_index(c1: u8, c2: u8) -> usize {
        (c1 as usize) << 8 | c2 as usize
    }
}
