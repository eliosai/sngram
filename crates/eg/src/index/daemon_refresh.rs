//! Refresh worker entrypoint used by the index daemon.

use crate::flags::{HiArgs, Mode};

use super::{
    catalog::GenerationCatalog, config, manifest, postings, request, roots::SearchRoots, runtime,
    store, walk,
};

pub fn run(args: &HiArgs) -> anyhow::Result<()> {
    let Mode::Search(_) = args.mode() else {
        return Ok(());
    };
    if args.index().is_no_index()
        || args.index().is_maintenance()
        || request::searches_stdin(args)
        || matches!(args.index().backend(), config::IndexBackend::TantivyRam)
    {
        return Ok(());
    }

    let table = sngram_weights::weights();
    let table_fingerprint = table.fingerprint();
    let roots = SearchRoots::from_args(args)?;
    let catalog = GenerationCatalog::open(args, table_fingerprint);
    let generation = catalog.best_ready_generation(&roots)?;
    let location = generation.location;
    runtime::clear_journal_clean(&location.state_root);

    let collected = walk::collect_haystacks(args, &location.state_root)?;
    let snapshot = manifest::current_snapshot(
        args,
        &location.corpus_root,
        &collected.haystacks,
        &collected.dirs,
    )?;
    match args.index().backend() {
        config::IndexBackend::Postings => postings::refresh_index(
            args,
            table_fingerprint,
            &table,
            &location.index_dir().join("postings-v5"),
            &snapshot,
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
            )?;
        },
        config::IndexBackend::TantivyRam => unreachable!("filtered above"),
    }
    runtime::mark_journal_clean(&location.state_root)?;
    Ok(())
}
