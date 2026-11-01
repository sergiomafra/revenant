//! The seams of an MP4/MOV video whose `moov` index lies away from its `mdat` (part 1.6, D12): the index places every
//! sample in the file, so each piece's content is checked where the file needs it.
//!
//! The file lives in "file offsets": logical 0 is the `ftyp` at the video's start, the samples sit at the offsets the
//! orphan `moov` lists, and the `moov` itself goes at `P`, the end of `mdat`, where it must be the box found at `q` (the
//! anchor). Each checkable sample (H.264/HEVC NAL units, Motion-JPEG frames) is walked by the pairing's structural check
//! ([`crate::formats::isobmff::check_sample`]), through the view of the pieces chosen so far. Only the bytes the check reads are
//! evidence (the NAL lengths and header bytes, a JPEG's SOI and EOI); the payloads are not, so a seam is proven only when
//! checked bytes lie within one unit of it on both sides: a cut one unit earlier or later would then read a checked byte
//! from another place.

use std::rc::Rc;

use super::{
    Budget, Checks, Proof, Seams, Search, Space, Taken, WalkEnd, Walked, health, improves, search,
};
use crate::cache::CachedSource;
use crate::filesystem::{FsState, Placement};
use crate::formats::isobmff::{MAX_NAL_UNITS, SampleEnd, check_sample, index_health, walk_sample};
use crate::formats::{FileKind, Health, Reader, SECTOR, cancelled};
use crate::pairing::{self, Moov, Video, fits, load_moov};
use crate::scan::{CancelToken, Finding};
use crate::source::Source;

/// Sample-check steps (one per NAL unit or JPEG) per search (one orphan `moov` for one video), all its walks and its
/// proof together: past it a walk cannot decide (the search is then not exhaustive) and the proof is `Unproven`. The
/// pairing's per-scan budget.
pub(crate) const SEARCH_STEPS: u64 = 1 << 24;
/// Bytes per load of a walk's reader: one sector (it reads a few bytes per NAL unit, and every byte asked is charged to
/// the file's read budget).
const WALK_WINDOW: usize = SECTOR as usize;
/// Samples walked between two looks at the cancel flag.
const CANCEL_EVERY: usize = 1024;
/// Most suspicious positions given per piece (the engine tries at most its break-point cap).
const HINTS: usize = 64;

/// A moov-less video's samples, as its orphan `moov` lists them, walked across seams.
pub(crate) struct VideoSeams {
    moov: Rc<Moov>,
    /// Where the `moov` goes, relative to the file (the end of `mdat`).
    p: u64,
    /// The `mdat` box header, relative to the file: checked where the video was found.
    mdat: (u64, u64),
    /// The space's unit (cluster or sector): a seam is proven only with checked bytes within one unit on both sides.
    unit: u64,
    /// Sample-check steps left.
    steps: u64,
    /// Every checkable sample has the same size: another clip of the same format could pass every check in place.
    uniform: bool,
    /// The gaps between checked bytes of the last view the re-check asked about: (view length, gaps by start).
    gaps: Option<(u64, Vec<(u64, u64)>)>,
}

impl VideoSeams {
    /// A moov-less video at `start` (its `MdatLayout`) with the orphan `moov` at `q`: known length P + moov size, anchor
    /// (P, q), samples from the checkable tracks (≤ the pairing's sample budget). `None` when the video has a `moov`
    /// or is no video, or the `moov` is not usable for it (the pairing's load and arithmetic filter).
    pub(crate) fn with_orphan(
        source: &dyn Source,
        start: u64,
        q: u64,
        size: u64,
    ) -> Option<VideoSeams> {
        let cache = CachedSource::new(source, SECTOR, 1 << 20);
        let f = Finding {
            id: 0,
            start,
            len: 0,
            kind: FileKind::Mp4,
            health: Health::Damaged,
            fragments: Vec::new(),
            origin: None,
        };
        let v = Video::of(&mut Reader::new(&cache), &f)?;
        let m = load_moov(
            &cache,
            q,
            size,
            std::slice::from_ref(&v),
            &mut pairing::Budget::new(),
        )?;
        fits(&v, q, m.c_min, m.c_end).then(|| VideoSeams::new(&v, Rc::new(m)))
    }

    /// Video `v` with the loaded `moov` `m` (the caller checked [`fits`]); sector units until [`VideoSeams::with_unit`].
    pub(crate) fn new(v: &Video, m: Rc<Moov>) -> VideoSeams {
        let uniform = m.items.len() >= 2 && m.items.iter().all(|it| it.size == m.items[0].size);
        VideoSeams {
            uniform,
            moov: m,
            p: v.p,
            mdat: (v.mdat, v.payload),
            unit: SECTOR,
            steps: SEARCH_STEPS,
            gaps: None,
        }
    }

