//! Seam search: where the next piece of a fragmented file continues among the free units of a volume.
//!
//! A file found in pieces is walked by its format ([`Seams`]) over a logical view of the pieces chosen so far. Where the
//! walk breaks, the search tries the break points of the current piece ([`Limits::break_points`], nearest the break
//! first) and, for each, the free unit starts where the next piece could begin ([`Space::candidates`]: next free
//! forward within the window, then, on a cluster grid, the first hole from the heap start). A candidate passes when the
//! walk over it shows content evidence past the seam; a candidate with no evidence either way (blind) is tried too, after
//! the evident ones, and its complete solutions count toward uniqueness (ruling Q2). Once one complete solution is
//! found, a separate re-check with its own budget tries earlier break points of every piece of it (the format's most
//! suspicious ones first) for another complete solution (Q26, Q28). The window bounds the distance to a candidate, not a
//! piece's length (Q25). The search is bounded by
//! [`Limits`] and a [`Budget`]; any cap or budget hit makes it not exhaustive, and the file can then be at most
//! `Partial`.
//!
//! Every read goes through the source the caller gives (the scan's `CachedSource`, which never reads a known-bad sector
//! again), counted by a private wrapper so each file's reads are charged to its budget.
//!
//! The scan's reassembly step ([`reassemble`]) runs this search, after the pairing, for every candidate the scan kept
//! ([`is_candidate`]), in id order, and turns each result that improves the finding into an `Updated` ([`improves`]),
//! with the health of [`health`].
// Some helpers serve the unit tests only (and the scan reads some limits through their constants).
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::EngineError;
use crate::cache::{CachedSource, Runs};
use crate::filesystem::{FsState, Placement};
use crate::formats::jpeg::decode_health;
use crate::formats::jpeg_join::MIN_FIRST_PIECE;
use crate::formats::{CancelScope, FileKind, Fragment, Health, Reader, SECTOR, cancelled};
pub(crate) use crate::pairing::Grid;
use crate::pairing::grid_of;
use crate::scan::{CancelToken, Finding};
use crate::source::{DiskId, Fragmented, Source, VolumeId};

pub(crate) mod jpeg;
pub(crate) mod raw;
pub(crate) mod video;

/// Most pieces of a file on a cluster grid.
pub(crate) const MAX_PIECES: usize = 16;
/// Most pieces of a file off any grid (512-byte sectors).
pub(crate) const MAX_PIECES_NO_GRID: usize = 4;
/// Most candidates (prefilter survivors) per seam; more means the seam was not exhausted.
pub(crate) const SEAM_CANDIDATES: usize = 4096;
/// Bytes after a piece's end where the next piece may start, on a grid (and from the heap start for the first hole).
pub(crate) const SEAM_WINDOW: u64 = 256 << 20;
/// The same off any grid: the 1.3 JPEG gap window (`jpeg_join::JPEG_GAP_MAX`).
pub(crate) const NO_GRID_WINDOW: u64 = 64 << 20;
/// Break points tried per seam; more means the seam was not exhausted.
pub(crate) const BREAK_POINTS: usize = 64;
/// Candidate walks per file.
pub(crate) const FILE_TRIES: u64 = 1 << 16;
/// Candidate walks per scan.
pub(crate) const SCAN_TRIES: u64 = 1 << 22;
/// Candidate walks per file for the uniqueness re-check of a found solution (Q28), apart from [`FILE_TRIES`].
pub(crate) const RECHECK_TRIES: u64 = 1 << 14;
/// Bytes asked of the source per file.
pub(crate) const FILE_READ: u64 = 512 << 20;
/// Bytes asked of the source per scan.
pub(crate) const SCAN_READ: u64 = 16 << 30;
/// Findings kept as reassembly candidates per scan.
pub(crate) const MAX_CANDIDATES: usize = 4096;
/// Runs of bytes claimed by `Intact` findings kept per scan.
pub(crate) const MAX_CLAIMED_RUNS: usize = 1 << 18;
/// A file's own embedded files the search takes into account (D10); past it, none (conservative: they then stop its
/// free runs like any claimed file).
pub(crate) const MAX_OWN: usize = 64;
/// Candidate units listed (prefilter calls, answered from memory or not) per try charged to the file's budget.
const LISTED_PER_TRY: u64 = 64;
/// Prefilter answers remembered per file (past it they are asked again, and charged again).
const PREFILTER_MEMO: usize = 1 << 16;

/// Every cap and budget of the reassembly. Unit tests in the crate lower them through a value of their own.
#[derive(Clone, Debug)]
pub(crate) struct Limits {
    pub pieces: usize,
    pub pieces_no_grid: usize,
    pub seam_candidates: usize,
    pub seam_window: u64,
    pub no_grid_window: u64,
    pub break_points: usize,
    pub file_tries: u64,
    pub scan_tries: u64,
    pub recheck_tries: u64,
    pub file_read: u64,
    pub scan_read: u64,
    pub candidates: usize,
    pub claimed_runs: usize,
    pub own_files: usize,
}

impl Limits {
    pub(crate) const DEFAULT: Limits = Limits {
        pieces: MAX_PIECES,
        pieces_no_grid: MAX_PIECES_NO_GRID,
        seam_candidates: SEAM_CANDIDATES,
        seam_window: SEAM_WINDOW,
        no_grid_window: NO_GRID_WINDOW,
        break_points: BREAK_POINTS,
        file_tries: FILE_TRIES,
        scan_tries: SCAN_TRIES,
        recheck_tries: RECHECK_TRIES,
        file_read: FILE_READ,
        scan_read: SCAN_READ,
        candidates: MAX_CANDIDATES,
        claimed_runs: MAX_CLAIMED_RUNS,
        own_files: MAX_OWN,
    };
}

/// Bytes no piece may use: `Listing.live` and the pieces of every `Intact` finding (and `Intact` reassembly so far).
#[derive(Clone)]
pub(crate) struct Taken {
    live: Runs,
    claimed: Runs,
}

impl Taken {
    pub(crate) fn new(live: &[Fragment], claimed: &Runs) -> Taken {
        let mut runs = Runs::default();
        for f in live.iter().filter(|f| f.len > 0) {
            runs.add(f.start, f.start.saturating_add(f.len), usize::MAX);
        }
        Taken {
            live: runs,
            claimed: claimed.clone(),
        }
    }

    pub(crate) fn claim(&mut self, pieces: &[Fragment]) {
        for f in pieces.iter().filter(|f| f.len > 0) {
            self.claimed
                .add(f.start, f.start.saturating_add(f.len), usize::MAX);
        }
    }

    /// The claimed `Intact` ranges overlapping `[start, end)`, ascending. `Runs` merges touching ranges, so these are
    /// merged ranges, not one finding's piece: the caller tests coverage by `own` (Q7), never equality.
    pub(crate) fn overlapping(&self, start: u64, end: u64) -> Vec<Fragment> {
        let mut out = Vec::new();
        let mut pos = start;
        while pos < end {
            match self.claimed.first_ending_after(pos) {
                Some((s, e)) if s < end => {
                    out.push(Fragment {
                        start: s,
                        len: e - s,
                    });
                    pos = e;
                }
                _ => break,
            }
        }
        out
    }
}

/// Where the pieces of one file may lie: a volume's cluster grid when the file starts on one (cluster ≥ 4096), else
/// 512-byte sectors, forward only, with a smaller window and fewer pieces.
pub(crate) struct Space<'a> {
    taken: &'a Taken,
    /// Starts of the scan's findings, ascending.
    starts: &'a [u64],
    /// Units are `base + k × unit`.
    base: u64,
    unit: u64,
    /// The heap end (grid) or the source end, whichever comes first.
    end: u64,
    grid: bool,
    window: u64,
    max_pieces: usize,
}

