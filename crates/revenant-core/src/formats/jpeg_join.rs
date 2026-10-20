//! Bifragment gap carving for baseline JPEGs: a photo whose entropy-coded data breaks (an impossible code, a foreign
//! file's head, erased space) may continue further on the disk, after a gap holding other data. The search tries every
//! sector in a bounded window after the break where the photo's data could continue, and keeps a continuation only if
//! our entropy decoder walks the two pieces to the exact MCU count with `EOI` right after it, and the DC values of the
//! blocks on both sides of the seam agree as well as the rest of the image does.
//!
//! Bounds: the first piece is at least [`MIN_FIRST_PIECE`]; the window is [`JPEG_GAP_MAX`] bytes (one record per sector);
//! at most [`MAX_JOIN_TRIES`] joins are decoded and the tries read at most [`JOIN_BYTES_MAX`] bytes in all. A try whose
//! seam is sure to fail the seam test once the MCU row below it is decoded stops there, and so does a try reaching a
//! decoder state a failed try reached over the same bytes. The search looks at the scan's cancel flag every MiB of the window, before every try and
//! at every MCU row of a try.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};

use super::jpeg::{JPEG_MAX_LEN, Jpeg, contiguous, data_end, walk_header};
use super::jpeg_entropy::{self, Cursor, End, Frame};
use super::{Format, Fragment, HEAD_LEN, Health, Reader, SECTOR, builtin, cancelled};
use crate::EngineError;
use crate::source::{Fragmented, Source, VolumeId};

/// Bytes after the break in which a continuation may start (and end).
pub(crate) const JPEG_GAP_MAX: u64 = 64 << 20;
/// Sector boundaries before the first sign of a break where the first piece may end.
const BREAK_BACK: u64 = 4;
/// Sector starts tried per distinct `EOI` (the first ones whose first event is that `EOI`).
const PER_EOI: usize = 4;
/// Joins decoded at most. The search is exhaustive: hitting this cap means a continuation may have gone untried, so the
/// result cannot be unique.
pub(crate) const MAX_JOIN_TRIES: usize = 4096;
/// Bytes the tries (and the checks of the files in the gap) may read from the source, per search (same meaning as
/// [`MAX_JOIN_TRIES`] when hit).
const JOIN_BYTES_MAX: u64 = 64 << 20;
/// Shortest first piece a search starts from: a few sectors of a photo hold too little of it to recover, and every head
/// with a frame could otherwise start a search (ruling Q8).
const MIN_FIRST_PIECE: u64 = 16 << 10;
/// Bytes a try's reader loads at a time (most tries stop within a few rows).
const TRY_WINDOW: usize = 64 << 10;
/// Decoder states remembered from failed tries (see [`Memo`]).
const MEMO_MAX: usize = 16_384;
/// Window sectors examined between two looks at the cancel flag (1 MiB).
const SECTORS_PER_CANCEL_CHECK: u64 = 2048;
/// Most first-piece bytes kept in memory for the tries.
const FIRST_PIECE_TAIL_MAX: u64 = 1 << 20;

/// Per-sector record of the window pass: no event yet, or a "not this photo's data" event.
const NONE: u64 = u64::MAX;
const BAD: u64 = u64::MAX - 1;

#[cfg(test)]
thread_local! {
    /// Joins decoded by `join` on this thread.
    pub(crate) static TRIES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Tries stopped by a state a failed try reached, on this thread.
    pub(crate) static MEMO_HITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// A JPEG put back together from two pieces.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Joined {
    pub fragments: [Fragment; 2],
    pub len: u64,
    /// Exactly one candidate passed AND every candidate in the window was tried (no cap or budget hit).
    pub unique: bool,
}

/// Where the first piece may end, with the decoder's state to resume from.
struct Break {
    /// Absolute, sector-aligned end of the first piece.
    c1: u64,
    /// The last cursor of the contiguous walk at or before `c1`.
    cur: Cursor,
    /// The early seam test's base (see [`early_base`]); `None` when there is none.
    early_base: Option<f64>,
}

