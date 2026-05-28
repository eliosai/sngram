use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use opendal::Operator;

use crate::counter::BigramCounter;
use crate::mint::mint;
use crate::paths::ensure_dir;
use crate::source::{
    DATASETS, ParquetFile, count_file, hf_operator, is_not_found, is_transient, list_files,
};

pub type Milestone = (u64, &'static str);

const HARD_RETRY_MAX: u32 = 5;

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

    for ds in DATASETS {
        say(&format!("\n====== {} ({}) ======", ds.id, ds.repo));
        let op = match hf_operator(ds.repo, Some(&token)) {
            Ok(op) => op,
            Err(e) => {
                say(&format!(
                    "!! WARN cannot build operator for {} ({e:#}); skipping whole dataset",
                    ds.repo
                ));
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
            let (files, list_outcome) = list_with_retry(&op, prefix).await;
            match list_outcome {
                ListOutcome::Ok => {}
                ListOutcome::Missing => {
                    say(&format!("   .. prefix not found on HF: {prefix} (skipping)"));
                    ds_stats.prefixes_missing += 1;
                    continue;
                }
                ListOutcome::Failed => {
                    say(&format!(
                        "!! WARN list failed permanently for {prefix} after {HARD_RETRY_MAX} retries; skipping"
                    ));
                    ds_stats.prefixes_failed += 1;
                    continue;
                }
            }
            if files.is_empty() {
                say("   (no parquet files in this prefix)");
                continue;
            }
            say(&format!("   {} files", files.len()));
            for (i, f) in files.iter().enumerate() {
                let t0 = Instant::now();
                let bytes = match count_with_retry(&op, f, ds.field, &counter).await {
                    Ok(b) => b,
                    Err(e) => {
                        say(&format!("!! skip {} ({e:#})", f.path));
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
                let active_secs = first_count_at
                    .map(|t| t.elapsed().as_secs_f64().max(1.0))
                    .unwrap_or(1.0);
                let avg_mbps = (cum as f64) / 1e6 / active_secs;
                let inst_mbps = (bytes as f64) / 1e6 / file_dt.as_secs_f64().max(0.001);
                let leaf = f.path.rsplit('/').next().unwrap_or(&f.path);
                say(&format!(
                    "   [{:>4}/{:<4}] +{:>7} KB  cum {:>7.2} GB  now {:>4.0} MB/s  avg {:>4.0} MB/s  {}",
                    i + 1,
                    files.len(),
                    bytes / 1_000,
                    (cum as f64) / 1e9,
                    inst_mbps,
                    avg_mbps,
                    leaf,
                ));
                while next < milestones.len() && cum >= milestones[next].0 {
                    if let Err(e) = mint(&counter, &mint_dir, milestones[next].1) {
                        say(&format!(
                            "!! WARN mint failed for {} ({e:#}); continuing",
                            milestones[next].1
                        ));
                    }
                    next += 1;
                }
            }
        }
        say(&format!(
            "-- {} done: {} files counted, {} skipped, {} prefixes missing, {} prefix-level failures, {:.2} GB",
            ds.id,
            ds_stats.files_ok,
            ds_stats.files_skipped,
            ds_stats.prefixes_missing,
            ds_stats.prefixes_failed,
            (ds_stats.bytes as f64) / 1e9,
        ));
        overall.add(ds_stats);
    }
    if let Err(e) = mint(&counter, &mint_dir, "final") {
        say(&format!("!! WARN final mint failed ({e:#})"));
    }
    let wall = start.elapsed().as_secs();
    let active = first_count_at
        .map(|t| t.elapsed().as_secs())
        .unwrap_or(wall);
    let avg = if active > 0 { (cum as f64) / 1e6 / (active as f64) } else { 0.0 };
    say(&format!(
        "\n=== summary ===\ntotal: {:.2} GB  wall {} s  active {} s  avg {:.0} MB/s",
        (cum as f64) / 1e9,
        wall,
        active,
        avg,
    ));
    say(&format!(
        "files: {} counted, {} skipped",
        overall.files_ok, overall.files_skipped
    ));
    say(&format!(
        "prefixes: {} missing on HF (acceptable), {} failed permanently (look into these)",
        overall.prefixes_missing, overall.prefixes_failed
    ));
    Ok(())
}

fn say(msg: &str) {
    println!("{msg}");
    let _ = std::io::stdout().flush();
}

enum ListOutcome {
    Ok,
    Missing,
    Failed,
}

async fn list_with_retry(op: &Operator, prefix: &str) -> (Vec<ParquetFile>, ListOutcome) {
    let mut delay = Duration::from_secs(2);
    let mut hard_attempts: u32 = 0;
    loop {
        match list_files(op, prefix).await {
            Ok(f) => return (f, ListOutcome::Ok),
            Err(e) if is_transient(&e) => {
                say(&format!(
                    "   .. list rate-limited on {prefix} ({e:#}); retry in {}s",
                    delay.as_secs()
                ));
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(60));
            }
            Err(e) if is_not_found(&e) => return (Vec::new(), ListOutcome::Missing),
            Err(e) => {
                hard_attempts += 1;
                if hard_attempts >= HARD_RETRY_MAX {
                    say(&format!("!! list error final on {prefix} after {hard_attempts} tries: {e:#}"));
                    return (Vec::new(), ListOutcome::Failed);
                }
                say(&format!(
                    "   .. list error on {prefix} ({e:#}); try {hard_attempts}/{HARD_RETRY_MAX} in {}s",
                    delay.as_secs()
                ));
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(60));
            }
        }
    }
}

async fn count_with_retry(
    op: &Operator,
    file: &ParquetFile,
    field: &str,
    counter: &BigramCounter,
) -> anyhow::Result<u64> {
    let mut delay = Duration::from_secs(2);
    let mut hard_attempts: u32 = 0;
    loop {
        match count_file(op, file, field, counter).await {
            Ok(b) => return Ok(b),
            Err(e) if is_transient(&e) => {
                say(&format!(
                    "   .. rate-limited on {} ({e:#}); retry in {}s",
                    file.path,
                    delay.as_secs()
                ));
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(60));
            }
            Err(e) if is_not_found(&e) => return Err(e),
            Err(e) => {
                hard_attempts += 1;
                if hard_attempts >= HARD_RETRY_MAX {
                    return Err(e);
                }
                say(&format!(
                    "   .. error on {} ({e:#}); try {hard_attempts}/{HARD_RETRY_MAX} in {}s",
                    file.path,
                    delay.as_secs()
                ));
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(60));
            }
        }
    }
}