impl<'a> Space<'a> {
    /// For a file starting at `start`: the grid of `grids` holding it at a cluster start (cluster ≥ 4096), else none.
    /// `starts` must be ascending.
    pub(crate) fn new(
        grids: &[Grid],
        start: u64,
        source_len: u64,
        taken: &'a Taken,
        starts: &'a [u64],
        limits: &Limits,
    ) -> Space<'a> {
        debug_assert!(starts.is_sorted());
        match grid_of(grids, start) {
            Some((heap, cluster, heap_end)) => Space {
                taken,
                starts,
                base: heap,
                unit: cluster,
                end: heap_end.min(source_len),
                grid: true,
                window: limits.seam_window,
                max_pieces: limits.pieces,
            },
            None => Space {
                taken,
                starts,
                base: 0,
                unit: SECTOR,
                end: source_len,
                grid: false,
                window: limits.no_grid_window,
                max_pieces: limits.pieces_no_grid,
            },
        }
    }

    /// Cluster size, or [`SECTOR`].
    pub(crate) fn unit(&self) -> u64 {
        self.unit
    }

    pub(crate) fn on_grid(&self) -> bool {
        self.grid
    }

    pub(crate) fn max_pieces(&self) -> usize {
        self.max_pieces
    }

    /// The unit start at or before `p` (`base` for `p` before it).
    fn floor(&self, p: u64) -> u64 {
        let p = p.max(self.base);
        p - (p - self.base) % self.unit
    }

    /// The unit start at or after `p`.
    fn ceil(&self, p: u64) -> u64 {
        let f = self.floor(p);
        if f >= p {
            f
        } else {
            f.saturating_add(self.unit)
        }
    }

    fn is_unit_start(&self, p: u64) -> bool {
        p >= self.base && (p - self.base).is_multiple_of(self.unit)
    }

    /// The range of the taken bytes, `used` or a finding start (a one-byte range) holding `pos` or the first one after
    /// it: the earliest start.
    fn next_block(&self, pos: u64, used: &[Fragment]) -> Option<(u64, u64)> {
        let mut best: Option<(u64, u64)> = None;
        let mut consider = |r: (u64, u64)| {
            if best.is_none_or(|b| r.0 < b.0) {
                best = Some(r);
            }
        };
        if let Some(r) = self.taken.live.first_ending_after(pos) {
            consider(r);
        }
        if let Some(r) = self.taken.claimed.first_ending_after(pos) {
            consider(r);
        }
        for p in used {
            let e = p.start.saturating_add(p.len);
            if p.len > 0 && e > pos {
                consider((p.start, e));
            }
        }
        let i = self.starts.partition_point(|&s| s < pos);
        if let Some(&s) = self.starts.get(i) {
            consider((s, s + 1));
        }
        best
    }

    /// Bytes free in a row from `g` (whole units, at most `max`): stops at a taken range (unless every claimed byte of it
    /// is covered by `own` ranges at their exact absolute positions: a coverage test, not equality, because `Runs` merges
    /// touching ranges: Q7), at a range of `used`, at a finding start other than `g` itself or an `own` start (D9, D10),
    /// at the heap end or the source end. Steps over taken ranges by range, never per cluster (a 2 TB heap).
    pub(crate) fn free_run(&self, g: u64, used: &[Fragment], own: &[Fragment], max: u64) -> u64 {
        if g < self.base || g >= self.end || max == 0 {
            return 0;
        }
        // Units overlapping a stop are not free: look up to the end of the unit holding the run's last byte.
        let horizon = self.ceil(g.saturating_add(max)).min(self.end);
        let mut stop = horizon;
        if let Some((s, _)) = self.taken.live.first_ending_after(g) {
            stop = stop.min(s.max(g));
        }
        let mut pos = g;
        while let Some((s, e)) = self.taken.claimed.first_ending_after(pos) {
            if s >= stop {
                break;
            }
            if covered(own, s, e) {
                pos = e;
                continue;
            }
            stop = stop.min(s.max(g));
            break;
        }
        for p in used.iter().filter(|p| p.len > 0) {
            if p.start.saturating_add(p.len) > g {
                stop = stop.min(p.start.max(g));
            }
        }
        let i = self.starts.partition_point(|&s| s <= g);
        if let Some(&s) = self.starts[i..]
            .iter()
            .find(|&&s| !own.iter().any(|o| o.start == s))
        {
            stop = stop.min(s);
        }
        let stop = if stop == horizon {
            stop
        } else {
            self.floor(stop).max(g)
        };
        (stop - g).min(max)
    }

    /// Candidate starts after a piece ending at `end` (D4): free unit starts ascending after `end` (never the unit right
    /// at `end`: that is no seam) up to `end + window`,
    /// then (grid only) from the heap start up to `end` (at most `window` from the heap start); never inside `used`, a
    /// taken range, or a unit holding a finding start. `keep` (the seam's prefilter, which [`search`] wraps so that its
    /// reads are charged to the file's read budget and memoized per file by (absolute start, logical seam)) is applied
    /// WHILE enumerating, and `cap` counts the survivors only (Q1). The `bool` is `true` when more survivors existed
    /// than `cap` ("cap hit"); a window cut is NOT a cap hit (Q4).
    pub(crate) fn candidates(
        &self,
        end: u64,
        used: &[Fragment],
        cap: usize,
        keep: &mut dyn FnMut(u64) -> bool,
    ) -> (Vec<u64>, bool) {
        self.candidates_with_own(end, used, &[], 0, cap, keep, &mut 0)
    }

    /// [`Space::candidates`] for the seam at logical `seam` of a file whose own embedded files are `own` (logical, D10):
    /// a unit taken only by claimed ranges and finding starts that those files cover at their exact places, were the
    /// piece to start there, is a candidate too (D9, Q7: a coverage test, as in [`Space::free_run`]). `work` counts those
    /// checks (one per taken range met, never one per unit), for the caller to charge.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn candidates_with_own(
        &self,
        end: u64,
        used: &[Fragment],
        own: &[Fragment],
        seam: u64,
        cap: usize,
        keep: &mut dyn FnMut(u64) -> bool,
        work: &mut u64,
    ) -> (Vec<u64>, bool) {
        let mut out = Vec::new();
        let forward_to = end.saturating_add(self.window).min(self.end);
        // The unit right at `end` would continue the piece contiguously: no seam, never a candidate.
        let from = if self.is_unit_start(end) {
            end.saturating_add(self.unit)
        } else {
            self.ceil(end)
        };
        if self.list(
            from,
            forward_to,
            used,
            (own, seam, &mut *work),
            cap,
            keep,
            &mut out,
        ) {
            return (out, true);
        }
        if self.grid {
            let wrap_to = end.min(self.base.saturating_add(self.window));
            if self.list(
                self.base,
                wrap_to,
                used,
                (own, seam, work),
                cap,
                keep,
                &mut out,
            ) {
                return (out, true);
            }
        }
        (out, false)
    }

    /// Appends the free unit starts in `[from, to)` that `keep` accepts; `true` on the first survivor past `cap`. A unit
    /// taken only by the file's own files at their places (`own`: logical ranges and the seam) counts as free.
    #[allow(clippy::too_many_arguments)]
    fn list(
        &self,
        from: u64,
        to: u64,
        used: &[Fragment],
        (own, seam, work): (&[Fragment], u64, &mut u64),
        cap: usize,
        keep: &mut dyn FnMut(u64) -> bool,
        out: &mut Vec<u64>,
    ) -> bool {
        let mut u = from;
        while u < to {
            let free_to = match self.next_block(u, used) {
                Some((s, e)) if s < u.saturating_add(self.unit) => {
                    let mine = own_at(own, u, seam);
                    if !mine.is_empty() {
                        *work += 1;
                    }
                    if !mine.is_empty() && self.free_run(u, used, &mine, 1) > 0 {
                        if keep(u) {
                            if out.len() == cap {
                                return true;
                            }
                            out.push(u);
                        }
                        u += self.unit;
                        continue;
                    }
                    u = self.ceil(e).max(u + self.unit);
                    continue;
                }
                Some((s, _)) => self.floor(s).min(to),
                None => to,
            };
            while u < free_to {
                if keep(u) {
                    if out.len() == cap {
                        return true;
                    }
                    out.push(u);
                }
                u += self.unit;
            }
        }
        false
    }
}

