//! A scan's state at a block boundary (ruling D22): everything the commit loop, the shared read stage and the finders
//! carry from one block to the next, plus the cache's bad map and coverage, so a scan resumed from it emits exactly the
//! events the uninterrupted scan would have emitted after that boundary (for a source that answers the same every time).
//! What is not stored is rebuilt from the file-system listing, which a session stores once (ruling D23).

use std::collections::{BTreeMap, VecDeque};
use std::rc::Rc;

use super::pipeline::{Block, FinderTails, Finders, Shared, lock};
use super::{Carved, Commit, MAX_MOOVS, MAX_OWN_STARTS, MAX_VIDEOS, Pending, ScanSummary};
use crate::EngineError;
use crate::cache::Runs;
use crate::codec::{Codec, Dec, Enc};
use crate::filesystem::{Listing, MAX_FILES, MAX_ORPHAN_DIRS, OrphanDir};
use crate::formats::Fragment;
use crate::scan::Finding;

/// What a state is valid for: a scan of a source of `total` bytes, with these sector and block sizes (as the scan uses
/// them: normalized) and file systems on or off (ruling P44). A state is resumed only by a scan of the same shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Shape {
    pub(super) sector: u64,
    pub(super) block: u64,
    pub(super) filesystems: bool,
    pub(super) total: u64,
}

/// A scan's state at a block boundary, right after its `Progress` (and its `DiskFailing`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ScanState {
    /// The scan it belongs to.
    shape: Shape,
    /// The next block's start.
    position: u64,
    /// Events emitted so far, every one but `Rate` (ruling P4).
    events: u64,
    summary: ScanSummary,
    run: Option<(u64, u64)>,
    unreadable_runs: u64,
    failing_sent: bool,
    pending: Vec<Pending>,
    next_id: u64,
    moovs: Vec<(u64, u64)>,
    /// `Runs` as `(start, end)`.
    own_moovs: Vec<(u64, u64)>,
    videos: Vec<Finding>,
    capped: bool,
    starts: Vec<u64>,
    next_kept: u64,
    /// Each own-file start and the ranges of each finding owning it, by value.
    own: Vec<(u64, Vec<Vec<Fragment>>)>,
    own_len: u64,
    dirs: Vec<OrphanDir>,
    carved: Vec<Carved>,
    orphans_capped: bool,
    /// `Shared::skips`.
    skips: Vec<(u64, u64)>,
    /// The finders' tails as of `position`.
    tails: FinderTails,
    /// The cache's whole bad map (ruling P5).
    bad: Vec<(u64, u64)>,
    /// The cache's coverage runs that end past `position`.
    coverage: Vec<(u64, u64)>,
}

impl ScanState {
    /// The next block's start.
    pub(crate) fn position(&self) -> u64 {
        self.position
    }

    /// Every contract event emitted up to this boundary: all but `Rate`, so the boundary's `Progress` and its
    /// `DiskFailing` are included (ruling P4).
    pub(crate) fn events(&self) -> u64 {
        self.events
    }

    /// The cache's bad map stored, as `(start, end)` runs.
    #[cfg(test)]
    pub(crate) fn bad(&self) -> &[(u64, u64)] {
        &self.bad
    }
}

/// Where a scan can be saved, handed to the hook of [`super::scan_with`].
pub(crate) enum Checkpoint<'a> {
    /// Once, after the file-system listing (or with an empty listing when `filesystems` is off), before any event.
    /// Never called when the listing was read during a cancel or after the device vanished (ruling P3).
    Listing(&'a Listing),
    /// At every block boundary, after its `Progress` and `DiskFailing` (fixed order `Progress`, `DiskFailing`, `Rate`;
    /// `Rate` is never counted). `state()` builds the state on demand.
    Boundary {
        read: u64,
        events: u64,
        state: &'a dyn Fn() -> ScanState,
    },
    /// After the pairing step, before the `Updated` events (a resume from it runs the pairing again; ruling D24).
    Paired {
        events: u64,
        state: &'a dyn Fn() -> ScanState,
    },
}

/// What a scan resumes from: the listing saved at [`Checkpoint::Listing`] and a state saved at a later checkpoint.
pub(crate) struct Resume {
    pub listing: Listing,
    pub state: ScanState,
}

