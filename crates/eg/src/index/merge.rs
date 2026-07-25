//! Sorted run files and their parallel partitioned merge.

use std::{
    cmp::Ordering,
    collections::{BTreeMap, BinaryHeap},
    fs::File,
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    sync::mpsc,
};

use anyhow::Context;
use memmap2::{Mmap, MmapOptions};
use rayon::prelude::*;

use super::huffman::{CodeLengths, Encoder, HUFF_MIN_COUNT};
use super::postings::{POSTINGS_MAGIC, SectionWriter, TABLE_MAGIC};
use super::progress::BuildProgress;

/// Run record layout: hash u32, ord u32, mask u16
pub const RUN_PAIR_SIZE: usize = 10;
/// Delta-coded table records per skip-directory block
pub const RECORDS_PER_BLOCK: usize = 256;
/// Bytes in a per-block bitmap marking inline df=1 records
pub const BLOCK_BITMAP_SIZE: usize = RECORDS_PER_BLOCK / 8;
/// Directory entry layout: first hash32, records byte offset, postings byte offset
pub const DIRECTORY_ENTRY_SIZE: usize = 16;
/// Hash-range partitions merged in parallel
const PARTITION_COUNT: usize = 64;
/// High hash bits selecting a partition
const PARTITION_SHIFT: u32 = 32 - PARTITION_COUNT.trailing_zeros();

/// One selected gram occurrence spilled to a run file
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Pair {
    pub hash: u32,
    pub ord: u32,
    pub mask: u16,
}

impl Ord for Pair {
    fn cmp(&self, other: &Self) -> Ordering {
        self.hash.cmp(&other.hash).then(self.ord.cmp(&other.ord))
    }
}