/// A file's own embedded files as the search uses them: none past [`Limits::own_files`] (I-3).
pub(crate) fn own_within(own: Vec<Fragment>, limits: &Limits) -> Vec<Fragment> {
    if own.len() > limits.own_files {
        Vec::new()
    } else {
        own
    }
}

/// The embedded files `own` (logical) that would lie in a piece starting at absolute `g`, logical `l`.
fn own_at(own: &[Fragment], g: u64, l: u64) -> Vec<Fragment> {
    own.iter()
        .filter(|o| o.start >= l)
        .map(|o| Fragment {
            start: g + (o.start - l),
            len: o.len,
        })
        .collect()
}

/// Whether the ranges of `own` together hold every byte of `[start, end)`.
fn covered(own: &[Fragment], start: u64, end: u64) -> bool {
    let mut own = own.to_vec();
    own.sort_by_key(|o| o.start);
    let mut pos = start;
    for o in own {
        if o.start > pos {
            break;
        }
        pos = pos.max(o.start.saturating_add(o.len));
        if pos >= end {
            return true;
        }
    }
    pos >= end
}

/// A format's view of its own content, walked across seams.
pub(crate) trait Seams {
    type State: Clone;
    /// The state before the file's first byte (logical 0).
    fn begin(&mut self) -> Self::State;
    /// Walks the file's logical bytes `view` (the pieces so far, then a candidate run) from `from` up to `limit`.
    /// `seams`: the logical positions where one piece of `view` ends and the next begins, ascending (a format whose
    /// evidence is measured from a seam, such as the JPEG's MCU row below it, needs to know where they are).
    fn walk(
        &mut self,
        view: &dyn Source,
        seams: &[u64],
        from: &Self::State,
        limit: u64,
    ) -> Walked<Self::State>;
    /// Final proof over the whole file, `seams` = logical positions where one piece ends and the next begins.
    fn prove(&mut self, view: &dyn Source, seams: &[u64]) -> Proof;
    /// The file's length when the format says it (moov sample table, TIFF extent).
    fn known_len(&self) -> Option<u64> {
        None
    }
    /// Logical offset `l` that must lie at absolute `p` (a video's orphan `moov`).
    fn anchor(&self) -> Option<(u64, u64)> {
        None
    }
    /// Logical `[offset, len)` ranges of the file's own embedded files (D10).
    fn own(&self) -> Vec<Fragment> {
        Vec::new()
    }
    /// A cheap reject of candidate `g` for the seam at logical `seam` (default: keep). Called while the candidates are
    /// listed (Q1); `r` reads through the counted source, so its bytes are charged to the file's read budget.
    fn prefilter(&mut self, r: &mut Reader, g: u64, seam: u64) -> bool {
        let _ = (r, g, seam);
        true
    }
    /// Logical positions inside `(start, end)`, a piece of a found solution walked as `view`, where the content looks
    /// least continuous, most suspicious first (Q28): the uniqueness re-check cuts the piece first at the unit boundary
    /// at or before each (positions in the same unit count once), then goes on nearest to the piece's end, at most
    /// [`Limits::break_points`] per piece in all. The re-check asks it for every piece of one solution in a row.
    /// Default: none.
    fn suspicious_breaks(&mut self, view: &dyn Source, start: u64, end: u64) -> Vec<u64> {
        let _ = (view, start, end);
        Vec::new()
    }
    /// Whether [`Seams::prefilter`]'s answer depends only on the candidate and the seam, so the search may remember it
    /// per file (default). A format whose answer depends on what its earlier walks met says no: it is asked every time.
    fn memo_prefilter(&self) -> bool {
        true
    }
    /// Whether the search may stop at its first complete solution when that solution is not `Proven` (default: no).
    /// Its pieces are the result whatever comes after (the first solution in candidate order), and a second solution
    /// or the re-check could only take uniqueness away from a result that is `Partial` anyway: for a format whose
    /// seams are mostly unprovable (a RAW's sensor data), the rest of the search is work that cannot change anything.
    /// The result is then reported neither unique nor exhaustive.
    fn settles_unproven(&self) -> bool {
        false
    }
}

pub(crate) struct Walked<S> {
    pub end: WalkEnd,
    /// Logical end of the last unit the walk checked by content (its evidence).
    pub checked: u64,
    /// States where the walk may continue in another piece, as (logical position, state), ascending; the last one at
    /// or before `checked`.
    pub resume: Vec<(u64, S)>,
}

pub(crate) enum WalkEnd {
    /// The file ends at logical `len`.
    Complete { len: u64 },
    /// The content stops fitting at logical `at` (or the view ended there).
    Broken { at: u64 },
    /// The walk could not decide (cancelled, unreadable).
    Stopped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Proof {
    Proven,
    Unproven,
    Failed,
}

impl Proof {
    /// The weaker of two proofs.
    fn min(self, other: Proof) -> Proof {
        let rank = |p: Proof| match p {
            Proof::Failed => 0,
            Proof::Unproven => 1,
            Proof::Proven => 2,
        };
        if rank(other) < rank(self) {
            other
        } else {
            self
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Search {
    /// Absolute, file order; one piece = contiguous.
    pub pieces: Vec<Fragment>,
    pub len: u64,
    /// The walk reached the file's end (and the known size).
    pub complete: bool,
    /// Exactly one complete solution.
    pub unique: bool,
    /// No cap or budget hit anywhere.
    pub exhaustive: bool,
    pub proof: Proof,
}

/// Tries and bytes left, per scan.
pub(crate) struct Budget {
    tries: u64,
    read: u64,
}

impl Budget {
    pub(crate) fn new(limits: &Limits) -> Budget {
        Budget {
            tries: limits.scan_tries,
            read: limits.scan_read,
        }
    }

    pub(crate) fn spent(&self) -> bool {
        self.tries == 0 || self.read == 0
    }

    pub(crate) fn tries_left(&self) -> u64 {
        self.tries
    }

    pub(crate) fn read_left(&self) -> u64 {
        self.read
    }

    /// One file's caps: the per-file limits, within what the scan has left.
    fn for_file(&self, limits: &Limits) -> (u64, u64) {
        (
            limits.file_tries.min(self.tries),
            limits.file_read.min(self.read),
        )
    }

    fn charge(&mut self, tries: u64, read: u64) {
        self.tries = self.tries.saturating_sub(tries);
        self.read = self.read.saturating_sub(read);
    }
}

/// A source that counts the bytes read from it and refuses reads once `limit` bytes were read (so one walk cannot run
/// past the file's read budget; the search is then not exhaustive).
struct Counted<'a> {
    inner: &'a dyn Source,
    read: AtomicU64,
    limit: AtomicU64,
    over: AtomicBool,
}

impl<'a> Counted<'a> {
    fn new(inner: &'a dyn Source) -> Self {
        Counted {
            inner,
            read: AtomicU64::new(0),
            limit: AtomicU64::new(u64::MAX),
            over: AtomicBool::new(false),
        }
    }

    fn read(&self) -> u64 {
        self.read.load(Ordering::Relaxed)
    }

    fn set_limit(&self, limit: u64) {
        self.limit.store(limit, Ordering::Relaxed);
    }

    /// Whether a read was refused for the limit.
    fn over(&self) -> bool {
        self.over.load(Ordering::Relaxed)
    }
}

impl Source for Counted<'_> {
    fn len(&self) -> u64 {
        self.inner.len()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, EngineError> {
        if self.read() >= self.limit.load(Ordering::Relaxed) {
            self.over.store(true, Ordering::Relaxed);
            return Err(EngineError::Read {
                offset,
                cause: std::io::Error::other(ReadBudgetSpent),
            });
        }
        let n = self.inner.read_at(offset, buf)?;
        self.read.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }

    fn volume(&self) -> Option<VolumeId> {
        self.inner.volume()
    }

    fn disk(&self) -> Option<DiskId> {
        self.inner.disk()
    }
}

/// Why [`Counted`] refused a read: the file's read budget is spent (not a device error, not a cancel). The search
/// reports it through [`Counted::over`], never as an error.
#[derive(Debug)]
struct ReadBudgetSpent;

impl std::fmt::Display for ReadBudgetSpent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("read_budget_spent")
    }
}

impl std::error::Error for ReadBudgetSpent {}

/// Why the search stopped before trying everything.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stop {
    Cancelled,
    Budget,
    /// A second complete solution: the result cannot be unique, nothing more to learn.
    Ambiguous,
    /// The first complete solution is not `Proven` and the format settles for it ([`Seams::settles_unproven`]).
    Settled,
}

