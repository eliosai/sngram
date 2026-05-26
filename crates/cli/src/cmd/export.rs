//! Export a completed weight table to various formats.

use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, bail};

use crate::ExportFormat;
use crate::session;

/// # Errors
///
/// Returns error if session not completed or write fails.
pub fn run(name: &str, format: ExportFormat, output: &Path) -> anyhow::Result<()> {
    if session::state(name) != session::State::Completed {
        bail!("session '{name}' is not completed");
    }
    let src = session::weights(name);
    let bytes = fs::read(&src).context("reading weights")?;

    match format {
        ExportFormat::Bin => export_bin(&bytes, output),
        ExportFormat::Json => export_json(&bytes, output),
        ExportFormat::Csv => export_csv(&bytes, output),
        ExportFormat::Rust => export_rust(&bytes, output),
    }
}

fn export_bin(bytes: &[u8], output: &Path) -> anyhow::Result<()> {
    fs::write(output, bytes).context("writing binary")
}

fn export_json(bytes: &[u8], output: &Path) -> anyhow::Result<()> {
    let table = sngram_types::WeightTable::from_bytes(bytes)?;
    let mut file = fs::File::create(output).context("creating json")?;
    writeln!(file, "{{")?;
    writeln!(file, "  \"version\": {},", table.version())?;
    writeln!(file, "  \"weights\": {{")?;
    let mut first = true;
    for c1 in 0u16..=255 {
        for c2 in 0u16..=255 {
            let w = table.weight(c1 as u8, c2 as u8);
            if w == u32::MAX { continue; }
            if !first { writeln!(file, ",")?; }
            first = false;
            write!(file, "    \"{c1:02x}{c2:02x}\": {w}")?;
        }
    }
    writeln!(file, "\n  }}")?;
    writeln!(file, "}}")?;
    Ok(())
}

fn export_csv(bytes: &[u8], output: &Path) -> anyhow::Result<()> {
    let table = sngram_types::WeightTable::from_bytes(bytes)?;
    let mut file = fs::File::create(output).context("creating csv")?;
    writeln!(file, "c1,c2,weight")?;
    for c1 in 0u16..=255 {
        for c2 in 0u16..=255 {
            let w = table.weight(c1 as u8, c2 as u8);
            writeln!(file, "{c1},{c2},{w}")?;
        }
    }
    Ok(())
}

fn export_rust(bytes: &[u8], output: &Path) -> anyhow::Result<()> {
    let table = sngram_types::WeightTable::from_bytes(bytes)?;
    let mut file = fs::File::create(output).context("creating rust")?;
    writeln!(file, "pub const TABLE: [u32; 65536] = [")?;
    for c1 in 0u16..=255 {
        for c2 in 0u16..=255 {
            let w = table.weight(c1 as u8, c2 as u8);
            write!(file, "    {w},")?;
            if c2 % 8 == 7 { writeln!(file)?; }
        }
    }
    writeln!(file, "];")?;
    Ok(())
}
