//! Index manifest and freshness checks.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    time::UNIX_EPOCH,
};

use anyhow::{Context, bail};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::{flags::HiArgs, haystack::Haystack};

const MANIFEST_VERSION: u32 = 5;
const TANTIVY_SCHEMA_VERSION: u32 = 1;
const POSTINGS_SCHEMA_VERSION: u32 = 3;
const TANTIVY_BACKEND: &str = "tantivy";
const POSTINGS_BACKEND: &str = "postings";
const TANTIVY_COMPAT_VERSION: &str = "0.26.1";
const MANIFEST_BINARY_FILE_NAME: &str = "manifest.bin";
const MANIFEST_BINARY_MAGIC: &[u8; 8] = b"EGMANI3\0";
const MANIFEST_BINARY_VERSION: u32 = 1;

#[derive(Clone, Copy)]
pub(super) enum ManifestBackend {
    Tantivy,
    Postings,
}

impl ManifestBackend {
    const fn id(self) -> &'static str {
        match self {
            Self::Tantivy => TANTIVY_BACKEND,
            Self::Postings => POSTINGS_BACKEND,
        }
    }

    const fn schema_version(self) -> u32 {
        match self {
            Self::Tantivy => TANTIVY_SCHEMA_VERSION,
            Self::Postings => POSTINGS_SCHEMA_VERSION,
        }
    }

    const fn engine_version(self) -> &'static str {
        match self {
            Self::Tantivy => TANTIVY_COMPAT_VERSION,
            Self::Postings => "",
        }
    }
}

pub(super) fn current_snapshot(
    args: &HiArgs,
    index_root: &Path,
    haystacks: &[Haystack],
    dir_paths: &[PathBuf],
) -> anyhow::Result<CurrentSnapshot> {
    let mut hashes = HashSet::with_capacity(haystacks.len());
    let mut files = Vec::with_capacity(haystacks.len());
    let mut dirs = BTreeMap::new();
    let git_untracked = git_untracked_paths(args, index_root)?;
    for dir in dir_paths {
        insert_dir(
            index_root,
            &super::absolute_path(args.cwd(), dir),
            &mut dirs,
        )?;
    }
    for (ord, haystack) in haystacks.iter().enumerate() {
        let absolute = super::absolute_path(args.cwd(), haystack.path());
        let relative = relative_path(index_root, &absolute);
        let path_hash = path_hash(&relative);
        if !hashes.insert(path_hash) {
            bail!("indexed search path hash collision for {relative:?}");
        }
        let metadata = fs::metadata(&absolute)
            .with_context(|| format!("failed to stat {} for indexing", absolute.display()))?;
        let is_git_untracked = git_untracked
            .as_ref()
            .is_some_and(|untracked| untracked.contains(&relative));
        files.push(CurrentFile {
            ord,
            path: haystack.path().to_path_buf(),
            manifest: ManifestFile {
                path: relative,
                display_path: haystack.path().to_string_lossy().into_owned(),
                path_hash,
                len: metadata.len(),
                modified_ns: modified_ns(&metadata),
                changed_ns: changed_ns(&metadata),
                explicit: haystack.is_explicit(),
                git_untracked: is_git_untracked,
            },
        });
        if let Some(parent) = absolute.parent() {
            insert_dir(index_root, parent, &mut dirs)?;
        }
    }
    insert_dir(index_root, index_root, &mut dirs)?;
    Ok(CurrentSnapshot {
        walk_fingerprint: args.index_walk_fingerprint(),
        git_freshness: git_untracked.is_some(),
        files,
        dirs: dirs
            .into_values()
            .map(|manifest| CurrentDir { manifest })
            .collect(),
    })
}