/// A complete solution: its pieces and whether some seam on its path had no content evidence.
type Solution = (Vec<Fragment>, bool);

/// One file's search.
struct Ctx<'c, S: Seams> {
    seams: &'c mut S,
    source: &'c Counted<'c>,
    space: &'c Space<'c>,
    limits: &'c Limits,
    cancel: &'c CancelToken,
    /// The effective known length, and whether the format's and the caller's disagree.
    known: Option<u64>,
    conflict: bool,
    anchor: Option<(u64, u64)>,
    own: Vec<Fragment>,
    tries: u64,
    max_tries: u64,
    max_read: u64,
    /// Candidate units listed so far (each [`LISTED_PER_TRY`] cost a try).
    listed: u64,
    memo: HashMap<(u64, u64), bool>,
    solutions: Vec<Solution>,
    /// The partial path that checked the most logical bytes: (bytes, pieces).
    best: Option<(u64, Vec<Fragment>)>,
    exhaustive: bool,
    stop: Option<Stop>,
    /// Ranges no candidate may start in besides the path's pieces: during the re-check, the found solution's units (Q33,
    /// Q36).
    excluded: Vec<Fragment>,
    /// Set by [`Ctx::cut_short`]: the re-check's top-hinted cut was not walked to the end when it is set during it.
    short: bool,
    /// The proof of the first solution, when it was taken as it was found ([`Seams::settles_unproven`]).
    first_proof: Option<Proof>,
}

impl<'c, S: Seams> Ctx<'c, S> {
    fn new(
        seams: &'c mut S,
        source: &'c Counted<'c>,
        space: &'c Space<'c>,
        limits: &'c Limits,
        cancel: &'c CancelToken,
        (known, conflict): (Option<u64>, bool),
        (max_tries, max_read): (u64, u64),
    ) -> Self {
        let anchor = seams.anchor();
        let own = own_within(seams.own(), limits);
        Ctx {
            seams,
            source,
            space,
            limits,
            cancel,
            known,
            conflict,
            anchor,
            own,
            tries: 0,
            listed: 0,
            max_tries,
            max_read,
            memo: HashMap::new(),
            solutions: Vec::new(),
            best: None,
            exhaustive: true,
            stop: None,
            excluded: Vec::new(),
            short: false,
            first_proof: None,
        }
    }

    /// Not exhaustive because something was left untried (a cap, a budget, an undecided walk); the piece cap is not
    /// one of these: past it no alternative fits within the cap (Q34).
    fn cut_short(&mut self) {
        self.exhaustive = false;
        self.short = true;
    }

    fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled() || cancelled()
    }

    /// Takes one try from the budget; `false` (and the search stops) when cancelled or the budget is spent.
    fn charge(&mut self) -> bool {
        if self.stop.is_some() {
            return false;
        }
        if self.is_cancelled() {
            self.stop = Some(Stop::Cancelled);
            return false;
        }
        if self.tries >= self.max_tries || self.source.read() >= self.max_read {
            self.cut_short();
            self.stop = Some(Stop::Budget);
            return false;
        }
        self.tries += 1;
        true
    }

    /// The seam's prefilter for candidate `g` at logical `seam`, memoized per (absolute start, logical seam), its reads
    /// charged to the file's read budget (Q1). Listing itself is charged too: every [`LISTED_PER_TRY`] calls, answered
    /// from memory or not, cost one try, so the listing work of a file is bounded by its try budget.
    fn keep(&mut self, g: u64, seam: u64) -> bool {
        self.listed += 1;
        if self.listed.is_multiple_of(LISTED_PER_TRY) && !self.charge() {
            return false;
        }
        let memo = self.seams.memo_prefilter();
        if let Some(&v) = self.memo.get(&(g, seam)).filter(|_| memo) {
            return v;
        }
        if self.stop.is_some() {
            return false;
        }
        if self.is_cancelled() {
            self.stop = Some(Stop::Cancelled);
            return false;
        }
        if self.source.read() >= self.max_read {
            self.cut_short();
            self.stop = Some(Stop::Budget);
            return false;
        }
        let v = {
            let mut r = Reader::with_window(self.source, SECTOR as usize);
            self.seams.prefilter(&mut r, g, seam)
        };
        if memo && self.memo.len() < PREFILTER_MEMO {
            self.memo.insert((g, seam), v);
        }
        v
    }

    /// Longest piece that may start at logical `l`: what is left of the known length, else unbounded (Q25: the window
    /// bounds the distance to a candidate, not a piece; a piece runs until its walk breaks or the read budget ends).
    fn max_len(&self, l: u64) -> Option<u64> {
        match self.known {
            Some(k) if k <= l => None,
            Some(k) => Some(k - l),
            None => Some(u64::MAX),
        }
    }

    /// The file's own embedded files that would lie in a piece starting at absolute `g`, logical `l`.
    fn own_at(&self, g: u64, l: u64) -> Vec<Fragment> {
        own_at(&self.own, g, l)
    }

    /// Walks `pieces ++ [g, run]` from `from` up to `limit`, as one try.
    fn walk(
        &mut self,
        pieces: &[Fragment],
        g: u64,
        run: u64,
        from: &S::State,
        limit: u64,
    ) -> Option<Walked<S::State>> {
        if !self.charge() {
            return None;
        }
        let seams: Vec<u64> = pieces
            .iter()
            .scan(0, |end, p| {
                *end += p.len;
                Some(*end)
            })
            .collect();
        let mut v = pieces.to_vec();
        v.push(Fragment { start: g, len: run });
        let source: &dyn Source = self.source;
        let view = Fragmented::new(source, v);
        Some(self.seams.walk(&view, &seams, from, limit))
    }

    /// Records `pieces ++ [g, len]` as a partial result when it checked more bytes than the best so far.
    fn partial(&mut self, pieces: &[Fragment], g: u64, len: u64) {
        if len == 0 {
            return;
        }
        let total = pieces.iter().map(|p| p.len).sum::<u64>() + len;
        if self.best.as_ref().is_none_or(|b| total > b.0) {
            let mut p = pieces.to_vec();
            p.push(Fragment { start: g, len });
            self.best = Some((total, p));
        }
    }

    /// A walk completed at `pieces ++ [g, len]`: a solution when the known length and the anchor agree.
    fn solution(&mut self, pieces: &[Fragment], g: u64, len: u64, blind: bool) {
        let mut sol = pieces.to_vec();
        sol.push(Fragment { start: g, len });
        let total: u64 = sol.iter().map(|p| p.len).sum();
        if self.conflict || self.known.is_some_and(|k| k != total) {
            return;
        }
        if let Some((l, p)) = self.anchor {
            let source: &dyn Source = self.source;
            let view = Fragmented::new(source, sol.clone());
            if view.absolute(l, 1).first().map(|f| f.start) != Some(p) {
                return;
            }
        }
        self.solutions.push((sol, blind));
        if self.solutions.len() >= 2 {
            self.stop = Some(Stop::Ambiguous);
        } else if self.seams.settles_unproven() {
            // As the final proof does: past the read limit (its reads are still charged to the file).
            self.source.set_limit(u64::MAX);
            let source: &dyn Source = self.source;
            let proof = proof_of(&mut *self.seams, source, &self.solutions[0]);
            self.source.set_limit(self.max_read);
            self.first_proof = Some(proof);
            if proof != Proof::Proven {
                self.stop = Some(Stop::Settled);
            }
        }
    }

    /// The piece starting at absolute `g`, logical `l`, after `pieces`, walked from `from`; then its seams.
    fn dfs(
        &mut self,
        pieces: &mut Vec<Fragment>,
        from: &(u64, S::State),
        g: u64,
        l: u64,
        blind: bool,
    ) {
        if self.stop.is_some() {
            return;
        }
        let Some(max) = self.max_len(l) else {
            return;
        };
        let own = self.own_at(g, l);
        let run = self.space.free_run(g, pieces, &own, max);
        if run == 0 {
            return;
        }
        let Some(w) = self.walk(pieces, g, run, &from.1, l + run) else {
            return;
        };
        let checked = w.checked.clamp(l, l + run);
        self.partial(pieces, g, checked - l);
        match w.end {
            WalkEnd::Complete { len } => {
                if len > l && len <= l + run {
                    self.solution(pieces, g, len - l, blind);
                }
            }
            WalkEnd::Stopped => self.cut_short(),
            WalkEnd::Broken { at } => self.seams_after(pieces, from, g, l, run, &w, at, blind),
        }
    }

    /// The break points of the piece `[g, run)` at logical `l` whose walk `w` broke at `at`, and for each the next
    /// piece's candidates (D4, D5, Q2).
    #[allow(clippy::too_many_arguments)]
    fn seams_after(
        &mut self,
        pieces: &mut Vec<Fragment>,
        from: &(u64, S::State),
        g: u64,
        l: u64,
        run: u64,
        w: &Walked<S::State>,
        at: u64,
        blind: bool,
    ) {
        if at <= l {
            return;
        }
        if pieces.len() + 1 >= self.space.max_pieces() {
            self.exhaustive = false;
            return;
        }
        let floor = std::iter::once(from.0)
            .chain(w.resume.iter().map(|r| r.0))
            .filter(|&p| p <= w.checked)
            .max()
            .unwrap_or(l)
            .max(l);
        let breaks = self.breaks(g, l, floor, at.min(l + run), true);
        self.try_breaks(pieces, from, g, l, w, breaks, blind);
    }

    /// Logical unit boundaries `b` in `[low, top]` of the piece starting at absolute `g`, logical `l` (`b > l`), nearest
    /// to `top` first, at most `break_points`; past that, when `counts`, the search is not exhaustive.
    fn breaks(&mut self, g: u64, l: u64, low: u64, top: u64, counts: bool) -> Vec<u64> {
        let lowest = g + (low.max(l) - l);
        let mut abs = self.space.floor(g + (top.max(l) - l));
        let mut out = Vec::new();
        while abs > g && abs >= lowest {
            if out.len() == self.limits.break_points {
                if counts {
                    self.cut_short();
                }
                break;
            }
            out.push(l + (abs - g));
            abs -= self.space.unit();
        }
        out
    }

    /// For each break point `b` of the piece `[g, ..)` at logical `l` walked as `w`: the piece cut at `b`, then the next
    /// piece's candidates, evident ones first, then blind ones (Q2).
    #[allow(clippy::too_many_arguments)]
    fn try_breaks(
        &mut self,
        pieces: &mut Vec<Fragment>,
        from: &(u64, S::State),
        g: u64,
        l: u64,
        w: &Walked<S::State>,
        breaks: Vec<u64>,
        blind: bool,
    ) {
        for b in breaks {
            if self.stop.is_some() {
                return;
            }
            let st = w
                .resume
                .iter()
                .rev()
                .find(|r| r.0 <= b)
                .unwrap_or(from)
                .clone();
            pieces.push(Fragment {
                start: g,
                len: b - l,
            });
            let (evident, blinds) = self.classify(pieces, &st, g + (b - l), b);
            // Q2: blind candidates are descended into too, after the evident ones, and count toward uniqueness.
            for g2 in evident {
                self.dfs(pieces, &st, g2, b, blind);
            }
            for g2 in blinds {
                self.dfs(pieces, &st, g2, b, true);
            }
            pieces.pop();
        }
    }

    /// The uniqueness re-check of the one complete solution found (Q26, Q28): for every piece, earlier break points (the
    /// format's suspicious ones first, then nearest to the piece's end, at most `break_points` per piece, taken round
    /// robin over the pieces), each followed by the next piece's candidates with content evidence past the new seam
    /// (Q29: blind ones are not descended into here) and the search from there. Any other complete solution makes the
    /// result not unique. It spends its own `max_tries` and never changes `exhaustive`:
    /// running out of it or of break points leaves the result as the search left it.
    fn recheck(&mut self, max_tries: u64) {
        let Some((sol, _)) = self.solutions.first().cloned() else {
            return;
        };
        let saved = self.exhaustive;
        self.max_tries = self.tries.saturating_add(max_tries);
        let mut starts = Vec::with_capacity(sol.len());
        let mut l = 0;
        for p in &sol {
            starts.push(l);
            l += p.len;
        }
        let total = l;
        let mut lists = Vec::with_capacity(sol.len());
        // Q33: the top-hinted cut of each piece (when the format gives hints), whose candidates must all be walked.
        let mut top: Vec<Option<u64>> = Vec::with_capacity(sol.len());
        for (i, p) in sol.iter().enumerate() {
            let (l, end) = (starts[i], starts[i] + p.len);
            let source: &dyn Source = self.source;
            let view = Fragmented::new(source, sol.clone());
            let mut list: Vec<u64> = Vec::new();
            for b in self.seams.suspicious_breaks(&view, l, end) {
                if b <= l || b >= end || list.len() == self.limits.break_points {
                    continue;
                }
                // The unit boundary at or before the suspicious position: the piece is cut there.
                let b = l + (self.space.floor(p.start + (b - l)) - p.start);
                if b > l && !list.contains(&b) {
                    list.push(b);
                }
            }
            top.push(list.first().copied());
            for b in self.breaks(p.start, l, l + 1, end - 1, false) {
                if list.len() == self.limits.break_points {
                    break;
                }
                if !list.contains(&b) {
                    list.push(b);
                }
            }
            lists.push(list);
        }
        // A piece whose cut would need a piece beyond the cap has no alternative to walk (Q34).
        let mut top_done: Vec<bool> = (0..sol.len())
            .map(|i| i + 2 > self.space.max_pieces())
            .collect();
        let root = (0, self.seams.begin());
        let (head, last) = sol.split_at(sol.len() - 1);
        let Some(w) = self.walk(head, last[0].start, last[0].len, &root.1, total) else {
            self.end_recheck(saved, &top, &top_done);
            return;
        };
        let mut resume = vec![root];
        resume.extend(w.resume);
        // Q33, Q36: no candidate starts in a unit of the solution itself. Its later units, decoded out of step, could
        // look evident and take the whole budget. This hides true paths that reuse or rejoin the solution's own pieces
        // (a burst neighbour's data spliced into the middle of a photo): the JPEG does not rely on uniqueness for those, a
        // content-only JPEG in two or more pieces is at most Partial (Q37, `jpeg::jpeg_health_cap`).
        self.excluded = sol.clone();
        'ranks: for rank in 0..self.limits.break_points {
            for (i, list) in lists.iter().enumerate() {
                let Some(&b) = list.get(rank) else {
                    continue;
                };
                if self.stop.is_some() {
                    break 'ranks;
                }
                // The piece cut at `b` is piece i + 1; the next one must fit too.
                if i + 2 > self.space.max_pieces() {
                    // No alternative fits within the piece cap: nothing to walk (counted done above, Q34).
                    continue;
                }
                let st = resume
                    .iter()
                    .rev()
                    .find(|r| r.0 <= b)
                    .unwrap_or(&resume[0])
                    .clone();
                let p = sol[i];
                let mut pieces = sol[..i].to_vec();
                pieces.push(Fragment {
                    start: p.start,
                    len: b - starts[i],
                });
                // Q29: here only an alternative with content evidence past the new seam counts; a blind one (units the
                // format cannot check, filled from any free run) is not descended into.
                let is_top = rank == 0 && top[i] == Some(b);
                self.short = false;
                let (evident, _) = self.classify(&pieces, &st, p.start + (b - starts[i]), b);
                for g2 in evident {
                    self.dfs(&mut pieces, &st, g2, b, false);
                }
                // Walked to the end: no candidate or break-point cap, budget or cancel cut it short (the piece cap may be
                // hit: past it no alternative fits).
                top_done[i] |= is_top && !self.short && self.stop.is_none();
            }
        }
        self.end_recheck(saved, &top, &top_done);
    }

    /// Ends the re-check: `exhaustive` as the search left it, unless the candidates of a top-hinted cut were not all
    /// walked (Q33; a second solution found makes that moot), and a re-check budget hit is no stop.
    fn end_recheck(&mut self, saved: bool, top: &[Option<u64>], top_done: &[bool]) {
        self.excluded.clear();
        let missed = top
            .iter()
            .zip(top_done)
            .any(|(t, &done)| t.is_some() && !done);
        self.exhaustive = saved && (!missed || self.stop == Some(Stop::Ambiguous));
        if self.stop == Some(Stop::Budget) {
            self.stop = None;
        }
    }

    /// The anchor-implied start of the piece after a seam at logical `b`, absolute `end`, when it is free.
    fn anchored(&self, pieces: &[Fragment], end: u64, b: u64) -> Option<u64> {
        let (l, p) = self.anchor?;
        let g2 = p.checked_sub(l.checked_sub(b)?)?;
        let ok = g2 != end
            && self.space.is_unit_start(g2)
            && self.space.free_run(g2, pieces, &self.own_at(g2, b), 1) > 0;
        ok.then_some(g2)
    }

    /// The candidates after `pieces` (seam at logical `b`, absolute `end`), walked from `st`: the evident ones (content
    /// evidence past the seam) and the blind ones (none: broken only further on, or complete through blind data), each
    /// in D4 order.
    fn classify(
        &mut self,
        pieces: &[Fragment],
        st: &(u64, S::State),
        end: u64,
        b: u64,
    ) -> (Vec<u64>, Vec<u64>) {
        let mut cands: Vec<u64> = self.anchored(pieces, end, b).into_iter().collect();
        let space = self.space;
        let cap = self.limits.seam_candidates;
        let mut used = pieces.to_vec();
        used.extend_from_slice(&self.excluded);
        let own = std::mem::take(&mut self.own);
        let mut work = 0;
        let (listed, capped) = space.candidates_with_own(
            end,
            &used,
            &own,
            b,
            cap,
            &mut |g2| self.keep(g2, b),
            &mut work,
        );
        self.own = own;
        // The own-file checks of the listing are listing work too (I-3): charged as the prefilter calls are.
        let before = self.listed;
        self.listed += work;
        for _ in before / LISTED_PER_TRY..self.listed / LISTED_PER_TRY {
            if !self.charge() {
                break;
            }
        }
        if capped {
            self.cut_short();
        }
        let first = cands.first().copied();
        // The listing never holds the unit at `end` (no seam), nor does the anchored start.
        cands.extend(listed.into_iter().filter(|&g2| Some(g2) != first));
        let mut evident = Vec::new();
        let mut blinds = Vec::new();
        for g2 in cands {
            if self.stop.is_some() {
                break;
            }
            let Some(max) = self.max_len(b) else {
                break;
            };
            let run2 = space.free_run(g2, pieces, &self.own_at(g2, b), max);
            if run2 == 0 {
                continue;
            }
            let Some(w2) = self.walk(pieces, g2, run2, &st.1, b + run2) else {
                break;
            };
            match w2.end {
                // Complete without evidence past the seam (blind data to the end): blind, in D4 order with the others.
                WalkEnd::Complete { .. } if w2.checked <= b => blinds.push(g2),
                WalkEnd::Complete { .. } => evident.push(g2),
                WalkEnd::Broken { .. } if w2.checked > b => evident.push(g2),
                // No evidence either way at the seam, broken only further on (even inside its own run: a blind
                // cluster then foreign data): blind, descended. Only a candidate broken right at the seam is dropped.
                WalkEnd::Broken { at } if at > b => blinds.push(g2),
                WalkEnd::Broken { .. } => {}
                WalkEnd::Stopped => self.cut_short(),
            }
        }
        (evident, blinds)
    }
}