/// Bifragment gap carving for the baseline JPEG at `start` (entropy data from `scan_start`) whose contiguous walk ended
/// at `cut` without an `Intact` result. `None` when the frame is not one our decoder covers, the contiguous data already
/// decodes to `EOI` with every MCU, no continuation passes, or the scan was cancelled.
pub(crate) fn join(r: &mut Reader, start: u64, scan_start: u64, cut: u64) -> Option<Joined> {
    let frame = jpeg_entropy::frame(r, start, scan_start)?;
    let mut run0 = jpeg_entropy::decode(r, &frame, frame.start(scan_start), cut, true);
    if matches!(run0.end, End::Eoi { .. }) {
        return None;
    }
    // The first sign that the data stopped being this photo: a decode error, the cut, or a uniform (erased) sector,
    // which the decoder would walk through as valid blocks. The decode is redone up to such a sector, so its resume
    // points lie right before it. An error is reported at a marker's `FF`, whose next byte may be the first one that is
    // not this photo's: the photo may break right after the `FF`.
    let mut x = match run0.end {
        End::Error { at } => (at + 1).min(cut),
        _ => cut,
    };
    let used = run0.pos.saturating_add(2).min(cut);
    let uniform = data_end(r, scan_start, used);
    if uniform < x {
        x = uniform;
        run0 = jpeg_entropy::decode(r, &frame, frame.start(scan_start), uniform, true);
    }

    let bpm = blocks_per_mcu(&frame);
    let first_rows = Dc {
        head: &run0.dc,
        tail: &[],
    };
    let boundaries = boundary_means(&frame, first_rows);
    let last_sector = x - x % SECTOR;
    let breaks: Vec<Break> = (0..BREAK_BACK)
        .filter_map(|k| last_sector.checked_sub(k * SECTOR))
        .filter(|&c| c > scan_start && c + BREAK_BACK * SECTOR > x && c - start >= MIN_FIRST_PIECE)
        .filter_map(|c1| {
            let cur = run0.resume.iter().rev().find(|c| c.pos <= c1)?.clone();
            let early_base = early_base(&frame, &boundaries, cur.mcu);
            Some(Break {
                c1,
                cur,
                early_base,
            })
        })
        .collect();
    let lowest = breaks.iter().map(|b| b.c1).min()?;

    // The first piece's last bytes, read once (while the reader still holds them): every try starts in them.
    let tail_from = breaks
        .iter()
        .map(|b| start + (b.cur.pos - start) / SECTOR * SECTOR)
        .min()?
        .max(x.saturating_sub(FIRST_PIECE_TAIL_MAX));
    let tail_to = breaks.iter().map(|b| b.c1).max()?;
    let mut tail = vec![0u8; (tail_to - tail_from) as usize];
    let n = r.read(tail_from, &mut tail);
    tail.truncate(n);
    let source = Overlay {
        inner: r.source,
        at: tail_from,
        bytes: tail,
        read: AtomicU64::new(0),
    };

    let Window {
        candidates,
        truncated,
        heads,
    } = window(r, lowest)?;

    let mut tries = 0usize;
    let mut memo = Memo::default();
    let mut first: Option<(u64, u64, u64)> = None;
    let mut passes = 0usize;
    let mut exhausted = true;
    'search: for &(g, e) in &candidates {
        for b in &breaks {
            if g <= b.c1 || (b.c1 - start) + (e + 2 - g) > JPEG_MAX_LEN {
                continue;
            }
            if cancelled() {
                return None;
            }
            if tries >= max_tries() || source.read() >= JOIN_BYTES_MAX {
                exhausted = false;
                break 'search;
            }
            tries += 1;
            #[cfg(test)]
            TRIES.with(|t| t.set(t.get() + 1));
            let pieces = [
                Fragment {
                    start,
                    len: b.c1 - start,
                },
                Fragment {
                    start: g,
                    len: e + 2 - g,
                },
            ];
            match try_join(&source, &frame, &run0.dc[..], bpm, b, pieces, &mut memo) {
                Try::Pass => {
                    passes += 1;
                    if first.is_some() {
                        break 'search; // a second pass: the result is at most `Partial` whatever else passes
                    }
                    first = Some((b.c1, g, e));
                }
                Try::Fail => {}
                Try::Unknown if cancelled() => return None,
                Try::Unknown => {
                    exhausted = false; // the budget ran out inside this try
                    break 'search;
                }
            }
        }
    }
    let (c1, g, e) = first?;
    // Q7: an EOI whose sectors were not all tried (more than PER_EOI of them) may hide a continuation, unless it is the
    // passing continuation's own (the rest are inside it) or those sectors belong to a file in the gap that is whole on
    // its own and coded differently from this photo.
    let on_restart = frame.restart_interval > 0 && on_restart_marker(&source, c1, g);
    let mut unique = passes == 1 && exhausted && !on_restart;
    if unique {
        let mut checked = HashMap::new();
        unique = truncated
            .iter()
            .filter(|t| t.1 != e)
            .all(|&(g0, te)| explained(&source, &frame, &heads, g0, te, &mut checked));
    }
    let fragments = [
        Fragment {
            start,
            len: c1 - start,
        },
        Fragment {
            start: g,
            len: e + 2 - g,
        },
    ];
    Some(Joined {
        fragments,
        len: fragments[0].len + fragments[1].len,
        unique,
    })
}

/// Q15: whether the seam between a first piece ending at `c1` and a second piece starting at `g` falls exactly on a
/// restart marker: the first piece ends with one, the second starts with one, or the marker is split between them. Every
/// DC predictor resets there, so any photo of the same frame continues validly and the DC seam test carries little
/// evidence: the join is at most `Partial`.
fn on_restart_marker(source: &dyn Source, c1: u64, g: u64) -> bool {
    let (mut before, mut after) = ([0u8; 2], [0u8; 2]);
    let read = |at: u64, buf: &mut [u8; 2]| source.read_at(at, buf).is_ok_and(|n| n == 2);
    if !read(c1 - 2, &mut before) || !read(g, &mut after) {
        return false;
    }
    let rst = |b: u8| (0xD0..=0xD7).contains(&b);
    (before[0] == 0xFF && rst(before[1]))
        || (after[0] == 0xFF && rst(after[1]))
        || (before[1] == 0xFF && rst(after[0]))
}

