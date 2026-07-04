//! Types for sparse n-gram weight tables.

mod content;
mod error;
mod table;

pub use content::Content;
pub use error::TableError;
pub use table::{PROVENANCE_MAX, TABLE_BINARY_SIZE, WeightTable};
