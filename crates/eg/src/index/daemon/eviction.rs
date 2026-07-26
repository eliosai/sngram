//! Least-recently-queried eviction of watched trees against the watch budget.

use std::{
    collections::{HashMap, HashSet},
    env,
    fs::{self, File},
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

/// Shortest time a tree keeps its watches before another tree may take them
const MIN_HOLD: Duration = Duration::from_secs(30);
const MIN_HOLD_ENV: &str = "EG_INDEXD_WATCH_MIN_HOLD_MS";

/// When a foreground query last registered interest in one indexed tree
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct QueryTouch(SystemTime);

impl QueryTouch {
    /// Touch the foreground stamped on the daemon request it rewrote
    pub fn of_request(request: &Path) -> Option<Self> {
        fs::metadata(request)
            .and_then(|meta| meta.modified())
            .ok()
            .map(Self)
    }
}

/// A dated claim on the watch budget from one indexed tree
pub struct Claimant<'a> {
    state_root: &'a Path,
    touch: QueryTouch,
}

impl<'a> Claimant<'a> {
    /// A claim, which an undated request cannot make
    pub fn new(state_root: &'a Path, request: &Path) -> Option<Self> {
        QueryTouch::of_request(request).map(|touch| Self { state_root, touch })
    }
}

/// One tree's last query and the start of its current watch tenancy
struct Tenant {
    touch: QueryTouch,
    watching_since: Option<Instant>,
}

/// Query recency and watch tenancy for the trees the daemon knows
pub struct WatchTenure {
    tenants: HashMap<PathBuf, Tenant>,
    min_hold: Duration,
}

impl WatchTenure {
    pub fn from_env() -> Self {
        Self::with_min_hold(configured_min_hold())
    }

    pub fn with_min_hold(min_hold: Duration) -> Self {
        Self {
            tenants: HashMap::new(),
            min_hold,
        }
    }

    /// Record the foreground touch this poll read for one tree
    pub fn observe(&mut self, state_root: &Path, touch: QueryTouch) {
        self.tenants
            .entry(state_root.to_path_buf())
            .and_modify(|tenant| tenant.touch = touch)
            .or_insert(Tenant {
                touch,
                watching_since: None,
            });
    }

    /// Start a watch tenancy for a tree the watcher now covers whole
    pub fn watching(&mut self, state_root: &Path) {
        if let Some(tenant) = self.tenants.get_mut(state_root) {
            tenant.watching_since.get_or_insert_with(Instant::now);
        }
    }

    /// End a watch tenancy for a tree that gave its watches back
    pub fn released(&mut self, state_root: &Path) {
        if let Some(tenant) = self.tenants.get_mut(state_root) {
            tenant.watching_since = None;
        }
    }

    /// Drop trees whose lease has lapsed
    pub fn retain_live(&mut self, live: &HashSet<&Path>) {
        self.tenants
            .retain(|state_root, _| live.contains(state_root.as_path()));
    }

    /// Least recently queried tree that may yield its watches to this claim
    pub fn victim(&self, claimant: &Claimant<'_>, evictable: &[PathBuf]) -> Option<PathBuf> {
        evictable
            .iter()
            .filter(|state_root| state_root.as_path() != claimant.state_root)
            .filter_map(|state_root| {
                Some((self.yieldable(state_root, claimant.touch)?, state_root))
            })
            .min_by_key(|(touch, _)| *touch)
            .map(|(_, state_root)| state_root.clone())
    }

    /// Touch of a tree past its minimum hold and staler than the claim
    fn yieldable(&self, state_root: &Path, claim: QueryTouch) -> Option<QueryTouch> {
        let tenant = self.tenants.get(state_root)?;
        let held_for = tenant.watching_since?.elapsed();
        (held_for >= self.min_hold && tenant.touch < claim).then_some(tenant.touch)
    }
}

/// True while a foreground query holds this lease open, and when unsure
pub fn lease_is_held(lease: &Path) -> bool {
    let Ok(file) = File::open(lease) else {
        return false;
    };
    match file.try_lock() {
        Ok(()) => {
            let _ = file.unlock();
            false
        },
        Err(_) => true,
    }
}

fn configured_min_hold() -> Duration {
    let value = env::var(MIN_HOLD_ENV).ok();
    min_hold_from(value.as_deref())
}

fn min_hold_from(value: Option<&str>) -> Duration {
    value
        .and_then(|value| value.parse::<u64>().ok())
        .map_or(MIN_HOLD, Duration::from_millis)
}

#[cfg(test)]
mod tests {
    use super::{Claimant, MIN_HOLD, QueryTouch, WatchTenure, lease_is_held, min_hold_from};
    use std::{
        collections::HashSet,
        fs::{self, File},
        path::{Path, PathBuf},
        time::{Duration, SystemTime},
    };

    fn touch_at(seconds: u64) -> QueryTouch {
        QueryTouch(SystemTime::UNIX_EPOCH + Duration::from_secs(seconds))
    }

    /// Tenure with `(state_root, touch)` trees already watched, no hold left
    fn watching(trees: &[(&str, u64)]) -> WatchTenure {
        let mut tenure = WatchTenure::with_min_hold(Duration::ZERO);
        for (state_root, seconds) in trees {
            tenure.observe(Path::new(state_root), touch_at(*seconds));
            tenure.watching(Path::new(state_root));
        }
        tenure
    }