pub(super) fn fast_snapshot(
    args: &HiArgs,
    index_root: &Path,
    manifest: &Manifest,
) -> anyhow::Result<Option<CurrentSnapshot>> {
    if manifest.walk_fingerprint != args.index_walk_fingerprint() {
        return Ok(None);
    }
    if manifest.dirs.is_empty() {
        return Ok(None);
    }
    if let Some(snapshot) = git_fast_snapshot(args, index_root, manifest)? {
        return Ok(Some(snapshot));
    }
    let dirs = manifest
        .dirs
        .par_iter()
        .map(|dir| current_dir(index_root, dir))
        .collect::<anyhow::Result<Vec<_>>>()?;
    if dirs
        .iter()
        .zip(&manifest.dirs)
        .any(|(new, old)| new.manifest != *old)
    {
        return Ok(None);
    }
    let file_pairs = manifest
        .files
        .par_iter()
        .enumerate()
        .map(|(ord, file)| current_file_from_manifest(args, index_root, ord, file))
        .collect::<anyhow::Result<Vec<_>>>()?;
    if file_pairs.iter().any(Option::is_none) {
        return Ok(None);
    }
    let mut files = Vec::with_capacity(file_pairs.len());
    for pair in file_pairs.into_iter().flatten() {
        files.push(pair);
    }
    Ok(Some(CurrentSnapshot {
        walk_fingerprint: manifest.walk_fingerprint,
        git_freshness: manifest.git_freshness,
        files,
        dirs,
    }))
}

pub(super) fn manifest_for(
    backend: ManifestBackend,
    table_spec: sngram_weights::BuiltinTable,
    snapshot: &CurrentSnapshot,
) -> Manifest {
    Manifest {
        version: MANIFEST_VERSION,
        schema_version: backend.schema_version(),
        backend: backend.id().to_owned(),
        engine_version: backend.engine_version().to_owned(),
        table_id: table_spec.id().to_owned(),
        table_fingerprint: table_spec.fingerprint(),
        walk_fingerprint: snapshot.walk_fingerprint,
        git_freshness: snapshot.git_freshness,
        dirs: snapshot
            .dirs
            .iter()
            .map(|dir| dir.manifest.clone())
            .collect(),
        files: snapshot
            .files
            .iter()
            .map(|file| file.manifest.clone())
            .collect(),
    }
}

pub(super) fn read_manifest(path: &Path) -> anyhow::Result<Option<Manifest>> {
    if !path.exists() {
        return Ok(None);
    }
    let binary_path = binary_manifest_path(path);
    if binary_path.exists() && binary_is_fresh(&binary_path, path) {
        let bytes = fs::read(&binary_path).with_context(|| {
            format!(
                "failed to read binary index manifest {}",
                binary_path.display()
            )
        })?;
        return decode_binary_manifest(&bytes).map(Some).with_context(|| {
            format!(
                "failed to parse binary index manifest {}",
                binary_path.display()
            )
        });
    }
    let bytes = fs::read(path)
        .with_context(|| format!("failed to read index manifest {}", path.display()))?;
    let manifest = serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to parse index manifest {}", path.display()))?;
    if let Err(err) = write_binary_manifest(&binary_path, &manifest) {
        log::debug!(
            "eg index: failed to refresh binary manifest {}: {err}",
            binary_path.display()
        );
    }
    Ok(Some(manifest))
}

pub(super) fn is_compatible(
    manifest: &Manifest,
    backend: ManifestBackend,
    table_spec: sngram_weights::BuiltinTable,
) -> bool {
    manifest.version == MANIFEST_VERSION
        && manifest.schema_version == backend.schema_version()
        && manifest.backend == backend.id()
        && manifest.engine_version == backend.engine_version()
        && manifest.table_id == table_spec.id()
        && manifest.table_fingerprint == table_spec.fingerprint()
}

pub(super) fn write_manifest(path: &Path, manifest: &Manifest) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec(manifest).context("failed to encode index manifest")?;
    fs::write(path, bytes)
        .with_context(|| format!("failed to write index manifest {}", path.display()))?;
    write_binary_manifest(&binary_manifest_path(path), manifest)
}

