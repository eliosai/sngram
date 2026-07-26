//! Candidate verification through the copied ripgrep search workers.

use std::{
    collections::BTreeSet,
    mem,
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering},
    },
    time::Instant,
};

use anyhow::bail;

use crate::{
    flags::{HiArgs, SearchMode},
    haystack::Haystack,
};

use super::{bench, manifest, roots::SearchRoots};

pub struct CandidateVerifier<'a, 'b> {
    args: &'a HiArgs,
    mode: SearchMode,
    started_at: Instant,
    roots: &'a SearchRoots,
    snapshot: &'a manifest::CurrentSnapshot,
    candidates: &'a BTreeSet<usize>,
    bench: Option<&'b mut bench::BenchReport>,
}

impl<'a, 'b> CandidateVerifier<'a, 'b> {
    pub const fn new(
        args: &'a HiArgs,
        mode: SearchMode,
        started_at: Instant,
        roots: &'a SearchRoots,
        snapshot: &'a manifest::CurrentSnapshot,
        candidates: &'a BTreeSet<usize>,
        bench: Option<&'b mut bench::BenchReport>,
    ) -> Self {
        Self {
            args,
            mode,
            started_at,
            roots,
            snapshot,
            candidates,
            bench,
        }
    }

    pub fn verify(self) -> anyhow::Result<bool> {
        if let Some(report) = self.bench {
            return verify_for_bench(
                self.args,
                self.mode,
                self.roots,
                self.snapshot,
                self.candidates,
                report,
            );
        }
        if is_full_corpus_mode(self.args, self.mode) {
            return verify_full_corpus(
                self.args,
                self.mode,
                self.started_at,
                self.roots,
                self.snapshot,
                self.candidates,
            );
        }
        let ordered = ordered_candidates(self.snapshot, self.candidates);
        verify_buffered(
            self.args,
            self.mode,
            self.started_at,
            self.roots,
            self.snapshot,
            &ordered,
        )
    }
}

/// Build the haystack for one candidate under the invocation's display rules
fn candidate_haystack(roots: &SearchRoots, file: &manifest::CurrentFile) -> Haystack {
    Haystack::from_index_path(roots.display_path(&file.path), file.is_explicit())
}

/// Candidate document ordinals in the manifest's requested output order.
fn ordered_candidates(
    snapshot: &manifest::CurrentSnapshot,
    candidates: &BTreeSet<usize>,
) -> Vec<usize> {
    let ordered: Vec<usize> = candidates
        .iter()
        .copied()
        .filter(|&ord| ord < snapshot.file_count())
        .collect();
    if snapshot.has_uncovered() {
        return sorted_by_path(snapshot, ordered);
    }
    ordered
}

/// Order a candidate set that mixes indexed files with freshly changed ones
fn sorted_by_path(snapshot: &manifest::CurrentSnapshot, ordered: Vec<usize>) -> Vec<usize> {
    let mut keyed: Vec<(std::path::PathBuf, usize)> = ordered
        .into_iter()
        .filter_map(|ord| snapshot.file(ord).map(|file| (file.path, ord)))
        .collect();
    keyed.sort_unstable();
    keyed.into_iter().map(|(_, ord)| ord).collect()
}

/// Every document ordinal in the manifest's requested output order.
fn all_ordered(snapshot: &manifest::CurrentSnapshot) -> Vec<usize> {
    snapshot.ordinals().collect()
}

/// Candidate files one verify worker owns before a second worker earns its cost
const FILES_PER_VERIFY_WORKER: usize = 24;

/// Return true when the mode reports on the whole corpus, not just matches.
pub fn is_full_corpus_mode(args: &HiArgs, mode: SearchMode) -> bool {
    matches!(mode, SearchMode::FilesWithoutMatch)
        || (args.include_zero() && matches!(mode, SearchMode::Count | SearchMode::CountMatches))
}

/// One verify worker per chunk of candidates, never over the thread budget
const fn verify_worker_count(threads: usize, candidates: usize) -> usize {
    let budget = if threads == 0 { 1 } else { threads };
    let wanted = candidates / FILES_PER_VERIFY_WORKER;
    if wanted <= 1 {
        1
    } else if wanted > budget {
        budget
    } else {
        wanted
    }
}

