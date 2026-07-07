//! Embedded indexed-vs-unindexed regex benchmark suite.

use std::{
    collections::BTreeMap,
    env,
    ffi::OsString,
    io::{self, Write},
    path::{Path, PathBuf},
    process::Command,
    time::Instant,
};

use anyhow::Context as _;
use serde::Deserialize;

use crate::flags::{HiArgs, Mode};

const SUITE_TSV: &str = include_str!("data/fp-queries.tsv");

pub fn run(args: &HiArgs) -> anyhow::Result<bool> {
    let Mode::Search(_) = args.mode() else {
        anyhow::bail!("--bench only supports search paths");
    };
    anyhow::ensure!(
        args.search_paths()
            .iter()
            .all(|path| path != Path::new("-")),
        "--bench benchmarks files and directories, not stdin"
    );

    let suite = BenchSuite::embedded()?;
    let exe = env::current_exe().context("failed to locate eg executable")?;
    let table = BenchTable::new(suite.run(args, &exe)?);
    table.print()?;
    let false_negatives = table.false_negative_ids();
    anyhow::ensure!(
        false_negatives.is_empty(),
        "false negatives: indexed hits diverge from scan hits for {}",
        false_negatives.join(", ")
    );
    Ok(true)
}

struct BenchSuite {
    cases: Vec<BenchCase>,
}

impl BenchSuite {
    fn embedded() -> anyhow::Result<Self> {
        parse_suite(SUITE_TSV)
    }

    fn run(&self, args: &HiArgs, exe: &Path) -> anyhow::Result<BenchRun> {
        let warm = self.warm(args, exe)?;
        let rows = self
            .cases
            .iter()
            .map(|case| CaseRunner::new(args, exe, case).run())
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(BenchRun { warm, rows })
    }

    fn warm(&self, args: &HiArgs, exe: &Path) -> anyhow::Result<Option<IndexedMeasurement>> {
        let Some(case) = self.cases.first() else {
            return Ok(None);
        };
        CaseRunner::new(args, exe, case).indexed().map(Some)
    }
}

struct BenchRun {
    warm: Option<IndexedMeasurement>,
    rows: Vec<BenchRow>,
}

struct BenchCase {
    id: String,
    pattern: String,
    flags: Vec<String>,
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
        let rg = self.rg()?;
        Ok(BenchRow::new(self.case, indexed, unindexed, rg))
    }

    fn indexed(&self) -> anyhow::Result<IndexedMeasurement> {
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
        args.extend(self.case.flags.iter().map(OsString::from));
        args.push(OsString::from("--"));
        args.push(OsString::from(&self.case.pattern));
        args.extend(search_paths(self.args));
        measure_command(self.exe, args).map(IndexedMeasurement::from_run)
    }

    fn unindexed(&self) -> anyhow::Result<UnindexedMeasurement> {
        let mut args = vec![
            OsString::from("--no-index"),
            OsString::from("--files-with-matches"),
        ];
        args.extend(self.case.flags.iter().map(OsString::from));
        args.push(OsString::from("--"));
        args.push(OsString::from(&self.case.pattern));
        args.extend(search_paths(self.args));
        measure_command(self.exe, args).map(UnindexedMeasurement::from_run)
    }

    fn rg(&self) -> anyhow::Result<Option<UnindexedMeasurement>> {
        let Some(rg) = binary_in_path("rg") else {
            return Ok(None);
        };
        let mut args = vec![OsString::from("--files-with-matches")];
        args.extend(self.case.flags.iter().map(OsString::from));
        args.push(OsString::from("--"));
        args.push(OsString::from(&self.case.pattern));
        args.extend(search_paths(self.args));
        measure_command(&rg, args)
            .map(UnindexedMeasurement::from_run)
            .map(Some)
    }
}

fn search_paths(args: &HiArgs) -> impl Iterator<Item = OsString> + '_ {
    args.search_paths()
        .iter()
        .map(|path| path.as_os_str().to_os_string())
}

struct MeasuredCommand {
    code: Option<i32>,
    stdout: String,
    wall_ms: f64,
}