    /// The space's unit (a seam must have checked bytes within it on both sides to be proven).
    pub(crate) fn with_unit(mut self, unit: u64) -> VideoSeams {
        self.unit = unit.max(1);
        self
    }

    fn len(&self) -> u64 {
        self.p + self.moov.size
    }

    /// Whether the view holds, at `P`, the bytes of the `moov` box found at `q`; `None` when they cannot be read.
    fn moov_at_p(&self, r: &mut Reader) -> Option<bool> {
        let size = self.moov.size;
        let mut head = [0u8; 8];
        if r.read(self.p, &mut head) != 8 || r.bad_in(self.p, self.p + 8) {
            return None;
        }
        let want = u32::try_from(size).ok()?.to_be_bytes();
        if head[..4] != want || &head[4..] != b"moov" {
            return Some(false);
        }
        let payload = &self.moov.payload;
        let mut buf = vec![0u8; 64 << 10];
        let mut done = 0usize;
        while done < payload.len() {
            let n = buf.len().min(payload.len() - done);
            let at = self.p + 8 + done as u64;
            if r.read(at, &mut buf[..n]) != n || r.bad_in(at, at + n as u64) {
                return None;
            }
            if buf[..n] != payload[done..done + n] {
                return Some(false);
            }
            done += n;
        }
        Some(true)
    }

    /// Walks every sample of a view of `len` bytes in file order, handing `checked` each range of checked bytes; the
    /// first sample that does not pass ends it (a cancel ends it as `Spent`).
    fn walk_all(
        &mut self,
        r: &mut Reader,
        len: u64,
        checked: &mut dyn FnMut(u64, u64),
    ) -> Result<(), SampleEnd> {
        let moov = Rc::clone(&self.moov);
        for (i, it) in moov.items.iter().enumerate() {
            if i % CANCEL_EVERY == CANCEL_EVERY - 1 && cancelled() {
                return Err(SampleEnd::Spent);
            }
            let got = walk_sample(
                &mut |off, buf| read_in(r, off, buf, len),
                &it.sample(),
                &mut self.steps,
                checked,
            );
            if got != SampleEnd::Pass {
                return Err(got);
            }
        }
        Ok(())
    }
}

/// Reads `buf.len()` bytes at `off` when they lie below `limit`, are all there and none is a known-bad sector.
fn read_in(r: &mut Reader, off: u64, buf: &mut [u8], limit: u64) -> bool {
    let end = off.saturating_add(buf.len() as u64);
    end <= limit && r.read(off, buf) == buf.len() && !r.bad_in(off, end)
}

impl Seams for VideoSeams {
    /// The index of the next sample to check (in file order).
    type State = usize;

    fn begin(&mut self) -> usize {
        0
    }

    /// The samples from `from` in file order, each one wholly inside the view checked: `checked` is the end of the
    /// last checked byte (a resume point after each passing sample), `Broken` at the first failing one (where the bytes
    /// it read stop fitting), or where the view ends; `Complete` when every sample passes and the view reaches the known
    /// length with the `moov` bytes at `P` (the engine checks that `P` lies at the anchor).
    fn walk(
        &mut self,
        view: &dyn Source,
        seams: &[u64],
        from: &usize,
        limit: u64,
    ) -> Walked<usize> {
        let _ = seams;
        let limit = limit.min(view.len());
        let mut r = Reader::with_window(view, WALK_WINDOW);
        let moov = Rc::clone(&self.moov);
        let mut checked = 0u64;
        let mut resume = Vec::new();
        let mut i = *from;
        let end = loop {
            let Some(it) = moov.items.get(i) else {
                if limit < self.len() {
                    break WalkEnd::Broken { at: limit };
                }
                break match self.moov_at_p(&mut r) {
                    Some(true) => WalkEnd::Complete { len: self.len() },
                    Some(false) => WalkEnd::Broken { at: self.len() },
                    None => WalkEnd::Broken { at: self.p },
                };
            };
            if (i - *from) % CANCEL_EVERY == CANCEL_EVERY - 1 && cancelled() {
                break WalkEnd::Stopped;
            }
            let mut last = None;
            let s = it.sample();
            let got = walk_sample(
                &mut |off, buf| read_in(&mut r, off, buf, limit),
                &s,
                &mut self.steps,
                &mut |_, to| last = Some(to),
            );
            if let Some(to) = last {
                checked = checked.max(to);
            }
            match got {
                SampleEnd::Pass => {
                    let prev = resume.last().map_or(0, |r: &(u64, usize)| r.0);
                    resume.push((last.unwrap_or(s.offset).max(prev), i + 1));
                    i += 1;
                }
                SampleEnd::Fail { at } => break WalkEnd::Broken { at },
                SampleEnd::Unread { at } => break WalkEnd::Broken { at: at.min(limit) },
                SampleEnd::Spent => break WalkEnd::Stopped,
            }
        };
        Walked {
            end,
            checked,
            resume,
        }
    }

