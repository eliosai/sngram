//! Embedded indexed-vs-unindexed regex benchmark suite.

use std::{
    ffi::OsString,
    io::{self, Write},
    path::Path,
    process::Command,
    time::Instant,
};

use anyhow::Context as _;
use serde::Deserialize;

use crate::flags::{HiArgs, Mode};

const SUITE_TSV: &str = include_str!("bench_suite.tsv");

pub fn run(args: &HiArgs) -> anyhow::Result<bool> {
    let Mode::Search(_) = args.mode() else {
        anyhow::bail!("--bench-suite only supports search paths");
    };
    anyhow::ensure!(
        args.search_paths()
            .iter()
            .all(|path| path != Path::new("-")),
        "--bench-suite benchmarks files and directories, not stdin"
    );

    let suite = BenchSuite::embedded()?;
    let exe = std::env::current_exe().context("failed to locate eg executable")?;
    let rows = suite.run(args, &exe)?;
    BenchTable::new(rows).print()?;
    Ok(true)
}

struct BenchSuite {
    cases: Vec<BenchCase>,
}

impl BenchSuite {
    fn embedded() -> anyhow::Result<Self> {
        parse_suite(SUITE_TSV)
    }

    fn run(&self, args: &HiArgs, exe: &Path) -> anyhow::Result<Vec<BenchRow>> {
        self.cases
            .iter()
            .map(|case| CaseRunner::new(args, exe, case).run())
            .collect()
    }
}

struct BenchCase {
    id: String,
    pattern: String,
}

struct CaseRunner<'a> {
    args: &'a HiArgs,
    exe: &'a Path,
    case: &'a BenchCase,
}

impl<'a> CaseRunner<'a> {
    fn new(args: &'a HiArgs, exe: &'a Path, case: &'a BenchCase) -> Self {
        Self { args, exe, case }
    }

    fn run(&self) -> anyhow::Result<BenchRow> {
        let indexed = self.indexed()?;
        let unindexed = self.unindexed()?;
        Ok(BenchRow::new(self.case, indexed, unindexed))
    }

    fn indexed(&self) -> anyhow::Result<IndexedStats> {
        let mut args = vec![
            OsString::from("--bench"),
            OsString::from("--index=auto"),
            OsString::from(format!(
                "--index-backend={}",
                self.args.index().backend_name()
            )),
        ];
        if let Some(dir) = self.args.index().dir() {
            args.push(OsString::from("--index-dir"));
            args.push(dir.as_os_str().to_os_string());
        }
        args.push(OsString::from("--"));
        args.push(OsString::from(&self.case.pattern));
        args.extend(search_paths(self.args));
        Ok(IndexedStats::from_run(run_child(self.exe, args)?))
    }

    fn unindexed(&self) -> anyhow::Result<UnindexedStats> {
        let mut args = vec![
            OsString::from("--no-index"),
            OsString::from("--files-with-matches"),
            OsString::from("--"),
            OsString::from(&self.case.pattern),
        ];
        args.extend(search_paths(self.args));
        Ok(UnindexedStats::from_run(run_child(self.exe, args)?))
    }
}

fn search_paths(args: &HiArgs) -> impl Iterator<Item = OsString> + '_ {
    args.search_paths()
        .iter()
        .map(|path| path.as_os_str().to_os_string())
}

struct ChildRun {
    code: Option<i32>,
    stdout: String,
    wall_ms: f64,
}

fn run_child(exe: &Path, args: Vec<OsString>) -> anyhow::Result<ChildRun> {
    let started = Instant::now();
    let output = Command::new(exe).args(args).output()?;
    Ok(ChildRun {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        wall_ms: started.elapsed().as_secs_f64() * 1000.0,
    })
}

struct IndexedStats {
    run: ChildRun,
    report: Option<BenchJson>,
}

impl IndexedStats {
    fn from_run(run: ChildRun) -> Self {
        let report = parse_report(&run.stdout);
        Self { run, report }
    }

    fn ok(&self) -> bool {
        self.report.as_ref().is_some_and(|report| report.ok)
    }

    fn status(&self) -> &'static str {
        let Some(report) = self.report.as_ref() else {
            return "bad_json";
        };
        if report.ok {
            return "ok";
        }
        if report.query_too_broad {
            return "too_broad";
        }
        if report.selectivity_rejected {
            return "too_many_candidates";
        }
        "unsupported"
    }
}

struct UnindexedStats {
    run: ChildRun,
    matched_files: u64,
}

impl UnindexedStats {
    fn from_run(run: ChildRun) -> Self {
        let matched_files = if matches!(run.code, Some(0)) {
            run.stdout.lines().count() as u64
        } else {
            0
        };
        Self { run, matched_files }
    }

    fn ok(&self) -> bool {
        matches!(self.run.code, Some(0 | 1))
    }
}

