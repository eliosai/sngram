//! File-based runtime coordination for indexed search.

use std::{
    env,
    ffi::OsString,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, SystemTime},
};

const RUNTIME_DIR_NAME: &str = "runtime";
const LEASE_FILE_NAME: &str = "lease";
const WAKE_FILE_NAME: &str = "wake";
const WATCHER_READY_FILE_NAME: &str = "watcher-ready";
const JOURNAL_CLEAN_FILE_NAME: &str = "journal-clean";
const REQUESTS_DIR_NAME: &str = "requests";
const DAEMON_BINARY_NAME: &str = "eg-indexd";
const DAEMON_REFRESH_ENV: &str = "EG_INDEX_DAEMON_REFRESH";
const DISABLE_DAEMON_AUTOSPAWN_ENV: &str = "EG_INDEXD_DISABLE_AUTOSPAWN";
const LEASE_TTL: Duration = Duration::from_mins(1);

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

    pub fn refresh_best_effort(&self) {
        refresh_best_effort(self.index_root, self.state_root);
    }
}

pub fn refresh_best_effort(index_root: &Path, state_root: &Path) {
    let runtime = runtime_dir(state_root);
    let _ = fs::create_dir_all(&runtime);
    let _ = write_marker(&runtime.join(LEASE_FILE_NAME));
    let _ = write_marker(&runtime.join(WAKE_FILE_NAME));
    let _ = register(
        index_root,
        state_root,
        env::current_dir(),
        env::args_os().skip(1),
    );
    ensure_daemon_best_effort();
}

pub fn is_daemon_refresh() -> bool {
    env::var_os(DAEMON_REFRESH_ENV).is_some()
}

pub fn daemon_freshness_proof(state_root: &Path) -> bool {
    let runtime = runtime_dir(state_root);
    if !runtime.join(WATCHER_READY_FILE_NAME).exists() {
        return false;
    }
    if !runtime.join(JOURNAL_CLEAN_FILE_NAME).exists() {
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
        .is_ok_and(|age| age <= LEASE_TTL)
}

pub fn mark_journal_clean(state_root: &Path) -> std::io::Result<()> {
    let runtime = runtime_dir(state_root);
    fs::create_dir_all(&runtime)?;
    write_marker(&runtime.join(JOURNAL_CLEAN_FILE_NAME))
}

pub fn clear_journal_clean(state_root: &Path) {
    let _ = fs::remove_file(runtime_dir(state_root).join(JOURNAL_CLEAN_FILE_NAME));
}

fn ensure_daemon_best_effort() {
    if env::var_os(DISABLE_DAEMON_AUTOSPAWN_ENV).is_some() {
        return;
    }
    let Some(binary) = daemon_binary() else {
        return;
    };
    let runtime_root = global_runtime_root();
    let _ = fs::create_dir_all(&runtime_root);
    let _ = Command::new(binary)
        .arg("--runtime-root")
        .arg(runtime_root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

fn daemon_binary() -> Option<PathBuf> {
    let current = env::current_exe().ok()?;
    let dir = current.parent()?;
    let binary = dir.join(DAEMON_BINARY_NAME);
    binary.exists().then_some(binary)
}

fn register(
    index_root: &Path,
    state_root: &Path,
    cwd: std::io::Result<PathBuf>,
    args: impl IntoIterator<Item = OsString>,
) -> std::io::Result<()> {
    let requests = global_runtime_root().join(REQUESTS_DIR_NAME);
    fs::create_dir_all(&requests)?;
    let key = hash_paths(index_root, state_root);
    let request = requests.join(format!("{key:016x}.request"));
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(request)?;
    if let Ok(cwd) = cwd {
        writeln!(file, "cwd={}", hex_encode(os_bytes(cwd.into_os_string())))?;
    }
    writeln!(file, "index_root={}", index_root.display())?;
    writeln!(file, "state_root={}", state_root.display())?;
    for arg in args {
        writeln!(file, "arg={}", hex_encode(os_bytes(arg)))?;
    }
    file.sync_all()
}

fn global_runtime_root() -> PathBuf {
    if let Some(root) = env::var_os("XDG_RUNTIME_DIR").filter(|value| !value.is_empty()) {
        return PathBuf::from(root).join("eg");
    }
    env::temp_dir().join("eg-runtime")
}

fn runtime_dir(state_root: &Path) -> PathBuf {
    state_root.join(RUNTIME_DIR_NAME)
}

fn write_marker(path: &Path) -> std::io::Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)?;
    writeln!(file, "{}", std::process::id())?;
    file.sync_all()
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
    use super::{daemon_freshness_proof, refresh_best_effort};
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

    #[test]
    fn proof_requires_watcher_ready_and_lease() {
        let root = scratch("proof");
        assert!(!daemon_freshness_proof(&root));

        refresh_best_effort(&root, &root);
        assert!(!daemon_freshness_proof(&root));

        let runtime = root.join("runtime");
        fs::create_dir_all(&runtime).expect("runtime");
        fs::write(runtime.join("watcher-ready"), "ready").expect("ready");
        assert!(!daemon_freshness_proof(&root));

        fs::write(runtime.join("journal-clean"), "clean").expect("clean");
        assert!(daemon_freshness_proof(&root));
    }
}