fn measure_command(exe: &Path, args: Vec<OsString>) -> anyhow::Result<MeasuredCommand> {
    let started = Instant::now();
    let output = Command::new(exe).args(args).output()?;
    Ok(MeasuredCommand {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        wall_ms: started.elapsed().as_secs_f64() * 1000.0,
    })
}

fn binary_in_path(name: &str) -> Option<PathBuf> {
    env::var_os("PATH").and_then(|paths| {
        env::split_paths(&paths)
            .map(|path| path.join(name))
            .find(|path| path.is_file())
    })
}

struct IndexedMeasurement {
    run: MeasuredCommand,
    report: Option<BenchJson>,
}

impl IndexedMeasurement {
    fn from_run(run: MeasuredCommand) -> Self {
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

struct UnindexedMeasurement {
    run: MeasuredCommand,
    matched_files: u64,
}

impl UnindexedMeasurement {
    fn from_run(run: MeasuredCommand) -> Self {
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
    indexed: IndexedMeasurement,
    unindexed: UnindexedMeasurement,
    rg: Option<UnindexedMeasurement>,
}

impl BenchRow {
    fn new(
        case: &BenchCase,
        indexed: IndexedMeasurement,
        unindexed: UnindexedMeasurement,
        rg: Option<UnindexedMeasurement>,
    ) -> Self {
        Self {
            id: case.id.clone(),
            pattern: case.pattern.clone(),
            indexed,
            unindexed,
            rg,
        }
    }

    fn false_negative(&self) -> bool {
        let Some(report) = self.indexed.report.as_ref() else {
            return false;
        };
        self.indexed.ok()
            && self.unindexed.ok()
            && report.counts.matched_files != self.unindexed.matched_files
    }

    fn scan_speedup(&self) -> Option<f64> {
        (self.indexed.ok() && self.unindexed.ok() && self.indexed.run.wall_ms > 0.0)
            .then_some(self.unindexed.run.wall_ms / self.indexed.run.wall_ms)
    }

    fn rg_speedup(&self) -> Option<f64> {
        let rg = self.rg.as_ref()?;
        (self.indexed.ok() && rg.ok() && self.indexed.run.wall_ms > 0.0)
            .then_some(rg.run.wall_ms / self.indexed.run.wall_ms)
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
    bytes: BytesJson,
    #[serde(default)]
    selectivity_rejected: bool,
    #[serde(default)]
    query_too_broad: bool,
}

#[derive(Clone, Copy, Default, Deserialize)]
struct BytesJson {
    #[serde(default)]
    mmap_bytes: u64,
    #[serde(default)]
    corpus_text_bytes: u64,
}

#[derive(Clone, Copy, Default, Deserialize)]
struct TimingsJson {
    #[serde(default)]
    total: f64,
    #[serde(default)]
    cold_build_total: f64,
    #[serde(default)]
    index_open: f64,
    #[serde(default)]
    index_lookup: f64,
    #[serde(default)]
    verify_haystacks: f64,
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
    warm: Option<IndexedMeasurement>,
    rows: Vec<BenchRow>,
}

impl BenchTable {
    fn new(run: BenchRun) -> Self {
        Self {
            warm: run.warm,
            rows: run.rows,
        }
    }

    fn print(&self) -> anyhow::Result<()> {
        let stdout = io::stdout();
        let mut out = stdout.lock();
        self.write_header(&mut out)?;
        for row in &self.rows {
            self.write_row(&mut out, row)?;
        }
        self.write_class_summaries(&mut out)?;
        self.write_summary(&mut out)?;
        Ok(())
    }

    fn write_class_summaries(&self, out: &mut impl Write) -> io::Result<()> {
        let mut classes: BTreeMap<&str, Vec<&BenchRow>> = BTreeMap::new();
        for row in &self.rows {
            let class = row.id.split('_').next().unwrap_or(&row.id);
            classes.entry(class).or_default().push(row);
        }
        for (class, rows) in classes {
            let summary = Summary::from_rows(rows.into_iter());
            writeln!(
                out,
                "class {:<10} regexes={} candidates={} verified={} matches={} false_positives={} false_positive_pct={:.2}",
                class,
                summary.regexes,
                summary.candidates,
                summary.verified,
                summary.matches,
                summary.false_positives,
                summary.false_positive_pct(),
            )?;
        }
        Ok(())
    }

    fn false_negative_ids(&self) -> Vec<&str> {
        self.rows
            .iter()
            .filter(|row| row.false_negative())
            .map(|row| row.id.as_str())
            .collect()
    }

    fn index_bytes(&self) -> BytesJson {
        self.warm
            .iter()
            .chain(self.rows.iter().map(|row| &row.indexed))
            .filter_map(|measurement| measurement.report.as_ref())
            .map(|report| report.bytes)
            .find(|bytes| bytes.mmap_bytes > 0)
            .unwrap_or_default()
    }

    fn write_header(&self, out: &mut impl Write) -> io::Result<()> {
        writeln!(
            out,
            "{:<18} {:<28} {:>9} {:>9} {:>9} {:>9} {:>7} {:>7} {:>8} {:>7} {:>7} {:>7} {:>6} {:>6} {:>6} {:>7} {:>6} {:>6} status",
            "regex",
            "pattern",
            "idx_wall",
            "idx_total",
            "scan_wall",
            "rg_wall",
            "scan_x",
            "rg_x",
            "cold",
            "open",
            "lookup",
            "verify",
            "cand",
            "ver",
            "hit",
            "fp_pct",
            "scan_hit",
            "rg_hit",
        )
    }

    fn write_row(&self, out: &mut impl Write, row: &BenchRow) -> io::Result<()> {
        let report = row.indexed.report.as_ref();
        let timings = report.map_or_else(TimingsJson::default, |report| report.timings_ms);
        let counts = report.map_or_else(CountsJson::default, |report| report.counts);
        let fps = report.map_or_else(FalsePositiveJson::default, |report| report.false_positives);
        let rg_wall = row.rg.as_ref().map_or(0.0, |rg| rg.run.wall_ms);
        let rg_hits = row.rg.as_ref().map_or(0, |rg| rg.matched_files);
        writeln!(
            out,
            "{:<18} {:<28} {:>9.2} {:>9.2} {:>9.2} {:>9.2} {:>7} {:>7} {:>8.2} {:>7.2} {:>7.2} {:>7.2} {:>6} {:>6} {:>6} {:>7.2} {:>6} {:>6} {}",
            cell(&row.id, 18),
            cell(&row.pattern, 28),
            row.indexed.run.wall_ms,
            timings.total,
            row.unindexed.run.wall_ms,
            rg_wall,
            Speed(row.scan_speedup()),
            Speed(row.rg_speedup()),
            timings.cold_build_total,
            timings.index_open,
            timings.index_lookup,
            timings.verify_haystacks,
            counts.candidate_files,
            counts.verified_files,
            counts.matched_files,
            fps.false_positive_pct,
            row.unindexed.matched_files,
            rg_hits,
            row.indexed.status(),
        )
    }

    fn write_summary(&self, out: &mut impl Write) -> io::Result<()> {
        let summary = Summary::from_rows(&self.rows);
        let bytes = self.index_bytes();
        writeln!(
            out,
            "summary regexes={} indexed_ok={} unsupported={} scan_ok={} rg_ok={} warm_wall_ms={:.2} idx_wall_ms={:.2} scan_wall_ms={:.2} rg_wall_ms={:.2} speedup_scan={} speedup_rg={} candidates={} verified={} matches={} false_positives={} false_positive_pct={:.2} false_negative_rows={} index_bytes={} corpus_bytes={} index_ratio={}",
            summary.regexes,
            summary.indexed_ok,
            summary.unsupported,
            summary.scan_ok,
            summary.rg_ok,
            self.warm.as_ref().map_or(0.0, |warm| warm.run.wall_ms),
            summary.indexed_wall_ms,
            summary.scan_wall_ms,
            summary.rg_wall_ms,
            Speed(summary.scan_speedup()),
            Speed(summary.rg_speedup()),
            summary.candidates,
            summary.verified,
            summary.matches,
            summary.false_positives,
            summary.false_positive_pct(),
            self.false_negative_ids().len(),
            bytes.mmap_bytes,
            bytes.corpus_text_bytes,
            Ratio(bytes.mmap_bytes, bytes.corpus_text_bytes),
        )
    }
}

#[derive(Default)]
struct Summary {
    regexes: usize,
    indexed_ok: usize,
    unsupported: usize,
    scan_ok: usize,
    rg_ok: usize,
    indexed_wall_ms: f64,
    scan_wall_ms: f64,
    rg_wall_ms: f64,
    candidates: u64,
    verified: u64,
    matches: u64,
    false_positives: u64,
}

impl Summary {
    fn from_rows<'a>(rows: impl IntoIterator<Item = &'a BenchRow>) -> Self {
        let mut summary = Self::default();
        for row in rows {
            summary.regexes += 1;
            summary.add(row);
        }
        summary
    }

    fn add(&mut self, row: &BenchRow) {
        self.indexed_wall_ms += row.indexed.run.wall_ms;
        self.scan_wall_ms += row.unindexed.run.wall_ms;
        self.indexed_ok += usize::from(row.indexed.ok());
        self.scan_ok += usize::from(row.unindexed.ok());
        if let Some(rg) = row.rg.as_ref() {
            self.rg_wall_ms += rg.run.wall_ms;
            self.rg_ok += usize::from(rg.ok());
        }
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

    fn scan_speedup(&self) -> Option<f64> {
        (self.indexed_wall_ms > 0.0).then_some(self.scan_wall_ms / self.indexed_wall_ms)
    }

    fn rg_speedup(&self) -> Option<f64> {
        (self.indexed_wall_ms > 0.0 && self.rg_ok > 0)
            .then_some(self.rg_wall_ms / self.indexed_wall_ms)
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

struct Ratio(u64, u64);

impl std::fmt::Display for Ratio {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.1 == 0 {
            return f.write_str("n/a");
        }
        write!(f, "{:.2}", self.0 as f64 / self.1 as f64)
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
        if line.trim_start().starts_with('#') {
            continue;
        }
        cases.push(parse_case(line_number, line)?);
    }
    anyhow::ensure!(!cases.is_empty(), "benchmark suite is empty");
    Ok(BenchSuite { cases })
}

fn parse_case(line_number: usize, line: &str) -> anyhow::Result<BenchCase> {
    let columns = line.split('\t').collect::<Vec<_>>();
    anyhow::ensure!(
        (2..=3).contains(&columns.len()),
        "invalid benchmark TSV line {line_number}: expected two or three columns"
    );
    let id = columns[0];
    let pattern = columns[1];
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
        flags: columns
            .get(2)
            .map_or_else(Vec::new, |flags| parse_flags(flags)),
    })
}

fn parse_flags(flags: &str) -> Vec<String> {
    flags.split_whitespace().map(ToString::to_string).collect()
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
    use super::{
        BenchCase, BenchJson, BenchRow, BenchRun, BenchTable, CountsJson, IndexedMeasurement,
        MeasuredCommand, Summary, UnindexedMeasurement, cell, parse_suite,
    };

    fn command(stdout: &str) -> MeasuredCommand {
        MeasuredCommand {
            code: Some(0),
            stdout: stdout.to_string(),
            wall_ms: 1.0,
        }
    }

    fn indexed(ok: bool, matched_files: u64) -> IndexedMeasurement {
        IndexedMeasurement {
            run: command(""),
            report: Some(BenchJson {
                ok,
                counts: CountsJson {
                    matched_files,
                    ..CountsJson::default()
                },
                ..BenchJson::default()
            }),
        }
    }

    fn scanned(matched_files: u64) -> UnindexedMeasurement {
        UnindexedMeasurement {
            run: command(""),
            matched_files,
        }
    }

    fn row(id: &str, indexed_hits: u64, scan_hits: u64) -> BenchRow {
        BenchRow::new(
            &BenchCase {
                id: id.to_string(),
                pattern: "p".to_string(),
                flags: Vec::new(),
            },
            indexed(true, indexed_hits),
            scanned(scan_hits),
            None,
        )
    }

    #[test]
    fn false_negative_rows_are_detected() {
        let rows = vec![row("lit_ok", 5, 5), row("gap_fn", 3, 5)];
        let table = BenchTable::new(BenchRun { warm: None, rows });
        assert_eq!(vec!["gap_fn"], table.false_negative_ids());

        let mut out = Vec::new();
        table.write_summary(&mut out).expect("summary");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("false_negative_rows=1"));
    }

    fn row_with_fps(id: &str, verified: u64, matched: u64) -> BenchRow {
        let mut row = row(id, matched, matched);
        let report = row.indexed.report.as_mut().expect("report");
        report.counts.verified_files = verified;
        report.false_positives.false_positive_files = verified - matched;
        row
    }

    #[test]
    fn summary_reports_index_to_corpus_ratio() {
        let mut sized = row("lit_sized", 1, 1);
        let report = sized.indexed.report.as_mut().expect("report");
        report.bytes.mmap_bytes = 500;
        report.bytes.corpus_text_bytes = 1000;
        let table = BenchTable::new(BenchRun {
            warm: None,
            rows: vec![sized],
        });

        let mut out = Vec::new();
        table.write_summary(&mut out).expect("summary");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("index_bytes=500"));
        assert!(text.contains("corpus_bytes=1000"));
        assert!(text.contains("index_ratio=0.50"));
    }

    #[test]
    fn class_summaries_group_rows_by_id_prefix() {
        let rows = vec![
            row_with_fps("lit_rare", 10, 5),
            row_with_fps("lit_common", 10, 10),
            row_with_fps("gap_pair", 4, 0),
        ];
        let table = BenchTable::new(BenchRun { warm: None, rows });

        let mut out = Vec::new();
        table.write_class_summaries(&mut out).expect("classes");
        let text = String::from_utf8(out).expect("utf8");
        let lines: Vec<&str> = text.lines().collect();

        assert_eq!(2, lines.len());
        assert!(lines[0].starts_with("class gap"));
        assert!(lines[0].contains("regexes=1"));
        assert!(lines[0].contains("false_positive_pct=100.00"));
        assert!(lines[1].starts_with("class lit"));
        assert!(lines[1].contains("regexes=2"));
        assert!(lines[1].contains("false_positive_pct=25.00"));
    }

    #[test]
    fn unsupported_rows_are_not_false_negatives() {
        let unsupported = BenchRow::new(
            &BenchCase {
                id: "broad".to_string(),
                pattern: "p".to_string(),
                flags: Vec::new(),
            },
            indexed(false, 0),
            scanned(9),
            None,
        );
        let table = BenchTable::new(BenchRun {
            warm: None,
            rows: vec![unsupported],
        });
        assert!(table.false_negative_ids().is_empty());
    }

    #[test]
    fn embedded_suite_has_many_regexes() {
        let suite = parse_suite(super::SUITE_TSV).expect("suite parses");
        assert!(suite.cases.len() >= 290);
        assert_eq!("lit_rare", suite.cases[0].id);
        assert_eq!("sched_clock", suite.cases[0].pattern);
    }

    #[test]
    fn embedded_suite_covers_simple_query_classes() {
        let suite = parse_suite(super::SUITE_TSV).expect("suite parses");
        for class in ["simple_", "prose_", "zero_", "gap_", "anchor_", "config_"] {
            assert!(
                suite.cases.iter().any(|case| case.id.starts_with(class)),
                "missing class {class}"
            );
        }
    }

    #[test]
    fn suite_flags_are_preserved() {
        let suite = parse_suite("fixed\tkfree(skb)\t-F\nword\tkfree\t-i -w").expect("suite parses");

        assert_eq!(vec!["-F"], suite.cases[0].flags);
        assert_eq!(vec!["-i", "-w"], suite.cases[1].flags);
    }

    #[test]
    fn invalid_suite_line_is_rejected() {
        let err = parse_suite("id\tpattern\nmissing")
            .err()
            .expect("invalid suite should fail");
        assert!(err.to_string().contains("expected two or three columns"));
    }

    #[test]
    fn table_summary_formats_zero_rows() {
        let mut out = Vec::new();
        BenchTable::new(BenchRun {
            warm: None,
            rows: Vec::new(),
        })
        .write_summary(&mut out)
        .expect("summary");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("summary regexes=0"));
        assert!(text.contains("speedup_scan=n/a"));
        assert!(text.contains("speedup_rg=n/a"));
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