pub(super) fn changed_ordinals(old: &Manifest, new: &Manifest) -> Option<Vec<usize>> {
    if old.version != new.version
        || old.schema_version != new.schema_version
        || old.backend != new.backend
        || old.engine_version != new.engine_version
        || old.table_id != new.table_id
        || old.table_fingerprint != new.table_fingerprint
        || old.walk_fingerprint != new.walk_fingerprint
        || old.git_freshness != new.git_freshness
        || old.dirs != new.dirs
        || old.files.len() != new.files.len()
    {
        return None;
    }
    let mut changed = Vec::new();
    for (ord, (old_file, new_file)) in old.files.iter().zip(&new.files).enumerate() {
        if old_file.path != new_file.path
            || old_file.path_hash != new_file.path_hash
            || old_file.git_untracked != new_file.git_untracked
        {
            return None;
        }
        if old_file.len != new_file.len
            || old_file.modified_ns != new_file.modified_ns
            || old_file.changed_ns != new_file.changed_ns
        {
            changed.push(ord);
        }
    }
    Some(changed)
}

fn binary_manifest_path(path: &Path) -> PathBuf {
    path.with_file_name(MANIFEST_BINARY_FILE_NAME)
}

fn binary_is_fresh(binary_path: &Path, json_path: &Path) -> bool {
    let Ok(binary) = fs::metadata(binary_path) else {
        return false;
    };
    let Ok(json) = fs::metadata(json_path) else {
        return false;
    };
    match (binary.modified(), json.modified()) {
        (Ok(binary), Ok(json)) => binary >= json,
        _ => false,
    }
}

fn write_binary_manifest(path: &Path, manifest: &Manifest) -> anyhow::Result<()> {
    let mut bytes = Vec::with_capacity(binary_manifest_capacity(manifest));
    bytes.extend_from_slice(MANIFEST_BINARY_MAGIC);
    write_u32(&mut bytes, MANIFEST_BINARY_VERSION);
    write_u32(&mut bytes, manifest.version);
    write_u32(&mut bytes, manifest.schema_version);
    write_string(&mut bytes, &manifest.backend)?;
    write_string(&mut bytes, &manifest.engine_version)?;
    write_string(&mut bytes, &manifest.table_id)?;
    write_u64(&mut bytes, manifest.table_fingerprint);
    write_u64(&mut bytes, manifest.walk_fingerprint);
    write_bool(&mut bytes, manifest.git_freshness);
    write_u32(&mut bytes, len_u32(manifest.dirs.len())?);
    write_u32(&mut bytes, len_u32(manifest.files.len())?);
    for dir in &manifest.dirs {
        write_string(&mut bytes, &dir.path)?;
        write_option_u64(&mut bytes, dir.modified_ns);
        write_option_u64(&mut bytes, dir.changed_ns);
    }
    for file in &manifest.files {
        write_string(&mut bytes, &file.path)?;
        write_string(&mut bytes, &file.display_path)?;
        write_u64(&mut bytes, file.path_hash);
        write_u64(&mut bytes, file.len);
        write_option_u64(&mut bytes, file.modified_ns);
        write_option_u64(&mut bytes, file.changed_ns);
        write_bool(&mut bytes, file.explicit);
        write_bool(&mut bytes, file.git_untracked);
    }
    let mut output = fs::File::create(path)
        .with_context(|| format!("failed to create binary index manifest {}", path.display()))?;
    output
        .write_all(&bytes)
        .with_context(|| format!("failed to write binary index manifest {}", path.display()))
}

fn binary_manifest_capacity(manifest: &Manifest) -> usize {
    64 + manifest
        .dirs
        .iter()
        .map(|dir| dir.path.len() + 24)
        .sum::<usize>()
        + manifest
            .files
            .iter()
            .map(|file| file.path.len() + file.display_path.len() + 48)
            .sum::<usize>()
}

