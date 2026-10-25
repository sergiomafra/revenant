//! The scan: walks a source sector by sector, lets each format recognize and measure files, and reports events. Blocks
//! are read ahead and candidate files measured on worker threads ([`pipeline`]); this module's commit loop takes the
//! results in source order, so the events are those of a sequential scan.

mod fs;
mod pipeline;

use std::collections::{BTreeMap, VecDeque};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::EngineError;
use crate::cache::{CACHE_WINDOW, CachedSource, Runs};
use crate::filesystem::{
    FsOrigin, FsState, MAX_FILES, MAX_HEAD, MAX_ORPHAN_DIRS, OrphanDir, OrphanFinder, Placement,
    SourceProbe, Volume, head_len, kind_class, orphan_entries,
};
use crate::formats::{FileKind, Format, Fragment, Health, Measurement};
use crate::source::Source;
use fs::{FsJobResult, FsPlan, merge, tail_fits};
use pipeline::{Block, CloseQueue, Finders, Head, Measured, Outcome, Shared};

/// Bytes of a sector start shown to `Format::recognizes`.
const HEAD_LEN: usize = 16;

/// A file found by the scan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    /// Sequential from 1, in discovery order.
    pub id: u64,
    /// Where the file starts (the first piece's start when it is in pieces).
    pub start: u64,
    /// Total length of the file (the sum of its pieces when it has any).
    pub len: u64,
    pub kind: FileKind,
    pub health: Health,
    /// Empty for a contiguous file. Otherwise at least 2 pieces in file order: the first starts at `start`, the lengths
    /// sum to `len`, no two overlap.
    pub fragments: Vec<Fragment>,
    /// Where the file system says this file is, when it does (name, folder, state, dates). `None` for a file found only
    /// by its signature.
    pub origin: Option<FsOrigin>,
}

impl Finding {
    /// The file's byte ranges in file order: `[Fragment { start, len }]` for a contiguous file, else `fragments`.
    pub fn ranges(&self) -> Vec<Fragment> {
        ranges(self.start, self.len, &self.fragments)
    }
}

/// Whether a file at `at` of `len` bytes keeps the pieces contract: no pieces, or at least 2 in file order whose first
/// starts at `at`, whose lengths sum to `len`, that do not overlap and that lie inside a source of `source_len` bytes.
fn pieces_ok(at: u64, len: u64, p: &[Fragment], source_len: u64) -> bool {
    if p.is_empty() {
        return true;
    }
    let inside = p.iter().all(|f| {
        f.start
            .checked_add(f.len)
            .is_some_and(|end| end <= source_len)
    });
    let sum = p.iter().try_fold(0u64, |acc, f| acc.checked_add(f.len));
    let mut sorted = p.to_vec();
    sorted.sort_by_key(|f| f.start);
    let disjoint = sorted
        .windows(2)
        .all(|w| w[0].start.saturating_add(w[0].len) <= w[1].start);
    p.len() >= 2 && p[0].start == at && sum == Some(len) && inside && disjoint
}

/// `[Fragment { start, len }]` when `fragments` is empty, else `fragments`.
pub(crate) fn ranges(start: u64, len: u64, fragments: &[Fragment]) -> Vec<Fragment> {
    if fragments.is_empty() {
        vec![Fragment { start, len }]
    } else {
        fragments.to_vec()
    }
}

/// Most blocks read ahead of the commit loop.
const MAX_LOOKAHEAD_BLOCKS: usize = 16;
/// Most threads `threads: 0` resolves to.
const MAX_AUTO_THREADS: usize = 16;
/// Most threads an explicit `threads` gives.
const MAX_THREADS: usize = 64;

