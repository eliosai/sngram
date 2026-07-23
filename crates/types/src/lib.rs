//! Shared types for sparse n-gram extraction and weight tables.

mod bytes;
mod content;
mod error;
mod gram;
mod hashing;
mod learn;
mod query;
mod scan;
mod spng;
mod table;
mod tuning;

pub use bytes::{ByteSet256, EdgeBytes, SaturatingByteCounts256};
pub use content::Content;
pub use error::TableError;
pub use gram::Gram;
pub use hashing::HashKey;
pub use learn::LearnError;
pub use query::{DfStats, GramNeedle, PlanExpr, QueryError, QueryPlan, ScanNeed};
pub use scan::{ByteRange, GramKey, ScanError, ScanEvent, ScanFlags, ScanSummary, ScannedGram};
pub use table::WeightTable;
