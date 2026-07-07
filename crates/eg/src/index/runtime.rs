//! File-based runtime coordination for indexed search.

use std::{
    env,
    ffi::OsString,
    fs::{self, OpenOptions, TryLockError},
    io::{self, ErrorKind, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::OnceLock,
    time::{Duration, SystemTime},
};

const RUNTIME_DIR_NAME: &str = "runtime";
const LEASE_FILE_NAME: &str = "lease";
const WAKE_FILE_NAME: &str = "wake";
const WATCHER_READY_FILE_NAME: &str = "watcher-ready";
const JOURNAL_CLEAN_FILE_NAME: &str = "journal-clean";
const OWNER_FILE_NAME: &str = "daemon-owner";
const REQUESTS_DIR_NAME: &str = "requests";
const STARTUP_READY_FILE_NAME: &str = "startup-ready";
const LOCK_FILE_NAME: &str = "daemon.lock";
const DAEMON_BINARY_NAME: &str = "eg-indexd";
const DAEMON_REFRESH_ENV: &str = "EG_INDEX_DAEMON_REFRESH";
const DISABLE_DAEMON_AUTOSPAWN_ENV: &str = "EG_INDEXD_DISABLE_AUTOSPAWN";
const LEASE_TTL_ENV: &str = "EG_INDEXD_LEASE_TTL_SECS";
const DEFAULT_LEASE_TTL: Duration = Duration::from_hours(24);
const DAEMON_STARTUP_WAIT: Duration = Duration::from_secs(5);
const INDEX_READY_POLL: Duration = Duration::from_millis(50);
static GLOBAL_RUNTIME_ROOT: OnceLock<PathBuf> = OnceLock::new();

pub struct Lease<'a> {
    index_root: &'a Path,
    state_root: &'a Path,
}

impl<'a> Lease<'a> {
    pub const fn new(index_root: &'a Path, state_root: &'a Path) -> Self {
        Self {
            index_root,
            state_root,
        }
    }

    pub fn request_refresh(&self) -> io::Result<()> {
        request_refresh(self.index_root, self.state_root)
    }

    pub fn keep_alive_best_effort(&self) {
        keep_alive_best_effort(self.index_root, self.state_root);
    }
}

pub fn keep_alive_best_effort(index_root: &Path, state_root: &Path) {
    register_best_effort(index_root, state_root, false, false);
}

pub fn request_refresh(index_root: &Path, state_root: &Path) -> io::Result<()> {
    register_required(index_root, state_root, true, true)
}

pub enum ProofWait {
    Ready,
    DaemonStopped,
    TimedOut,
}

pub fn wait_for_freshness_proof(state_root: &Path, timeout: Duration) -> ProofWait {
    let started = std::time::Instant::now();
    while started.elapsed() <= timeout {
        if daemon_freshness_proof(state_root) {
            return ProofWait::Ready;
        }
        if !daemon_running() {
            return ProofWait::DaemonStopped;
        }
        std::thread::sleep(INDEX_READY_POLL);
    }
    if daemon_freshness_proof(state_root) {
        ProofWait::Ready
    } else {
        ProofWait::TimedOut
    }
}

fn register_best_effort(index_root: &Path, state_root: &Path, wake: bool, durable: bool) {
    let _ = register_required(index_root, state_root, wake, durable);
}

fn register_required(
    index_root: &Path,
    state_root: &Path,
    wake: bool,
    durable: bool,
) -> io::Result<()> {
    let runtime = runtime_dir(state_root);
    fs::create_dir_all(&runtime)?;
    touch_lease(state_root, durable)?;
    register(
        index_root,
        state_root,
        env::current_dir(),
        env::current_exe(),
        env::args_os().skip(1),
        durable,
    )?;
    if wake {
        write_marker(&runtime.join(WAKE_FILE_NAME))?;
        ensure_daemon()?;
    }
    Ok(())
}

pub fn is_daemon_refresh() -> bool {
    env::var_os(DAEMON_REFRESH_ENV).is_some()
}

pub fn daemon_running() -> bool {
    live_daemon_owner(&global_runtime_root()).is_some()
}