/// How a scan reads and measures.
///
/// Memory: the scan holds at most `cache_bytes` of cached source data, plus for each reading thread (the scan thread
/// and each measuring thread) one window being loaded (1 MiB) and a pool of at most 4 reader windows (4 MiB) kept
/// between measurements, plus the validation decodes of every scan in the process (the budget counts at most 512 MiB
/// at once: the JPEG bytes and the 1-byte-per-pixel output; the decoder's internal buffers, such as a progressive
/// image's coefficients, come on top) and what each running measurement holds (for example a `moov` of at most 16 MiB,
/// or a JPEG gap search's DC values, about 3 × 13 MiB at 210 megapixels, plus 3 MiB),
/// plus about 200 B per recognized sector start in the read-ahead (at most lookahead × block / sector of them), plus the
/// orphan `moov` candidates and `Damaged` videos kept for pairing (at most 4096 + 4096 per block read ahead, and 4096
/// findings) and the start of every finding (8 B each), plus the own-file starts of `Partial` RAWs (at most 65,536,
/// about 70 to 90 B each, at most about 6 MiB, plus each RAW's ranges), plus the old-directory candidates (16 B each, at most 65,536 kept) and the
/// findings without origin on a cluster grid they may name (≈ 64 B each, at most 262,144), plus the file-system listing: up to 262,144 files and 2^20
/// pieces, ≈ 355 MiB kept for the whole scan in the worst case at those caps, with a peak of ≈ 640 MiB while it is read
/// (on exFAT, plus the allocation bitmap, ≤ 32 MiB, and the root entries waiting for it, ≤ 68 MiB; core README), and,
/// after the scan, the pairing step's one `moov` and its sample lists (up to about 128 MiB). So with
/// the defaults on a 16-thread machine, about 256 MiB + 17 × 5 MiB + 512 MiB, plus the measurements' own buffers.
pub struct ScanOptions {
    /// Granularity of candidate starts and of unreadable-region handling, in bytes. Above 512, the detectors see a sector that
    /// failed only in part as wholly unreadable (health stays conservative), and a file-system file whose first piece is
    /// not sector-aligned is silently dropped (it can still be carved).
    pub sector: u64,
    /// Bytes read per block (rounded down to a whole number of sectors, at least one).
    pub block: usize,
    /// Bytes of recently read data the scan and the detectors share, so each sector is read from the source once (values
    /// below 1 MiB are raised to it). The read-ahead is sized to fit in it (see [`ScanOptions::lookahead_blocks`]).
    /// "Once" needs room for the work in flight: from about 128 MiB on (the default is 256 MiB). Smaller caches evict
    /// windows still needed and read them again: with 32 MiB the 1 GiB `cut-jpeg` bench image is read 9.46 times over
    /// with 1 thread (5.94 with 16), with 64 MiB 3.63 (2.78) times.
    pub cache_bytes: usize,
    /// Threads that measure candidate files: 0 (the default) means one per available core, at most 16; 1 measures on
    /// the scan thread itself; other values are capped at 64. The events are the same for any value.
    pub threads: usize,
    /// Read the FAT/exFAT file systems first (the default): files they list come out with their [`Finding::origin`].
    /// `false` scans by signature alone: no `Volume` events and every origin `None`.
    pub filesystems: bool,
    /// Tests: no `fs_job` reuse of the carving.
    #[cfg(test)]
    pub(crate) no_reuse: bool,
    /// Tests: the workers hand every file-system job back unmeasured.
    #[cfg(test)]
    pub(crate) drop_fs_jobs: bool,
}

impl ScanOptions {
    /// Sector size actually used: at least the head length.
    fn sector_len(&self) -> u64 {
        self.sector.max(HEAD_LEN as u64)
    }

    /// Block size actually used: a whole number of sectors, at least one.
    fn block_len(&self) -> u64 {
        (self.block as u64 / self.sector_len()).max(1) * self.sector_len()
    }

    /// Threads `threads` stands for.
    pub fn resolved_threads(&self) -> usize {
        match self.threads {
            0 => std::thread::available_parallelism()
                .map_or(1, |n| n.get())
                .clamp(1, MAX_AUTO_THREADS),
            n => n.min(MAX_THREADS),
        }
    }

    /// Blocks read ahead of the commit loop: `min(16, max(1, cache / (2 × block)))`, so the read-ahead always fits in
    /// half the cache and a block read ahead is still cached when it is examined.
    pub fn lookahead_blocks(&self) -> usize {
        let cache = self.cache_bytes.max(CACHE_WINDOW) as u64;
        (cache / (2 * self.block_len())).clamp(1, MAX_LOOKAHEAD_BLOCKS as u64) as usize
    }
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            sector: 512,
            block: 4 * 1024 * 1024,
            cache_bytes: 256 * 1024 * 1024,
            threads: 0,
            filesystems: true,
            #[cfg(test)]
            no_reuse: false,
            #[cfg(test)]
            drop_fs_jobs: false,
        }
    }
}

/// Cooperative cancellation flag shared between clones.
#[derive(Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// A FAT/exFAT volume of the source, before the first `Progress`, one per volume in index order.
    /// `listing_truncated`: a cap or budget of the file-system listing dropped or degraded some files (the same for
    /// every volume of the scan): some files found may lack their name, folder or dates.
    Volume {
        volume: Volume,
        listing_truncated: bool,
    },
    Found(Finding),
    /// `read` bytes of `total` have been examined.
    Progress {
        read: u64,
        total: u64,
    },
    /// A run of consecutive sectors that could not be read.
    Unreadable {
        start: u64,
        len: u64,
    },
    /// A better version of a finding already reported: it replaces the earlier `Found` with the same `id`. Emitted after
    /// every `Found`, before `scan` returns, one per id in id order (today: a video put back together with its separated
    /// `moov`, and a finding an old folder of a quick-formatted card names, ruling D11; both merged into one).
    Updated(Finding),
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct ScanSummary {
    pub found: u64,
    pub unreadable_bytes: u64,
    /// `Updated` events emitted.
    pub updated: u64,
    /// The file-system listing was truncated by one of its caps or budgets (see [`Event::Volume`]), or a cap of the
    /// old folders of a quick-formatted card dropped some of them or some of their names (ruling R7; the `Volume`
    /// events, emitted before the search, do not carry that part).
    pub listing_truncated: bool,
}

/// Most orphan `moov` candidates kept per scan (the first ones, by offset).
const MAX_MOOVS: usize = 4096;
/// Most `Damaged` moov-less video candidates kept per scan (the first ones, by id).
const MAX_VIDEOS: usize = 4096;
/// Bytes of a `moov` candidate's header: a skip range overlapping them drops the candidate.
const MOOV_HEAD: u64 = 16;
/// Most own-file starts of `Partial` findings kept at once (ruling D12); past it, further ones are not kept.
const MAX_OWN_STARTS: usize = 65_536;

