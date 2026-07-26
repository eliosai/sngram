//! Settling whether a daemon-owned index can answer this query.

use std::time::{Duration, Instant, SystemTime};

use anyhow::bail;

use crate::flags::{HiArgs, SearchMode};

use super::{
    dirty::{self, Vouched},
    generation::Generation,
    progress, runtime,
};

const COLD_BUILD_WAIT: Duration = Duration::from_secs(60 * 60);
const COLD_PROGRESS_POLL: Duration = Duration::from_millis(100);
const DAEMON_GONE_GRACE: Duration = Duration::from_secs(5);
/// One poll cycle of grace for a rebuild that is already nearly done.
///
/// A rebuild landing inside the grace keeps the query on the indexed path,
/// which is what a small corpus does. A corpus churning faster than it
/// rebuilds never lands, and waiting on it costs seconds where the exact
/// scan that replaces it costs tens of milliseconds.
const CHURN_WAIT: Duration = COLD_PROGRESS_POLL;
const CHURN_WAIT_ENV: &str = "EG_INDEX_CHURN_WAIT_MS";
/// Generation source naming a compatible index without a daemon proof
const STALE_SOURCE: &str = "stale";
const DISABLE_AUTOSPAWN_ENV: &str = "EG_INDEXD_DISABLE_AUTOSPAWN";

/// Why a query is answered by the exact unindexed scan instead of the index.
#[derive(Clone, Copy)]
pub enum ScanReason {
    /// a usable generation exists but the corpus moved under it
    Churning,
    /// the daemon never published a first index in the time allowed
    BuildTimedOut,
    /// no daemon can own this index right now
    DaemonUnavailable,
    /// the daemon cannot watch this tree, so it can never prove freshness
    Unwatchable,
    /// the daemon replaced the generation while this query was reading it
    Republished,
}

impl ScanReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Churning => "the corpus changed since the index was published",
            Self::BuildTimedOut => "the daemon did not publish a first index in time",
            Self::DaemonUnavailable => "no daemon owns this index",
            Self::Unwatchable => "the daemon cannot watch this tree for changes",
            Self::Republished => "the daemon republished the index during this query",
        }
    }
}

/// How long the foreground is willing to wait for a daemon-owned index.
#[derive(Clone, Copy, PartialEq)]
pub enum WaitPolicy {
    /// benchmarks measure the indexed path, so a missing proof is an error
    Required,
    /// nothing is published yet, so waiting is the only way to use the index
    FirstBuild,
    /// a usable generation exists, so a churning corpus is scanned instead
    Churning,
}

impl WaitPolicy {
    pub fn for_query(args: &HiArgs, generation: &Generation) -> Self {
        if args.index().bench() {
            Self::Required
        } else if generation.source() == STALE_SOURCE {
            Self::Churning
        } else {
            Self::FirstBuild
        }
    }

    fn budget(self) -> Duration {
        match self {
            Self::Required | Self::FirstBuild => COLD_BUILD_WAIT,
            Self::Churning => churn_wait(),
        }
    }

    const fn expiry(self) -> ScanReason {
        match self {
            Self::Required | Self::FirstBuild => ScanReason::BuildTimedOut,
            Self::Churning => ScanReason::Churning,
        }
    }
}

fn churn_wait() -> Duration {
    churn_wait_from(std::env::var(CHURN_WAIT_ENV).ok().as_deref())
}

fn churn_wait_from(value: Option<&str>) -> Duration {
    value
        .and_then(|value| value.parse::<u64>().ok())
        .map_or(CHURN_WAIT, Duration::from_millis)
}

/// How this query is answered once its generation is chosen.
pub enum Settled {
    Vouched(Vouched),
    Scan(ScanReason),
}

