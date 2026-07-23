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
//! - [`query`] decomposes one regex pattern into a planned gram prefilter.
//! - `weights` (tier features such as `12tb`) loads the embedded table.
//! - `learn` module (feature `learn`) trains fresh weight tables.

mod query;
mod scan;
#[cfg(feature = "12tb")]
mod weights;

#[cfg(feature = "learn")]
pub mod learn;

pub use query::query;
pub use scan::scan;
pub use sngram_types::{
    DfStats, GramNeedle, LearnError, PlanExpr, QueryError, QueryPlan, ScanError, ScanNeed,
};
#[cfg(feature = "12tb")]
pub use weights::weights;