struct BenchRow {
    id: String,
    pattern: String,
    indexed: IndexedStats,
    unindexed: UnindexedStats,
}

impl BenchRow {
    fn new(case: &BenchCase, indexed: IndexedStats, unindexed: UnindexedStats) -> Self {
        Self {
            id: case.id.clone(),
            pattern: case.pattern.clone(),
            indexed,
            unindexed,
        }
    }

    fn speedup(&self) -> Option<f64> {
        (self.indexed.ok() && self.unindexed.ok() && self.indexed.run.wall_ms > 0.0)
            .then_some(self.unindexed.run.wall_ms / self.indexed.run.wall_ms)
    }
}

#[derive(Default, Deserialize)]
struct BenchJson {
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    timings_ms: TimingsJson,
    #[serde(default)]
    counts: CountsJson,
    #[serde(default)]
    false_positives: FalsePositiveJson,
    #[serde(default)]
    selectivity_rejected: bool,
    #[serde(default)]
    query_too_broad: bool,
}

#[derive(Clone, Copy, Default, Deserialize)]
struct TimingsJson {
    #[serde(default)]
    total: f64,
    #[serde(default)]
    initial_build: f64,
    #[serde(default)]
    index_mmap: f64,
    #[serde(default)]
    index_query: f64,
    #[serde(default)]
    verify_candidates: f64,
}

#[derive(Clone, Copy, Default, Deserialize)]
struct CountsJson {
    #[serde(default)]
    candidate_files: u64,
    #[serde(default)]
    verified_files: u64,
    #[serde(default)]
    matched_files: u64,
}

#[derive(Clone, Copy, Default, Deserialize)]
struct FalsePositiveJson {
    #[serde(default)]
    false_positive_files: u64,
    #[serde(default)]
    false_positive_pct: f64,
}

fn parse_report(stdout: &str) -> Option<BenchJson> {
    serde_json::from_str(stdout).ok()
}

struct BenchTable {
    rows: Vec<BenchRow>,
}

impl BenchTable {
    fn new(rows: Vec<BenchRow>) -> Self {
        Self { rows }
    }

    fn print(&self) -> anyhow::Result<()> {
        let stdout = io::stdout();
        let mut out = stdout.lock();
        self.write_header(&mut out)?;
        for row in &self.rows {
            self.write_row(&mut out, row)?;
        }
        self.write_summary(&mut out)?;
        Ok(())
    }

    fn write_header(&self, out: &mut impl Write) -> io::Result<()> {
        writeln!(
            out,
            "{:<18} {:<28} {:>9} {:>9} {:>9} {:>7} {:>8} {:>7} {:>7} {:>7} {:>6} {:>6} {:>6} {:>7} {:>6} status",
            "regex",
            "pattern",
            "idx_wall",
            "idx_total",
            "rg_wall",
            "speed",
            "build",
            "mmap",
            "query",
            "verify",
            "cand",
            "ver",
            "hit",
            "fp_pct",
            "rg_hit",
        )
    }

    fn write_row(&self, out: &mut impl Write, row: &BenchRow) -> io::Result<()> {
        let report = row.indexed.report.as_ref();
        let timings = report.map_or_else(TimingsJson::default, |report| report.timings_ms);
        let counts = report.map_or_else(CountsJson::default, |report| report.counts);
        let fps = report.map_or_else(FalsePositiveJson::default, |report| report.false_positives);
        writeln!(
            out,
            "{:<18} {:<28} {:>9.2} {:>9.2} {:>9.2} {:>7} {:>8.2} {:>7.2} {:>7.2} {:>7.2} {:>6} {:>6} {:>6} {:>7.2} {:>6} {}",
            cell(&row.id, 18),
            cell(&row.pattern, 28),
            row.indexed.run.wall_ms,
            timings.total,
            row.unindexed.run.wall_ms,
            Speed(row.speedup()),
            timings.initial_build,
            timings.index_mmap,
            timings.index_query,
            timings.verify_candidates,
            counts.candidate_files,
            counts.verified_files,
            counts.matched_files,
            fps.false_positive_pct,
            row.unindexed.matched_files,
            row.indexed.status(),
        )
    }

    fn write_summary(&self, out: &mut impl Write) -> io::Result<()> {
        let summary = Summary::from_rows(&self.rows);
        writeln!(
            out,
            "summary regexes={} indexed_ok={} unsupported={} rg_ok={} idx_wall_ms={:.2} rg_wall_ms={:.2} speedup={} candidates={} verified={} matches={} false_positives={} false_positive_pct={:.2}",
            summary.regexes,
            summary.indexed_ok,
            summary.unsupported,
            summary.rg_ok,
            summary.indexed_wall_ms,
            summary.rg_wall_ms,
            Speed(summary.speedup()),
            summary.candidates,
            summary.verified,
            summary.matches,
            summary.false_positives,
            summary.false_positive_pct(),
        )
    }
}

