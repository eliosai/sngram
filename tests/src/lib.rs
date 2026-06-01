//! Integration tests for the sngram workspace.
#![allow(missing_docs)]

#[cfg(test)]
mod tests {
    use sngram::pattern::Pattern;
    use sngram::plan::QueryPlan;
    use sngram_types::{Content, TABLE_BINARY_SIZE, WeightTable};

    fn weight_table() -> WeightTable {
        let mut buf = vec![0u8; TABLE_BINARY_SIZE];
        buf[..4].copy_from_slice(b"SPNG");
        buf[4..8].copy_from_slice(&1u32.to_le_bytes());
        for c1 in 0u16..256 {
            for c2 in 0u16..256 {
                let w = crc32fast::hash(&[c1 as u8, c2 as u8]);
                let idx = (c1 as usize) << 8 | c2 as usize;
                let off = 16 + idx * 4;
                buf[off..off + 4].copy_from_slice(&w.to_le_bytes());
            }
        }
        let crc = crc32fast::hash(&buf[16..]);
        buf[8..12].copy_from_slice(&crc.to_le_bytes());
        WeightTable::from_bytes(&buf).unwrap()
    }

    /// Gather every gram referenced anywhere in a plan.
    fn collect_grams(plan: &QueryPlan, out: &mut Vec<Vec<u8>>) {
        match plan {
            QueryPlan::All | QueryPlan::None => {}
            QueryPlan::And { grams, sub } | QueryPlan::Or { grams, sub } => {
                out.extend(grams.iter().cloned());
                for s in sub {
                    collect_grams(s, out);
                }
            }
        }
    }

    #[test]
    fn index_produces_grams_for_literal() {
        let table = weight_table();
        let content = Content::new(b"MAX_FILE_SIZE");
        let grams = sngram::index(&table, &content);
        assert!(!grams.is_empty());
    }

    #[test]
    fn query_constrains_a_literal() {
        let table = weight_table();
        let pat = Pattern::new("MAX_FILE_SIZE").unwrap();
        assert!(matches!(sngram::query(&table, &pat), QueryPlan::And { .. }));
    }

    #[test]
    fn covering_grams_are_valid_substrings() {
        let table = weight_table();
        let pat = Pattern::new("MAX_FILE_SIZE").unwrap();
        let mut grams = Vec::new();
        collect_grams(&sngram::query(&table, &pat), &mut grams);

        assert!(!grams.is_empty());
        for bytes in &grams {
            assert!(bytes.len() >= 3, "gram too short: {bytes:?}");
            assert!(
                b"MAX_FILE_SIZE".windows(bytes.len()).any(|w| w == bytes),
                "gram not a substring: {:?}",
                String::from_utf8_lossy(bytes),
            );
        }
    }

    #[test]
    fn scan_count_matches_index_count() {
        let table = weight_table();
        let content = Content::new(b"use std::collections::HashMap;");

        let index_count = sngram::index(&table, &content).hashes().count();
        let mut scan_count = 0usize;
        sngram::scan(&table, &content, |_, _| scan_count += 1);

        assert_eq!(index_count, scan_count);
    }

    #[test]
    fn regex_wildcard_extracts_grams() {
        let table = weight_table();
        let pat = Pattern::new(r"/usr/local/.*\.conf").unwrap();
        assert_ne!(sngram::query(&table, &pat), QueryPlan::All);
    }

    #[test]
    fn unconstrainable_regex_is_all() {
        let table = weight_table();
        let pat = Pattern::new(".*").unwrap();
        assert_eq!(sngram::query(&table, &pat), QueryPlan::All);
    }

    #[test]
    fn deterministic_across_runs() {
        let table = weight_table();
        let content = Content::new(b"hello world");
        let h1: Vec<u64> = sngram::index(&table, &content).hashes().collect();
        let h2: Vec<u64> = sngram::index(&table, &content).hashes().collect();
        assert_eq!(h1, h2);
    }

    #[test]
    fn types_are_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<WeightTable>();
        assert_send_sync::<Content<'_>>();
        assert_send_sync::<QueryPlan>();
    }

    #[test]
    fn concurrent_indexing() {
        let table = std::sync::Arc::new(weight_table());
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let t = table.clone();
                std::thread::spawn(move || {
                    let data = format!("thread {i} content");
                    let content = Content::new(data.as_bytes());
                    sngram::index(&t, &content).hashes().count()
                })
            })
            .collect();
        let total: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();
        assert!(total > 0);
    }
}