    /// `Proven` iff the `mdat` header lies before every seam, every checkable sample passes over the view, the `moov`
    /// bytes sit at `P`, and every seam has checked bytes within one unit before it and within one unit after it (a cut a
    /// unit earlier or later would read one of them from elsewhere; the `mdat` header and the `moov` count as checked);
    /// `Failed` when a sample does not pass or the `moov` is not at `P`; `Unproven` otherwise (a seam in a long
    /// unchecked stretch, the steps spent, an unreadable byte), and always with a seam when every checkable sample has
    /// the same size (the checks then tell nothing about which clip a run of the same format comes from).
    fn prove(&mut self, view: &dyn Source, seams: &[u64]) -> Proof {
        if seams.first().is_some_and(|&s| s < self.mdat.1) {
            return Proof::Unproven;
        }
        let unit = self.unit;
        let mut near = Near::new(seams, unit);
        near.range(self.mdat.0, self.mdat.1);
        let mut r = Reader::with_window(view, WALK_WINDOW);
        match self.walk_all(&mut r, view.len(), &mut |a, b| near.range(a, b)) {
            Ok(()) => {}
            Err(SampleEnd::Fail { .. }) => return Proof::Failed,
            Err(_) => return Proof::Unproven,
        }
        match self.moov_at_p(&mut r) {
            Some(true) => {}
            Some(false) => return Proof::Failed,
            None => return Proof::Unproven,
        }
        near.range(self.p, self.len());
        if near.all_ok() && !(self.uniform && !seams.is_empty()) {
            Proof::Proven
        } else {
            Proof::Unproven
        }
    }

    fn known_len(&self) -> Option<u64> {
        Some(self.len())
    }

    fn anchor(&self) -> Option<(u64, u64)> {
        Some((self.p, self.moov.q))
    }

    /// The first sample that starts at or past the seam, read at its place in a piece starting at absolute `g`, passes
    /// its check, as far as it lies within the candidate's first unit (bytes past it may be in another piece: they
    /// are not read, and what was read decides). A seam with no sample starting within one unit after it is blind:
    /// kept (Q2). An unreadable byte drops the candidate.
    fn prefilter(&mut self, r: &mut Reader, g: u64, seam: u64) -> bool {
        let items = &self.moov.items;
        let i = items.partition_point(|it| it.offset < seam);
        let Some(it) = items.get(i) else {
            return true;
        };
        let stop = seam.saturating_add(self.unit);
        if it.offset >= stop {
            return true;
        }
        let mut beyond = false;
        // One step per NAL unit, as many as a sample may hold: never spent.
        let mut steps = MAX_NAL_UNITS as u64;
        let passed = check_sample(
            &mut |off, buf| {
                let end = off + buf.len() as u64;
                if end > stop {
                    beyond = true;
                    return false;
                }
                let at = g + (off - seam);
                r.read(at, buf) == buf.len() && !r.bad_in(at, at + buf.len() as u64)
            },
            &it.sample(),
            &mut steps,
        );
        // Stopped at the first byte past the unit: what was read fitted.
        passed || beyond
    }

    /// Unit boundaries inside `(start, end)` that fall in a stretch the checks never read (a NAL unit's payload, a
    /// Motion-JPEG frame's body, an uncheckable track, the `mdat` padding) with no checked byte within one unit on some
    /// side, longest stretch first: a cut there is the least constrained (a seam there would not be proven), as another
    /// file's data could stand in it unnoticed. Elsewhere a cut is refuted by the checked bytes around it, so none is
    /// given (the engine's default order then applies). The checked ranges of the whole view are
    /// walked once (the re-check asks for its pieces in a row).
    fn suspicious_breaks(&mut self, view: &dyn Source, start: u64, end: u64) -> Vec<u64> {
        let len = view.len();
        if self.gaps.as_ref().is_none_or(|g| g.0 != len) {
            let mut gaps = Vec::new();
            let mut prev = self.mdat.1;
            let mut r = Reader::with_window(view, WALK_WINDOW);
            let ok = self
                .walk_all(&mut r, len, &mut |a, b| {
                    if a > prev {
                        gaps.push((prev, a));
                    }
                    prev = prev.max(b);
                })
                .is_ok();
            if ok && self.p > prev {
                gaps.push((prev, self.p));
            }
            self.gaps = Some((len, if ok { gaps } else { Vec::new() }));
        }
        let Some((_, gaps)) = &self.gaps else {
            return Vec::new();
        };
        let unit = self.unit;
        let mut inside: Vec<(u64, u64)> = gaps
            .iter()
            .filter_map(|&(a, b)| {
                // The last unit boundary inside the stretch (logical boundaries are multiples of the unit: every piece
                // starts on one and all but the last are whole units).
                let x = (b.min(end) - 1) / unit * unit;
                // Only where a seam would not be proven: no checked byte within one unit on some side.
                let weak = a + unit <= x || b >= x + unit;
                (x > a && x > start && x < end && weak).then_some((x, b - a))
            })
            .collect();
        inside.sort_by(|a, b| b.1.cmp(&a.1).then(b.0.cmp(&a.0)));
        inside.truncate(HINTS);
        inside.into_iter().map(|(x, _)| x).collect()
    }
}

