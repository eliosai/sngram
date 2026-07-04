//! Sparse n-gram extraction via monotonic stack (convex hull).
//!
//! The hot loop is branch-bound, so the implementation is shaped around three
//! measured wins (each verified byte-identical against a frozen reference in
//! `tests/differential.rs`): the weight lookup goes through
//! [`WeightTable::matrix`] so its bounds check vanishes, stack entries pack
//! into one `u64` (position | weight), and the stack top lives in registers —
//! the no-pop fast path never touches the stack array. The measured traps are
//! documented too: branchless dedup, emit batching, and block-staged lookups
//! all REGRESSED 10–35%; don't reintroduce them without new numbers.

use std::collections::VecDeque;

use sngram_types::WeightTable;

use crate::gram::Gram;
use crate::hashing;
use crate::hashing::HashKey;

/// Scan-time format options; index build and query plan must agree on them.
///
/// `key` selects the hash space (see [`HashKey`]); `fold` scans the
/// ASCII-case-folded stream into the folded twin space (the emitted hashes are
/// automatically tagged with [`HashKey::folded`]); `line_sentinels` brackets
/// every document with a virtual `\n` so anchored line-boundary grams exist at
/// the document's first and last line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ScanOptions {
    /// Deployment hash key for the emitted gram hashes.
    pub key: HashKey,
    /// Bracket the document with virtual line terminators.
    pub line_sentinels: bool,
    /// Scan the ASCII-folded stream, emitting into the folded twin space.
    pub fold: bool,
}

/// Shortest gram emitted or matched; a sparse gram spans at least one bigram.
///
/// Frozen format vocabulary: changing it rekeys every index. The primary gram
/// space is never normalized — folding lives in the twin space, not here.
pub const MIN_LEN: usize = 3;
/// Longest gram emitted; bounds index entries and covering-set members.
///
/// Frozen format vocabulary: changing it rekeys every index.
pub const MAX_LEN: usize = 100;
const STACK_CAP: usize = 128;

/// prefix-hash ring: holds `H[p]` for the last `RING` positions; an emittable
/// gram start is at most `MAX_LEN` behind the current position, well inside
const RING: usize = 128;
const RING_MASK: usize = RING - 1;

/// streaming window: keeps recent bytes so an emitted gram stays contiguous; sized so compaction (a `WINDOW_KEEP`-byte memmove every `WINDOW_CAP - WINDOW_KEEP` bytes) amortizes to ~nothing while a fresh scanner's zero-init stays cheap for tiny documents (1 KiB measured equal to 4 KiB at steady state, +12% on 64 B docs)
const WINDOW_CAP: usize = 1024;
/// bytes kept on compaction, at least the longest gram so every still-emittable gram start stays in the window
const WINDOW_KEEP: usize = 128;

