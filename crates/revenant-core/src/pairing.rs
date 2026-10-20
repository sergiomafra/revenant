//! Puts a camera video back together when its `moov` index ended up away from its `mdat` on a fragmented card: the
//! video's first piece (`ftyp`, the start of `mdat`) was found `Damaged` for want of a `moov`, and an orphan `moov`
//! (found by byte pattern during the scan) holds the rest. Runs after the scan, over the scan's cache.
//!
//! For a video at `s` whose `mdat` would end at `P` (relative to the file), a `moov` box at `q` is its own when the
//! file's layout fits: the second piece starts at `q − P + L1` on a sector boundary, the index's first chunk sits at the
//! start of `mdat` and its data ends before `P`, the samples before the break point `L1` pass a check at `s` and those
//! after it at `q − P`. The check validates each sample's structure (NAL unit lengths and header bytes, or a JPEG's
//! start and end markers), never its payload, so a break point that several sector boundaries pass is ambiguous.

use std::collections::HashSet;

use crate::cache::{CACHE_WINDOW, CachedSource};
use crate::formats::isobmff::{
    Check, MOOV_MAX_LEN, child, index_health, index_tracks, moovless_layout,
};
use crate::formats::{Fragment, Health, Reader, SECTOR};
use crate::scan::{CancelToken, Finding};
use crate::source::Source;

/// Most `moov` bytes loaded per scan, counted in whole cache windows (what a load may read from the source).
pub(crate) const MOOV_LOAD_BUDGET: u64 = 1 << 30;
/// Most samples listed (allocated and sorted) per scan, all `moov`s together, charged from the `stsz` counts before
/// listing (ruling Q11).
pub(crate) const SAMPLE_BUDGET: u64 = 16_000_000;
/// Most (video, `moov`) pairs that pass the arithmetic filter and are checked.
const MAX_PAIRS: usize = 256;
/// Fewest checkable samples an index must list.
const MIN_SAMPLES: usize = 8;
/// The index's first chunk must start within this many bytes of the start of the `mdat` payload.
const FIRST_CHUNK_SLACK: u64 = 64 * 1024;
/// Samples verified on each side of the break.
const VERIFY: usize = 32;
/// Most break points tried per pair.
const MAX_BREAKS: u64 = 4096;
/// Most NAL units walked per sample.
const MAX_NAL_UNITS: usize = 512;
/// Most NAL units (or JPEG marker checks) walked by one pairing run, all pairs together: past it no further pair is
/// checked. Bounds the time a crafted index can cost.
const MAX_STEPS: u64 = 1 << 24;
/// Bytes per load of the sample readers (they read a few bytes per NAL unit).
const READ_WINDOW: usize = 64 * 1024;

/// A `Damaged` video without `moov`.
struct Video<'f> {
    finding: &'f Finding,
    kind: crate::formats::FileKind,
    /// Where `moov` would start, relative to the file: the end of `mdat`.
    p: u64,
    /// Start of the `mdat` payload, relative to the file.
    payload: u64,
}

/// A checkable sample: its offset in the file, size (from `stsz`, 32-bit) and track check. 16 bytes.
#[derive(Clone, Copy)]
struct Item {
    offset: u64,
    size: u32,
    check: Check,
}

impl Item {
    fn len(&self) -> u64 {
        u64::from(self.size)
    }
}

/// A loaded orphan `moov`.
struct Moov {
    /// Absolute offset of the box.
    q: u64,
    /// Box size (header included).
    size: u64,
    payload: std::rc::Rc<Vec<u8>>,
    /// Checkable samples, by offset.
    items: Vec<Item>,
    /// Lowest sample offset of any track.
    c_min: u64,
    /// Highest sample end of any track.
    c_end: u64,
}

/// A pair that passed.
struct Pass {
    video: usize,
    moov: usize,
    finding: Finding,
}

/// The bounds of one pairing run (the constants above; tests lower some of them).
#[derive(Clone, Copy)]
struct Limits {
    load_budget: u64,
    sample_budget: u64,
    max_pairs: usize,
    max_breaks: u64,
    max_steps: u64,
}