#[derive(Default)]
struct Summary {
    regexes: usize,
    indexed_ok: usize,
    unsupported: usize,
    rg_ok: usize,
    indexed_wall_ms: f64,
    rg_wall_ms: f64,
    candidates: u64,
    verified: u64,
    matches: u64,
    false_positives: u64,
}

impl Summary {
    fn from_rows(rows: &[BenchRow]) -> Self {
        let mut summary = Self {
            regexes: rows.len(),
            ..Self::default()
        };
        for row in rows {
            summary.add(row);
        }
        summary
    }

    fn add(&mut self, row: &BenchRow) {
        self.indexed_wall_ms += row.indexed.run.wall_ms;
        self.rg_wall_ms += row.unindexed.run.wall_ms;
        self.indexed_ok += usize::from(row.indexed.ok());
        self.rg_ok += usize::from(row.unindexed.ok());
        self.unsupported += usize::from(!row.indexed.ok());
        self.add_report(row.indexed.report.as_ref());
    }

    fn add_report(&mut self, report: Option<&BenchJson>) {
        let Some(report) = report else {
            return;
        };
        self.candidates += report.counts.candidate_files;
        self.verified += report.counts.verified_files;
        self.matches += report.counts.matched_files;
        self.false_positives += report.false_positives.false_positive_files;
    }

    fn speedup(&self) -> Option<f64> {
        (self.indexed_wall_ms > 0.0).then_some(self.rg_wall_ms / self.indexed_wall_ms)
    }

    fn false_positive_pct(&self) -> f64 {
        if self.verified == 0 {
            0.0
        } else {
            (self.false_positives as f64 / self.verified as f64) * 100.0
        }
    }
}

struct Speed(Option<f64>);

impl std::fmt::Display for Speed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            Some(speedup) => write!(f, "{speedup:.2}x"),
            None => f.write_str("n/a"),
        }
    }
}

fn parse_suite(tsv: &str) -> anyhow::Result<BenchSuite> {
    let mut cases = Vec::new();
    for (line_number, line) in tsv.lines().enumerate() {
        let line_number = line_number + 1;
        if line_number == 1 && line == "id\tpattern" {
            continue;
        }
        if line.trim().is_empty() {
            continue;
        }
        cases.push(parse_case(line_number, line)?);
    }
    anyhow::ensure!(!cases.is_empty(), "benchmark suite is empty");
    Ok(BenchSuite { cases })
}

fn parse_case(line_number: usize, line: &str) -> anyhow::Result<BenchCase> {
    let Some((id, pattern)) = line.split_once('\t') else {
        anyhow::bail!("invalid benchmark TSV line {line_number}: missing tab");
    };
    anyhow::ensure!(
        !id.is_empty(),
        "invalid benchmark TSV line {line_number}: empty id"
    );
    anyhow::ensure!(
        !pattern.is_empty(),
        "invalid benchmark TSV line {line_number}: empty pattern"
    );
    Ok(BenchCase {
        id: id.to_string(),
        pattern: pattern.to_string(),
    })
}

fn cell(value: &str, width: usize) -> String {
    let mut chars = value.chars();
    let mut out: String = chars.by_ref().take(width).collect();
    if chars.next().is_some() {
        out.pop();
        out.push('~');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{BenchTable, Summary, cell, parse_suite};

    #[test]
    fn embedded_suite_has_many_regexes() {
        let suite = parse_suite(super::SUITE_TSV).expect("suite parses");
        assert!(suite.cases.len() >= 12);
        assert_eq!("literal_main", suite.cases[0].id);
        assert_eq!("fn main", suite.cases[0].pattern);
    }

    #[test]
    fn invalid_suite_line_is_rejected() {
        let err = match parse_suite("id\tpattern\nmissing-tab") {
            Ok(_) => panic!("invalid suite should fail"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("missing tab"));
    }

    #[test]
    fn table_summary_formats_zero_rows() {
        let mut out = Vec::new();
        BenchTable::new(Vec::new())
            .write_summary(&mut out)
            .expect("summary");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("summary regexes=0"));
        assert!(text.contains("speedup=n/a"));
    }

    #[test]
    fn summary_false_positive_percentage_uses_verified_total() {
        let summary = Summary {
            verified: 8,
            false_positives: 2,
            ..Summary::default()
        };
        assert_eq!(25.0, summary.false_positive_pct());
    }

    #[test]
    fn cells_are_truncated_without_changing_short_values() {
        assert_eq!("literal", cell("literal", 10));
        assert_eq!("abcdefghi~", cell("abcdefghijklmnopqrstuvwxyz", 10));
    }
}
