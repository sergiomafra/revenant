//! A read cache shared by the scan and the format detectors, so each sector of the source is read from it once, plus a
//! bounded map of the sectors found unreadable, so a known bad sector is never read again.
//!
//! The source is read in aligned windows of [`CACHE_WINDOW`] bytes (rounded down to whole sectors). A window is loaded with
//! one read; if that fails it is resolved sector by sector: unreadable sectors become zeros in the window and enter the
//! shared bad map. Loaded windows are kept, least recently used first out, up to the capacity. Loading a window again
//! after it was evicted reads around the bad runs already known and never touches them again. A read that overlaps an
//! unreadable sector fails every time, as the source would, without touching the source again.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

use crate::EngineError;
use crate::source::{Source, VolumeId};

/// Bytes per cache window (rounded down to a whole number of sectors, at least one).
pub(crate) const CACHE_WINDOW: usize = 1 << 20;
/// Most merged runs the bad map remembers. Past it, new unreadable sectors are not remembered: they are read again when
/// their window is loaded again (slower, same results).
const MAX_BAD_RUNS: usize = 1 << 20;
/// Most merged runs of coverage kept; the lowest are dropped past it (a dropped run only means a block is read again).
const MAX_COVERAGE_RUNS: usize = 1 << 16;

/// Merged, non-overlapping, non-adjacent half-open runs `start -> end`.
#[derive(Default)]
struct Runs(BTreeMap<u64, u64>);

impl Runs {
    /// Adds `[start, end)`, merging with the runs it overlaps or touches. Adds nothing and returns `false` when that would
    /// need a new run while there are already `cap` runs.
    fn add(&mut self, start: u64, end: u64, cap: usize) -> bool {
        let mut s = start;
        let mut e = end;
        if let Some((&ps, &pe)) = self.0.range(..start).next_back()
            && pe >= start
        {
            s = ps;
        }
        let touched: Vec<u64> = self.0.range(s..=e).map(|(&k, _)| k).collect();
        if touched.is_empty() && self.0.len() >= cap {
            return false;
        }
        for k in touched {
            if let Some(ke) = self.0.remove(&k) {
                e = e.max(ke);
            }
        }
        self.0.insert(s, e);
        true
    }

    /// The runs overlapping `[start, end)`, clipped to it, as `(start, len)`, ascending.
    fn within(&self, start: u64, end: u64) -> Vec<(u64, u64)> {
        let mut out = Vec::new();
        if start >= end {
            return out;
        }
        if let Some((_, &pe)) = self.0.range(..start).next_back()
            && pe > start
        {
            out.push((start, pe.min(end) - start));
        }
        out.extend(self.0.range(start..end).map(|(&s, &e)| (s, e.min(end) - s)));
        out
    }

    /// Whether one run holds all of `[start, end)` (always for an empty range).
    fn covers(&self, start: u64, end: u64) -> bool {
        start >= end
            || self
                .0
                .range(..=start)
                .next_back()
                .is_some_and(|(_, &e)| e >= end)
    }

    /// Drops the runs that end at or before `pos`.
    fn drop_below(&mut self, pos: u64) {
        while let Some((&s, &e)) = self.0.first_key_value()
            && e <= pos
        {
            self.0.remove(&s);
        }
    }

    fn len(&self) -> usize {
        self.0.len()
    }
}

/// A loaded window.
struct Window {
    /// Its bytes (unreadable sectors as zeros); shorter than a full window only at the end of the source.
    data: Vec<u8>,
    /// Every unreadable run in it, as absolute `(start, end)`, ascending.
    bad: Vec<(u64, u64)>,
}

enum Slot {
    /// A thread is loading it; the others wait on `CachedSource::loaded`.
    Loading,
    /// Loaded; the `u64` is its last-use tick (its key in `State::lru`).
    Ready(Arc<Window>, u64),
}

#[derive(Default)]
struct State {
    slots: HashMap<u64, Slot>,
    /// Last-use tick -> window index, oldest first.
    lru: BTreeMap<u64, u64>,
    tick: u64,
    /// Bytes of the `Ready` windows.
    resident: usize,
    bad: Runs,
    /// Windows fully resolved with every unreadable sector in them remembered.
    coverage: Runs,
}

