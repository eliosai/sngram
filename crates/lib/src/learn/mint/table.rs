use sngram_types::WeightTable;

#[cfg(test)]
use super::MintOptions;
use super::{Tuning, compute_weight, tune_weight};
use crate::learn::BigramCounter;

impl BigramCounter {
    /// Serialize the learned weight table in the `SPNG` binary format
    #[must_use]
    pub fn to_table_bytes(&self) -> Vec<u8> {
        self.weight_table(Tuning::OFF).to_bytes()
    }

    fn weight_table(&self, tuning: Tuning) -> WeightTable {
        let total = self.pairs_processed();
        WeightTable::from_weight_fn(|first, second| {
            let raw = compute_weight(total, self.count(first, second));
            tune_weight(raw, first, second, tuning)
        })
    }

    /// Serialize a tuned table with test provenance
    ///
    /// # Errors
    ///
    /// Returns an error for invalid provenance
    #[cfg(test)]
    pub fn mint_table_bytes(
        &self,
        options: &MintOptions<'_>,
    ) -> Result<Vec<u8>, sngram_types::TableError> {
        Ok(self
            .weight_table(options.tuning)
            .with_provenance(options.provenance)?
            .to_bytes())
    }
}
