//! Inspect a completed weight table.

use anyhow::{Context, bail};
use sngram_types::WeightTable;

use crate::session;

/// # Errors
///
/// Returns error if session not found or not completed.
pub fn run(name: &str, top_rare: usize, top_common: usize) -> anyhow::Result<()> {
    if session::state(name) != session::State::Completed {
        bail!("session '{name}' is not completed");
    }
    let bytes = std::fs::read(session::weights(name))
        .context("reading weights file")?;
    let table = WeightTable::from_bytes(&bytes)
        .context("parsing weight table")?;

    println!("  Session     {name}");
    println!("  Version     {}", table.version());
    println!("  Size        {} bytes", bytes.len());
    println!();
    print_top("  Rarest bigrams", &ranked_pairs(&table, true), top_rare);
    println!();
    print_top("  Most common bigrams", &ranked_pairs(&table, false), top_common);
    Ok(())
}

fn ranked_pairs(table: &WeightTable, descending: bool) -> Vec<(u8, u8, u32)> {
    let mut pairs: Vec<(u8, u8, u32)> = (0u16..=255)
        .flat_map(|c1| {
            (0u16..=255).map(move |c2| {
                (c1 as u8, c2 as u8, table.weight(c1 as u8, c2 as u8))
            })
        })
        .filter(|(_, _, w)| *w != u32::MAX && *w > 0)
        .collect();
    if descending {
        pairs.sort_unstable_by(|a, b| b.2.cmp(&a.2));
    } else {
        pairs.sort_unstable_by(|a, b| a.2.cmp(&b.2));
    }
    pairs
}

fn print_top(header: &str, pairs: &[(u8, u8, u32)], n: usize) {
    println!("{header}");
    for &(c1, c2, w) in pairs.iter().take(n) {
        let display = format_pair(c1, c2);
        println!("    {display:<12} {w:>12}");
    }
}

fn format_pair(c1: u8, c2: u8) -> String {
    let s1 = displayable(c1);
    let s2 = displayable(c2);
    format!("{s1}{s2}")
}

fn displayable(b: u8) -> String {
    if b.is_ascii_graphic() || b == b' ' {
        String::from(b as char)
    } else {
        format!("\\x{b:02x}")
    }
}