const LIMITS: Limits = Limits {
    load_budget: MOOV_LOAD_BUDGET,
    sample_budget: SAMPLE_BUDGET,
    max_pairs: MAX_PAIRS,
    max_breaks: MAX_BREAKS,
    max_steps: MAX_STEPS,
};

/// Work counter shared by the checks of one run.
struct Steps {
    used: u64,
    max: u64,
}

impl Steps {
    /// Counts one step; `false` once the run's budget is spent.
    fn take(&mut self) -> bool {
        self.used += 1;
        self.used <= self.max
    }

    fn spent(&self) -> bool {
        self.used > self.max
    }
}

/// What one run has left of its budgets, and whether a bound was hit.
struct Budget {
    load: u64,
    samples: u64,
    /// No bound was hit: every candidate the filters let through was checked.
    complete: bool,
}

/// Damaged moov-less videos × orphan `moov` boxes (start, declared size, as the scan saw them) → the reassembled findings (same ids, in the order of `videos`),
/// each with 2 pieces. Takes the [`CachedSource`]: `bad_runs` over both pieces is the only complete record of the
/// unreadable sectors the scan met (so a piece's health agrees with the `Unreadable` events).
///
/// Returns nothing once `cancel` is seen (checked before each `moov` and each pair, and while a pair tries its break
/// points). A video with two or more passing `moov`s, or a `moov` passing for two or more videos, pairs with none of
/// them. A reassembled video is `Intact` only when exactly one break point passed and no bound was hit (the load budget,
/// the sample budget, the pair cap or the step budget: an unchecked pair might have passed too); otherwise at most
/// `Partial`.
pub(crate) fn pair_videos(
    source: &CachedSource,
    videos: &[Finding],
    moovs: &[(u64, u64)],
    cancel: &CancelToken,
) -> Vec<Finding> {
    pair_with(source, videos, moovs, cancel, LIMITS).0
}

/// [`pair_videos`] under `limits`; also returns the sample-check steps used.
fn pair_with(
    source: &CachedSource,
    videos: &[Finding],
    moovs: &[(u64, u64)],
    cancel: &CancelToken,
    limits: Limits,
) -> (Vec<Finding>, u64) {
    let mut r = Reader::new(source);
    let vids: Vec<Video> = videos
        .iter()
        .filter_map(|f| {
            let l = moovless_layout(&mut r, f.start)?;
            let rel = l.mdat_at.checked_sub(f.start)?;
            Some(Video {
                finding: f,
                kind: l.kind,
                p: rel.checked_add(l.declared)?,
                payload: rel + l.header_len,
            })
        })
        .collect();
    // `(q − P) ≡ 0 (mod 512)` needs only `q` and each video's `P`: candidates no video can take are never loaded.
    let residues: HashSet<u64> = vids.iter().map(|v| v.p % SECTOR).collect();
    let mut budget = Budget {
        load: limits.load_budget,
        samples: limits.sample_budget,
        complete: true,
    };
    let mut pairs = 0usize;
    let mut steps = Steps {
        used: 0,
        max: limits.max_steps,
    };
    let mut passed: Vec<Pass> = Vec::new();
    'moovs: for (mi, &(q, size)) in moovs.iter().enumerate() {
        if cancel.is_cancelled() {
            return (Vec::new(), steps.used);
        }
        if !residues.contains(&(q % SECTOR)) {
            continue;
        }
        let Some(m) = load_moov(source, q, size, &vids, &mut budget) else {
            continue;
        };
        for (vi, v) in vids.iter().enumerate() {
            if !fits(v, q, m.c_min, m.c_end) {
                continue;
            }
            if pairs == limits.max_pairs || steps.spent() {
                budget.complete = false;
                break 'moovs;
            }
            pairs += 1;
            if cancel.is_cancelled() {
                return (Vec::new(), steps.used);
            }
            if let Some(finding) = try_pair(source, v, &m, &mut steps, limits.max_breaks, cancel) {
                passed.push(Pass {
                    video: vi,
                    moov: mi,
                    finding,
                });
            }
        }
    }
    if cancel.is_cancelled() {
        return (Vec::new(), steps.used);
    }
    let complete = budget.complete && !steps.spent();
    let mut out: Vec<(usize, Finding)> = passed
        .iter()
        .filter(|p| {
            passed.iter().filter(|o| o.video == p.video).count() == 1
                && passed.iter().filter(|o| o.moov == p.moov).count() == 1
        })
        .map(|p| {
            let mut f = p.finding.clone();
            if !complete && f.health == Health::Intact {
                f.health = Health::Partial;
            }
            (p.video, f)
        })
        .collect();
    out.sort_by_key(|(v, _)| *v);
    (out.into_iter().map(|(_, f)| f).collect(), steps.used)
}

