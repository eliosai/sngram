//! File-based daemon for eg indexes.

mod watch;

use std::{
    collections::HashMap,
    env,
    ffi::OsString,
    fs,
    fs::{File, OpenOptions, TryLockError},
    io::{BufRead, BufReader, ErrorKind, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, SystemTime},
};

use anyhow::Context;

const REQUESTS_DIR_NAME: &str = "requests";
const RUNTIME_DIR_NAME: &str = "runtime";
const WATCHER_READY_FILE_NAME: &str = "watcher-ready";
const JOURNAL_CLEAN_FILE_NAME: &str = "journal-clean";
const OWNER_FILE_NAME: &str = "daemon-owner";
const LEASE_FILE_NAME: &str = "lease";
const WAKE_FILE_NAME: &str = "wake";
const WATCH_DIRS_FILE_NAME: &str = "watch-dirs";
const INDEX_DIR_NAME: &str = "index";
const POSTINGS_MANIFEST: &str = "postings-v9/manifest.bin";
const POSTINGS_JSON_MANIFEST: &str = "postings-v9/manifest.json";
const TANTIVY_JSON_MANIFEST: &str = "tantivy-v2/manifest.json";
const LOCK_FILE_NAME: &str = "daemon.lock";
const STARTUP_READY_FILE_NAME: &str = "startup-ready";
const EG_BINARY_NAME: &str = "eg";
const DAEMON_REFRESH_ENV: &str = "EG_INDEX_DAEMON_REFRESH";
const RUNTIME_ROOT_ENV: &str = "EG_INDEXD_RUNTIME_ROOT";
const LEASE_TTL_ENV: &str = "EG_INDEXD_LEASE_TTL_SECS";
const DEFAULT_LEASE_TTL: Duration = Duration::from_hours(24);
const POLL_INTERVAL: Duration = Duration::from_secs(2);
const STARTUP_IDLE_GRACE: Duration = Duration::from_mins(1);

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
}

impl Daemon {
    fn new(runtime_root: PathBuf, owner: String) -> anyhow::Result<Self> {
        Ok(Self {
            runtime_root,
            owner,
            watcher: watch::Watcher::new()?,
            watch_stamps: HashMap::new(),
        })
    }

    fn serve(&mut self) -> anyhow::Result<()> {
        self.prepare_startup()?;
        let requests = self.runtime_root.join(REQUESTS_DIR_NAME);
        let _ = fs::create_dir_all(&requests);
        self.watcher.watch_signal_dir(&requests)?;
        let started = std::time::Instant::now();
        loop {
            let requests = read_requests(&self.runtime_root)?;
            self.clear_dirty()?;
            self.refresh_requests(&requests);
            consolidate_children(&requests);
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
        self.mark_startup_ready()
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
            let _ = self.serve_request(request);
        }
    }

    fn serve_request(&mut self, request: &Request) -> anyhow::Result<()> {
        self.watch_request(request)?;
        self.refresh_if_needed(request)
    }

    fn watch_request(&mut self, request: &Request) -> anyhow::Result<()> {
        if self.sync_watches(request)? {
            mark_watcher_ready(&request.state_root)?;
        }
        mark_owner(&request.state_root, &self.owner)?;
        Ok(())
    }

    /// Watch the walked directory set, or the whole tree before a first build
    fn sync_watches(&mut self, request: &Request) -> anyhow::Result<bool> {
        let path = request
            .state_root
            .join(RUNTIME_DIR_NAME)
            .join(WATCH_DIRS_FILE_NAME);
        let Ok(stamp) = fs::metadata(&path).and_then(|meta| meta.modified()) else {
            return self
                .watcher
                .watch_tree(&request.index_root, &request.state_root);
        };
        if self.watch_stamps.get(&request.state_root) == Some(&stamp) {
            return Ok(true);
        }
        let dirs = read_watch_dirs(&path, &request.index_root)?;
        let watched = self
            .watcher
            .watch_dirs(&request.index_root, &dirs, &request.state_root)?;
        if watched {
            self.watch_stamps.insert(request.state_root.clone(), stamp);
        }
        Ok(watched)
    }

    fn refresh_if_needed(&mut self, request: &Request) -> anyhow::Result<()> {
        if request.has_live_lease() && request.needs_refresh() {
            if refresh_request(request, &self.runtime_root).is_ok() {
                mark_lease_live(&request.state_root)?;
            }
            self.clear_dirty()?;
        }
        Ok(())
    }