/// Whether the sectors of the EOI at `e` whose first one is `g0` belong to a file in the gap: the last file head before
/// `g0` is a JPEG measured whole (`Intact`) on its own, contiguously up to that EOI, with a frame other than `frame` (the
/// same frame could hold this photo's continuation: two photos of one camera written interleaved). Each head is
/// measured once (`checked`), through `source`, so it counts against the search's read budget.
fn explained(
    source: &Overlay,
    frame: &Frame,
    heads: &[u64],
    g0: u64,
    e: u64,
    checked: &mut HashMap<u64, Option<u64>>,
) -> bool {
    let i = heads.partition_point(|&h| h < g0);
    let Some(&h) = i.checked_sub(1).and_then(|i| heads.get(i)) else {
        return false;
    };
    let end = *checked.entry(h).or_insert_with(|| {
        if source.read() >= JOIN_BYTES_MAX {
            return None;
        }
        let mut hr = Reader::new(source);
        let (head, n) = hr.head(h);
        if !Jpeg.recognizes(&head[..n]) {
            return None;
        }
        let header = walk_header(&mut hr, h, JPEG_MAX_LEN)?;
        if jpeg_entropy::frame(&mut hr, h, header.scan_start).as_ref() == Some(frame) {
            return None;
        }
        let m = contiguous(&mut hr, h, &header);
        (m.health == Health::Intact).then_some(h + m.len)
    });
    end == Some(e + 2)
}

/// How a try ended.
enum Try {
    Pass,
    Fail,
    /// Cancelled, or the read budget ran out inside it.
    Unknown,
}

/// A decoder state inside a second piece: the absolute position of the next byte, the EOI the piece ends at, and the
/// state's [`Cursor::key`]. The rest of the walk from it reads the same bytes and ends the same way, whatever came before.
type MemoKey = (u64, u64, (u64, u64, u32, [i32; 4]));

/// States that failed tries passed through at MCU-row boundaries (at most [`MEMO_MAX`]): a later try reaching one fails
/// the same way, so it stops there (tries that share the EOI often fall into step a few rows after their seams).
#[derive(Default)]
struct Memo(HashSet<MemoKey>);
#[cfg(test)]
thread_local! {
    /// Lowered by tests to exercise the cap cheaply.
    pub(crate) static MAX_TRIES: std::cell::Cell<usize> = const { std::cell::Cell::new(MAX_JOIN_TRIES) };
}

#[cfg(test)]
fn max_tries() -> usize {
    MAX_TRIES.with(std::cell::Cell::get)
}

#[cfg(not(test))]
fn max_tries() -> usize {
    MAX_JOIN_TRIES
}

/// One forward pass over the sectors `g` in `(lowest, lowest + JPEG_GAP_MAX)` (and before the source's end). For each,
/// the first event at or after `g`: an `FF D9` (`EOI` at that `FF`), or a sign that the bytes are not entropy-coded data
/// (an `FF` followed by anything but `00`, `D0`–`D7`, `D9` or `FF`; a sector of one repeated byte value or an unreadable
/// one; a sector where another file starts). Returns the candidates `(g, e)`: for each distinct `EOI` at `e`, the first
/// [`PER_EOI`] sector starts whose first event it is, ascending. `None` if the scan was cancelled.
fn window(r: &mut Reader, lowest: u64) -> Option<Window> {
    let first = lowest + SECTOR;
    let end = lowest.saturating_add(JPEG_GAP_MAX).min(r.len);
    let mut heads = Vec::new();
    if first >= end {
        return Some(Window {
            candidates: Vec::new(),
            truncated: Vec::new(),
            heads,
        });
    }
    let sectors = (end - first).div_ceil(SECTOR) as usize;
    let mut rec = vec![NONE; sectors];
    let formats = builtin();
    let mut bytes = [0u8; SECTOR as usize];
    // One forward sweep, so the reader loads each window once.
    for (i, rec) in rec.iter_mut().enumerate() {
        if (i as u64).is_multiple_of(SECTORS_PER_CANCEL_CHECK) && cancelled() {
            return None;
        }
        let s = first + i as u64 * SECTOR;
        let next = s + SECTOR;
        // The sector, read once (the byte after it only when an `FF` ends the sector, so the reader moves forward).
        let n = r.read(s, &mut bytes);
        // Events at the sector's start come first: another file's head, or an erased or unreadable sector (`r` reads
        // the disk itself, so the sector alignment is absolute).
        let head = &bytes[..n.min(HEAD_LEN)];
        if formats
            .iter()
            .any(|f| f.recognizes(head) && f.confirms(r.source, s))
        {
            heads.push(s);
            *rec = BAD;
            continue;
        }
        if data_end(r, s, next.min(end)) == s {
            *rec = BAD;
            continue;
        }
        let data = &bytes[..n];
        let mut p = 0;
        while let Some(k) = memchr::memchr(0xFF, &data[p..]) {
            let f = p + k;
            let following = match data.get(f + 1) {
                Some(&b) => Some(b),
                None => r.byte(next),
            };
            match following {
                None => break,
                Some(0x00 | 0xD0..=0xD7 | 0xFF) => p = f + 1,
                Some(0xD9) => {
                    *rec = s + f as u64;
                    break;
                }
                Some(_) => {
                    *rec = BAD;
                    break;
                }
            }
        }
    }
    // The first event at or after each sector start.
    let mut next = NONE;
    for v in rec.iter_mut().rev() {
        if *v == NONE {
            *v = next;
        } else {
            next = *v;
        }
    }
    let mut candidates = Vec::new();
    let mut truncated = Vec::new();
    let (mut last, mut taken, mut group) = (NONE, 0, 0);
    for (i, &e) in rec.iter().enumerate() {
        if e == NONE || e == BAD {
            continue;
        }
        let g = first + i as u64 * SECTOR;
        if e != last {
            (last, taken, group) = (e, 0, g);
        }
        if taken < PER_EOI {
            candidates.push((g, e));
            taken += 1;
        } else if taken == PER_EOI {
            truncated.push((group, e));
            taken += 1;
        }
    }
    Some(Window {
        candidates,
        truncated,
        heads,
    })
}

