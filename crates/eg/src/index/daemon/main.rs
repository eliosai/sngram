//! File-based daemon for eg indexes.

mod budget;
mod churn;
mod dirty;
mod eviction;
mod generations;
mod markers;
mod rebuild;
mod reclaim;
mod watch;

use std::{
    collections::{HashMap, HashSet},
    env,
    ffi::OsString,
    fs,
    fs::{File, OpenOptions, TryLockError},
    io::{BufRead, BufReader, ErrorKind, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

use anyhow::Context;

use budget::WatchBudget;
use eviction::{Claimant, QueryTouch, WatchTenure};
use markers::{
    JOURNAL_CLEAN_FILE_NAME, OWNER_FILE_NAME, WATCH_REFUSED_FILE_NAME, WATCHER_READY_FILE_NAME,
};
use watch::WatchOutcome;

const REQUESTS_DIR_NAME: &str = "requests";
const RUNTIME_DIR_NAME: &str = "runtime";
const WAKE_FILE_NAME: &str = "wake";
const WATCH_DIRS_FILE_NAME: &str = "watch-dirs";
const INDEX_DIR_NAME: &str = "index";
const BINARY_MANIFEST_NAME: &str = "manifest.bin";
const JSON_MANIFEST_NAME: &str = "manifest.json";
const LOCK_FILE_NAME: &str = "daemon.lock";
const STARTUP_READY_FILE_NAME: &str = "startup-ready";
const LEASE_TTL_ENV: &str = "EG_INDEXD_LEASE_TTL_SECS";
const DEFAULT_LEASE_TTL: Duration = Duration::from_hours(24);
const POLL_INTERVAL: Duration = Duration::from_secs(2);
const STARTUP_IDLE_GRACE: Duration = Duration::from_mins(1);
const RECLAIM_INTERVAL: Duration = Duration::from_mins(5);
/// Refusal a tree earns when a more recently queried tree takes its watches
const EVICTED_REASON: &str =
    "eg-indexd gave this tree's filesystem watches to a tree queried more recently";
/// How often a running rebuild pauses to drain the watcher and republish
const REFRESH_POLL: Duration = Duration::from_millis(5);

fn main() {
    if let Err(err) = run() {
        eprintln!("eg-indexd: {err}");
        std::process::exit(2);
    }
}

fn run() -> anyhow::Result<()> {
    let runtime_root = runtime_root_from_args()?;
    fs::create_dir_all(&runtime_root)?;
    let Some(lock) = DaemonLock::acquire(&runtime_root)? else {
        return Ok(());
    };
    Daemon::new(runtime_root, lock.owner().to_owned())?.serve()
}

struct Daemon {
    runtime_root: PathBuf,
    owner: String,
    watcher: watch::Watcher,
    watch_stamps: HashMap<PathBuf, SystemTime>,
    refused: HashMap<PathBuf, usize>,
    tenure: WatchTenure,
    churn: churn::Churn,
    dirt: dirty::Dirt,
    drained_at: SystemTime,
    last_reclaim: Instant,
}

impl Daemon {
    fn new(runtime_root: PathBuf, owner: String) -> anyhow::Result<Self> {
        Self::with_budget(runtime_root, owner, WatchBudget::from_env())
    }

    fn with_budget(
        runtime_root: PathBuf,
        owner: String,
        budget: WatchBudget,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            runtime_root,
            owner,
            watcher: watch::Watcher::with_budget(budget)?,
            watch_stamps: HashMap::new(),
            refused: HashMap::new(),
            tenure: WatchTenure::from_env(),
            churn: churn::Churn::default(),
            dirt: dirty::Dirt::default(),
            drained_at: SystemTime::UNIX_EPOCH,
            last_reclaim: Instant::now(),
        })
    }

    fn serve(&mut self) -> anyhow::Result<()> {
        self.prepare_startup()?;
        let requests = self.runtime_root.join(REQUESTS_DIR_NAME);
        let _ = fs::create_dir_all(&requests);
        self.watcher.watch_signal_dir(&requests)?;
        let started = Instant::now();
        loop {
            let requests = read_requests(&self.runtime_root)?;
            self.clear_dirty()?;
            self.publish_dirt(&requests);
            self.release_unleased(&requests);
            self.refresh_requests(&requests);
            consolidate_children(&requests);
            self.reclaim_when_due(&requests);
            if requests.iter().any(Request::has_live_lease) {
                self.wait_for_changes(POLL_INTERVAL)?;
                continue;
            }
            if requests.is_empty() && started.elapsed() < STARTUP_IDLE_GRACE {
                self.wait_for_changes(POLL_INTERVAL)?;
                continue;
            }
            self.cleanup_requests(&requests)?;
            return Ok(());
        }
    }

    fn prepare_startup(&self) -> anyhow::Result<()> {
        let _ = fs::remove_file(self.runtime_root.join(STARTUP_READY_FILE_NAME));
        let requests = read_requests(&self.runtime_root)?;
        for request in &requests {
            match startup_disposition(request) {
                StartupDisposition::Adopt => adopt_request(request),
                StartupDisposition::Discard => discard_state(request),
            }
        }
        self.reclaim(&requests);
        self.mark_startup_ready()
    }

    /// Reclaim state left by a daemon that died, and generations now retired
    fn reclaim(&self, requests: &[Request]) {
        let roots = requests.iter().map(Request::known_root).collect::<Vec<_>>();
        let mut reclaimed = reclaim::sweep(&roots);
        reclaimed.absorb(&reclaim::sweep_quarantine(&self.runtime_root));
        if reclaimed.paths > 0 {
            log::debug!(
                "eg-indexd: reclaimed {} paths holding {} bytes",
                reclaimed.paths,
                reclaimed.bytes
            );
        }
    }

    fn reclaim_when_due(&mut self, requests: &[Request]) {
        if self.last_reclaim.elapsed() < RECLAIM_INTERVAL {
            return;
        }
        self.reclaim(requests);
        self.last_reclaim = Instant::now();
    }

    fn refresh_requests(&mut self, requests: &[Request]) {
        for request in requests {
            if !request.index_root.is_dir() {
                discard_request(request);
                continue;
            }
            if !request.has_live_lease() {
                continue;
            }
            if let Some(touch) = QueryTouch::of_request(&request.path) {
                self.tenure.observe(&request.state_root, touch);
            }
            let _ = self.serve_request(request);
        }
    }

    fn serve_request(&mut self, request: &Request) -> anyhow::Result<()> {
        if self.watch_request(request)? == WatchOutcome::Exhausted {
            return Ok(());
        }
        self.refresh_if_needed(request)
    }

    fn watch_request(&mut self, request: &Request) -> anyhow::Result<WatchOutcome> {
        let outcome = self.sync_watches_until_it_fits(request)?;
        match outcome {
            WatchOutcome::Watching => {
                self.refused.remove(&request.state_root);
                self.tenure.watching(&request.state_root);
                markers::clear_watch_refusal(&request.state_root);
                markers::mark_watcher_ready(&request.state_root)?;
            },
            WatchOutcome::Unwatchable => {},
            WatchOutcome::Exhausted => self.refuse_watch(request),
        }
        markers::mark_owner(&request.state_root, &self.owner)?;
        Ok(outcome)
    }

    /// Watch this tree, evicting least recently queried trees until it fits
    fn sync_watches_until_it_fits(&mut self, request: &Request) -> anyhow::Result<WatchOutcome> {
        let mut outcome = self.sync_watches(request)?;
        while outcome == WatchOutcome::Exhausted {
            let Some(claimant) = Claimant::new(&request.state_root, &request.path) else {
                break;
            };
            let evictable = self.evictable_trees();
            let Some(victim) = self.tenure.victim(&claimant, &evictable) else {
                break;
            };
            self.evict(&victim);
            outcome = self.sync_watches(request)?;
        }
        Ok(outcome)
    }

    /// Watched trees no foreground query is holding open right now
    fn evictable_trees(&self) -> Vec<PathBuf> {
        self.watcher
            .watched_trees()
            .into_iter()
            .filter(|state_root| !eviction::lease_is_held(&markers::lease_path(state_root)))
            .collect()
    }

    /// Withdraw a tree's freshness proof, then return its watches to the budget
    fn evict(&mut self, state_root: &Path) {
        markers::clear_journal_clean(state_root);
        markers::clear_watcher_ready(state_root);
        let _ = markers::mark_watch_refused(state_root, EVICTED_REASON);
        self.dirt.forget(state_root);
        self.watcher.release_tree(state_root);
        self.watch_stamps.remove(state_root);
        self.tenure.released(state_root);
        log::debug!(
            "eg-indexd: evicted the watches for {}",
            state_root.display()
        );
    }

    /// Withdraw the freshness proof for a tree the budget cannot cover
    fn refuse_watch(&mut self, request: &Request) {
        let spare = self.watcher.spare();
        if self.refused.insert(request.state_root.clone(), spare) == Some(spare) {
            return;
        }
        self.watch_stamps.remove(&request.state_root);
        markers::clear_watcher_ready(&request.state_root);
        let reason = format!(
            "eg-indexd holds {} of its {} allowed filesystem watches, which is not enough to watch {} for changes",
            self.watcher.held(),
            self.watcher.ceiling(),
            request.index_root.display()
        );
        let _ = markers::mark_watch_refused(&request.state_root, &reason);
        log::debug!("eg-indexd: {reason}");
    }

    /// True until other trees free watches for a refused tree to retry with
    fn is_backed_off(&self, state_root: &Path) -> bool {
        self.refused
            .get(state_root)
            .is_some_and(|spare| self.watcher.spare() <= *spare)
    }

    /// Watch the walked directory set, or the whole tree before a first build
    fn sync_watches(&mut self, request: &Request) -> anyhow::Result<WatchOutcome> {
        if self.is_backed_off(&request.state_root) {
            return Ok(WatchOutcome::Exhausted);
        }
        let path = request
            .state_root
            .join(RUNTIME_DIR_NAME)
            .join(WATCH_DIRS_FILE_NAME);
        let Ok(stamp) = fs::metadata(&path).and_then(|meta| meta.modified()) else {
            return self
                .watcher
                .watch_tree(&request.index_root, &request.state_root);
        };
        if self.watch_stamps.get(&request.state_root) == Some(&stamp)
            && self.watcher.watches_tree(&request.state_root)
        {
            return Ok(WatchOutcome::Watching);
        }
        let dirs = read_watch_dirs(&path, &request.index_root)?;
        let outcome = self
            .watcher
            .watch_dirs(&request.index_root, &dirs, &request.state_root)?;
        if outcome == WatchOutcome::Watching {
            self.watch_stamps.insert(request.state_root.clone(), stamp);
        }
        Ok(outcome)
    }

    /// Return the watch budget held for trees whose lease has lapsed
    fn release_unleased(&mut self, requests: &[Request]) {
        let live: HashSet<&Path> = requests
            .iter()
            .filter(|request| request.has_live_lease())
            .map(|request| request.state_root.as_path())
            .collect();
        self.refused
            .retain(|state_root, _| live.contains(state_root.as_path()));
        self.tenure.retain_live(&live);
        for state_root in self.watcher.watched_trees() {
            if live.contains(state_root.as_path()) {
                continue;
            }
            self.watcher.release_tree(&state_root);
            self.watch_stamps.remove(&state_root);
            self.dirt.forget(&state_root);
            markers::clear_watcher_ready(&state_root);
        }
    }

    /// Rebuild, then prove the walk beat every change it raced before publishing
    fn refresh_if_needed(&mut self, request: &Request) -> anyhow::Result<()> {
        if !request.has_live_lease() || !request.needs_refresh() {
            return Ok(());
        }
        if self.churn.defers(&request.state_root, request.has_index()) {
            return Ok(());
        }
        self.dirt.begin_build(&request.state_root);
        let built = self.run_refresh(request)?;
        if built {
            markers::mark_lease_live(&request.state_root)?;
            self.churn.settle(&request.state_root);
        }
        self.clear_dirty()?;
        if built {
            self.publish_generation(request)?;
        }
        self.publish_dirt(std::slice::from_ref(request));
        Ok(())
    }

    /// Rebuild while still draining the watcher and republishing the change set
    fn run_refresh(&mut self, request: &Request) -> anyhow::Result<bool> {
        let rebuild = request.rebuild();
        let Some(mut child) = rebuild.spawn(&self.runtime_root)? else {
            return Ok(false);
        };
        loop {
            if let Some(status) = child.try_wait()? {
                return Ok(rebuild.published(status));
            }
            self.clear_dirty()?;
            self.publish_dirt(std::slice::from_ref(request));
            std::thread::sleep(REFRESH_POLL);
        }
    }

    /// Stand the freshness proof up only for a generation nothing outran
    fn publish_generation(&mut self, request: &Request) -> anyhow::Result<()> {
        let uncontended = self.dirt.build_was_uncontended(&request.state_root);
        self.dirt.commit_build(&request.state_root);
        if uncontended && request.has_index() {
            markers::mark_journal_clean(&request.state_root)?;
        }
        Ok(())
    }

    /// Republish every watched tree's change set with the drain that produced it
    fn publish_dirt(&mut self, requests: &[Request]) {
        let drained_at = self.drained_at;
        for request in requests.iter().filter(|request| request.has_live_lease()) {
            let _ = self
                .dirt
                .publish(&request.state_root, &request.index_root, drained_at);
        }
    }

    fn clear_dirty(&mut self) -> anyhow::Result<()> {
        self.drained_at = SystemTime::now();
        let events = self.watcher.drain_dirty()?;
        self.absorb(events);
        Ok(())
    }

    fn wait_for_changes(&mut self, timeout: Duration) -> anyhow::Result<()> {
        let events = self.watcher.wait_dirty(timeout)?;
        self.absorb(events);
        Ok(())
    }

    fn absorb(&mut self, events: watch::DirtyEvents) {
        for (state_root, changes) in events.into_trees() {
            self.churn.mark(&state_root);
            self.dirt.record(&state_root, &changes);
            markers::clear_journal_clean(&state_root);
        }
    }

    fn cleanup_requests(&self, requests: &[Request]) -> anyhow::Result<()> {
        let _ = fs::remove_file(self.runtime_root.join(STARTUP_READY_FILE_NAME));
        for request in requests {
            cleanup_state_root(&request.state_root)?;
            remove_file_if_exists(&request.path).with_context(|| {
                format!("failed to remove daemon request {}", request.path.display())
            })?;
        }
        Ok(())
    }

    fn mark_startup_ready(&self) -> anyhow::Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(self.runtime_root.join(STARTUP_READY_FILE_NAME))?;
        writeln!(file, "{}", self.owner)?;
        file.sync_all()?;
        Ok(())
    }
}