/// The arithmetic filter: the second piece starts on a sector boundary, the index's first sample sits within
/// [`FIRST_CHUNK_SLACK`] of the start of the `mdat` payload and its last one ends by `P`.
fn fits(v: &Video, q: u64, c_min: u64, c_end: u64) -> bool {
    q.wrapping_sub(v.p).is_multiple_of(SECTOR)
        && v.payload <= c_min
        && c_min < v.payload + FIRST_CHUNK_SLACK
        && c_end <= v.p
}

/// Loads and parses the `moov` box at `q` of the declared `size`. The cache windows it spans are charged to the load
/// budget first, then (only when some video in `vids` passes the arithmetic filter, judged from the tables without
/// listing a sample) the checkable tracks' sample counts to the sample budget; a candidate past either budget is dropped
/// and clears `complete`. `None` when it is not a usable index: past the source, unreadable, garbled, without `mvhd` or
/// `trak`, with an unusable sample table, with fewer than [`MIN_SAMPLES`] checkable samples, or that no video fits.
fn load_moov(
    source: &CachedSource,
    q: u64,
    size: u64,
    vids: &[Video],
    budget: &mut Budget,
) -> Option<Moov> {
    if !(24..=MOOV_MAX_LEN + 8).contains(&size) || q.checked_add(size)? > source.len() {
        return None;
    }
    let window = CACHE_WINDOW as u64;
    let cost = (q + size).div_ceil(window) * window - q / window * window;
    if cost > budget.load {
        budget.complete = false;
        return None;
    }
    budget.load -= cost;
    let mut r = Reader::new(source);
    let payload = r.load_box(q + 8, size - 8);
    if r.bad_in(q, q + size) || !source.bad_runs(q, q + size).is_empty() {
        return None;
    }
    let mut garbled = false;
    let has_mvhd = child(&payload, b"mvhd", &mut garbled).is_some();
    let has_trak = child(&payload, b"trak", &mut garbled).is_some();
    if garbled || !has_mvhd || !has_trak {
        return None;
    }
    let tracks = index_tracks(&payload)?;
    let mut bounds = (u64::MAX, 0u64);
    for t in &tracks {
        if t.count > 0 {
            let (lo, hi) = t.bounds()?;
            bounds = (bounds.0.min(lo), bounds.1.max(hi));
        }
    }
    let (c_min, c_end) = bounds;
    let checkable: u64 = tracks
        .iter()
        .filter(|t| t.check.is_some())
        .map(|t| t.count as u64)
        .sum();
    if checkable < MIN_SAMPLES as u64 || !vids.iter().any(|v| fits(v, q, c_min, c_end)) {
        return None;
    }
    if checkable > budget.samples {
        budget.complete = false;
        return None;
    }
    budget.samples -= checkable;
    let mut items: Vec<Item> = Vec::with_capacity(checkable as usize);
    for t in &tracks {
        if let Some(check) = t.check {
            items.extend(t.samples().iter().map(|s| Item {
                offset: s.offset,
                size: u32::try_from(s.size).unwrap_or(u32::MAX),
                check,
            }));
        }
    }
    items.sort_unstable_by_key(|i| i.offset);
    drop(tracks);
    Some(Moov {
        q,
        size,
        payload,
        items,
        c_min,
        c_end,
    })
}