/// What the window pass found.
struct Window {
    /// `(g, e)`: a sector start and the EOI that is its first event, in the order they are tried.
    candidates: Vec<(u64, u64)>,
    /// `(first sector, e)` of each EOI with more than [`PER_EOI`] sectors whose first event it is (not all tried).
    truncated: Vec<(u64, u64)>,
    /// Sector starts where another file starts, ascending.
    heads: Vec<u64>,
}

/// Decodes the photo through `pieces` from the break `b`: passes when every MCU decodes, `EOI` follows right at the end
/// of the second piece, and the DC seam test holds. At each MCU row it looks at the cancel flag and the read budget
/// (`Unknown` if either stops it) and at `memo`; after the MCU row below the seam it gives up if the seam is already sure
/// to fail the test (see [`early_base`]).
fn try_join(
    source: &Overlay,
    frame: &Frame,
    dc0: &[i32],
    bpm: u64,
    b: &Break,
    pieces: [Fragment; 2],
    memo: &mut Memo,
) -> Try {
    let start = pieces[0].start;
    let first_len = pieces[0].len;
    let (g, e) = (pieces[1].start, pieces[1].start + pieces[1].len - 2);
    let view = Fragmented::new(source, pieces.to_vec());
    let len = view.len();
    let mut vr = Reader::with_window(&view, TRY_WINDOW);
    let from = b.cur.shifted_back(start);
    let seam = b.cur.mcu;
    let head = &dc0[..(seam * bpm) as usize];
    let row = u64::from(frame.mcus_x);
    let check_at = (seam / row + 2) * row;
    let mut seen: Vec<MemoKey> = Vec::new();
    let mut stopped: Option<Try> = None;
    let mut seam_failed = false;
    let run = jpeg_entropy::decode_rows(
        &mut vr,
        frame,
        from,
        len,
        true,
        false,
        u64::MAX,
        &mut |c, dc| {
            if cancelled() || source.read() >= JOIN_BYTES_MAX {
                stopped = Some(Try::Unknown);
                return false;
            }
            if c.pos >= first_len {
                let key = (g + (c.pos - first_len), e, c.key());
                if memo.0.contains(&key) {
                    #[cfg(test)]
                    MEMO_HITS.with(|h| h.set(h.get() + 1));
                    stopped = Some(Try::Fail);
                    return false;
                }
                seen.push(key);
            }
            if c.mcu == check_at
                && let (Some(base), Some(at_seam)) =
                    (b.early_base, seam_mean(frame, Dc { head, tail: dc }, seam))
                && !within(at_seam, base)
            {
                seam_failed = true;
                return false;
            }
            true
        },
    );
    match stopped {
        Some(Try::Unknown) => return Try::Unknown,
        Some(_) => {}
        None if seam_failed => return Try::Fail,
        None if run.end == (End::Eoi { at: len - 2 }) => {
            let whole: Vec<i32> = head.iter().chain(&run.dc).copied().collect();
            return if seam_ok(frame, &whole, seam) {
                Try::Pass
            } else {
                Try::Fail
            };
        }
        None => {}
    }
    // The walk itself failed (or reached a state that did): every state it passed through leads to the same failure.
    for key in seen {
        if memo.0.len() >= MEMO_MAX {
            break;
        }
        memo.0.insert(key);
    }
    Try::Fail
}

/// Luma DC values in coding order, as two consecutive slices (the first piece's, then the continuation's).
#[derive(Clone, Copy)]
struct Dc<'a> {
    head: &'a [i32],
    tail: &'a [i32],
}