pub fn daemon_autospawn_disabled() -> bool {
    env::var_os(DISABLE_DAEMON_AUTOSPAWN_ENV).is_some()
}

pub const fn daemon_watch_supported() -> bool {
    cfg!(target_os = "linux")
}

pub fn daemon_freshness_proof(state_root: &Path) -> bool {
    daemon_freshness_proof_in(state_root, &global_runtime_root())
}

fn daemon_freshness_proof_in(state_root: &Path, global_runtime: &Path) -> bool {
    let runtime = runtime_dir(state_root);
    if !startup_ready(global_runtime) || !owner_matches_live_daemon(&runtime, global_runtime) {
        return false;
    }
    if !runtime.join(WATCHER_READY_FILE_NAME).exists() {
        return false;
    }
    let Ok(clean_modified) =
        fs::metadata(runtime.join(JOURNAL_CLEAN_FILE_NAME)).and_then(|meta| meta.modified())
    else {
        return false;
    };
    if fs::metadata(runtime.join(WAKE_FILE_NAME))
        .and_then(|meta| meta.modified())
        .is_ok_and(|wake_modified| wake_modified > clean_modified)
    {
        return false;
    }
    let Ok(metadata) = fs::metadata(runtime.join(LEASE_FILE_NAME)) else {
        return false;
    };
    let Ok(modified) = metadata.modified() else {
        return false;
    };
    SystemTime::now()
        .duration_since(modified)
        .is_ok_and(|age| age <= lease_ttl())
}

pub fn mark_journal_clean(state_root: &Path) -> std::io::Result<()> {
    let runtime = runtime_dir(state_root);
    fs::create_dir_all(&runtime)?;
    write_marker(&runtime.join(JOURNAL_CLEAN_FILE_NAME))
}

pub fn clear_journal_clean(state_root: &Path) {
    let _ = fs::remove_file(runtime_dir(state_root).join(JOURNAL_CLEAN_FILE_NAME));
}

fn ensure_daemon() -> io::Result<()> {
    if env::var_os(DISABLE_DAEMON_AUTOSPAWN_ENV).is_some() {
        return Ok(());
    }
    let runtime_root = global_runtime_root();
    if startup_ready(&runtime_root) {
        return Ok(());
    }
    let Some(source) = daemon_source_binary() else {
        return Err(io::Error::new(
            ErrorKind::NotFound,
            "eg-indexd binary was not found next to eg",
        ));
    };
    fs::create_dir_all(&runtime_root)?;
    let binary = install_daemon_binary(&source, &runtime_root)?;
    Command::new(binary)
        .arg("--runtime-root")
        .arg(runtime_root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    if wait_for_startup_ready(DAEMON_STARTUP_WAIT) {
        Ok(())
    } else {
        Err(io::Error::new(
            ErrorKind::TimedOut,
            "eg-indexd did not report startup readiness",
        ))
    }
}

fn daemon_source_binary() -> Option<PathBuf> {
    let current = env::current_exe().ok()?;
    let dir = current.parent()?;
    let binary = dir.join(DAEMON_BINARY_NAME);
    binary.exists().then_some(binary)
}

fn install_daemon_binary(source: &Path, runtime_root: &Path) -> std::io::Result<PathBuf> {
    let dest_dir = runtime_root.join("bin");
    fs::create_dir_all(&dest_dir)?;
    let dest = dest_dir.join(DAEMON_BINARY_NAME);
    if daemon_install_is_current(source, &dest)? {
        return Ok(dest);
    }

    let tmp = dest.with_extension(format!("tmp-{}", std::process::id()));
    fs::copy(source, &tmp)?;
    OpenOptions::new().read(true).open(&tmp)?.sync_all()?;
    fs::rename(tmp, &dest)?;
    Ok(dest)
}

fn daemon_install_is_current(source: &Path, dest: &Path) -> std::io::Result<bool> {
    let source = fs::metadata(source)?;
    let Ok(dest) = fs::metadata(dest) else {
        return Ok(false);
    };
    if source.len() != dest.len() {
        return Ok(false);
    }
    match (source.modified(), dest.modified()) {
        (Ok(source), Ok(dest)) => Ok(dest >= source),
        _ => Ok(true),
    }
}

fn register(
    index_root: &Path,
    state_root: &Path,
    cwd: std::io::Result<PathBuf>,
    eg_binary: std::io::Result<PathBuf>,
    args: impl IntoIterator<Item = OsString>,
    durable: bool,
) -> std::io::Result<()> {
    let requests = global_runtime_root().join(REQUESTS_DIR_NAME);
    fs::create_dir_all(&requests)?;
    let key = hash_paths(index_root, state_root);
    let request = requests.join(format!("{key:016x}.request"));
    let tmp = requests.join(format!("{key:016x}.{}.tmp", std::process::id()));
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&tmp)?;
    if let Ok(cwd) = cwd {
        writeln!(file, "cwd={}", hex_encode(os_bytes(cwd.into_os_string())))?;
    }
    if let Ok(eg_binary) = eg_binary {
        writeln!(
            file,
            "eg_binary={}",
            hex_encode(os_bytes(eg_binary.into_os_string()))
        )?;
    }
    writeln!(
        file,
        "index_root={}",
        hex_encode(os_bytes(index_root.as_os_str().to_os_string()))
    )?;
    writeln!(
        file,
        "state_root={}",
        hex_encode(os_bytes(state_root.as_os_str().to_os_string()))
    )?;
    for arg in args {
        writeln!(file, "arg={}", hex_encode(os_bytes(arg)))?;
    }
    if durable {
        file.sync_all()?;
    }
    drop(file);
    fs::rename(tmp, request)
}

