//! Sparse n-gram extraction for code search indexing.
//!
//! Stateless, `Send + Sync`, zero contention.
//!
//! # Algorithm
//!
//! A weight table assigns a u32 weight to every byte pair (bigram).
//! Rare pairs get high weights, common pairs get low weights.
//!
//! **Indexing** (per document): a monotonic stack scans all byte
//! pairs left-to-right. Substrings where both border weights are
//! strictly greater than all internal weights are emitted as
//! sparse n-grams. These go into an inverted index keyed by hash.
//!
//! **Querying** (per regex): the pattern's HIR is folded into a
//! conservative boolean query over gram presence. Literals cover to
//! the grams the scan is guaranteed to emit for them (maximal for a
//! lone literal, minimal per branch for wide variant sets), which are
//! looked up in the inverted index.
//!
//! # API
//!
//! - [`scan`] extracts sparse n-grams and metadata from one byte stream.
//! - `scan_async` extracts the same index format from an asynchronous stream.
//! - [`query`] decomposes one regex pattern into a planned gram prefilter.
//! - `weights` embeds the trained production weight table.
//! - `learn` module (feature `learn`) trains fresh weight tables.

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
#[cfg(feature = "learn")]
pub use learn::error::LearnError;
pub use query::plan::{DfStats, GramNeedle, PlanExpr, QueryError, QueryPlan, ScanNeed};
pub use query::query;
pub use scan::TextScanner;
pub use scan::event::{ByteRange, GramKey, ScanError, ScanEvent, ScanSummary, ScannedGram};
pub use scan::flags::ScanFlags;
pub use scan::scan;
#[cfg(feature = "stream")]
pub use scan::scan_async;
pub use table::WeightTable;
pub use table::error::TableError;
#[cfg(feature = "weights")]
pub use weights::weights;