/// Zero-allocation extraction. Calls `emit(start, end, hash)` for each gram.
///
/// The hash is a 64-bit rolling polynomial over the gram's bytes, computed in
/// O(1) per gram from prefix hashes — identical to `Gram::hash` of the same
/// bytes, so index keys and query keys agree.
///
/// # Panics
///
/// Panics if `content` is 4 GiB or larger (positions are packed into 32 bits;
/// feed inputs that large through `StreamScanner` instead).
#[inline]
#[allow(
    clippy::indexing_slicing,
    reason = "matrix index is u8<<8|u8 < 65536; stack and ring indices are masked or < len <= STACK_CAP"
)]
#[allow(
    clippy::too_many_lines,
    reason = "the hot loop is kept as one linear automaton; splitting it costs measured throughput"
)]
pub fn scan(table: &WeightTable, content: &[u8], mut emit: impl FnMut(usize, usize, u64)) {
    if content.len() < MIN_LEN {
        return;
    }
    assert!(
        u32::try_from(content.len()).is_ok(),
        "scan supports content up to 4 GiB; use StreamScanner beyond that"
    );
    let matrix = table.matrix();
    let mut buf = [0u64; STACK_CAP];
    let mut len = 0usize;
    // write-through register cache of buf[len-1], valid iff len > 0
    let mut tpos = 0usize;
    let mut tw = 0u32;
    // rolling prefix hash: h is H[i+1] inside the loop, ring holds recent H[p]
    let mut ring = [0u64; RING];
    let mut h = u64::from(content[0]);
    ring[0] = h;

    for (i, (&c1, &c2)) in content.iter().zip(&content[1..]).enumerate() {
        h = hashing::step(h, c2);
        ring[(i + 1) & RING_MASK] = h;
        let w = matrix[(usize::from(c1) << 8) | usize::from(c2)];
        let end = i + 2;
        while len > 0 {
            if tw >= w {
                // the surviving top spans a hull gram ending here
                emit_hashed(&ring, h, tpos, end, &mut emit);
                // dedup: an equal-weight top collapses into the new entry; the
                // cache needs no refresh — the push below rewrites it
                len -= usize::from(tw == w);
                break;
            }
            // drain: strictly lighter top closes its gram and pops
            len -= 1;
            emit_hashed(&ring, h, tpos, end, &mut emit);
            if len == 0 {
                break;
            }
            let e = buf[len - 1];
            tpos = unpack_pos(e);
            tw = unpack_weight(e);
        }
        if len >= STACK_CAP {
            // evict oldest: too far back to start a gram within MAX_LEN
            buf.copy_within(1.., 0);
            len -= 1;
        }
        buf[len] = pack(i, w);
        len += 1;
        tpos = i;
        tw = w;
    }
}

/// emit a length-valid gram with its O(1) rolling hash; `h_end` is `H[end-1]`
#[inline]
#[allow(clippy::indexing_slicing, reason = "ring index masked to RING")]
fn emit_hashed(
    ring: &[u64; RING],
    h_end: u64,
    start: usize,
    end: usize,
    emit: &mut impl FnMut(usize, usize, u64),
) {
    let len = end - start;
    if (MIN_LEN..=MAX_LEN).contains(&len) {
        let h_before = if start == 0 {
            0
        } else {
            ring[(start - 1) & RING_MASK]
        };
        emit(start, end, hashing::from_prefixes(h_end, h_before, len));
    }
}

#[inline]
#[allow(
    clippy::cast_possible_truncation,
    reason = "scan asserts content < 4 GiB, so pos fits u32"
)]
const fn pack(pos: usize, weight: u32) -> u64 {
    ((pos as u64) << 32) | weight as u64
}

#[inline]
#[allow(clippy::cast_possible_truncation, reason = "high 32 bits extracted")]
const fn unpack_pos(entry: u64) -> usize {
    (entry >> 32) as usize
}

#[inline]
#[allow(clippy::cast_possible_truncation, reason = "low 32 bits extracted")]
const fn unpack_weight(entry: u64) -> u32 {
    entry as u32
}

/// streaming sparse n-gram extraction that holds a bounded window, never the whole document
pub struct StreamScanner<'t> {
    matrix: &'t [u32; 65536],
    window: [u8; WINDOW_CAP],
    wlen: usize,
    base: usize,
    /// monotonic stack of (absolute position, weight); positions are unbounded
    /// in a stream, so entries stay unpacked
    stack: [(usize, u32); STACK_CAP],
    slen: usize,
    /// rolling prefix hash of everything pushed since the last `finish`
    hash: u64,
    /// recent prefix-hash values `H[p]`, indexed by absolute position masked
    ring: [u64; RING],
    /// effective hash space: the deployment key, fold-tagged when folding
    ekey: HashKey,
    opts: ScanOptions,
    /// whether the current document received its leading sentinel
    started: bool,
}

impl<'t> StreamScanner<'t> {
    /// new scanner bound to a weight table, ready to receive byte chunks
    #[must_use]
    pub fn new(table: &'t WeightTable) -> Self {
        Self::with_options(table, ScanOptions::default())
    }

