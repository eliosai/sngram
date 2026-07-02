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

/// Shortest gram emitted or matched; a sparse gram spans at least one bigram.
pub const MIN_LEN: usize = 3;
/// Longest gram emitted; bounds index entries and covering-set members.
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
}

impl<'t> StreamScanner<'t> {
    /// new scanner bound to a weight table, ready to receive byte chunks
    #[must_use]
    pub fn new(table: &'t WeightTable) -> Self {
        Self {
            matrix: table.matrix(),
            window: [0; WINDOW_CAP],
            wlen: 0,
            base: 0,
            stack: [(0, 0); STACK_CAP],
            slen: 0,
            hash: 0,
            ring: [0; RING],
        }
    }

    /// feed the next chunk, emitting each gram's bytes and rolling hash as it
    /// closes, identical to [`scan`](crate::scan) over the concatenation of all chunks
    #[allow(
        clippy::indexing_slicing,
        reason = "wlen stays <= WINDOW_CAP, stack indices < slen <= STACK_CAP, ring indices masked, and a valid gram start is within MAX_LEN of end, kept in the window by WINDOW_KEEP"
    )]
    #[allow(
        clippy::excessive_nesting,
        clippy::too_many_lines,
        reason = "the hot loop is kept as one linear automaton; splitting it costs measured throughput"
    )]
    pub fn push(&mut self, chunk: &[u8], mut emit: impl FnMut(&[u8], u64)) {
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
            self.window[filled..filled + take].copy_from_slice(&rest[..take]);
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
                while self.slen > 0 {
                    if tw >= w {
                        emit_window(&self.window, &self.ring, self.base, h, tpos, end, &mut emit);
                        self.slen -= usize::from(tw == w);
                        break;
                    }
                    self.slen -= 1;
                    emit_window(&self.window, &self.ring, self.base, h, tpos, end, &mut emit);
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
            hashing::from_prefixes(h_end, h_before, len),
        );
    }
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
        self.finish();
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