fn runtime_root_from_args() -> anyhow::Result<PathBuf> {
    let mut args = env::args_os().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--runtime-root" {
            let Some(root) = args.next() else {
                anyhow::bail!("--runtime-root requires a path");
            };
            return Ok(PathBuf::from(root));
        }
    }
    if let Some(root) = env::var_os("XDG_RUNTIME_DIR").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(root).join("eg"));
    }
    Ok(env::temp_dir().join("eg-runtime"))
}

struct DaemonLock {
    path: PathBuf,
    owner: String,
    file: File,
}

impl DaemonLock {
    fn acquire(runtime_root: &Path) -> anyhow::Result<Option<Self>> {
        let path = runtime_root.join(LOCK_FILE_NAME);
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)?;
        match file.try_lock() {
            Ok(()) => {
                let owner = daemon_owner_token();
                file.set_len(0)?;
                writeln!(file, "{owner}")?;
                file.sync_all()?;
                Ok(Some(Self { path, owner, file }))
            },
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(err)) => Err(err.into()),
        }
    }

    fn owner(&self) -> &str {
        &self.owner
    }
}

fn daemon_owner_token() -> String {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    format!("{}-{nanos}", std::process::id())
}

impl Drop for DaemonLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
        let _ = fs::remove_file(&self.path);
    }
}

