//! Speed and time-left estimates for long operations (the rescue copy): an exponential moving average of the bytes per
//! second, with a half-life of 10 s, so a slow stretch of the card shows up within seconds without the number jumping
//! on every read.

use std::time::{Duration, Instant};

/// Half-life of the moving average: a rate seen 10 s ago weighs half as much as the current one.
const HALF_LIFE_SECS: f64 = 10.0;
/// Samples closer together than this are folded into the next one (an instant rate over microseconds is noise).
const MIN_INTERVAL: Duration = Duration::from_millis(100);

/// The bytes per second of a running operation, from `(bytes done, when)` samples.
#[derive(Clone, Debug, Default)]
pub struct RateMeter {
    /// The last sample the average was updated at.
    last: Option<(u64, Instant)>,
    /// Bytes per second; `None` until two samples at least [`MIN_INTERVAL`] apart.
    rate: Option<f64>,
}

impl RateMeter {
    pub fn new() -> RateMeter {
        RateMeter::default()
    }

    /// `done` bytes (a running total, never decreasing) at `at`. The first interval sets the rate; each later one moves
    /// it toward the interval's rate by `1 − 2^(−seconds / 10)`.
    pub fn sample(&mut self, done: u64, at: Instant) {
        let Some((last_done, last_at)) = self.last else {
            self.last = Some((done, at));
            return;
        };
        let dt = at.saturating_duration_since(last_at);
        if dt < MIN_INTERVAL {
            return;
        }
        let secs = dt.as_secs_f64();
        let current = done.saturating_sub(last_done) as f64 / secs;
        self.rate = Some(match self.rate {
            None => current,
            Some(rate) => {
                let weight = 1.0 - (-secs / HALF_LIFE_SECS).exp2();
                rate + weight * (current - rate)
            }
        });
        self.last = Some((done, at));
    }

    /// Bytes per second (0 until known).
    pub fn bytes_per_sec(&self) -> u64 {
        self.rate.map_or(0, |r| r.round() as u64)
    }

    /// Seconds to do `remaining` more bytes at the current rate, rounded up; `None` while the rate is unknown or 0.
    pub fn eta_secs(&self, remaining: u64) -> Option<u64> {
        let rate = self.rate.filter(|&r| r > 0.0)?;
        Some((remaining as f64 / rate).ceil() as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_meter_smooths_and_estimates() {
        let mut meter = RateMeter::new();
        assert_eq!(meter.eta_secs(100_000_000), None);
        assert_eq!(meter.bytes_per_sec(), 0);
        let t0 = Instant::now();
        // 10 MB/s for 30 s, sampled every 250 ms.
        for i in 0..=120u64 {
            meter.sample(i * 2_500_000, t0 + Duration::from_millis(i * 250));
        }
        let rate = meter.bytes_per_sec();
        assert!(rate.abs_diff(10_000_000) <= 500_000, "{rate}");
        let eta = meter.eta_secs(100_000_000).unwrap();
        assert!((9..=11).contains(&eta), "{eta}");

        // A slower stretch moves it, smoothly: after 10 s at 5 MB/s it is halfway (half-life).
        let t1 = t0 + Duration::from_secs(30);
        let base = 120 * 2_500_000;
        for i in 1..=40u64 {
            meter.sample(base + i * 1_250_000, t1 + Duration::from_millis(i * 250));
        }
        let rate = meter.bytes_per_sec();
        assert!(rate.abs_diff(7_500_000) <= 375_000, "{rate}");

        // Samples closer than 100 ms are folded into the next one.
        let mut m = RateMeter::new();
        m.sample(0, t0);
        m.sample(1_000_000, t0 + Duration::from_millis(1));
        assert_eq!(m.bytes_per_sec(), 0);
        m.sample(2_000_000, t0 + Duration::from_millis(200));
        assert_eq!(m.bytes_per_sec(), 10_000_000);
        assert_eq!(m.eta_secs(0), Some(0));
    }
}
