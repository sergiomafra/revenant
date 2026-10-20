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
use crate::formats::{Format, Measurement};
use crate::source::Source;

/// How often a wait for a measurement looks at the cancel token.
const CANCEL_POLL: Duration = Duration::from_millis(10);

pub(super) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The first measurement of the formats that recognize `head`, as the sequential scan takes it.
pub(super) fn measure(
    formats: &[Box<dyn Format>],
    source: &dyn Source,
    at: u64,
    head: &[u8],
) -> Option<Measurement> {
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
                measure(self.formats, &self.cache, job.at, &job.head[..job.head_len])
            }));
            job.slot.fill(match result {
                Ok(m) => SlotState::Done(m),
                Err(_) => SlotState::Dropped,
            });
        }
    }

    /// Reads the block `[pos, end)` (or defers it) and, with workers, queues a job for each head to examine.
    pub(super) fn fill(&self, pos: u64, end: u64, workers: bool) -> Block {
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
            return Block {
                pos,
                end,
                bad,
                heads: Vec::new(),
                deferred: true,
            };
        }
        let mut bad: Option<Vec<bool>> = None;
        let mut heads = Vec::new();
        self.cache.visit(pos, end, |from, bytes, runs| {
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