fn decode_binary_manifest(bytes: &[u8]) -> anyhow::Result<Manifest> {
    let mut reader = BinaryManifestReader { bytes, pos: 0 };
    reader.read_magic()?;
    let binary_version = reader.read_u32()?;
    if binary_version != MANIFEST_BINARY_VERSION {
        bail!("unsupported binary manifest version {binary_version}");
    }
    let version = reader.read_u32()?;
    let schema_version = reader.read_u32()?;
    let backend = reader.read_string()?;
    let engine_version = reader.read_string()?;
    let table_id = reader.read_string()?;
    let table_fingerprint = reader.read_u64()?;
    let walk_fingerprint = reader.read_u64()?;
    let git_freshness = reader.read_bool()?;
    let dir_count = reader.read_usize()?;
    let file_count = reader.read_usize()?;
    let mut dirs = Vec::with_capacity(dir_count);
    for _ in 0..dir_count {
        dirs.push(ManifestDir {
            path: reader.read_string()?,
            modified_ns: reader.read_option_u64()?,
            changed_ns: reader.read_option_u64()?,
        });
    }
    let mut files = Vec::with_capacity(file_count);
    for _ in 0..file_count {
        files.push(ManifestFile {
            path: reader.read_string()?,
            display_path: reader.read_string()?,
            path_hash: reader.read_u64()?,
            len: reader.read_u64()?,
            modified_ns: reader.read_option_u64()?,
            changed_ns: reader.read_option_u64()?,
            explicit: reader.read_bool()?,
            git_untracked: reader.read_bool()?,
        });
    }
    reader.finish()?;
    Ok(Manifest {
        version,
        schema_version,
        backend,
        engine_version,
        table_id,
        table_fingerprint,
        walk_fingerprint,
        git_freshness,
        dirs,
        files,
    })
}

fn len_u32(len: usize) -> anyhow::Result<u32> {
    u32::try_from(len).context("binary manifest length does not fit in u32")
}

fn write_string(bytes: &mut Vec<u8>, value: &str) -> anyhow::Result<()> {
    write_u32(bytes, len_u32(value.len())?);
    bytes.extend_from_slice(value.as_bytes());
    Ok(())
}

fn write_option_u64(bytes: &mut Vec<u8>, value: Option<u64>) {
    match value {
        Some(value) => {
            write_bool(bytes, true);
            write_u64(bytes, value);
        },
        None => write_bool(bytes, false),
    }
}

fn write_bool(bytes: &mut Vec<u8>, value: bool) {
    bytes.push(u8::from(value));
}

fn write_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn write_u64(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

struct BinaryManifestReader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl BinaryManifestReader<'_> {
    fn read_magic(&mut self) -> anyhow::Result<()> {
        let magic = self.read_exact(MANIFEST_BINARY_MAGIC.len())?;
        if magic != MANIFEST_BINARY_MAGIC {
            bail!("invalid binary manifest magic");
        }
        Ok(())
    }

    fn read_usize(&mut self) -> anyhow::Result<usize> {
        usize::try_from(self.read_u32()?).context("binary manifest length does not fit in usize")
    }

    fn read_string(&mut self) -> anyhow::Result<String> {
        let len = self.read_usize()?;
        let bytes = self.read_exact(len)?;
        String::from_utf8(bytes.to_owned()).context("binary manifest string is not valid UTF-8")
    }

    fn read_option_u64(&mut self) -> anyhow::Result<Option<u64>> {
        if self.read_bool()? {
            Ok(Some(self.read_u64()?))
        } else {
            Ok(None)
        }
    }

    fn read_bool(&mut self) -> anyhow::Result<bool> {
        match self.read_u8()? {
            0 => Ok(false),
            1 => Ok(true),
            other => bail!("invalid binary manifest bool value {other}"),
        }
    }

    fn read_u8(&mut self) -> anyhow::Result<u8> {
        Ok(self.read_exact(1)?[0])
    }

    fn read_u32(&mut self) -> anyhow::Result<u32> {
        Ok(u32::from_le_bytes(
            self.read_exact(4)?.try_into().expect("four bytes"),
        ))
    }

    fn read_u64(&mut self) -> anyhow::Result<u64> {
        Ok(u64::from_le_bytes(
            self.read_exact(8)?.try_into().expect("eight bytes"),
        ))
    }

    fn read_exact(&mut self, len: usize) -> anyhow::Result<&[u8]> {
        let end = self
            .pos
            .checked_add(len)
            .context("binary manifest offset overflow")?;
        let Some(slice) = self.bytes.get(self.pos..end) else {
            bail!("binary manifest ended early");
        };
        self.pos = end;
        Ok(slice)
    }

    fn finish(&self) -> anyhow::Result<()> {
        if self.pos != self.bytes.len() {
            bail!("binary manifest has trailing bytes");
        }
        Ok(())
    }
}