/// The state at the boundary `position`, after its `Progress` (ruling D22). The finders' tails are those the first
/// block read ahead started from, or the finders' own when nothing is read ahead.
pub(super) fn capture(
    commit: &Commit,
    shared: &Shared,
    ahead: &VecDeque<Block>,
    finders: &Finders,
    shape: Shape,
    position: u64,
) -> ScanState {
    // Every field named: a new one fails to compile until it is saved or listed as rebuilt.
    let Commit {
        shared: _,
        on_event: _,
        events,
        summary,
        run,
        unreadable_runs,
        failing_sent,
        // Timing, not state: a resumed scan measures its own speed.
        rate: _,
        pending,
        next_id,
        moovs,
        own_moovs,
        videos,
        capped,
        starts,
        next_kept,
        own,
        own_len,
        // Rebuilt from the listing's volumes.
        grids: _,
        volumes: _,
        orphan_heads: _,
        dirs,
        carved,
        orphans_capped,
    } = commit;
    let tails = ahead
        .front()
        .map_or_else(|| finders.tails(), |b| b.tails.clone());
    let (bad, coverage) = shared.cache.snapshot(position);
    ScanState {
        shape,
        position,
        events: *events,
        summary: summary.clone(),
        run: *run,
        unreadable_runs: *unreadable_runs,
        failing_sent: *failing_sent,
        pending: pending.iter().cloned().collect(),
        next_id: *next_id,
        moovs: moovs.clone(),
        own_moovs: own_moovs.to_vec(),
        videos: videos.clone(),
        capped: *capped,
        starts: starts.clone(),
        next_kept: *next_kept as u64,
        own: own
            .iter()
            .map(|(&s, owners)| (s, owners.iter().map(|o| o.to_vec()).collect()))
            .collect(),
        own_len: *own_len as u64,
        dirs: dirs.clone(),
        carved: carved.clone(),
        orphans_capped: *orphans_capped,
        skips: lock(&shared.skips).to_vec(),
        tails,
        bad,
        coverage,
    }
}

fn corrupt<T>() -> Result<T, EngineError> {
    Err(EngineError::SessionCorrupt)
}

impl ScanState {
    /// Puts the state back into a fresh scan of `shape`: the commit loop, the skip ranges, the finders and the cache
    /// (before anything is read). Returns the position to read from. `SessionCorrupt` when the state cannot belong to
    /// such a scan: saved by a scan of another shape (ruling P44), or not consistent with it.
    pub(super) fn apply(
        self,
        commit: &mut Commit,
        shared: &Shared,
        finders: &mut Finders,
        shape: Shape,
    ) -> Result<u64, EngineError> {
        let ScanState {
            shape: saved,
            position,
            events,
            summary,
            run,
            unreadable_runs,
            failing_sent,
            pending,
            next_id,
            moovs,
            own_moovs,
            videos,
            capped,
            starts,
            next_kept,
            own,
            own_len,
            dirs,
            carved,
            orphans_capped,
            skips,
            tails,
            bad,
            coverage,
        } = self;
        let (total, block) = (shape.total, shape.block);
        if saved != shape
            || position > total
            || (position != total && !position.is_multiple_of(block))
        {
            return corrupt();
        }
        if !own.windows(2).all(|w| w[0].0 < w[1].0) {
            return corrupt();
        }
        let (Some(own_moovs), Some(skips)) = (
            Runs::from_vec(own_moovs, usize::MAX),
            Runs::from_vec(skips, usize::MAX),
        ) else {
            return corrupt();
        };
        if shared.cache.restore(bad, coverage).is_none() || !finders.restore(tails) {
            return corrupt();
        }
        *lock(&shared.skips) = skips;
        commit.events = events;
        commit.summary = summary;
        commit.run = run;
        commit.unreadable_runs = unreadable_runs;
        commit.failing_sent = failing_sent;
        commit.pending = pending.into();
        commit.next_id = next_id;
        commit.moovs = moovs;
        commit.own_moovs = own_moovs;
        commit.videos = videos;
        commit.capped = capped;
        commit.starts = starts;
        commit.next_kept = usize::try_from(next_kept).or_else(|_| corrupt())?;
        commit.own = own
            .into_iter()
            .map(|(s, owners)| (s, owners.into_iter().map(Rc::from).collect()))
            .collect::<BTreeMap<_, _>>();
        commit.own_len = own_len as usize;
        commit.dirs = dirs;
        commit.carved = carved;
        commit.orphans_capped = orphans_capped;
        Ok(position)
    }
}

impl Codec for Pending {
    fn enc(&self, e: &mut Enc) {
        let Pending {
            finding,
            ranges,
            end,
        } = self;
        e.put(finding);
        e.seq(ranges);
        e.u64(*end);
    }
    fn dec(d: &mut Dec) -> Result<Self, EngineError> {
        Ok(Pending {
            finding: d.get()?,
            ranges: d.seq()?,
            end: d.u64()?,
        })
    }
}

impl Codec for Carved {
    fn enc(&self, e: &mut Enc) {
        let Carved {
            id,
            start,
            len,
            kind,
            health,
            fragments,
        } = self;
        e.u64(*id);
        e.u64(*start);
        e.u64(*len);
        e.put(kind);
        e.put(health);
        e.seq(fragments);
    }
    fn dec(d: &mut Dec) -> Result<Self, EngineError> {
        Ok(Carved {
            id: d.u64()?,
            start: d.u64()?,
            len: d.u64()?,
            kind: d.get()?,
            health: d.get()?,
            fragments: d.seq::<Fragment>()?.into_boxed_slice(),
        })
    }
}