impl State {
    /// Drops the least recently used window, returning it; `None` when no window is loaded.
    fn evict_oldest(&mut self) -> Option<Arc<Window>> {
        let (_, victim) = self.lru.pop_first()?;
        match self.slots.remove(&victim) {
            Some(Slot::Ready(w, _)) => {
                self.resident -= w.data.len();
                Some(w)
            }
            _ => None,
        }
    }
}

/// A read-only memoizing view of a source. `Send + Sync`: any number of threads may read through it at once, and a
/// window being loaded by one of them is waited for by the others, never read twice concurrently.
pub(crate) struct CachedSource<'a> {
    inner: &'a dyn Source,
    len: u64,
    sector: u64,
    /// Window length: a whole number of sectors.
    window: u64,
    capacity: usize,
    bad_cap: usize,
    state: Mutex<State>,
    loaded: Condvar,
}

/// Removes a `Loading` slot left behind if the loading thread panics, so the waiters do not wait forever.
struct LoadingGuard<'c, 'a> {
    cache: &'c CachedSource<'a>,
    index: u64,
    armed: bool,
}

impl Drop for LoadingGuard<'_, '_> {
    fn drop(&mut self) {
        if self.armed {
            self.cache.lock().slots.remove(&self.index);
            self.cache.loaded.notify_all();
        }
    }
}

impl<'a> CachedSource<'a> {
    /// `sector`: bad-sector granularity (the scan's normalized sector size); `capacity`: bytes of windows kept (LRU),
    /// at least one window.
    pub(crate) fn new(inner: &'a dyn Source, sector: u64, capacity: usize) -> Self {
        let sector = sector.max(1);
        let window = (CACHE_WINDOW as u64 / sector).max(1) * sector;
        Self {
            inner,
            len: inner.len(),
            sector,
            window,
            capacity: capacity.max(window as usize),
            bad_cap: MAX_BAD_RUNS,
            state: Mutex::new(State::default()),
            loaded: Condvar::new(),
        }
    }

    /// Every byte of [start, end) has been read, or found unreadable, through this cache.
    pub(crate) fn covered(&self, start: u64, end: u64) -> bool {
        self.lock().coverage.covers(start, end)
    }

    /// Unreadable runs `(start, len)` inside [start, end), merged, ascending.
    pub(crate) fn bad_runs(&self, start: u64, end: u64) -> Vec<(u64, u64)> {
        self.lock().bad.within(start, end)
    }

