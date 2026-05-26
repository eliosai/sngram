//! Checkpoint and resume via redb.
//!
//! Stores bigram counts and progress in a single ACID database.

use std::path::Path;

use anyhow::Context;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};

use crate::counter::BigramCounter;

const COUNTS: TableDefinition<u32, u64> = TableDefinition::new("counts");
const META: TableDefinition<&str, u64> = TableDefinition::new("meta");

/// Checkpoint state: counter data + how many datasets completed.
pub struct CheckpointData {
    pub counter: BigramCounter,
    pub completed_datasets: usize,
}

/// # Errors
///
/// Returns error on database write failure.
pub fn save(
    db_path: &Path,
    counter: &BigramCounter,
    completed_datasets: usize,
) -> anyhow::Result<()> {
    let db = Database::create(db_path).context("creating checkpoint")?;
    let txn = db.begin_write().context("begin write")?;
    let _ = txn.delete_table(COUNTS);
    let _ = txn.delete_table(META);
    write_counts(&txn, counter)?;
    write_meta(&txn, counter, completed_datasets)?;
    txn.commit().context("commit")?;
    Ok(())
}

/// # Errors
///
/// Returns error if database is missing or corrupt.
pub fn restore(db_path: &Path) -> anyhow::Result<CheckpointData> {
    let db = Database::open(db_path).context("opening checkpoint")?;
    let txn = db.begin_read().context("begin read")?;
    let counter = BigramCounter::new();
    read_counts(&txn, &counter)?;
    let completed = read_meta(&txn, &counter)?;
    Ok(CheckpointData { counter, completed_datasets: completed })
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
                table.insert(idx, val).context("insert")?;
            }
        }
    }
    Ok(())
}

fn write_meta(
    txn: &redb::WriteTransaction,
    counter: &BigramCounter,
    completed: usize,
) -> anyhow::Result<()> {
    let mut table = txn.open_table(META).context("open meta")?;
    table.insert("pairs_processed", counter.pairs_processed()).context("pairs")?;
    table.insert("files_processed", counter.files_processed()).context("files")?;
    table.insert("completed_datasets", completed as u64).context("completed")?;
    Ok(())
}

#[expect(clippy::cast_possible_truncation, reason = "idx masked to u8 range")]
fn read_counts(
    txn: &redb::ReadTransaction,
    counter: &BigramCounter,
) -> anyhow::Result<()> {
    let table = txn.open_table(COUNTS).context("open counts")?;
    for entry in table.iter().context("iterating")? {
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
) -> anyhow::Result<usize> {
    let table = txn.open_table(META).context("open meta")?;
    if let Some(v) = table.get("pairs_processed").context("pairs")? {
        counter.add_pairs(v.value());
    }
    if let Some(v) = table.get("files_processed").context("files")? {
        counter.add_files(v.value());
    }
    let completed = table.get("completed_datasets").context("completed")?
        .map_or(0, |v| v.value() as usize);
    Ok(completed)
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
        save(&db, &orig, 1).unwrap();

        let data = restore(&db).unwrap();
        assert_eq!(data.counter.count(b'h', b'e'), orig.count(b'h', b'e'));
        assert_eq!(data.counter.pairs_processed(), orig.pairs_processed());
        assert_eq!(data.completed_datasets, 1);
    }

    #[test]
    fn produces_same_table() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("test.redb");

        let orig = BigramCounter::new();
        orig.process(b"fn main() { }");
        save(&db, &orig, 0).unwrap();

        let data = restore(&db).unwrap();
        let t1 = WeightTable::from_bytes(&orig.to_table_bytes()).unwrap();
        let t2 = WeightTable::from_bytes(&data.counter.to_table_bytes()).unwrap();

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
        save(&db, &c1, 0).unwrap();

        let c2 = BigramCounter::new();
        c2.process(b"bbb");
        save(&db, &c2, 2).unwrap();

        let data = restore(&db).unwrap();
        assert!(data.counter.count(b'b', b'b') > 0);
        assert_eq!(data.counter.count(b'a', b'a'), 0);
        assert_eq!(data.completed_datasets, 2);
    }
}