#[derive(Clone)]
struct Request {
    path: PathBuf,
    index_root: PathBuf,
    state_root: PathBuf,
    cwd: PathBuf,
    eg_binary: Option<PathBuf>,
    args: Vec<OsString>,
}

impl Request {
    /// The rebuild this request runs when its index falls behind
    fn rebuild(&self) -> rebuild::Rebuild<'_> {
        rebuild::Rebuild::new(
            &self.cwd,
            &self.args,
            &self.state_root,
            self.eg_binary.as_deref(),
        )
    }

    fn has_live_lease(&self) -> bool {
        let Ok(metadata) = fs::metadata(markers::lease_path(&self.state_root)) else {
            return false;
        };
        let Ok(modified) = metadata.modified() else {
            return false;
        };
        SystemTime::now()
            .duration_since(modified)
            .is_ok_and(|age| age <= lease_ttl())
    }

    fn has_index(&self) -> bool {
        let index = self.state_root.join(INDEX_DIR_NAME);
        [
            generations::POSTINGS_GENERATION,
            generations::TANTIVY_GENERATION,
        ]
        .iter()
        .any(|generation| {
            let published = index.join(generation);
            published.join(BINARY_MANIFEST_NAME).exists()
                || published.join(JSON_MANIFEST_NAME).exists()
        })
    }

    /// The reclaimer's view of this request's paths
    fn known_root(&self) -> reclaim::KnownRoot {
        reclaim::KnownRoot {
            index_root: self.index_root.clone(),
            state_root: self.state_root.clone(),
            lease_live: self.has_live_lease(),
        }
    }

    fn needs_refresh(&self) -> bool {
        let runtime = self.state_root.join(RUNTIME_DIR_NAME);
        let clean = runtime.join(JOURNAL_CLEAN_FILE_NAME);
        let wake = runtime.join(WAKE_FILE_NAME);
        let Ok(clean_modified) = fs::metadata(&clean).and_then(|meta| meta.modified()) else {
            return true;
        };
        fs::metadata(&wake)
            .and_then(|meta| meta.modified())
            .is_ok_and(|wake_modified| wake_modified > clean_modified)
    }

    fn can_serve_children(&self) -> bool {
        self.has_live_lease() && self.has_index() && !self.needs_refresh()
    }

    fn is_idle_child_of(&self, parent: &Self) -> bool {
        self.index_root != parent.index_root
            && !self.has_live_lease()
            && self.index_root.starts_with(&parent.index_root)
    }
}

fn lease_ttl() -> Duration {
    let value = env::var(LEASE_TTL_ENV).ok();
    lease_ttl_from(value.as_deref())
}

fn lease_ttl_from(value: Option<&str>) -> Duration {
    value
        .and_then(|value| value.parse::<u64>().ok())
        .map_or(DEFAULT_LEASE_TTL, Duration::from_secs)
}

fn read_watch_dirs(path: &Path, index_root: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let file = File::open(path)?;
    let mut dirs = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line?;
        if line.is_empty() {
            continue;
        }
        dirs.push(index_root.join(path_from_hex(&line)?));
    }
    Ok(dirs)
}

fn read_requests(runtime_root: &Path) -> anyhow::Result<Vec<Request>> {
    let requests = runtime_root.join(REQUESTS_DIR_NAME);
    let Ok(entries) = fs::read_dir(requests) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if !entry.file_type()?.is_file()
            || path.extension().and_then(|ext| ext.to_str()) != Some("request")
        {
            continue;
        }
        match read_request(&path) {
            Ok(Some(request)) => out.push(request),
            Ok(None) | Err(_) => quarantine_request(&path),
        }
    }
    Ok(out)
}