impl PartialOrd for Pair {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Pair {
    const ZERO: Self = Self {
        hash: 0,
        ord: 0,
        mask: 0,
    };
}

/// Pair counts below this compare instead, where four counting passes cost
/// more than they save
const MIN_RADIX_PAIRS: usize = 4096;

/// Sort one run's pairs into (hash, ord) order.
///
/// Documents reach a run under ascending ordinals, so counting passes over the
/// hash bytes alone leave ordinals ascending within every hash. Runs that do
/// not arrive that way fall back to comparing both fields.
pub fn sort_pairs(pairs: &mut Vec<Pair>) {
    if pairs.len() < MIN_RADIX_PAIRS || !ordinals_ascending(pairs) {
        pairs.sort_unstable();
        return;
    }
    let mut scratch = vec![Pair::ZERO; pairs.len()];
    scatter(pairs, &mut scratch, 0);
    scatter(&scratch, pairs, 8);
    scatter(pairs, &mut scratch, 16);
    scatter(&scratch, pairs, 24);
}

/// Whether ordinals never step backwards, which the counting passes rely on
fn ordinals_ascending(pairs: &[Pair]) -> bool {
    pairs.windows(2).all(|step| step[0].ord <= step[1].ord)
}

/// Move every pair into `out` by one byte of its hash, keeping equal bytes in
/// their arrival order
fn scatter(input: &[Pair], out: &mut [Pair], shift: u32) {
    let mut counts = [0u32; 256];
    for pair in input {
        counts[digit(pair.hash, shift)] += 1;
    }
    let mut total = 0u32;
    for count in &mut counts {
        let start = total;
        total += *count;
        *count = start;
    }
    for &pair in input {
        let at = &mut counts[digit(pair.hash, shift)];
        out[*at as usize] = pair;
        *at += 1;
    }
}

const fn digit(hash: u32, shift: u32) -> usize {
    ((hash >> shift) & 0xFF) as usize
}

pub fn run_path(runs_dir: &Path, id: usize) -> PathBuf {
    runs_dir.join(format!("{id:08}.run"))
}

pub fn write_pair(writer: &mut BufWriter<File>, pair: Pair) -> anyhow::Result<()> {
    let mut bytes = [0u8; RUN_PAIR_SIZE];
    bytes[..4].copy_from_slice(&pair.hash.to_le_bytes());
    bytes[4..8].copy_from_slice(&pair.ord.to_le_bytes());
    bytes[8..10].copy_from_slice(&pair.mask.to_le_bytes());
    writer.write_all(&bytes)?;
    Ok(())
}

/// Parse the pair at a record index of a run body
fn pair_at(bytes: &[u8], index: usize) -> Option<Pair> {
    let at = index.checked_mul(RUN_PAIR_SIZE)?;
    let record = bytes.get(at..at + RUN_PAIR_SIZE)?;
    Some(Pair {
        hash: u32::from_le_bytes(record[..4].try_into().expect("four bytes")),
        ord: u32::from_le_bytes(record[4..8].try_into().expect("four bytes")),
        mask: u16::from_le_bytes(record[8..10].try_into().expect("two bytes")),
    })
}

/// Merge sorted runs into the final table and postings sections.
///
/// Each run is sorted by (hash, ord), so the 32-bit hash space splits into
/// contiguous ranges found by binary search in every run. Partitions merge
/// independently on the thread pool while the caller thread streams finished
/// partitions to disk in order, fixing up directory offsets as it goes.
pub fn merge_runs(
    runs_dir: &Path,
    run_count: usize,
    table_path: &Path,
    postings_path: &Path,
    progress: Option<&BuildProgress>,
    pairs_total: u64,
    code_lengths: &CodeLengths,
) -> anyhow::Result<()> {
    let runs = mmap_runs(runs_dir, run_count)?;
    let bounds: Vec<[usize; PARTITION_COUNT + 1]> =
        runs.iter().map(|run| partition_bounds(run)).collect();
    let (sender, receiver) = mpsc::channel();
    rayon::scope(|scope| {
        let runs = &runs;
        let bounds = &bounds;
        scope.spawn(move |_| {
            (0..PARTITION_COUNT)
                .into_par_iter()
                .for_each_with(sender, |sender, partition| {
                    let out = merge_partition(runs, bounds, partition, code_lengths);
                    let _ = sender.send((partition, out));
                });
        });
        write_partitions(
            receiver,
            table_path,
            postings_path,
            progress,
            pairs_total,
            code_lengths,
        )
    })
}

/// Map every non-empty run file; empty runs contribute nothing to the merge
#[allow(unsafe_code)]
fn mmap_runs(runs_dir: &Path, run_count: usize) -> anyhow::Result<Vec<Mmap>> {
    let mut runs = Vec::with_capacity(run_count);
    for id in 0..run_count {
        let path = run_path(runs_dir, id);
        let file =
            File::open(&path).with_context(|| format!("failed to open run {}", path.display()))?;
        let len = file
            .metadata()
            .with_context(|| format!("failed to stat run {}", path.display()))?
            .len();
        if len == 0 {
            continue;
        }
        let map = unsafe { MmapOptions::new().map(&file) }
            .with_context(|| format!("failed to mmap run {}", path.display()))?;
        runs.push(map);
    }
    Ok(runs)
}

/// Record indices in one run where each hash partition begins
fn partition_bounds(run: &[u8]) -> [usize; PARTITION_COUNT + 1] {
    let pair_count = run.len() / RUN_PAIR_SIZE;
    let mut bounds = [0usize; PARTITION_COUNT + 1];
    bounds[PARTITION_COUNT] = pair_count;
    for partition in 1..PARTITION_COUNT {
        let floor = (partition as u32) << PARTITION_SHIFT;
        bounds[partition] = lower_bound(run, bounds[partition - 1], pair_count, floor);
    }
    bounds
}

/// First record index in [lo, hi) whose hash is at least floor
fn lower_bound(run: &[u8], mut lo: usize, mut hi: usize, floor: u32) -> usize {
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let hash = pair_at(run, mid).map_or(u32::MAX, |pair| pair.hash);
        if hash < floor {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

/// One partition's encoded table and postings fragments
struct PartitionOutput {
    records: Vec<u8>,
    directory: Vec<DirectoryEntry>,
    bitmaps: Vec<u8>,
    postings: Vec<u8>,
    block_count: u32,
    pairs_done: u64,
}

/// Directory row pending offset fixup at write time
struct DirectoryEntry {
    first_hash: u32,
    records_offset: u32,
    postings_offset: u64,
}

/// Cursor over one run's records inside a partition range
struct RunSlice<'a> {
    run: &'a [u8],
    next: usize,
    end: usize,
}

impl RunSlice<'_> {
    fn next_pair(&mut self) -> Option<Pair> {
        if self.next >= self.end {
            return None;
        }
        let pair = pair_at(self.run, self.next);
        self.next += 1;
        pair
    }
}

#[derive(Eq, PartialEq)]
struct HeapItem {
    pair: Pair,
    run_id: usize,
}

impl Ord for HeapItem {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .pair
            .cmp(&self.pair)
            .then(other.run_id.cmp(&self.run_id))
    }
}