fn verify_for_bench(
    args: &HiArgs,
    mode: SearchMode,
    roots: &SearchRoots,
    snapshot: &manifest::CurrentSnapshot,
    candidates: &BTreeSet<usize>,
    report: &mut bench::BenchReport,
) -> anyhow::Result<bool> {
    let started_at = Instant::now();
    let full_corpus = is_full_corpus_mode(args, mode);
    let ordered = if full_corpus {
        all_ordered(snapshot)
    } else {
        ordered_candidates(snapshot, candidates)
    };
    if !full_corpus && verify_worker_count(args.threads(), ordered.len()) > 1 {
        let facts = verify_candidates_for_bench(args, mode, roots, snapshot, &ordered)?;
        report.set_verification(
            facts.verified_files,
            facts.matched_files,
            facts.bytes_verified,
        );
        report.timing_mut().set_verify_haystacks(started_at);
        return Ok(facts.matched_any);
    }
    let mut facts = BenchFacts::default();
    let sink = termcolor::NoColor::new(Vec::new());
    let mut searcher =
        args.search_worker(args.matcher()?, args.searcher()?, args.printer(mode, sink))?;
    for ord in ordered {
        let Some(file) = snapshot.file(ord) else {
            continue;
        };
        let in_candidates = candidates.contains(&ord);
        let Some(search_result) = bench_search(
            args,
            mode,
            roots,
            in_candidates,
            &mut facts,
            &mut searcher,
            &file,
        )?
        else {
            continue;
        };
        facts.record_match(search_result.has_match(), in_candidates);
    }
    report.set_verification(
        facts.verified_files,
        facts.matched_files,
        facts.bytes_verified,
    );
    report.timing_mut().set_verify_haystacks(started_at);
    Ok(facts.matched_any)
}

fn verify_candidates_for_bench(
    args: &HiArgs,
    mode: SearchMode,
    roots: &SearchRoots,
    snapshot: &manifest::CurrentSnapshot,
    ordered: &[usize],
) -> anyhow::Result<BenchFacts> {
    let next_pos = AtomicUsize::new(0);
    let sink = termcolor::NoColor::new(Vec::new());
    let prototype =
        args.search_worker(args.matcher()?, args.searcher()?, args.printer(mode, sink))?;
    let worker_count = verify_worker_count(args.threads(), ordered.len());
    std::thread::scope(|scope| -> anyhow::Result<BenchFacts> {
        let mut handles = Vec::with_capacity(worker_count);
        for _ in 0..worker_count {
            let searcher = prototype.clone();
            handles.push(
                scope.spawn(|| {
                    bench_worker(args, mode, roots, snapshot, ordered, &next_pos, searcher)
                }),
            );
        }
        collect_bench_workers(handles)
    })
}

fn bench_worker(
    args: &HiArgs,
    mode: SearchMode,
    roots: &SearchRoots,
    snapshot: &manifest::CurrentSnapshot,
    ordered: &[usize],
    next_pos: &AtomicUsize,
    mut searcher: crate::search::SearchWorker<termcolor::NoColor<Vec<u8>>>,
) -> anyhow::Result<BenchFacts> {
    let mut facts = BenchFacts::default();
    loop {
        let pos = next_pos.fetch_add(1, AtomicOrdering::Relaxed);
        let Some(&ord) = ordered.get(pos) else {
            return Ok(facts);
        };
        let Some(file) = snapshot.file(ord) else {
            continue;
        };
        let Some(search_result) =
            bench_search(args, mode, roots, true, &mut facts, &mut searcher, &file)?
        else {
            continue;
        };
        facts.record_match(search_result.has_match(), true);
    }
}

fn collect_bench_workers(
    handles: Vec<std::thread::ScopedJoinHandle<'_, anyhow::Result<BenchFacts>>>,
) -> anyhow::Result<BenchFacts> {
    let mut facts = BenchFacts::default();
    for handle in handles {
        match handle.join() {
            Ok(Ok(worker_facts)) => facts.merge(worker_facts),
            Ok(Err(err)) => return Err(err),
            Err(_) => bail!("indexed benchmark worker thread panicked"),
        }
    }
    Ok(facts)
}

fn bench_search(
    args: &HiArgs,
    mode: SearchMode,
    roots: &SearchRoots,
    in_candidates: bool,
    facts: &mut BenchFacts,
    searcher: &mut crate::search::SearchWorker<termcolor::NoColor<Vec<u8>>>,
    file: &manifest::CurrentFile,
) -> anyhow::Result<Option<crate::search::SearchResult>> {
    if !in_candidates && !is_full_corpus_mode(args, mode) {
        return Ok(None);
    }
    let search_result = if in_candidates {
        facts.verified_files += 1;
        facts.bytes_verified = facts.bytes_verified.saturating_add(file.len());
        searcher.search(&candidate_haystack(roots, file))
    } else if file.is_skipped_binary() {
        return Ok(None);
    } else {
        searcher.search_absent(&roots.display_path(&file.path))
    };
    match search_result {
        Ok(search_result) => Ok(Some(search_result)),
        Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => Ok(None),
        Err(err) => {
            err_message!("{}: {}", roots.display_path(&file.path).display(), err);
            Ok(None)
        },
    }
}