/// Where a sample's bytes are read: the first piece's place (`a`, the video's start), the second's (`b = q − P`, may be
/// "negative": then the sample must lie past `P − q`), or split at the break point `l1` (bytes before it at `a`).
struct Places<'s> {
    ra: Reader<'s>,
    rb: Reader<'s>,
    a: u64,
    q: u64,
    p: u64,
}

#[derive(Clone, Copy)]
enum Side {
    A,
    B,
    Split(u64),
}

impl Places<'_> {
    /// Reads `buf.len()` bytes of the file at file offset `off` on `side`; `false` if they are not all in the source.
    fn read(&mut self, side: Side, off: u64, buf: &mut [u8]) -> bool {
        let end = off + buf.len() as u64;
        match side {
            Side::A => self.read_a(off, buf),
            Side::B => self.read_b(off, buf),
            Side::Split(l1) if end <= l1 => self.read_a(off, buf),
            Side::Split(l1) if off >= l1 => self.read_b(off, buf),
            Side::Split(l1) => {
                let (lo, hi) = buf.split_at_mut((l1 - off) as usize);
                self.read_a(off, lo) && self.read_b(l1, hi)
            }
        }
    }

    fn read_a(&mut self, off: u64, buf: &mut [u8]) -> bool {
        self.a
            .checked_add(off)
            .is_some_and(|at| self.ra.read(at, buf) == buf.len())
    }

    fn read_b(&mut self, off: u64, buf: &mut [u8]) -> bool {
        self.q
            .checked_add(off)
            .and_then(|x| x.checked_sub(self.p))
            .is_some_and(|at| self.rb.read(at, buf) == buf.len())
    }

    /// Whether the sample read on `side` passes its check.
    fn passes(&mut self, side: Side, it: &Item, steps: &mut Steps) -> bool {
        match it.check {
            Check::Mjpeg => {
                let (mut soi, mut eoi) = ([0u8; 3], [0u8; 2]);
                steps.take()
                    && it.len() >= 5
                    && self.read(side, it.offset, &mut soi)
                    && self.read(side, it.offset + it.len() - 2, &mut eoi)
                    && soi == [0xFF, 0xD8, 0xFF]
                    && eoi == [0xFF, 0xD9]
            }
            Check::Nal { length_size, hevc } => {
                let ls = u64::from(length_size);
                let need = ls + if hevc { 2 } else { 1 };
                let mut pos = 0u64;
                for _ in 0..MAX_NAL_UNITS {
                    if pos == it.len() {
                        return true;
                    }
                    let mut h = [0u8; 6];
                    let h = &mut h[..need as usize];
                    if !steps.take()
                        || pos + need > it.len()
                        || !self.read(side, it.offset + pos, h)
                    {
                        return false;
                    }
                    let len = h[..ls as usize]
                        .iter()
                        .fold(0u64, |acc, &b| (acc << 8) | u64::from(b));
                    let header = h[ls as usize];
                    let ok = header & 0x80 == 0
                        && if hevc {
                            len >= 2 && (header >> 1) & 0x3F <= 40 && h[ls as usize + 1] & 7 != 0
                        } else {
                            len >= 1 && (1..=23).contains(&(header & 0x1F))
                        };
                    if !ok {
                        return false;
                    }
                    pos += ls + len;
                    if pos > it.len() {
                        return false;
                    }
                }
                pos == it.len()
            }
        }
    }
}

/// Up to [`VERIFY`] evenly spaced indices of `range`, both ends included.
fn spaced(range: std::ops::Range<usize>) -> impl Iterator<Item = usize> {
    let n = range.len();
    let take = n.min(VERIFY);
    (0..take).map(move |j| {
        range.start
            + if take < 2 {
                0
            } else {
                j * (n - 1) / (take - 1)
            }
    })
}