/// For each seam, whether checked bytes lie within one unit before it and within one unit after it, fed with the
/// checked ranges in file order.
struct Near<'s> {
    seams: &'s [u64],
    unit: u64,
    /// Seams decided so far (the next one to look at).
    next: usize,
    /// End of the checked bytes so far.
    prev: Option<u64>,
    ok: bool,
}

impl<'s> Near<'s> {
    fn new(seams: &'s [u64], unit: u64) -> Self {
        Near {
            seams,
            unit,
            next: 0,
            prev: None,
            ok: true,
        }
    }

    /// The checked range `[a, b)`, after every earlier one.
    fn range(&mut self, a: u64, b: u64) {
        if a >= b {
            return;
        }
        while let Some(&s) = self.seams.get(self.next) {
            if s >= b {
                break;
            }
            if a >= s {
                // Checked bytes from `a` on after the seam, and the last one before it at `prev - 1`.
                let after = a - s < self.unit;
                let before = self.prev.is_some_and(|p| p + self.unit > s);
                self.ok &= after && before;
            }
            self.next += 1;
        }
        self.prev = Some(self.prev.map_or(b, |p| p.max(b)));
    }

    /// Every seam decided and near checked bytes on both sides.
    fn all_ok(&self) -> bool {
        self.ok && self.next == self.seams.len()
    }
}

/// The reassembly step's videos: the orphan `moov`s, loaded when a video first needs one (with the pairing's
/// budgets, its arithmetic filter judged against every candidate video), kept for the next ones.
pub(crate) struct Moovs<'f> {
    videos: Vec<Video<'f>>,
    loaded: Vec<Option<Option<Rc<Moov>>>>,
    budget: pairing::Budget,
}

impl<'f> Moovs<'f> {
    /// For `candidates` (the moov-less videos among them) and `n` orphan `moov`s, loaded within `budget` (what the
    /// pairing left).
    pub(crate) fn new(
        source: &CachedSource,
        candidates: &'f [Finding],
        n: usize,
        budget: pairing::Budget,
    ) -> Moovs<'f> {
        let mut r = Reader::new(source);
        let videos = candidates
            .iter()
            .filter(|f| matches!(f.kind, FileKind::Mp4 | FileKind::Mov))
            .filter_map(|f| Video::of(&mut r, f))
            .collect();
        Moovs {
            videos,
            loaded: vec![None; n],
            budget,
        }
    }

    /// The `moov` `i` of `moovs`, loaded once.
    fn get(&mut self, source: &CachedSource, moovs: &[(u64, u64)], i: usize) -> Option<Rc<Moov>> {
        if self.loaded[i].is_none() {
            let (q, size) = moovs[i];
            let m = load_moov(source, q, size, &self.videos, &mut self.budget).map(Rc::new);
            self.loaded[i] = Some(m);
        }
        self.loaded[i].clone().flatten()
    }
}

/// What the search of one video gave.
pub(crate) struct Searched {
    /// The finding it becomes, when that improves it.
    pub new: Option<Finding>,
    /// The orphan `moov`s (indices in `Context::moovs`) that gave it a complete solution.
    pub moovs: Vec<usize>,
}