fn insert_dir(
    root: &Path,
    absolute: &Path,
    dirs: &mut BTreeMap<String, ManifestDir>,
) -> anyhow::Result<()> {
    let relative = relative_path(root, absolute);
    if dirs.contains_key(&relative) {
        return Ok(());
    }
    let metadata = fs::metadata(absolute)
        .with_context(|| format!("failed to stat {} for index freshness", absolute.display()))?;
    if !metadata.is_dir() {
        return Ok(());
    }
    dirs.insert(
        relative.clone(),
        ManifestDir {
            path: relative,
            modified_ns: modified_ns(&metadata),
            changed_ns: changed_ns(&metadata),
        },
    );
    Ok(())
}

fn current_dir(root: &Path, old: &ManifestDir) -> anyhow::Result<CurrentDir> {
    let absolute = root.join(&old.path);
    let metadata = fs::metadata(&absolute)
        .with_context(|| format!("failed to stat {} for index freshness", absolute.display()))?;
    if !metadata.is_dir() {
        bail!(
            "indexed directory is no longer a directory: {}",
            absolute.display()
        );
    }
    Ok(CurrentDir {
        manifest: ManifestDir {
            path: old.path.clone(),
            modified_ns: modified_ns(&metadata),
            changed_ns: changed_ns(&metadata),
        },
    })
}

fn current_file_from_manifest(
    _args: &HiArgs,
    root: &Path,
    ord: usize,
    old: &ManifestFile,
) -> anyhow::Result<Option<CurrentFile>> {
    let absolute = root.join(&old.path);
    let metadata = match fs::metadata(&absolute) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(err).with_context(|| {
                format!("failed to stat {} for index freshness", absolute.display())
            });
        },
    };
    if !metadata.is_file() {
        return Ok(None);
    }
    let path = if old.display_path.is_empty() {
        absolute
    } else {
        PathBuf::from(&old.display_path)
    };
    Ok(Some(CurrentFile {
        ord,
        path,
        manifest: ManifestFile {
            path: old.path.clone(),
            display_path: old.display_path.clone(),
            path_hash: old.path_hash,
            len: metadata.len(),
            modified_ns: modified_ns(&metadata),
            changed_ns: changed_ns(&metadata),
            explicit: old.explicit,
            git_untracked: old.git_untracked,
        },
    }))
}

fn current_file_from_clean_manifest(ord: usize, root: &Path, old: &ManifestFile) -> CurrentFile {
    let absolute = root.join(&old.path);
    let path = if old.display_path.is_empty() {
        absolute
    } else {
        PathBuf::from(&old.display_path)
    };
    CurrentFile {
        ord,
        path,
        manifest: old.clone(),
    }
}

