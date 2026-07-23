//! Count-to-weight minting policy.

use sngram_types::WeightTable;

use super::BigramCounter;
use super::settings::LearnSettings;

impl BigramCounter {
    /// Serialize the learned weight table in the `SPNG` binary format.
    #[must_use]
    pub fn to_table_bytes(&self) -> Vec<u8> {
        self.weight_table(Tuning::OFF).to_bytes()
    }

    fn weight_table(&self, tuning: Tuning) -> WeightTable {
        let total = self.pairs_processed();
        WeightTable::from_weight_fn(|c1, c2| {
            let raw = compute_weight(total, self.count(c1, c2));
            tune_weight(raw, c1, c2, tuning)
        })
    }

    #[cfg(test)]
    fn mint_table_bytes(
        &self,
        options: &MintOptions<'_>,
    ) -> Result<Vec<u8>, sngram_types::TableError> {
        Ok(self
            .weight_table(options.tuning)
            .with_provenance(options.provenance)?
            .to_bytes())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Tuning {
    boundary_discount: u32,
    boundary_floor: u32,
}

impl Tuning {
    const OFF: Self = Self {
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
struct MintOptions<'a> {
    provenance: &'a str,
    tuning: Tuning,
}

const fn is_boundary_pair(c1: u8, c2: u8) -> bool {
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

const fn tune_weight(raw: u32, c1: u8, c2: u8, tuning: Tuning) -> u32 {
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
fn compute_weight(total: u64, count: u64) -> u32 {
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

    fn counter_with_corpus() -> BigramCounter {
        let counter = BigramCounter::new();
        for _ in 0..50 {
            counter.process(b"sched_clock init\nsched_boost done\nmodule.rs v1.2-rc:3");
        }
        counter
    }

    #[test]
    fn mint_round_trips_version_and_provenance() {
        let counter = counter_with_corpus();
        let options = MintOptions {
            provenance: "corpus=fs-validate;date=2026-07-03;commit=deadbeef",
            tuning: Tuning::default(),
        };
        let table = WeightTable::from_bytes(&counter.mint_table_bytes(&options).unwrap()).unwrap();

        assert_eq!(table.version(), 2);
        assert_eq!(table.provenance(), Some(options.provenance));
    }

    #[test]
    fn mint_rejects_oversized_provenance() {
        let counter = BigramCounter::new();
        let big = "x".repeat(2048);
        let options = MintOptions {
            provenance: &big,
            tuning: Tuning::OFF,
        };

        assert!(counter.mint_table_bytes(&options).is_err());
    }

    #[test]
    fn identity_tuning_matches_v1_weights() {
        let counter = counter_with_corpus();
        let v1 = WeightTable::from_bytes(&counter.to_table_bytes()).unwrap();
        let options = MintOptions {
            provenance: "p",
            tuning: Tuning::OFF,
        };
        let v2 = WeightTable::from_bytes(&counter.mint_table_bytes(&options).unwrap()).unwrap();

        for a in [b'_', b's', b'c', b'\n', b'.', b'k'] {
            for b in [b'_', b's', b'c', b'\n', b'.', b'k'] {
                assert_eq!(v1.weight(a, b), v2.weight(a, b), "({a},{b})");
            }
        }
    }

    #[test]
    fn boundary_pairs_discount_toward_floor() {
        let counter = counter_with_corpus();
        let tuning = Tuning {
            boundary_discount: 16,
            boundary_floor: 1,
        };
        let v1 = WeightTable::from_bytes(&counter.to_table_bytes()).unwrap();
        let options = MintOptions {
            provenance: "p",
            tuning,
        };
        let v2 = WeightTable::from_bytes(&counter.mint_table_bytes(&options).unwrap()).unwrap();

        for (a, b) in [
            (b'd', b'_'),
            (b'_', b'c'),
            (b'e', b'.'),
            (b'.', b'r'),
            (b'1', b'-'),
            (b'c', b':'),
            (b't', b'\n'),
            (b'\n', b's'),
        ] {
            let expected = (v1.weight(a, b) / 16).max(1);
            assert_eq!(v2.weight(a, b), expected, "boundary pair ({a},{b})");
        }
    }

    #[test]
    fn interior_pairs_pass_through_tuned_mint() {
        let counter = counter_with_corpus();
        let v1 = WeightTable::from_bytes(&counter.to_table_bytes()).unwrap();
        let options = MintOptions {
            provenance: "p",
            tuning: Tuning::default(),
        };
        let v2 = WeightTable::from_bytes(&counter.mint_table_bytes(&options).unwrap()).unwrap();

        for (a, b) in [(b's', b'c'), (b'c', b'h'), (b'o', b'c'), (b'z', b'q')] {
            assert_eq!(v1.weight(a, b), v2.weight(a, b), "interior pair ({a},{b})");
        }
    }
}
