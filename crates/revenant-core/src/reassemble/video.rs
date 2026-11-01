//! The seams of an MP4/MOV video (part 1.6, D12): its index places every sample in the file, so each piece's content is
//! checked where the file needs it. Three layouts:
//!
//! - **An orphan `moov`** ([`VideoSeams::with_orphan`]): a video found without its index (`ftyp`, `mdat`), whose `moov`
//!   lies away from it. The `moov` goes at `P`, the end of `mdat`, where it must be the box found at `q` (the anchor).
//! - **Its own `moov` first** ([`VideoSeams::with_own_moov`]): `ftyp, moov, mdat`, the index in the first piece. The
//!   bytes before the `mdat` payload must lie in place; the known length is the end of `mdat` when its size is
//!   declared, else (size 0) the highest sample end.
//! - **Fragmented** ([`MoofSeams`]): `ftyp, moov (with mvex)`, then `moof`/`mdat` pairs (and `sidx styp free skip`),
//!   each `moof`'s samples ([`crate::formats::isobmff::fragment_samples`]) checked in the `mdat` after it, the fragments
//!   in sequence (a whole fragment of another clip checks itself). Complete when an `mfra` closes the chain or the
//!   known size (the file system's) is reached; else the walk ends at the last valid box (at most `Partial`).
//!
//! The file lives in "file offsets": logical 0 is the `ftyp` at the video's start. Each checkable sample (H.264/HEVC
//! NAL units, Motion-JPEG frames) is walked by the pairing's structural check
//! ([`crate::formats::isobmff::check_sample`]), through the view of the pieces chosen so far. Only the bytes the checks
//! read are evidence (the NAL lengths and header bytes, a JPEG's SOI and EOI, the boxes); the payloads are not, so a seam
//! is proven only when checked bytes lie within one unit of it on both sides (a cut one unit earlier or later would then
//! read a checked byte from another place), and not all of them belong to samples of one size (ruling Q39: a run of
//! another clip of a constant-bitrate camera passes every check where frames of one size are expected).

use std::collections::{BTreeSet, VecDeque};
use std::rc::Rc;

use super::{
    Budget, Checks, Counted, Proof, Refutes, Seams, Search, Space, Taken, WalkEnd, Walked, health,
    hint_of, improves, replaces, search,
};
use crate::cache::CachedSource;
use crate::filesystem::{FsState, Placement};
use crate::formats::isobmff::{
    Chain, CheckedSample, FragTrack, MAX_BOXES, MAX_NAL_UNITS, MOOV_MAX_LEN, SampleEnd, be32,
    check_sample, child, frag_track, fragment_samples, index_first_layout, index_health,
    index_tracks, parse_header, tfra_offsets, walk_sample,
};
use crate::formats::{FileKind, Fragment, Health, Reader, SECTOR, cancelled};
use crate::pairing::{self, Item, Moov, Video, fits, load_moov};
use crate::scan::{CancelToken, Finding};
use crate::source::{Fragmented, Source};

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
/// Bytes compared per read when two first pieces are compared (D17).
const COMPARE_CHUNK: usize = 64 << 10;
/// Largest head (`ftyp` to the `mdat` payload or the first fragment) of an index-first video kept in memory.
const HEAD_MAX: u64 = MOOV_MAX_LEN + (1 << 20);
/// Most expected checks a fragmented video remembers for its prefilter (past it every candidate is kept).
const MAX_EXPECT: usize = 1 << 18;
/// Top-level boxes of a fragmented file, besides `moof` and `mdat`, that its chain may hold.
const FRAGMENT_BOXES: [&[u8; 4]; 7] = [
    b"moof", b"mdat", b"sidx", b"styp", b"free", b"skip", b"mfra",
];

/// The state of a walk of a video with its own `moov`: its head is not checked yet.
const HEAD: usize = usize::MAX;

/// A video's samples, as its `moov` lists them, walked across seams.
pub(crate) struct VideoSeams {
    moov: Rc<Moov>,
    /// The end of `mdat`, relative to the file: where an orphan `moov` goes, or (own `moov`) the file's end.
    p: u64,
    /// The `moov` is an orphan that goes at `P` (the anchor); else it is the video's own, in its first piece.
    orphan: bool,
    /// Own `moov`: the file's bytes before the `mdat` payload (`ftyp`, `moov`, `mdat` header), which the first piece
    /// must hold as they were read.
    head: Option<Rc<Vec<u8>>>,
    /// The `mdat` box header, relative to the file: checked where the video was found.
    mdat: (u64, u64),
    /// The space's unit (cluster or sector): a seam is proven only with checked bytes within one unit on both sides.
    unit: u64,
    /// Sample-check steps left.
    steps: u64,
    /// The gaps between checked bytes of the last view the re-check asked about: (view length, gaps by start).
    gaps: Option<(u64, Vec<(u64, u64)>)>,
}

/// How the bytes of a head compare with what lies in place.
enum InPlace {
    Same,
    /// The first byte that differs is at `at - 1`.
    Differs(u64),
    /// A byte from `at` on could not be read (past the view, or a known-bad sector).
    Unread(u64),
}