    /// new scanner with explicit format options
    #[must_use]
    pub fn with_options(table: &'t WeightTable, opts: ScanOptions) -> Self {
        Self {
            matrix: table.matrix(),
            window: [0; WINDOW_CAP],
            wlen: 0,
            base: 0,
            stack: [(0, 0); STACK_CAP],
            slen: 0,
            hash: 0,
            ring: [0; RING],
            ekey: if opts.fold {
                opts.key.folded()
            } else {
                opts.key
            },
            opts,
            started: false,
        }
    }

    /// feed the next chunk, emitting each gram's bytes and rolling hash as it
    /// closes, identical to [`scan`](crate::scan) over the concatenation of all chunks
    pub fn push(&mut self, chunk: &[u8], mut emit: impl FnMut(&[u8], u64)) {
        if self.opts.line_sentinels && !self.started {
            self.started = true;
            self.push_raw(b"\n", &mut emit);
        }
        self.push_raw(chunk, &mut emit);
    }

    /// the automaton over one chunk, fold applied at window-copy time
    #[allow(
        clippy::indexing_slicing,
        reason = "wlen stays <= WINDOW_CAP, stack indices < slen <= STACK_CAP, ring indices masked, and a valid gram start is within MAX_LEN of end, kept in the window by WINDOW_KEEP"
    )]
    #[allow(
        clippy::excessive_nesting,
        clippy::too_many_lines,
        reason = "the hot loop is kept as one linear automaton; splitting it costs measured throughput"
    )]
    fn push_raw(&mut self, chunk: &[u8], mut emit: impl FnMut(&[u8], u64)) {
        // register caches of stack[slen-1] and the prefix hash
        let (mut tpos, mut tw) = if self.slen > 0 {
            self.stack[self.slen - 1]
        } else {
            (0, 0)
        };
        let mut h = self.hash;
        let mut rest = chunk;
        while !rest.is_empty() {
            if self.wlen == WINDOW_CAP {
                self.compact();
            }
            // bulk-copy as much of the chunk as fits, then run the automaton
            // over the copied span in a tight loop: no per-byte window-full
            // branch, no per-byte store
            let take = rest.len().min(WINDOW_CAP - self.wlen);
            let filled = self.wlen;
            if self.opts.fold {
                for (dst, src) in self.window[filled..filled + take]
                    .iter_mut()
                    .zip(&rest[..take])
                {
                    *dst = src.to_ascii_lowercase();
                }
            } else {
                self.window[filled..filled + take].copy_from_slice(&rest[..take]);
            }
            self.wlen += take;
            rest = &rest[take..];

            if filled == 0 && take > 0 {
                // first byte of the document seeds the prefix hash
                h = u64::from(self.window[0]);
                self.ring[0] = h;
            }
            // j indexes the second byte of each newly completed pair
            for j in filled.max(1)..filled + take {
                h = hashing::step(h, self.window[j]);
                self.ring[(self.base + j) & RING_MASK] = h;
                let w = self.matrix
                    [(usize::from(self.window[j - 1]) << 8) | usize::from(self.window[j])];
                let pos = self.base + j - 1;
                let end = self.base + j + 1;
                let key = self.ekey;
                while self.slen > 0 {
                    if tw >= w {
                        emit_window(
                            &self.window,
                            &self.ring,
                            self.base,
                            h,
                            tpos,
                            end,
                            key,
                            &mut emit,
                        );
                        self.slen -= usize::from(tw == w);
                        break;
                    }
                    self.slen -= 1;
                    emit_window(
                        &self.window,
                        &self.ring,
                        self.base,
                        h,
                        tpos,
                        end,
                        key,
                        &mut emit,
                    );
                    if self.slen == 0 {
                        break;
                    }
                    (tpos, tw) = self.stack[self.slen - 1];
                }
                if self.slen >= STACK_CAP {
                    // evict oldest: too far back to start a gram within MAX_LEN
                    self.stack.copy_within(1.., 0);
                    self.slen -= 1;
                }
                self.stack[self.slen] = (pos, w);
                self.slen += 1;
                tpos = pos;
                tw = w;
            }
        }
        self.hash = h;
    }

    /// end the current document and reset for the next, emitting nothing since scan leaves no closed grams at end of input
    pub const fn finish(&mut self) {
        self.wlen = 0;
        self.base = 0;
        self.slen = 0;
        self.hash = 0;
        self.started = false;
    }

    /// end the current document honoring the options: feeds the trailing
    /// sentinel (which can close grams, hence the emit), then resets
    pub fn finish_doc(&mut self, mut emit: impl FnMut(&[u8], u64)) {
        if self.opts.line_sentinels && self.started {
            self.push_raw(b"\n", &mut emit);
        }
        self.finish();
    }

    /// slide the still-emittable tail to the window front so more bytes fit, dropping only bytes too old to start a gram
    fn compact(&mut self) {
        const DROP: usize = WINDOW_CAP - WINDOW_KEEP;
        self.window.copy_within(DROP.., 0);
        self.wlen = WINDOW_KEEP;
        self.base += DROP;
    }
}