/// The moov-less video `f` searched with every orphan `moov` that fits it (each one a search, all within the file's
/// budget); `old` is what it is so far (the pairing's result, or `f`). Only a complete solution (the `moov` in place)
/// can improve it; two or more `moov`s giving one make it not unique.
#[allow(clippy::too_many_arguments)]
pub(crate) fn reassemble_video(
    source: &CachedSource,
    f: &Finding,
    old: &Finding,
    taken: &Taken,
    ctx: &super::Context,
    moovs: &mut Moovs,
    budget: &mut Budget,
    cancel: &CancelToken,
) -> Searched {
    let mut out = Searched {
        new: None,
        moovs: Vec::new(),
    };
    let Some(v) = Video::of(&mut Reader::new(source), f) else {
        return out;
    };
    let space = Space::new(
        ctx.grids,
        f.start,
        source.len(),
        taken,
        ctx.starts,
        ctx.limits,
    );
    let known = f
        .origin
        .as_ref()
        .filter(|o| o.state == FsState::Deleted)
        .map(|o| o.size);
    let limits = ctx.limits;
    // One file's budget for all its searches.
    let mut file = Budget {
        tries: limits.file_tries.min(budget.tries),
        read: limits.file_read.min(budget.read),
    };
    let (tries0, read0) = (file.tries, file.read);
    let mut complete: Vec<(Search, Rc<Moov>)> = Vec::new();
    let mut short = false;
    for (i, &(q, _)) in ctx.moovs.iter().enumerate() {
        if cancel.is_cancelled() {
            break;
        }
        if !q.wrapping_sub(v.p).is_multiple_of(SECTOR) {
            continue;
        }
        let Some(m) = moovs.get(source, ctx.moovs, i) else {
            continue;
        };
        if !fits(&v, q, m.c_min, m.c_end) {
            continue;
        }
        if file.spent() {
            short = true;
            break;
        }
        let mut seams = VideoSeams::new(&v, Rc::clone(&m)).with_unit(space.unit());
        let found = search(
            &mut seams, source, &space, f.start, known, limits, &mut file, cancel,
        );
        if let Some(found) =
            found.filter(|s| s.complete && s.pieces.first().map(|p| p.start) == Some(f.start))
        {
            if !found.exhaustive {
                short = true;
            }
            out.moovs.push(i);
            complete.push((found, m));
        } else if file.spent() {
            short = true;
        }
    }
    budget.charge(tries0 - file.tries, read0 - file.read);
    if cancel.is_cancelled() {
        return out;
    }
    let several = complete.len() > 1;
    let Some((mut found, m)) = complete.into_iter().next() else {
        return out;
    };
    found.unique &= !several;
    found.exhaustive &= !short && moovs.budget.complete;
    let len = found.len;
    let checks = Checks {
        capped: ctx.capped,
        frags_capped: ctx.frags_capped,
        claimed_capped: ctx.claimed_capped,
        listing_truncated: ctx.listing_truncated,
        bad: found
            .pieces
            .iter()
            .any(|p| !source.bad_runs(p.start, p.start + p.len).is_empty()),
        size_ok: known.is_none_or(|k| k == len),
        own_ok: index_health(&m.payload, len, false) == Health::Intact,
        restarts: false,
        content_only: true,
    };
    let new = Finding {
        id: f.id,
        start: f.start,
        len,
        kind: v.kind,
        health: health(&found, v.kind, &checks),
        fragments: if found.pieces.len() > 1 {
            found.pieces.clone()
        } else {
            Vec::new()
        },
        origin: f.origin.clone().map(|mut o| {
            o.placement = Placement::Carved;
            o
        }),
    };
    out.new = improves(old, &new, found.complete).then_some(new);
    out
}

#[cfg(test)]
mod tests {
    use super::super::{Grid, Limits};
    use super::*;
    use crate::cache::Runs;
    use crate::formats::Fragment;
    use crate::formats::tests::Mem;
    use crate::source::Fragmented;
    use revenant_testkit::{AvcOptions, AvcVideo, avc_video};

    const MIB: u64 = 1 << 20;
    const HEAP: u64 = MIB;

    fn video(samples: usize, seed: u64, small_nals: bool) -> AvcVideo {
        avc_video(&AvcOptions {
            samples,
            seed,
            mov: false,
            mdat_pad: 0,
            small_nals,
        })
    }

    /// A card of seeded noise after a 1 MiB heap start, `clusters` clusters of `cluster` bytes.
    fn noise_card(clusters: u64, cluster: u64) -> Vec<u8> {
        let mut x = 0x5EEDu64;
        (0..HEAP + clusters * cluster)
            .map(|_| {
                x = x
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (x >> 56) as u8
            })
            .collect()
    }

    /// `v` cut into runs of `counts` clusters (the last one the rest) placed at clusters `at` of `card`; the runs.
    fn scatter(
        card: &mut [u8],
        v: &[u8],
        cluster: u64,
        at: &[u64],
        counts: &[u64],
    ) -> Vec<Fragment> {
        let mut from = 0u64;
        at.iter()
            .zip(counts)
            .enumerate()
            .map(|(k, (&c, &n))| {
                let to = if k + 1 == at.len() {
                    v.len() as u64
                } else {
                    from + n * cluster
                };
                let start = HEAP + c * cluster;
                card[start as usize..(start + to - from) as usize]
                    .copy_from_slice(&v[from as usize..to as usize]);
                let f = Fragment {
                    start,
                    len: to - from,
                };
                from = to;
                f
            })
            .collect()
    }