/// Whether every range of `inner` lies inside the union of `outer` (disjoint ranges).
fn within(inner: &[Fragment], outer: &[Fragment]) -> bool {
    inner.iter().all(|r| {
        let end = r.start.saturating_add(r.len);
        let covered: u64 = outer
            .iter()
            .map(|o| {
                let (lo, hi) = (o.start.max(r.start), o.start.saturating_add(o.len).min(end));
                hi.saturating_sub(lo)
            })
            .sum();
        covered == r.len
    })
}

/// A finding emitted without origin that starts on a volume's cluster grid: an old directory may name it (ruling D11).
struct Carved {
    id: u64,
    start: u64,
    len: u64,
    kind: FileKind,
    health: Health,
    fragments: Box<[Fragment]>,
}

impl Carved {
    /// The finding as it was emitted.
    fn finding(&self) -> Finding {
        Finding {
            id: self.id,
            start: self.start,
            len: self.len,
            kind: self.kind,
            health: self.health,
            fragments: self.fragments.to_vec(),
            origin: None,
        }
    }
}

/// A finding not yet emitted.
struct Pending {
    finding: Finding,
    /// Its byte ranges (the pieces, or the whole file).
    ranges: Vec<Fragment>,
    /// The end of its farthest range, rounded up to a whole sector: it is emitted once the scan reaches it.
    end: u64,
}

/// The in-order half of the scan: turns blocks and measurements into events exactly as a sequential walk would.
struct Commit<'a, 's> {
    shared: &'a Shared<'s>,
    on_event: &'a mut dyn FnMut(Event),
    summary: ScanSummary,
    /// The open unreadable run `(start, len)`.
    run: Option<(u64, u64)>,
    /// Found but not yet emitted, in start order.
    pending: VecDeque<Pending>,
    next_id: u64,
    /// Orphan `moov` candidates kept, ascending, as (box start, declared size).
    moovs: Vec<(u64, u64)>,
    /// Ranges of the non-`Damaged` ISO BMFF findings (their own `moov` is no orphan); pruned like the skip ranges.
    own_moovs: Runs,
    /// `Damaged` MP4/MOV findings, in id order (candidates for pairing).
    videos: Vec<Finding>,
    /// A candidate (of either kind) was dropped by a cap: an unchecked one might have paired, so no pairing is `Intact`.
    capped: bool,
    /// The start of every finding, ascending (one per finding): a file starting inside a paired video's pieces makes
    /// the pairing at most `Partial` (ruling Q13).
    starts: Vec<u64>,
    /// The next kept file-system file the walk has not reached (index in `FsPlan::kept`).
    next_kept: usize,
    /// Where an own file of a `Partial` finding starts → the ranges of each such finding, in add order (ruling D12; at
    /// most [`MAX_OWN_STARTS`], pruned below the scan position).
    own: BTreeMap<u64, Vec<Rc<[Fragment]>>>,
    /// Entries (start, finding) in `own`.
    own_len: usize,
    /// `(heap offset, cluster size, heap end)` of each volume, for the pairing (ruling D13) and the old directories.
    grids: Vec<(u64, u64, u64)>,
    /// The volumes of the listing (for the old directories, ruling D11).
    volumes: Vec<Volume>,
    /// Bytes of a cluster start each volume's old-directory check looks at, by volume index.
    orphan_heads: Vec<u64>,
    /// Old-directory candidates kept, in commit order (at most [`MAX_ORPHAN_DIRS`]).
    dirs: Vec<OrphanDir>,
    /// Findings emitted without origin on a cluster grid, in id (and start) order (at most [`MAX_FILES`]).
    carved: Vec<Carved>,
    /// A cap dropped old-directory candidates or carved findings: some names may be missing (ruling R7).
    orphans_capped: bool,
}

#[cfg(test)]
thread_local! {
    /// File-system jobs the commit loop ran itself on this thread (no worker result: `NotMeasured`, a deferred block,
    /// `threads 1`), and those of them in a deferred block.
    pub(crate) static COMMIT_FS_JOBS: std::cell::Cell<(usize, usize)> = const { std::cell::Cell::new((0, 0)) };
}

impl Commit<'_, '_> {
    /// Emits `event` unless the scan is cancelled or the source has vanished; either seen afterwards stops the scan
    /// before anything else is emitted.
    fn emit(&mut self, event: Event) -> Result<(), EngineError> {
        self.shared.check()?;
        (self.on_event)(event);
        self.shared.check()
    }

    fn flush_run(&mut self) -> Result<(), EngineError> {
        if let Some((start, len)) = self.run.take() {
            self.summary.unreadable_bytes += len;
            self.emit(Event::Unreadable { start, len })?;
        }
        Ok(())
    }

    fn emit_found(&mut self, p: Pending) -> Result<(), EngineError> {
        self.summary.found += 1;
        let f = &p.finding;
        if f.origin.is_none()
            && self.grids.iter().any(|&(heap, cluster, end)| {
                f.start >= heap && f.start < end && (f.start - heap).is_multiple_of(cluster)
            })
        {
            if self.carved.len() < MAX_FILES {
                self.carved.push(Carved {
                    id: f.id,
                    start: f.start,
                    len: f.len,
                    kind: f.kind,
                    health: f.health,
                    fragments: f.fragments.clone().into_boxed_slice(),
                });
            } else {
                self.orphans_capped = true;
            }
        }
        if f.health == Health::Damaged && matches!(f.kind, FileKind::Mp4 | FileKind::Mov) {
            if self.videos.len() < MAX_VIDEOS {
                self.videos.push(f.clone());
            } else {
                self.capped = true;
            }
        }
        self.emit(Event::Found(p.finding))
    }