/// emit the gram bytes and rolling hash for an absolute (start, end) span if
/// its length is valid; starts older than `MAX_LEN` fail the length check
/// before any window arithmetic, so `start - base` never underflows
#[inline]
#[allow(
    clippy::indexing_slicing,
    reason = "a length-valid start is within MAX_LEN of end, kept in the window by WINDOW_KEEP; ring indices masked"
)]
#[allow(
    clippy::too_many_arguments,
    reason = "hot-path free function over disjoint scanner fields; a struct would force whole-self borrows"
)]
fn emit_window(
    window: &[u8; WINDOW_CAP],
    ring: &[u64; RING],
    base: usize,
    h_end: u64,
    start: usize,
    end: usize,
    key: HashKey,
    emit: &mut impl FnMut(&[u8], u64),
) {
    let len = end - start;
    if (MIN_LEN..=MAX_LEN).contains(&len) {
        let h_before = if start == 0 {
            0
        } else {
            ring[(start - 1) & RING_MASK]
        };
        emit(
            &window[start - base..end - base],
            hashing::from_prefixes_keyed(h_end, h_before, len, key),
        );
    }
}

/// scan a whole slice under explicit options, emitting gram bytes and hashes
///
/// The bytes each emission sees are the scanned stream's — folded when `fold`
/// is on, including the virtual `\n` sentinels when `line_sentinels` is on —
/// which is exactly what an index stores and a plan queries.
pub fn scan_with(
    table: &WeightTable,
    content: &[u8],
    opts: ScanOptions,
    mut emit: impl FnMut(&[u8], u64),
) {
    let mut scanner = StreamScanner::with_options(table, opts);
    scanner.push(content, &mut emit);
    scanner.finish_doc(&mut emit);
}

/// drive a scanner from an async buffered reader, reusing its buffer so nothing is allocated for reads
#[cfg(feature = "stream")]
impl StreamScanner<'_> {
    /// stream a whole reader through the scanner, emitting each gram, returns a forwarded io error if the read fails
    #[allow(
        clippy::missing_errors_doc,
        reason = "the only failure is a forwarded reader io error, named in the summary"
    )]
    pub async fn index_reader<R>(
        &mut self,
        mut reader: R,
        mut emit: impl FnMut(&[u8], u64),
    ) -> std::io::Result<()>
    where
        R: tokio::io::AsyncBufRead + Unpin,
    {
        use tokio::io::AsyncBufReadExt;
        loop {
            let chunk = reader.fill_buf().await?;
            if chunk.is_empty() {
                break;
            }
            let len = chunk.len();
            self.push(chunk, &mut emit);
            reader.consume(len);
        }
        self.finish_doc(&mut emit);
        Ok(())
    }
}