impl PartialOrd for HeapItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn merge_partition(
    runs: &[Mmap],
    bounds: &[[usize; PARTITION_COUNT + 1]],
    partition: usize,
    code_lengths: &CodeLengths,
) -> anyhow::Result<PartitionOutput> {
    let encoder = Encoder::new(code_lengths);
    let mut slices = partition_slices(runs, bounds, partition);
    let mut heap = seed_heap(&mut slices);
    let mut builder = PartitionBuilder::new();
    let mut group = DocGroup::default();
    let mut pairs_done = 0u64;
    while let Some(item) = heap.pop() {
        group.absorb(item.pair, &mut builder, &encoder)?;
        pairs_done += 1;
        if let Some(pair) = slices[item.run_id].next_pair() {
            heap.push(HeapItem {
                pair,
                run_id: item.run_id,
            });
        }
    }
    group.flush(&mut builder, &encoder)?;
    Ok(builder.finish(pairs_done))
}

/// Each run's record range for one hash partition
fn partition_slices<'a>(
    runs: &'a [Mmap],
    bounds: &[[usize; PARTITION_COUNT + 1]],
    partition: usize,
) -> Vec<RunSlice<'a>> {
    runs.iter()
        .zip(bounds)
        .map(|(run, bound)| RunSlice {
            run,
            next: bound[partition],
            end: bound[partition + 1],
        })
        .collect()
}

/// Heap seeded with the first pair of every non-empty slice
fn seed_heap(slices: &mut [RunSlice]) -> BinaryHeap<HeapItem> {
    let mut heap = BinaryHeap::with_capacity(slices.len());
    for (run_id, slice) in slices.iter_mut().enumerate() {
        if let Some(pair) = slice.next_pair() {
            heap.push(HeapItem { pair, run_id });
        }
    }
    heap
}

/// Ordered docs for the current hash, OR-merging duplicate ordinals
#[derive(Default)]
struct DocGroup {
    hash: Option<u32>,
    docs: Vec<(u32, u16)>,
}

impl DocGroup {
    fn absorb(
        &mut self,
        pair: Pair,
        builder: &mut PartitionBuilder,
        encoder: &Encoder,
    ) -> anyhow::Result<()> {
        if self.hash != Some(pair.hash) {
            self.flush(builder, encoder)?;
            self.hash = Some(pair.hash);
        }
        if let Some(last) = self.docs.last_mut().filter(|last| last.0 == pair.ord) {
            last.1 |= pair.mask;
        } else {
            self.docs.push((pair.ord, pair.mask));
        }
        Ok(())
    }

    fn flush(&mut self, builder: &mut PartitionBuilder, encoder: &Encoder) -> anyhow::Result<()> {
        if let Some(hash) = self.hash.take() {
            builder.push(hash, &self.docs, encoder)?;
            self.docs.clear();
        }
        Ok(())
    }
}

/// Builds one partition's delta-coded records in skip-directory blocks;
/// df=1 lists inline into the record instead of the postings fragment
struct PartitionBuilder {
    out: PartitionOutput,
    block_bitmap: [u8; BLOCK_BITMAP_SIZE],
    block_records: usize,
    previous_hash: u32,
}

