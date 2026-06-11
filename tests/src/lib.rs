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
                out.extend(grams.iter().map(|g| g.as_bytes().to_vec()));
                for s in sub {
                    collect_grams(s, out);
                }
            }
        }
    }

    fn scan_grams(table: &WeightTable, doc: &[u8]) -> Vec<(Vec<u8>, u64)> {
        let mut out = Vec::new();
        sngram::scan(table, &Content::new(doc), |s, e, h| {
            out.push((doc[s..e].to_vec(), h));
        });
        out
    }

    #[test]
    fn scan_produces_grams_for_literal() {
        let table = weight_table();
        assert!(!scan_grams(&table, b"MAX_FILE_SIZE").is_empty());
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
    fn scan_emits_one_hash_per_gram() {
        let table = weight_table();
        let doc = b"use std::collections::HashMap;";

        let grams = scan_grams(&table, doc);
        let mut scan_count = 0usize;
        sngram::scan(&table, &Content::new(doc), |_, _, _| scan_count += 1);

        assert_eq!(grams.len(), scan_count);
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
        let h1 = scan_grams(&table, b"hello world");
        let h2 = scan_grams(&table, b"hello world");
        assert!(!h1.is_empty());
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
                    let mut n = 0usize;
                    sngram::scan(&t, &content, |_, _, _| n += 1);
                    n
                })
            })
            .collect();
        let total: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();
        assert!(total > 0);
    }
}