/// Covering grams of a single literal, as raw bytes. The query analysis ANDs
/// these per literal: a document containing `literal` contains all of them
/// (`cover(L) ⊆ scan(D)` for any `D ⊇ L`), so none is a false negative.
#[allow(
    clippy::indexing_slicing,
    reason = "cover emits start..end within literal"
)]
#[must_use]
pub fn cover_one(table: &WeightTable, literal: &[u8]) -> Vec<Gram> {
    let mut grams = Vec::new();
    cover(table, literal, |start, end| {
        if (MIN_LEN..=MAX_LEN).contains(&(end - start)) {
            grams.push(Gram::from(&literal[start..end]));
        }
    });
    grams
}

/// Minimal covering n-grams (danlark1 `BuildCoveringNgrams`): the same hull as
/// [`scan`] restricted to the minimal set, so `cover(L)` is always a subset of
/// `scan(D)` for any `D` containing `L` — the guarantee against missed matches.
#[allow(
    clippy::indexing_slicing,
    reason = "front read only while deque non-empty"
)]
fn cover(table: &WeightTable, s: &[u8], mut emit: impl FnMut(usize, usize)) {
    let mut stack: VecDeque<(u32, usize)> = VecDeque::new();

    for i in 0..s.len().saturating_sub(1) {
        let w = table.weight(s[i], s[i + 1]);
        if stack.len() > 1 && i + 3 - stack[0].1 >= MAX_LEN {
            emit(stack[0].1, stack[1].1 + 2);
            stack.pop_front();
        }
        while let Some(&(top, pos)) = stack.back() {
            if w <= top {
                break;
            }
            if stack[0].0 == top {
                glue_plateau(&mut stack, pos, i + 2, &mut emit);
            }
            stack.pop_back();
        }
        stack.push_back((w, i));
    }
    drain(&mut stack, &mut emit);
}

/// Emit the consecutive grams of an equal-weight plateau, left to right.
fn glue_plateau(
    stack: &mut VecDeque<(u32, usize)>,
    back_pos: usize,
    end: usize,
    emit: &mut impl FnMut(usize, usize),
) {
    emit(back_pos, end);
    drain(stack, emit);
}