    fn find(
        src: &Mem,
        seams: &mut VideoSeams,
        grid: Grid,
        start: u64,
        limits: &Limits,
    ) -> Option<Search> {
        let taken = Taken::new(&[], &Runs::default());
        let starts = [start];
        let space = Space::new(&[grid], start, src.len(), &taken, &starts, limits);
        let mut budget = Budget::new(limits);
        search(
            seams,
            src,
            &space,
            start,
            None,
            limits,
            &mut budget,
            &CancelToken::default(),
        )
    }

    /// `v` on a 32 KiB-cluster noise card in 3 runs; the image, the runs and the seams of `v` at its orphan `moov`.
    fn three_runs(v: &AvcVideo) -> (Mem, Vec<Fragment>, VideoSeams) {
        const C: u64 = 32 << 10;
        let n = (v.bytes.len() as u64).div_ceil(C);
        let mut card = noise_card(400, C);
        let runs = scatter(&mut card, &v.bytes, C, &[300, 20, 150], &[n / 3, n / 3, 0]);
        let last = runs[2];
        let q = last.start + (v.moov.start as u64 - (v.bytes.len() as u64 - last.len));
        let src = Mem(card);
        let seams = VideoSeams::with_orphan(&src, runs[0].start, q, v.moov.len() as u64)
            .expect("the moov fits")
            .with_unit(C);
        (src, runs, seams)
    }

    #[test]
    fn video_in_three_pieces_on_a_grid_is_proven_and_unique() {
        let v = video(300, 7, true);
        let (src, runs, mut seams) = three_runs(&v);
        let found = find(
            &src,
            &mut seams,
            (HEAP, 32 << 10, src.len()),
            runs[0].start,
            &Limits::DEFAULT,
        )
        .expect("a result");
        assert_eq!(found.pieces, runs);
        assert!(
            found.complete && found.unique && found.exhaustive,
            "{found:?}"
        );
        assert_eq!(found.proof, Proof::Proven);
        let view = Fragmented::new(&src, found.pieces.clone());
        let mut bytes = vec![0u8; v.bytes.len()];
        view.read_at(0, &mut bytes).unwrap();
        assert_eq!(bytes, v.bytes);
    }

    #[test]
    fn a_moov_of_another_video_is_no_solution() {
        // The same layout, the index of a look-alike clip (same options, another seed, padded to the same length) at
        // the place of the video's own: its samples refute the video's data at once.
        let bare = |seed| video(300, seed, true).moov.start;
        let p = bare(7).max(bare(8)) + 512;
        let padded = |seed| {
            avc_video(&AvcOptions {
                samples: 300,
                seed,
                mov: false,
                mdat_pad: p - bare(seed),
                small_nals: true,
            })
        };
        let (v, other) = (padded(7), padded(8));
        assert_eq!(other.moov.start, v.moov.start);
        let (Mem(mut card), runs, _) = three_runs(&v);
        let last = runs[2];
        let q = last.start + (v.moov.start as u64 - (v.bytes.len() as u64 - last.len));
        card[q as usize..q as usize + other.moov.len()]
            .copy_from_slice(&other.bytes[other.moov.clone()]);
        let src = Mem(card);
        let mut seams = VideoSeams::with_orphan(&src, runs[0].start, q, other.moov.len() as u64)
            .expect("the arithmetic fits")
            .with_unit(32 << 10);
        let found = find(
            &src,
            &mut seams,
            (HEAP, 32 << 10, src.len()),
            runs[0].start,
            &Limits::DEFAULT,
        );
        assert!(found.as_ref().is_none_or(|f| !f.complete), "{found:?}");
    }

    /// The checked ranges of `v` (contiguous), as the proof sees them.
    fn checked_ranges(v: &AvcVideo, seams: &mut VideoSeams) -> Vec<(u64, u64)> {
        let src = Mem(v.bytes.clone());
        let mut r = Reader::new(&src);
        let mut out = Vec::new();
        seams
            .walk_all(&mut r, src.len(), &mut |a, b| out.push((a, b)))
            .unwrap();
        out
    }

