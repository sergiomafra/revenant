//! The scan: walks a source sector by sector, lets each format recognize and measure files, and reports events. Blocks
//! are read ahead and candidate files measured on worker threads ([`pipeline`]); this module's commit loop takes the
//! results in source order, so the events are those of a sequential scan.

mod pipeline;

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::EngineError;
use crate::cache::{CACHE_WINDOW, CachedSource};
use crate::formats::{FileKind, Format, Fragment, Health, Measurement};
use crate::source::Source;
use pipeline::{Block, CloseQueue, Outcome, Shared};

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
}

impl Finding {
    /// The file's byte ranges in file order: `[Fragment { start, len }]` for a contiguous file, else `fragments`.
    pub fn ranges(&self) -> Vec<Fragment> {
        ranges(self.start, self.len, &self.fragments)
    }
}

/// Whether a measurement at `at` keeps the pieces contract: no pieces, or at least 2 in file order whose first starts at
/// `at`, whose lengths sum to `len`, that do not overlap and that lie inside a source of `source_len` bytes.
fn pieces_ok(at: u64, m: &Measurement, source_len: u64) -> bool {
    let p = &m.fragments;
    if p.is_empty() {
        return true;
    }
    let inside = p.iter().all(|f| {
        f.start
            .checked_add(f.len)
            .is_some_and(|end| end <= source_len)
    });
    let sum = p.iter().try_fold(0u64, |acc, f| acc.checked_add(f.len));
    let mut sorted = p.clone();
    sorted.sort_by_key(|f| f.start);
    let disjoint = sorted
        .windows(2)
        .all(|w| w[0].start.saturating_add(w[0].len) <= w[1].start);
    p.len() >= 2 && p[0].start == at && sum == Some(m.len) && inside && disjoint
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
/// image's coefficients, come on top) and what each running measurement holds (for example a `moov` of at most 16 MiB),
/// plus about 200 B per recognized sector start in the read-ahead (at most lookahead × block / sector of them). So with
/// the defaults on a 16-thread machine, about 256 MiB + 17 × 5 MiB + 512 MiB, plus the measurements' own buffers.
pub struct ScanOptions {
    /// Granularity of candidate starts and of unreadable-region handling, in bytes. Above 512, the detectors see a sector that
    /// failed only in part as wholly unreadable (health stays conservative).
    pub sector: u64,
    /// Bytes read per block (rounded down to a whole number of sectors, at least one).
    pub block: usize,
    /// Bytes of recently read data the scan and the detectors share, so each sector is read from the source once (values
    /// below 1 MiB are raised to it). The read-ahead is sized to fit in it (see [`ScanOptions::lookahead_blocks`]).
    pub cache_bytes: usize,
    /// Threads that measure candidate files: 0 (the default) means one per available core, at most 16; 1 measures on
    /// the scan thread itself; other values are capped at 64. The events are the same for any value.
    pub threads: usize,
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
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct ScanSummary {
    pub found: u64,
    pub unreadable_bytes: u64,
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
}

impl Commit<'_, '_> {
    /// Emits `event` unless the scan is cancelled; a cancel seen afterwards stops the scan before anything else is
    /// emitted.
    fn emit(&mut self, event: Event) -> Result<(), EngineError> {
        if self.shared.cancel.is_cancelled() {
            return Err(EngineError::Cancelled);
        }
        (self.on_event)(event);
        if self.shared.cancel.is_cancelled() {
            return Err(EngineError::Cancelled);
        }
        Ok(())
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
        self.emit(Event::Found(p.finding))
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
            if block.bad.as_ref().is_some_and(|b| b[idx]) {
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
            while heads.next_if(|h| h.at < at).is_some() {}
            if !self.shared.skipped(at) {
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
                            Outcome::NotMeasured => self.measure(at, &h.head[..h.head_len]),
                            Outcome::Cancelled => return Err(EngineError::Cancelled),
                        }
                    }
                    None if block.deferred => self
                        .shared
                        .head_at(at)
                        .and_then(|(head, n)| self.measure(at, &head[..n])),
                    None => None,
                };
                if let Some(m) = found {
                    self.add(at, m);
                }
            }
            at = sector_end;
        }
        Ok(())
    }

    fn measure(&self, at: u64, head: &[u8]) -> Option<Measurement> {
        pipeline::measure(self.shared.formats, &self.shared.cache, at, head)
    }

    /// Turns the measurement at `at` into a pending finding; an `Intact` one adds a skip range per piece.
    fn add(&mut self, at: u64, m: Measurement) {
        if m.len == 0 {
            return;
        }
        // A detector bug: caught in debug builds, ignored (no finding) in release.
        let ok = pieces_ok(at, &m, self.shared.total);
        debug_assert!(ok, "pieces contract broken at {at}: {m:?}");
        if !ok {
            return;
        }
        let sector = self.shared.sector;
        let round_up = |x: u64| x.div_ceil(sector).saturating_mul(sector);
        let ranges = ranges(at, m.len, &m.fragments);
        let end = ranges
            .iter()
            .map(|r| round_up(r.start.saturating_add(r.len)))
            .max()
            .unwrap_or(at);
        if m.health == Health::Intact {
            let mut skips = pipeline::lock(&self.shared.skips);
            for r in &ranges {
                skips.add(
                    r.start / sector * sector,
                    round_up(r.start.saturating_add(r.len)),
                    usize::MAX,
                );
            }
        }
        let finding = Finding {
            id: self.next_id,
            start: at,
            len: m.len,
            kind: m.kind,
            health: m.health,
            fragments: m.fragments,
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
/// emitted in the order they were found (by start), so one found inside another is emitted right after it. `measure`
/// reads the whole source (not just the block), so files may cross block boundaries. A measurement whose pieces break
/// the [`Finding::fragments`] contract is a detector bug: it panics in debug builds and is ignored (no finding) in
/// release builds.
///
/// Threads: the scan thread reads up to [`ScanOptions::lookahead_blocks`] blocks ahead and queues each recognized head
/// to `opts.threads` measuring threads; it then takes the blocks in order and, for each head the sequential walk would
/// examine, waits for its measurement. The events, ids and summary are the same for any `threads` and `cache_bytes`.
///
/// The scan and every `measure` read through one [`CachedSource`] of `opts.cache_bytes`, so each sector is read from
/// `source` once and a sector found unreadable is never read again. A block wholly inside an `Intact` finding whose bytes
/// have all gone through the cache already (read by `measure`) is not read again: its unreadable sectors come from the
/// cache's bad map.
///
/// `opts.sector` is raised to at least 16 (the head length) and `opts.block` rounded down to whole sectors, so a head
/// never straddles two blocks. Cancellation is checked before each block examined or read ahead, before and after
/// every event and while waiting for a measurement: once it is seen no further event is emitted, the measuring threads
/// drop their remaining jobs, and the scan returns `Err(Cancelled)` after they have all stopped; findings still pending
/// are dropped.
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
    let cache = CachedSource::new(source, sector, opts.cache_bytes.max(CACHE_WINDOW));
    let shared = Shared::new(cache, formats, cancel, sector);
    let total = shared.total;

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
        };
        let mut ahead: VecDeque<Block> = VecDeque::with_capacity(lookahead);
        let mut fill_pos = 0u64;
        loop {
            if cancel.is_cancelled() {
                return Err(EngineError::Cancelled);
            }
            while ahead.len() < lookahead && fill_pos < total {
                if cancel.is_cancelled() {
                    return Err(EngineError::Cancelled);
                }
                let end = (fill_pos + block).min(total);
                ahead.push_back(shared.fill(fill_pos, end, workers > 0));
                fill_pos = end;
                shared.cache.forget_coverage_below(fill_pos);
            }
            let Some(b) = ahead.pop_front() else { break };
            let read = b.end;
            commit.block(b)?;
            shared.prune_skips(read);
            commit.emit(Event::Progress { read, total })?;
        }
        commit.flush_run()?;
        while let Some(p) = commit.pending.pop_front() {
            commit.emit_found(p)?;
        }
        if total == 0 {
            commit.emit(Event::Progress { read: 0, total })?;
        }
        Ok(commit.summary)
    })
}