fn global_runtime_root() -> PathBuf {
    GLOBAL_RUNTIME_ROOT
        .get_or_init(select_global_runtime_root)
        .clone()
}

fn select_global_runtime_root() -> PathBuf {
    if let Some(root) = env::var_os("XDG_RUNTIME_DIR").filter(|value| !value.is_empty()) {
        let candidate = PathBuf::from(root).join("eg");
        if runtime_root_is_writable(&candidate) {
            return candidate;
        }
    }
    let fallback = env::temp_dir().join("eg-runtime");
    let _ = fs::create_dir_all(&fallback);
    fallback
}

fn runtime_root_is_writable(path: &Path) -> bool {
    fn check(path: &Path) -> io::Result<()> {
        fs::create_dir_all(path)?;
        let stamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        let probe = path.join(format!(".write-check-{}-{stamp}", std::process::id()));
        OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&probe)?;
        fs::remove_file(probe)
    }
    check(path).is_ok()
}

fn startup_ready(global_runtime: &Path) -> bool {
    let Some(owner) = live_daemon_owner(global_runtime) else {
        return false;
    };
    fs::read_to_string(global_runtime.join(STARTUP_READY_FILE_NAME))
        .is_ok_and(|ready_owner| ready_owner.trim() == owner)
}

fn wait_for_startup_ready(timeout: Duration) -> bool {
    let started = std::time::Instant::now();
    while started.elapsed() <= timeout {
        if startup_ready(&global_runtime_root())
            || env::var_os(DISABLE_DAEMON_AUTOSPAWN_ENV).is_some()
        {
            return true;
        }
        std::thread::sleep(INDEX_READY_POLL);
    }
    false
}

fn runtime_dir(state_root: &Path) -> PathBuf {
    state_root.join(RUNTIME_DIR_NAME)
}

fn owner_matches_live_daemon(runtime: &Path, global_runtime: &Path) -> bool {
    let Ok(owner) = fs::read_to_string(runtime.join(OWNER_FILE_NAME)) else {
        return false;
    };
    let owner = owner.trim();
    if owner.is_empty() {
        return false;
    }
    live_daemon_owner(global_runtime).is_some_and(|live_owner| live_owner == owner)
}

fn live_daemon_owner(global_runtime: &Path) -> Option<String> {
    let Ok(file) = OpenOptions::new()
        .read(true)
        .open(global_runtime.join(LOCK_FILE_NAME))
    else {
        return None;
    };
    match file.try_lock_shared() {
        Ok(()) => None,
        Err(TryLockError::WouldBlock) => fs::read_to_string(global_runtime.join(LOCK_FILE_NAME))
            .ok()
            .map(|owner| owner.trim().to_owned())
            .filter(|owner| !owner.is_empty()),
        Err(TryLockError::Error(_)) => None,
    }
}

