//! File-based maintenance daemon for eg indexes.

mod watch;

use std::{
    env,
    ffi::OsString,
    fs,
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, SystemTime},
};

const REQUESTS_DIR_NAME: &str = "requests";
const RUNTIME_DIR_NAME: &str = "runtime";
const WATCHER_READY_FILE_NAME: &str = "watcher-ready";
const JOURNAL_CLEAN_FILE_NAME: &str = "journal-clean";
const LEASE_FILE_NAME: &str = "lease";
const WAKE_FILE_NAME: &str = "wake";
const INDEX_DIR_NAME: &str = "index";
const POSTINGS_MANIFEST: &str = "postings-v5/manifest.bin";
const POSTINGS_JSON_MANIFEST: &str = "postings-v5/manifest.json";
const LOCK_FILE_NAME: &str = "daemon.lock";
const EG_BINARY_NAME: &str = "eg";
const DAEMON_REFRESH_ENV: &str = "EG_INDEX_DAEMON_REFRESH";
const LEASE_TTL: Duration = Duration::from_mins(1);
const POLL_INTERVAL: Duration = Duration::from_secs(2);

fn main() {
    if let Err(err) = run() {
        eprintln!("eg-indexd: {err}");
        std::process::exit(2);
    }
}

fn run() -> anyhow::Result<()> {
    let runtime_root = runtime_root_from_args()?;
    fs::create_dir_all(&runtime_root)?;
    let Some(_lock) = DaemonLock::acquire(&runtime_root)? else {
        return Ok(());
    };
    Daemon::new(runtime_root)?.serve()
}

struct Daemon {
    runtime_root: PathBuf,
    watcher: watch::Watcher,
}

impl Daemon {
    fn new(runtime_root: PathBuf) -> anyhow::Result<Self> {
        Ok(Self {
            runtime_root,
            watcher: watch::Watcher::new()?,
        })
    }

