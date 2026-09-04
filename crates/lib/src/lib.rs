#![doc = include_str!("../README.md")]

mod bytes;
mod hashing;
mod query;
mod scan;
mod table;
#[cfg(feature = "weights")]
mod weights;

#[cfg(feature = "learn")]
pub mod learn;

pub use bytes::{ByteSet256, EdgeBytes, SaturatingByteCounts256};
pub use query::plan::{DfStats, GramNeedle, PlanExpr, QueryError, QueryPlan, ScanNeed};
pub use query::query;
pub use scan::binary::is_binary;
pub use scan::flags::ScanFlags;
pub use scan::output::{ByteRange, GramKey, ScanSummary, ScannedGram};
pub use scan::scan;
pub use table::WeightTable;
pub use table::error::TableError;
#[cfg(feature = "weights")]
pub use weights::weights;