    /// The scan tells the cache its position: coverage runs entirely below `pos` are pruned (keeps ≤ 65,536 runs).
    pub(crate) fn forget_coverage_below(&self, pos: u64) {
        self.lock().coverage.drop_below(pos);
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The window with index `index`, loading it if needed.
    fn window_at(&self, index: u64) -> Arc<Window> {
        let mut st = self.lock();
        loop {
            let state = &mut *st;
            match state.slots.get_mut(&index) {
                Some(Slot::Ready(w, used)) => {
                    let w = Arc::clone(w);
                    state.lru.remove(used);
                    state.tick += 1;
                    *used = state.tick;
                    state.lru.insert(state.tick, index);
                    return w;
                }
                Some(Slot::Loading) => {
                    st = self.loaded.wait(st).unwrap_or_else(PoisonError::into_inner);
                }
                None => break,
            }
        }
        let start = index * self.window;
        let want = (self.len - start).min(self.window) as usize;
        st.slots.insert(index, Slot::Loading);
        let known = st.bad.within(start, start + want as u64);
        // Make room before loading: the evicted window's buffer (when nobody else holds it) becomes this window's, so a long
        // scan does not map fresh memory for every window and nothing is kept outside `resident` but in-flight loads.
        let mut buf = Vec::new();
        while st.resident + want > self.capacity
            && let Some(w) = st.evict_oldest()
        {
            if buf.capacity() == 0
                && let Ok(w) = Arc::try_unwrap(w)
            {
                buf = w.data;
            }
        }
        drop(st);

        let mut guard = LoadingGuard {
            cache: self,
            index,
            armed: true,
        };
        let (window, new_bad) = self.load(buf, start, want, &known);
        let window = Arc::new(window);

        let mut st = self.lock();
        let state = &mut *st;
        let mut complete = true;
        for &(s, e) in &new_bad {
            complete &= state.bad.add(s, e, self.bad_cap);
        }
        if complete && window.data.len() == want {
            state.coverage.add(start, start + want as u64, usize::MAX);
            while state.coverage.len() > MAX_COVERAGE_RUNS {
                state.coverage.0.pop_first();
            }
        }
        state.tick += 1;
        state.lru.insert(state.tick, index);
        state.resident += window.data.len();
        state
            .slots
            .insert(index, Slot::Ready(Arc::clone(&window), state.tick));
        // Concurrent loads may have filled the room made above.
        while state.resident > self.capacity
            && state
                .lru
                .first_key_value()
                .is_some_and(|(_, &v)| v != index)
        {
            state.evict_oldest();
        }
        guard.armed = false;
        drop(st);
        self.loaded.notify_all();
        window
    }

    /// Reads the window `[start, start + want)` from the inner source around the `known` bad runs (`(start, len)`,
    /// ascending, inside it), which are never touched. Each stretch between them is read with one read, or sector by sector
    /// if that fails. Returns the window and the unreadable runs found now (absolute `(start, end)`, ascending).
    ///
    /// `data` is a recycled buffer (or empty): every byte kept is overwritten by a read or zeroed.
    fn load(
        &self,
        mut data: Vec<u8>,
        start: u64,
        want: usize,
        known: &[(u64, u64)],
    ) -> (Window, Vec<(u64, u64)>) {
        let end = start + want as u64;
        data.resize(want, 0);
        let mut bad: Vec<(u64, u64)> = Vec::new();
        let mut new_bad: Vec<(u64, u64)> = Vec::new();
        // The stretches to read and the known runs, in order: `(from, to, known)`.
        let mut plan = Vec::with_capacity(2 * known.len() + 1);
        let mut cur = start;
        for &(s, l) in known {
            if s > cur {
                plan.push((cur, s, false));
            }
            plan.push((s, s + l, true));
            cur = s + l;
        }
        if cur < end {
            plan.push((cur, end, false));
        }
        let mut filled = want;
        'plan: for (from, to, is_known) in plan {
            if is_known {
                data[(from - start) as usize..(to - start) as usize].fill(0);
                push_run(&mut bad, from, to);
                continue;
            }
            let rel = (from - start) as usize;
            let stretch = &mut data[rel..(to - start) as usize];
            match self.inner.read_at(from, stretch) {
                Ok(n) if n < stretch.len() => {
                    filled = rel + n; // the source ends (or shrank) here
                    break 'plan;
                }
                Ok(_) => {}
                Err(_) => {
                    for (i, chunk) in stretch.chunks_mut(self.sector as usize).enumerate() {
                        let at = from + i as u64 * self.sector;
                        match self.inner.read_at(at, chunk) {
                            Ok(n) if n < chunk.len() => {
                                filled = (at - start) as usize + n;
                                break 'plan;
                            }
                            Ok(_) => {}
                            Err(_) => {
                                chunk.fill(0);
                                let to = at + chunk.len() as u64;
                                push_run(&mut bad, at, to);
                                push_run(&mut new_bad, at, to);
                            }
                        }
                    }
                }
            }
        }
        data.truncate(filled);
        let data_end = start + filled as u64;
        bad.retain(|&(s, _)| s < data_end);
        (Window { data, bad }, new_bad)
    }

    #[cfg(test)]
    fn with_bad_cap(mut self, cap: usize) -> Self {
        self.bad_cap = cap;
        self
    }

    /// Bytes of window data the cache holds (loaded windows; no buffer is kept outside them).
    #[cfg(test)]
    fn held(&self) -> usize {
        self.lock().resident
    }

    #[cfg(test)]
    fn remembered_bad_runs(&self) -> usize {
        self.lock().bad.len()
    }
}

/// Appends `[s, e)` to ascending runs, merging with the last one when they touch.
fn push_run(runs: &mut Vec<(u64, u64)>, s: u64, e: u64) {
    match runs.last_mut() {
        Some(last) if last.1 >= s => last.1 = last.1.max(e),
        _ => runs.push((s, e)),
    }
}

