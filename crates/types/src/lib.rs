//! Shared types for sparse n-gram extraction and weight tables.

mod content;
mod error;
mod gram;
mod hashing;
mod learn;
mod query;
mod scan;
mod table;

pub use content::Content;
pub use error::TableError;
pub use gram::Gram;
pub use hashing::HashKey;
pub use learn::LearnError;
pub use query::{DfStats, GramNeedle, PlanExpr, QueryError, QueryPlan, ScanNeed};
pub use scan::{
    ByteRange, ByteSet256, EdgeBytes, GramKey, SaturatingByteCounts256, ScanError, ScanEvent,
    ScanFlags, ScanSummary, ScannedGram,
};
pub use table::WeightTable;