fn read_request(path: &Path) -> anyhow::Result<Option<Request>> {
    let file = File::open(path)?;
    let mut builder = RequestBuilder::default();
    for line in BufReader::new(file).lines() {
        builder.read_line(&line?)?;
    }
    Ok(builder.finish(path.to_path_buf()))
}

#[derive(Default)]
struct RequestBuilder {
    index_root: Option<PathBuf>,
    state_root: Option<PathBuf>,
    cwd: Option<PathBuf>,
    eg_binary: Option<PathBuf>,
    args: Vec<OsString>,
}

impl RequestBuilder {
    fn read_line(&mut self, line: &str) -> anyhow::Result<()> {
        if let Some(value) = line.strip_prefix("index_root=") {
            self.index_root = Some(path_from_hex(value)?);
        } else if let Some(value) = line.strip_prefix("state_root=") {
            self.state_root = Some(path_from_hex(value)?);
        } else if let Some(value) = line.strip_prefix("cwd=") {
            self.cwd = Some(path_from_hex(value)?);
        } else if let Some(value) = line.strip_prefix("eg_binary=") {
            self.eg_binary = Some(path_from_hex(value)?);
        } else if let Some(value) = line.strip_prefix("arg=") {
            self.args.push(os_string_from_bytes(hex_decode(value)?));
        }
        Ok(())
    }

    fn finish(self, path: PathBuf) -> Option<Request> {
        self.index_root
            .zip(self.state_root)
            .zip(self.cwd)
            .map(|((index_root, state_root), cwd)| Request {
                path,
                index_root,
                state_root,
                cwd,
                eg_binary: self.eg_binary,
                args: self.args,
            })
    }
}

fn path_from_hex(value: &str) -> anyhow::Result<PathBuf> {
    Ok(PathBuf::from(os_string_from_bytes(hex_decode(value)?)))
}

fn quarantine_request(path: &Path) {
    let stamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let Some(file_name) = path.file_name() else {
        return;
    };
    let quarantine = path.with_file_name(format!("{}.bad-{stamp}", file_name.to_string_lossy()));
    let _ = fs::rename(path, quarantine);
}

fn cleanup_state_root(state_root: &Path) -> anyhow::Result<()> {
    let runtime = state_root.join(RUNTIME_DIR_NAME);
    let index = state_root.join(INDEX_DIR_NAME);
    remove_path_if_exists(&index)
        .with_context(|| format!("failed to delete stale index {}", index.display()))?;
    for marker in [
        JOURNAL_CLEAN_FILE_NAME,
        WATCHER_READY_FILE_NAME,
        WATCH_REFUSED_FILE_NAME,
        OWNER_FILE_NAME,
        dirty::DIRTY_FILE_NAME,
    ] {
        let path = runtime.join(marker);
        remove_file_if_exists(&path)
            .with_context(|| format!("failed to remove daemon marker {}", path.display()))?;
    }
    Ok(())
}

fn remove_path_if_exists(path: &Path) -> std::io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

fn remove_file_if_exists(path: &Path) -> std::io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

enum StartupDisposition {
    Adopt,
    Discard,
}

fn startup_disposition(request: &Request) -> StartupDisposition {
    if request.index_root.is_dir() && request.has_live_lease() {
        StartupDisposition::Adopt
    } else {
        StartupDisposition::Discard
    }
}

fn adopt_request(request: &Request) {
    markers::clear_journal_clean(&request.state_root);
    markers::clear_watch_refusal(&request.state_root);
}

fn discard_state(request: &Request) {
    let _ = cleanup_state_root(&request.state_root);
}

fn discard_request(request: &Request) {
    discard_state(request);
    let _ = remove_file_if_exists(&request.path);
}

fn consolidate_children(requests: &[Request]) {
    for parent in requests
        .iter()
        .filter(|request| request.can_serve_children())
    {
        for child in requests
            .iter()
            .filter(|child| child.is_idle_child_of(parent))
        {
            remove_consolidated_child_index(child);
        }
    }
}

fn remove_consolidated_child_index(child: &Request) {
    let index = child.state_root.join(INDEX_DIR_NAME);
    if let Err(err) = remove_path_if_exists(&index) {
        log::debug!(
            "eg-indexd: failed to consolidate {}: {err}",
            index.display()
        );
    }
}

#[cfg(unix)]
fn os_string_from_bytes(bytes: Vec<u8>) -> OsString {
    use std::os::unix::ffi::OsStringExt;
    OsString::from_vec(bytes)
}

#[cfg(not(unix))]
fn os_string_from_bytes(bytes: Vec<u8>) -> OsString {
    OsString::from(String::from_utf8_lossy(&bytes).into_owned())
}

fn hex_decode(hex: &str) -> anyhow::Result<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        anyhow::bail!("hex field has odd length");
    }
    let mut out = Vec::with_capacity(hex.len() / 2);
    let bytes = hex.as_bytes();
    for pair in bytes.chunks_exact(2) {
        let high = hex_nibble(pair[0])?;
        let low = hex_nibble(pair[1])?;
        out.push((high << 4) | low);
    }
    Ok(out)
}

fn hex_nibble(byte: u8) -> anyhow::Result<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => anyhow::bail!("invalid hex field"),
    }
}

#[cfg(test)]
mod tests {
    use super::markers::{LEASE_FILE_NAME, lease_path, mark_lease_live, watcher_ready_path};
    use super::{
        DEFAULT_LEASE_TTL, Daemon, File, JOURNAL_CLEAN_FILE_NAME, QueryTouch, RUNTIME_DIR_NAME,
        Request, StartupDisposition, WATCH_DIRS_FILE_NAME, WATCH_REFUSED_FILE_NAME,
        WATCHER_READY_FILE_NAME, WatchBudget, WatchOutcome, WatchTenure, adopt_request,
        consolidate_children, discard_request, discard_state, lease_ttl_from, read_request,
        read_requests, startup_disposition,
    };
    use super::{dirty, markers};
    use std::{
        ffi::OsString,
        fs,
        path::{Path, PathBuf},
        time::Duration,
    };