/// The proof of a complete solution: the format's over its merged pieces, lowered to `Unproven` when a seam on its path
/// had no content evidence (then without asking the format).
fn proof_of<S: Seams + ?Sized>(
    seams: &mut S,
    source: &dyn Source,
    (sol, blind): &Solution,
) -> Proof {
    if *blind {
        return Proof::Unproven;
    }
    let pieces = merged(sol);
    let mut cut = 0;
    let mut cuts = Vec::new();
    for p in &pieces[..pieces.len() - 1] {
        cut += p.len;
        cuts.push(cut);
    }
    let view = Fragmented::new(source, pieces);
    seams.prove(&view, &cuts)
}

/// Adjacent pieces merged.
fn merged(pieces: &[Fragment]) -> Vec<Fragment> {
    let mut out: Vec<Fragment> = Vec::new();
    for &p in pieces {
        match out.last_mut() {
            Some(last) if last.start + last.len == p.start => last.len += p.len,
            _ => out.push(p),
        }
    }
    out
}

/// Searches the pieces of the file starting at `start`.
///
/// `known`: the file-system entry's size for a Deleted-origin file (Q3), else `None`. The effective known length is the
/// format's (`Seams::known_len`) and `known`; when both are `Some` they must agree, else no solution is `complete`.
/// When any is known, the pieces must sum to it exactly.
///
/// The result is the first complete solution in candidate order (evidence candidates before blind ones at each seam),
/// else the path that checked the most bytes; `None` when cancelled or when nothing was checked. The proof of a
/// solution is the format's, lowered to `Unproven` when a seam on its path had no content evidence; a partial result is
/// `Unproven`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn search<S: Seams>(
    seams: &mut S,
    source: &dyn Source,
    space: &Space,
    start: u64,
    known: Option<u64>,
    limits: &Limits,
    budget: &mut Budget,
    cancel: &CancelToken,
) -> Option<Search> {
    let counted = Counted::new(source);
    let known = match (seams.known_len(), known) {
        (Some(a), Some(b)) => (Some(a), a != b),
        (a, b) => (a.or(b), false),
    };
    let caps = budget.for_file(limits);
    counted.set_limit(caps.1);
    let mut ctx = Ctx::new(seams, &counted, space, limits, cancel, known, caps);
    let root = (0, ctx.seams.begin());
    ctx.dfs(&mut Vec::new(), &root, start, 0, false);
    // A read refused during the search makes it not exhaustive; one refused during the re-check does not.
    let refused = counted.over();
    if ctx.solutions.len() == 1 && ctx.stop.is_none() {
        let left = budget.tries_left().saturating_sub(ctx.tries);
        ctx.recheck(limits.recheck_tries.min(left));
    }

    let Ctx {
        seams,
        tries,
        solutions,
        best,
        exhaustive,
        stop,
        first_proof,
        ..
    } = ctx;
    // Settled on its first solution: the rest was not searched.
    let settled = stop == Some(Stop::Settled);
    let exhaustive = exhaustive && !refused && !settled;
    counted.set_limit(u64::MAX);
    let found = if stop == Some(Stop::Cancelled) {
        None
    } else if let Some(first) = solutions.first() {
        let pieces = merged(&first.0);
        let proof = match first_proof {
            Some(p) => p,
            None => proof_of(seams, &counted, first),
        };
        Some(Search {
            len: pieces.iter().map(|p| p.len).sum(),
            pieces,
            complete: true,
            unique: solutions.len() == 1 && !settled,
            exhaustive,
            proof,
        })
    } else {
        best.map(|(len, pieces)| Search {
            pieces: merged(&pieces),
            len,
            complete: false,
            unique: false,
            exhaustive,
            proof: Proof::Unproven,
        })
    };
    budget.charge(tries, counted.read());
    found
}