    /// `v` up to its `moov` at 0, noise after it, its `moov` 64 KiB further: the image, the seams at that `moov` and the
    /// view of the whole video (a seam at `P`).
    fn moov_apart(v: &AvcVideo, unit: u64) -> (Mem, VideoSeams, Vec<Fragment>) {
        let p = v.moov.start;
        let q = p + (64 << 10);
        let mut card = v.bytes[..p].to_vec();
        card.resize(q, 0x5A);
        card.extend_from_slice(&v.bytes[v.moov.clone()]);
        card.resize(card.len() + 4096, 0x5A);
        let src = Mem(card);
        let seams = VideoSeams::with_orphan(&src, 0, q as u64, v.moov.len() as u64)
            .expect("a moov-less video and its moov")
            .with_unit(unit);
        let pieces = vec![
            Fragment {
                start: 0,
                len: p as u64,
            },
            Fragment {
                start: q as u64,
                len: v.moov.len() as u64,
            },
        ];
        (src, seams, pieces)
    }

    #[test]
    fn a_seam_far_from_checked_bytes_is_unproven() {
        // Large NAL units (2–24 KiB): on 4 KiB units, a seam in the middle of a long payload has no checked byte within
        // a unit of it; one right after a NAL header has.
        let v = video(40, 5, false);
        let (src, mut seams, pieces) = moov_apart(&v, 4096);
        let ranges = checked_ranges(&v, &mut seams);
        let (a, b) = ranges
            .windows(2)
            .map(|w| (w[0].1, w[1].0))
            .find(|&(a, b)| b - a > 3 * 4096)
            .expect("a long payload");
        // The view's own seam at `P` (after a long last payload) is left out of these.
        let whole = Fragmented::new(&src, pieces);
        let far = (a + b) / 2;
        assert_eq!(seams.prove(&whole, &[far]), Proof::Unproven);
        // Right after a header, before a long payload: a cut one unit later would read no checked byte elsewhere.
        assert_eq!(seams.prove(&whole, &[a + 100]), Proof::Unproven);
        assert_eq!(seams.prove(&whole, &[b - 100]), Proof::Unproven);
        // Between two headers less than a unit apart (the end of one NAL unit's short payload).
        let (c, d) = ranges
            .windows(2)
            .map(|w| (w[0].1, w[1].0))
            .find(|&(c, d)| d - c < 2048)
            .expect("a short payload");
        assert_eq!(seams.prove(&whole, &[(c + d) / 2]), Proof::Proven);
        assert_eq!(seams.prove(&whole, &[]), Proof::Proven);
        // The same far seam on 32 KiB units: checked bytes on both sides within one unit.
        assert!(b - a < 32 << 10);
        seams.unit = 32 << 10;
        assert_eq!(seams.prove(&whole, &[far]), Proof::Proven);
        // A seam inside the mdat header region (before the payload) is never proven.
        assert_eq!(seams.prove(&whole, &[seams.mdat.1 - 1]), Proof::Unproven);
    }

    #[test]
    fn samples_all_of_one_size_never_prove_a_seam() {
        // A video of 40 one-NAL samples of 1000 bytes each (the testkit index patched to a fixed sample size): a run of
        // another clip of the same format would pass every check in place.
        const S: u64 = 1000;
        let v = video(40, 5, true);
        let ftyp = &v.bytes[..v.mdat_payload.start - 8];
        let mut payload = Vec::new();
        let mut x = 11u64;
        for _ in 0..40 {
            payload.extend_from_slice(&((S - 4) as u32).to_be_bytes());
            payload.push(0x65);
            for _ in 0..S - 5 {
                x = x
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                payload.push((x >> 56) as u8);
            }
        }
        let mut moov = v.bytes[v.moov.clone()].to_vec();
        let at = |m: &[u8], kind: &[u8; 4]| m.windows(4).position(|w| w == kind).unwrap() + 4;
        let stsz = at(&moov, b"stsz");
        moov[stsz + 4..stsz + 8].copy_from_slice(&(S as u32).to_be_bytes());
        let stco = at(&moov, b"stco");
        let data_at = ftyp.len() as u64 + 8;
        for c in 0..10u64 {
            let e = stco + 8 + 4 * c as usize;
            moov[e..e + 4].copy_from_slice(&((data_at + 4 * S * c) as u32).to_be_bytes());
        }
        let mut file = ftyp.to_vec();
        file.extend_from_slice(&((8 + payload.len()) as u32).to_be_bytes());
        file.extend_from_slice(b"mdat");
        file.extend_from_slice(&payload);
        let p = file.len();
        let q = p + (64 << 10);
        let mut card = file.clone();
        card.resize(q, 0x5A);
        card.extend_from_slice(&moov);
        card.resize(card.len() + 4096, 0x5A);
        let src = Mem(card);
        let mut seams = VideoSeams::with_orphan(&src, 0, q as u64, moov.len() as u64)
            .expect("a moov-less video and its moov")
            .with_unit(4096);
        assert!(seams.uniform);
        let pieces = vec![
            Fragment {
                start: 0,
                len: p as u64,
            },
            Fragment {
                start: q as u64,
                len: moov.len() as u64,
            },
        ];
        let view = Fragmented::new(&src, pieces);
        // A seam 3 bytes after the header of sample 20: checked bytes right around it.
        let seam = data_at + 20 * S + 8;
        assert_eq!(seams.prove(&view, &[]), Proof::Proven);
        assert_eq!(seams.prove(&view, &[seam]), Proof::Unproven);
        seams.uniform = false;
        assert_eq!(seams.prove(&view, &[seam]), Proof::Proven);
    }