    /// Keeps the `moov` candidates of a block just examined (each header ends in it, so every finding that could hold
    /// it has been added): drops those whose header overlaps a committed skip range (those blocks may not have been
    /// read at all, so keeping them would depend on timing) or that lie inside a non-`Damaged` ISO BMFF finding.
    ///
    /// The per-block cap counts only for a block that committed skip ranges do not wholly cover: such a block may or may
    /// not have been read (timing), and every candidate in it is dropped anyway.
    fn keep_moovs(&mut self, (pos, end): (u64, u64), found: Vec<(u64, u64)>, capped: bool) {
        let skips = pipeline::lock(&self.shared.skips);
        self.capped |= capped && !skips.covers(pos, end);
        for (q, size) in found {
            if skips.overlaps(q, q + MOOV_HEAD)
                || self.own_moovs.overlaps(q, q + 1)
                || self.shared.fs.overlaps_live(q, q + 1)
            {
                continue;
            }
            if self.moovs.len() < MAX_MOOVS {
                self.moovs.push((q, size));
            } else {
                self.capped = true;
            }
        }
    }

    /// Keeps the old-directory candidates of a block just examined with the rules of [`Commit::keep_moovs`]: one whose
    /// head overlaps a committed skip range is dropped, and the per-block cap counts only for a block that skip ranges
    /// do not wholly cover; so is one inside a live range or a directory the listing walked (ruling D11). The global cap
    /// keeps the first ones in commit order.
    fn keep_orphans(&mut self, (pos, end): (u64, u64), found: Vec<OrphanDir>, capped: bool) {
        let skips = pipeline::lock(&self.shared.skips);
        self.orphans_capped |= capped && !skips.covers(pos, end);
        let fs = &self.shared.fs;
        for c in found {
            let head = self
                .orphan_heads
                .get(c.volume as usize)
                .copied()
                .unwrap_or(1);
            if skips.overlaps(c.at, c.at.saturating_add(head))
                || fs.overlaps_live(c.at, c.at + 1)
                || fs.overlaps_walked(c.at, c.at + 1)
            {
                continue;
            }
            if self.dirs.len() < MAX_ORPHAN_DIRS {
                self.dirs.push(c);
            } else {
                self.orphans_capped = true;
            }
        }
    }

    /// After the scan: pairs `Damaged` moov-less videos with orphan `moov`s; returns each video put back together.
    fn pair(&mut self) -> Result<Vec<Finding>, EngineError> {
        let mut out = Vec::new();
        if self.videos.is_empty() || self.moovs.is_empty() {
            return Ok(out);
        }
        let cancel = self.shared.cancel;
        let paired = crate::pairing::pair_videos(
            &self.shared.cache,
            &self.videos,
            &self.moovs,
            &self.starts,
            &self.grids,
            cancel,
        );
        if cancel.is_cancelled() {
            return Err(EngineError::Cancelled);
        }
        for mut f in paired {
            // A pairing bug: caught in debug builds, ignored (no event) in release.
            let ok = pieces_ok(f.start, f.len, &f.fragments, self.shared.total);
            debug_assert!(ok, "pieces contract broken by pairing: {f:?}");
            if !ok {
                continue;
            }
            if self.capped && f.health == Health::Intact {
                f.health = Health::Partial;
            }
            out.push(f);
        }
        Ok(out)
    }

    /// After the pairing: the names the old directories give (ruling D11), merged with the pairing's updates, then one
    /// `Updated` per id, in id order. A finding named by an entry of another recorded size is at most `Partial` when it
    /// is longer, or when what follows it up to that size (contiguous) is neither padding nor whole files (ruling D6).
    fn update(&mut self, paired: Vec<Finding>) -> Result<(), EngineError> {
        let mut out: BTreeMap<u64, Finding> = paired.into_iter().map(|f| (f.id, f)).collect();
        let cancel = self.shared.cancel;
        if !self.dirs.is_empty() && !self.carved.is_empty() {
            let wanted: Vec<(u64, u8)> = self
                .carved
                .iter()
                .map(|r| (r.start, kind_class(r.kind)))
                .collect();
            let fs = &self.shared.fs;
            let (entries, truncated) = orphan_entries(
                &self.shared.cache,
                &self.volumes,
                &self.dirs,
                &fs.walked,
                &fs.live,
                &wanted,
                cancel,
            );
            if cancel.is_cancelled() {
                return Err(EngineError::Cancelled);
            }
            self.summary.listing_truncated |= truncated;
            for e in entries {
                let Ok(k) = self.carved.binary_search_by_key(&e.start, |r| r.start) else {
                    continue;
                };
                let r = &self.carved[k];
                let mut f = out.remove(&r.id).unwrap_or_else(|| r.finding());
                if f.health == Health::Intact && f.len != e.size {
                    let end = ranges(f.start, f.len, &f.fragments)
                        .last()
                        .map_or(f.start, |p| p.start.saturating_add(p.len));
                    let fits = f.len < e.size
                        && tail_fits(
                            self.shared.formats,
                            &self.shared.cache,
                            end,
                            e.size - f.len,
                            cancel,
                        );
                    if !fits {
                        f.health = Health::Partial;
                    }
                }
                f.origin = Some(FsOrigin {
                    volume: e.volume,
                    folder: e.folder,
                    name: e.name,
                    state: FsState::Orphaned,
                    placement: Placement::Carved,
                    size: e.size,
                    created: e.created,
                    modified: e.modified,
                });
                out.insert(f.id, f);
            }
            if cancel.is_cancelled() {
                return Err(EngineError::Cancelled);
            }
        }
        if self.orphans_capped && !self.dirs.is_empty() {
            self.summary.listing_truncated = true;
        }
        for f in out.into_values() {
            self.summary.updated += 1;
            self.emit(Event::Updated(f))?;
        }
        Ok(())
    }

