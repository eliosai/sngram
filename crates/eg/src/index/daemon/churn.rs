//! Rebuild debounce: a churning corpus is rebuilt once it settles, not per event.

use std::{
    collections::HashMap,
    env,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// Quiet time a churning corpus must reach before a rebuild is worth starting
const DEBOUNCE_QUIET: Duration = Duration::from_secs(1);
/// Longest a rebuild is deferred while a corpus keeps churning
const DEBOUNCE_CEILING: Duration = Duration::from_secs(30);
const DEBOUNCE_QUIET_ENV: &str = "EG_INDEXD_DEBOUNCE_MS";

/// When each watched corpus last moved, and how long its rebuild has waited.
#[derive(Default)]
pub struct Churn {
    dirty_since: HashMap<PathBuf, Instant>,
    last_event: HashMap<PathBuf, Instant>,
}

impl Churn {
    /// Record that the watcher saw this corpus change
    pub fn mark(&mut self, state_root: &Path) {
        let now = Instant::now();
        self.dirty_since
            .entry(state_root.to_path_buf())
            .or_insert(now);
        self.last_event.insert(state_root.to_path_buf(), now);
    }

    /// Forget a corpus once its rebuild ran
    pub fn settle(&mut self, state_root: &Path) {
        self.dirty_since.remove(state_root);
        self.last_event.remove(state_root);
    }

    /// True while a still-churning corpus should keep its rebuild deferred
    pub fn defers(&self, state_root: &Path, has_index: bool) -> bool {
        if !has_index {
            return false;
        }
        let Some(last_event) = self.last_event.get(state_root) else {
            return false;
        };
        let deferred_for = self
            .dirty_since
            .get(state_root)
            .map_or(Duration::ZERO, Instant::elapsed);
        last_event.elapsed() < debounce_quiet() && deferred_for < DEBOUNCE_CEILING
    }
}

fn debounce_quiet() -> Duration {
    env::var(DEBOUNCE_QUIET_ENV)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map_or(DEBOUNCE_QUIET, Duration::from_millis)
}

#[cfg(test)]
mod tests {
    use super::{Churn, DEBOUNCE_CEILING};
    use std::{
        path::{Path, PathBuf},
        time::{Duration, Instant},
    };

    #[test]
    fn a_rebuild_is_deferred_until_the_corpus_settles() {
        let mut churn = Churn::default();
        let state_root = Path::new("/tmp/eg-churn-settle");

        assert!(!churn.defers(state_root, true));

        churn.mark(state_root);
        assert!(churn.defers(state_root, true));

        churn.settle(state_root);
        assert!(!churn.defers(state_root, true));
    }

    #[test]
    fn a_corpus_without_an_index_is_never_deferred() {
        let mut churn = Churn::default();
        let state_root = Path::new("/tmp/eg-churn-first-build");
        churn.mark(state_root);

        assert!(!churn.defers(state_root, false));
    }

    #[test]
    fn deferral_stops_once_the_ceiling_passes() {
        let mut churn = Churn::default();
        let state_root = PathBuf::from("/tmp/eg-churn-ceiling");
        churn.mark(&state_root);
        let long_ago = Instant::now()
            .checked_sub(DEBOUNCE_CEILING + Duration::from_secs(1))
            .expect("monotonic clock has enough history");
        churn.dirty_since.insert(state_root.clone(), long_ago);

        assert!(!churn.defers(&state_root, true));
    }
}
