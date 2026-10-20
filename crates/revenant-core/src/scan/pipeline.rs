//! The scan's read stage and its measuring workers. The read stage (on the scan thread) reads blocks ahead of the
//! commit, finds the sector starts some format recognizes and queues one measuring job per head; worker threads measure
//! them. Which heads count, and in what order, is decided by the commit loop alone (`super::scan`), so the events do not
//! depend on how many workers there are or when they finish.

use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use super::{CancelToken, HEAD_LEN};
use crate::cache::{CachedSource, Runs};
use crate::formats::{CancelScope, Format, MOOV_MAX_LEN, Measurement};
use crate::source::Source;

/// How often a wait for a measurement looks at the cancel token.
const CANCEL_POLL: Duration = Duration::from_millis(10);

pub(super) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The first measurement of the formats that recognize `head`, as the sequential scan takes it. A long measurement
/// gives up once `cancel` is set (its result is then never used).
pub(super) fn measure(
    formats: &[Box<dyn Format>],
    source: &dyn Source,
    at: u64,
    head: &[u8],
    cancel: &CancelToken,
) -> Option<Measurement> {
    let _scope = CancelScope::enter(cancel);
    formats
        .iter()
        .filter(|f| f.recognizes(head))
        .find_map(|f| f.measure(source, at))
}

/// Where a job's result lands.
enum SlotState {
    Waiting,
    Done(Option<Measurement>),
    /// Not measured: the scan was cancelled, the head lies in a committed skip range, or `measure` panicked.
    Dropped,
}

pub(super) struct Slot {
    state: Mutex<SlotState>,
    ready: Condvar,
}

/// A measured job's outcome, as the commit loop sees it.
pub(super) enum Outcome {
    Measured(Option<Measurement>),
    /// The worker did not measure it: the commit loop measures it itself.
    NotMeasured,
    Cancelled,
}

impl Slot {
    fn new() -> Self {
        Self {
            state: Mutex::new(SlotState::Waiting),
            ready: Condvar::new(),
        }
    }

    fn fill(&self, state: SlotState) {
        *lock(&self.state) = state;
        self.ready.notify_all();
    }