    fn roots(paths: &[&str]) -> Vec<PathBuf> {
        paths.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn the_least_recently_queried_tree_yields_its_watches() {
        let tenure = watching(&[("/a/.eg", 30), ("/b/.eg", 10), ("/c/.eg", 20)]);
        let claimant = Claimant {
            state_root: Path::new("/d/.eg"),
            touch: touch_at(40),
        };

        let victim = tenure.victim(&claimant, &roots(&["/a/.eg", "/b/.eg", "/c/.eg"]));

        assert_eq!(Some(PathBuf::from("/b/.eg")), victim);
    }

    #[test]
    fn a_tree_queried_more_recently_than_the_claim_keeps_its_watches() {
        let tenure = watching(&[("/a/.eg", 30), ("/b/.eg", 20)]);
        let claimant = Claimant {
            state_root: Path::new("/c/.eg"),
            touch: touch_at(10),
        };

        assert_eq!(
            None,
            tenure.victim(&claimant, &roots(&["/a/.eg", "/b/.eg"]))
        );
    }

    #[test]
    fn a_claimant_never_evicts_itself() {
        let tenure = watching(&[("/a/.eg", 10)]);
        let claimant = Claimant {
            state_root: Path::new("/a/.eg"),
            touch: touch_at(50),
        };

        assert_eq!(None, tenure.victim(&claimant, &roots(&["/a/.eg"])));
    }

    #[test]
    fn a_tree_inside_its_minimum_hold_keeps_its_watches() {
        let mut tenure = WatchTenure::with_min_hold(Duration::from_secs(30));
        tenure.observe(Path::new("/a/.eg"), touch_at(10));
        tenure.watching(Path::new("/a/.eg"));
        let claimant = Claimant {
            state_root: Path::new("/b/.eg"),
            touch: touch_at(20),
        };

        assert_eq!(None, tenure.victim(&claimant, &roots(&["/a/.eg"])));
    }

    #[test]
    fn a_tree_that_gave_its_watches_back_is_not_a_victim_again() {
        let mut tenure = watching(&[("/a/.eg", 10)]);
        tenure.released(Path::new("/a/.eg"));
        let claimant = Claimant {
            state_root: Path::new("/b/.eg"),
            touch: touch_at(20),
        };

        assert_eq!(None, tenure.victim(&claimant, &roots(&["/a/.eg"])));
    }

    #[test]
    fn a_tree_the_daemon_never_saw_is_not_a_victim() {
        let tenure = watching(&[("/a/.eg", 10)]);
        let claimant = Claimant {
            state_root: Path::new("/b/.eg"),
            touch: touch_at(20),
        };

        assert_eq!(None, tenure.victim(&claimant, &roots(&["/unknown/.eg"])));
    }

    #[test]
    fn a_later_query_moves_a_tree_up_the_order() {
        let mut tenure = watching(&[("/a/.eg", 10), ("/b/.eg", 20)]);
        tenure.observe(Path::new("/a/.eg"), touch_at(30));
        let claimant = Claimant {
            state_root: Path::new("/c/.eg"),
            touch: touch_at(40),
        };

        assert_eq!(
            Some(PathBuf::from("/b/.eg")),
            tenure.victim(&claimant, &roots(&["/a/.eg", "/b/.eg"]))
        );
    }

    #[test]
    fn a_lapsed_lease_drops_the_tree_from_the_order() {
        let mut tenure = watching(&[("/a/.eg", 10), ("/b/.eg", 20)]);
        let live: HashSet<&Path> = std::iter::once(Path::new("/b/.eg")).collect();
        tenure.retain_live(&live);
        let claimant = Claimant {
            state_root: Path::new("/c/.eg"),
            touch: touch_at(40),
        };

        assert_eq!(
            Some(PathBuf::from("/b/.eg")),
            tenure.victim(&claimant, &roots(&["/a/.eg", "/b/.eg"]))
        );
    }

    #[test]
    fn an_undated_request_cannot_claim_anything() {
        let dir = tempfile::tempdir().expect("scratch dir");
        let missing = dir.path().join("absent.request");

        assert!(Claimant::new(Path::new("/a/.eg"), &missing).is_none());
    }

    #[test]
    fn a_request_dates_a_claim_by_its_modification_time() {
        let dir = tempfile::tempdir().expect("scratch dir");
        let request = dir.path().join("entry.request");
        fs::write(&request, "request").expect("request");

        let claimant = Claimant::new(Path::new("/a/.eg"), &request).expect("claim");
        let stamp = fs::metadata(&request)
            .and_then(|meta| meta.modified())
            .expect("stamp");

        assert_eq!(QueryTouch(stamp), claimant.touch);
    }

    #[test]
    fn a_lease_no_query_opened_is_not_held() {
        let dir = tempfile::tempdir().expect("scratch dir");
        let lease = dir.path().join("lease");
        fs::write(&lease, "lease").expect("lease");

        assert!(!lease_is_held(&lease));
        assert!(!lease_is_held(&dir.path().join("absent")));
    }

    #[test]
    fn a_lease_a_query_shares_is_held() {
        let dir = tempfile::tempdir().expect("scratch dir");
        let lease = dir.path().join("lease");
        fs::write(&lease, "lease").expect("lease");
        let query = File::open(&lease).expect("query");
        query.try_lock_shared().expect("share the lease");

        assert!(lease_is_held(&lease));

        drop(query);

        assert!(!lease_is_held(&lease));
    }

    #[test]
    fn the_default_minimum_hold_is_thirty_seconds() {
        assert_eq!(Duration::from_secs(30), MIN_HOLD);
        assert_eq!(MIN_HOLD, min_hold_from(None));
    }

    #[test]
    fn the_minimum_hold_override_uses_milliseconds() {
        assert_eq!(Duration::from_millis(250), min_hold_from(Some("250")));
        assert_eq!(Duration::ZERO, min_hold_from(Some("0")));
        assert_eq!(MIN_HOLD, min_hold_from(Some("not-a-number")));
    }
}
