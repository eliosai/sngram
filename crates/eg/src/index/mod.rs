//! Sparse n-gram index integration.

pub(crate) mod backend;
pub(crate) mod config;

use anyhow::bail;

use crate::flags::HiArgs;

/// Run an indexed search.
pub(crate) fn run(_args: &HiArgs) -> anyhow::Result<bool> {
    bail!("indexed search is not implemented yet; use --no-index")
}