    /// Walks the block's sectors in order.
    fn block(&mut self, block: Block) -> Result<(), EngineError> {
        let sector = self.shared.sector;
        let mut heads = block.heads.into_iter().peekable();
        let mut at = block.pos;
        while at < block.end {
            while self.pending.front().is_some_and(|p| at >= p.end) {
                // Everything inside the finding has been read: report its unreadable run first.
                self.flush_run()?;
                let p = self.pending.pop_front().expect("front exists");
                self.emit_found(p)?;
            }
            let idx = ((at - block.pos) / sector) as usize;
            let sector_end = (at + sector).min(block.end);
            let bad = block.bad.as_ref().is_some_and(|b| b[idx]);
            while heads.next_if(|h| h.at < at).is_some() {}
            // A kept file-system file starting here is always reported, before the bad-sector and skip checks; the
            // carving head here is consumed.
            let kept = self.kept_at(at);
            if let Some(k) = kept {
                let head = heads.next_if(|h| h.at == at);
                let r = self.fs_result(k, at, head, block.deferred)?;
                // The carving counts only where the sequential carving would have examined the head (defensive: no
                // known card makes a skipped or unreadable start's carving win the merge).
                let carved = (!bad && !self.shared.skipped(at))
                    .then_some(r.carved.as_ref())
                    .flatten();
                let m = merge(&self.shared.fs.kept[k], &r.view, carved);
                let own = r.own_of(&m);
                let measurement = Measurement {
                    len: m.len,
                    kind: m.kind,
                    health: m.health,
                    fragments: m.fragments,
                };
                self.add(at, measurement, Some(m.origin), own);
            }
            if bad {
                match &mut self.run {
                    Some((s, l)) if *s + *l == at => *l += sector_end - at,
                    _ => {
                        self.flush_run()?;
                        self.run = Some((at, sector_end - at));
                    }
                }
                for p in &mut self.pending {
                    if p.finding.health == Health::Intact
                        && p.ranges
                            .iter()
                            .any(|r| r.start < sector_end && at < r.start.saturating_add(r.len))
                    {
                        p.finding.health = Health::Partial;
                    }
                }
                at = sector_end;
                continue;
            }
            self.flush_run()?;
            if kept.is_none() && !self.shared.skipped(at) {
                let found = match heads.next_if(|h| h.at == at) {
                    Some(h) => {
                        let outcome = match &h.slot {
                            Some(slot) => slot.wait(self.shared.cancel),
                            None => Outcome::NotMeasured,
                        };
                        match outcome {
                            Outcome::Measured(m) => m,
                            _ if self.shared.cancel.is_cancelled() => {
                                return Err(EngineError::Cancelled);
                            }
                            Outcome::NotMeasured | Outcome::MeasuredFs(_) => {
                                self.measure(at, &h.head[..h.head_len])
                            }
                            Outcome::Cancelled => return Err(EngineError::Cancelled),
                        }
                    }
                    None if block.deferred => self
                        .shared
                        .head_at(at)
                        .and_then(|(head, n)| self.measure(at, &head[..n])),
                    None => None,
                };
                match found {
                    // The head at the first cluster of a live file of another kind is consumed without a finding, and
                    // so is a `Partial` finding's own file (ruling D12); an `Intact` measurement there still skips its
                    // range (ruling R10).
                    Some(Measured { m, .. })
                        if self.shared.fs.is_other(at) || self.is_own(at, &m) =>
                    {
                        if m.health == Health::Intact
                            && pieces_ok(at, m.len, &m.fragments, self.shared.total)
                        {
                            self.skip(&ranges(at, m.len, &m.fragments));
                        }
                    }
                    Some(Measured { m, own }) => self.add(at, m, None, own),
                    None => {}
                }
            }
            at = sector_end;
        }
        Ok(())
    }

    /// Ruling D12: whether the carved measurement `m` at `at` is the own file of an earlier `Partial` finding, one of
    /// whose own files starts at `at` and whose ranges hold all of `m`'s.
    fn is_own(&self, at: u64, m: &Measurement) -> bool {
        self.own.get(&at).is_some_and(|owners| {
            let inner = ranges(at, m.len, &m.fragments);
            owners.iter().any(|owner| within(&inner, owner))
        })
    }

    /// Forgets the own-file starts below `pos` (the walk is past them).
    fn drop_own_below(&mut self, pos: u64) {
        while let Some(entry) = self.own.first_entry()
            && *entry.key() < pos
        {
            self.own_len -= entry.remove().len();
        }
    }

    /// The index of the kept file-system file starting at `at`, if any (`at` only grows during a scan).
    fn kept_at(&mut self, at: u64) -> Option<usize> {
        let starts = &self.shared.fs.starts;
        while self.next_kept < starts.len() && starts[self.next_kept] < at {
            self.next_kept += 1;
        }
        (starts.get(self.next_kept) == Some(&at)).then_some(self.next_kept)
    }