/// Checks video `v` against `moov` `m` (the arithmetic filter already passed) and returns the reassembled finding.
fn try_pair(
    source: &CachedSource,
    v: &Video,
    m: &Moov,
    steps: &mut Steps,
    max_breaks: u64,
    cancel: &CancelToken,
) -> Option<Finding> {
    let s = v.finding.start;
    let mut at = Places {
        ra: Reader::with_window(source, READ_WINDOW),
        rb: Reader::with_window(source, READ_WINDOW),
        a: s,
        q: m.q,
        p: v.p,
    };
    let items = &m.items;
    let n = items.len();
    // k: the first sample that is not at the first piece's place (A holds on a prefix).
    let k = if at.passes(Side::A, &items[n - 1], steps) {
        n
    } else {
        let (mut lo, mut hi) = (0usize, n - 1); // A(hi) fails; the answer is in [lo, hi]
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if at.passes(Side::A, &items[mid], steps) {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    };
    for i in spaced(0..k) {
        if !at.passes(Side::A, &items[i], steps) {
            return None;
        }
    }
    for i in spaced(k + 1..n.max(k + 1)) {
        if !at.passes(Side::B, &items[i], steps) {
            return None;
        }
    }
    // Break points: sector boundaries between the start of sample k − 1 and the start of sample k + 1. Not from the end
    // of sample k − 1: the check reads lengths and header bytes only, so a sample cut inside its last NAL unit's payload
    // still passes at A. Samples k − 1 and k are both read across each break point.
    let lo = match k.checked_sub(1) {
        Some(prev) => v.payload.max(items[prev].offset),
        None => v.payload,
    };
    let hi = items.get(k + 1).map_or(v.p, |next| next.offset.min(v.p));
    let first = lo.div_ceil(SECTOR).checked_mul(SECTOR)?;
    let last = hi / SECTOR * SECTOR;
    if first > last || first == 0 {
        return None;
    }
    let count = (last - first) / SECTOR + 1;
    let mut passing = Vec::new();
    for j in 0..count.min(max_breaks) {
        if cancel.is_cancelled() || steps.spent() {
            return None;
        }
        let l1 = first + j * SECTOR;
        let split = Side::Split(l1);
        let ok = (k.saturating_sub(1)..(k + 1).min(n)).all(|i| at.passes(split, &items[i], steps));
        if ok {
            passing.push(l1);
            if passing.len() == 2 {
                break; // ambiguous: the smallest is kept
            }
        }
    }
    let &l1 = passing.first()?;
    let ambiguous = passing.len() > 1 || count > max_breaks;

    let g = m.q.checked_add(l1)?.checked_sub(v.p)?;
    let second_len = v.p - l1 + m.size;
    let total = source.len();
    let fits = s.checked_add(l1).is_some_and(|e| e <= total)
        && g.checked_add(second_len).is_some_and(|e| e <= total)
        && (s + l1 <= g || g + second_len <= s);
    if !fits {
        return None;
    }
    let pieces = vec![
        Fragment { start: s, len: l1 },
        Fragment {
            start: g,
            len: second_len,
        },
    ];
    let health = match index_health(&m.payload, v.p + m.size, false) {
        Health::Intact
            if ambiguous
                || pieces
                    .iter()
                    .any(|f| !source.bad_runs(f.start, f.start + f.len).is_empty()) =>
        {
            Health::Partial
        }
        h => h,
    };
    Some(Finding {
        id: v.finding.id,
        start: s,
        len: v.p + m.size,
        kind: v.kind,
        health,
        fragments: pieces,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::FileKind;
    use crate::formats::tests::Mem;
    use revenant_testkit::{AvcOptions, AvcVideo, avc_video};

    const MIB: usize = 1 << 20;

    fn video(seed: u64, mdat_pad: usize) -> AvcVideo {
        avc_video(&AvcOptions {
            samples: 120,
            seed,
            mov: false,
            mdat_pad,
            small_nals: true,
        })
    }

    /// Two videos padded so both moovs would sit at the same `P` (each index fits both videos), split at 2/5: first
    /// pieces at 1 and 4 MiB, second pieces at 10 MiB (v2) and 16 MiB (v1). Returns the image, the two Damaged
    /// findings and the moov candidates in offset order (v2's, then v1's).
    fn siblings() -> (Mem, Vec<Finding>, Vec<(u64, u64)>) {
        let bare = |seed| video(seed, 0).moov.start;
        let p = bare(1).max(bare(2)) + 512;
        let (v1, v2) = (video(1, p - bare(1)), video(2, p - bare(2)));
        let mut image = vec![0u8; 24 * MIB];
        let mut x = 7u64;
        for b in image.iter_mut() {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *b = x as u8;
        }
        let mut moovs = Vec::new();
        let mut findings = Vec::new();
        for (id, v, first, second) in [(1, &v1, MIB, 16 * MIB), (2, &v2, 4 * MIB, 10 * MIB)] {
            let pl = &v.mdat_payload;
            let l1 = (pl.start + pl.len() * 2 / 5).div_ceil(512) * 512;
            image[first..first + l1].copy_from_slice(&v.bytes[..l1]);
            image[second..second + v.bytes.len() - l1].copy_from_slice(&v.bytes[l1..]);
            findings.push(Finding {
                id,
                start: first as u64,
                len: p as u64,
                kind: FileKind::Mp4,
                health: Health::Damaged,
                fragments: Vec::new(),
            });
            moovs.push(((second + v.moov.start - l1) as u64, v.moov.len() as u64));
        }
        moovs.sort_unstable();
        (Mem(image), findings, moovs)
    }

    fn run(limits: Limits, moovs: usize) -> (Vec<(u64, Health)>, u64) {
        let (mem, findings, all) = siblings();
        let cache = CachedSource::new(&mem, 512, 64 * MIB);
        let (out, steps) = pair_with(
            &cache,
            &findings,
            &all[..moovs],
            &CancelToken::default(),
            limits,
        );
        (out.iter().map(|f| (f.id, f.health)).collect(), steps)
    }

    #[test]
    fn siblings_pair_intact_within_the_limits() {
        assert_eq!(
            run(LIMITS, 2).0,
            vec![(1, Health::Intact), (2, Health::Intact)]
        );
    }

    #[test]
    fn an_exhausted_sample_budget_makes_a_pairing_partial() {
        // Room for one moov's 120 samples: the second one is never listed, so v2's pairing cannot be shown unique.
        let limits = Limits {
            sample_budget: 120,
            ..LIMITS
        };
        assert_eq!(run(limits, 2).0, vec![(2, Health::Partial)]);
    }

    #[test]
    fn an_exhausted_step_budget_makes_a_pairing_partial() {
        // Exactly the steps the first moov's two pairs need: v2 pairs with it, then the next pair runs out.
        let (first, steps) = run(LIMITS, 1);
        assert_eq!(first, vec![(2, Health::Intact)]);
        let limits = Limits {
            max_steps: steps,
            ..LIMITS
        };
        assert_eq!(run(limits, 2).0, vec![(2, Health::Partial)]);
        // One step short for the very last pair (v2 against v1's moov, which fails anyway): both pairings stand, but
        // a pair was cut short.
        let (all, total) = run(LIMITS, 2);
        assert_eq!(all.len(), 2);
        let limits = Limits {
            max_steps: total - 1,
            ..LIMITS
        };
        assert_eq!(
            run(limits, 2).0,
            vec![(1, Health::Partial), (2, Health::Partial)]
        );
    }

    #[test]
    fn spaced_takes_both_ends_and_at_most_verify() {
        assert_eq!(spaced(3..3).count(), 0);
        assert_eq!(spaced(5..6).collect::<Vec<_>>(), vec![5]);
        assert_eq!(spaced(0..4).collect::<Vec<_>>(), vec![0, 1, 2, 3]);
        let many: Vec<usize> = spaced(10..1010).collect();
        assert_eq!(many.len(), VERIFY);
        assert_eq!((many[0], many[VERIFY - 1]), (10, 1009));
        assert!(many.windows(2).all(|w| w[0] < w[1]));
    }
}