impl Source for CachedSource<'_> {
    fn len(&self) -> u64 {
        self.len
    }

    /// Exactly what the inner source would return, memoized. A range overlapping an unreadable sector fails with the
    /// offset of the first such sector in it, every time, without reading that sector again.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, EngineError> {
        if offset >= self.len {
            return Ok(0);
        }
        let want = (self.len - offset).min(buf.len() as u64) as usize;
        let mut done = 0;
        while done < want {
            let pos = offset + done as u64;
            let index = pos / self.window;
            let w = self.window_at(index);
            let from = (pos - index * self.window) as usize;
            if from >= w.data.len() {
                break; // the source shrank
            }
            let n = (w.data.len() - from).min(want - done);
            let end = pos + n as u64;
            if let Some(&(s, _)) = w.bad.iter().find(|&&(s, e)| s < end && e > pos) {
                return Err(EngineError::Read {
                    offset: s.max(pos - pos % self.sector),
                    cause: std::io::Error::other("unreadable sector"),
                });
            }
            buf[done..done + n].copy_from_slice(&w.data[from..from + n]);
            done += n;
            if (w.data.len() as u64) < self.window && from + n == w.data.len() {
                break; // the end of the source
            }
        }
        Ok(done)
    }

    fn volume(&self) -> Option<VolumeId> {
        self.inner.volume()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    const MIB: u64 = 1 << 20;

    /// In-memory source that fails every read touching one of `bad` (`[start, end)`) and counts what reaches it.
    struct Mem {
        data: Vec<u8>,
        bad: Vec<(u64, u64)>,
        calls: AtomicU64,
        bad_attempts: AtomicU64,
    }

    impl Mem {
        fn new(len: u64, seed: u64, bad: Vec<(u64, u64)>) -> Self {
            let mut rng = Rng(seed);
            Self {
                data: (0..len).map(|_| rng.next() as u8).collect(),
                bad,
                calls: AtomicU64::new(0),
                bad_attempts: AtomicU64::new(0),
            }
        }
    }

    impl Source for Mem {
        fn len(&self) -> u64 {
            self.data.len() as u64
        }

        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, EngineError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let end = offset + buf.len() as u64;
            if self.bad.iter().any(|&(s, e)| offset < e && end > s) {
                self.bad_attempts.fetch_add(1, Ordering::SeqCst);
                return Err(EngineError::Read {
                    offset,
                    cause: std::io::Error::other("injected"),
                });
            }
            let len = self.data.len() as u64;
            if offset >= len {
                return Ok(0);
            }
            let n = (len - offset).min(buf.len() as u64) as usize;
            buf[..n].copy_from_slice(&self.data[offset as usize..offset as usize + n]);
            Ok(n)
        }

        fn volume(&self) -> Option<VolumeId> {
            None
        }
    }

    /// splitmix64.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
    }

    /// `Ok(bytes)` or `Err(offset)` of one read.
    fn read(s: &dyn Source, offset: u64, len: usize) -> Result<Vec<u8>, u64> {
        let mut buf = vec![0u8; len];
        match s.read_at(offset, &mut buf) {
            Ok(n) => {
                buf.truncate(n);
                Ok(buf)
            }
            Err(EngineError::Read { offset, .. }) => Err(offset),
            Err(e) => panic!("{e:?}"),
        }
    }

    /// Every 512-byte sector of `[start, end)` read one by one.
    fn sectors(s: &dyn Source, start: u64, end: u64) -> Vec<Result<Vec<u8>, u64>> {
        (start..end)
            .step_by(512)
            .map(|at| read(s, at, 512))
            .collect()
    }

    #[test]
    fn serves_identical_bytes() {
        let inner = Mem::new(16 * MIB, 1, Vec::new());
        let cache = CachedSource::new(&inner, 512, 4 * MIB as usize);
        let mut rng = Rng(2);
        for _ in 0..1000 {
            let offset = rng.next() % (17 * MIB);
            let len = (rng.next() % (3 * MIB + 1)) as usize;
            assert_eq!(
                read(&cache, offset, len),
                read(&inner, offset, len),
                "{offset} {len}"
            );
        }
    }

    #[test]
    fn known_bad_sector_is_never_served_as_data() {
        let bad = MIB + 4096;
        let inner = Mem::new(4 * MIB, 3, vec![(bad, bad + 512)]);
        let cache = CachedSource::new(&inner, 512, 64 * MIB as usize);
        for _ in 0..3 {
            assert_eq!(read(&cache, MIB, MIB as usize), Err(bad));
        }
        assert_eq!(read(&cache, bad, 512), Err(bad));
        assert_eq!(read(&cache, bad + 512, 512), read(&inner, bad + 512, 512));
        assert!(read(&cache, bad + 512, 512).is_ok());
        assert_eq!(cache.bad_runs(0, 4 * MIB), vec![(bad, 512)]);
        assert!(inner.bad_attempts.load(Ordering::SeqCst) <= 2);
    }

    #[test]
    fn concurrent_readers_load_each_window_once() {
        let inner = Mem::new(16 * MIB, 4, Vec::new());
        let cache = CachedSource::new(&inner, 512, 64 * MIB as usize);
        std::thread::scope(|s| {
            for t in 0..8u64 {
                let cache = &cache;
                let inner = &inner;
                s.spawn(move || {
                    for k in 0..256u64 {
                        let at = (t * 2 * MIB + k * 64 * 1024) % (16 * MIB);
                        assert_eq!(read(cache, at, 64 * 1024), read(inner, at, 64 * 1024));
                    }
                });
            }
        });
        // The reads made by the threads' own comparisons go to `inner` directly: subtract them.
        let window_reads = inner.calls.load(Ordering::SeqCst) - 8 * 256;
        assert_eq!(window_reads, 16);
    }

    #[test]
    fn evicted_window_does_not_retry_a_known_bad_sector() {
        let bad = MIB + 4096;
        let inner = Mem::new(16 * MIB, 5, vec![(bad, bad + 512)]);
        let cache = CachedSource::new(&inner, 512, 2 * MIB as usize);
        let whole = read(&cache, MIB, MIB as usize);
        let first = sectors(&cache, MIB, 2 * MIB);
        for at in (4 * MIB..12 * MIB).step_by(MIB as usize) {
            read(&cache, at, MIB as usize).unwrap();
        }
        assert_eq!(read(&cache, MIB, MIB as usize), whole);
        assert_eq!(sectors(&cache, MIB, 2 * MIB), first);
        assert_eq!(whole, Err(bad));
        assert_eq!(first.iter().filter(|r| r.is_err()).count(), 1);
        assert!(inner.bad_attempts.load(Ordering::SeqCst) <= 2);
    }

    #[test]
    fn bad_map_is_capped() {
        // Every other sector of the first 4 MiB is unreadable: 4096 runs.
        let bad: Vec<(u64, u64)> = (0..4 * MIB).step_by(1024).map(|s| (s, s + 512)).collect();
        let inner = Mem::new(8 * MIB, 6, bad);
        let uncapped = CachedSource::new(&inner, 512, MIB as usize);
        let capped = CachedSource::new(&inner, 512, MIB as usize).with_bad_cap(100);
        for _ in 0..2 {
            for at in (0..8 * MIB).step_by(MIB as usize) {
                assert_eq!(
                    read(&capped, at, MIB as usize),
                    read(&uncapped, at, MIB as usize)
                );
            }
            assert_eq!(sectors(&capped, 0, 8 * MIB), sectors(&uncapped, 0, 8 * MIB));
        }
        assert_eq!(uncapped.remembered_bad_runs(), 4096);
        assert!(capped.remembered_bad_runs() <= 100);
        assert_eq!(
            capped.bad_runs(0, 8 * MIB).len(),
            capped.remembered_bad_runs()
        );
        assert!(!capped.covered(MIB, 2 * MIB));
    }

    #[test]
    fn memory_is_bounded() {
        let inner = Mem::new(64 * MIB, 7, Vec::new());
        let cache = CachedSource::new(&inner, 512, 8 * MIB as usize);
        for at in (0..64 * MIB).step_by(256 * 1024) {
            read(&cache, at, 256 * 1024).unwrap();
            assert!(cache.held() <= 8 * MIB as usize);
        }
        assert_eq!(cache.held(), 8 * MIB as usize);
    }

    #[test]
    fn coverage_follows_loaded_windows_and_is_pruned() {
        let bad = MIB + 4096;
        let inner = Mem::new(4 * MIB, 8, vec![(bad, bad + 512)]);
        let cache = CachedSource::new(&inner, 512, 64 * MIB as usize);
        assert!(!cache.covered(0, 1));
        read(&cache, 0, MIB as usize).unwrap();
        assert!(cache.covered(0, MIB) && !cache.covered(0, 2 * MIB));
        // A window holding an unreadable sector is covered once resolved.
        assert_eq!(read(&cache, bad, 512), Err(bad));
        assert!(cache.covered(0, 2 * MIB) && !cache.covered(0, 2 * MIB + 1));
        cache.forget_coverage_below(2 * MIB);
        assert!(!cache.covered(0, MIB));
        assert_eq!(cache.bad_runs(0, 4 * MIB), vec![(bad, 512)]);
    }
}