    /// The job result of the kept file `k` at `at`: the worker's, else run here (the same function, so the result does
    /// not depend on who ran it).
    fn fs_result(
        &self,
        k: usize,
        at: u64,
        head: Option<Head>,
        deferred: bool,
    ) -> Result<FsJobResult, EngineError> {
        let outcome = match head.and_then(|h| h.slot) {
            Some(slot) => slot.wait(self.shared.cancel),
            None => Outcome::NotMeasured,
        };
        match outcome {
            Outcome::MeasuredFs(r) => Ok(*r),
            Outcome::Cancelled => Err(EngineError::Cancelled),
            _ if self.shared.cancel.is_cancelled() => Err(EngineError::Cancelled),
            Outcome::NotMeasured | Outcome::Measured(_) => {
                #[cfg(test)]
                COMMIT_FS_JOBS.with(|c| {
                    let (n, d) = c.get();
                    c.set((n + 1, d + usize::from(deferred)));
                });
                let _ = deferred;
                Ok(self.shared.fs_job(k, at))
            }
        }
    }

    /// Adds a skip range per range (rounded out to whole sectors).
    fn skip(&self, ranges: &[Fragment]) {
        let sector = self.shared.sector;
        let mut skips = pipeline::lock(&self.shared.skips);
        for r in ranges {
            skips.add(
                r.start / sector * sector,
                r.start
                    .saturating_add(r.len)
                    .div_ceil(sector)
                    .saturating_mul(sector),
                usize::MAX,
            );
        }
    }

    fn measure(&self, at: u64, head: &[u8]) -> Option<Measured> {
        pipeline::measure(
            self.shared.formats,
            &self.shared.cache,
            at,
            head,
            self.shared.cancel,
        )
    }

    /// Turns the measurement at `at` into a pending finding; an `Intact` one adds a skip range per piece. A finding whose
    /// origin is not a live file and whose ranges overlap a live file is at most `Partial` (ruling D7), and so is one
    /// with a known unreadable sector in a piece below `at`. A `Partial` one
    /// keeps the starts of its `own` files after `at` (sector-aligned ones: no head is examined elsewhere) with its
    /// ranges, so a carved file found there inside it is not reported (ruling D12).
    fn add(&mut self, at: u64, mut m: Measurement, origin: Option<FsOrigin>, own: Vec<Fragment>) {
        if m.len == 0 {
            return;
        }
        // A detector bug: caught in debug builds, ignored (no finding) in release.
        let ok = pieces_ok(at, m.len, &m.fragments, self.shared.total);
        debug_assert!(ok, "pieces contract broken at {at}: {m:?}");
        if !ok {
            return;
        }
        let sector = self.shared.sector;
        let round_up = |x: u64| x.div_ceil(sector).saturating_mul(sector);
        let ranges = ranges(at, m.len, &m.fragments);
        if m.health == Health::Intact
            && origin.as_ref().is_none_or(|o| o.state != FsState::Live)
            && ranges.iter().any(|r| {
                self.shared
                    .fs
                    .overlaps_live(r.start, r.start.saturating_add(r.len))
            })
        {
            m.health = Health::Partial;
        }
        // Unreadable sectors only lower pending findings, so a piece below `at` (a file system's later piece placed
        // before its start) was passed before this finding existed. Everything below `at` is examined, so the cache's
        // bad map there is complete and the result does not depend on threads or cache size.
        if m.health == Health::Intact
            && ranges.iter().any(|r| {
                let end = r.start.saturating_add(r.len).min(at);
                r.start < end && !self.shared.cache.bad_runs(r.start, end).is_empty()
            })
        {
            m.health = Health::Partial;
        }
        let end = ranges
            .iter()
            .map(|r| round_up(r.start.saturating_add(r.len)))
            .max()
            .unwrap_or(at);
        if m.health != Health::Damaged
            && matches!(
                m.kind,
                FileKind::Mp4 | FileKind::Mov | FileKind::Cr3 | FileKind::Heic
            )
        {
            for r in &ranges {
                self.own_moovs
                    .add(r.start, r.start.saturating_add(r.len), usize::MAX);
            }
        }
        if m.health == Health::Intact {
            self.skip(&ranges);
        }
        if m.health == Health::Partial && !own.is_empty() {
            let owner: Rc<[Fragment]> = ranges.clone().into();
            let mut starts: Vec<u64> = own
                .iter()
                .map(|o| o.start)
                .filter(|&s| s > at && s.is_multiple_of(sector))
                .collect();
            starts.sort_unstable();
            starts.dedup();
            for s in starts {
                if self.own_len >= MAX_OWN_STARTS {
                    break;
                }
                self.own.entry(s).or_default().push(Rc::clone(&owner));
                self.own_len += 1;
            }
        }
        self.starts.push(at);
        let finding = Finding {
            id: self.next_id,
            start: at,
            len: m.len,
            kind: m.kind,
            health: m.health,
            fragments: m.fragments,
            origin,
        };
        self.next_id += 1;
        self.pending.push_back(Pending {
            finding,
            ranges,
            end,
        });
    }
}

