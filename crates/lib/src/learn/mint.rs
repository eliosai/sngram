//! Private count-to-weight minting policy.

use super::settings::LearnSettings;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tuning {
    pub boundary_discount: u32,
    pub boundary_floor: u32,
}

impl Tuning {
    pub const OFF: Self = Self {
        boundary_discount: LearnSettings::MINT_OFF_BOUNDARY_DISCOUNT,
        boundary_floor: LearnSettings::MINT_OFF_BOUNDARY_FLOOR,
    };
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            boundary_discount: LearnSettings::MINT_DEFAULT_BOUNDARY_DISCOUNT,
            boundary_floor: LearnSettings::MINT_DEFAULT_BOUNDARY_FLOOR,
        }
    }
}

#[cfg(test)]
#[derive(Debug, Clone)]
pub struct MintOptions<'a> {
    pub provenance: &'a str,
    pub tuning: Tuning,
}

pub const fn is_boundary_pair(c1: u8, c2: u8) -> bool {
    is_separator(c1)
        || is_separator(c2)
        || is_line_terminator(c1)
        || is_line_terminator(c2)
        || (c1.is_ascii_lowercase() && c2.is_ascii_uppercase())
}

const fn is_separator(c: u8) -> bool {
    matches!(c, b'_' | b'.' | b'/' | b'-' | b':')
}

const fn is_line_terminator(c: u8) -> bool {
    matches!(c, b'\n' | b'\r')
}

pub const fn tune_weight(raw: u32, c1: u8, c2: u8, tuning: Tuning) -> u32 {
    if tuning.boundary_discount <= 1 || !is_boundary_pair(c1, c2) {
        return raw;
    }
    let discounted = raw / tuning.boundary_discount;
    if discounted < tuning.boundary_floor {
        tuning.boundary_floor
    } else {
        discounted
    }
}

#[allow(clippy::cast_possible_truncation, reason = "min() clamps to u32::MAX")]
pub fn compute_weight(total: u64, count: u64) -> u32 {
    if count == 0 {
        return u32::MAX;
    }
    (total / count).min(u64::from(u32::MAX)) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case_seam_discounts_lower_to_upper_only() {
        assert!(is_boundary_pair(b'd', b'C'));
        assert!(!is_boundary_pair(b'D', b'c'));
        assert!(!is_boundary_pair(b'D', b'C'));
        assert!(!is_boundary_pair(b'd', b'c'));
    }

    #[test]
    fn identity_tuning_passes_weights_through() {
        assert_eq!(tune_weight(64, b'a', b'_', Tuning::OFF), 64);
    }
}
