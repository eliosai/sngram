use std::io::Write as _;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use opendal::Operator;
use tracing::{debug, error, info, warn};

fn say(msg: &str) {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let secs = ts % 86400;
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    println!("[{h:02}:{m:02}:{s:02}] {msg}");
    let _ = std::io::stdout().flush();
}

use crate::counter::BigramCounter;
use crate::mint::mint;
use crate::paths::ensure_dir;
use crate::source::{
    DATASETS, FileStream, ParquetFile, drain_stream, hf_operator, is_not_found, is_transient,
    list_files, open_stream,
};

pub type Milestone = (u64, &'static str);

const HARD_RETRY_MAX: u32 = 5;
const RETRY_BASE: Duration = Duration::from_secs(2);
const RETRY_CAP: Duration = Duration::from_secs(60);

#[derive(Default, Debug, Clone, Copy)]
struct Stats {
    files_ok: u64,
    files_skipped: u64,
    prefixes_missing: u64,
    prefixes_failed: u64,
    bytes: u64,
}

impl Stats {
    fn add(&mut self, o: Stats) {
        self.files_ok += o.files_ok;
        self.files_skipped += o.files_skipped;
        self.prefixes_missing += o.prefixes_missing;
        self.prefixes_failed += o.prefixes_failed;
        self.bytes += o.bytes;
    }
}

pub async fn learn(
    token: String,
    mint_dir: PathBuf,
    milestones: Vec<Milestone>,
) -> anyhow::Result<()> {
    ensure_dir(&mint_dir)?;
    let counter = BigramCounter::new();
    let mut cum: u64 = 0;
    let mut next: usize = 0;
    let mut overall = Stats::default();
    let start = Instant::now();
    let mut first_count_at: Option<Instant> = None;

    say(&format!("sngram learn -> {}", mint_dir.display()));
    say(&format!(
        "order: {}",
        DATASETS.iter().map(|d| d.id).collect::<Vec<_>>().join(" -> ")
    ));
    info!(
        target: "sngram::run",
        mint_dir = %mint_dir.display(),
        datasets = %DATASETS.iter().map(|d| d.id).collect::<Vec<_>>().join(" -> "),
        n_milestones = milestones.len(),
        "learn starting"
    );

    for ds in DATASETS {
        say(&format!("\n====== {} ({}) ======", ds.id, ds.repo));
        info!(
            target: "sngram::dataset",
            dataset = ds.id,
            repo = ds.repo,
            field = ds.field,
            prefix = ds.prefix,
            n_langs = ds.langs.len(),
            "entering dataset"
        );
        let op = match hf_operator(ds.repo, Some(&token)) {
            Ok(op) => op,
            Err(e) => {
                error!(
                    target: "sngram::dataset",
                    dataset = ds.id,
                    repo = ds.repo,
                    error = %format!("{e:#}"),
                    "cannot build operator; skipping whole dataset"
                );
                overall.prefixes_failed += 1;
                continue;
            }
        };
        let prefixes: Vec<String> = if ds.langs.is_empty() {
            vec![ds.prefix.to_owned()]
        } else {
            ds.langs.iter().map(|l| format!("{}{l}/", ds.prefix)).collect()
        };
        let mut ds_stats = Stats::default();
        for prefix in &prefixes {
            say(&format!("-- {}/{}", ds.id, prefix.trim_end_matches('/')));
            info!(target: "sngram::prefix", dataset = ds.id, prefix = %prefix, "listing");
            let list_t0 = Instant::now();
            let (files, list_outcome) = list_with_retry(&op, prefix).await;
            let list_ms = list_t0.elapsed().as_millis();
            let n_files = files.len();
            match list_outcome {
                ListOutcome::Ok => {
                    say(&format!("   {} files", n_files));
                    info!(
                        target: "sngram::prefix",
                        dataset = ds.id,
                        prefix = %prefix,
                        n_files,
                        list_ms,
                        "list complete"
                    );
                }
                ListOutcome::Missing => {
                    say(&format!("   .. prefix not found on HF: {prefix} (skipping)"));
                    warn!(
                        target: "sngram::prefix",
                        dataset = ds.id,
                        prefix = %prefix,
                        "prefix not found on HF; skipping"
                    );
                    ds_stats.prefixes_missing += 1;
                    continue;
                }
                ListOutcome::Failed => {
                    say(&format!(
                        "!! WARN list failed permanently for {prefix} after {HARD_RETRY_MAX} retries; skipping"
                    ));
                    error!(
                        target: "sngram::prefix",
                        dataset = ds.id,
                        prefix = %prefix,
                        max_retries = HARD_RETRY_MAX,
                        "list failed permanently; skipping prefix"
                    );
                    ds_stats.prefixes_failed += 1;
                    continue;
                }
            }
            if files.is_empty() {
                say("   (no parquet files in this prefix)");
                debug!(target: "sngram::prefix", dataset = ds.id, prefix = %prefix, "empty prefix");
                continue;
            }
            let (tx, mut rx) = tokio::sync::mpsc::channel::<PrefetchItem>(1);
            let op_p = op.clone();
            let field = ds.field;
            let prefetch_handle = tokio::spawn(async move {
                for (idx, file) in files.into_iter().enumerate() {
                    let opened = open_with_inline_retry(&op_p, &file, field).await;
                    if tx.send(PrefetchItem { idx, file, opened }).await.is_err() {
                        break;
                    }
                }
            });
            while let Some(item) = rx.recv().await {
                debug!(
                    target: "sngram::file",
                    dataset = ds.id,
                    idx = item.idx + 1,
                    of = n_files,
                    path = %item.file.path,
                    compressed_size = item.file.size,
                    prefetched = item.opened.is_ok(),
                    "starting file"
                );
                let t0 = Instant::now();
                let bytes = match drain_with_inline_retry(&op, &item.file, ds.field, item.opened.ok(), &counter).await {
                    Ok(b) => b,
                    Err(e) => {
                        say(&format!("!! skip {} ({e:#})", item.file.path));
                        error!(
                            target: "sngram::file",
                            dataset = ds.id,
                            path = %item.file.path,
                            error = %format!("{e:#}"),
                            "permanent failure, skipping file"
                        );
                        ds_stats.files_skipped += 1;
                        continue;
                    }
                };
                let file_dt = t0.elapsed();
                ds_stats.files_ok += 1;
                ds_stats.bytes += bytes;
                cum += bytes;
                if first_count_at.is_none() {
                    first_count_at = Some(t0);
                }
                let i = item.idx;
                let f = &item.file;
                let active_secs = first_count_at
                    .map_or(1.0, |t| t.elapsed().as_secs_f64().max(1.0));
                let avg_mbps = (bytes_to_f(cum)) / 1e6 / active_secs;
                let inst_mbps = (bytes_to_f(bytes)) / 1e6 / file_dt.as_secs_f64().max(0.001);
                let leaf = f.path.rsplit('/').next().unwrap_or(&f.path);
                say(&format!(
                    "   [{:>4}/{:<4}] +{:>7} KB  cum {:>7.2} GB  now {:>4.0} MB/s  avg {:>4.0} MB/s  {}",
                    i + 1,
                    n_files,
                    bytes / 1_000,
                    bytes_to_f(cum) / 1e9,
                    inst_mbps,
                    avg_mbps,
                    leaf,
                ));
                info!(
                    target: "sngram::file",
                    dataset = ds.id,
                    idx = i + 1,
                    of = n_files,
                    file = leaf,
                    bytes_kb = bytes / 1_000,
                    cum_gb = format!("{:.2}", bytes_to_f(cum) / 1e9),
                    inst_mbps = format!("{inst_mbps:.0}"),
                    avg_mbps = format!("{avg_mbps:.0}"),
                    file_dt_s = format!("{:.1}", file_dt.as_secs_f64()),
                    "file done"
                );
                while next < milestones.len() && cum >= milestones[next].0 {
                    let label = milestones[next].1;
                    info!(target: "sngram::mint", label, cum_gb = format!("{:.2}", bytes_to_f(cum) / 1e9), "milestone hit");
                    match mint(&counter, &mint_dir, label) {
                        Ok(path) => {
                            say(&format!("** MINT [{label}] -> {}", path.display()));
                            info!(target: "sngram::mint", label, path = %path.display(), "mint written");
                        }
                        Err(e) => {
                            say(&format!("!! WARN mint failed for {label} ({e:#}); continuing"));
                            error!(
                                target: "sngram::mint",
                                label,
                                error = %format!("{e:#}"),
                                "mint failed; continuing"
                            );
                        }
                    }
                    next += 1;
                }
            }
            let _ = prefetch_handle.await;
        }
        say(&format!(
            "-- {} done: {} files counted, {} skipped, {} prefixes missing, {} failed, {:.2} GB",
            ds.id,
            ds_stats.files_ok,
            ds_stats.files_skipped,
            ds_stats.prefixes_missing,
            ds_stats.prefixes_failed,
            bytes_to_f(ds_stats.bytes) / 1e9,
        ));
        info!(
            target: "sngram::dataset",
            dataset = ds.id,
            files_ok = ds_stats.files_ok,
            files_skipped = ds_stats.files_skipped,
            prefixes_missing = ds_stats.prefixes_missing,
            prefixes_failed = ds_stats.prefixes_failed,
            bytes_gb = format!("{:.2}", bytes_to_f(ds_stats.bytes) / 1e9),
            "dataset done"
        );
        overall.add(ds_stats);
    }
    match mint(&counter, &mint_dir, "final") {
        Ok(path) => {
            say(&format!("** MINT [final] -> {}", path.display()));
            info!(target: "sngram::mint", path = %path.display(), "final mint written");
        }
        Err(e) => {
            say(&format!("!! WARN final mint failed ({e:#})"));
            error!(target: "sngram::mint", error = %format!("{e:#}"), "final mint failed");
        }
    }
    let wall = start.elapsed().as_secs();
    let active = first_count_at.map_or(wall, |t| t.elapsed().as_secs());
    let avg = if active > 0 { bytes_to_f(cum) / 1e6 / (active as f64) } else { 0.0 };
    say(&format!(
        "\n=== summary === total: {:.2} GB  wall {} s  active {} s  avg {:.0} MB/s",
        bytes_to_f(cum) / 1e9,
        wall,
        active,
        avg,
    ));
    say(&format!(
        "files: {} counted, {} skipped",
        overall.files_ok, overall.files_skipped
    ));
    say(&format!(
        "prefixes: {} missing on HF (acceptable), {} failed permanently",
        overall.prefixes_missing, overall.prefixes_failed
    ));
    info!(
        target: "sngram::summary",
        total_gb = format!("{:.2}", bytes_to_f(cum) / 1e9),
        wall_s = wall,
        active_s = active,
        avg_mbps = format!("{avg:.0}"),
        files_ok = overall.files_ok,
        files_skipped = overall.files_skipped,
        prefixes_missing = overall.prefixes_missing,
        prefixes_failed = overall.prefixes_failed,
        "run summary"
    );
    Ok(())
}

#[allow(clippy::cast_precision_loss, reason = "stats display only")]
fn bytes_to_f(b: u64) -> f64 {
    b as f64
}

enum ListOutcome {
    Ok,
    Missing,
    Failed,
}

async fn list_with_retry(op: &Operator, prefix: &str) -> (Vec<ParquetFile>, ListOutcome) {
    let mut delay = RETRY_BASE;
    let mut hard_attempts: u32 = 0;
    loop {
        match list_files(op, prefix).await {
            Ok(f) => return (f, ListOutcome::Ok),
            Err(e) if is_transient(&e) => {
                warn!(
                    target: "sngram::list",
                    prefix = %prefix,
                    delay_s = delay.as_secs(),
                    error = %format!("{e:#}"),
                    "transient list error; retrying"
                );
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(RETRY_CAP);
            }
            Err(e) if is_not_found(&e) => {
                debug!(target: "sngram::list", prefix = %prefix, "not found");
                return (Vec::new(), ListOutcome::Missing);
            }
            Err(e) => {
                hard_attempts += 1;
                if hard_attempts >= HARD_RETRY_MAX {
                    error!(
                        target: "sngram::list",
                        prefix = %prefix,
                        attempts = hard_attempts,
                        error = %format!("{e:#}"),
                        "list permanently failed"
                    );
                    return (Vec::new(), ListOutcome::Failed);
                }
                warn!(
                    target: "sngram::list",
                    prefix = %prefix,
                    attempt = hard_attempts,
                    max = HARD_RETRY_MAX,
                    delay_s = delay.as_secs(),
                    error = %format!("{e:#}"),
                    "list hard error; retrying"
                );
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(RETRY_CAP);
            }
        }
    }
}

struct PrefetchItem {
    idx: usize,
    file: ParquetFile,
    opened: anyhow::Result<FileStream>,
}

/// Open a file's stream with inline retry on transient errors. Used by the
/// prefetch task so the main loop can hand the already-opened stream
/// directly to `drain_stream`, overlapping open of file N+1 with drain of N.
async fn open_with_inline_retry(
    op: &Operator,
    file: &ParquetFile,
    field: &str,
) -> anyhow::Result<FileStream> {
    let mut delay = RETRY_BASE;
    let mut hard_attempts: u32 = 0;
    loop {
        match open_stream(op, file, field).await {
            Ok(s) => return Ok(s),
            Err(e) if is_not_found(&e) => return Err(e),
            Err(e) if is_transient(&e) => {
                warn!(
                    target: "sngram::file",
                    path = %file.path,
                    delay_s = delay.as_secs(),
                    error = %format!("{e:#}"),
                    "transient open error; retrying"
                );
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(RETRY_CAP);
            }
            Err(e) => {
                hard_attempts += 1;
                if hard_attempts >= HARD_RETRY_MAX {
                    return Err(e);
                }
                warn!(
                    target: "sngram::file",
                    path = %file.path,
                    attempt = hard_attempts,
                    max = HARD_RETRY_MAX,
                    delay_s = delay.as_secs(),
                    error = %format!("{e:#}"),
                    "hard open error; retrying"
                );
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(RETRY_CAP);
            }
        }
    }
}

/// Drain a file into the counter, with inline retry on transient errors.
/// Accepts a possibly-already-opened stream from the prefetcher; if drain
/// fails transiently we re-open the file from scratch.
async fn drain_with_inline_retry(
    op: &Operator,
    file: &ParquetFile,
    field: &str,
    mut prefetched: Option<FileStream>,
    counter: &BigramCounter,
) -> anyhow::Result<u64> {
    let mut delay = RETRY_BASE;
    let mut hard_attempts: u32 = 0;
    loop {
        let stream = match prefetched.take() {
            Some(s) => s,
            None => match open_stream(op, file, field).await {
                Ok(s) => s,
                Err(e) if is_not_found(&e) => return Err(e),
                Err(e) if is_transient(&e) => {
                    warn!(
                        target: "sngram::file",
                        path = %file.path,
                        delay_s = delay.as_secs(),
                        error = %format!("{e:#}"),
                        "transient reopen error; retrying"
                    );
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(RETRY_CAP);
                    continue;
                }
                Err(e) => {
                    hard_attempts += 1;
                    if hard_attempts >= HARD_RETRY_MAX {
                        return Err(e);
                    }
                    warn!(
                        target: "sngram::file",
                        path = %file.path,
                        attempt = hard_attempts,
                        delay_s = delay.as_secs(),
                        error = %format!("{e:#}"),
                        "hard reopen error; retrying"
                    );
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(RETRY_CAP);
                    continue;
                }
            },
        };
        match drain_stream(stream, file, counter).await {
            Ok(b) => return Ok(b),
            Err(e) if is_not_found(&e) => return Err(e),
            Err(e) if is_transient(&e) => {
                warn!(
                    target: "sngram::file",
                    path = %file.path,
                    delay_s = delay.as_secs(),
                    error = %format!("{e:#}"),
                    "transient drain error (timeout / rate-limit / conn issue); reopening"
                );
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(RETRY_CAP);
            }
            Err(e) => {
                hard_attempts += 1;
                if hard_attempts >= HARD_RETRY_MAX {
                    return Err(e);
                }
                warn!(
                    target: "sngram::file",
                    path = %file.path,
                    attempt = hard_attempts,
                    max = HARD_RETRY_MAX,
                    delay_s = delay.as_secs(),
                    error = %format!("{e:#}"),
                    "hard drain error; reopening"
                );
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(RETRY_CAP);
            }
        }
    }
}