impl Dc<'_> {
    fn get(&self, i: u64) -> Option<i32> {
        let i = usize::try_from(i).ok()?;
        match i.checked_sub(self.head.len()) {
            None => Some(self.head[i]),
            Some(j) => self.tail.get(j).copied(),
        }
    }
}

fn blocks_per_mcu(frame: &Frame) -> u64 {
    let (h, v) = frame.luma_blocks_per_mcu();
    u64::from(h * v)
}

/// The luma DC grid of a frame: `blocks_x × blocks_y` blocks over the MCU grid.
struct Grid<'a> {
    dc: Dc<'a>,
    mcus_x: u64,
    h: u64,
    v: u64,
    blocks_x: u64,
    blocks_y: u64,
}

impl<'a> Grid<'a> {
    fn new(frame: &Frame, dc: Dc<'a>) -> Self {
        let (h, v) = frame.luma_blocks_per_mcu();
        let (h, v) = (u64::from(h), u64::from(v));
        Grid {
            dc,
            mcus_x: u64::from(frame.mcus_x),
            h,
            v,
            blocks_x: u64::from(frame.mcus_x) * h,
            blocks_y: u64::from(frame.mcus_y) * v,
        }
    }

    /// DC of the block at column `bx`, row `by`; `None` if it was not decoded.
    fn at(&self, bx: u64, by: u64) -> Option<i32> {
        let mcu = by / self.v * self.mcus_x + bx / self.h;
        let k = by % self.v * self.h + bx % self.h;
        self.dc.get(mcu * self.h * self.v + k)
    }

    /// `|ΔDC|` between block row `by` and the row above it, summed over `columns`, with the pair count; pairs not decoded
    /// are skipped.
    fn diffs(&self, by: u64, columns: std::ops::Range<u64>) -> (f64, u64) {
        let (mut sum, mut n) = (0.0, 0);
        if by == 0 || by >= self.blocks_y {
            return (sum, n);
        }
        for bx in columns {
            if let (Some(a), Some(b)) = (self.at(bx, by - 1), self.at(bx, by)) {
                sum += f64::from(a.abs_diff(b));
                n += 1;
            }
        }
        (sum, n)
    }
}

/// Mean `|ΔDC|` of every block-row boundary `b` (between rows `b − 1` and `b`) fully decoded, as `(b, mean)`.
fn boundary_means(frame: &Frame, dc: Dc) -> Vec<(u64, f64)> {
    let g = Grid::new(frame, dc);
    (1..g.blocks_y)
        .map_while(|by| {
            let (sum, n) = g.diffs(by, 0..g.blocks_x);
            (n == g.blocks_x).then(|| (by, sum / n as f64))
        })
        .collect()
}

/// The block-row boundaries the seam at MCU `seam_mcu` (row `R`, column `C`) runs along: for the MCU columns `≥ C` the
/// top of MCU row `R`, for those `< C` the top of MCU row `R + 1`. As `(block row, block columns)`.
fn seam_rows(g: &Grid, seam_mcu: u64) -> [(u64, std::ops::Range<u64>); 2] {
    let (r, c) = (seam_mcu / g.mcus_x, seam_mcu % g.mcus_x);
    [(r * g.v, c * g.h..g.blocks_x), ((r + 1) * g.v, 0..c * g.h)]
}

/// Mean `|ΔDC|` across the seam at `seam_mcu`; `None` when no pair across it was decoded.
fn seam_mean(frame: &Frame, dc: Dc, seam_mcu: u64) -> Option<f64> {
    let g = Grid::new(frame, dc);
    let (mut sum, mut n) = (0.0, 0);
    for (by, cols) in seam_rows(&g, seam_mcu) {
        let (s, k) = g.diffs(by, cols);
        sum += s;
        n += k;
    }
    (n > 0).then(|| sum / n as f64)
}

/// An upper bound on the base [`seam_ok`] will compare the seam at `seam_mcu` with, from the block-row boundaries
/// `known` (as `(b, mean)`, decoded from the first piece); `None` when they cannot bound it. The base is the median of
/// the `n` boundaries off the seam; those above the seam are some of them, and the median of `n` values is at most the
/// `⌊n/2⌋`-th smallest (from 0) of any `k > ⌊n/2⌋` of them. So a seam rougher than 3 × this bound + 4 fails the final
/// test too (ruling Q14: the early test only drops tries that cannot pass). With the seam in the upper half of the image
/// there is no bound: the rows below, not decoded yet, could be rougher than the ones above (sky over a landscape).
fn early_base(frame: &Frame, known: &[(u64, f64)], seam_mcu: u64) -> Option<f64> {
    let g = Grid::new(
        frame,
        Dc {
            head: &[],
            tail: &[],
        },
    );
    let seam = seam_rows(&g, seam_mcu);
    let on_seam = seam
        .iter()
        .filter(|(by, cols)| (1..g.blocks_y).contains(by) && !cols.is_empty())
        .count() as u64;
    let n = g.blocks_y.saturating_sub(1).saturating_sub(on_seam);
    let top = seam[0].0;
    let mut above: Vec<f64> = known
        .iter()
        .filter(|&&(b, _)| b < top)
        .map(|&(_, m)| m)
        .collect();
    let i = usize::try_from(n / 2).ok()?;
    if above.len() <= i {
        return None;
    }
    above.sort_by(f64::total_cmp);
    Some(above[i])
}