/// Whether a finding the scan emits is a reassembly candidate (D2): never one of a live file (its chain says where it
/// is); a JPEG that is not `Intact` with a first piece of at least 16 KiB (the 1.3 `MIN_FIRST_PIECE`), an MP4/MOV that
/// is not `Intact`, a `Partial` TIFF RAW, and any JPEG, MP4/MOV or TIFF RAW of a deleted file that came out `Partial`.
/// All of them are searched.
pub(crate) fn is_candidate(f: &Finding) -> bool {
    let origin = f.origin.as_ref().map(|o| o.state);
    if origin == Some(FsState::Live) {
        return false;
    }
    let first = f.fragments.first().map_or(f.len, |p| p.len);
    let raw = matches!(
        f.kind,
        FileKind::Arw
            | FileKind::Cr2
            | FileKind::Nef
            | FileKind::Dng
            | FileKind::Orf
            | FileKind::Rw2
            | FileKind::Tiff
    );
    let searched = matches!(f.kind, FileKind::Jpeg | FileKind::Mp4 | FileKind::Mov) || raw;
    match f.health {
        Health::Partial if searched && origin == Some(FsState::Deleted) => true,
        Health::Intact => false,
        _ if f.kind == FileKind::Jpeg => first >= MIN_FIRST_PIECE,
        _ if matches!(f.kind, FileKind::Mp4 | FileKind::Mov) => true,
        Health::Partial => raw,
        Health::Damaged => false,
    }
}

/// What the reassembly step starts from: the scan's grids, the listing's live ranges, the ranges of every `Intact`
/// finding emitted, the start of every finding (ascending), the orphan `moov`s, the caps hit and the limits.
pub(crate) struct Context<'a> {
    pub grids: &'a [Grid],
    pub live: &'a [Fragment],
    pub claimed: &'a Runs,
    pub starts: &'a [u64],
    pub moovs: &'a [(u64, u64)],
    /// A pairing candidate was dropped by a cap (C10: conservative, it blocks a JPEG's `Intact` too).
    pub capped: bool,
    /// More candidates than [`Limits::candidates`]: a dropped one might compete for the same pieces (D2).
    pub frags_capped: bool,
    /// More claimed runs than [`Limits::claimed_runs`]: some `Intact` pieces may read free (D18).
    pub claimed_capped: bool,
    /// A volume listing dropped entries: live clusters may be unknown (Q5).
    pub listing_truncated: bool,
    /// What the pairing left of its `moov` load and sample budgets: the videos' `moov`s are loaded within it.
    pub moov_budget: crate::pairing::Budget,
    pub limits: &'a Limits,
}

/// What the scan checks besides the search for the health of a reassembly (D6).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Checks {
    pub capped: bool,
    pub frags_capped: bool,
    pub claimed_capped: bool,
    pub listing_truncated: bool,
    /// An unreadable sector in a piece.
    pub bad: bool,
    /// The pieces sum to the file-system entry's size, when one is known.
    pub size_ok: bool,
    /// The format's own check of the whole file over its pieces is `Intact` (the JPEG: the strict decode; a video: its
    /// index against its length).
    pub own_ok: bool,
    /// A JPEG with restart markers (DRI > 0): at most `Partial` in 2 or more pieces (Q16).
    pub restarts: bool,
    /// The pieces were placed by their content alone, no file system proving where they lie (every result of
    /// [`search`] so far; a JPEG in 2 or more pieces is then at most `Partial`, ruling Q37).
    pub content_only: bool,
}

/// The health of a reassembly of a file of `kind` (D6): `Intact` iff the search is complete, unique, exhaustive and
/// `Proven`, no cap was hit, the listing is whole (Q5), no piece holds an unreadable sector, the known size matches, the
/// format's own check passes, it is no JPEG with restart markers in 2 or more pieces (Q16) and, for a JPEG, the format's
/// cap allows it ([`jpeg::jpeg_health_cap`]: placed by content alone, 2 or more pieces never are, Q37); else `Partial`.
pub(crate) fn health(found: &Search, kind: FileKind, c: &Checks) -> Health {
    let search = found.complete && found.unique && found.exhaustive && found.proof == Proof::Proven;
    let many = found.pieces.len() >= 2;
    let format = match kind {
        FileKind::Jpeg => {
            jpeg::jpeg_health_cap(found, c.content_only) == Health::Intact && !(c.restarts && many)
        }
        _ => true,
    };
    let scan = !c.capped
        && !c.frags_capped
        && !c.claimed_capped
        && !c.listing_truncated
        && !c.bad
        && c.size_ok
        && c.own_ok;
    if search && format && scan {
        Health::Intact
    } else {
        Health::Partial
    }
}

fn rank(h: Health) -> u8 {
    match h {
        Health::Damaged => 0,
        Health::Partial => 1,
        Health::Intact => 2,
    }
}

/// Whether `new` should replace `old` (D6): strictly better health, or both `Partial` and `new` is complete (its walk
/// reached the file's end) and longer.
pub(crate) fn improves(old: &Finding, new: &Finding, complete: bool) -> bool {
    rank(new.health) > rank(old.health)
        || (old.health == Health::Partial
            && new.health == Health::Partial
            && complete
            && new.len > old.len)
}

/// A format whose walk can tell that a placement's bytes are not the file's.
pub(crate) trait Refutes {
    /// Whether the walk of `view` from the file's start fails before its end by content (a check refutes its bytes);
    /// a cut by the view's end, an unreadable byte or spent steps refute nothing.
    fn refutes(&mut self, view: &dyn Source) -> bool;
}

/// Whether `new`, complete but no better than `old` by [`improves`] (both `Partial`, the same length), still replaces
/// it (ruling Q43): it has other pieces, and `old`'s own pieces (its `fragments`, else one piece of its length), walked
/// by the same seams over `source`, fail before their end by content. Whatever placed `old` (a guess, a recorded run,
/// a chain), a placement its content does not refute stays: otherwise [`improves`] is the only rule (D6).
pub(crate) fn replaces(
    old: &Finding,
    new: &Finding,
    complete: bool,
    source: &dyn Source,
    seams: &mut dyn Refutes,
) -> bool {
    if !(complete
        && old.health == Health::Partial
        && new.health == Health::Partial
        && new.len == old.len
        && new.fragments != old.fragments)
    {
        return false;
    }
    let pieces = if old.fragments.is_empty() {
        vec![Fragment {
            start: old.start,
            len: old.len,
        }]
    } else {
        old.fragments.clone()
    };
    seams.refutes(&Fragmented::new(source, pieces))
}

