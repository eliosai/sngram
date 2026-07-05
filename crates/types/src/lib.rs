//! Shared types for sparse n-gram extraction and weight tables.

mod content;
mod error;
mod gram;
mod hashing;
mod scan;
mod table;

pub use content::Content;
pub use error::TableError;
pub use gram::Gram;
pub use hashing::HashKey;
pub use scan::{Boundary, GramSpace, ScanError, ScanEvent, ScanFacts, ScanSummary, ScannedGram};
pub use table::WeightTable;