/// The seam is no rougher than the image: `V_seam ≤ 3 × V_base + 4`.
fn within(at_seam: f64, base: f64) -> bool {
    at_seam <= 3.0 * base + 4.0
}

fn median(values: impl Iterator<Item = f64>) -> Option<f64> {
    let mut v: Vec<f64> = values.collect();
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    let mid = v.len() / 2;
    Some(if !v.len().is_multiple_of(2) {
        v[mid]
    } else {
        (v[mid - 1] + v[mid]) / 2.0
    })
}

/// DC seam test. `dc`: luma DC of every block of the whole joined image in coding order; `seam_mcu`: the first MCU
/// decoded with bytes of the second piece. Passes iff the image has at least 4 block rows and the mean `|ΔDC|` across the
/// seam is at most 3 × the median of the other block-row boundaries' means + 4.
pub(crate) fn seam_ok(frame: &Frame, dc: &[i32], seam_mcu: u64) -> bool {
    let dc = Dc {
        head: dc,
        tail: &[],
    };
    let g = Grid::new(frame, dc);
    if g.blocks_y < 4 {
        return false;
    }
    let seam = seam_rows(&g, seam_mcu);
    let on_seam = |b: u64| seam.iter().any(|(by, cols)| *by == b && !cols.is_empty());
    let base = median(
        boundary_means(frame, dc)
            .into_iter()
            .filter(|&(b, _)| !on_seam(b))
            .map(|(_, m)| m),
    );
    match (seam_mean(frame, dc, seam_mcu), base) {
        (Some(at_seam), Some(base)) => within(at_seam, base),
        _ => false,
    }
}

/// `inner`, with the bytes `[at, at + bytes.len())` served from memory (reads wholly inside them), counting the bytes
/// read from `inner` (the search's read budget).
struct Overlay<'a> {
    inner: &'a dyn Source,
    at: u64,
    bytes: Vec<u8>,
    read: AtomicU64,
}

impl Overlay<'_> {
    fn read(&self) -> u64 {
        self.read.load(Ordering::Relaxed)
    }
}