#[derive(Default)]
struct BenchFacts {
    matched_any: bool,
    matched_files: usize,
    verified_files: usize,
    bytes_verified: u64,
}

impl BenchFacts {
    fn record_match(&mut self, matched: bool, in_candidates: bool) {
        if matched {
            self.matched_any = true;
            if in_candidates {
                self.matched_files += 1;
            }
        }
    }

    fn merge(&mut self, other: Self) {
        self.matched_any |= other.matched_any;
        self.matched_files += other.matched_files;
        self.verified_files += other.verified_files;
        self.bytes_verified = self.bytes_verified.saturating_add(other.bytes_verified);
    }
}

/// Report on every corpus file for modes that print zero-match files too.
///
/// Files the index ruled out have no matches by soundness, so they are emitted
/// through the printer with an empty search: the exact zero-count or
/// without-match line. Candidate files are searched for real.
fn verify_full_corpus(
    args: &HiArgs,
    mode: SearchMode,
    started_at: Instant,
    roots: &SearchRoots,
    snapshot: &manifest::CurrentSnapshot,
    candidates: &BTreeSet<usize>,
) -> anyhow::Result<bool> {
    let ordered = all_ordered(snapshot);
    let mut matched = false;
    let mut stats = args.stats();
    let mut searcher = args.search_worker(
        args.matcher()?,
        args.searcher()?,
        args.printer(mode, args.stdout()),
    )?;
    for &ord in &ordered {
        let Some(file) = snapshot.file(ord) else {
            continue;
        };
        if !roots.contains(args.cwd(), &file.path) {
            continue;
        }
        let haystack = candidate_haystack(roots, &file);
        let search_result = if candidates.contains(&ord) {
            searcher.search(&haystack)
        } else if file.is_skipped_binary() {
            continue;
        } else {
            searcher.search_absent(haystack.path())
        };
        let search_result = match search_result {
            Ok(search_result) => search_result,
            Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => break,
            Err(err) => {
                err_message!("{}: {}", haystack.path().display(), err);
                continue;
            },
        };
        matched = matched || search_result.has_match();
        if let Some(ref mut stats) = stats
            && let Some(search_stats) = search_result.stats()
        {
            *stats += search_stats;
        }
    }
    if let Some(ref stats) = stats {
        let writer = searcher.printer().get_mut();
        let _ = crate::print_stats(mode, stats, started_at, writer);
    }
    Ok(matched)
}

/// Reorder buffer that releases per-file output strictly in path order.
struct Reorder {
    next_emit: usize,
    slots: Vec<Option<termcolor::Buffer>>,
}

impl Reorder {
    fn new(len: usize) -> Self {
        Self {
            next_emit: 0,
            slots: (0..len).map(|_| None).collect(),
        }
    }

    /// Store one file's buffered output, then flush the completed prefix.
    fn record_and_flush(
        &mut self,
        pos: usize,
        buffer: termcolor::Buffer,
        bufwtr: &termcolor::BufferWriter,
    ) -> std::io::Result<()> {
        if let Some(slot) = self.slots.get_mut(pos) {
            *slot = Some(buffer);
        }
        while self.slots.get(self.next_emit).is_some_and(Option::is_some) {
            if let Some(Some(ready)) = self.slots.get_mut(self.next_emit).map(Option::take) {
                bufwtr.print(&ready)?;
            }
            self.next_emit += 1;
        }
        Ok(())
    }
}

/// Shared state for the parallel verify workers.
struct Verify<'a> {
    args: &'a HiArgs,
    roots: &'a SearchRoots,
    snapshot: &'a manifest::CurrentSnapshot,
    ordered: &'a [usize],
    next_pos: &'a AtomicUsize,
    matched: &'a AtomicBool,
    stats: Option<&'a Mutex<grep::printer::Stats>>,
    reorder: &'a Mutex<Reorder>,
    bufwtr: &'a termcolor::BufferWriter,
}

/// Verify a path-ordered candidate set through per-file buffers.
fn verify_buffered(
    args: &HiArgs,
    mode: SearchMode,
    started_at: Instant,
    roots: &SearchRoots,
    snapshot: &manifest::CurrentSnapshot,
    ordered: &[usize],
) -> anyhow::Result<bool> {
    let bufwtr = args.buffer_writer();
    let stats = args.stats().map(Mutex::new);
    let matched = AtomicBool::new(false);
    let next_pos = AtomicUsize::new(0);
    let reorder = Mutex::new(Reorder::new(ordered.len()));
    let prototype = args.search_worker(
        args.matcher()?,
        args.searcher()?,
        args.printer(mode, bufwtr.buffer()),
    )?;
    let ctx = Verify {
        args,
        roots,
        snapshot,
        ordered,
        next_pos: &next_pos,
        matched: &matched,
        stats: stats.as_ref(),
        reorder: &reorder,
        bufwtr: &bufwtr,
    };
    spawn_verify_workers(&prototype, &ctx)?;
    if let Some(ref locked_stats) = stats {
        print_buffered_stats(mode, started_at, locked_stats, &bufwtr);
    }
    Ok(matched.load(AtomicOrdering::SeqCst))
}