    fn clear_dirty(&mut self) -> anyhow::Result<()> {
        for state_root in self.watcher.drain_dirty()? {
            clear_journal_clean(&state_root);
        }
        Ok(())
    }

    fn wait_for_changes(&mut self, timeout: Duration) -> anyhow::Result<()> {
        for state_root in self.watcher.wait_dirty(timeout)? {
            clear_journal_clean(&state_root);
        }
        Ok(())
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
    fn configured_eg_binary(&self) -> Option<PathBuf> {
        self.eg_binary
            .as_ref()
            .filter(|binary| binary.exists())
            .cloned()
    }

    fn has_live_lease(&self) -> bool {
        let Ok(metadata) =
            fs::metadata(self.state_root.join(RUNTIME_DIR_NAME).join(LEASE_FILE_NAME))
        else {
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
        index.join(POSTINGS_MANIFEST).exists()
            || index.join(POSTINGS_JSON_MANIFEST).exists()
            || index.join(TANTIVY_JSON_MANIFEST).exists()
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

fn mark_watcher_ready(state_root: &Path) -> std::io::Result<()> {
    let runtime = state_root.join(RUNTIME_DIR_NAME);
    fs::create_dir_all(&runtime)?;
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(runtime.join(WATCHER_READY_FILE_NAME))?;
    writeln!(file, "{}", std::process::id())?;
    file.sync_all()
}

fn mark_owner(state_root: &Path, owner: &str) -> std::io::Result<()> {
    let runtime = state_root.join(RUNTIME_DIR_NAME);
    fs::create_dir_all(&runtime)?;
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(runtime.join(OWNER_FILE_NAME))?;
    writeln!(file, "{owner}")?;
    file.sync_all()
}

fn mark_lease_live(state_root: &Path) -> std::io::Result<()> {
    let runtime = state_root.join(RUNTIME_DIR_NAME);
    fs::create_dir_all(&runtime)?;
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(runtime.join(LEASE_FILE_NAME))?;
    writeln!(file, "{}", std::process::id())?;
    file.sync_all()
}

fn refresh_request(request: &Request, runtime_root: &Path) -> anyhow::Result<()> {
    clear_journal_clean(&request.state_root);
    let Some(binary) = request.configured_eg_binary().or_else(sibling_eg_binary) else {
        return Ok(());
    };
    let status = Command::new(binary)
        .args(&request.args)
        .current_dir(&request.cwd)
        .env(DAEMON_REFRESH_ENV, "1")
        .env(RUNTIME_ROOT_ENV, runtime_root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if !status.success() {
        clear_journal_clean(&request.state_root);
    }
    Ok(())
}

fn sibling_eg_binary() -> Option<PathBuf> {
    let current = env::current_exe().ok()?;
    let dir = current.parent()?;
    let binary = dir.join(EG_BINARY_NAME);
    binary.exists().then_some(binary)
}

fn clear_journal_clean(state_root: &Path) {
    let _ = fs::remove_file(
        state_root
            .join(RUNTIME_DIR_NAME)
            .join(JOURNAL_CLEAN_FILE_NAME),
    );
}

fn cleanup_state_root(state_root: &Path) -> anyhow::Result<()> {
    let runtime = state_root.join(RUNTIME_DIR_NAME);
    let index = state_root.join(INDEX_DIR_NAME);
    remove_path_if_exists(&index)
        .with_context(|| format!("failed to delete stale index {}", index.display()))?;
    for marker in [
        JOURNAL_CLEAN_FILE_NAME,
        WATCHER_READY_FILE_NAME,
        OWNER_FILE_NAME,
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
    clear_journal_clean(&request.state_root);
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
    use super::{
        DEFAULT_LEASE_TTL, Daemon, JOURNAL_CLEAN_FILE_NAME, LEASE_FILE_NAME, RUNTIME_DIR_NAME,
        Request, StartupDisposition, adopt_request, consolidate_children, discard_request,
        discard_state, lease_ttl_from, mark_lease_live, read_request, read_requests,
        startup_disposition,
    };
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
    fn configured_eg_binary_must_exist() {
        let root_guard = scratch("configured-eg");
        let root = root_guard.path().to_path_buf();
        let binary = root.join("eg");
        let mut request = request_for(&root);
        request.eg_binary = Some(binary.clone());

        assert_eq!(None, request.configured_eg_binary());

        fs::write(&binary, "eg").expect("binary");
        assert_eq!(Some(binary), request.configured_eg_binary());
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
}