    fn scratch(name: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("eg-indexd-{name}-"))
            .tempdir()
            .expect("scratch dir")
    }

    fn request_for(state_root: &Path) -> Request {
        Request {
            path: state_root.join("request.request"),
            index_root: state_root.to_path_buf(),
            state_root: state_root.to_path_buf(),
            cwd: state_root.to_path_buf(),
            eg_binary: None,
            args: Vec::new(),
        }
    }

    fn write_request(path: &Path, index_root: &Path, state_root: &Path) {
        fs::write(
            path,
            format!(
                "\
cwd=2f746d70
index_root={}
state_root={}
",
                hex_path(index_root),
                hex_path(state_root)
            ),
        )
        .expect("request");
    }

    fn hex_path(path: &Path) -> String {
        hex_bytes(path.as_os_str().as_encoded_bytes())
    }

    fn hex_bytes(bytes: &[u8]) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut out = String::with_capacity(bytes.len() * 2);
        for &byte in bytes {
            out.push(char::from(HEX[usize::from(byte >> 4)]));
            out.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        out
    }

    #[test]
    fn request_decodes_cwd_and_replay_args() {
        let root_guard = scratch("request");
        let root = root_guard.path().to_path_buf();
        let request = root.join("entry.request");
        fs::write(
            &request,
            "\
cwd=2f746d702f65672d637764
eg_binary=2f746d702f6567
index_root=2f746d702f65672d726f6f74
state_root=2f746d702f65672d7374617465
arg=2d2d696e6465783d6175746f
arg=6e6565646c65
",
        )
        .expect("request");

        let request = read_request(&request).expect("read").expect("request");
        assert_eq!(PathBuf::from("/tmp/eg-cwd"), request.cwd);
        assert_eq!(PathBuf::from("/tmp/eg-root"), request.index_root);
        assert_eq!(PathBuf::from("/tmp/eg-state"), request.state_root);
        assert_eq!(Some(PathBuf::from("/tmp/eg")), request.eg_binary);
        assert_eq!(
            vec![OsString::from("--index=auto"), OsString::from("needle")],
            request.args
        );
    }

    #[test]
    fn default_maintenance_ttl_is_one_day() {
        assert_eq!(DEFAULT_LEASE_TTL, Duration::from_hours(24));
        assert_eq!(lease_ttl_from(None), DEFAULT_LEASE_TTL);
    }

    #[test]
    fn maintenance_ttl_override_uses_seconds() {
        assert_eq!(lease_ttl_from(Some("5")), Duration::from_secs(5));
        assert_eq!(lease_ttl_from(Some("nope")), DEFAULT_LEASE_TTL);
    }

    #[test]
    fn mark_lease_live_updates_runtime_lease() {
        let root_guard = scratch("lease-live");
        let root = root_guard.path().to_path_buf();
        let lease = root.join(RUNTIME_DIR_NAME).join(LEASE_FILE_NAME);

        mark_lease_live(&root).expect("mark lease");

        assert!(lease.exists());
    }

    #[test]
    fn startup_adopts_live_rooted_requests() {
        let root_guard = scratch("adopt");
        let root = root_guard.path().to_path_buf();
        let request = request_for(&root);
        let runtime = root.join(RUNTIME_DIR_NAME);
        fs::create_dir_all(&runtime).expect("runtime");
        fs::write(runtime.join(LEASE_FILE_NAME), "lease").expect("lease");
        fs::write(runtime.join(JOURNAL_CLEAN_FILE_NAME), "clean").expect("clean");

        assert!(matches!(
            startup_disposition(&request),
            StartupDisposition::Adopt
        ));
        adopt_request(&request);
        assert!(!runtime.join(JOURNAL_CLEAN_FILE_NAME).exists());
        assert!(runtime.join(LEASE_FILE_NAME).exists());
    }

    #[test]
    fn startup_discards_requests_for_missing_roots() {
        let root_guard = scratch("discard-root");
        let root = root_guard.path().to_path_buf();
        let mut request = request_for(&root);
        request.index_root = root.join("gone");
        let runtime = root.join(RUNTIME_DIR_NAME);
        fs::create_dir_all(&runtime).expect("runtime");
        fs::write(runtime.join(LEASE_FILE_NAME), "lease").expect("lease");
        fs::write(&request.path, "stale").expect("request file");

        assert!(matches!(
            startup_disposition(&request),
            StartupDisposition::Discard
        ));
        discard_state(&request);
        assert!(request.path.exists());
        assert!(!runtime.join(JOURNAL_CLEAN_FILE_NAME).exists());
    }

    #[test]
    fn startup_discards_expired_leases() {
        let root_guard = scratch("discard-lease");
        let root = root_guard.path().to_path_buf();
        let request = request_for(&root);

        assert!(matches!(
            startup_disposition(&request),
            StartupDisposition::Discard
        ));
    }

    #[test]
    fn wake_newer_than_clean_requests_refresh() {
        let root_guard = scratch("wake");
        let root = root_guard.path().to_path_buf();
        let request = request_for(&root);
        let runtime = root.join("runtime");
        fs::create_dir_all(&runtime).expect("runtime");
        fs::write(runtime.join("journal-clean"), "clean").expect("clean");
        std::thread::sleep(Duration::from_millis(5));
        fs::write(runtime.join("wake"), "wake").expect("wake");

        assert!(request.needs_refresh());
    }

    #[test]
    fn clean_newer_than_wake_skips_refresh() {
        let root_guard = scratch("clean");
        let root = root_guard.path().to_path_buf();
        let request = request_for(&root);
        let runtime = root.join("runtime");
        fs::create_dir_all(&runtime).expect("runtime");
        fs::write(runtime.join("wake"), "wake").expect("wake");
        std::thread::sleep(Duration::from_millis(5));
        fs::write(runtime.join("journal-clean"), "clean").expect("clean");

        assert!(!request.needs_refresh());
    }

    #[test]
    fn malformed_request_is_quarantined_and_ignored() {
        let root_guard = scratch("bad-request");
        let root = root_guard.path().to_path_buf();
        let requests = root.join("requests");
        fs::create_dir_all(&requests).expect("requests");
        let request = requests.join("bad.request");
        fs::write(
            &request,
            "cwd=not-hex\nindex_root=2f746d70\nstate_root=2f746d70\n",
        )
        .expect("bad request");

        let parsed = read_requests(&root).expect("read requests");

        assert!(parsed.is_empty());
        assert!(!request.exists());
        assert!(
            fs::read_dir(&requests)
                .expect("request dir")
                .any(|entry| entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .contains(".bad-")),
            "malformed request should be quarantined"
        );
    }

    #[test]
    fn dead_request_discard_removes_state_and_request_file() {
        let root_guard = scratch("startup-cleanup");
        let root = root_guard.path().to_path_buf();
        let state = root.join("state");
        let request_path = root.join("entry.request");
        fs::create_dir_all(state.join("index")).expect("index");
        fs::write(&request_path, "request").expect("request");
        let request = Request {
            path: request_path.clone(),
            ..request_for(&state)
        };

        assert!(matches!(
            startup_disposition(&request),
            StartupDisposition::Discard
        ));
        discard_request(&request);

        assert!(!state.join("index").exists());
        assert!(!request_path.exists());
    }

    fn corpus_with_index(root: &Path, name: &str, live: bool) -> PathBuf {
        let corpus = root.join(name);
        fs::create_dir_all(corpus.join("index")).expect("index");
        fs::write(corpus.join("index/data"), "data").expect("data");
        if live {
            let runtime = corpus.join(RUNTIME_DIR_NAME);
            fs::create_dir_all(&runtime).expect("runtime");
            fs::write(runtime.join(LEASE_FILE_NAME), "lease").expect("lease");
            fs::write(runtime.join(JOURNAL_CLEAN_FILE_NAME), "clean").expect("clean");
        }
        write_request(
            &root.join(format!("requests/{name}.request")),
            &corpus,
            &corpus,
        );
        corpus
    }

    #[test]
    fn startup_discards_dead_requests_and_adopts_live_ones() {
        let root_guard = scratch("startup-all-clean");
        let root = root_guard.path().to_path_buf();
        fs::create_dir_all(root.join("requests")).expect("requests");
        let dead = corpus_with_index(&root, "dead", false);
        let live = corpus_with_index(&root, "live", true);
        let daemon = Daemon::new(root.clone(), "owner".to_owned()).expect("daemon");

        daemon.prepare_startup().expect("startup");

        assert!(!dead.join("index").exists());
        assert!(root.join("requests/dead.request").exists());
        assert!(live.join("index/data").exists());
        assert!(root.join("requests/live.request").exists());
        assert!(
            !live
                .join(RUNTIME_DIR_NAME)
                .join(JOURNAL_CLEAN_FILE_NAME)
                .exists()
        );
        assert!(root.join("startup-ready").exists());
    }

    #[cfg(unix)]
    #[test]
    fn startup_survives_undeletable_state() {
        use std::os::unix::fs::PermissionsExt;

        let root_guard = scratch("startup-clean-fail");
        let root = root_guard.path().to_path_buf();
        let requests = root.join("requests");
        let state = root.join("state");
        fs::create_dir_all(&requests).expect("requests");
        fs::create_dir_all(state.join("index")).expect("index");
        write_request(&requests.join("entry.request"), &state, &state);
        fs::set_permissions(&state, fs::Permissions::from_mode(0o555)).expect("readonly");
        let daemon = Daemon::new(root.clone(), "owner".to_owned()).expect("daemon");

        let result = daemon.prepare_startup();

        fs::set_permissions(&state, fs::Permissions::from_mode(0o755)).expect("writable");
        assert!(result.is_ok());
        assert!(root.join("startup-ready").exists());
    }

    #[test]
    fn graceful_cleanup_removes_index_markers_and_request_file() {
        let root_guard = scratch("graceful-cleanup");
        let root = root_guard.path().to_path_buf();
        let state = root.join("state");
        let runtime = state.join("runtime");
        let request_path = root.join("entry.request");
        fs::create_dir_all(state.join("index")).expect("index");
        fs::create_dir_all(&runtime).expect("runtime");
        fs::write(runtime.join("journal-clean"), "clean").expect("clean");
        fs::write(runtime.join("watcher-ready"), "ready").expect("ready");
        fs::write(runtime.join("daemon-owner"), "owner").expect("owner");
        fs::write(root.join("startup-ready"), "owner").expect("startup ready");
        fs::write(&request_path, "request").expect("request");
        let request = Request {
            path: request_path.clone(),
            ..request_for(&state)
        };
        let daemon = Daemon::new(root.clone(), "owner".to_owned()).expect("daemon");

        daemon.cleanup_requests(&[request]).expect("cleanup");

        assert!(!state.join("index").exists());
        assert!(!runtime.join("journal-clean").exists());
        assert!(!runtime.join("watcher-ready").exists());
        assert!(!runtime.join("daemon-owner").exists());
        assert!(!request_path.exists());
        assert!(!root.join("startup-ready").exists());
    }

    #[test]
    fn child_consolidation_requires_clean_live_parent() {
        let root_guard = scratch("consolidate");
        let root = root_guard.path().to_path_buf();
        let parent_root = root.join("repo");
        let child_root = parent_root.join("src");
        let parent_state = root.join("parent-state");
        let child_state = root.join("child-state");
        fs::create_dir_all(parent_state.join("index/postings-v9")).expect("parent index");
        fs::create_dir_all(child_state.join("index")).expect("child index");
        fs::write(parent_state.join("index/postings-v9/manifest.json"), "{}").expect("manifest");
        let mut parent = request_for(&parent_state);
        parent.index_root = parent_root;
        let mut child = request_for(&child_state);
        child.index_root = child_root;

        consolidate_children(&[parent.clone(), child.clone()]);
        assert!(child_state.join("index").exists());

        let runtime = parent_state.join("runtime");
        fs::create_dir_all(&runtime).expect("runtime");
        fs::write(runtime.join("lease"), "lease").expect("lease");
        fs::write(runtime.join("journal-clean"), "clean").expect("clean");
        consolidate_children(&[parent, child]);

        assert!(!child_state.join("index").exists());
    }

    #[test]
    fn child_consolidation_keeps_live_child_index() {
        let root_guard = scratch("consolidate-live-child");
        let root = root_guard.path().to_path_buf();
        let parent_root = root.join("repo");
        let child_root = parent_root.join("src");
        let parent_state = root.join("parent-state");
        let child_state = root.join("child-state");
        fs::create_dir_all(parent_state.join("index/postings-v9")).expect("parent index");
        fs::create_dir_all(child_state.join("index")).expect("child index");
        fs::write(parent_state.join("index/postings-v9/manifest.json"), "{}").expect("manifest");
        for state in [&parent_state, &child_state] {
            let runtime = state.join("runtime");
            fs::create_dir_all(&runtime).expect("runtime");
            fs::write(runtime.join("lease"), "lease").expect("lease");
            fs::write(runtime.join("journal-clean"), "clean").expect("clean");
        }
        let mut parent = request_for(&parent_state);
        parent.index_root = parent_root;
        let mut child = request_for(&child_state);
        child.index_root = child_root;

        consolidate_children(&[parent, child]);

        assert!(child_state.join("index").exists());
    }

    /// Corpus root holding `depth` nested subdirectories and a live lease
    fn leased_corpus(root: &Path, depth: usize) -> Request {
        let mut nested = root.to_path_buf();
        for level in 0..depth {
            nested = nested.join(format!("level{level}"));
        }
        fs::create_dir_all(&nested).expect("nested");
        let state = root.join(".eg");
        let runtime = state.join(RUNTIME_DIR_NAME);
        fs::create_dir_all(&runtime).expect("runtime");
        fs::write(runtime.join(LEASE_FILE_NAME), "lease").expect("lease");
        Request {
            path: root.join("entry.request"),
            index_root: root.to_path_buf(),
            state_root: state,
            cwd: root.to_path_buf(),
            eg_binary: None,
            args: Vec::new(),
        }
    }

    fn budgeted_daemon(runtime_root: &Path, ceiling: usize) -> Daemon {
        Daemon::with_budget(
            runtime_root.to_path_buf(),
            "owner".to_owned(),
            WatchBudget::with_ceiling(ceiling),
        )
        .expect("daemon")
    }

    #[test]
    fn a_tree_over_the_watch_budget_loses_its_freshness_proof() {
        let root_guard = scratch("watch-budget");
        let root = root_guard.path().to_path_buf();
        let request = leased_corpus(&root.join("corpus"), 6);
        let runtime = request.state_root.join(RUNTIME_DIR_NAME);
        fs::write(runtime.join(WATCHER_READY_FILE_NAME), "stale").expect("stale ready");
        let mut daemon = budgeted_daemon(&root, 3);

        let outcome = daemon.watch_request(&request).expect("watch request");

        assert_eq!(WatchOutcome::Exhausted, outcome);
        assert!(!runtime.join(WATCHER_READY_FILE_NAME).exists());
        let reason = fs::read_to_string(runtime.join(WATCH_REFUSED_FILE_NAME)).expect("refusal");
        assert!(reason.contains("filesystem watches"), "{reason}");
        assert_eq!(0, daemon.watcher.held());
    }

    #[test]
    fn a_watched_tree_clears_an_earlier_refusal() {
        let root_guard = scratch("watch-recover");
        let root = root_guard.path().to_path_buf();
        let request = leased_corpus(&root.join("corpus"), 2);
        let runtime = request.state_root.join(RUNTIME_DIR_NAME);
        fs::write(runtime.join(WATCH_REFUSED_FILE_NAME), "stale refusal").expect("refusal");
        let mut daemon = budgeted_daemon(&root, 64);

        let outcome = daemon.watch_request(&request).expect("watch request");

        assert_eq!(WatchOutcome::Watching, outcome);
        assert!(runtime.join(WATCHER_READY_FILE_NAME).exists());
        assert!(!runtime.join(WATCH_REFUSED_FILE_NAME).exists());
    }

    #[test]
    fn a_refused_tree_is_retried_only_after_watches_come_back() {
        let root_guard = scratch("watch-backoff");
        let root = root_guard.path().to_path_buf();
        let holder = leased_corpus(&root.join("holder"), 2);
        let refused = leased_corpus(&root.join("refused"), 4);
        let mut daemon = budgeted_daemon(&root, 4);
        daemon.watch_request(&holder).expect("holder");

        assert_eq!(
            WatchOutcome::Exhausted,
            daemon.watch_request(&refused).expect("refused")
        );
        assert!(daemon.is_backed_off(&refused.state_root));

        daemon.watcher.release_tree(&holder.state_root);

        assert!(!daemon.is_backed_off(&refused.state_root));
    }

    #[test]
    fn a_lapsed_lease_returns_its_watches() {
        let root_guard = scratch("watch-lapse");
        let root = root_guard.path().to_path_buf();
        let kept = leased_corpus(&root.join("kept"), 2);
        let lapsed = leased_corpus(&root.join("lapsed"), 2);
        let mut daemon = budgeted_daemon(&root, 64);
        daemon.watch_request(&kept).expect("kept");
        daemon.watch_request(&lapsed).expect("lapsed");
        let held_with_both = daemon.watcher.held();
        let lapsed_runtime = lapsed.state_root.join(RUNTIME_DIR_NAME);
        fs::remove_file(lapsed_runtime.join(LEASE_FILE_NAME)).expect("expire lease");

        daemon.release_unleased(&[kept.clone(), lapsed.clone()]);

        assert!(daemon.watcher.held() < held_with_both);
        assert!(daemon.watcher.watches_tree(&kept.state_root));
        assert!(!daemon.watcher.watches_tree(&lapsed.state_root));
        assert!(!lapsed_runtime.join(WATCHER_READY_FILE_NAME).exists());
    }

    /// Leased corpus whose daemon request carries a query touch of now
    fn queried_corpus(root: &Path, depth: usize) -> Request {
        let request = leased_corpus(root, depth);
        fs::write(&request.path, "request").expect("request");
        std::thread::sleep(Duration::from_millis(10));
        request
    }

    fn evicting_daemon(runtime_root: &Path, ceiling: usize) -> Daemon {
        let mut daemon = budgeted_daemon(runtime_root, ceiling);
        daemon.tenure = WatchTenure::with_min_hold(Duration::ZERO);
        daemon
    }

    /// Serve one request the way a poll does, reading its query touch first
    fn watch_query(daemon: &mut Daemon, request: &Request) -> WatchOutcome {
        let touch = QueryTouch::of_request(&request.path).expect("query touch");
        daemon.tenure.observe(&request.state_root, touch);
        daemon.watch_request(request).expect("watch request")
    }

    /// Publish the walked directory list the daemon scopes its watches by
    fn publish_watch_dirs(request: &Request, dirs: &[&str]) {
        let runtime = request.state_root.join(RUNTIME_DIR_NAME);
        fs::create_dir_all(&runtime).expect("runtime");
        let lines: Vec<String> = dirs.iter().map(|dir| hex_bytes(dir.as_bytes())).collect();
        fs::write(runtime.join(WATCH_DIRS_FILE_NAME), lines.join("\n")).expect("watch dirs");
    }

    #[test]
    fn a_more_recently_queried_tree_takes_the_least_recently_used_watches() {
        let root_guard = scratch("evict-lru");
        let root = root_guard.path().to_path_buf();
        let idle = queried_corpus(&root.join("idle"), 1);
        let busy = queried_corpus(&root.join("busy"), 1);
        let wanting = queried_corpus(&root.join("wanting"), 1);
        let mut daemon = evicting_daemon(&root, 5);
        assert_eq!(WatchOutcome::Watching, watch_query(&mut daemon, &idle));
        assert_eq!(WatchOutcome::Watching, watch_query(&mut daemon, &busy));

        assert_eq!(WatchOutcome::Watching, watch_query(&mut daemon, &wanting));

        assert!(!daemon.watcher.watches_tree(&idle.state_root));
        assert!(daemon.watcher.watches_tree(&busy.state_root));
        assert!(daemon.watcher.watches_tree(&wanting.state_root));
        let refusal = fs::read_to_string(
            idle.state_root
                .join(RUNTIME_DIR_NAME)
                .join(WATCH_REFUSED_FILE_NAME),
        )
        .expect("refusal");
        assert!(refusal.contains("queried more recently"), "{refusal}");
    }

    #[test]
    fn an_evicted_tree_never_proves_freshness_without_its_watches_back() {
        let root_guard = scratch("evict-proof");
        let root = root_guard.path().to_path_buf();
        let idle = queried_corpus(&root.join("idle"), 1);
        let wanting = queried_corpus(&root.join("wanting"), 1);
        publish_watch_dirs(&idle, &["level0"]);
        let mut daemon = evicting_daemon(&root, 3);
        assert_eq!(WatchOutcome::Watching, watch_query(&mut daemon, &idle));
        assert!(watcher_ready_path(&idle.state_root).exists());

        assert_eq!(WatchOutcome::Watching, watch_query(&mut daemon, &wanting));

        assert!(!watcher_ready_path(&idle.state_root).exists());
        assert!(!daemon.watcher.watches_tree(&idle.state_root));
        // the directory list is unchanged, so only real watches restore the proof
        assert_eq!(WatchOutcome::Exhausted, watch_query(&mut daemon, &idle));
        assert!(!watcher_ready_path(&idle.state_root).exists());
        assert!(!daemon.watcher.watches_tree(&idle.state_root));
    }

    /// Change set the daemon publishes for one tree right now
    fn published_change_set(daemon: &mut Daemon, request: &Request) -> String {
        daemon.publish_dirt(std::slice::from_ref(request));
        fs::read_to_string(
            request
                .state_root
                .join(RUNTIME_DIR_NAME)
                .join(dirty::DIRTY_FILE_NAME),
        )
        .expect("change set")
    }

    #[test]
    fn an_evicted_tree_names_no_bounded_change_set_once_it_is_watched_again() {
        let root_guard = scratch("evict-dirt");
        let root = root_guard.path().to_path_buf();
        let idle = queried_corpus(&root.join("idle"), 1);
        let wanting = queried_corpus(&root.join("wanting"), 1);
        let mut daemon = evicting_daemon(&root, 3);
        assert_eq!(WatchOutcome::Watching, watch_query(&mut daemon, &idle));
        daemon.dirt.begin_build(&idle.state_root);
        daemon.dirt.commit_build(&idle.state_root);
        let bounded = published_change_set(&mut daemon, &idle);
        assert!(!bounded.contains(dirty::UNBOUNDED_MARKER), "{bounded}");

        assert_eq!(WatchOutcome::Watching, watch_query(&mut daemon, &wanting));
        daemon.evict(&wanting.state_root);
        assert_eq!(WatchOutcome::Watching, watch_query(&mut daemon, &idle));

        let after = published_change_set(&mut daemon, &idle);
        assert!(after.contains(dirty::UNBOUNDED_MARKER), "{after}");
    }

    #[test]
    fn an_evicted_tree_loses_the_freshness_proof_it_already_earned() {
        let root_guard = scratch("evict-clean");
        let root = root_guard.path().to_path_buf();
        let idle = queried_corpus(&root.join("idle"), 1);
        let wanting = queried_corpus(&root.join("wanting"), 1);
        let mut daemon = evicting_daemon(&root, 3);
        assert_eq!(WatchOutcome::Watching, watch_query(&mut daemon, &idle));
        markers::mark_journal_clean(&idle.state_root).expect("proof");

        assert_eq!(WatchOutcome::Watching, watch_query(&mut daemon, &wanting));

        assert!(
            !idle
                .state_root
                .join(RUNTIME_DIR_NAME)
                .join(JOURNAL_CLEAN_FILE_NAME)
                .exists()
        );
    }

    #[test]
    fn a_tree_whose_lease_a_query_holds_is_never_evicted() {
        let root_guard = scratch("evict-leased");
        let root = root_guard.path().to_path_buf();
        let idle = queried_corpus(&root.join("idle"), 1);
        let wanting = queried_corpus(&root.join("wanting"), 1);
        let mut daemon = evicting_daemon(&root, 3);
        assert_eq!(WatchOutcome::Watching, watch_query(&mut daemon, &idle));
        let hold = File::open(lease_path(&idle.state_root)).expect("lease");
        hold.try_lock_shared().expect("hold the lease");

        assert_eq!(WatchOutcome::Exhausted, watch_query(&mut daemon, &wanting));

        assert!(daemon.watcher.watches_tree(&idle.state_root));
        assert!(watcher_ready_path(&idle.state_root).exists());
        assert!(!daemon.watcher.watches_tree(&wanting.state_root));

        drop(hold);

        assert_eq!(WatchOutcome::Watching, watch_query(&mut daemon, &wanting));
        assert!(!daemon.watcher.watches_tree(&idle.state_root));
    }

    #[test]
    fn a_tree_queried_before_every_holder_is_refused_rather_than_served() {
        let root_guard = scratch("evict-order");
        let root = root_guard.path().to_path_buf();
        let wanting = queried_corpus(&root.join("wanting"), 1);
        let busy = queried_corpus(&root.join("busy"), 1);
        let mut daemon = evicting_daemon(&root, 3);
        assert_eq!(WatchOutcome::Watching, watch_query(&mut daemon, &busy));

        assert_eq!(WatchOutcome::Exhausted, watch_query(&mut daemon, &wanting));

        assert!(daemon.watcher.watches_tree(&busy.state_root));
        assert!(!daemon.watcher.watches_tree(&wanting.state_root));
    }

    #[test]
    fn a_freshly_watched_tree_keeps_its_watches_through_the_minimum_hold() {
        let root_guard = scratch("evict-hold");
        let root = root_guard.path().to_path_buf();
        let idle = queried_corpus(&root.join("idle"), 1);
        let wanting = queried_corpus(&root.join("wanting"), 1);
        let mut daemon = budgeted_daemon(&root, 3);
        assert_eq!(WatchOutcome::Watching, watch_query(&mut daemon, &idle));

        assert_eq!(WatchOutcome::Exhausted, watch_query(&mut daemon, &wanting));

        assert!(daemon.watcher.watches_tree(&idle.state_root));
        assert!(!daemon.watcher.watches_tree(&wanting.state_root));
    }
}
