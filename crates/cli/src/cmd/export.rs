//! Export subcommand.

use std::path::Path;
use crate::ExportFormat;

pub fn run(_name: &str, _format: ExportFormat, _output: &Path) -> anyhow::Result<()> {
    anyhow::bail!("not yet implemented: export")
}