/// Pop the stack down to one entry, emitting the gram spanning each popped pair.
fn drain(stack: &mut VecDeque<(u32, usize)>, emit: &mut impl FnMut(usize, usize)) {
    while stack.len() > 1 {
        let Some((_, top)) = stack.pop_back() else {
            break;
        };
        if let Some(&(_, below)) = stack.back() {
            emit(below, top + 2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sngram_types::TABLE_BINARY_SIZE;

    fn table() -> WeightTable {
        let mut buf = vec![0u8; TABLE_BINARY_SIZE];
        buf[..4].copy_from_slice(b"SPNG");
        buf[4..8].copy_from_slice(&1u32.to_le_bytes());
        for c1 in 0u8..=255 {
            for c2 in 0u8..=255 {
                let w = crc32fast::hash(&[c1, c2]);
                let idx = (usize::from(c1) << 8) | usize::from(c2);
                let off = 16 + idx * 4;
                buf[off..off + 4].copy_from_slice(&w.to_le_bytes());
            }
        }
        let crc = crc32fast::hash(&buf[16..]);
        buf[8..12].copy_from_slice(&crc.to_le_bytes());
        WeightTable::from_bytes(&buf).unwrap()
    }

    fn collect(table: &WeightTable, doc: &[u8], opts: ScanOptions) -> Vec<(Vec<u8>, u64)> {
        let mut out = Vec::new();
        scan_with(table, doc, opts, |g, h| out.push((g.to_vec(), h)));
        out
    }

    #[test]
    fn gram_length_bounds_are_frozen_format_vocabulary() {
        assert_eq!(MIN_LEN, 3);
        assert_eq!(MAX_LEN, 100);
    }

    #[test]
    fn default_options_match_legacy_scan() {
        let t = table();
        let doc = b"fn main() { let x = foo_bar(42); }";
        let mut legacy = Vec::new();
        scan(&t, doc, |s, e, h| legacy.push((doc[s..e].to_vec(), h)));
        assert_eq!(collect(&t, doc, ScanOptions::default()), legacy);
    }

    #[test]
    fn sentinels_equal_scanning_bracketed_bytes() {
        let t = table();
        let doc = b"static int sched_clock_init(void);";
        let mut bracketed = Vec::with_capacity(doc.len() + 2);
        bracketed.push(b'\n');
        bracketed.extend_from_slice(doc);
        bracketed.push(b'\n');
        let with_sentinels = collect(
            &t,
            doc,
            ScanOptions {
                line_sentinels: true,
                ..ScanOptions::default()
            },
        );
        assert_eq!(
            with_sentinels,
            collect(&t, &bracketed, ScanOptions::default())
        );
    }

    #[test]
    fn sentinel_grams_reach_document_edges() {
        let t = table();
        let grams = collect(
            &t,
            b"EXPORT_SYMBOL(foo);",
            ScanOptions {
                line_sentinels: true,
                ..ScanOptions::default()
            },
        );
        assert!(
            grams.iter().any(|(g, _)| g.first() == Some(&b'\n')),
            "a leading boundary gram must exist"
        );
        assert!(
            grams.iter().any(|(g, _)| g.last() == Some(&b'\n')),
            "a trailing boundary gram must exist"
        );
    }

    #[test]
    fn fold_equals_scanning_folded_bytes_in_folded_space() {
        let t = table();
        let doc = b"Sched_Clock INIT KFree";
        let folded_doc: Vec<u8> = doc.iter().map(u8::to_ascii_lowercase).collect();
        let via_fold = collect(
            &t,
            doc,
            ScanOptions {
                fold: true,
                ..ScanOptions::default()
            },
        );
        let via_prefold = collect(
            &t,
            &folded_doc,
            ScanOptions {
                key: HashKey::UNKEYED.folded(),
                ..ScanOptions::default()
            },
        );
        assert_eq!(via_fold, via_prefold);
        assert!(!via_fold.is_empty());
    }

    #[test]
    fn folded_space_never_collides_with_primary() {
        let t = table();
        let doc = b"already lowercase text";
        let primary = collect(&t, doc, ScanOptions::default());
        let folded = collect(
            &t,
            doc,
            ScanOptions {
                fold: true,
                ..ScanOptions::default()
            },
        );
        for ((gp, hp), (gf, hf)) in primary.iter().zip(&folded) {
            assert_eq!(gp, gf, "same windows on already-folded text");
            assert_ne!(hp, hf, "spaces must be disjoint for {gp:?}");
        }
    }

    #[test]
    fn keyed_emissions_match_direct_keyed_hashing() {
        let t = table();
        let key = HashKey::new(0x5EED_F00D_0123_4567);
        let doc = b"pub fn read_lock(&self) -> Guard;";
        for (g, h) in collect(
            &t,
            doc,
            ScanOptions {
                key,
                ..ScanOptions::default()
            },
        ) {
            assert_eq!(h, hashing::hash_bytes_keyed(&g, key), "gram {g:?}");
        }
    }

    #[test]
    fn finish_doc_isolates_documents() {
        let t = table();
        let opts = ScanOptions {
            line_sentinels: true,
            ..ScanOptions::default()
        };
        let mut scanner = StreamScanner::with_options(&t, opts);
        let mut first = Vec::new();
        scanner.push(b"first document body", |g, h| first.push((g.to_vec(), h)));
        scanner.finish_doc(|g, h| first.push((g.to_vec(), h)));
        assert!(!first.is_empty(), "first document must emit grams");
        let mut second = Vec::new();
        scanner.push(b"second document body", |g, h| second.push((g.to_vec(), h)));
        scanner.finish_doc(|g, h| second.push((g.to_vec(), h)));
        assert_eq!(second, collect(&t, b"second document body", opts));
    }
}