impl PartitionBuilder {
    fn new() -> Self {
        Self {
            out: PartitionOutput {
                records: Vec::new(),
                directory: Vec::new(),
                bitmaps: Vec::new(),
                postings: Vec::new(),
                block_count: 0,
                pairs_done: 0,
            },
            block_bitmap: [0u8; BLOCK_BITMAP_SIZE],
            block_records: 0,
            previous_hash: 0,
        }
    }

    fn push(&mut self, hash: u32, docs: &[(u32, u16)], encoder: &Encoder) -> anyhow::Result<()> {
        let gap = if self.block_records == 0 {
            self.begin_block(hash)?;
            0
        } else {
            hash - self.previous_hash
        };
        push_uvarint(&mut self.out.records, gap);
        if let [(ord, mask)] = docs {
            self.block_bitmap[self.block_records / 8] |= 1 << (self.block_records % 8);
            push_uvarint(&mut self.out.records, *ord);
            push_uvarint(&mut self.out.records, u32::from(*mask));
        } else {
            let count = u32::try_from(docs.len()).context("posting count does not fit in u32")?;
            let list = encode_posting_list(docs, encoder);
            let size = u32::try_from(list.len()).context("posting list exceeds u32 bytes")?;
            push_uvarint(&mut self.out.records, count);
            push_uvarint(&mut self.out.records, size);
            self.out.postings.extend_from_slice(&list);
        }
        self.previous_hash = hash;
        self.block_records = (self.block_records + 1) % RECORDS_PER_BLOCK;
        Ok(())
    }

    fn begin_block(&mut self, hash: u32) -> anyhow::Result<()> {
        if self.out.block_count > 0 {
            self.flush_bitmap();
        }
        self.out.directory.push(DirectoryEntry {
            first_hash: hash,
            records_offset: u32::try_from(self.out.records.len())
                .context("partition records exceed u32 bytes")?,
            postings_offset: self.out.postings.len() as u64,
        });
        self.out.block_count += 1;
        Ok(())
    }

    fn flush_bitmap(&mut self) {
        self.out.bitmaps.extend_from_slice(&self.block_bitmap);
        self.block_bitmap = [0u8; BLOCK_BITMAP_SIZE];
    }

    fn finish(mut self, pairs_done: u64) -> PartitionOutput {
        if self.out.block_count > 0 {
            self.flush_bitmap();
        }
        self.out.pairs_done = pairs_done;
        self.out
    }
}

/// Stream finished partitions to disk in partition order
fn write_partitions(
    receiver: mpsc::Receiver<(usize, anyhow::Result<PartitionOutput>)>,
    table_path: &Path,
    postings_path: &Path,
    progress: Option<&BuildProgress>,
    pairs_total: u64,
    code_lengths: &CodeLengths,
) -> anyhow::Result<()> {
    let mut sink = PartitionSink::create(table_path, postings_path, code_lengths)?;
    let mut pending = BTreeMap::new();
    while sink.next < PARTITION_COUNT {
        let (partition, out) = receiver
            .recv()
            .context("postings merge worker disconnected")?;
        pending.insert(partition, out?);
        while let Some(out) = pending.remove(&sink.next) {
            sink.write(out, progress, pairs_total)?;
        }
    }
    sink.finish()
}

/// In-order writer of partition fragments into both section files
struct PartitionSink {
    table_writer: SectionWriter,
    postings_writer: SectionWriter,
    tail: SectionTail,
    next: usize,
}

impl PartitionSink {
    fn create(
        table_path: &Path,
        postings_path: &Path,
        code_lengths: &CodeLengths,
    ) -> anyhow::Result<Self> {
        let table_writer = SectionWriter::create(table_path, TABLE_MAGIC)?;
        let mut postings_writer = SectionWriter::create(postings_path, POSTINGS_MAGIC)?;
        postings_writer.write_all(code_lengths.as_bytes())?;
        Ok(Self {
            table_writer,
            postings_writer,
            tail: SectionTail::default(),
            next: 0,
        })
    }

