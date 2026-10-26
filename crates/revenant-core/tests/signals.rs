//! Early signals of a scan: `DiskFailing` once the unreadable runs or bytes emitted reach a threshold (deterministic),
//! and the opt-in, timing-based `Rate` right after each `Progress` (outside the determinism contract).

mod support;

use std::time::Duration;

use revenant_core::formats::all_formats;
use revenant_core::scan::{FAILING_BYTES, FAILING_RUNS};
use revenant_core::source::Source;
use revenant_core::{CancelToken, Event, ScanOptions, scan};
use support::SimSource;

const KIB: u64 = 1 << 10;
const MIB: u64 = 1 << 20;

/// Deterministic noise (SplitMix64), built in memory.
fn noise(len: u64, seed: u64) -> Vec<u8> {
    let mut state = seed;
    let mut out = Vec::with_capacity(len as usize);
    while (out.len() as u64) < len {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        out.extend_from_slice(&z.to_le_bytes());
    }
    out.truncate(len as usize);
    out
}

fn opts(threads: usize, cache_bytes: usize, rates: bool) -> ScanOptions {
    ScanOptions {
        block: MIB as usize,
        threads,
        cache_bytes,
        rates,
        ..ScanOptions::default()
    }
}

fn events(source: &dyn Source, opts: &ScanOptions) -> Vec<Event> {
    let mut events = Vec::new();
    scan(
        source,
        &all_formats(),
        opts,
        &CancelToken::default(),
        &mut |e| events.push(e),
    )
    .unwrap();
    events
}

fn failing(events: &[Event]) -> Vec<(usize, &Event)> {
    events
        .iter()
        .enumerate()
        .filter(|(_, e)| matches!(e, Event::DiskFailing { .. }))
        .collect()
}

#[test]
fn failing_disk_is_flagged_early() {
    let len = 256 * MIB;
    let mut source = SimSource::new(noise(len, 81));
    let mut at = MIB;
    while at < len {
        source = source.bad(at, 512);
        at += 256 * KIB;
    }
    let base = events(&source, &opts(1, 256 * MIB as usize, false));
    let flagged = failing(&base);
    assert_eq!(flagged.len(), 1, "{flagged:?}");
    let (i, e) = flagged[0];
    // The 16th bad sector ends at 1 MiB + 15 × 256 KiB + 512: the first 1 MiB boundary at or past it is 5 MiB.
    let first_boundary = (MIB + 15 * 256 * KIB + 512).div_ceil(MIB) * MIB;
    assert_eq!(
        *e,
        Event::DiskFailing {
            unreadable_runs: FAILING_RUNS,
            unreadable_bytes: FAILING_RUNS * 512,
            read: first_boundary,
        }
    );
    assert_eq!(
        base[i - 1],
        Event::Progress {
            read: first_boundary,
            total: len,
        }
    );
    // Exactly 16 Unreadable events before it.
    let runs = base[..i]
        .iter()
        .filter(|e| matches!(e, Event::Unreadable { .. }))
        .count() as u64;
    assert_eq!(runs, FAILING_RUNS);
    for o in [
        opts(8, 256 * MIB as usize, false),
        opts(1, MIB as usize, false),
    ] {
        assert_eq!(
            events(&source, &o),
            base,
            "threads {}, cache {}",
            o.threads,
            o.cache_bytes
        );
    }
}

#[test]
fn few_bad_sectors_do_not_flag() {
    let len = 16 * MIB;
    let mut source = SimSource::new(noise(len, 82));
    for k in 0..FAILING_RUNS - 1 {
        source = source.bad(MIB + k * 512 * KIB, 512);
    }
    let ev = events(&source, &opts(1, 256 * MIB as usize, false));
    let runs = ev
        .iter()
        .filter(|e| matches!(e, Event::Unreadable { .. }))
        .count() as u64;
    assert_eq!(runs, FAILING_RUNS - 1);
    assert!(failing(&ev).is_empty(), "{:?}", failing(&ev));
}

#[test]
fn big_bad_region_flags_by_bytes() {
    let len = 16 * MIB;
    let source = SimSource::new(noise(len, 83)).bad(4 * MIB, FAILING_BYTES);
    let ev = events(&source, &opts(1, 256 * MIB as usize, false));
    let flagged = failing(&ev);
    assert_eq!(flagged.len(), 1, "{flagged:?}");
    let (i, e) = flagged[0];
    // The run is flushed at the first readable sector, 8 MiB, inside the block ending at 9 MiB.
    assert_eq!(
        *e,
        Event::DiskFailing {
            unreadable_runs: 1,
            unreadable_bytes: FAILING_BYTES,
            read: 9 * MIB,
        }
    );
    assert_eq!(
        ev[i - 1],
        Event::Progress {
            read: 9 * MIB,
            total: len,
        }
    );
    // One byte less does not flag.
    let source = SimSource::new(noise(len, 83)).bad(4 * MIB, FAILING_BYTES - 512);
    let ev = events(&source, &opts(1, 256 * MIB as usize, false));
    assert!(failing(&ev).is_empty());
}

fn without_rates(events: &[Event]) -> Vec<Event> {
    events
        .iter()
        .filter(|e| !matches!(e, Event::Rate { .. }))
        .cloned()
        .collect()
}

/// Every `Rate` comes right after a `Progress` (or after the `DiskFailing` that follows one), and every `Progress` has
/// its `Rate`.
fn assert_rate_after_each_progress(events: &[Event]) {
    for (i, e) in events.iter().enumerate() {
        match e {
            Event::Progress { .. } => {
                let next = match events.get(i + 1) {
                    Some(Event::DiskFailing { .. }) => events.get(i + 2),
                    other => other,
                };
                assert!(
                    matches!(next, Some(Event::Rate { .. })),
                    "no Rate after Progress #{i}: {next:?}"
                );
            }
            Event::Rate { .. } => {
                let prev = match &events[i - 1] {
                    Event::DiskFailing { .. } => &events[i - 2],
                    other => other,
                };
                assert!(
                    matches!(prev, Event::Progress { .. }),
                    "Rate #{i} after {prev:?}"
                );
            }
            _ => {}
        }
    }
}

#[test]
fn rates_only_when_asked() {
    let len = 16 * MIB;
    let source = SimSource::new(noise(len, 84)).bad(4 * MIB, FAILING_BYTES);
    let default = events(&source, &opts(1, 256 * MIB as usize, false));
    assert!(!default.iter().any(|e| matches!(e, Event::Rate { .. })));
    for threads in [1, 8] {
        let with = events(&source, &opts(threads, 256 * MIB as usize, true));
        let rates = with
            .iter()
            .filter(|e| matches!(e, Event::Rate { .. }))
            .count();
        assert_eq!(rates, 16);
        assert_rate_after_each_progress(&with);
        assert_eq!(without_rates(&with), default);
        // The fixed order where both fall on one boundary: Progress, DiskFailing, Rate.
        let (i, _) = failing(&with)[0];
        assert!(matches!(with[i - 1], Event::Progress { .. }));
        assert!(matches!(with[i + 1], Event::Rate { .. }));
    }

    // A window load slower than STALL is counted.
    let source = SimSource::new(noise(len, 85)).slow(10 * MIB, 512, Duration::from_millis(2500));
    let with = events(&source, &opts(1, 256 * MIB as usize, true));
    assert_rate_after_each_progress(&with);
    let last = with
        .iter()
        .rev()
        .find_map(|e| match e {
            Event::Rate { stalls, .. } => Some(*stalls),
            _ => None,
        })
        .expect("a Rate");
    assert!(last >= 1, "stalls {last}");
}
