//! What the reassembly step spends, for the budget measure (`tests::reassembly_budget_measure`, ruling Q9): built only
//! for the crate's unit tests, recording only while [`record`] holds its guard. Nothing here changes what the step does.

use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use super::Budget;
use crate::formats::{FileKind, Health};
use crate::scan::Finding;

thread_local! {
    /// Recording on this thread: only the thread holding [`record`]'s guard records, so the unit tests running beside it
    /// (other scans, other reassembly steps) never add to what it measures.
    static ON: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
/// One recording at a time (the unit tests run in parallel).
static ONE: Mutex<()> = Mutex::new(());
static SEARCHES: Mutex<Vec<SearchStat>> = Mutex::new(Vec::new());
static FILES: Mutex<Vec<FileStat>> = Mutex::new(Vec::new());
static STEPS: Mutex<Vec<StepStat>> = Mutex::new(Vec::new());

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One [`super::search`]: its tries and bytes (the re-check's included, and apart), and how it ended.
#[derive(Clone, Debug, Default)]
pub(crate) struct SearchStat {
    pub tries: u64,
    pub read: u64,
    pub recheck_tries: u64,
    pub recheck_read: u64,
    /// The main search stopped on the file's try or read budget.
    pub budget_hit: bool,
    /// The re-check used all of its tries.
    pub recheck_spent: bool,
    pub exhaustive: bool,
    pub complete: bool,
    pub unique: bool,
}

/// One candidate of the step: its time, what it took from the scan's budget, its searches and its outcome.
#[derive(Clone, Debug)]
pub(crate) struct FileStat {
    pub id: u64,
    pub kind: FileKind,
    pub before: Health,
    pub start: u64,
    pub len: u64,
    /// `None`: the step left it as it was.
    pub after: Option<Health>,
    pub pieces: usize,
    pub secs: f64,
    pub tries: u64,
    pub read: u64,
    pub searches: Vec<SearchStat>,
}

/// One reassembly step.
#[derive(Clone, Debug)]
pub(crate) struct StepStat {
    pub candidates: usize,
    pub secs: f64,
    pub tries_left: u64,
    pub read_left: u64,
}

/// What was recorded while the guard of [`record`] lived.
pub(crate) struct Recorded {
    pub files: Vec<FileStat>,
    pub steps: Vec<StepStat>,
}

/// Records every reassembly step run until the guard is dropped, then hands what it recorded to [`take`].
pub(crate) fn record() -> MutexGuard<'static, ()> {
    let g = lock(&ONE);
    lock(&SEARCHES).clear();
    lock(&FILES).clear();
    lock(&STEPS).clear();
    ON.with(|o| o.set(true));
    g
}

/// Stops recording and returns what was recorded.
pub(crate) fn take() -> Recorded {
    ON.with(|o| o.set(false));
    Recorded {
        files: std::mem::take(&mut lock(&FILES)),
        steps: std::mem::take(&mut lock(&STEPS)),
    }
}

fn on() -> bool {
    ON.with(std::cell::Cell::get)
}

pub(crate) fn search(s: SearchStat) {
    if on() {
        lock(&SEARCHES).push(s);
    }
}

pub(crate) struct Step(Instant);

impl Step {
    pub(crate) fn begin() -> Step {
        Step(Instant::now())
    }

    pub(crate) fn end(self, candidates: usize, budget: &Budget) {
        if on() {
            lock(&STEPS).push(StepStat {
                candidates,
                secs: self.0.elapsed().as_secs_f64(),
                tries_left: budget.tries_left(),
                read_left: budget.read_left(),
            });
        }
    }
}

pub(crate) struct File {
    at: Instant,
    id: u64,
    kind: FileKind,
    before: Health,
    start: u64,
    len: u64,
    tries: u64,
    read: u64,
}

impl File {
    pub(crate) fn begin(f: &Finding, budget: &Budget) -> File {
        if on() {
            lock(&SEARCHES).clear();
        }
        File {
            at: Instant::now(),
            id: f.id,
            kind: f.kind,
            before: f.health,
            start: f.start,
            len: f.len,
            tries: budget.tries_left(),
            read: budget.read_left(),
        }
    }

    pub(crate) fn end(self, budget: &Budget, new: Option<&Finding>) {
        if !on() {
            return;
        }
        let searches = std::mem::take(&mut *lock(&SEARCHES));
        lock(&FILES).push(FileStat {
            id: self.id,
            kind: self.kind,
            before: self.before,
            start: self.start,
            len: self.len,
            after: new.map(|n| n.health),
            pieces: new.map_or(0, |n| n.fragments.len().max(1)),
            secs: self.at.elapsed().as_secs_f64(),
            tries: self.tries - budget.tries_left(),
            read: self.read - budget.read_left(),
            searches,
        });
    }
}