/// Emit the stats footer after the last candidate's output
fn print_buffered_stats(
    mode: SearchMode,
    started_at: Instant,
    stats: &Mutex<grep::printer::Stats>,
    bufwtr: &termcolor::BufferWriter,
) {
    let stats = stats
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut wtr = bufwtr.buffer();
    let _ = crate::print_stats(mode, &stats, started_at, &mut wtr);
    let _ = bufwtr.print(&wtr);
}

/// Fan the prototype worker out over the candidate-scaled thread count
fn spawn_verify_workers(
    prototype: &crate::search::SearchWorker<termcolor::Buffer>,
    ctx: &Verify,
) -> anyhow::Result<()> {
    let worker_count = verify_worker_count(ctx.args.threads(), ctx.ordered.len());
    std::thread::scope(|scope| -> anyhow::Result<()> {
        let mut handles = Vec::with_capacity(worker_count);
        for _ in 0..worker_count {
            let searcher = prototype.clone();
            handles.push(scope.spawn(move || verify_worker(ctx, searcher)));
        }
        join_verify_workers(handles)
    })
}

fn join_verify_workers(
    handles: Vec<std::thread::ScopedJoinHandle<'_, std::io::Result<()>>>,
) -> anyhow::Result<()> {
    for handle in handles {
        match handle.join() {
            Ok(Ok(())) => {},
            Ok(Err(err)) if err.kind() == std::io::ErrorKind::BrokenPipe => {},
            Ok(Err(err)) => return Err(err.into()),
            Err(_) => bail!("indexed search worker thread panicked"),
        }
    }
    Ok(())
}

/// One verify worker: pull path-ordered candidates and emit through the reorder buffer.
fn verify_worker(
    ctx: &Verify,
    mut searcher: crate::search::SearchWorker<termcolor::Buffer>,
) -> std::io::Result<()> {
    loop {
        if ctx.matched.load(AtomicOrdering::SeqCst) && ctx.args.quit_after_match() {
            return Ok(());
        }
        let pos = ctx.next_pos.fetch_add(1, AtomicOrdering::Relaxed);
        let Some(&ord) = ctx.ordered.get(pos) else {
            return Ok(());
        };
        let buffer = match ctx.snapshot.file(ord) {
            Some(file) => {
                verify_one(ctx, &mut searcher, &file)?;
                mem::replace(searcher.printer().get_mut(), ctx.bufwtr.buffer())
            },
            None => ctx.bufwtr.buffer(),
        };
        ctx.reorder
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .record_and_flush(pos, buffer, ctx.bufwtr)?;
    }
}

/// Search one candidate file, updating the shared match flag and stats.
fn verify_one(
    ctx: &Verify,
    searcher: &mut crate::search::SearchWorker<termcolor::Buffer>,
    file: &manifest::CurrentFile,
) -> std::io::Result<()> {
    let haystack = candidate_haystack(ctx.roots, file);
    let search_result = match searcher.search(&haystack) {
        Ok(search_result) => search_result,
        Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => return Err(err),
        Err(err) => {
            err_message!("{}: {}", haystack.path().display(), err);
            return Ok(());
        },
    };
    if search_result.has_match() {
        ctx.matched.store(true, AtomicOrdering::SeqCst);
    }
    if let Some(locked_stats) = ctx.stats
        && let Some(search_stats) = search_result.stats()
    {
        *locked_stats
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) += search_stats;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{FILES_PER_VERIFY_WORKER, verify_worker_count};

    #[test]
    fn worker_count_follows_the_candidate_set() {
        assert_eq!(verify_worker_count(12, 0), 1);
        assert_eq!(verify_worker_count(12, FILES_PER_VERIFY_WORKER), 1);
        assert_eq!(verify_worker_count(12, FILES_PER_VERIFY_WORKER * 2), 2);
        assert_eq!(verify_worker_count(12, FILES_PER_VERIFY_WORKER * 7), 7);
        assert_eq!(verify_worker_count(12, FILES_PER_VERIFY_WORKER * 40), 12);
    }

    #[test]
    fn worker_count_never_exceeds_the_thread_budget() {
        assert_eq!(verify_worker_count(1, 100_000), 1);
        assert_eq!(verify_worker_count(0, 100_000), 1);
        assert_eq!(verify_worker_count(4, 100_000), 4);
    }
}