/// Settle how this query is answered: what the daemon vouches for, or a scan.
///
/// A query asks the daemon to vouch first, because a vouch costs a round trip
/// where waiting for a rebuild costs the rebuild.
pub fn settle(
    args: &HiArgs,
    mode: SearchMode,
    generation: &Generation,
    floor: SystemTime,
    cold_build: bool,
) -> anyhow::Result<Settled> {
    if args.index().bench() {
        return settle_bench(args, generation, cold_build);
    }
    if let Some(vouched) = dirty::vouch(args, mode, generation, floor) {
        return Ok(Settled::Vouched(vouched));
    }
    if !cold_build {
        return Ok(Settled::Scan(ScanReason::Churning));
    }
    if let Some(reason) = ensure_index_ready(generation, WaitPolicy::for_query(args, generation))? {
        return Ok(Settled::Scan(reason));
    }
    Ok(dirty::vouch(args, mode, generation, dirty::change_floor())
        .map_or(Settled::Scan(ScanReason::Churning), Settled::Vouched))
}

/// A benchmark measures the indexed path, so it waits rather than unions
fn settle_bench(
    args: &HiArgs,
    generation: &Generation,
    cold_build: bool,
) -> anyhow::Result<Settled> {
    if cold_build
        && let Some(reason) =
            ensure_index_ready(generation, WaitPolicy::for_query(args, generation))?
    {
        return Ok(Settled::Scan(reason));
    }
    Ok(Settled::Vouched(Vouched::Covered))
}

/// An index failure a benchmark keeps and every other query scans past
pub fn index_answered<T>(args: &HiArgs, result: anyhow::Result<T>) -> anyhow::Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(err) if args.index().bench() => Err(err),
        Err(err) => {
            log::debug!("eg index: the proved generation stopped answering: {err:#}");
            Ok(None)
        },
    }
}

/// Answer a query with the exact unindexed scan, complete by construction.
pub fn scan_instead(args: &HiArgs, mode: SearchMode, reason: &str) -> anyhow::Result<bool> {
    log::debug!("eg index: scanning directly because of {reason}");
    if args.threads() == 1 {
        crate::search(args, mode)
    } else {
        crate::search_parallel(args, mode)
    }
}

/// Settle whether the index can serve; `Some` means scan this query instead.
fn ensure_index_ready(
    generation: &Generation,
    policy: WaitPolicy,
) -> anyhow::Result<Option<ScanReason>> {
    if !runtime::daemon_watch_supported() {
        if policy == WaitPolicy::Required {
            bail!("indexed daemon search requires Linux filesystem watch support; use --no-index");
        }
        return Ok(Some(ScanReason::DaemonUnavailable));
    }
    let _hold = runtime::LeaseHold::acquire(generation.state_root());
    wait_for_daemon_proof(generation, policy)
}

fn wait_for_daemon_proof(
    generation: &Generation,
    policy: WaitPolicy,
) -> anyhow::Result<Option<ScanReason>> {
    let started = Instant::now();
    let lease = runtime::Lease::new(generation.index_root(), generation.state_root());
    let catching_up = generation.source() == STALE_SOURCE;
    let wake_floor = runtime::wake_mtime(generation.state_root());
    let mut progress =
        progress::BuildProgressRenderer::new(policy != WaitPolicy::Required, catching_up);
    let mut daemon_gone_since = None;
    loop {
        progress.tick(generation.state_root());
        if let Some(reason) = check_daemon_available(&mut daemon_gone_since, policy)? {
            progress.finish();
            return Ok(Some(reason));
        }
        if let Some(reason) = check_tree_is_watchable(generation, policy)? {
            progress.finish();
            return Ok(Some(reason));
        }
        if catching_up
            && let Some(floor) = wake_floor
            && runtime::daemon_caught_up_since(generation.state_root(), floor)
        {
            progress.finish();
            return Ok(None);
        }
        match wait_one_proof_poll(generation, &lease, started, policy)? {
            ProofPoll::Waiting => {},
            ProofPoll::Ready => {
                progress.finish();
                return Ok(None);
            },
            ProofPoll::GiveUp(reason) => {
                progress.finish();
                return Ok(Some(reason));
            },
        }
    }
}

/// Outcome of one bounded wait for the daemon's freshness proof.
enum ProofPoll {
    Ready,
    Waiting,
    GiveUp(ScanReason),
}

