//! Failure-simulating sources shared by the integration tests (`mod support;`).
//!
//! [`SimSource`] serves bytes from memory (or virtual zeros) and can make chosen ranges unreadable, flaky or slow, make
//! the whole device vanish or shrink after some reads, and logs every read it receives.
#![allow(dead_code)]

use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use revenant_core::EngineError;
use revenant_core::source::{Source, VolumeId};

enum Data {
    Bytes(Vec<u8>),
    /// `len` bytes of zeros generated on read (nothing allocated).
    Zeros(u64),
}

/// A range whose first `left` reads fail.
struct Flaky {
    start: u64,
    end: u64,
    left: u32,
}

#[derive(Default)]
struct State {
    /// Every `read_at` call, in order, as `(offset, buf.len())`.
    reads: Vec<(u64, usize)>,
    /// `read_at` calls so far (not reset by `clear_reads`).
    calls: u64,
    flaky: Vec<Flaky>,
    /// A read reached `gone_at` (or `gone_after` passed): every read fails until `revive`.
    gone: bool,
    /// `revive` was called: the vanishing triggers are off.
    revived: bool,
}

/// An in-memory [`Source`] that simulates a failing device. `Send + Sync`; the read log and the flaky counters live
/// behind a mutex.
pub struct SimSource {
    data: Data,
    /// Ranges `[start, end)` a read touching fails: `Read { offset: first bad byte in the read }`.
    bad: Vec<(u64, u64)>,
    /// Ranges `[start, end)` and the delay of each read call touching them.
    slow: Vec<(u64, u64, Duration)>,
    gone_at: Option<u64>,
    gone_after: Option<u64>,
    /// After this many reads, reads return short past the new length (`len()` unchanged).
    shrink: Option<(u64, u64)>,
    state: Mutex<State>,
}

impl SimSource {
    pub fn new(data: Vec<u8>) -> SimSource {
        Self::with(Data::Bytes(data))
    }

    /// `len` bytes of zeros, generated on read (no `Vec` of that size).
    pub fn zeros(len: u64) -> SimSource {
        Self::with(Data::Zeros(len))
    }

    fn with(data: Data) -> SimSource {
        SimSource {
            data,
            bad: Vec::new(),
            slow: Vec::new(),
            gone_at: None,
            gone_after: None,
            shrink: None,
            state: Mutex::new(State::default()),
        }
    }

    /// A read touching `[start, start + len)` fails with `Read { offset: its first bad byte }`.
    pub fn bad(mut self, start: u64, len: u64) -> Self {
        self.bad.push((start, start + len));
        self
    }

    /// The first `failures` reads touching `[start, start + len)` fail like [`SimSource::bad`], then they succeed.
    pub fn flaky(self, start: u64, len: u64, failures: u32) -> Self {
        self.lock().flaky.push(Flaky {
            start,
            end: start + len,
            left: failures,
        });
        self
    }

    /// Each read call touching `[start, start + len)` sleeps `per_read` first.
    pub fn slow(mut self, start: u64, len: u64, per_read: Duration) -> Self {
        self.slow.push((start, start + len, per_read));
        self
    }

    /// The first read reaching `offset` and every later read fail with `DeviceGone { offset: the read's offset }`.
    pub fn gone_at(mut self, offset: u64) -> Self {
        self.gone_at = Some(offset);
        self
    }

    /// Read call number `reads + 1` and every later one fail with `DeviceGone`.
    pub fn gone_after(mut self, reads: u64) -> Self {
        self.gone_after = Some(reads);
        self
    }

    /// After `reads` read calls, reads return short past `new_len` (0 bytes at or past it); `len()` is unchanged.
    pub fn shrink_after(mut self, reads: u64, new_len: u64) -> Self {
        self.shrink = Some((reads, new_len));
        self
    }

    /// The device is back: reads succeed again and the vanishing triggers (`gone_at`, `gone_after`, `shrink_after`) are
    /// off from now on.
    pub fn revive(&self) {
        let mut st = self.lock();
        st.gone = false;
        st.revived = true;
    }

    /// Every `read_at` call so far (since the last `clear_reads`), in order, as `(offset, buf.len())`.
    pub fn reads(&self) -> Vec<(u64, usize)> {
        self.lock().reads.clone()
    }

    pub fn clear_reads(&self) {
        self.lock().reads.clear();
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn touches(offset: u64, end: u64, (s, e): (u64, u64)) -> bool {
    offset < e && end > s
}

impl Source for SimSource {
    fn len(&self) -> u64 {
        match &self.data {
            Data::Bytes(b) => b.len() as u64,
            Data::Zeros(len) => *len,
        }
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, EngineError> {
        let end = offset.saturating_add(buf.len() as u64);
        let mut len = self.len();
        let delay: Duration = self
            .slow
            .iter()
            .filter(|&&(s, e, _)| touches(offset, end, (s, e)))
            .map(|&(_, _, d)| d)
            .sum();
        {
            let mut st = self.lock();
            st.reads.push((offset, buf.len()));
            st.calls += 1;
            if !st.revived {
                if self.gone_after.is_some_and(|n| st.calls > n)
                    || self.gone_at.is_some_and(|at| end > at)
                {
                    st.gone = true;
                }
                if let Some((after, new_len)) = self.shrink
                    && st.calls > after
                {
                    len = len.min(new_len);
                }
            }
            if st.gone {
                return Err(EngineError::DeviceGone { offset });
            }
        }
        if !delay.is_zero() {
            std::thread::sleep(delay);
        }
        let mut failed = self
            .bad
            .iter()
            .filter(|&&r| touches(offset, end, r))
            .map(|&(s, _)| s.max(offset))
            .min();
        {
            let mut st = self.lock();
            for f in st.flaky.iter_mut() {
                if f.left > 0 && touches(offset, end, (f.start, f.end)) {
                    f.left -= 1;
                    let at = f.start.max(offset);
                    failed = Some(failed.map_or(at, |b| b.min(at)));
                }
            }
        }
        if let Some(at) = failed {
            return Err(EngineError::Read {
                offset: at,
                cause: std::io::Error::other("simulated bad sector"),
            });
        }
        if offset >= len {
            return Ok(0);
        }
        let n = (len - offset).min(buf.len() as u64) as usize;
        match &self.data {
            Data::Bytes(b) => buf[..n].copy_from_slice(&b[offset as usize..offset as usize + n]),
            Data::Zeros(_) => buf[..n].fill(0),
        }
        Ok(n)
    }

    fn volume(&self) -> Option<VolumeId> {
        None
    }
}