    #[test]
    fn prove_fails_on_a_sample_out_of_place() {
        let v = video(300, 7, true);
        let (src, runs, mut seams) = three_runs(&v);
        // The second and third runs swapped in the view.
        let view = Fragmented::new(&src, vec![runs[0], runs[2], runs[1]]);
        let seams_at = [runs[0].len, runs[0].len + runs[2].len];
        assert_eq!(seams.prove(&view, &seams_at), Proof::Failed);
        let view = Fragmented::new(&src, runs.clone());
        assert_eq!(
            seams.prove(&view, &[runs[0].len, runs[0].len + runs[1].len]),
            Proof::Proven
        );
    }

    #[test]
    fn prefilter_checks_the_first_sample_past_the_seam_within_one_unit() {
        let v = video(300, 7, true);
        let (src, runs, mut seams) = three_runs(&v);
        let seam = runs[0].len;
        let mut r = Reader::with_window(&src, SECTOR as usize);
        assert!(
            seams.prefilter(&mut r, runs[1].start, seam),
            "the true continuation"
        );
        // Noise, the third run (out of step), zeros.
        assert!(!seams.prefilter(&mut r, HEAP + 100 * (32 << 10), seam));
        assert!(!seams.prefilter(&mut r, runs[2].start, seam));
        let zeros = Mem(vec![0u8; src.len() as usize]);
        let mut z = Reader::with_window(&zeros, SECTOR as usize);
        assert!(!seams.prefilter(&mut z, runs[1].start, seam));
        // No sample starts within a unit after a seam in the moov: blind, kept.
        assert!(seams.prefilter(&mut z, runs[1].start, seams.p + 8));
        // On 512-byte units, a sample starting 100 bytes after the seam and ending past the unit: only its bytes in the
        // unit are read, and they fit.
        seams.unit = SECTOR;
        let it = seams
            .moov
            .items
            .iter()
            .map(|it| it.sample())
            .find(|s| {
                s.offset > runs[0].len + 100 && s.offset < runs[0].len + runs[1].len && s.size > 600
            })
            .expect("a long sample in the second run");
        let seam = it.offset - 100;
        let g = runs[1].start + (seam - runs[0].len);
        assert!(seams.prefilter(&mut r, g, seam));
        assert!(!seams.prefilter(&mut z, g, seam));
    }

    #[test]
    fn suspicious_breaks_name_unchecked_stretches_longest_first() {
        let v = video(40, 5, false);
        let (src, mut seams, pieces) = moov_apart(&v, 4096);
        let ranges = checked_ranges(&v, &mut seams);
        let whole = Fragmented::new(&src, pieces);
        let hints = seams.suspicious_breaks(&whole, 0, v.bytes.len() as u64);
        assert!(!hints.is_empty());
        // Each hint is a unit boundary inside a stretch no check reads, with no checked byte within a unit on some side
        // (so the stretch is at least a unit long), the longest stretch first.
        let mut bounds = vec![(0, seams.mdat.1)];
        bounds.extend_from_slice(&ranges);
        bounds.push((seams.p, seams.p));
        let stretch = |x: u64| {
            bounds
                .windows(2)
                .map(|w| (w[0].1, w[1].0))
                .find(|&(a, b)| a < x && x < b)
                .map(|(a, b)| b - a)
        };
        let lens: Vec<u64> = hints
            .iter()
            .map(|&x| {
                assert_eq!(x % 4096, 0);
                let len = stretch(x).unwrap_or_else(|| panic!("{x} is in no unchecked stretch"));
                assert!(len > 4096, "{x}: {len}");
                len
            })
            .collect();
        assert!(lens.windows(2).all(|w| w[0] >= w[1]), "{lens:?}");
        // Small NAL units on 32 KiB units: every boundary has checked bytes within a unit on both sides.
        let (src, runs, mut seams) = three_runs(&video(300, 7, true));
        let view = Fragmented::new(&src, runs);
        assert!(seams.suspicious_breaks(&view, 0, view.len()).is_empty());
    }
}
