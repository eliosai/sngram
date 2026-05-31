//! Types for sparse n-gram weight tables.
#![allow(missing_docs, reason = "self-documenting accessor methods")]

mod content;
mod error;
mod gram;
mod table;

pub use content::Content;
pub use error::TableError;
pub use gram::{IndexGram, IndexGrams, QueryGram, QueryGrams};
pub use table::{TABLE_BINARY_SIZE, WeightTable};