/// Whether the view holds `head` at logical 0, read within `limit`.
fn head_in_place(r: &mut Reader, head: &[u8], limit: u64) -> InPlace {
    let mut buf = vec![0u8; COMPARE_CHUNK.min(head.len())];
    let mut done = 0usize;
    while done < head.len() {
        let n = buf.len().min(head.len() - done);
        let at = done as u64;
        if !read_in(r, at, &mut buf[..n], limit) {
            return InPlace::Unread(at.min(limit));
        }
        if let Some(d) = buf[..n]
            .iter()
            .zip(&head[done..done + n])
            .position(|(a, b)| a != b)
        {
            return InPlace::Differs(at + d as u64 + 1);
        }
        done += n;
    }
    InPlace::Same
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

    /// A video whose `moov` is in its first piece (`ftyp, moov, mdat`): known length = the end of `mdat` when its size
    /// is declared, else (size 0) the highest sample end; no anchor. `None` for another layout, a `moov` larger than
    /// [`MOOV_MAX_LEN`], unreadable or garbled, without a checkable sample, or with samples outside the `mdat`.
    pub(crate) fn with_own_moov(source: &dyn Source, start: u64) -> Option<VideoSeams> {
        let cache = CachedSource::new(source, SECTOR, 1 << 20);
        let mut r = Reader::new(&cache);
        let l = index_first_layout(&mut r, start)?;
        let (mdat_at, declared) = l.mdat?;
        let (moov_at, hl, size) = l.moov;
        let len = size.checked_sub(hl).filter(|&n| n <= MOOV_MAX_LEN)?;
        if l.head_end - start > HEAD_MAX {
            return None;
        }
        let payload = r.load_box(moov_at + hl, len);
        if r.bad_in(moov_at, moov_at + size) || (payload.len() as u64) < len {
            return None;
        }
        let tracks = index_tracks(&payload)?;
        let mut bounds = (u64::MAX, 0u64);
        let mut items: Vec<Item> = Vec::new();
        for t in &tracks {
            if t.count == 0 {
                continue;
            }
            let (lo, hi) = t.bounds()?;
            bounds = (bounds.0.min(lo), bounds.1.max(hi));
            if let Some(check) = t.check {
                items.extend(t.samples().iter().map(|s| Item {
                    offset: s.offset,
                    size: u32::try_from(s.size).unwrap_or(u32::MAX),
                    check,
                }));
            }
        }
        if items.is_empty() {
            return None;
        }
        items.sort_unstable_by_key(|i| i.offset);
        drop(tracks);
        let (c_min, c_end) = bounds;
        let payload_at = l.head_end - start;
        let p = match declared {
            Some(d) => (mdat_at - start).checked_add(d)?,
            None => c_end,
        };
        if c_min < payload_at || c_end > p {
            return None;
        }
        let mut head = vec![0u8; payload_at as usize];
        if r.read(start, &mut head) != head.len() || r.bad_in(start, l.head_end) {
            return None;
        }
        let moov = Moov {
            q: moov_at,
            size,
            payload,
            items,
            c_min,
            c_end,
        };
        Some(VideoSeams {
            moov: Rc::new(moov),
            p,
            orphan: false,
            head: Some(Rc::new(head)),
            mdat: (mdat_at - start, payload_at),
            unit: SECTOR,
            steps: SEARCH_STEPS,
            gaps: None,
        })
    }

    /// Video `v` with the loaded `moov` `m` (the caller checked [`fits`]); sector units until [`VideoSeams::with_unit`].
    pub(crate) fn new(v: &Video, m: Rc<Moov>) -> VideoSeams {
        VideoSeams {
            moov: m,
            p: v.p,
            orphan: true,
            head: None,
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
        if self.orphan {
            self.p + self.moov.size
        } else {
            self.p
        }
    }

    /// The `moov` payload (the video's own, or the orphan).
    pub(crate) fn moov_payload(&self) -> &[u8] {
        &self.moov.payload
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

    /// Walks every sample of a view of `len` bytes in file order, handing `checked` each range of checked bytes with
    /// the size of its sample; the first sample that does not pass ends it (a cancel ends it as `Spent`).
    fn walk_all(
        &mut self,
        r: &mut Reader,
        len: u64,
        checked: &mut dyn FnMut(u64, u64, u64),
    ) -> Result<(), SampleEnd> {
        let moov = Rc::clone(&self.moov);
        for (i, it) in moov.items.iter().enumerate() {
            if i % CANCEL_EVERY == CANCEL_EVERY - 1 && cancelled() {
                return Err(SampleEnd::Spent);
            }
            let size = u64::from(it.size);
            let got = walk_sample(
                &mut |off, buf| read_in(r, off, buf, len),
                &it.sample(),
                &mut self.steps,
                &mut |a, b| checked(a, b, size),
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
    /// The index of the next sample to check (in file order); [`HEAD`] before the head of an own-`moov` video.
    type State = usize;

    fn begin(&mut self) -> usize {
        if self.head.is_some() { HEAD } else { 0 }
    }

    /// The samples from `from` in file order, each one wholly inside the view checked: `checked` is the end of the
    /// last checked byte (a resume point after each passing sample), `Broken` at the first failing one (where the bytes
    /// it read stop fitting), or where the view ends; `Complete` when every sample passes and the view reaches the known
    /// length, with the `moov` bytes at `P` for an orphan (the engine checks that `P` lies at the anchor). An own-`moov`
    /// video first checks its head in place (`Broken` past the first byte that differs).
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
        if i == HEAD {
            let head = Rc::clone(self.head.as_ref().expect("an own moov"));
            match head_in_place(&mut r, &head, limit) {
                InPlace::Same => {
                    checked = head.len() as u64;
                    resume.push((checked, 0));
                    i = 0;
                }
                InPlace::Differs(at) | InPlace::Unread(at) => {
                    return Walked {
                        end: WalkEnd::Broken { at },
                        checked,
                        resume,
                    };
                }
            }
        }
        let first = i;
        let end = loop {
            let Some(it) = moov.items.get(i) else {
                if limit < self.len() {
                    break WalkEnd::Broken { at: limit };
                }
                if !self.orphan {
                    break WalkEnd::Complete { len: self.len() };
                }
                break match self.moov_at_p(&mut r) {
                    Some(true) => WalkEnd::Complete { len: self.len() },
                    Some(false) => WalkEnd::Broken { at: self.len() },
                    None => WalkEnd::Broken { at: self.p },
                };
            };
            if (i - first) % CANCEL_EVERY == CANCEL_EVERY - 1 && cancelled() {
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

    /// `Proven` iff the `mdat` payload starts before every seam (an own `moov`'s head lying in place), every checkable
    /// sample passes over the view, an orphan `moov`'s bytes sit at `P`, and every seam has checked bytes within one
    /// unit before it and within one unit after it, not all of samples of one size ([`Near`]; the `mdat` header, an own
    /// head and an orphan `moov` count as checked); `Failed` when a sample does not pass, the head is not in place or the
    /// `moov` is not at `P`; `Unproven` otherwise (a seam in a long unchecked stretch or among frames of one size, the
    /// steps spent, an unreadable byte).
    fn prove(&mut self, view: &dyn Source, seams: &[u64]) -> Proof {
        if seams.first().is_some_and(|&s| s < self.mdat.1) {
            return Proof::Unproven;
        }
        let mut near = Near::new(seams, self.unit);
        let mut r = Reader::with_window(view, WALK_WINDOW);
        match &self.head {
            Some(head) => {
                match head_in_place(&mut r, head, view.len()) {
                    InPlace::Same => {}
                    InPlace::Differs(_) => return Proof::Failed,
                    InPlace::Unread(_) => return Proof::Unproven,
                }
                near.range(0, self.mdat.1, None);
            }
            None => near.range(self.mdat.0, self.mdat.1, None),
        }
        match self.walk_all(&mut r, view.len(), &mut |a, b, size| {
            near.range(a, b, Some(size))
        }) {
            Ok(()) => {}
            Err(SampleEnd::Fail { .. }) => return Proof::Failed,
            Err(_) => return Proof::Unproven,
        }
        if self.orphan {
            match self.moov_at_p(&mut r) {
                Some(true) => {}
                Some(false) => return Proof::Failed,
                None => return Proof::Unproven,
            }
            near.range(self.p, self.len(), None);
        }
        if near.finish() {
            Proof::Proven
        } else {
            Proof::Unproven
        }
    }

    fn known_len(&self) -> Option<u64> {
        Some(self.len())
    }

    fn anchor(&self) -> Option<(u64, u64)> {
        self.orphan.then_some((self.p, self.moov.q))
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
        within_unit(r, g, seam, stop, &it.sample())
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
                .walk_all(&mut r, len, &mut |a, b, _| {
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
        weak_breaks(gaps, self.unit, start, end)
    }
}

/// Whether sample `s` (at or past logical `seam`) passes its check read at its place in a piece starting at absolute `g`,
/// as far as it lies before logical `stop` (bytes past it are not read, and what was read decides); an unreadable
/// byte fails it.
fn within_unit(r: &mut Reader, g: u64, seam: u64, stop: u64, s: &CheckedSample) -> bool {
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
        s,
        &mut steps,
    );
    // Stopped at the first byte past the unit: what was read fitted.
    passed || beyond
}

/// The unit boundaries of `(start, end)` inside the unchecked `gaps` (by start) where a seam would not be proven (no
/// checked byte within one unit on some side), longest gap first, at most [`HINTS`].
fn weak_breaks(gaps: &[(u64, u64)], unit: u64, start: u64, end: u64) -> Vec<u64> {
    let mut inside: Vec<(u64, u64)> = gaps
        .iter()
        .filter(|&&(a, b)| b > a && end > 0)
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

/// For each seam, whether checked bytes lie within one unit before it and within one unit after it, and whether those
/// within one unit on either side are not all of samples of one size (ruling Q39: frames of one size are what another
/// clip of a constant-bitrate camera would show there too). Fed with the checked ranges in file order, each with its
/// sample's size, or `None` for structure (box headers, the index), which is never another clip's.
///
/// Ruling Q42, for fragmented video: ranges come in groups (the samples of one fragment), and a seam needs checked bytes
/// of one same group on both sides; a barrier (a fragment's start) within one unit after a seam, or on it, leaves it
/// unproven: two clips of one camera share sequence numbers, times and positions, so nothing tells a whole fragment
/// of the other clip from the clip's own.
struct Near<'s> {
    seams: &'s [u64],
    unit: u64,
    /// Seams decided so far (the next one to look at).
    next: usize,
    /// The ranges that may lie within one unit of the next seam: (start, end, key, group).
    recent: VecDeque<(u64, u64, u64, u64)>,
    /// The barriers at or after the next seam.
    barriers: VecDeque<u64>,
    /// Keys handed to structural ranges so far (each one its own).
    structural: u64,
    ok: bool,
}

impl<'s> Near<'s> {
    fn new(seams: &'s [u64], unit: u64) -> Self {
        Near {
            seams,
            unit,
            next: 0,
            recent: VecDeque::new(),
            barriers: VecDeque::new(),
            structural: 0,
            ok: true,
        }
    }

    /// The checked range `[a, b)` (after every earlier one): of a sample of `size` bytes, or structure (`None`).
    fn range(&mut self, a: u64, b: u64, size: Option<u64>) {
        let key = size.unwrap_or_else(|| {
            self.structural += 1;
            u64::MAX - self.structural
        });
        self.push(a, b, key, 0);
    }

    /// The checked range `[a, b)` of a sample of `size` bytes of group `group` (a fragment).
    fn sample(&mut self, a: u64, b: u64, size: u64, group: u64) {
        self.push(a, b, size, group);
    }

    /// A fragment starts at `x`.
    fn barrier(&mut self, x: u64) {
        self.settle(x);
        if self.next < self.seams.len() {
            self.barriers.push_back(x);
        }
    }

    /// Decides the seams that end a unit before `x` (nothing later can lie within one unit after them), drops what can
    /// no longer matter for the next one.
    fn settle(&mut self, x: u64) {
        while let Some(&s) = self.seams.get(self.next) {
            if s.saturating_add(self.unit) > x {
                break;
            }
            self.decide(s);
            self.next += 1;
        }
        let Some(&s) = self.seams.get(self.next) else {
            self.recent.clear();
            self.barriers.clear();
            return;
        };
        while self
            .recent
            .front()
            .is_some_and(|r| r.1.saturating_add(self.unit) <= s)
        {
            self.recent.pop_front();
        }
        while self.barriers.front().is_some_and(|&b| b < s) {
            self.barriers.pop_front();
        }
    }

    fn push(&mut self, a: u64, b: u64, key: u64, group: u64) {
        if a >= b {
            return;
        }
        self.settle(a);
        if self.next < self.seams.len() {
            self.recent.push_back((a, b, key, group));
        }
    }

    fn decide(&mut self, s: u64) {
        let u = self.unit;
        let before: Vec<u64> = self
            .recent
            .iter()
            .filter(|r| r.0 < s && r.1.saturating_add(u) > s)
            .map(|r| r.3)
            .collect();
        let same = self
            .recent
            .iter()
            .any(|r| r.1 > s && r.0 < s.saturating_add(u) && before.contains(&r.3));
        let barrier = self
            .barriers
            .iter()
            .any(|&b| b >= s && b < s.saturating_add(u));
        let mut keys = self
            .recent
            .iter()
            .filter(|r| r.0 < s.saturating_add(u) && r.1.saturating_add(u) > s)
            .map(|r| r.2);
        let first = keys.next();
        let mixed = keys.any(|k| Some(k) != first);
        self.ok &= same && !barrier && mixed;
    }

    /// Every seam near checked bytes of one group on both sides, with no barrier within one unit after it, not all of
    /// samples of one size.
    fn finish(mut self) -> bool {
        while let Some(&s) = self.seams.get(self.next) {
            self.decide(s);
            self.next += 1;
        }
        self.ok
    }
}

/// What a fragmented video's walk tells about a checked range.
#[derive(Clone, Copy)]
enum Mark {
    /// The head, a box header, a `moof`, an `mfro`.
    Structure,
    /// A sample's checked bytes, of the fragment whose `moof` is at `fragment`.
    Sample { size: u64, fragment: u64 },
    /// A fragment (its `moof`) starts here (an empty range).
    Fragment,
}

/// A check a fragmented video's walk expects at a logical offset: a box header, or a sample.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Expect {
    Header,
    Sample { size: u64 },
}

/// The seams of a fragmented MP4/MOV (`moof`): its head (`ftyp` to the first fragment box) must lie in place, then the
/// top-level boxes follow in order, each header checked (a valid size, a type of [`FRAGMENT_BOXES`]), each `moof`
/// parsed ([`fragment_samples`], each listed sample a step) and its samples checked in the `mdat` right after it, each
/// fragment following the last ([`Chain::follows`]: sequence number, decode time, track). At most [`MAX_BOXES`] boxes
/// per walk (C9). `Complete` when an `mfra` closes the chain (its `mfro` gives its size, its `tfra` lists no `moof`
/// past the last one walked) or the known size is reached; else the walk ends at the last valid box (at most
/// `Partial`).
pub(crate) struct MoofSeams {
    track: FragTrack,
    head: Rc<Vec<u8>>,
    /// The file-system entry's size, when one is known: where the chain must end.
    known: Option<u64>,
    unit: u64,
    steps: u64,
    /// The checks walks expected past where they stopped, for the prefilter (by logical offset); every walk's, as
    /// different paths may expect different things at one offset (a candidate passing any of them is kept).
    expect: BTreeSet<(u64, Expect)>,
    /// More than [`MAX_EXPECT`] were met: the prefilter keeps every candidate.
    expect_full: bool,
}

/// Where a walk of a fragmented video stands.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct MoofState {
    /// Logical offset of the box to read next, or of the `moof` whose samples are being checked; 0: the head is not
    /// checked yet.
    pos: u64,
    /// Samples of that `moof` already checked.
    k: usize,
    /// The chain before the box at `pos` (`None` before the first fragment).
    chain: Option<Chain>,
}

/// How a walk of a fragmented video ended.
struct MoofRun {
    end: WalkEnd,
    checked: u64,
    resume: Vec<(u64, MoofState)>,
    /// A check refuted the bytes (not a cut by the view's end, an unreadable byte or a budget).
    failed: bool,
    /// The `moof`s walked (logical offsets) and the payload of the `mfra` that completed the walk.
    moofs: Vec<u64>,
    mfra: Option<(u64, u64)>,
}

impl MoofSeams {
    /// A fragmented video at `start`: an `ftyp`, a `moov` with an `mvex` and a checkable video track ([`frag_track`]),
    /// then a fragment box. `None` for any other layout, or a head larger than [`HEAD_MAX`] or unreadable.
    pub(crate) fn new(source: &dyn Source, start: u64) -> Option<MoofSeams> {
        let cache = CachedSource::new(source, SECTOR, 1 << 20);
        let mut r = Reader::new(&cache);
        let l = index_first_layout(&mut r, start)?;
        if l.mdat.is_some() || l.head_end - start > HEAD_MAX {
            return None;
        }
        let (moov_at, hl, size) = l.moov;
        let len = size.checked_sub(hl).filter(|&n| n <= MOOV_MAX_LEN)?;
        let payload = r.load_box(moov_at + hl, len);
        let mut garbled = false;
        child(&payload, b"mvex", &mut garbled)?;
        let track = frag_track(&payload)?;
        let mut head = vec![0u8; (l.head_end - start) as usize];
        if r.read(start, &mut head) != head.len() || r.bad_in(start, l.head_end) {
            return None;
        }
        Some(MoofSeams {
            track,
            head: Rc::new(head),
            known: None,
            unit: SECTOR,
            steps: SEARCH_STEPS,
            expect: BTreeSet::new(),
            expect_full: false,
        })
    }

    /// The space's unit (a seam must have checked bytes within it on both sides to be proven).
    pub(crate) fn with_unit(mut self, unit: u64) -> MoofSeams {
        self.unit = unit.max(1);
        self
    }

    /// The file-system entry's size, when one is known (Q3): the chain is complete when it ends there.
    pub(crate) fn with_known(mut self, known: Option<u64>) -> MoofSeams {
        self.known = known;
        self
    }

    fn remember(&mut self, at: u64, e: Expect) {
        if self.expect.len() >= MAX_EXPECT {
            self.expect_full = true;
        } else {
            self.expect.insert((at, e));
        }
    }

    /// Walks the view from `from` up to `limit`, handing `on` each checked range (with its sample's size, or `None` for
    /// structure); `record` keeps what it expected for the prefilter.
    fn run(
        &mut self,
        view: &dyn Source,
        from: &MoofState,
        limit: u64,
        record: bool,
        on: &mut dyn FnMut(u64, u64, Mark),
    ) -> MoofRun {
        let limit = limit.min(view.len());
        let mut r = Reader::with_window(view, WALK_WINDOW);
        let (mut pos, mut k, mut chain) = (from.pos, from.k, from.chain);
        let mut out = MoofRun {
            end: WalkEnd::Stopped,
            checked: 0,
            resume: Vec::new(),
            failed: false,
            moofs: Vec::new(),
            mfra: None,
        };
        let at = |pos, k, chain| MoofState { pos, k, chain };
        if pos == 0 {
            let head = Rc::clone(&self.head);
            match head_in_place(&mut r, &head, limit) {
                InPlace::Same => {
                    pos = head.len() as u64;
                    on(0, pos, Mark::Structure);
                    out.checked = pos;
                    out.resume.push((pos, at(pos, 0, None)));
                }
                InPlace::Differs(at) => {
                    out.end = WalkEnd::Broken { at };
                    out.failed = true;
                    return out;
                }
                InPlace::Unread(at) => {
                    out.end = WalkEnd::Broken { at };
                    return out;
                }
            }
        }
        let mut boxes = 0usize;
        out.end = 'walk: loop {
            if self.known == Some(pos) {
                break WalkEnd::Complete { len: pos };
            }
            if record {
                self.remember(pos, Expect::Header);
            }
            if pos >= limit {
                // The chain is valid up to the view's end.
                out.checked = out.checked.max(pos);
                break WalkEnd::Broken { at: limit };
            }
            boxes += 1;
            if boxes > MAX_BOXES || (boxes.is_multiple_of(CANCEL_EVERY) && cancelled()) {
                break WalkEnd::Stopped;
            }
            let Some(h) = self.header(&mut r, pos, limit) else {
                break WalkEnd::Broken { at: limit };
            };
            let Some((kind, hl, end)) = h else {
                // Not a box of the chain: the file ends at the last valid box (or another file's data is here).
                out.checked = out.checked.max(pos);
                out.failed = true;
                break WalkEnd::Broken {
                    at: (pos + 8).min(limit),
                };
            };
            if &kind == b"moof" {
                on(pos, pos, Mark::Fragment);
            }
            on(pos, pos + hl, Mark::Structure);
            out.checked = out.checked.max(pos + hl);
            match &kind {
                b"mfra" => {
                    if end - pos - hl > MOOV_MAX_LEN {
                        out.failed = true;
                        break WalkEnd::Broken { at: pos + hl };
                    }
                    if end > limit {
                        break WalkEnd::Broken { at: limit };
                    }
                    // It closes this chain only when its `tfra` lists no `moof` past the last one walked (a jump from an
                    // earlier fragment to the `mfra` would leave some out); without a `tfra` it tells nothing.
                    let mut payload = vec![0u8; (end - pos - hl) as usize];
                    if !read_in(&mut r, pos + hl, &mut payload, limit) {
                        break WalkEnd::Broken { at: pos + hl };
                    }
                    match (tfra_offsets(&payload), chain) {
                        (Some(o), Some(c)) if !o.is_empty() && o.iter().all(|&x| x <= c.at) => {}
                        (Some(o), _) if o.is_empty() => break WalkEnd::Broken { at: end },
                        _ => {
                            out.failed = true;
                            break WalkEnd::Broken { at: end };
                        }
                    }
                    // The last box of an `mfra` is its `mfro`: size 16, version and flags, the `mfra`'s size.
                    let mut tail = [0u8; 16];
                    if end < pos + hl + 16 || !read_in(&mut r, end - 16, &mut tail, limit) {
                        out.failed = end < pos + hl + 16;
                        break WalkEnd::Broken { at: end };
                    }
                    if be32(&tail) != 16
                        || &tail[4..8] != b"mfro"
                        || u64::from(be32(&tail[12..])) != end - pos
                    {
                        out.failed = true;
                        break WalkEnd::Broken { at: end };
                    }
                    on(end - 16, end, Mark::Structure);
                    out.checked = end;
                    out.mfra = Some((pos + hl, end));
                    break WalkEnd::Complete { len: end };
                }
                b"moof" => {
                    // As the measure's `load`: a `moof` larger than an index may be is garbled, never loaded.
                    if end - pos - hl > MOOV_MAX_LEN {
                        out.failed = true;
                        break WalkEnd::Broken { at: pos + hl };
                    }
                    if end > limit {
                        break WalkEnd::Broken { at: limit };
                    }
                    let mut payload = vec![0u8; (end - pos - hl) as usize];
                    if !read_in(&mut r, pos + hl, &mut payload, limit) {
                        break WalkEnd::Broken { at: pos + hl };
                    }
                    // Every sample listed costs a step; the fragment must follow the chain (its sequence number, its
                    // decode time): a fragment of another clip checks itself, nothing else tells it apart.
                    let Some(frag) = fragment_samples(pos, &payload, &self.track, &mut self.steps)
                    else {
                        if self.steps == 0 {
                            break WalkEnd::Stopped;
                        }
                        out.failed = true;
                        break WalkEnd::Broken { at: end };
                    };
                    let Some(next) = Chain::follows(chain, &frag, pos) else {
                        out.failed = true;
                        break WalkEnd::Broken { at: end };
                    };
                    out.moofs.push(pos);
                    let samples = frag.samples;
                    if record {
                        self.remember(end, Expect::Header);
                        for s in samples.iter().skip(k) {
                            self.remember(s.offset, Expect::Sample { size: s.size });
                        }
                    }
                    // The `mdat` right after it holds its samples.
                    let Some(m) = self.header(&mut r, end, limit) else {
                        break WalkEnd::Broken { at: limit };
                    };
                    let Some((mkind, mhl, mend)) = m.filter(|m| &m.0 == b"mdat") else {
                        out.failed = true;
                        break WalkEnd::Broken {
                            at: (end + 8).min(limit),
                        };
                    };
                    let _ = mkind;
                    if samples
                        .iter()
                        .any(|s| s.offset < end + mhl || s.offset.saturating_add(s.size) > mend)
                    {
                        out.failed = true;
                        break WalkEnd::Broken { at: end + mhl };
                    }
                    on(pos + hl, end, Mark::Structure);
                    on(end, end + mhl, Mark::Structure);
                    out.checked = out.checked.max(end + mhl);
                    if record {
                        self.remember(mend, Expect::Header);
                    }
                    for (j, s) in samples.iter().enumerate().skip(k) {
                        if j % CANCEL_EVERY == CANCEL_EVERY - 1 && cancelled() {
                            break 'walk WalkEnd::Stopped;
                        }
                        let mut last = None;
                        let got = walk_sample(
                            &mut |off, buf| read_in(&mut r, off, buf, limit),
                            s,
                            &mut self.steps,
                            &mut |a, b| {
                                on(
                                    a,
                                    b,
                                    Mark::Sample {
                                        size: s.size,
                                        fragment: pos,
                                    },
                                );
                                last = Some(b);
                            },
                        );
                        if let Some(to) = last {
                            out.checked = out.checked.max(to);
                        }
                        match got {
                            SampleEnd::Pass => {
                                let prev = out.resume.last().map_or(0, |r| r.0);
                                out.resume.push((
                                    last.unwrap_or(s.offset).max(prev),
                                    at(pos, j + 1, chain),
                                ));
                            }
                            SampleEnd::Fail { at } => {
                                out.failed = true;
                                break 'walk WalkEnd::Broken { at };
                            }
                            SampleEnd::Unread { at } => {
                                break 'walk WalkEnd::Broken { at: at.min(limit) };
                            }
                            SampleEnd::Spent => break 'walk WalkEnd::Stopped,
                        }
                    }
                    pos = mend;
                    chain = Some(next);
                }
                _ => pos = end,
            }
            k = 0;
            let prev = out.resume.last().map_or(0, |r| r.0);
            out.resume.push((pos.max(prev), at(pos, 0, chain)));
        };
        out
    }

    /// The top-level box header at `pos`: `None` when it cannot be read within `limit` (cut by the view, or
    /// unreadable); `Some(None)` when it is not a box of the chain (an invalid size or type, a size 0, past the known
    /// size); else its type, header length and end.
    fn header(&self, r: &mut Reader, pos: u64, limit: u64) -> Option<Option<([u8; 4], u64, u64)>> {
        let mut b = [0u8; 16];
        let n = (limit.saturating_sub(pos)).min(16) as usize;
        if n < 8 || !read_in(r, pos, &mut b[..n], limit) {
            return None;
        }
        let Some(h) = parse_header(&b[..n]) else {
            // A 64-bit size cut by the view is undecided; anything else is no box.
            return if n < 16 && be32(&b) == 1 {
                None
            } else {
                Some(None)
            };
        };
        let ok = FRAGMENT_BOXES.contains(&&h.kind)
            && h.size.is_some_and(|s| s >= h.header_len)
            && pos.checked_add(h.size.unwrap_or(0)).is_some();
        let end = pos.saturating_add(h.size.unwrap_or(0));
        Some((ok && self.known.is_none_or(|k| end <= k)).then_some((h.kind, h.header_len, end)))
    }

    /// Whether the chain of the view is valid to its very end (every box whole, every sample passing): a fragmented
    /// video with no known size and no `mfra` that came out this way is as complete as can be told.
    pub(crate) fn clean(&mut self, view: &dyn Source) -> bool {
        let len = view.len();
        let run = self.run(view, &MoofState::default(), len, false, &mut |_, _, _| {});
        matches!(run.end, WalkEnd::Broken { at } if at == len) && run.checked == len
    }
}

impl Seams for MoofSeams {
    type State = MoofState;

    fn begin(&mut self) -> MoofState {
        MoofState::default()
    }

    /// The boxes from `from` in order: `checked` is the end of the last checked byte (the end of the last valid box when
    /// the chain stops there), a resume point after each passing sample and each box; `Broken` where the bytes stop
    /// fitting (an invalid header, a garbled `moof`, a failing sample) or where the view ends; `Complete` at an `mfra`
    /// or the known size.
    fn walk(
        &mut self,
        view: &dyn Source,
        seams: &[u64],
        from: &MoofState,
        limit: u64,
    ) -> Walked<MoofState> {
        let _ = seams;
        let run = self.run(view, from, limit, true, &mut |_, _, _| {});
        Walked {
            end: run.end,
            checked: run.checked,
            resume: run.resume,
        }
    }

    /// `Proven` iff no seam falls in the head, the whole view walks `Complete` to its end, and every seam is near
    /// checked bytes on both sides, not all of samples of one size ([`Near`]; box headers and `moof`s count as
    /// structure); `Failed` when a check refutes the bytes; `Unproven` otherwise.
    fn prove(&mut self, view: &dyn Source, seams: &[u64]) -> Proof {
        if seams.first().is_some_and(|&s| s < self.head.len() as u64) {
            return Proof::Unproven;
        }
        let mut near = Near::new(seams, self.unit);
        let len = view.len();
        let run = self.run(
            view,
            &MoofState::default(),
            len,
            false,
            // Q42: only a fragment's own samples prove a seam; its boxes never do across one.
            &mut |a, b, mark| match mark {
                Mark::Structure => {}
                Mark::Fragment => near.barrier(a),
                Mark::Sample { size, fragment } => near.sample(a, b, size, fragment),
            },
        );
        match run.end {
            WalkEnd::Complete { len: l } if l == len => {
                // An `mfra` that lists where the fragments are (`tfra`): each listed `moof` must be one walked here.
                if let Some((a, b)) = run.mfra {
                    let mut r = Reader::with_window(view, WALK_WINDOW);
                    if b - a > MOOV_MAX_LEN {
                        return Proof::Unproven;
                    }
                    let mut payload = vec![0u8; (b - a) as usize];
                    if !read_in(&mut r, a, &mut payload, len) {
                        return Proof::Unproven;
                    }
                    match tfra_offsets(&payload) {
                        None => return Proof::Unproven,
                        Some(o) if o.iter().any(|x| !run.moofs.contains(x)) => {
                            return Proof::Failed;
                        }
                        Some(_) => {}
                    }
                }
                if near.finish() {
                    Proof::Proven
                } else {
                    Proof::Unproven
                }
            }
            _ if run.failed => Proof::Failed,
            _ => Proof::Unproven,
        }
    }

    fn known_len(&self) -> Option<u64> {
        self.known
    }

    /// Its answers depend on what earlier walks expected (the paths before a seam may differ): never remembered.
    fn memo_prefilter(&self) -> bool {
        false
    }

    /// Keeps candidate `g` for the seam when one of the checks a walk expected within one unit after the seam (a box
    /// header, a sample) passes read at its place in a piece starting at `g`, as far as it lies within that unit; when
    /// none is expected there (or too many were remembered), blind: kept (Q2).
    fn prefilter(&mut self, r: &mut Reader, g: u64, seam: u64) -> bool {
        if self.expect_full {
            return true;
        }
        let stop = seam.saturating_add(self.unit);
        let mut any = false;
        let wanted: Vec<(u64, Expect)> = self
            .expect
            .range((seam, Expect::Header)..(stop, Expect::Header))
            .copied()
            .collect();
        for (off, e) in wanted {
            any = true;
            let pass = match e {
                Expect::Header => {
                    if off + 8 > stop {
                        true
                    } else {
                        let mut b = [0u8; 8];
                        let at = g + (off - seam);
                        r.read(at, &mut b) == 8
                            && !r.bad_in(at, at + 8)
                            && parse_header(&b).is_some_and(|h| {
                                FRAGMENT_BOXES.contains(&&h.kind) && h.size.is_some()
                            })
                    }
                }
                Expect::Sample { size } => within_unit(
                    r,
                    g,
                    seam,
                    stop,
                    &CheckedSample {
                        offset: off,
                        size,
                        check: self.track.check,
                    },
                ),
            };
            if pass {
                return true;
            }
        }
        !any
    }

    /// As [`VideoSeams::suspicious_breaks`], over the checked ranges of the boxes and samples.
    fn suspicious_breaks(&mut self, view: &dyn Source, start: u64, end: u64) -> Vec<u64> {
        let mut gaps = Vec::new();
        let mut prev = self.head.len() as u64;
        let len = view.len();
        let run = self.run(view, &MoofState::default(), len, false, &mut |a, b, _| {
            if a > prev {
                gaps.push((prev, a));
            }
            prev = prev.max(b);
        });
        if matches!(run.end, WalkEnd::Complete { .. }) && len > prev {
            gaps.push((prev, len));
        }
        weak_breaks(&gaps, self.unit, start, end)
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

/// The length of the first piece of video `v` under `moov` `m`: where its samples stop passing over the free run from
/// its start (the end of its last checked byte), read through `counted`; `None` when the run is empty, the walk is
/// complete (no piece to compare) or undecided.
fn first_piece(
    counted: &Counted,
    v: &Video,
    m: &Rc<Moov>,
    ctx: &super::Context,
    taken: &Taken,
) -> Option<(u64, u64)> {
    let start = v.finding.start;
    let space = Space::new(
        ctx.grids,
        start,
        counted.len(),
        taken,
        ctx.starts,
        ctx.limits,
    );
    let run = space.free_run(start, &[], &[], v.p.saturating_add(m.size));
    if run == 0 {
        return None;
    }
    let mut seams = VideoSeams::new(v, Rc::clone(m)).with_unit(space.unit());
    let view = Fragmented::new(counted, vec![Fragment { start, len: run }]);
    let w = seams.walk(&view, &[], &0, run);
    matches!(w.end, WalkEnd::Broken { .. }).then_some((w.checked, run))
}

/// How many bytes from `a` and from `b` are equal in a row, at most `max`, read through `counted` (`None` when a read
/// fails, the budget among them).
fn equal_run(counted: &Counted, a: u64, b: u64, max: u64) -> Option<u64> {
    let mut x = vec![0u8; COMPARE_CHUNK];
    let mut y = vec![0u8; COMPARE_CHUNK];
    let mut done = 0u64;
    while done < max {
        let n = (max - done).min(COMPARE_CHUNK as u64) as usize;
        if counted.read_at(a + done, &mut x[..n]).ok()? != n
            || counted.read_at(b + done, &mut y[..n]).ok()? != n
        {
            return None;
        }
        if let Some(d) = x[..n].iter().zip(&y[..n]).position(|(p, q)| p != q) {
            return Some(done + d as u64);
        }
        done += n as u64;
    }
    Some(done)
}

/// The other candidate videos that `m` fits and whose first piece is the same as `v`'s (D17): the same length (where
/// the samples stop passing) and the same bytes, compared through the cache within the file's read budget. Each one
/// with the run of its bytes equal to `v`'s from its start (no piece of `v` is taken from there: those bytes are `v`'s
/// own). Nothing when `v`'s first piece cannot be told.
fn twins(
    source: &CachedSource,
    videos: &[Video],
    v: &Video,
    m: &Rc<Moov>,
    ctx: &super::Context,
    taken: &Taken,
    file: &mut Budget,
) -> Vec<(u64, Fragment)> {
    let others: Vec<&Video> = videos
        .iter()
        .filter(|o| {
            o.finding.id != v.finding.id
                && o.p == v.p
                && o.mdat == v.mdat
                && fits(o, m.q, m.c_min, m.c_end)
        })
        .collect();
    if others.is_empty() {
        return Vec::new();
    }
    let counted = Counted::new(source);
    counted.set_limit(file.read);
    let mut out = Vec::new();
    if let Some((len, run)) = first_piece(&counted, v, m, ctx, taken) {
        for o in others {
            let Some((olen, orun)) = first_piece(&counted, o, m, ctx, taken) else {
                continue;
            };
            if olen != len || len == 0 {
                continue;
            }
            let Some(same) = equal_run(&counted, v.finding.start, o.finding.start, run.min(orun))
            else {
                continue;
            };
            if same >= len {
                out.push((
                    o.finding.id,
                    Fragment {
                        start: o.finding.start,
                        len: same,
                    },
                ));
            }
        }
    }
    file.charge(0, counted.read());
    out
}

/// The own `moov` of video `f` (not `Intact`) as `(absolute start, declared size)`: the box header at the end of its
/// `mdat` ([`Video::of`]), read through its finding's pieces, when the whole box lies inside them (ruling Q52). One
/// 16-byte read.
pub(crate) fn own_moov(source: &CachedSource, f: &Finding) -> Option<(u64, u64)> {
    if f.health == Health::Intact {
        return None;
    }
    let v = Video::of(&mut Reader::new(source), f)?;
    let view = Fragmented::new(source, f.ranges());
    let mut head = [0u8; 16];
    if view.read_at(v.p, &mut head).ok()? != head.len() {
        return None;
    }
    let size = crate::scan::moov_head(&head)?;
    if v.p.checked_add(size)? > f.len {
        return None;
    }
    let mut at = 0u64;
    f.ranges()
        .iter()
        .find_map(|r| {
            let inside = (at..at + r.len).contains(&v.p);
            let q = inside.then(|| r.start + (v.p - at));
            at += r.len;
            q
        })
        .map(|q| (q, size))
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
/// can improve it; two or more `moov`s giving one make it not unique. D17: a `moov` that also fits another candidate
/// video with the same first piece (a copy of it) is searched only for the lowest id of them, with the copies' bytes
/// kept out of its pieces; the others are not searched with it.
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
    let hint = hint_of(f);
    let space = Space::new(
        ctx.grids,
        f.start,
        source.len(),
        taken,
        ctx.starts,
        ctx.limits,
    )
    .with_hint(&hint);
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
        let copies = twins(source, &moovs.videos, &v, &m, ctx, taken, &mut file);
        if copies.iter().any(|(id, _)| *id < f.id) {
            // The lowest id of the copies is searched with this `moov`.
            continue;
        }
        let kept = (!copies.is_empty()).then(|| {
            let mut t = taken.clone();
            t.claim(&copies.iter().map(|c| c.1).collect::<Vec<_>>());
            t
        });
        let apart = kept.as_ref().map(|t| {
            Space::new(ctx.grids, f.start, source.len(), t, ctx.starts, ctx.limits).with_hint(&hint)
        });
        let space = apart.as_ref().unwrap_or(&space);
        let mut seams = VideoSeams::new(&v, Rc::clone(&m)).with_unit(space.unit());
        let found = search(
            &mut seams, source, space, f.start, known, limits, &mut file, cancel,
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
    let checks = checks(
        ctx,
        source,
        &found,
        known,
        index_health(&m.payload, len, false) == Health::Intact,
    );
    out.new = finding(f, &found, v.kind, &checks).filter(|new| improves(old, new, found.complete));
    out
}

/// The scan's checks of a video reassembly (D6): `own_ok` is the format's own check over the pieces.
fn checks(
    ctx: &super::Context,
    source: &CachedSource,
    found: &Search,
    known: Option<u64>,
    own_ok: bool,
) -> Checks {
    Checks {
        capped: ctx.capped,
        frags_capped: ctx.frags_capped,
        claimed_capped: ctx.claimed_capped,
        listing_truncated: ctx.listing_truncated,
        bad: found
            .pieces
            .iter()
            .any(|p| !source.bad_runs(p.start, p.start + p.len).is_empty()),
        size_ok: known.is_none_or(|k| k == found.len),
        own_ok,
        restarts: false,
        content_only: true,
    }
}

/// The finding `f` becomes with the pieces of `found`.
fn finding(f: &Finding, found: &Search, kind: FileKind, checks: &Checks) -> Option<Finding> {
    Some(Finding {
        id: f.id,
        start: f.start,
        len: found.len,
        kind,
        health: health(found, kind, checks),
        fragments: if found.pieces.len() > 1 {
            found.pieces.clone()
        } else {
            Vec::new()
        },
        origin: f.origin.clone().map(|mut o| {
            o.placement = Placement::Carved;
            o
        }),
    })
}

impl Refutes for VideoSeams {
    fn refutes(&mut self, view: &dyn Source) -> bool {
        let mut r = Reader::with_window(view, WALK_WINDOW);
        if let Some(head) = self.head.clone()
            && let InPlace::Differs(_) = head_in_place(&mut r, &head, view.len())
        {
            return true;
        }
        matches!(
            self.walk_all(&mut r, view.len(), &mut |_, _, _| {}),
            Err(SampleEnd::Fail { .. })
        )
    }
}

impl Refutes for MoofSeams {
    fn refutes(&mut self, view: &dyn Source) -> bool {
        let len = view.len();
        let run = self.run(view, &MoofState::default(), len, false, &mut |_, _, _| {});
        run.failed && !matches!(run.end, WalkEnd::Stopped)
    }
}

/// A video whose index comes before its media data (its own `moov` first, or fragments) searched; `None` when `f` has
/// no such layout (the caller then tries the orphan `moov`s), else the finding it becomes when that improves it. Its
/// head (`ftyp` to the media data) is read within the file's read budget. A fragmented video with no known size comes
/// out at most `Partial`; one whose chain of boxes is valid to the end of its pieces counts as complete for
/// [`improves`].
pub(crate) fn reassemble_index_first(
    source: &CachedSource,
    f: &Finding,
    taken: &Taken,
    ctx: &super::Context,
    budget: &mut Budget,
    cancel: &CancelToken,
) -> Option<Option<Finding>> {
    let layout = index_first_layout(&mut Reader::new(source), f.start)?;
    let limits = ctx.limits;
    if budget.spent() {
        return Some(None);
    }
    let known = f
        .origin
        .as_ref()
        .filter(|o| o.state == FsState::Deleted)
        .map(|o| o.size);
    let space = Space::new(ctx.grids, f.start, source.len(), taken, ctx.starts, limits)
        .with_hint(&hint_of(f));
    let counted = Counted::new(source);
    counted.set_limit(limits.file_read.min(budget.read));
    let (found, own_ok, clean, mut seams): (_, _, _, Box<dyn Refutes>) = if layout.mdat.is_some() {
        let seams = VideoSeams::with_own_moov(&counted, f.start);
        budget.charge(0, counted.read());
        let mut seams = seams?.with_unit(space.unit());
        let found = search(
            &mut seams, source, &space, f.start, known, limits, budget, cancel,
        );
        let Some(found) = found else {
            return Some(None);
        };
        let own = index_health(seams.moov_payload(), found.len, false) == Health::Intact;
        (found, own, false, Box::new(seams))
    } else {
        let seams = MoofSeams::new(&counted, f.start);
        budget.charge(0, counted.read());
        let mut seams = seams?.with_unit(space.unit()).with_known(known);
        let found = search(
            &mut seams, source, &space, f.start, known, limits, budget, cancel,
        );
        let Some(found) = found else {
            return Some(None);
        };
        let clean = !found.complete
            && known.is_none()
            && seams.clean(&Fragmented::new(source, found.pieces.clone()));
        (found, true, clean, Box::new(seams))
    };
    if cancel.is_cancelled() || found.pieces.first().map(|p| p.start) != Some(f.start) {
        return Some(None);
    }
    let checks = checks(ctx, source, &found, known, own_ok);
    Some(finding(f, &found, layout.kind, &checks).filter(|new| {
        improves(f, new, found.complete || clean)
            || replaces(f, new, found.complete, source, seams.as_mut())
    }))
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

    fn find<S: Seams>(
        src: &Mem,
        seams: &mut S,
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
            .walk_all(&mut r, src.len(), &mut |a, b, _| out.push((a, b)))
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

    /// `v` with every byte its checks do not read (outside the checked ranges, the header before the `mdat` payload
    /// and the `moov`) re-randomised from `seed`: another clip with the same NAL layout, sample by sample.
    fn donor_of(v: &AvcVideo, seams: &mut VideoSeams, seed: u64) -> Vec<u8> {
        let mut keep = vec![false; v.bytes.len()];
        keep[..v.mdat_payload.start].fill(true);
        keep[v.moov.clone()].fill(true);
        for (a, b) in checked_ranges(v, seams) {
            keep[a as usize..b as usize].fill(true);
        }
        let mut x = seed;
        v.bytes
            .iter()
            .zip(&keep)
            .map(|(&b, &k)| {
                x = x
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                if k { b } else { (x >> 56) as u8 }
            })
            .collect()
    }

    #[test]
    #[ignore = "Q39 residual risk: a donor with the clip's exact NAL layout, sample by sample, shows the very bytes \
                the checks read where the clip's own run would; see the core README (video residual risks)"]
    fn a_donor_with_the_exact_nal_layout_is_not_intact() {
        // Ruling Q39, the Task 5 reviewer's probe: A's middle run is lost (written over), and a donor clip with A's NAL
        // lengths and headers but other payload holds the matching range at cluster 100.
        const C: u64 = 32 << 10;
        let v = video(300, 7, true);
        let (Mem(mut card), runs, mut seams) = three_runs(&v);
        let donor = donor_of(&v, &mut seams, 0xD0D0);
        assert_ne!(donor, v.bytes);
        let mid = runs[1];
        let noise = noise_card(400, C);
        card[mid.start as usize..(mid.start + mid.len) as usize]
            .copy_from_slice(&noise[mid.start as usize..(mid.start + mid.len) as usize]);
        let from = runs[0].len as usize;
        let at = (HEAP + 100 * C) as usize;
        card[at..at + mid.len as usize].copy_from_slice(&donor[from..from + mid.len as usize]);
        let src = Mem(card);
        let found = find(
            &src,
            &mut seams,
            (HEAP, C, src.len()),
            runs[0].start,
            &Limits::DEFAULT,
        );
        if let Some(f) = &found {
            assert!(
                !(f.complete && f.unique && f.exhaustive && f.proof == Proof::Proven),
                "{f:?}"
            );
        }
    }

    /// The 300-sample testkit clip of `seed` with its samples rewritten to `sizes` (one NAL unit each, seeded payload
    /// from `payload_seed`) and its index patched to match: a constant-bitrate camera when the sizes repeat.
    fn sized(sizes: &[u32], seed: u64, payload_seed: u64) -> AvcVideo {
        let v = video(sizes.len(), seed, true);
        let mut payload = Vec::new();
        let mut x = payload_seed;
        for (i, &size) in sizes.iter().enumerate() {
            payload.extend_from_slice(&(size - 4).to_be_bytes());
            payload.push(if i % 4 == 0 { 0x65 } else { 0x41 });
            for _ in 0..size - 5 {
                x = x
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                payload.push((x >> 56) as u8);
            }
        }
        let mut moov = v.bytes[v.moov.clone()].to_vec();
        let at = |m: &[u8], kind: &[u8; 4]| m.windows(4).position(|w| w == kind).unwrap() + 4;
        let stsz = at(&moov, b"stsz");
        for (i, &size) in sizes.iter().enumerate() {
            moov[stsz + 12 + 4 * i..stsz + 16 + 4 * i].copy_from_slice(&size.to_be_bytes());
        }
        let stco = at(&moov, b"stco");
        let data_at = v.mdat_payload.start as u32;
        let mut off = data_at;
        for (i, &size) in sizes.iter().enumerate() {
            if i % 4 == 0 {
                let e = stco + 8 + 4 * (i / 4);
                moov[e..e + 4].copy_from_slice(&off.to_be_bytes());
            }
            off += size;
        }
        let mut bytes = v.bytes[..v.mdat_payload.start - 8].to_vec();
        bytes.extend_from_slice(&((8 + payload.len()) as u32).to_be_bytes());
        bytes.extend_from_slice(b"mdat");
        bytes.extend_from_slice(&payload);
        let moov_at = bytes.len();
        bytes.extend_from_slice(&moov);
        AvcVideo {
            mdat_payload: v.mdat_payload.start..moov_at,
            moov: moov_at..bytes.len(),
            bytes,
        }
    }

    #[test]
    fn a_constant_bitrate_donor_is_not_intact() {
        // Ruling Q39: a clip whose frames are all 1000 bytes but its first ten (so the clip is not uniform as a whole),
        // its middle run lost, and the same run of another clip of the same constant-bitrate camera at cluster 100:
        // every check passes there, so the seams around it must not be proven.
        const C: u64 = 32 << 10;
        let sizes: Vec<u32> = (0..300)
            .map(|i| if i < 10 { 600 + 70 * i } else { 1000 })
            .collect();
        let v = sized(&sizes, 7, 1);
        let donor = sized(&sizes, 7, 2);
        assert_eq!(v.moov, donor.moov);
        let (Mem(mut card), runs, mut seams) = three_runs(&v);
        assert!(!seams.moov.items.iter().all(|it| it.size == 1000));
        let mid = runs[1];
        let noise = noise_card(400, C);
        card[mid.start as usize..(mid.start + mid.len) as usize]
            .copy_from_slice(&noise[mid.start as usize..(mid.start + mid.len) as usize]);
        let from = runs[0].len as usize;
        let at = (HEAP + 100 * C) as usize;
        card[at..at + mid.len as usize]
            .copy_from_slice(&donor.bytes[from..from + mid.len as usize]);
        let src = Mem(card);
        let found = find(
            &src,
            &mut seams,
            (HEAP, C, src.len()),
            runs[0].start,
            &Limits::DEFAULT,
        );
        let f = found.expect("the donor's run fits");
        assert!(f.complete, "{f:?}");
        assert_ne!(f.proof, Proof::Proven, "{f:?}");
    }

    /// A fragmented clip (H.265, 16 samples per fragment) on a 32 KiB-cluster noise card in 3 runs; the image, the runs
    /// and its seams (no known size).
    fn moof_runs() -> (Mem, Vec<Fragment>, MoofSeams, Vec<u8>) {
        moof_runs_of(true, 16)
    }

    /// [`moof_runs`] with samples of 2–24 KiB unless `small_nals`, `per` samples per fragment.
    fn moof_runs_of(small_nals: bool, per: usize) -> (Mem, Vec<Fragment>, MoofSeams, Vec<u8>) {
        use revenant_testkit::{VideoCodec, VideoLayout, camera_video};
        const C: u64 = 32 << 10;
        let v = camera_video(
            &AvcOptions {
                samples: 300,
                seed: 13,
                mov: false,
                mdat_pad: 0,
                small_nals,
            },
            VideoCodec::Hevc,
            VideoLayout::Fragmented {
                samples_per_fragment: per,
            },
        );
        let n = (v.bytes.len() as u64).div_ceil(C);
        let mut card = noise_card(400, C);
        let runs = scatter(&mut card, &v.bytes, C, &[300, 20, 150], &[n / 3, n / 3, 0]);
        let src = Mem(card);
        let seams = MoofSeams::new(&src, runs[0].start)
            .expect("a fragmented video")
            .with_unit(C);
        (src, runs, seams, v.bytes)
    }

    #[test]
    fn moof_video_in_three_pieces_is_found_and_proven_with_its_size() {
        // With fragments of 16 small samples a cut always has a fragment start within a cluster: never proven (Q42).
        const C: u64 = 32 << 10;
        let (src, runs, seams, bytes) = moof_runs();
        let mut seams = seams.with_known(Some(bytes.len() as u64));
        let found = find(
            &src,
            &mut seams,
            (HEAP, C, src.len()),
            runs[0].start,
            &Limits::DEFAULT,
        )
        .unwrap();
        assert_eq!(found.pieces, runs);
        assert!(found.complete, "{found:?}");
        assert_eq!(found.proof, Proof::Unproven);
        // Fragments of 64 large samples (about 850 KiB): the cuts lie inside a fragment's media data, far from its boxes.
        let (src, runs, seams, bytes) = moof_runs_of(false, 64);
        let mut seams = seams.with_known(Some(bytes.len() as u64));
        let found = find(
            &src,
            &mut seams,
            (HEAP, C, src.len()),
            runs[0].start,
            &Limits::DEFAULT,
        )
        .unwrap();
        assert_eq!(found.pieces, runs);
        assert!(
            found.complete && found.unique && found.exhaustive,
            "{found:?}"
        );
        assert_eq!(found.proof, Proof::Proven);
        // Without a size the chain has no end to reach: the true pieces, never complete.
        let (src, runs, mut seams, _) = moof_runs_of(false, 64);
        let found = find(
            &src,
            &mut seams,
            (HEAP, C, src.len()),
            runs[0].start,
            &Limits::DEFAULT,
        )
        .unwrap();
        assert_eq!(found.pieces, runs);
        assert!(!found.complete, "{found:?}");
        assert!(seams.clean(&Fragmented::new(&src, found.pieces)));
        // The runs out of order: a sample fails.
        let view = Fragmented::new(&src, vec![runs[0], runs[2], runs[1]]);
        let mut seams = seams.with_known(Some(bytes.len() as u64));
        assert_eq!(
            seams.prove(&view, &[runs[0].len, runs[0].len + runs[2].len]),
            Proof::Failed
        );
    }

    #[test]
    fn moof_prefilter_checks_what_the_walk_expected_past_the_seam() {
        let (src, runs, mut seams, _) = moof_runs();
        let seam = runs[0].len;
        // The walk of the first piece learns what comes next (the samples of the fragment it was in).
        let view = Fragmented::new(&src, vec![runs[0]]);
        let w = seams.walk(&view, &[], &MoofState::default(), seam);
        assert!(matches!(w.end, WalkEnd::Broken { at } if at == seam));
        let mut r = Reader::with_window(&src, SECTOR as usize);
        assert!(
            seams.prefilter(&mut r, runs[1].start, seam),
            "the true continuation"
        );
        assert!(
            !seams.prefilter(&mut r, HEAP + 100 * (32 << 10), seam),
            "noise"
        );
        assert!(
            !seams.prefilter(&mut r, runs[2].start, seam),
            "the third run"
        );
        // Nothing expected that far: blind, kept.
        assert!(seams.prefilter(&mut r, HEAP + 100 * (32 << 10), seam + (1 << 30)));
        assert!(!seams.memo_prefilter());
    }

    #[test]
    fn moof_walk_is_bounded_by_box_count() {
        // C9: the head of a fragmented clip, then more tiny `free` boxes than the cap: the walk stops undecided, after
        // at most `MAX_BOXES` headers.
        let (src, runs, _, bytes) = moof_runs();
        let _ = (src, runs);
        let seams = MoofSeams::new(&Mem(bytes.clone()), 0).unwrap();
        let head = seams.head.len();
        let mut file = bytes[..head].to_vec();
        file.extend_from_slice(&8u32.to_be_bytes());
        file.extend_from_slice(b"styp");
        for _ in 0..MAX_BOXES + 100 {
            file.extend_from_slice(&8u32.to_be_bytes());
            file.extend_from_slice(b"free");
        }
        let src = Mem(file);
        let mut seams = MoofSeams::new(&src, 0).unwrap();
        let w = seams.walk(&src, &[], &MoofState::default(), src.len());
        assert!(matches!(w.end, WalkEnd::Stopped));
        assert!(w.checked <= (head + 8 * (MAX_BOXES + 1)) as u64);
        // A garbled `moof` (a size past it, a trun past its box) never panics: the walk breaks there.
        let mut file = bytes.clone();
        let moof = file[head..].windows(4).position(|w| w == b"moof").unwrap() + head;
        let traf = file[moof..].windows(4).position(|w| w == b"traf").unwrap() + moof;
        file[traf - 4..traf].copy_from_slice(&u32::MAX.to_be_bytes());
        let src = Mem(file);
        let mut seams = MoofSeams::new(&src, 0).unwrap();
        let w = seams.walk(&src, &[], &MoofState::default(), src.len());
        assert!(matches!(w.end, WalkEnd::Broken { .. }));
    }

    /// A box `kind` holding `body`.
    fn bx(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut b = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        b.extend_from_slice(kind);
        b.extend_from_slice(body);
        b
    }

    /// `out` padded by a `free` box to a whole number of `c`-byte units.
    fn pad_to(out: &mut Vec<u8>, c: usize) {
        let r = out.len() % c;
        if r != 0 {
            let mut p = c - r;
            if p < 8 {
                p += c;
            }
            out.extend(bx(b"free", &vec![0u8; p - 8]));
        }
    }

    /// A fragmented clip (H.265, 300 samples of seed `seed`, 16 per fragment) relaid as parts padded to whole 32 KiB
    /// units: the head (`ftyp`, `moov`), then each fragment (`moof`, `mdat`); the parts and the `moof` offsets in the file.
    fn padded_parts(seed: u64) -> (Vec<Vec<u8>>, Vec<u64>) {
        use revenant_testkit::{VideoCodec, VideoLayout, camera_video};
        const C: usize = 32 << 10;
        let v = camera_video(
            &AvcOptions {
                samples: 300,
                seed,
                mov: false,
                mdat_pad: 0,
                small_nals: true,
            },
            VideoCodec::Hevc,
            VideoLayout::Fragmented {
                samples_per_fragment: 16,
            },
        );
        let b = &v.bytes;
        let mut boxes = Vec::new();
        let mut pos = 0;
        while pos + 8 <= b.len() {
            let size = u32::from_be_bytes(b[pos..pos + 4].try_into().unwrap()) as usize;
            boxes.push((b[pos + 4..pos + 8].to_vec(), pos, pos + size));
            pos += size;
        }
        let first = boxes.iter().position(|x| x.0 == b"moof").unwrap();
        let mut head = b[..boxes[first].1].to_vec();
        pad_to(&mut head, C);
        let mut parts = vec![head];
        let mut moofs = Vec::new();
        let mut at = parts[0].len() as u64;
        for w in boxes[first..].chunks(2) {
            let mut part = b[w[0].1..w[1].2].to_vec();
            pad_to(&mut part, C);
            moofs.push(at);
            at += part.len() as u64;
            parts.push(part);
        }
        (parts, moofs)
    }

    /// An `mfra` whose `tfra` (track 1, version 0) lists `moofs` (none: no `tfra`), then its `mfro`.
    fn mfra(moofs: Option<&[u64]>) -> Vec<u8> {
        let tfra = moofs.map(|m| {
            let mut body = vec![0u8; 4];
            body.extend_from_slice(&1u32.to_be_bytes());
            body.extend_from_slice(&[0, 0, 0, 0]);
            body.extend_from_slice(&(m.len() as u32).to_be_bytes());
            for (i, &o) in m.iter().enumerate() {
                body.extend_from_slice(&(i as u32 * 16).to_be_bytes());
                body.extend_from_slice(&(o as u32).to_be_bytes());
                body.extend_from_slice(&[1, 1, 1]);
            }
            bx(b"tfra", &body)
        });
        let tfra = tfra.unwrap_or_default();
        let size = (8 + tfra.len() + 16) as u32;
        let mut mfro = vec![0u8; 4];
        mfro.extend_from_slice(&size.to_be_bytes());
        bx(b"mfra", &[tfra, bx(b"mfro", &mfro)].concat())
    }

    /// Clip A of `parts` (each one unit; the last one followed by `tail`) on a 32 KiB-cluster noise card: the head and
    /// fragments 0–3 at cluster 300, fragment 4 at cluster 20 when `own4`, the rest at cluster 150; `other` written at
    /// cluster 100. The image and A's runs.
    fn a_card(
        parts: &[Vec<u8>],
        tail: &[u8],
        own4: bool,
        other: Option<&[u8]>,
    ) -> (Mem, Vec<Fragment>) {
        const C: u64 = 32 << 10;
        assert!(parts.iter().all(|p| p.len() as u64 == C));
        let mut card = noise_card(400, C);
        let mut put = |cluster: u64, bytes: &[u8]| {
            let at = (HEAP + cluster * C) as usize;
            card[at..at + bytes.len()].copy_from_slice(bytes);
            Fragment {
                start: at as u64,
                len: bytes.len() as u64,
            }
        };
        let r1 = put(300, &parts[..5].concat());
        let r2 = if own4 { Some(put(20, &parts[5])) } else { None };
        let r3 = put(150, &[parts[6..].concat(), tail.to_vec()].concat());
        if let Some(o) = other {
            put(100, o);
        }
        let runs = [Some(r1), r2, Some(r3)].into_iter().flatten().collect();
        (Mem(card), runs)
    }

    #[test]
    fn a_fragment_of_another_clip_on_box_boundaries_is_not_intact() {
        // Two clips of one camera (seeds 13, 99), fragments padded to whole clusters, A closed by an `mfra`. A's
        // fragment 4 is lost; B's fragment 0 (sequence 1 where 5 is due) lies at cluster 100: every seam on a box
        // boundary, B's fragment checks itself.
        const C: u64 = 32 << 10;
        let (a, moofs) = padded_parts(13);
        let (b, _) = padded_parts(99);
        let (src, runs) = a_card(&a, &mfra(Some(&moofs)), false, Some(&b[1]));
        let mut seams = MoofSeams::new(&src, runs[0].start).unwrap().with_unit(C);
        let found = find(
            &src,
            &mut seams,
            (HEAP, C, src.len()),
            runs[0].start,
            &Limits::DEFAULT,
        );
        if let Some(f) = &found {
            assert!(
                !(f.complete && f.unique && f.exhaustive && f.proof == Proof::Proven),
                "{f:?}"
            );
        }
        // A's own fragment in place: the right pieces, its `tfra` agreeing; every seam on a fragment boundary, so never
        // proven by content (Q42): at most `Partial`.
        let (src, runs) = a_card(&a, &mfra(Some(&moofs)), true, Some(&b[1]));
        let mut seams = MoofSeams::new(&src, runs[0].start).unwrap().with_unit(C);
        let found = find(
            &src,
            &mut seams,
            (HEAP, C, src.len()),
            runs[0].start,
            &Limits::DEFAULT,
        )
        .unwrap();
        assert_eq!(found.pieces, runs);
        assert!(
            found.complete && found.unique && found.exhaustive,
            "{found:?}"
        );
        assert_eq!(found.proof, Proof::Unproven);
        // A `tfra` that puts one `moof` where none is refutes the placement.
        let mut wrong = moofs.clone();
        wrong[5] += 8;
        let (src, runs) = a_card(&a, &mfra(Some(&wrong)), true, None);
        let view = Fragmented::new(&src, runs.clone());
        let mut seams = MoofSeams::new(&src, runs[0].start).unwrap().with_unit(C);
        let cuts = [runs[0].len, runs[0].len + runs[1].len];
        assert_eq!(seams.prove(&view, &cuts), Proof::Failed);
    }

    /// A source of `head` then zeros up to `len`, counting the bytes read.
    struct Zeros {
        head: Vec<u8>,
        len: u64,
        read: std::sync::atomic::AtomicU64,
    }

    impl Source for Zeros {
        fn len(&self) -> u64 {
            self.len
        }
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, crate::EngineError> {
            let n = buf.len().min(self.len.saturating_sub(offset) as usize);
            for (i, b) in buf[..n].iter_mut().enumerate() {
                *b = self.head.get(offset as usize + i).copied().unwrap_or(0);
            }
            self.read
                .fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
            Ok(n)
        }
        fn volume(&self) -> Option<crate::source::VolumeId> {
            None
        }
    }

    #[test]
    fn a_huge_moof_is_never_loaded() {
        // A `moof` declaring 1 GiB right after the head: refused as garbled at its header, far fewer bytes read.
        let (a, _) = padded_parts(13);
        let mut head = a[0].clone();
        head.extend_from_slice(&(1u32 << 30).to_be_bytes());
        head.extend_from_slice(b"moof");
        let src = Zeros {
            head,
            len: 2 << 30,
            read: Default::default(),
        };
        let mut seams = MoofSeams::new(&src, 0).unwrap();
        let before = src.read.load(std::sync::atomic::Ordering::Relaxed);
        let from = seams.begin();
        let run = seams.run(&src, &from, src.len(), false, &mut |_, _, _| {});
        let read = src.read.load(std::sync::atomic::Ordering::Relaxed) - before;
        assert!(matches!(run.end, WalkEnd::Broken { .. }) && run.failed);
        assert!(read < 1 << 20, "{read} bytes read");
    }

    #[test]
    fn empty_samples_spend_the_walk_steps() {
        // IMPORTANT 1: `moof`s listing millions of zero-size samples (default size 0, no per-sample field), each with an
        // empty `mdat`: every listed sample costs a step, so a walk with few steps left stops undecided.
        let (a, _) = padded_parts(13);
        let mut file = a[0].clone();
        for seq in 1..=4u32 {
            let mut tfhd = 0x02_0000u32.to_be_bytes().to_vec();
            tfhd.extend_from_slice(&1u32.to_be_bytes());
            let mut trun = 0x1u32.to_be_bytes().to_vec();
            trun.extend_from_slice(&3_999_999u32.to_be_bytes());
            trun.extend_from_slice(&0i32.to_be_bytes());
            let mut mfhd = vec![0u8; 4];
            mfhd.extend_from_slice(&seq.to_be_bytes());
            let traf = bx(b"traf", &[bx(b"tfhd", &tfhd), bx(b"trun", &trun)].concat());
            file.extend(bx(b"moof", &[bx(b"mfhd", &mfhd), traf].concat()));
            file.extend(bx(b"mdat", &[]));
        }
        let src = Mem(file);
        let mut seams = MoofSeams::new(&src, 0).unwrap();
        seams.steps = 1 << 20;
        let from = seams.begin();
        let w = seams.walk(&src, &[], &from, src.len());
        assert!(matches!(w.end, WalkEnd::Stopped));
        assert_eq!(seams.steps, 0);
    }

    #[test]
    fn another_clips_fragment_in_its_own_place_is_not_intact() {
        // The Task 6 re-review's probe (ruling Q42): B's fragment 4 (sequence 5, the same decode time and size as A's,
        // one cluster like every padded part) at cluster 100, A's own fragment 4 lost, A's `tfra` right. Sequence, time
        // and position agree; only the seams tell, and they lie on fragment boundaries.
        const C: u64 = 32 << 10;
        let (a, moofs) = padded_parts(13);
        let (b, _) = padded_parts(99);
        let (src, runs) = a_card(&a, &mfra(Some(&moofs)), false, Some(&b[5]));
        let mut seams = MoofSeams::new(&src, runs[0].start).unwrap().with_unit(C);
        let found = find(
            &src,
            &mut seams,
            (HEAP, C, src.len()),
            runs[0].start,
            &Limits::DEFAULT,
        );
        if let Some(f) = &found {
            assert!(
                !(f.complete && f.unique && f.exhaustive && f.proof == Proof::Proven),
                "{f:?}"
            );
        }
    }

    /// A `Partial` finding of `bytes`' length at `start`, in `pieces` (none: one piece).
    fn partial_at(start: u64, len: u64, pieces: Vec<Fragment>) -> Finding {
        Finding {
            id: 1,
            start,
            len,
            kind: FileKind::Mp4,
            health: Health::Partial,
            fragments: pieces,
            origin: None,
        }
    }

    #[test]
    fn a_placement_its_content_does_not_refute_is_not_replaced() {
        // Ruling Q43: a complete `Partial` reassembly of the same length replaces an old `Partial` placement only when
        // the old pieces, walked by the same seams, fail before their end.
        const C: u64 = 32 << 10;
        let (a, _) = padded_parts(13);
        let clip = a.concat();
        let len = clip.len() as u64;
        // The clip whole at cluster 0 of a card, and a complete `Partial` reassembly in other pieces.
        let mut card = noise_card(400, C);
        card[HEAP as usize..(HEAP + len) as usize].copy_from_slice(&clip);
        let src = Mem(card);
        let old = partial_at(HEAP, len, Vec::new());
        let mut new = old.clone();
        new.fragments = vec![
            Fragment {
                start: HEAP,
                len: 5 * C,
            },
            Fragment {
                start: HEAP + 200 * C,
                len: len - 5 * C,
            },
        ];
        let mut seams = MoofSeams::new(&src, HEAP).unwrap().with_unit(C);
        assert!(
            !replaces(&old, &new, true, &src, &mut seams),
            "the old pieces pass"
        );
        // The same old placement with other data from its sixth cluster on: refuted, replaced.
        let mut card = src.0.clone();
        card[(HEAP + 5 * C) as usize..(HEAP + 6 * C) as usize].fill(0x5A);
        let src = Mem(card);
        let mut seams = MoofSeams::new(&src, HEAP).unwrap().with_unit(C);
        assert!(
            replaces(&old, &new, true, &src, &mut seams),
            "the old pieces fail"
        );
        // Never a better health, a shorter or longer one, the same pieces or an incomplete one.
        assert!(!replaces(&old, &old, true, &src, &mut seams));
        assert!(!replaces(&old, &new, false, &src, &mut seams));
    }

    #[test]
    fn a_moof_per_track_is_walked_through() {
        // An audio-only `moof` (track 2) after each video fragment, the sequence renumbered: the walk follows the chain
        // to the known size.
        let (a, _) = padded_parts(13);
        let clip = a.concat();
        let mut out = Vec::new();
        let mut pos = 0;
        let mut seq = 0u32;
        let be =
            |b: &[u8], at: usize| u32::from_be_bytes(b[at..at + 4].try_into().unwrap()) as usize;
        while pos < clip.len() {
            let size = be(&clip, pos);
            let kind = clip[pos + 4..pos + 8].to_vec();
            let mut bytes = clip[pos..pos + size].to_vec();
            if kind == b"moof" {
                seq += 1;
                bytes[8 + 12..8 + 16].copy_from_slice(&seq.to_be_bytes());
            }
            out.extend_from_slice(&bytes);
            if kind == b"mdat" {
                seq += 1;
                let mut mfhd = vec![0u8; 4];
                mfhd.extend_from_slice(&seq.to_be_bytes());
                let mut tfhd = 0x02_0000u32.to_be_bytes().to_vec();
                tfhd.extend_from_slice(&2u32.to_be_bytes());
                let build = |off: i32| {
                    let mut trun = 0x201u32.to_be_bytes().to_vec();
                    trun.extend_from_slice(&1u32.to_be_bytes());
                    trun.extend_from_slice(&off.to_be_bytes());
                    trun.extend_from_slice(&100u32.to_be_bytes());
                    let traf = bx(b"traf", &[bx(b"tfhd", &tfhd), bx(b"trun", &trun)].concat());
                    bx(b"moof", &[bx(b"mfhd", &mfhd), traf].concat())
                };
                let n = build(0).len() as i32;
                out.extend(build(n + 8));
                out.extend(bx(b"mdat", &[0x5A; 100]));
            }
            pos += size;
        }
        let src = Mem(out.clone());
        let mut seams = MoofSeams::new(&src, 0)
            .unwrap()
            .with_known(Some(out.len() as u64));
        let from = seams.begin();
        let w = seams.walk(&src, &[], &from, src.len());
        assert!(matches!(w.end, WalkEnd::Complete { len } if len == out.len() as u64));
    }
}
