//! Share of the per-user inotify watch limit this daemon may claim.

use std::{env, fs, path::Path};

const BUDGET_ENV: &str = "EG_INDEXD_WATCH_BUDGET";
const MAX_USER_WATCHES: &str = "/proc/sys/fs/inotify/max_user_watches";
const ASSUMED_MAX_USER_WATCHES: usize = 8192;
const BUDGET_DIVISOR: usize = 3;

/// Bounded count of inotify watches the daemon holds
pub struct WatchBudget {
    ceiling: usize,
    held: usize,
}

impl WatchBudget {
    pub fn from_env() -> Self {
        Self::with_ceiling(configured_ceiling(Path::new(MAX_USER_WATCHES)))
    }

    pub const fn with_ceiling(ceiling: usize) -> Self {
        Self { ceiling, held: 0 }
    }

    pub const fn ceiling(&self) -> usize {
        self.ceiling
    }

    pub const fn held(&self) -> usize {
        self.held
    }

    /// Watches still available before the ceiling is reached
    pub const fn spare(&self) -> usize {
        self.ceiling.saturating_sub(self.held)
    }

    /// Take one watch, or report that the budget is spent
    pub const fn claim(&mut self) -> bool {
        if self.held >= self.ceiling {
            return false;
        }
        self.held += 1;
        true
    }

    pub const fn release(&mut self, count: usize) {
        self.held = self.held.saturating_sub(count);
    }
}

fn configured_ceiling(max_user_watches: &Path) -> usize {
    if let Some(value) = env::var(BUDGET_ENV)
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
    {
        return value.max(1);
    }
    system_share(max_user_watches)
}

/// A third of the per-user limit, leaving the rest for editors and sync tools
fn system_share(max_user_watches: &Path) -> usize {
    let max = fs::read_to_string(max_user_watches)
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|max| *max > 0)
        .unwrap_or(ASSUMED_MAX_USER_WATCHES);
    (max / BUDGET_DIVISOR).max(1)
}

#[cfg(test)]
mod tests {
    use super::{ASSUMED_MAX_USER_WATCHES, BUDGET_DIVISOR, WatchBudget, system_share};
    use std::{fs, path::Path};

    #[test]
    fn claims_stop_at_the_ceiling() {
        let mut budget = WatchBudget::with_ceiling(2);

        assert!(budget.claim());
        assert!(budget.claim());
        assert!(!budget.claim());
        assert_eq!(2, budget.held());
        assert_eq!(0, budget.spare());
    }

    #[test]
    fn releasing_returns_capacity() {
        let mut budget = WatchBudget::with_ceiling(1);
        assert!(budget.claim());
        assert!(!budget.claim());

        budget.release(1);

        assert_eq!(0, budget.held());
        assert!(budget.claim());
    }

    #[test]
    fn releasing_more_than_held_saturates() {
        let mut budget = WatchBudget::with_ceiling(4);
        assert!(budget.claim());

        budget.release(9);

        assert_eq!(0, budget.held());
        assert_eq!(4, budget.spare());
    }

    #[test]
    fn system_share_is_a_third_of_the_user_limit() {
        let dir = tempfile::tempdir().expect("scratch dir");
        let path = dir.path().join("max_user_watches");
        fs::write(&path, "65536\n").expect("limit");

        assert_eq!(65536 / BUDGET_DIVISOR, system_share(&path));
    }

    #[test]
    fn unreadable_limit_falls_back_to_the_kernel_default() {
        let share = system_share(Path::new("/nonexistent/max_user_watches"));

        assert_eq!(ASSUMED_MAX_USER_WATCHES / BUDGET_DIVISOR, share);
    }

    #[test]
    fn nonsense_limit_falls_back_to_the_kernel_default() {
        let dir = tempfile::tempdir().expect("scratch dir");
        let path = dir.path().join("max_user_watches");
        fs::write(&path, "0\n").expect("limit");

        assert_eq!(
            ASSUMED_MAX_USER_WATCHES / BUDGET_DIVISOR,
            system_share(&path)
        );
    }

    #[test]
    fn ceiling_is_never_zero() {
        let dir = tempfile::tempdir().expect("scratch dir");
        let path = dir.path().join("max_user_watches");
        fs::write(&path, "1\n").expect("limit");

        assert_eq!(1, system_share(&path));
    }
}