    fn serve(&mut self) -> anyhow::Result<()> {
        loop {
            let requests = read_requests(&self.runtime_root)?;
            self.clear_dirty()?;
            self.refresh_requests(&requests)?;
            consolidate_children(&requests);
            if !requests.iter().any(Request::has_live_lease) {
                return Ok(());
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    fn refresh_requests(&mut self, requests: &[Request]) -> anyhow::Result<()> {
        for request in requests {
            self.watch_request(request)?;
            self.refresh_if_needed(request)?;
        }
        Ok(())
    }

    fn watch_request(&mut self, request: &Request) -> anyhow::Result<()> {
        if self
            .watcher
            .watch_tree(&request.index_root, &request.state_root)?
        {
            mark_watcher_ready(&request.state_root)?;
        }
        Ok(())
    }

    fn refresh_if_needed(&mut self, request: &Request) -> anyhow::Result<()> {
        if request.has_live_lease() && request.needs_refresh() {
            let _ = refresh_request(request);
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
    _file: File,
}

impl DaemonLock {
    fn acquire(runtime_root: &Path) -> anyhow::Result<Option<Self>> {
        let path = runtime_root.join(LOCK_FILE_NAME);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                writeln!(file, "{}", std::process::id())?;
                Ok(Some(Self { path, _file: file }))
            },
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Ok(None),
            Err(err) => Err(err.into()),
        }
    }
}

impl Drop for DaemonLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[derive(Clone)]
struct Request {
    index_root: PathBuf,
    state_root: PathBuf,
    cwd: PathBuf,
    args: Vec<OsString>,
}

impl Request {
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
            .is_ok_and(|age| age <= LEASE_TTL)
    }

    fn has_index(&self) -> bool {
        let index = self.state_root.join(INDEX_DIR_NAME);
        index.join(POSTINGS_MANIFEST).exists() || index.join(POSTINGS_JSON_MANIFEST).exists()
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
}

fn read_requests(runtime_root: &Path) -> anyhow::Result<Vec<Request>> {
    let requests = runtime_root.join(REQUESTS_DIR_NAME);
    let Ok(entries) = fs::read_dir(requests) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_file()
            && let Some(request) = read_request(&entry.path())?
        {
            out.push(request);
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
    Ok(builder.finish())
}

#[derive(Default)]
struct RequestBuilder {
    index_root: Option<PathBuf>,
    state_root: Option<PathBuf>,
    cwd: Option<PathBuf>,
    args: Vec<OsString>,
}

impl RequestBuilder {
    fn read_line(&mut self, line: &str) -> anyhow::Result<()> {
        if let Some(value) = line.strip_prefix("index_root=") {
            self.index_root = Some(PathBuf::from(value));
        } else if let Some(value) = line.strip_prefix("state_root=") {
            self.state_root = Some(PathBuf::from(value));
        } else if let Some(value) = line.strip_prefix("cwd=") {
            self.cwd = Some(PathBuf::from(os_string_from_bytes(hex_decode(value)?)));
        } else if let Some(value) = line.strip_prefix("arg=") {
            self.args.push(os_string_from_bytes(hex_decode(value)?));
        }
        Ok(())
    }

    fn finish(self) -> Option<Request> {
        self.index_root
            .zip(self.state_root)
            .zip(self.cwd)
            .map(|((index_root, state_root), cwd)| Request {
                index_root,
                state_root,
                cwd,
                args: self.args,
            })
    }
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

fn refresh_request(request: &Request) -> anyhow::Result<()> {
    clear_journal_clean(&request.state_root);
    let Some(binary) = eg_binary() else {
        return Ok(());
    };
    let status = Command::new(binary)
        .args(&request.args)
        .current_dir(&request.cwd)
        .env(DAEMON_REFRESH_ENV, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if !status.success() {
        clear_journal_clean(&request.state_root);
    }
    Ok(())
}

fn eg_binary() -> Option<PathBuf> {
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

fn consolidate_children(requests: &[Request]) {
    for parent in requests.iter().filter(|request| request.has_index()) {
        for child in requests {
            if child.index_root == parent.index_root {
                continue;
            }
            if child.index_root.starts_with(&parent.index_root) {
                let _ = fs::remove_dir_all(child.state_root.join(INDEX_DIR_NAME));
            }
        }
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
    use super::{Request, read_request};
    use std::{
        ffi::OsString,
        fs,
        path::{Path, PathBuf},
        time::Duration,
    };

    fn scratch(name: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("eg-indexd-{}-{stamp}-{name}", std::process::id()));
        fs::create_dir_all(&root).expect("scratch dir");
        root
    }

    fn request_for(state_root: &Path) -> Request {
        Request {
            index_root: state_root.to_path_buf(),
            state_root: state_root.to_path_buf(),
            cwd: state_root.to_path_buf(),
            args: Vec::new(),
        }
    }

    #[test]
    fn request_decodes_cwd_and_replay_args() {
        let root = scratch("request");
        let request = root.join("entry.request");
        fs::write(
            &request,
            "\
cwd=2f746d702f65672d637764
index_root=/tmp/eg-root
state_root=/tmp/eg-state
arg=2d2d696e6465783d6175746f
arg=6e6565646c65
",
        )
        .expect("request");

        let request = read_request(&request).expect("read").expect("request");
        assert_eq!(PathBuf::from("/tmp/eg-cwd"), request.cwd);
        assert_eq!(PathBuf::from("/tmp/eg-root"), request.index_root);
        assert_eq!(PathBuf::from("/tmp/eg-state"), request.state_root);
        assert_eq!(
            vec![OsString::from("--index=auto"), OsString::from("needle")],
            request.args
        );
    }

    #[test]
    fn wake_newer_than_clean_requests_refresh() {
        let root = scratch("wake");
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
        let root = scratch("clean");
        let request = request_for(&root);
        let runtime = root.join("runtime");
        fs::create_dir_all(&runtime).expect("runtime");
        fs::write(runtime.join("wake"), "wake").expect("wake");
        std::thread::sleep(Duration::from_millis(5));
        fs::write(runtime.join("journal-clean"), "clean").expect("clean");

        assert!(!request.needs_refresh());
    }
}
