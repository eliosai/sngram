//! Checkpoint and resume via redb.
//!
//! Stores bigram counts in a single ACID database.
//! Atomic writes ensure crash-safe checkpoints.

use std::path::Path;

use anyhow::Context;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};

use crate::counter::BigramCounter;

const COUNTS: TableDefinition<u32, u64> = TableDefinition::new("counts");
const META: TableDefinition<&str, u64> = TableDefinition::new("meta");

/// Save counter state, replacing any previous checkpoint.
///
/// # Errors
///
/// Returns error on database write failure.
pub fn save(db_path: &Path, counter: &BigramCounter) -> anyhow::Result<()> {
    let db = Database::create(db_path).context("creating checkpoint db")?;
    let txn = db.begin_write().context("begin write")?;
    clear_tables(&txn)?;
    write_counts(&txn, counter)?;
    write_meta(&txn, counter)?;
    txn.commit().context("commit checkpoint")?;
    Ok(())
}

/// Restore counter state from a checkpoint.
///
/// # Errors
///
/// Returns error if database is missing or corrupt.
pub fn restore(db_path: &Path) -> anyhow::Result<BigramCounter> {
    let db = Database::open(db_path).context("opening checkpoint")?;
    let txn = db.begin_read().context("begin read")?;
    let counter = BigramCounter::new();
    read_counts(&txn, &counter)?;
    read_meta(&txn, &counter)?;
    Ok(counter)
}

fn clear_tables(txn: &redb::WriteTransaction) -> anyhow::Result<()> {
    let _ = txn.delete_table(COUNTS);
    let _ = txn.delete_table(META);
    Ok(())
}

fn write_counts(
    txn: &redb::WriteTransaction,
    counter: &BigramCounter,
) -> anyhow::Result<()> {
    let mut table = txn.open_table(COUNTS).context("open counts")?;
    for c1 in 0u8..=255 {
        for c2 in 0u8..=255 {
            let val = counter.count(c1, c2);
            if val > 0 {
                let idx = u32::from(c1) << 8 | u32::from(c2);
                table.insert(idx, val).context("insert count")?;
            }
        }
    }
    Ok(())
}

fn write_meta(
    txn: &redb::WriteTransaction,
    counter: &BigramCounter,
) -> anyhow::Result<()> {
    let mut table = txn.open_table(META).context("open meta")?;
    table.insert("pairs_processed", counter.pairs_processed()).context("insert pairs")?;
    table.insert("files_processed", counter.files_processed()).context("insert files")?;
    Ok(())
}

#[expect(clippy::cast_possible_truncation, reason = "idx masked to u8 range")]
fn read_counts(
    txn: &redb::ReadTransaction,
    counter: &BigramCounter,
) -> anyhow::Result<()> {
    let table = txn.open_table(COUNTS).context("open counts")?;
    for entry in table.iter().context("iterating counts")? {
        let (k, v) = entry.context("reading entry")?;
        let idx = k.value();
        let c1 = ((idx >> 8) & 0xFF) as u8;
        let c2 = (idx & 0xFF) as u8;
        counter.add(c1, c2, v.value());
    }
    Ok(())
}

fn read_meta(
    txn: &redb::ReadTransaction,
    counter: &BigramCounter,
) -> anyhow::Result<()> {
    let table = txn.open_table(META).context("open meta")?;
    if let Some(v) = table.get("pairs_processed").context("read pairs")? {
        counter.add_pairs(v.value());
    }
    if let Some(v) = table.get("files_processed").context("read files")? {
        counter.add_files(v.value());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sngram_types::WeightTable;

    #[test]
    fn roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("test.redb");

        let orig = BigramCounter::new();
        orig.process(b"hello world");
        orig.process(b"rust is great");
        save(&db, &orig).unwrap();

        let restored = restore(&db).unwrap();
        assert_eq!(restored.count(b'h', b'e'), orig.count(b'h', b'e'));
        assert_eq!(restored.count(b'l', b'l'), orig.count(b'l', b'l'));
        assert_eq!(restored.pairs_processed(), orig.pairs_processed());
        assert_eq!(restored.files_processed(), orig.files_processed());
    }

    #[test]
    fn produces_same_table() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("test.redb");

        let orig = BigramCounter::new();
        orig.process(b"fn main() { }");
        save(&db, &orig).unwrap();

        let restored = restore(&db).unwrap();
        let t1 = WeightTable::from_bytes(&orig.to_table_bytes()).unwrap();
        let t2 = WeightTable::from_bytes(&restored.to_table_bytes()).unwrap();

        for c1 in 0u8..=255 {
            for c2 in 0u8..=255 {
                assert_eq!(t1.weight(c1, c2), t2.weight(c1, c2));
            }
        }
    }

    #[test]
    fn save_replaces_previous() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("test.redb");

        let c1 = BigramCounter::new();
        c1.process(b"aaa");
        save(&db, &c1).unwrap();

        let c2 = BigramCounter::new();
        c2.process(b"bbb");
        save(&db, &c2).unwrap();

        let restored = restore(&db).unwrap();
        assert!(restored.count(b'b', b'b') > 0, "new data present");
        assert_eq!(restored.count(b'a', b'a'), 0, "old data must be gone");
        assert_eq!(restored.pairs_processed(), c2.pairs_processed());
    }
}