/// A tree the daemon refuses to watch can never earn a freshness proof
fn check_tree_is_watchable(
    generation: &Generation,
    policy: WaitPolicy,
) -> anyhow::Result<Option<ScanReason>> {
    let Some(reason) = runtime::watch_refusal(generation.state_root()) else {
        return Ok(None);
    };
    if policy == WaitPolicy::Required {
        bail!(
            "indexed search cannot watch {} for changes.\n\nwhy: {reason}.\nwhat works: raise `fs.inotify.max_user_watches`, raise `EG_INDEXD_WATCH_BUDGET`, search a narrower path, or pass `--no-index` for an exact unindexed scan.",
            generation.index_root().display()
        );
    }
    log::debug!("eg index: daemon refused to watch this tree: {reason}");
    Ok(Some(ScanReason::Unwatchable))
}

/// Tolerate a daemon-liveness misread briefly before giving up on the index
fn check_daemon_available(
    gone_since: &mut Option<Instant>,
    policy: WaitPolicy,
) -> anyhow::Result<Option<ScanReason>> {
    if !runtime::daemon_autospawn_disabled() || runtime::daemon_running() {
        *gone_since = None;
        return Ok(None);
    }
    let since = gone_since.get_or_insert_with(Instant::now);
    if since.elapsed() > DAEMON_GONE_GRACE {
        if policy == WaitPolicy::Required {
            bail!(
                "indexed search needs eg-indexd running while {DISABLE_AUTOSPAWN_ENV} is set.\n\nwhat works: start eg-indexd, unset {DISABLE_AUTOSPAWN_ENV} so eg spawns it, or pass --no-index for an exact unindexed scan."
            );
        }
        return Ok(Some(ScanReason::DaemonUnavailable));
    }
    std::thread::sleep(COLD_PROGRESS_POLL);
    Ok(None)
}

/// One bounded proof poll against the daemon-owned generation
fn wait_one_proof_poll(
    generation: &Generation,
    lease: &runtime::Lease<'_>,
    started: Instant,
    policy: WaitPolicy,
) -> anyhow::Result<ProofPoll> {
    let budget = policy.budget();
    let poll = budget
        .saturating_sub(started.elapsed())
        .min(COLD_PROGRESS_POLL);
    match runtime::wait_for_freshness_proof(generation.state_root(), poll) {
        runtime::ProofWait::Ready => return Ok(ProofPoll::Ready),
        runtime::ProofWait::DaemonStopped if !runtime::daemon_autospawn_disabled() => {
            if started.elapsed() < budget {
                lease.request_refresh()?;
                return Ok(ProofPoll::Waiting);
            }
        },
        runtime::ProofWait::DaemonStopped | runtime::ProofWait::TimedOut
            if started.elapsed() < budget =>
        {
            return Ok(ProofPoll::Waiting);
        },
        runtime::ProofWait::DaemonStopped | runtime::ProofWait::TimedOut => {},
    }
    if policy == WaitPolicy::Required {
        bail!(
            "timed out waiting for the daemon to publish an index at {}.\n\nwhat works: check that eg-indexd is running and making progress, or pass --no-index for an exact unindexed scan.",
            generation.index_dir().display()
        );
    }
    Ok(ProofPoll::GiveUp(policy.expiry()))
}

#[cfg(test)]
mod tests {
    use super::{CHURN_WAIT, ScanReason, WaitPolicy, churn_wait_from};
    use std::time::Duration;

    #[test]
    fn a_churning_corpus_expires_into_a_scan_but_a_first_build_reports_a_timeout() {
        assert!(matches!(
            WaitPolicy::Churning.expiry(),
            ScanReason::Churning
        ));
        assert!(matches!(
            WaitPolicy::FirstBuild.expiry(),
            ScanReason::BuildTimedOut
        ));
    }

    #[test]
    fn a_first_build_outlasts_a_churn_grace_by_orders_of_magnitude() {
        assert!(WaitPolicy::FirstBuild.budget() > WaitPolicy::Churning.budget() * 1000);
    }

    #[test]
    fn churn_grace_override_uses_milliseconds() {
        assert_eq!(CHURN_WAIT, churn_wait_from(None));
        assert_eq!(Duration::from_millis(7), churn_wait_from(Some("7")));
        assert_eq!(Duration::ZERO, churn_wait_from(Some("0")));
        assert_eq!(CHURN_WAIT, churn_wait_from(Some("not-a-number")));
    }
}
