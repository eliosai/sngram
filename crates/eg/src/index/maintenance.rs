//! `--index=verify` and `--index=repair` handling.

use std::io::Write;

use crate::flags::HiArgs;

use super::{config, location, manifest, postings, roots::SearchRoots, walk};

pub fn run(args: &HiArgs) -> anyhow::Result<bool> {
    let table = sngram_weights::weights();
    let table_fingerprint = table.fingerprint();
    let roots = SearchRoots::from_args(args)?;
    let location = location::resolve(args, roots.build_root().path())?;
    let index_dir = location.index_dir();
    if !matches!(args.index().backend(), config::IndexBackend::Postings) {
        report_line(
            "eg index verify: only the postings backend is verifiable (tantivy is experimental)",
        );
        return Ok(false);
    }
    let index_home = index_dir.join("postings-v5");
    let report = postings::verify_index(&index_home, table_fingerprint)?;
    for line in report.lines() {
        report_line(&line);
    }
    if report.healthy() {
        report_line("eg index verify: index is healthy");
        return Ok(true);
    }
    if matches!(args.index().mode(), config::IndexMode::Repair) {
        report_line("eg index repair: fault found, rebuilding");
        rebuild_for_repair(args, table_fingerprint, &table, &location, &index_home)?;
        report_line("eg index repair: rebuild complete");
        return Ok(true);
    }
    report_line("eg index verify: index is unhealthy (run --index=repair to rebuild)");
    Ok(false)
}

fn rebuild_for_repair(
    args: &HiArgs,
    table_fingerprint: u64,
    table: &sngram_types::WeightTable,
    location: &location::IndexLocation,
    index_home: &std::path::Path,
) -> anyhow::Result<()> {
    let collected = walk::collect_haystacks(args, &location.state_root)?;
    let snapshot = manifest::current_snapshot(
        args,
        &location.corpus_root,
        &collected.haystacks,
        &collected.dirs,
    )?;
    postings::rebuild(args, table_fingerprint, table, index_home, &snapshot)?;
    Ok(())
}

fn report_line(line: &str) {
    let _ = writeln!(std::io::stdout().lock(), "{line}");
}
