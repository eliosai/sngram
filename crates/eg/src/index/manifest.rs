//! Index manifest and freshness checks.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::Command,
    time::UNIX_EPOCH,
};

use anyhow::{Context, bail};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::{flags::HiArgs, haystack::Haystack};

const MANIFEST_VERSION: u32 = 6;
const TANTIVY_SCHEMA_VERSION: u32 = 4;
const POSTINGS_SCHEMA_VERSION: u32 = 7;
const TANTIVY_BACKEND: &str = "tantivy";
const POSTINGS_BACKEND: &str = "postings";
const TANTIVY_COMPAT_VERSION: &str = "0.26.1";
const MANIFEST_BINARY_MAGIC: &[u8; 8] = b"EGMANI4\0";
const MANIFEST_BINARY_VERSION: u32 = 4;
const MANIFEST_BINARY_EXTENSION: &str = "bin";
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
/// Bytes hashed from each end of a file by the content-hash freshness mode.
const CONTENT_HASH_WINDOW: usize = 8 * 1024;
/// Environment variable forcing the JSON manifest to be written alongside the
/// binary one, for tooling that reads the human-readable form.
const JSON_MANIFEST_ENV: &str = "EG_INDEX_JSON_MANIFEST";

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
    let freshness = args.index().freshness();
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
                content_hash: content_freshness_hash(freshness, &absolute, metadata.len()),
                explicit: haystack.is_explicit(),
                git_untracked: is_git_untracked,
                skipped_binary: super::classify::is_binary_path(&absolute).with_context(|| {
                    format!("failed to classify {} for indexing", absolute.display())
                })?,
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
    table_fingerprint: u64,
    snapshot: &CurrentSnapshot,
) -> Manifest {
    Manifest {
        version: MANIFEST_VERSION,
        schema_version: backend.schema_version(),
        backend: backend.id().to_owned(),
        engine_version: backend.engine_version().to_owned(),
        table_fingerprint,
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

/// Read the manifest, preferring the binary form and falling back to JSON.
///
/// The binary manifest is the commit point; the JSON form is only written for
/// tooling (see [`write_manifest`]). Either alone is a valid, complete index
/// manifest, so a missing JSON file is not treated as a missing index.
pub(super) fn read_manifest(path: &Path) -> anyhow::Result<Option<Manifest>> {
    let binary_path = binary_manifest_path(path);
    let json_exists = path.exists();
    let binary_exists = binary_path.exists();
    if binary_exists && (!json_exists || binary_is_fresh(&binary_path, path)) {
        match read_binary_manifest(&binary_path) {
            Ok(manifest) => return Ok(Some(manifest)),
            Err(err) => log::debug!(
                "eg index: binary manifest {} unreadable ({err:#}); falling back to JSON",
                binary_path.display()
            ),
        }
    }
    if !json_exists {
        return Ok(None);
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

fn read_binary_manifest(binary_path: &Path) -> anyhow::Result<Manifest> {
    let bytes = fs::read(binary_path).with_context(|| {
        format!(
            "failed to read binary index manifest {}",
            binary_path.display()
        )
    })?;
    decode_binary_manifest(&bytes).with_context(|| {
        format!(
            "failed to parse binary index manifest {}",
            binary_path.display()
        )
    })
}

/// Return true when a manifest exists in either the binary or JSON form.
pub(super) fn manifest_present(path: &Path) -> bool {
    path.exists() || binary_manifest_path(path).exists()
}

/// Remove both manifest forms, ignoring files that are already gone.
pub(super) fn remove_manifest(path: &Path) -> anyhow::Result<()> {
    for candidate in [path.to_path_buf(), binary_manifest_path(path)] {
        match fs::remove_file(&candidate) {
            Ok(()) => {},
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {},
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("failed to remove manifest {}", candidate.display()));
            },
        }
    }
    Ok(())
}

pub(super) fn is_compatible(
    manifest: &Manifest,
    backend: ManifestBackend,
    table_fingerprint: u64,
) -> bool {
    manifest.version == MANIFEST_VERSION
        && manifest.schema_version == backend.schema_version()
        && manifest.backend == backend.id()
        && manifest.engine_version == backend.engine_version()
        && manifest.table_fingerprint == table_fingerprint
}

/// Write the manifest, always as binary and, when enabled, also as JSON.
///
/// The full-corpus JSON encode is megabytes on a large corpus and is rewritten
/// every build, so by default only the compact binary manifest is written. The
/// JSON form is added under `--debug` or when [`JSON_MANIFEST_ENV`] is set. The
/// JSON is written first so the binary lands last and stays the preferred read.
pub(super) fn write_manifest(path: &Path, manifest: &Manifest) -> anyhow::Result<()> {
    if json_manifest_enabled() {
        let bytes = serde_json::to_vec(manifest).context("failed to encode index manifest")?;
        write_synced(path, &bytes)
            .with_context(|| format!("failed to write index manifest {}", path.display()))?;
    }
    write_binary_manifest(&binary_manifest_path(path), manifest)
}

/// Return true when the human-readable JSON manifest should also be written.
fn json_manifest_enabled() -> bool {
    log::log_enabled!(log::Level::Debug) || std::env::var_os(JSON_MANIFEST_ENV).is_some()
}

/// Write bytes to a file and fsync it so the manifest is durable on return.
fn write_synced(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = fs::File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

pub(super) fn changed_ordinals(old: &Manifest, new: &Manifest) -> Option<Vec<usize>> {
    if old.version != new.version
        || old.schema_version != new.schema_version
        || old.backend != new.backend
        || old.engine_version != new.engine_version
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
        if old_file.skipped_binary != new_file.skipped_binary
            || file_content_changed(old_file, new_file)
        {
            changed.push(ord);
        }
    }
    Some(changed)
}

/// Decide whether a file changed. When both manifests carry a content hash
/// (hash freshness), compare those; otherwise fall back to stat fields.
fn file_content_changed(old: &ManifestFile, new: &ManifestFile) -> bool {
    match (old.content_hash, new.content_hash) {
        (Some(old_hash), Some(new_hash)) => old_hash != new_hash,
        _ => {
            old.len != new.len
                || old.modified_ns != new.modified_ns
                || old.changed_ns != new.changed_ns
        },
    }
}

fn binary_manifest_path(path: &Path) -> PathBuf {
    path.with_extension(MANIFEST_BINARY_EXTENSION)
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
        write_option_u64(&mut bytes, file.content_hash);
        write_bool(&mut bytes, file.explicit);
        write_bool(&mut bytes, file.git_untracked);
        write_bool(&mut bytes, file.skipped_binary);
    }
    let mut output = fs::File::create(path)
        .with_context(|| format!("failed to create binary index manifest {}", path.display()))?;
    output
        .write_all(&bytes)
        .with_context(|| format!("failed to write binary index manifest {}", path.display()))?;
    output
        .sync_all()
        .with_context(|| format!("failed to fsync binary index manifest {}", path.display()))
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
            .map(|file| file.path.len() + file.display_path.len() + 58)
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
            content_hash: reader.read_option_u64()?,
            explicit: reader.read_bool()?,
            git_untracked: reader.read_bool()?,
            skipped_binary: reader.read_bool()?,
        });
    }
    reader.finish()?;
    Ok(Manifest {
        version,
        schema_version,
        backend,
        engine_version,
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
        self.read_exact(1)?
            .first()
            .copied()
            .context("binary manifest ended early")
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
    args: &HiArgs,
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
    let content_hash = content_freshness_hash(args.index().freshness(), &absolute, metadata.len());
    let skipped_binary = super::classify::is_binary_path(&absolute).with_context(|| {
        format!(
            "failed to classify {} for index freshness",
            absolute.display()
        )
    })?;
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
            content_hash,
            explicit: old.explicit,
            git_untracked: old.git_untracked,
            skipped_binary,
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
        if manifest
            .files
            .get(ord)
            .is_some_and(|file| file.git_untracked)
        {
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
        let Some(old) = manifest.files.get(ord) else {
            return Ok(None);
        };
        let Some(file) = current_file_from_manifest(args, index_root, ord, old)? else {
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
        let (Some(status), Some(path)) = (field.get(..2), field.get(3..)) else {
            continue;
        };
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

fn fnv1a(mut hash: u64, bytes: &[u8]) -> u64 {
    for &byte in bytes {
        hash = (hash ^ u64::from(byte)).wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Content hash for hash freshness, or `None` for stat freshness.
fn content_freshness_hash(
    freshness: super::config::IndexFreshness,
    path: &Path,
    len: u64,
) -> Option<u64> {
    if freshness.is_hash() {
        content_hash(path, len)
    } else {
        None
    }
}

/// Fast content hash over the length plus the file's head and tail windows.
///
/// This catches same-stat mutations that touch either end of the file; a
/// change confined to the interior of a file larger than both windows is not
/// detected, matching the documented first/last-window trade-off. A read
/// failure yields `None`, which falls back to stat comparison.
fn content_hash(path: &Path, len: u64) -> Option<u64> {
    let mut file = fs::File::open(path).ok()?;
    let hash = fnv1a(FNV_OFFSET, &len.to_le_bytes());
    if len == 0 {
        return Some(hash);
    }
    let window = CONTENT_HASH_WINDOW as u64;
    if len <= 2 * window {
        let mut body = vec![0u8; usize::try_from(len).ok()?];
        file.read_exact(&mut body).ok()?;
        return Some(fnv1a(hash, &body));
    }
    let mut buf = vec![0u8; CONTENT_HASH_WINDOW];
    file.read_exact(&mut buf).ok()?;
    let hash = fnv1a(hash, &buf);
    file.seek(SeekFrom::Start(len - window)).ok()?;
    file.read_exact(&mut buf).ok()?;
    Some(fnv1a(hash, &buf))
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

    pub(super) fn is_skipped_binary(&self) -> bool {
        self.manifest.skipped_binary
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

impl Manifest {
    /// Number of files recorded in this manifest.
    pub(super) fn file_count(&self) -> usize {
        self.files.len()
    }
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
    content_hash: Option<u64>,
    #[serde(default)]
    explicit: bool,
    #[serde(default)]
    git_untracked: bool,
    #[serde(default)]
    skipped_binary: bool,
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{
        Manifest, ManifestFile, binary_manifest_path, changed_ordinals, file_content_changed,
        read_binary_manifest, write_binary_manifest,
    };

    fn file(len: u64, modified: u64, changed: u64, content: Option<u64>) -> ManifestFile {
        ManifestFile {
            path: "a".to_owned(),
            display_path: String::new(),
            path_hash: 1,
            len,
            modified_ns: Some(modified),
            changed_ns: Some(changed),
            content_hash: content,
            explicit: false,
            git_untracked: false,
            skipped_binary: false,
        }
    }

    fn manifest(files: Vec<ManifestFile>) -> Manifest {
        Manifest {
            version: 6,
            schema_version: 6,
            backend: "postings".to_owned(),
            engine_version: String::new(),
            table_fingerprint: 7,
            walk_fingerprint: 8,
            git_freshness: false,
            dirs: Vec::new(),
            files,
        }
    }

    fn scratch_path(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("eg-manifest-{}-{stamp}", std::process::id()));
        fs::create_dir_all(&root).expect("scratch dir");
        root.join(name)
    }

    #[test]
    fn content_hash_overrides_stat_when_present() {
        let old = file(10, 1, 1, Some(42));
        let touched = file(10, 2, 2, Some(42));
        assert!(
            !file_content_changed(&old, &touched),
            "same hash, new mtime is unchanged"
        );
        let mutated = file(10, 1, 1, Some(43));
        assert!(
            file_content_changed(&old, &mutated),
            "different hash is changed"
        );
    }

    #[test]
    fn stat_fields_used_when_hash_absent() {
        let old = file(10, 1, 1, None);
        assert!(
            file_content_changed(&old, &file(10, 2, 1, None)),
            "mtime change"
        );
        assert!(
            file_content_changed(&old, &file(11, 1, 1, None)),
            "len change"
        );
        assert!(
            !file_content_changed(&old, &file(10, 1, 1, None)),
            "identical stat"
        );
    }

    #[test]
    fn binary_manifest_path_is_specific_to_each_manifest() {
        assert_eq!(
            binary_manifest_path(Path::new("/idx/manifest.json")),
            Path::new("/idx/manifest.bin")
        );
        assert_eq!(
            binary_manifest_path(Path::new("/idx/delta-manifest.json")),
            Path::new("/idx/delta-manifest.bin")
        );
    }

    #[test]
    fn skipped_binary_transition_marks_ordinal_changed() {
        let old = manifest(vec![file(10, 1, 1, None)]);
        let mut new = manifest(vec![file(10, 1, 1, None)]);
        new.files[0].skipped_binary = true;

        assert_eq!(changed_ordinals(&old, &new), Some(vec![0]));
    }

    #[test]
    fn binary_manifest_round_trips_skipped_binary_disposition() {
        let path = scratch_path("manifest.bin");
        let mut source = manifest(vec![file(10, 1, 1, Some(42))]);
        source.files[0].skipped_binary = true;

        write_binary_manifest(&path, &source).expect("write binary manifest");
        let decoded = read_binary_manifest(&path).expect("read binary manifest");

        assert_eq!(decoded.files.len(), 1);
        assert!(decoded.files[0].skipped_binary);
        assert_eq!(decoded.files[0].content_hash, Some(42));

        fs::remove_file(&path).expect("remove manifest");
        fs::remove_dir(path.parent().expect("parent")).expect("remove scratch dir");
    }
}