fn git_fast_snapshot(
    args: &HiArgs,
    index_root: &Path,
    manifest: &Manifest,
) -> anyhow::Result<Option<CurrentSnapshot>> {
    if !args.index_git_freshness_safe() {
        return Ok(None);
    }
    if !manifest.git_freshness {
        return Ok(None);
    }
    let Some(git_root) = git_root(index_root)? else {
        return Ok(None);
    };
    let manifest_paths = manifest
        .files
        .iter()
        .enumerate()
        .map(|(ord, file)| (file.path.as_str(), ord))
        .collect::<HashMap<_, _>>();
    let manifest_untracked = manifest
        .files
        .iter()
        .filter(|file| file.git_untracked)
        .map(|file| file.path.clone())
        .collect::<HashSet<_>>();
    let dirty = git_status_paths(&git_root)?;
    let mut dirty_manifest_paths = HashSet::new();
    let mut current_untracked = HashSet::new();
    for path in dirty {
        let Some(relative) = git_relative_to_index_relative(&git_root, index_root, &path) else {
            continue;
        };
        if is_eg_state_path(&relative) {
            continue;
        }
        let Some(&ord) = manifest_paths.get(relative.as_str()) else {
            return Ok(None);
        };
        if is_ignore_control_path(&relative) {
            return Ok(None);
        }
        if manifest.files[ord].git_untracked {
            current_untracked.insert(relative.clone());
        }
        dirty_manifest_paths.insert(relative);
    }
    if current_untracked != manifest_untracked {
        return Ok(None);
    }

    let mut changed = HashMap::with_capacity(dirty_manifest_paths.len());
    for path in &dirty_manifest_paths {
        let Some(&ord) = manifest_paths.get(path.as_str()) else {
            return Ok(None);
        };
        let Some(file) = current_file_from_manifest(args, index_root, ord, &manifest.files[ord])?
        else {
            return Ok(None);
        };
        changed.insert(ord, file);
    }

    let dirs = manifest
        .dirs
        .iter()
        .cloned()
        .map(|manifest| CurrentDir { manifest })
        .collect();
    let mut files = Vec::with_capacity(manifest.files.len());
    for (ord, old) in manifest.files.iter().enumerate() {
        files.push(
            changed
                .remove(&ord)
                .unwrap_or_else(|| current_file_from_clean_manifest(ord, index_root, old)),
        );
    }
    log::debug!(
        "eg index: git freshness snapshot dirty={} untracked={}",
        dirty_manifest_paths.len(),
        manifest_untracked.len()
    );
    Ok(Some(CurrentSnapshot {
        walk_fingerprint: manifest.walk_fingerprint,
        git_freshness: manifest.git_freshness,
        files,
        dirs,
    }))
}

fn git_untracked_paths(
    args: &HiArgs,
    index_root: &Path,
) -> anyhow::Result<Option<HashSet<String>>> {
    if !args.index_git_freshness_safe() {
        return Ok(None);
    }
    let Some(git_root) = git_root(index_root)? else {
        return Ok(None);
    };
    let untracked = git_paths(
        &git_root,
        &[
            "ls-files",
            "-z",
            "--full-name",
            "--others",
            "--exclude-standard",
        ],
    )?
    .into_iter()
    .filter_map(|path| git_relative_to_index_relative(&git_root, index_root, &path))
    .filter(|path| !is_eg_state_path(path))
    .collect::<HashSet<_>>();
    Ok(Some(untracked))
}

fn git_root(index_root: &Path) -> anyhow::Result<Option<PathBuf>> {
    let output = match Command::new("git")
        .arg("-C")
        .arg(index_root)
        .args(["rev-parse", "--show-toplevel"])
        .output()
    {
        Ok(output) => output,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).context("failed to run git rev-parse for index freshness"),
    };
    if !output.status.success() {
        return Ok(None);
    }
    let root = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if root.is_empty() {
        return Ok(None);
    }
    Ok(Some(PathBuf::from(root)))
}

fn git_paths(git_root: &Path, args: &[&str]) -> anyhow::Result<Vec<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(git_root)
        .args(args)
        .output()
        .with_context(|| format!("failed to run git {} for index freshness", args.join(" ")))?;
    if !output.status.success() {
        return Ok(Vec::new());
    }
    Ok(output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| String::from_utf8_lossy(path).into_owned())
        .collect())
}

