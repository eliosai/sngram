//! Plan tuning against document frequencies
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;

use sngram::{DfStats, GramKey, GramNeedle, PlanExpr, QueryPlan};

struct MapDf {
    counts: HashMap<GramKey, u64>,
    total: u64,
}

impl DfStats for MapDf {
    fn entry_count(&self, key: GramKey) -> u64 {
        self.counts.get(&key).copied().unwrap_or(0)
    }

    fn total_entries(&self) -> u64 {
        self.total
    }
}

fn df_of(pairs: &[(&[u8], u64)], total: u64) -> MapDf {
    MapDf {
        counts: pairs.iter().map(|(gram, n)| (key(gram), *n)).collect(),
        total,
    }
}

/// A test-local key: the plan and the df map only need to agree on one injective mapping
const fn key(bytes: &[u8]) -> GramKey {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    let mut i = 0;
    while i < bytes.len() {
        hash = (hash ^ bytes[i] as u64).wrapping_mul(0x0000_0100_0000_01b3);
        i += 1;
    }
    GramKey::new(hash)
}

const fn plan(expr: PlanExpr) -> QueryPlan {
    QueryPlan::new(expr)
}

#[test]
fn tune_drops_stop_grams_but_keeps_a_discriminator() {
    let df = df_of(&[(b"the", 990), (b"ing", 900), (b"zqx", 2)], 1000);
    let mut plan = plan(PlanExpr::AllOf {
        grams: vec![
            GramNeedle::Key(key(b"the")),
            GramNeedle::Key(key(b"zqx")),
            GramNeedle::Key(key(b"ing")),
        ],
        needs: vec![],
        children: vec![],
    });
    plan.tune(&df, 500);
    let PlanExpr::AllOf { grams, .. } = plan.root() else {
        panic!("tuned plan must stay AllOf");
    };
    assert_eq!(grams.len(), 1);
    assert_eq!(grams[0], GramNeedle::Key(key(b"zqx")));
}

#[test]
fn tune_keeps_the_rarest_stop_gram_when_all_are_stops() {
    let df = df_of(&[(b"the", 990), (b"ing", 900)], 1000);
    let mut plan = plan(PlanExpr::AllOf {
        grams: vec![GramNeedle::Key(key(b"the")), GramNeedle::Key(key(b"ing"))],
        needs: vec![],
        children: vec![],
    });
    plan.tune(&df, 500);
    let PlanExpr::AllOf { grams, .. } = plan.root() else {
        panic!("tuned plan must stay AllOf");
    };
    assert_eq!(grams.len(), 1);
    assert_eq!(grams[0], GramNeedle::Key(key(b"ing")));
}

#[test]
fn tune_never_thins_or_bags() {
    let df = df_of(&[(b"the", 990), (b"zqx", 2)], 1000);
    let mut plan = plan(PlanExpr::AnyOf {
        grams: vec![GramNeedle::Key(key(b"the")), GramNeedle::Key(key(b"zqx"))],
        needs: vec![],
        children: vec![],
    });
    plan.tune(&df, 500);
    let PlanExpr::AnyOf { grams, .. } = plan.root() else {
        panic!("tuned plan must stay AnyOf");
    };
    assert_eq!(grams.len(), 2);
}