/// The reassembly step: every candidate (id order) searched; returns the findings it improves (same id; a file-system
/// file keeps its origin with placement `Carved`, D15). For a candidate of origin `Deleted`, the search gets the entry's
/// size as the known length (Q3): the pieces must sum to it. `paired`: the pairing's results, whose `Intact` pieces are
/// claimed first; each `Intact` reassembly claims its pieces for the next candidates. Sequential and deterministic: it
/// depends only on the source's bytes and its arguments. JPEGs, TIFF RAWs, videos with their index first or in
/// fragments, and moov-less videos with a fitting orphan `moov` (in `ctx.moovs`) are searched; a video the pairing made
/// `Intact` is left alone, and one it paired otherwise must improve on the pairing's result. Stops early, returning what it has, when cancelled (the caller looks).
pub(crate) fn reassemble(
    source: &CachedSource,
    candidates: &[Finding],
    paired: &[Finding],
    ctx: &Context,
    cancel: &CancelToken,
) -> Vec<Finding> {
    let _scope = CancelScope::enter(cancel);
    let mut taken = Taken::new(ctx.live, ctx.claimed);
    for f in paired.iter().filter(|f| f.health == Health::Intact) {
        taken.claim(&f.ranges());
    }
    let mut budget = Budget::new(ctx.limits);
    // The videos the pairing made `Intact` are done: neither searched nor counted for a `moov`'s filter.
    let intact: std::collections::HashSet<u64> = paired
        .iter()
        .filter(|p| p.health == Health::Intact)
        .map(|p| p.id)
        .collect();
    let done = |f: &Finding| intact.contains(&f.id);
    let videos: Vec<Finding> = candidates
        .iter()
        .filter(|f| matches!(f.kind, FileKind::Mp4 | FileKind::Mov) && !done(f))
        .cloned()
        .collect();
    let mut moovs = video::Moovs::new(source, &videos, ctx.moovs.len(), ctx.moov_budget);
    // Which video first had a complete solution with each orphan `moov` (index in `ctx.moovs`).
    let mut owner: HashMap<usize, u64> = HashMap::new();
    let mut out: Vec<Finding> = Vec::new();
    for f in candidates {
        if cancel.is_cancelled() || budget.spent() {
            break;
        }
        // A video with its index before its media data (its own `moov` first, or fragments).
        let index_first = if matches!(f.kind, FileKind::Mp4 | FileKind::Mov) && !done(f) {
            video::reassemble_index_first(source, f, &taken, ctx, &mut budget, cancel)
        } else {
            None
        };
        let new = match f.kind {
            _ if index_first.is_some() => index_first.flatten(),
            FileKind::Jpeg => reassemble_jpeg(source, f, &taken, ctx, &mut budget, cancel),
            FileKind::Arw
            | FileKind::Cr2
            | FileKind::Nef
            | FileKind::Dng
            | FileKind::Orf
            | FileKind::Rw2
            | FileKind::Tiff => reassemble_raw(source, f, &taken, ctx, &mut budget, cancel),
            FileKind::Mp4 | FileKind::Mov if !done(f) && !ctx.moovs.is_empty() => {
                let old = paired.iter().find(|p| p.id == f.id).unwrap_or(f);
                let r = video::reassemble_video(
                    source,
                    f,
                    old,
                    &taken,
                    ctx,
                    &mut moovs,
                    &mut budget,
                    cancel,
                );
                // As in the pairing, a `moov` that completes two videos (their first pieces are copies) goes back
                // together with neither: an earlier one, then not `Intact` (an `Intact` one claims its `moov`), is
                // dropped too, and so is this one.
                let rivals: Vec<u64> = r
                    .moovs
                    .iter()
                    .filter_map(|i| owner.get(i))
                    .copied()
                    .collect();
                for &i in &r.moovs {
                    owner.entry(i).or_insert(f.id);
                }
                if rivals.is_empty() {
                    r.new
                } else {
                    out.retain(|o| !rivals.contains(&o.id) || o.health == Health::Intact);
                    None
                }
            }
            _ => None,
        };
        if let Some(new) = new {
            if new.health == Health::Intact {
                taken.claim(&new.ranges());
            }
            out.push(new);
        }
    }
    out
}

/// The JPEG `f` searched; the finding it becomes when that improves it.
fn reassemble_jpeg(
    source: &CachedSource,
    f: &Finding,
    taken: &Taken,
    ctx: &Context,
    budget: &mut Budget,
    cancel: &CancelToken,
) -> Option<Finding> {
    let mut seams = jpeg::JpegSeams::new(source, f.start)?;
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
    let found = search(
        &mut seams, source, &space, f.start, known, ctx.limits, budget, cancel,
    )?;
    if cancel.is_cancelled() || found.pieces.first().map(|p| p.start) != Some(f.start) {
        return None;
    }
    let mut checks = Checks {
        capped: ctx.capped,
        frags_capped: ctx.frags_capped,
        claimed_capped: ctx.claimed_capped,
        listing_truncated: ctx.listing_truncated,
        bad: found
            .pieces
            .iter()
            .any(|p| !source.bad_runs(p.start, p.start + p.len).is_empty()),
        size_ok: known.is_none_or(|k| k == found.len),
        own_ok: true,
        restarts: seams.restart_interval() > 0,
        // Every result of the search is placed by content alone: an exFAT chain that proves a placement comes later
        // (D14).
        content_only: true,
    };
    // The strict decode is the costliest check: only when nothing else keeps the file from `Intact`.
    if health(&found, f.kind, &checks) == Health::Intact {
        let view = Fragmented::new(source, found.pieces.clone());
        checks.own_ok = decode_health(&mut Reader::new(&view), 0, found.len) == Health::Intact;
    }
    let new = Finding {
        id: f.id,
        start: f.start,
        len: found.len,
        kind: f.kind,
        health: health(&found, f.kind, &checks),
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
    improves(f, &new, found.complete).then_some(new)
}

/// The TIFF-based RAW `f` searched (D13); the finding it becomes when that improves it, or when it replaces a placement
/// its content refutes (Q43). The structure of its first piece is read within the file's read budget.
fn reassemble_raw(
    source: &CachedSource,
    f: &Finding,
    taken: &Taken,
    ctx: &Context,
    budget: &mut Budget,
    cancel: &CancelToken,
) -> Option<Finding> {
    if budget.spent() {
        return None;
    }
    let counted = Counted::new(source);
    counted.set_limit(ctx.limits.file_read.min(budget.read));
    let seams = raw::RawSeams::new(&counted, f.start);
    budget.charge(0, counted.read());
    let mut seams = seams?;
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
    let found = search(
        &mut seams, source, &space, f.start, known, ctx.limits, budget, cancel,
    )?;
    if cancel.is_cancelled() || found.pieces.first().map(|p| p.start) != Some(f.start) {
        return None;
    }
    let mut checks = Checks {
        capped: ctx.capped,
        frags_capped: ctx.frags_capped,
        claimed_capped: ctx.claimed_capped,
        listing_truncated: ctx.listing_truncated,
        bad: found
            .pieces
            .iter()
            .any(|p| !source.bad_runs(p.start, p.start + p.len).is_empty()),
        size_ok: known.is_none_or(|k| k == found.len),
        own_ok: true,
        restarts: false,
        content_only: true,
    };
    // The preview's strict decode: only when nothing else keeps the file from `Intact`.
    if health(&found, f.kind, &checks) == Health::Intact {
        let view = Fragmented::new(source, found.pieces.clone());
        checks.own_ok = seams.own_ok(&view);
    }
    let new = Finding {
        id: f.id,
        start: f.start,
        len: found.len,
        kind: f.kind,
        health: health(&found, f.kind, &checks),
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
    (improves(f, &new, found.complete) || replaces(f, &new, found.complete, source, &mut seams))
        .then_some(new)
}

#[cfg(test)]
mod tests;
