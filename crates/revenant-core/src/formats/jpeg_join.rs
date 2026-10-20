//! Bifragment gap carving for baseline JPEGs: a photo whose entropy-coded data breaks (an impossible code, a foreign
//! file's head, erased space) may continue further on the disk, after a gap holding other data. The search tries every
//! sector in a bounded window after the break where the photo's data could continue, and keeps a continuation only if
//! our entropy decoder walks the two pieces to the exact MCU count with `EOI` right after it, and the DC values of the
//! blocks on both sides of the seam agree as well as the rest of the image does.
//!
//! Bounds: the window is [`JPEG_GAP_MAX`] bytes (one record per sector), at most [`MAX_JOIN_TRIES`] joins are decoded
//! and at most [`JOIN_BYTES_MAX`] bytes of continuations in total. A try whose MCU row after the seam fails the seam test
//! stops there. The search looks at the scan's cancel flag every MiB of the window and before every try.

use super::jpeg::{JPEG_MAX_LEN, data_end};
use super::jpeg_entropy::{self, Cursor, End, Frame};
use super::{Fragment, Reader, SECTOR, builtin, cancelled};
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
/// Bytes of continuations decoded at most, over every try (same meaning as [`MAX_JOIN_TRIES`] when hit).
const JOIN_BYTES_MAX: u64 = 256 << 20;
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
    /// Median of the block-row boundary means above the seam (for the early seam test); `None` when there are none.
    base_above: Option<f64>,
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
    // points lie right before it.
    let mut x = match run0.end {
        End::Error { at } => at.min(cut),
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
        .filter(|&c| c > scan_start && c + BREAK_BACK * SECTOR > x)
        .filter_map(|c1| {
            let cur = run0.resume.iter().rev().find(|c| c.pos <= c1)?.clone();
            let seam_row_top =
                cur.mcu / u64::from(frame.mcus_x) * u64::from(frame.luma_blocks_per_mcu().1);
            let base_above = median(
                boundaries
                    .iter()
                    .filter(|&&(b, _)| b < seam_row_top)
                    .map(|&(_, m)| m),
            );
            Some(Break {
                c1,
                cur,
                base_above,
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
    };

    let candidates = window(r, lowest)?;

    let mut tries = 0usize;
    let mut bytes = 0u64;
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
            if tries >= max_tries() || bytes >= JOIN_BYTES_MAX {
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
            if try_join(&source, &frame, &run0.dc[..], bpm, b, pieces, &mut bytes) {
                passes += 1;
                if first.is_some() {
                    break 'search; // a second pass: the result is at most `Partial` whatever else passes
                }
                first = Some((b.c1, g, e));
            }
        }
    }
    let (c1, g, e) = first?;
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
        unique: passes == 1 && exhausted,
    })
}

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
fn window(r: &mut Reader, lowest: u64) -> Option<Vec<(u64, u64)>> {
    let first = lowest + SECTOR;
    let end = lowest.saturating_add(JPEG_GAP_MAX).min(r.len);
    if first >= end {
        return Some(Vec::new());
    }
    let sectors = (end - first).div_ceil(SECTOR) as usize;
    let mut rec = vec![NONE; sectors];
    let formats = builtin();
    // One forward sweep, so the reader loads each window once.
    for (i, rec) in rec.iter_mut().enumerate() {
        if (i as u64).is_multiple_of(SECTORS_PER_CANCEL_CHECK) && cancelled() {
            return None;
        }
        let s = first + i as u64 * SECTOR;
        let next = s + SECTOR;
        // Events at the sector's start come first: another file's head, or an erased or unreadable sector (`r` reads
        // the disk itself, so the sector alignment is absolute).
        if r.file_starts_at(formats, s) || data_end(r, s, next.min(end)) == s {
            *rec = BAD;
            continue;
        }
        let mut p = s;
        while let Some(f) = r.find_ff(p, next) {
            match r.byte(f + 1) {
                None => break,
                Some(0x00 | 0xD0..=0xD7 | 0xFF) => p = f + 1,
                Some(0xD9) => {
                    *rec = f;
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
    let (mut last, mut taken) = (NONE, 0);
    for (i, &e) in rec.iter().enumerate() {
        if e == NONE || e == BAD {
            continue;
        }
        if e != last {
            (last, taken) = (e, 0);
        }
        if taken < PER_EOI {
            candidates.push((first + i as u64 * SECTOR, e));
            taken += 1;
        }
    }
    Some(candidates)
}

/// Decodes the photo through `pieces` from the break `b`: passes when every MCU decodes, `EOI` follows right at the end
/// of the second piece, and the DC seam test holds. Gives up after the MCU row after the seam if the seam already fails
/// the test against the rows above it. Adds the continuation bytes decoded to `bytes`.
fn try_join(
    source: &dyn Source,
    frame: &Frame,
    dc0: &[i32],
    bpm: u64,
    b: &Break,
    pieces: [Fragment; 2],
    bytes: &mut u64,
) -> bool {
    let start = pieces[0].start;
    let view = Fragmented::new(source, pieces.to_vec());
    let len = view.len();
    let mut vr = Reader::new(&view);
    let from = b.cur.shifted_back(start);
    let seam = b.cur.mcu;
    let head = &dc0[..(seam * bpm) as usize];
    let row = u64::from(frame.mcus_x);
    let stop = (seam / row + 2) * row;
    let early = jpeg_entropy::decode_until(&mut vr, frame, from.clone(), len, true, stop);
    *bytes += early.pos - from.pos;
    let run = match early.end {
        End::Eoi { .. } => early,
        End::Error { .. } => return false,
        End::OutOfData if early.mcus < stop => return false,
        End::OutOfData => {
            let dc = Dc {
                head,
                tail: &early.dc,
            };
            if let (Some(base), Some(at_seam)) = (b.base_above, seam_mean(frame, dc, seam))
                && !within(at_seam, base)
            {
                return false;
            }
            let run = jpeg_entropy::decode(&mut vr, frame, from.clone(), len, true);
            *bytes += run.pos - from.pos;
            run
        }
    };
    if run.end != (End::Eoi { at: len - 2 }) {
        return false;
    }
    let whole: Vec<i32> = head.iter().chain(&run.dc).copied().collect();
    seam_ok(frame, &whole, seam)
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

/// `inner`, with the bytes `[at, at + bytes.len())` served from memory (reads wholly inside them).
struct Overlay<'a> {
    inner: &'a dyn Source,
    at: u64,
    bytes: Vec<u8>,
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
        self.inner.read_at(offset, buf)
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
        let a = testkit::jpeg_smooth(256, 192, 1);
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
}