impl Codec for OrphanDir {
    fn enc(&self, e: &mut Enc) {
        let OrphanDir { at, volume, parent } = self;
        e.u64(*at);
        e.u32(*volume);
        e.u32(*parent);
    }
    fn dec(d: &mut Dec) -> Result<Self, EngineError> {
        Ok(OrphanDir {
            at: d.u64()?,
            volume: d.u32()?,
            parent: d.u32()?,
        })
    }
}

/// A tail: its bytes and where they end.
fn enc_tail(e: &mut Enc, (bytes, end): &(Vec<u8>, u64)) {
    e.bytes(bytes);
    e.u64(*end);
}

fn dec_tail(d: &mut Dec) -> Result<(Vec<u8>, u64), EngineError> {
    Ok((d.bytes()?.to_vec(), d.u64()?))
}

impl Codec for FinderTails {
    fn enc(&self, e: &mut Enc) {
        let FinderTails { moov, orphan } = self;
        enc_tail(e, moov);
        e.bool(orphan.is_some());
        if let Some(t) = orphan {
            enc_tail(e, t);
        }
    }
    fn dec(d: &mut Dec) -> Result<Self, EngineError> {
        let moov = dec_tail(d)?;
        let orphan = if d.bool()? { Some(dec_tail(d)?) } else { None };
        Ok(FinderTails { moov, orphan })
    }
}

impl Codec for Shape {
    fn enc(&self, e: &mut Enc) {
        let Shape {
            sector,
            block,
            filesystems,
            total,
        } = self;
        e.u64(*sector);
        e.u64(*block);
        e.bool(*filesystems);
        e.u64(*total);
    }
    fn dec(d: &mut Dec) -> Result<Self, EngineError> {
        Ok(Shape {
            sector: d.u64()?,
            block: d.u64()?,
            filesystems: d.bool()?,
            total: d.u64()?,
        })
    }
}

impl Codec for ScanState {
    fn enc(&self, e: &mut Enc) {
        let ScanState {
            shape,
            position,
            events,
            summary,
            run,
            unreadable_runs,
            failing_sent,
            pending,
            next_id,
            moovs,
            own_moovs,
            videos,
            capped,
            starts,
            next_kept,
            own,
            own_len,
            dirs,
            carved,
            orphans_capped,
            skips,
            tails,
            bad,
            coverage,
        } = self;
        e.put(shape);
        e.u64(*position);
        e.u64(*events);
        e.put(summary);
        e.put(run);
        e.u64(*unreadable_runs);
        e.bool(*failing_sent);
        e.seq(pending);
        e.u64(*next_id);
        e.seq(moovs);
        e.seq(own_moovs);
        e.seq(videos);
        e.bool(*capped);
        e.seq(starts);
        e.u64(*next_kept);
        e.seq(own);
        e.u64(*own_len);
        e.seq(dirs);
        e.seq(carved);
        e.bool(*orphans_capped);
        e.seq(skips);
        e.put(tails);
        e.seq(bad);
        e.seq(coverage);
    }

    fn dec(d: &mut Dec) -> Result<Self, EngineError> {
        let state = ScanState {
            shape: d.get()?,
            position: d.u64()?,
            events: d.u64()?,
            summary: d.get()?,
            run: d.get()?,
            unreadable_runs: d.u64()?,
            failing_sent: d.bool()?,
            pending: d.seq()?,
            next_id: d.u64()?,
            moovs: d.seq()?,
            own_moovs: d.seq()?,
            videos: d.seq()?,
            capped: d.bool()?,
            starts: d.seq()?,
            next_kept: d.u64()?,
            own: d.seq()?,
            own_len: d.u64()?,
            dirs: d.seq()?,
            carved: d.seq()?,
            orphans_capped: d.bool()?,
            skips: d.seq()?,
            tails: d.get()?,
            bad: d.seq()?,
            coverage: d.seq()?,
        };
        // The caps the scan keeps, and `own_len` counting `own`'s entries (the scan subtracts from it).
        let entries: u64 = state.own.iter().map(|(_, o)| o.len() as u64).sum();
        let within_caps = state.moovs.len() <= MAX_MOOVS
            && state.videos.len() <= MAX_VIDEOS
            && state.dirs.len() <= MAX_ORPHAN_DIRS
            && state.carved.len() <= MAX_FILES
            && state.own_len <= MAX_OWN_STARTS as u64;
        if !within_caps || entries != state.own_len {
            return corrupt();
        }
        Ok(state)
    }
}

#[cfg(test)]
#[path = "state_tests.rs"]
mod tests;
