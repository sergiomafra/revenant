//! A process-wide bound on the memory the JPEG validation decodes use at once, so many measuring threads cannot each
//! hold a large decode at the same time.

use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};

/// Bytes the validation decodes of every scan in this process may hold at once.
pub(crate) static DECODE_BUDGET: Budget = Budget::new(512 << 20);

/// A counting limit on bytes held at once. Requests are served in arrival order, so a large one is never starved by a
/// stream of small ones.
pub(crate) struct Budget {
    limit: u64,
    state: Mutex<State>,
    changed: Condvar,
}

struct State {
    held: u64,
    /// The next ticket handed out.
    next: u64,
    /// The ticket whose turn it is.
    serving: u64,
}

/// Holds `n` bytes of a [`Budget`] until dropped.
pub(crate) struct BudgetGuard<'a> {
    budget: &'a Budget,
    n: u64,
}

impl Budget {
    pub(crate) const fn new(limit: u64) -> Self {
        Self {
            limit,
            state: Mutex::new(State {
                held: 0,
                next: 0,
                serving: 0,
            }),
            changed: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Blocks until `n` more bytes fit under the limit (a request above the limit waits until nothing is held, then runs
    /// alone); released when the guard drops.
    pub(crate) fn acquire(&self, n: u64) -> BudgetGuard<'_> {
        let mut st = self.lock();
        let ticket = st.next;
        st.next += 1;
        while st.serving != ticket || (st.held > 0 && st.held.saturating_add(n) > self.limit) {
            st = self
                .changed
                .wait(st)
                .unwrap_or_else(PoisonError::into_inner);
        }
        st.held += n;
        st.serving += 1;
        drop(st);
        // The next in line may fit too.
        self.changed.notify_all();
        BudgetGuard { budget: self, n }
    }
}

impl Drop for BudgetGuard<'_> {
    fn drop(&mut self) {
        self.budget.lock().held -= self.n;
        self.budget.changed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    #[test]
    fn budget_never_exceeds_its_limit() {
        const LIMIT: u64 = 1 << 20;
        let budget = Budget::new(LIMIT);
        // Bytes held by the threads right now, counted after acquiring and before releasing (never more than the budget's).
        let held = AtomicU64::new(0);
        let peak = AtomicU64::new(0);
        std::thread::scope(|s| {
            for t in 0..16u64 {
                let (budget, held, peak) = (&budget, &held, &peak);
                s.spawn(move || {
                    let mut rng = t.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
                    for _ in 0..200 {
                        rng ^= rng << 13;
                        rng ^= rng >> 7;
                        rng ^= rng << 17;
                        let n = rng % (LIMIT / 3 + 1);
                        let _guard = budget.acquire(n);
                        let now = held.fetch_add(n, Ordering::SeqCst) + n;
                        peak.fetch_max(now, Ordering::SeqCst);
                        assert!(now <= LIMIT, "{now} bytes held");
                        std::thread::sleep(Duration::from_micros(50));
                        held.fetch_sub(n, Ordering::SeqCst);
                    }
                });
            }
            let (budget, held) = (&budget, &held);
            s.spawn(move || {
                std::thread::sleep(Duration::from_millis(5));
                let _guard = budget.acquire(LIMIT + 1);
                assert_eq!(held.fetch_add(LIMIT + 1, Ordering::SeqCst), 0, "runs alone");
                std::thread::sleep(Duration::from_millis(2));
                held.fetch_sub(LIMIT + 1, Ordering::SeqCst);
            });
        });
        // The threads did overlap (otherwise the bound proves nothing).
        assert!(peak.load(Ordering::SeqCst) > LIMIT / 3);
    }
}