fn touch_lease(state_root: &Path, durable: bool) -> io::Result<()> {
    let runtime = runtime_dir(state_root);
    fs::create_dir_all(&runtime)?;
    write_marker_durable(&runtime.join(LEASE_FILE_NAME), durable)
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

fn write_marker(path: &Path) -> std::io::Result<()> {
    write_marker_durable(path, true)
}

fn write_marker_durable(path: &Path, durable: bool) -> std::io::Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)?;
    writeln!(file, "{}", std::process::id())?;
    if durable {
        file.sync_all()?;
    }
    Ok(())
}

#[cfg(unix)]
fn os_bytes(value: OsString) -> Vec<u8> {
    use std::os::unix::ffi::OsStringExt;
    value.into_vec()
}

#[cfg(not(unix))]
fn os_bytes(value: OsString) -> Vec<u8> {
    value.to_string_lossy().into_owned().into_bytes()
}

fn hex_encode(bytes: Vec<u8>) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    out
}

fn hash_paths(index_root: &Path, state_root: &Path) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in index_root
        .as_os_str()
        .to_string_lossy()
        .bytes()
        .chain([0])
        .chain(state_root.as_os_str().to_string_lossy().bytes())
    {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_LEASE_TTL, LEASE_FILE_NAME, LOCK_FILE_NAME, OWNER_FILE_NAME,
        STARTUP_READY_FILE_NAME, WAKE_FILE_NAME, daemon_freshness_proof_in, install_daemon_binary,
        keep_alive_best_effort, lease_ttl_from, request_refresh, runtime_root_is_writable,
    };
    use std::{fs, path::PathBuf};

    fn scratch(name: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("eg-runtime-{}-{stamp}-{name}", std::process::id()));
        fs::create_dir_all(&root).expect("scratch dir");
        root
    }

    fn live_daemon_proof(
        state_root: &std::path::Path,
        global_runtime: &std::path::Path,
    ) -> fs::File {
        fs::create_dir_all(global_runtime).expect("global runtime");
        fs::write(global_runtime.join(STARTUP_READY_FILE_NAME), "owner").expect("startup ready");
        let lock = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(true)
            .open(global_runtime.join(LOCK_FILE_NAME))
            .expect("lock");
        lock.try_lock().expect("hold daemon lock");
        fs::write(global_runtime.join(LOCK_FILE_NAME), "owner\n").expect("lock owner");
        let runtime = state_root.join("runtime");
        fs::create_dir_all(&runtime).expect("runtime");
        fs::write(runtime.join(OWNER_FILE_NAME), "owner\n").expect("owner");
        lock
    }

    #[test]
    fn proof_requires_watcher_ready_and_lease() {
        let root = scratch("proof");
        let global_runtime = scratch("proof-global");
        let _lock = live_daemon_proof(&root, &global_runtime);

        assert!(!daemon_freshness_proof_in(&root, &global_runtime));

        let runtime = root.join("runtime");
        fs::create_dir_all(&runtime).expect("runtime");
        fs::write(runtime.join(LEASE_FILE_NAME), "lease").expect("lease");
        fs::write(runtime.join(WAKE_FILE_NAME), "wake").expect("wake");
        assert!(!daemon_freshness_proof_in(&root, &global_runtime));

        fs::write(runtime.join("watcher-ready"), "ready").expect("ready");
        assert!(!daemon_freshness_proof_in(&root, &global_runtime));

        fs::write(runtime.join("journal-clean"), "clean").expect("clean");
        assert!(daemon_freshness_proof_in(&root, &global_runtime));
    }

    #[test]
    fn default_lease_ttl_is_one_day() {
        assert_eq!(DEFAULT_LEASE_TTL, std::time::Duration::from_hours(24));
        assert_eq!(lease_ttl_from(None), DEFAULT_LEASE_TTL);
    }

    #[test]
    fn lease_ttl_override_uses_seconds() {
        assert_eq!(lease_ttl_from(Some("7")), std::time::Duration::from_secs(7));
        assert_eq!(lease_ttl_from(Some("not-a-number")), DEFAULT_LEASE_TTL);
    }

    #[test]
    fn proof_rejects_wake_newer_than_clean() {
        let root = scratch("wake");
        let global_runtime = scratch("wake-global");
        let _lock = live_daemon_proof(&root, &global_runtime);
        let runtime = root.join("runtime");
        fs::write(runtime.join(LEASE_FILE_NAME), "lease").expect("lease");
        fs::write(runtime.join(WAKE_FILE_NAME), "wake").expect("wake");
        fs::write(runtime.join("watcher-ready"), "ready").expect("ready");
        fs::write(runtime.join("journal-clean"), "clean").expect("clean");
        assert!(daemon_freshness_proof_in(&root, &global_runtime));

        std::thread::sleep(std::time::Duration::from_millis(5));
        fs::write(runtime.join("wake"), "wake").expect("wake");

        assert!(!daemon_freshness_proof_in(&root, &global_runtime));
    }

    #[test]
    fn proof_rejects_stale_owner_without_live_lock() {
        let root = scratch("stale-owner");
        let global_runtime = scratch("stale-owner-global");
        {
            let _lock = live_daemon_proof(&root, &global_runtime);
        }
        let runtime = root.join("runtime");
        fs::write(runtime.join("watcher-ready"), "ready").expect("ready");
        fs::write(runtime.join("journal-clean"), "clean").expect("clean");

        assert!(!daemon_freshness_proof_in(&root, &global_runtime));
    }

    #[test]
    fn proof_rejects_startup_marker_from_previous_daemon() {
        let root = scratch("stale-startup");
        let global_runtime = scratch("stale-startup-global");
        let _lock = live_daemon_proof(&root, &global_runtime);
        fs::write(
            global_runtime.join(STARTUP_READY_FILE_NAME),
            "previous-owner",
        )
        .expect("stale startup ready");
        let runtime = root.join("runtime");
        fs::write(runtime.join(LEASE_FILE_NAME), "lease").expect("lease");
        fs::write(runtime.join("watcher-ready"), "ready").expect("ready");
        fs::write(runtime.join("journal-clean"), "clean").expect("clean");

        assert!(!daemon_freshness_proof_in(&root, &global_runtime));
    }

    #[test]
    fn keep_alive_failure_is_non_fatal() {
        let root = scratch("lease-failure");
        let state_root = root.join("state-file");
        fs::write(&state_root, "not a directory").expect("state file");

        keep_alive_best_effort(&state_root, &state_root);
    }

    #[test]
    fn required_refresh_reports_registration_failure() {
        let root = scratch("refresh-failure");
        let state_root = root.join("state-file");
        fs::write(&state_root, "not a directory").expect("state file");

        let err = request_refresh(&state_root, &state_root).expect_err("refresh should fail");

        assert_eq!(std::io::ErrorKind::NotADirectory, err.kind());
    }

    #[test]
    fn runtime_root_writability_rejects_plain_file() {
        let root = scratch("runtime-writable");
        let path = root.join("file");
        fs::write(&path, "not a directory").expect("file");

        assert!(!runtime_root_is_writable(&path));
    }

    #[test]
    fn daemon_binary_is_installed_under_runtime_root() {
        let root = scratch("daemon-install");
        let source = root.join("eg-indexd-source");
        fs::write(&source, "daemon v1").expect("source");

        let installed = install_daemon_binary(&source, &root).expect("install");

        assert_eq!(root.join("bin/eg-indexd"), installed);
        assert_eq!("daemon v1", fs::read_to_string(installed).expect("read"));
    }

    #[test]
    fn stale_daemon_install_is_replaced() {
        let root = scratch("daemon-reinstall");
        let source = root.join("eg-indexd-source");
        let installed = root.join("bin/eg-indexd");
        fs::create_dir_all(installed.parent().expect("parent")).expect("bin");
        fs::write(&source, "daemon v2 with more bytes").expect("source");
        fs::write(&installed, "old").expect("old");

        install_daemon_binary(&source, &root).expect("install");

        assert_eq!(
            "daemon v2 with more bytes",
            fs::read_to_string(installed).expect("read")
        );
    }
}