fn git_status_paths(git_root: &Path) -> anyhow::Result<Vec<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(git_root)
        .args(["status", "--porcelain=v1", "-z", "--untracked-files=all"])
        .output()
        .context("failed to run git status for index freshness")?;
    if !output.status.success() {
        return Ok(Vec::new());
    }
    let mut paths = Vec::new();
    let mut fields = output.stdout.split(|byte| *byte == 0).peekable();
    while let Some(field) = fields.next() {
        if field.len() < 4 {
            continue;
        }
        let status = &field[..2];
        let path = &field[3..];
        if !path.is_empty() {
            paths.push(String::from_utf8_lossy(path).into_owned());
        }
        if matches!(status, b"R " | b" R" | b"RR" | b"C " | b" C") {
            let _ = fields.next();
        }
    }
    Ok(paths)
}

fn git_relative_to_index_relative(
    git_root: &Path,
    index_root: &Path,
    git_relative: &str,
) -> Option<String> {
    let absolute = git_root.join(git_relative);
    absolute
        .starts_with(index_root)
        .then(|| relative_path(index_root, &absolute))
}

fn is_ignore_control_path(relative: &str) -> bool {
    relative
        .rsplit('/')
        .next()
        .is_some_and(|name| matches!(name, ".gitignore" | ".ignore" | ".rgignore"))
}

fn is_eg_state_path(relative: &str) -> bool {
    relative == ".eg" || relative.starts_with(".eg/")
}

fn relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

fn path_hash(path: &str) -> u64 {
    path.as_bytes()
        .iter()
        .fold(0xcbf2_9ce4_8422_2325u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
        })
}

fn modified_ns(metadata: &fs::Metadata) -> Option<u64> {
    let duration = metadata.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    u64::try_from(duration.as_nanos()).ok()
}

#[cfg(unix)]
fn changed_ns(metadata: &fs::Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;

    let secs = u64::try_from(metadata.ctime()).ok()?;
    let nanos = u64::try_from(metadata.ctime_nsec()).ok()?;
    secs.checked_mul(1_000_000_000)?.checked_add(nanos)
}

#[cfg(not(unix))]
fn changed_ns(_metadata: &fs::Metadata) -> Option<u64> {
    None
}

pub(super) struct CurrentFile {
    pub(super) ord: usize,
    pub(super) path: PathBuf,
    manifest: ManifestFile,
}

impl CurrentFile {
    pub(super) fn path_hash(&self) -> u64 {
        self.manifest.path_hash
    }

    pub(super) fn is_explicit(&self) -> bool {
        self.manifest.explicit
    }
}

pub(super) struct CurrentDir {
    manifest: ManifestDir,
}

pub(super) struct CurrentSnapshot {
    walk_fingerprint: u64,
    git_freshness: bool,
    pub(super) files: Vec<CurrentFile>,
    dirs: Vec<CurrentDir>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct Manifest {
    version: u32,
    #[serde(default)]
    schema_version: u32,
    #[serde(default)]
    backend: String,
    #[serde(default)]
    engine_version: String,
    table_id: String,
    table_fingerprint: u64,
    #[serde(default)]
    walk_fingerprint: u64,
    #[serde(default)]
    git_freshness: bool,
    #[serde(default)]
    dirs: Vec<ManifestDir>,
    files: Vec<ManifestFile>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct ManifestDir {
    path: String,
    modified_ns: Option<u64>,
    changed_ns: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ManifestFile {
    path: String,
    #[serde(default)]
    display_path: String,
    path_hash: u64,
    len: u64,
    modified_ns: Option<u64>,
    changed_ns: Option<u64>,
    #[serde(default)]
    explicit: bool,
    #[serde(default)]
    git_untracked: bool,
}