    fn write(
        &mut self,
        out: PartitionOutput,
        progress: Option<&BuildProgress>,
        pairs_total: u64,
    ) -> anyhow::Result<()> {
        self.table_writer.write_all(&out.records)?;
        self.postings_writer.write_all(&out.postings)?;
        self.tail.absorb(out)?;
        self.next += 1;
        if let Some(progress) = progress {
            progress.update_postings(
                PARTITION_COUNT,
                self.next as u64,
                pairs_total,
                self.tail.pairs_done,
            );
        }
        Ok(())
    }

    fn finish(self) -> anyhow::Result<()> {
        self.tail.finish(self.table_writer)?;
        self.postings_writer.finalize(1)
    }
}

/// Accumulated directory, bitmaps, and offsets awaiting the table footer
#[derive(Default)]
struct SectionTail {
    directory: Vec<u8>,
    bitmaps: Vec<u8>,
    block_count: u32,
    records_base: u64,
    postings_base: u64,
    pairs_done: u64,
}

impl SectionTail {
    fn absorb(&mut self, out: PartitionOutput) -> anyhow::Result<()> {
        let base = u32::try_from(self.records_base).context("table records exceed u32 bytes")?;
        for entry in &out.directory {
            self.directory
                .extend_from_slice(&entry.first_hash.to_le_bytes());
            let records_offset = entry
                .records_offset
                .checked_add(base)
                .context("table records exceed u32 bytes")?;
            self.directory
                .extend_from_slice(&records_offset.to_le_bytes());
            let postings_offset = entry.postings_offset + self.postings_base;
            self.directory
                .extend_from_slice(&postings_offset.to_le_bytes());
        }
        self.bitmaps.extend_from_slice(&out.bitmaps);
        self.block_count += out.block_count;
        self.records_base += out.records.len() as u64;
        self.postings_base += out.postings.len() as u64;
        self.pairs_done += out.pairs_done;
        Ok(())
    }

    fn finish(self, mut table_writer: SectionWriter) -> anyhow::Result<()> {
        table_writer.write_all(&self.directory)?;
        table_writer.write_all(&self.bitmaps)?;
        table_writer.write_all(&self.block_count.to_le_bytes())?;
        table_writer.finalize(1)
    }
}

/// Posting list layout: ascending ordinal gaps as uvarints, then the mask
/// column - raw uvarints for short lists, a Huffman bitstream otherwise
pub fn encode_posting_list(docs: &[(u32, u16)], encoder: &Encoder) -> Vec<u8> {
    let mut out = Vec::with_capacity(docs.len() * 3);
    let mut previous = 0u32;
    for (idx, &(doc, _)) in docs.iter().enumerate() {
        push_uvarint(&mut out, if idx == 0 { doc } else { doc - previous });
        previous = doc;
    }
    if docs.len() < HUFF_MIN_COUNT {
        for &(_, mask) in docs {
            push_uvarint(&mut out, u32::from(mask));
        }
    } else {
        encoder.encode_into(docs.iter().map(|&(_, mask)| mask), &mut out);
    }
    out
}

