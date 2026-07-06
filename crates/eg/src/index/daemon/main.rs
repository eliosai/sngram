//! File-based maintenance daemon for eg indexes.

use std::{
    env, fs,
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    thread,
    time::{Duration, SystemTime},
};

const REQUESTS_DIR_NAME: &str = "requests";
const RUNTIME_DIR_NAME: &str = "runtime";
const WATCHER_READY_FILE_NAME: &str = "watcher-ready";
const LEASE_FILE_NAME: &str = "lease";
const INDEX_DIR_NAME: &str = "index";
const POSTINGS_MANIFEST: &str = "postings-v5/manifest.bin";
const POSTINGS_JSON_MANIFEST: &str = "postings-v5/manifest.json";
const LOCK_FILE_NAME: &str = "daemon.lock";
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
    loop {
        let requests = read_requests(&runtime_root)?;
        for request in &requests {
            mark_watcher_ready(&request.state_root)?;
        }
        consolidate_children(&requests);
        if !requests.iter().any(Request::has_live_lease) {
            break;
        }
        thread::sleep(POLL_INTERVAL);
    }
    Ok(())
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
    let mut index_root = None;
    let mut state_root = None;
    for line in BufReader::new(file).lines() {
        let line = line?;
        if let Some(value) = line.strip_prefix("index_root=") {
            index_root = Some(PathBuf::from(value));
        } else if let Some(value) = line.strip_prefix("state_root=") {
            state_root = Some(PathBuf::from(value));
        }
    }
    Ok(index_root
        .zip(state_root)
        .map(|(index_root, state_root)| Request {
            index_root,
            state_root,
        }))
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
