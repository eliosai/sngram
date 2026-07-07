//! Refresh worker entrypoint used by the index daemon.

use crate::flags::{HiArgs, Mode};

use super::{
    bench,
    catalog::GenerationCatalog,
    config, manifest, postings,
    progress::{BuildPhase, BuildProgress},
    request,
    roots::SearchRoots,
    runtime, store, walk,
};

pub fn run(args: &HiArgs) -> anyhow::Result<()> {
    let Mode::Search(_) = args.mode() else {
        return Ok(());
    };
    if args.index().is_no_index() || request::searches_stdin(args) {
        return Ok(());
    }

    let table = sngram_weights::weights();
    let table_fingerprint = table.fingerprint();
    let roots = SearchRoots::from_args(args)?;
    let catalog = GenerationCatalog::open(args, table_fingerprint);
    let generation = catalog.best_ready_generation(&roots)?;
    let location = generation.location;
    runtime::clear_journal_clean(&location.state_root);
    let progress = BuildProgress::new(&location.state_root);
    progress.clear();
    if args.index().bench() {
        bench::clear_build_timings(&location.state_root);
    }

    let mut build_timings = bench::BuildTimings::default();
    progress.phase(BuildPhase::Walking);
    let walk_started_at = std::time::Instant::now();
    let collected = walk::collect_haystacks(args, &location.state_root, Some(&progress))?;
    build_timings.set_walk_collect(walk_started_at);
    progress.phase(BuildPhase::Snapshot);
    progress.start_snapshot(collected.haystacks.len());
    let snapshot_started_at = std::time::Instant::now();
    let snapshot = manifest::current_snapshot(
        args,
        &location.corpus_root,
        &collected.haystacks,
        &collected.dirs,
        Some(&progress),
    )?;
    build_timings.set_snapshot_build(snapshot_started_at);
    progress.start_scan(snapshot.file_count());
    let backend_timings = match args.index().backend() {
        config::IndexBackend::Postings => postings::refresh_index(
            args,
            table_fingerprint,
            &table,
            &location.index_dir().join("postings-v7"),
            &snapshot,
            Some(&progress),
        )?,
        config::IndexBackend::Tantivy => {
            let (schema, fields) = store::schema();
            store::refresh_index(
                args,
                table_fingerprint,
                &table,
                schema,
                fields,
                &location.index_dir().join("tantivy-v2"),
                &snapshot,
                Some(&progress),
            )?
        },
    };
    build_timings.absorb_backend(backend_timings);
    if args.index().bench() {
        bench::write_build_timings(&location.state_root, &build_timings)?;
    }
    progress.phase(BuildPhase::Ready);
    runtime::mark_journal_clean(&location.state_root)?;
    Ok(())
}