impl Source for Overlay<'_> {
    fn len(&self) -> u64 {
        self.inner.len()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, EngineError> {
        let end = self.at + self.bytes.len() as u64;
        if offset >= self.at && offset + buf.len() as u64 <= end {
            let from = (offset - self.at) as usize;
            buf.copy_from_slice(&self.bytes[from..from + buf.len()]);
            return Ok(buf.len());
        }
        let n = self.inner.read_at(offset, buf)?;
        self.read.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }

    fn volume(&self) -> Option<VolumeId> {
        self.inner.volume()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::tests::Mem;
    use crate::formats::{CancelScope, jpeg::walk_header};
    use crate::scan::CancelToken;
    use revenant_testkit as testkit;

    fn frame_of(bytes: &[u8]) -> Frame {
        let src = Mem(bytes.to_vec());
        let mut r = Reader::new(&src);
        let scan_start = walk_header(&mut r, 0, JPEG_MAX_LEN).unwrap().scan_start;
        jpeg_entropy::frame(&mut r, 0, scan_start).expect("a supported frame")
    }

    /// A 64×48-block grid filled by `f(bx, by)` in coding order (one block per MCU).
    fn grid(mut f: impl FnMut(u64, u64) -> i32) -> Vec<i32> {
        (0..48u64)
            .flat_map(|by| (0..64u64).map(move |bx| (bx, by)))
            .map(|(bx, by)| f(bx, by))
            .collect()
    }

    #[test]
    fn seam_check_rejects_a_glued_dc_grid() {
        let big = frame_of(&testkit::jpeg_flat_with_restarts(512, 384, 0));
        assert_eq!((big.mcus_x, big.mcus_y), (64, 48));
        let a = |bx: u64, by: u64| (2 * by + bx) as i32;
        let b = |bx: u64, by: u64| 500 - 3 * by as i32 + 2 * bx as i32;
        let smooth = grid(a);
        for seam in [64, 24 * 64, 24 * 64 + 17, 47 * 64 + 63, 5] {
            assert!(seam_ok(&big, &smooth, seam), "smooth, seam at {seam}");
        }
        // Rows < 24 from one gradient, ≥ 24 from another.
        let glued = grid(|bx, by| if by < 24 { a(bx, by) } else { b(bx, by) });
        assert!(!seam_ok(&big, &glued, 24 * 64));
        // Glued mid-row: MCUs before (row 24, column 10) from A, the rest from B.
        let mid = grid(|bx, by| {
            if by * 64 + bx < 24 * 64 + 10 {
                a(bx, by)
            } else {
                b(bx, by)
            }
        });
        assert!(!seam_ok(&big, &mid, 24 * 64 + 10));
        // Noise: the seam is as rough as the rest.
        let mut x = 99u64;
        let noise = grid(|_, _| {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((x >> 40) % 200) as i32 - 100
        });
        assert!(seam_ok(&big, &noise, 24 * 64));
        // Fewer than 4 block rows: never verifiable.
        let small = frame_of(&testkit::jpeg_flat_with_restarts(512, 24, 0));
        assert_eq!((small.mcus_x, small.mcus_y), (64, 3));
        let flat = vec![7; 64 * 3];
        assert!(!seam_ok(&small, &flat, 64));
    }

    #[test]
    fn the_early_seam_test_never_rejects_what_the_final_one_accepts() {
        // Sky over texture, as in a landscape: flat block rows above row `top`, rough ones from it on. Wherever the seam
        // falls, a join the final test accepts must not be dropped after the MCU row below the seam.
        let big = frame_of(&testkit::jpeg_flat_with_restarts(512, 384, 0));
        let mut x = 5u64;
        let mut noise = move || {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((x >> 40) % 120) as i32 - 60
        };
        let (mut accepted, mut sure) = (0, 0);
        for top in [6, 12, 20, 24, 30, 36, 42] {
            let dc = grid(|_, by| if by < top { 100 } else { 100 + noise() });
            for row in [top - 2, top, top + 2] {
                for col in [0, 17, 63] {
                    let seam = row * 64 + col;
                    let at_seam = seam_mean(
                        &big,
                        Dc {
                            head: &dc,
                            tail: &[],
                        },
                        seam,
                    )
                    .unwrap();
                    let head = Dc {
                        head: &dc[..seam as usize],
                        tail: &[],
                    };
                    let early = early_base(&big, &boundary_means(&big, head), seam);
                    let rejects = early.is_some_and(|base| !within(at_seam, base));
                    if seam_ok(&big, &dc, seam) {
                        accepted += 1;
                        assert!(
                            !rejects,
                            "seam at row {row}, column {col}, texture from {top}"
                        );
                    } else {
                        sure += usize::from(rejects);
                    }
                }
            }
        }
        assert!(accepted > 20, "{accepted}");
        // Still a sure reject below the middle of the image: a smooth photo glued to another one.
        let a = |bx: u64, by: u64| (2 * by + bx) as i32;
        let glued = grid(|bx, by| {
            if by < 40 {
                a(bx, by)
            } else {
                500 - 3 * by as i32
            }
        });
        let head = Dc {
            head: &glued[..40 * 64],
            tail: &[],
        };
        let at_seam = seam_mean(
            &big,
            Dc {
                head: &glued,
                tail: &[],
            },
            40 * 64,
        )
        .unwrap();
        let base = early_base(&big, &boundary_means(&big, head), 40 * 64).expect("a bound");
        assert!(!within(at_seam, base) && !seam_ok(&big, &glued, 40 * 64));
        assert!(sure > 0, "the early test never fires");
    }

    /// `a = jpeg(640, 480, 3, 0)`: `a[..c]` at 0 then `sectors` sectors of `[00; 510] FF D9`.
    fn hostile(sectors: usize) -> (Vec<u8>, u64) {
        let a = testkit::jpeg(640, 480, 3, 0);
        let c = (a.len() / 2).next_multiple_of(512);
        let mut img = a[..c].to_vec();
        for _ in 0..sectors {
            img.extend_from_slice(&[0; 510]);
            img.extend_from_slice(&[0xFF, 0xD9]);
        }
        let scan_start = {
            let src = Mem(a.clone());
            walk_header(&mut Reader::new(&src), 0, JPEG_MAX_LEN)
                .unwrap()
                .scan_start
        };
        (img, scan_start)
    }

    #[test]
    fn cancelled_search_stops_before_trying() {
        let (img, scan_start) = hostile(4096);
        let src = Mem(img);
        let cut = src.0.len() as u64;
        let tries = || TRIES.with(std::cell::Cell::get);

        let before = tries();
        let token = CancelToken::default();
        let _scope = CancelScope::enter(&token);
        join(&mut Reader::new(&src), 0, scan_start, cut);
        assert!(tries() - before > 1000, "not cancelled: it tries");

        token.cancel();
        let before = tries();
        assert_eq!(join(&mut Reader::new(&src), 0, scan_start, cut), None);
        assert_eq!(tries() - before, 0);
    }

    #[test]
    fn hitting_the_cap_is_not_unique() {
        let a = testkit::jpeg_smooth(1024, 768, 1);
        let c = (a.len() / 2).next_multiple_of(512);
        let scan_start = {
            let src = Mem(a.clone());
            walk_header(&mut Reader::new(&src), 0, JPEG_MAX_LEN)
                .unwrap()
                .scan_start
        };
        // a[..c], 4 KiB of zeros, the rest of a, then 64 sectors each ending in FF D9.
        let tail_at = c + 4096;
        let mut img = vec![0u8; tail_at];
        img[..c].copy_from_slice(&a[..c]);
        img.extend_from_slice(&a[c..]);
        img.resize(img.len().next_multiple_of(512), 0x11);
        for _ in 0..64 {
            img.extend_from_slice(&[0; 510]);
            img.extend_from_slice(&[0xFF, 0xD9]);
        }
        let src = Mem(img);
        let expected = [
            Fragment {
                start: 0,
                len: c as u64,
            },
            Fragment {
                start: tail_at as u64,
                len: (a.len() - c) as u64,
            },
        ];
        let cut = c as u64; // the contiguous walk would stop at the zeros
        let found = join(&mut Reader::new(&src), 0, scan_start, cut).expect("joined");
        assert_eq!((found.fragments, found.unique), (expected, true));
        MAX_TRIES.with(|m| m.set(8));
        let capped = join(&mut Reader::new(&src), 0, scan_start, cut);
        MAX_TRIES.with(|m| m.set(MAX_JOIN_TRIES));
        let capped = capped.expect("joined before the cap");
        assert_eq!((capped.fragments, capped.unique), (expected, false));
    }

    /// Cancels `token` at the first read of `[at, ..)` no larger than the try reader's window (the window pass reads 1 MiB
    /// at a time), and counts the reads there after that.
    struct CancelOnTail {
        inner: Mem,
        at: u64,
        token: CancelToken,
        after: std::sync::atomic::AtomicU64,
    }

    impl Source for CancelOnTail {
        fn len(&self) -> u64 {
            self.inner.len()
        }
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, EngineError> {
            if offset >= self.at && buf.len() <= TRY_WINDOW {
                if self.token.is_cancelled() {
                    self.after
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                } else {
                    self.token.cancel();
                }
            }
            self.inner.read_at(offset, buf)
        }
        fn volume(&self) -> Option<VolumeId> {
            None
        }
    }

    #[test]
    fn cancel_stops_a_try_within_a_row() {
        // A flat 8192×4096 photo (about 384 KiB of data, rows of 768 bytes) split after 32 KiB, its tail 1 MiB later:
        // the try of the true tail reads several windows of 64 KiB. The scan is cancelled as that try first reads the
        // tail; it must stop at the next MCU row, not read the rest of the tail.
        let a = testkit::jpeg_flat_with_restarts(8192, 4096, 0);
        let scan_start = {
            let src = Mem(a.clone());
            walk_header(&mut Reader::new(&src), 0, JPEG_MAX_LEN)
                .unwrap()
                .scan_start
        };
        let c = (scan_start as usize + 32 * 1024).next_multiple_of(512);
        let tail_at = c + (1 << 20);
        let mut img = vec![0u8; tail_at + a.len() - c + (4 << 20)];
        img[..c].copy_from_slice(&a[..c]);
        let mut x = 7u64;
        for b in &mut img[c..tail_at] {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            *b = (x >> 56) as u8;
        }
        img[tail_at..tail_at + a.len() - c].copy_from_slice(&a[c..]);
        let token = CancelToken::default();
        let src = CancelOnTail {
            inner: Mem(img),
            at: tail_at as u64,
            token: token.clone(),
            after: Default::default(),
        };
        let _scope = CancelScope::enter(&token);
        assert_eq!(join(&mut Reader::new(&src), 0, scan_start, c as u64), None);
        assert!(token.is_cancelled(), "the try reached the tail");
        assert_eq!(src.after.load(std::sync::atomic::Ordering::Relaxed), 0);
    }

    #[test]
    fn failed_tries_are_reused() {
        // A flat photo (every MCU the same 6 bits, so 1536 bytes are exactly 2048 MCUs) split after 32 KiB, and 1 MiB
        // later its rest with 4 KiB of it repeated (one MCU count too many: every try fails). The tries from 1536 bytes
        // before the cut at the group's first sector, and from the cut 1536 bytes into it, skip the same MCUs of the same
        // data: the second reaches a state the first failed from, and stops there.
        let a = testkit::jpeg_flat_with_restarts(8192, 4096, 0);
        let scan_start = {
            let src = Mem(a.clone());
            walk_header(&mut Reader::new(&src), 0, JPEG_MAX_LEN)
                .unwrap()
                .scan_start
        };
        let c = (scan_start as usize + 32 * 1024).next_multiple_of(512);
        let tail_at = c + (1 << 20);
        let tail = &a[c - 4096..];
        let mut img = vec![0u8; tail_at + tail.len() + (1 << 20)];
        img[..c].copy_from_slice(&a[..c]);
        let mut x = 7u64;
        for b in &mut img[c..tail_at] {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            *b = (x >> 56) as u8;
        }
        img[tail_at..tail_at + tail.len()].copy_from_slice(tail);
        let src = Mem(img);
        let hits = || MEMO_HITS.with(std::cell::Cell::get);
        let before = hits();
        assert_eq!(join(&mut Reader::new(&src), 0, scan_start, c as u64), None);
        assert!(hits() > before, "no try was stopped by a remembered state");
    }
}