/// Scans `source`, calling `on_event` for each finding, progress step and unreadable region.
///
/// The first format that recognizes a sector start and measures a file there wins. The scan reads every block. After an
/// `Intact` finding it examines no head inside the file's pieces (each rounded out to whole sectors; heads in the gaps
/// between pieces are examined); after a `Partial` or `Damaged` one it keeps examining heads, so files inside it are
/// still found (one broken file must not hide others). A finding is emitted once the scan has read past the end of its
/// farthest piece, so that unreadable sectors inside it are known: they are reported as `Unreadable` events (before the
/// `Found`), and an `Intact` finding with any of them inside one of its pieces is downgraded to `Partial`. Findings are
/// emitted in the order they were found (by start), so one found inside another is emitted right after it. A `Partial`
/// finding's own files ([`Format::own_files`], measured with it) are not reported when found by their signature at
/// their start wholly inside it; an `Intact` one of them still skips its range (rulings D12, R10). `measure`
/// reads the whole source (not just the block), so files may cross block boundaries. A measurement whose pieces break
/// the [`Finding::fragments`] contract is a detector bug: it panics in debug builds and is ignored (no finding) in
/// release builds.
///
/// Threads: the scan thread reads up to [`ScanOptions::lookahead_blocks`] blocks ahead and queues each recognized head
/// to `opts.threads` measuring threads; it then takes the blocks in order and, for each head the sequential walk would
/// examine, waits for its measurement. The events, ids and summary are the same for any `threads` and `cache_bytes`.
///
/// After every `Found`, `Damaged` MP4/MOV findings without a `moov` (at most 4096) are paired with orphan `moov` boxes
/// met during the scan (at most 4096, see `pairing`; the start of every finding is kept for it, ruling Q13: a pairing's
/// first piece ends where another file starts, and a file starting inside its second piece makes it at most
/// `Partial`; a live file's video is no candidate, and on a volume's cluster grid the break points on the grid are
/// tried first, ruling D13): each video put back together is emitted as an
/// [`Event::Updated`] with the same id and 2 pieces, then `scan` returns. An orphan `moov` candidate is a box start whose
/// first 16 bytes are a size in `[24, MOOV_MAX_LEN + 8]`, `moov`, any 4 bytes and `mvhd`, found in every block the scan
/// reads; one whose header overlaps a committed skip range (those blocks may not be read at all) or lies inside a
/// non-`Damaged` MP4/MOV/CR3/HEIC finding is dropped.
///
/// After a quick format (ruling D11): every block read is also searched for old directories at the cluster starts of
/// each volume (a FAT `.`/`..` pair, an exFAT entry set), not inside a live range, a walked directory or (head
/// included) a committed skip range; at most 65,536. After the pairing they are parsed, and each file entry names the
/// finding without origin that starts at its first cluster with its kind: that finding comes out as an `Updated` with
/// `origin { state: Orphaned, placement: Carved, .. }` (folder `?` where the old root's name is lost), at most
/// `Partial` when its recorded size disagrees (ruling D6). One `Updated` per id, pairing and naming merged, in id order.
/// See the core README ("File systems: after a quick format").
///
/// The scan and every `measure` read through one [`CachedSource`] of `opts.cache_bytes`, so each sector is read from
/// `source` once and a sector found unreadable is never read again (the pairing step reads the samples it checks and
/// the `moov`s it loads again, through the same cache). A block wholly inside an `Intact` finding whose bytes
/// have all gone through the cache already (read by `measure`) is not read again: its unreadable sectors come from the
/// cache's bad map.
///
/// File systems (`opts.filesystems`, ruling D1): before the first block the FAT/exFAT listing is read (metadata through
/// the cache; the FAT32 high-word heads through a probe over `source` that refuses ranges the cache knows to be bad),
/// and one [`Event::Volume`] is emitted per volume. For each distinct first-piece start one listed photo/video is kept
/// (live before deleted, then the latest `modified`, then listing order). At the sector holding it, before the
/// unreadable and skip checks, its merged finding is added and the carving head there is consumed; its job (a view of
/// its pieces measured, plus the carving at its start for a deleted file) is never dropped for a skip range, and every
/// commit path without a worker result runs the same `fs_job`. Nothing is skipped because of the file system: a live
/// file of another kind only consumes the head at its first cluster, and a finding that is not a live file's and
/// overlaps a live file is at most `Partial` (ruling D7). See `fs` and the core README ("Deduplication").
///
/// `opts.sector` is raised to at least 16 (the head length) and `opts.block` rounded down to whole sectors, so a head
/// never straddles two blocks. Cancellation is checked before each block examined or read ahead, before and after
/// every event, while waiting for a measurement and during the pairing step: once it is seen no further event is
/// emitted, the measuring threads drop their remaining jobs, and the scan returns `Err(Cancelled)` after they have all
/// stopped; findings still pending and `Updated` events not yet emitted are dropped. The cache also checks it before each
/// window load and each sector of a sector-by-sector resolution (ruling D3), so a running measurement ends quickly.
///
/// A source that vanishes (ruling D1: `DeviceGone`, or a short read before its length, seen by the cache or by the
/// FAT32 high-word probe) stops the scan like a cancel, with `Err(DeviceGone)`; its sectors are never reported
/// `Unreadable`, so the events emitted are a prefix of the scan's events on the whole source. A listing read while
/// cancelled or after the source vanished is never used (ruling P3): no `Volume` is emitted.
pub fn scan(
    source: &dyn Source,
    formats: &[Box<dyn Format>],
    opts: &ScanOptions,
    cancel: &CancelToken,
    on_event: &mut dyn FnMut(Event),
) -> Result<ScanSummary, EngineError> {
    let sector = opts.sector_len();
    let block = opts.block_len();
    let lookahead = opts.lookahead_blocks();
    let threads = opts.resolved_threads();
    let workers = if threads > 1 { threads } else { 0 };
    let cache = CachedSource::new(source, sector, opts.cache_bytes.max(CACHE_WINDOW))
        .with_cancel(cancel.clone());
    let mut shared = Shared::new(cache, formats, cancel, sector);
    let total = shared.total;
    // The file systems first (ruling D1): metadata through the cache, the high-word heads through a probe over the raw
    // source that refuses what the cache knows to be unreadable. Nothing is skipped because of them (ruling R10).
    let (volumes, listing_truncated): (Vec<Volume>, bool) = if opts.filesystems {
        let listing = crate::filesystem::read_with(
            &shared.cache,
            &SourceProbe::beside(source, &shared.cache),
            cancel,
        );
        let out = (listing.volumes.clone(), listing.truncated);
        shared.fs = FsPlan::new(listing, sector, total);
        out
    } else {
        (Vec::new(), false)
    };
    // A listing read while cancelled or after the source vanished may lack what it could not read: never used (P3).
    shared.check()?;
    #[cfg(test)]
    {
        shared.reuse = !opts.no_reuse;
        shared.drop_fs_jobs = opts.drop_fs_jobs;
    }
    let shared = shared;
    let grids = volumes
        .iter()
        .map(|v| {
            let heap_len = u64::from(v.cluster_count).saturating_mul(v.cluster_size);
            (
                v.heap_offset,
                v.cluster_size,
                v.heap_offset.saturating_add(heap_len),
            )
        })
        .collect();

    std::thread::scope(|s| {
        let _close = CloseQueue(&shared);
        for _ in 0..workers {
            s.spawn(|| shared.work());
        }
        let mut commit = Commit {
            shared: &shared,
            on_event,
            summary: ScanSummary::default(),
            run: None,
            pending: VecDeque::new(),
            next_id: 1,
            moovs: Vec::new(),
            own_moovs: Runs::default(),
            videos: Vec::new(),
            capped: false,
            starts: Vec::new(),
            next_kept: 0,
            own: BTreeMap::new(),
            own_len: 0,
            grids,
            orphan_heads: volumes.iter().map(head_len).collect(),
            volumes: volumes.clone(),
            dirs: Vec::new(),
            carved: Vec::new(),
            orphans_capped: false,
        };
        commit.summary.listing_truncated = listing_truncated;
        for volume in &volumes {
            commit.emit(Event::Volume {
                volume: volume.clone(),
                listing_truncated,
            })?;
        }
        let mut finders = Finders {
            orphan: (!volumes.is_empty()).then(|| OrphanFinder::new(&volumes)),
            ..Finders::default()
        };
        let mut ahead: VecDeque<Block> = VecDeque::with_capacity(lookahead);
        let mut fill_pos = 0u64;
        loop {
            shared.check()?;
            while ahead.len() < lookahead && fill_pos < total {
                shared.check()?;
                let end = (fill_pos + block).min(total);
                ahead.push_back(shared.fill(fill_pos, end, workers > 0, &mut finders)?);
                fill_pos = end;
                shared.cache.forget_coverage_below(fill_pos);
            }
            let Some(mut b) = ahead.pop_front() else {
                break;
            };
            let read = b.end;
            let (span, moovs, capped) =
                ((b.pos, b.end), std::mem::take(&mut b.moovs), b.moovs_capped);
            let (orphans, orphans_capped) = (std::mem::take(&mut b.orphans), b.orphans_capped);
            commit.block(b)?;
            commit.drop_own_below(read);
            commit.keep_moovs(span, moovs, capped);
            commit.keep_orphans(span, orphans, orphans_capped);
            // A `moov` header may start up to 15 bytes before the next block, an old directory's head up to
            // `MAX_HEAD - 1`: keep the skip ranges that could still overlap them.
            let keep_from = read.saturating_sub(MOOV_HEAD);
            shared.prune_skips(read.saturating_sub(MOOV_HEAD.max(MAX_HEAD)));
            commit.own_moovs.drop_below(keep_from);
            commit.emit(Event::Progress { read, total })?;
        }
        commit.flush_run()?;
        while let Some(p) = commit.pending.pop_front() {
            commit.emit_found(p)?;
        }
        if total == 0 {
            commit.emit(Event::Progress { read: 0, total })?;
        }
        let paired = commit.pair()?;
        commit.update(paired)?;
        // The pairing and the old directories read through the cache too: a source gone then may have changed them.
        shared.check()?;
        Ok(commit.summary)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn within_needs_every_byte_inside() {
        let f = |start, len| Fragment { start, len };
        let outer = [f(1000, 1000), f(2000, 500), f(5000, 100)];
        assert!(within(&[f(1000, 1000)], &outer));
        // Across two adjacent pieces, and several ranges each inside.
        assert!(within(&[f(1500, 900)], &outer));
        assert!(within(&[f(1100, 10), f(5000, 100)], &outer));
        // One byte out, before, after or in a gap.
        assert!(!within(&[f(999, 10)], &outer));
        assert!(!within(&[f(2400, 101)], &outer));
        assert!(!within(&[f(1100, 10), f(4999, 2)], &outer));
        assert!(!within(&[f(3000, 1)], &outer));
    }
}