pub fn push_uvarint(out: &mut Vec<u8>, mut value: u32) {
    while value >= 0x80 {
        out.push(value as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

#[cfg(test)]
mod tests {
    use super::{
        MIN_RADIX_PAIRS, PARTITION_COUNT, Pair, RUN_PAIR_SIZE, lower_bound, pair_at,
        partition_bounds, run_path, sort_pairs, write_pair,
    };
    use std::{
        fs::{self, File},
        io::{BufWriter, Write},
    };

    fn run_bytes(pairs: &[Pair]) -> Vec<u8> {
        let dir = tempfile::Builder::new()
            .prefix("eg-merge-run-")
            .tempdir()
            .unwrap();
        let path = run_path(dir.path(), 0);
        let mut writer = BufWriter::new(File::create(&path).unwrap());
        for &pair in pairs {
            write_pair(&mut writer, pair).unwrap();
        }
        writer.flush().unwrap();
        fs::read(&path).unwrap()
    }

    fn pair(hash: u32, ord: u32) -> Pair {
        Pair {
            hash,
            ord,
            mask: 0x11,
        }
    }

    #[test]
    fn pairs_round_trip_through_run_bytes() {
        let pairs = [
            pair(3, 1),
            Pair {
                hash: u32::MAX - 1,
                ord: u32::MAX,
                mask: 0x3FF,
            },
        ];
        let bytes = run_bytes(&pairs);

        assert_eq!(bytes.len(), 2 * RUN_PAIR_SIZE);
        assert_eq!(pair_at(&bytes, 0), Some(pairs[0]));
        assert_eq!(pair_at(&bytes, 1), Some(pairs[1]));
        assert_eq!(pair_at(&bytes, 2), None);
    }

    #[test]
    fn lower_bound_finds_first_hash_at_or_above_floor() {
        let bytes = run_bytes(&[pair(2, 0), pair(5, 0), pair(5, 1), pair(9, 0)]);

        assert_eq!(lower_bound(&bytes, 0, 4, 0), 0);
        assert_eq!(lower_bound(&bytes, 0, 4, 5), 1);
        assert_eq!(lower_bound(&bytes, 0, 4, 6), 3);
        assert_eq!(lower_bound(&bytes, 0, 4, 10), 4);
    }

    /// One run's worth of pairs: many documents, each under one ascending ord
    fn run_pairs(count: usize, per_doc: usize) -> Vec<Pair> {
        (0..count)
            .map(|i| Pair {
                hash: (i as u32).wrapping_mul(0x9E37_79B9),
                ord: (i / per_doc) as u32,
                mask: (i % 1023 + 1) as u16,
            })
            .collect()
    }

    #[test]
    fn counting_passes_match_the_comparison_sort() {
        for count in [MIN_RADIX_PAIRS - 1, MIN_RADIX_PAIRS, 40_000] {
            let mut sorted = run_pairs(count, 37);
            sorted.sort_unstable();
            let mut counted = run_pairs(count, 37);
            sort_pairs(&mut counted);
            assert_eq!(counted, sorted, "count {count}");
        }
    }

    #[test]
    fn descending_ordinals_fall_back_to_the_comparison_sort() {
        let mut pairs = run_pairs(MIN_RADIX_PAIRS * 2, 37);
        pairs.reverse();
        let mut sorted = pairs.clone();
        sorted.sort_unstable();
        sort_pairs(&mut pairs);
        assert_eq!(pairs, sorted);
    }

    #[test]
    fn equal_hashes_keep_ascending_ordinals() {
        let mut pairs: Vec<Pair> = (0..MIN_RADIX_PAIRS * 2)
            .map(|i| Pair {
                hash: (i % 8) as u32,
                ord: i as u32,
                mask: 1,
            })
            .collect();
        sort_pairs(&mut pairs);
        for step in pairs.windows(2) {
            assert!(
                step[0].hash < step[1].hash
                    || (step[0].hash == step[1].hash && step[0].ord < step[1].ord)
            );
        }
    }

    #[test]
    fn partition_bounds_cover_every_pair_exactly_once() {
        let pairs: Vec<Pair> = (0..1000u32)
            .map(|i| pair(i.wrapping_mul(0x9E37_79B9), i))
            .map(|mut p| {
                p.ord = 0;
                p
            })
            .collect();
        let mut sorted = pairs.clone();
        sorted.sort();
        let bytes = run_bytes(&sorted);
        let bounds = partition_bounds(&bytes);

        assert_eq!(bounds[0], 0);
        assert_eq!(bounds[PARTITION_COUNT], sorted.len());
        for window in bounds.windows(2) {
            assert!(window[0] <= window[1]);
        }
        let total: usize = bounds.windows(2).map(|window| window[1] - window[0]).sum();
        assert_eq!(total, sorted.len());
    }
}