    /// Waits for the result, looking at `cancel` every [`CANCEL_POLL`].
    pub(super) fn wait(&self, cancel: &CancelToken) -> Outcome {
        let mut st = lock(&self.state);
        loop {
            if !matches!(*st, SlotState::Waiting) {
                return match std::mem::replace(&mut *st, SlotState::Dropped) {
                    SlotState::Done(m) => Outcome::Measured(m),
                    _ => Outcome::NotMeasured,
                };
            }
            if cancel.is_cancelled() {
                return Outcome::Cancelled;
            }
            st = self
                .ready
                .wait_timeout(st, CANCEL_POLL)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

struct Job {
    at: u64,
    head: [u8; HEAD_LEN],
    head_len: usize,
    slot: Arc<Slot>,
}

/// A sector start some format recognizes.
pub(super) struct Head {
    pub(super) at: u64,
    pub(super) head: [u8; HEAD_LEN],
    pub(super) head_len: usize,
    /// The queued job's result; `None` when there are no workers (the commit loop measures it).
    pub(super) slot: Option<Arc<Slot>>,
}

/// A block read ahead of the commit loop.
pub(super) struct Block {
    pub(super) pos: u64,
    pub(super) end: u64,
    /// Per-sector unreadable flags; `None` when every sector was read.
    pub(super) bad: Option<Vec<bool>>,
    /// Heads to examine, ascending (none in a deferred block).
    pub(super) heads: Vec<Head>,
    /// Not read: wholly inside a committed skip range and already read through the cache (by a measure). Its flags come
    /// from the cache's bad map; a head the commit loop needs in it is read then.
    pub(super) deferred: bool,
    /// Orphan `moov` candidates whose 16-byte header ends in this block (see [`MoovFinder`]), ascending, as (box start,
    /// declared size).
    pub(super) moovs: Vec<(u64, u64)>,
    /// More candidates than [`MAX_BLOCK_MOOVS`] were met in this block: the rest were not kept.
    pub(super) moovs_capped: bool,
}

/// Most `moov` candidates kept per block.
pub(super) const MAX_BLOCK_MOOVS: usize = 4096;

/// Bytes of a candidate `moov` box header looked at: size, `moov`, the first child's size and `mvhd`.
const MOOV_HEAD: usize = 16;

/// Searches the bytes the read stage reads, in order, for orphan `moov` boxes: a box start `q` whose 16 bytes are a size
/// in `[24, MOOV_MAX_LEN + 8]`, `moov`, any 4 bytes and `mvhd`. Keeps the last 15 bytes seen, so a header across two
/// windows or blocks is found too; a block that is not read (deferred) breaks the stream.
#[derive(Default)]
pub(super) struct MoovFinder {
    /// The last bytes seen (at most 15) and where they end.
    tail: Vec<u8>,
    tail_end: u64,
}

/// The declared size, when `b` (16 bytes) is a candidate `moov` box header.
fn moov_head(b: &[u8]) -> Option<u64> {
    let size = u64::from(u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
    (&b[4..8] == b"moov" && &b[12..16] == b"mvhd" && (24..=MOOV_MAX_LEN + 8).contains(&size))
        .then_some(size)
}

impl MoovFinder {
    /// Forgets the tail: the next bytes do not follow the last ones.
    pub(super) fn reset(&mut self) {
        self.tail.clear();
    }

    /// Looks at `bytes`, which start at the absolute offset `from`; pushes each candidate's `(q, size)` (ascending) to
    /// `out` until it holds `cap`, then sets `capped`.
    fn feed(
        &mut self,
        from: u64,
        bytes: &[u8],
        out: &mut Vec<(u64, u64)>,
        cap: usize,
        capped: &mut bool,
    ) {
        let mut push = |q: u64, size: u64| {
            if out.len() < cap {
                out.push((q, size));
            } else {
                *capped = true;
            }
        };
        if self.tail_end != from {
            self.tail.clear();
        }
        // Headers that start in the tail and end in `bytes`.
        if !self.tail.is_empty() {
            let t = self.tail.len();
            let mut joined = [0u8; 2 * (MOOV_HEAD - 1)];
            let more = bytes.len().min(MOOV_HEAD - 1);
            joined[..t].copy_from_slice(&self.tail);
            joined[t..t + more].copy_from_slice(&bytes[..more]);
            for i in 0..t {
                if i + MOOV_HEAD > t
                    && i + MOOV_HEAD <= t + more
                    && let Some(size) = moov_head(&joined[i..i + MOOV_HEAD])
                {
                    push(from - (t - i) as u64, size);
                }
            }
        }
        for hit in memchr::memmem::find_iter(bytes, b"moov") {
            if hit >= 4
                && hit + MOOV_HEAD - 4 <= bytes.len()
                && let Some(size) = moov_head(&bytes[hit - 4..hit + MOOV_HEAD - 4])
            {
                push(from + (hit - 4) as u64, size);
            }
        }
        // Keep the last 15 bytes of the stream.
        let keep = MOOV_HEAD - 1;
        if bytes.len() >= keep {
            self.tail.clear();
            self.tail.extend_from_slice(&bytes[bytes.len() - keep..]);
        } else {
            self.tail.extend_from_slice(bytes);
            let excess = self.tail.len().saturating_sub(keep);
            self.tail.drain(..excess);
        }
        self.tail_end = from + bytes.len() as u64;
    }
}

/// State shared by the scan thread and the workers.
pub(super) struct Shared<'s> {
    pub(super) cache: CachedSource<'s>,
    pub(super) formats: &'s [Box<dyn Format>],
    pub(super) cancel: &'s CancelToken,
    pub(super) sector: u64,
    pub(super) total: u64,
    /// Sector-rounded ranges where no head is examined (the pieces of committed `Intact` findings); only grows, except
    /// that ranges behind the commit loop are pruned.
    pub(super) skips: Mutex<Runs>,
    /// Jobs not yet taken, and whether the queue is closed.
    queue: Mutex<(VecDeque<Job>, bool)>,
    queued: Condvar,
}

/// Closes the job queue when dropped (also on a panic of the scan thread), so the workers stop and the scope can join.
pub(super) struct CloseQueue<'a, 's>(pub(super) &'a Shared<'s>);

impl Drop for CloseQueue<'_, '_> {
    fn drop(&mut self) {
        lock(&self.0.queue).1 = true;
        self.0.queued.notify_all();
    }
}

impl<'s> Shared<'s> {
    pub(super) fn new(
        cache: CachedSource<'s>,
        formats: &'s [Box<dyn Format>],
        cancel: &'s CancelToken,
        sector: u64,
    ) -> Self {
        let total = cache.len();
        Self {
            cache,
            formats,
            cancel,
            sector,
            total,
            skips: Mutex::new(Runs::default()),
            queue: Mutex::new((VecDeque::new(), false)),
            queued: Condvar::new(),
        }
    }

    /// Whether a committed skip range holds the sector start `at`.
    pub(super) fn skipped(&self, at: u64) -> bool {
        lock(&self.skips).covers(at, at + 1)
    }

    /// Drops the skip ranges that end at or before `pos` (the commit loop is past them; every block still to come
    /// starts at or after it).
    pub(super) fn prune_skips(&self, pos: u64) {
        lock(&self.skips).drop_below(pos);
    }

    /// A worker: measures queued jobs in order until the queue is closed. After a cancel it drops every job unmeasured.
    pub(super) fn work(&self) {
        loop {
            let job = {
                let mut q = lock(&self.queue);
                loop {
                    if q.1 {
                        return;
                    }
                    if let Some(job) = q.0.pop_front() {
                        break job;
                    }
                    q = self.queued.wait(q).unwrap_or_else(PoisonError::into_inner);
                }
            };
            if self.cancel.is_cancelled() || self.skipped(job.at) {
                job.slot.fill(SlotState::Dropped);
                continue;
            }
            // A panicking detector must not leave the commit loop waiting: the job is handed back unmeasured, and the
            // commit loop measures it on the scan thread, where the panic surfaces as it would in a sequential scan.
            let result = catch_unwind(AssertUnwindSafe(|| {
                measure(
                    self.formats,
                    &self.cache,
                    job.at,
                    &job.head[..job.head_len],
                    self.cancel,
                )
            }));
            job.slot.fill(match result {
                Ok(m) => SlotState::Done(m),
                Err(_) => SlotState::Dropped,
            });
        }
    }

    /// Reads the block `[pos, end)` (or defers it) and, with workers, queues a job for each head to examine. `finder`
    /// searches the bytes read for orphan `moov` candidates.
    pub(super) fn fill(&self, pos: u64, end: u64, workers: bool, finder: &mut MoovFinder) -> Block {
        let sector = self.sector;
        let sectors = (end - pos).div_ceil(sector) as usize;
        if lock(&self.skips).covers(pos, end) && self.cache.covered(pos, end) {
            let mut bad = None;
            for (s, l) in self.cache.bad_runs(pos, end) {
                let flags = bad.get_or_insert_with(|| vec![false; sectors]);
                let first = ((s - pos) / sector) as usize;
                let last = ((s + l - pos).div_ceil(sector) as usize).min(sectors);
                flags[first..last].fill(true);
            }
            finder.reset();
            return Block {
                pos,
                end,
                bad,
                heads: Vec::new(),
                deferred: true,
                moovs: Vec::new(),
                moovs_capped: false,
            };
        }
        let mut bad: Option<Vec<bool>> = None;
        let mut heads = Vec::new();
        let (mut moovs, mut moovs_capped) = (Vec::new(), false);
        self.cache.visit(pos, end, |from, bytes, runs| {
            finder.feed(from, bytes, &mut moovs, MAX_BLOCK_MOOVS, &mut moovs_capped);
            let mut run = runs.iter().peekable();
            let mut at = from;
            let stop = from + bytes.len() as u64;
            while at < stop {
                let sector_end = (at + sector).min(end);
                while run.next_if(|&&(_, e)| e <= at).is_some() {}
                if run.peek().is_some_and(|&&(s, _)| s < sector_end) {
                    bad.get_or_insert_with(|| vec![false; sectors])
                        [((at - pos) / sector) as usize] = true;
                    at = sector_end;
                    continue;
                }
                let head_len = HEAD_LEN.min((self.total - at) as usize);
                let rel = (at - from) as usize;
                if let Some(head) = bytes.get(rel..rel + head_len)
                    && self.formats.iter().any(|f| f.recognizes(head))
                    && !self.skipped(at)
                {
                    let mut h = [0u8; HEAD_LEN];
                    h[..head_len].copy_from_slice(head);
                    heads.push(Head {
                        at,
                        head: h,
                        head_len,
                        slot: None,
                    });
                }
                at = sector_end;
            }
        });
        if workers && !heads.is_empty() {
            let mut q = lock(&self.queue);
            for h in &mut heads {
                let slot = Arc::new(Slot::new());
                h.slot = Some(Arc::clone(&slot));
                q.0.push_back(Job {
                    at: h.at,
                    head: h.head,
                    head_len: h.head_len,
                    slot,
                });
            }
            drop(q);
            self.queued.notify_all();
        }
        Block {
            pos,
            end,
            bad,
            heads,
            deferred: false,
            moovs,
            moovs_capped,
        }
    }

    /// The head at `at` read now (for a deferred block), if the sector is readable and some format recognizes it.
    pub(super) fn head_at(&self, at: u64) -> Option<([u8; HEAD_LEN], usize)> {
        let mut head = [0u8; HEAD_LEN];
        let want = HEAD_LEN.min((self.total - at) as usize);
        let n = self.cache.read_at(at, &mut head[..want]).ok()?;
        (n == want && self.formats.iter().any(|f| f.recognizes(&head[..n]))).then_some((head, n))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds `data` split at `cuts` and returns the candidates.
    fn find(data: &[u8], cuts: &[usize], cap: usize) -> (Vec<(u64, u64)>, bool) {
        let mut f = MoovFinder::default();
        let (mut out, mut capped) = (Vec::new(), false);
        let mut from = 0;
        for &to in cuts.iter().chain([data.len()].iter()) {
            f.feed(from as u64, &data[from..to], &mut out, cap, &mut capped);
            from = to;
        }
        (out, capped)
    }

    fn header(size: u32) -> Vec<u8> {
        let mut h = size.to_be_bytes().to_vec();
        h.extend_from_slice(b"moov\0\0\0\x6cmvhd");
        h
    }

    #[test]
    fn moov_headers_are_found_across_chunk_boundaries() {
        let mut data = vec![7u8; 4096];
        for (at, size) in [(0, 24), (1000, 4096), (2000, 1 << 24)] {
            data[at..at + 16].copy_from_slice(&header(size));
        }
        // Not candidates: a size below 24 or above 16 MiB + 8, no `mvhd`, a header cut by the end.
        data[3000..3016].copy_from_slice(&header(23));
        data[3100..3116].copy_from_slice(&header((1 << 24) + 9));
        data[3200..3216].copy_from_slice(&header(4096));
        data[3212] = b'x';
        data[4088..4096].copy_from_slice(&header(4096)[..8]);
        let want = vec![(0, 24), (1000, 4096), (2000, 1 << 24)];
        assert_eq!(find(&data, &[], 10), (want.clone(), false));
        // Every split of every header, and many tiny chunks.
        for d in 1..16 {
            assert_eq!(find(&data, &[1000 + d, 2000 + d], 10).0, want, "{d}");
        }
        let tiny: Vec<usize> = (1..4096).step_by(3).collect();
        assert_eq!(find(&data, &tiny, 10).0, want);
        // A gap (a block not read) breaks the stream.
        let mut f = MoovFinder::default();
        let (mut out, mut capped) = (Vec::new(), false);
        f.feed(0, &data[..1008], &mut out, 10, &mut capped);
        f.feed(5000, &data[1008..1016], &mut out, 10, &mut capped);
        assert_eq!(out, vec![(0, 24)]);
        // The cap.
        assert_eq!(find(&data, &[], 2), (want[..2].to_vec(), true));
    }
}
