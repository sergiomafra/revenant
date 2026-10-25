//! A scan resumed from any saved state emits exactly the events the uninterrupted scan emitted after it (ruling D22),
//! for any `threads` and `cache_bytes`, never reading a known bad sector again (P5), the listing again (D23) or what
//! lies below the state's position except what D24 lists.

use super::*;
use crate::codec::{from_bytes, to_bytes};
use crate::formats::all_formats;
use crate::scan::{CancelToken, Event, ScanOptions, ScanSummary, scan_with};
use crate::source::{Source, VolumeId};
use crate::test_support::SimSource;
use crate::test_support::cards::{self, Card};

const MIB: u64 = 1 << 20;

fn opts(threads: usize, cache_bytes: usize) -> ScanOptions {
    ScanOptions {
        block: MIB as usize,
        threads,
        cache_bytes,
        ..ScanOptions::default()
    }
}

/// A saved point: the events emitted before it, the boundary (`None` after the pairing) and the state, encoded.
#[derive(Clone)]
struct Point {
    events: u64,
    read: Option<u64>,
    state: Vec<u8>,
}

/// An uninterrupted scan with every checkpoint recorded.
struct Recorded {
    events: Vec<Event>,
    summary: ScanSummary,
    /// The listing the hook saw, encoded.
    listing: Vec<u8>,
    points: Vec<Point>,
}

fn record(source: &dyn Source, opts: &ScanOptions) -> Recorded {
    let mut events = Vec::new();
    let mut listing = None;
    let mut points = Vec::new();
    let mut hook = |c: Checkpoint| {
        match c {
            Checkpoint::Listing(l) => {
                assert!(listing.is_none(), "one listing");
                listing = Some(to_bytes(l));
            }
            Checkpoint::Boundary {
                read,
                events,
                state,
            } => {
                let s = state();
                assert_eq!((s.position(), s.events()), (read, events));
                points.push(Point {
                    events,
                    read: Some(read),
                    state: to_bytes(&s),
                });
            }
            Checkpoint::Paired { events, state } => points.push(Point {
                events,
                read: None,
                state: to_bytes(&state()),
            }),
        }
        Ok(())
    };
    let summary = scan_with(
        source,
        &all_formats(),
        opts,
        &CancelToken::default(),
        &mut |e| events.push(e),
        None,
        Some(&mut hook),
    )
    .unwrap();
    Recorded {
        events,
        summary,
        listing: listing.expect("the listing checkpoint"),
        points,
    }
}

/// The events of a scan resumed from `point` (state and listing decoded from their saved bytes).
fn resume(
    source: &dyn Source,
    opts: &ScanOptions,
    listing: &[u8],
    point: &Point,
) -> (Result<ScanSummary, EngineError>, Vec<Event>) {
    let (r, events, _) = resume_points(source, opts, listing, point);
    (r, events)
}

/// [`resume`], with the checkpoints of the resumed scan.
fn resume_points(
    source: &dyn Source,
    opts: &ScanOptions,
    listing: &[u8],
    point: &Point,
) -> (Result<ScanSummary, EngineError>, Vec<Event>, Vec<Point>) {
    let mut events = Vec::new();
    let mut points = Vec::new();
    let resume = Resume {
        listing: from_bytes(listing).unwrap(),
        state: from_bytes(&point.state).unwrap(),
    };
    let mut hook = |c: Checkpoint| {
        match c {
            Checkpoint::Listing(_) => panic!("a resumed scan reads no listing"),
            Checkpoint::Boundary {
                read,
                events,
                state,
            } => points.push(Point {
                events,
                read: Some(read),
                state: to_bytes(&state()),
            }),
            Checkpoint::Paired { events, state } => points.push(Point {
                events,
                read: None,
                state: to_bytes(&state()),
            }),
        }
        Ok(())
    };
    let r = scan_with(
        source,
        &all_formats(),
        opts,
        &CancelToken::default(),
        &mut |e| events.push(e),
        Some(resume),
        Some(&mut hook),
    );
    (r, events, points)
}

/// What of a saved state does not depend on timing: everything but the coverage, the finders' tails (a block read
/// ahead may be deferred or read, which resets them or not; the tails have their own test) and the bad runs past the
/// position (found by read-ahead or measurements whenever they ran).
fn deterministic(bytes: &[u8]) -> ScanState {
    let mut s: ScanState = from_bytes(bytes).unwrap();
    s.coverage.clear();
    s.tails = Default::default();
    s.bad = below(&s.bad, s.position);
    s
}

/// Resumes `r`'s scan at `p` with `opts`: the events after it, the summary, and the state at every later checkpoint
/// (its deterministic part) are those of the uninterrupted scan, so a state field not put back is seen at the next
/// boundary even when it changes no event.
fn check_resume(name: &str, source: &dyn Source, r: &Recorded, opts: &ScanOptions, p: &Point) {
    let (got, events, points) = resume_points(source, opts, &r.listing, p);
    let at = format!(
        "{name}: threads {} cache {}, resumed at {:?} after {} events",
        opts.threads, opts.cache_bytes, p.read, p.events
    );
    let mut whole = r.events[..p.events as usize].to_vec();
    whole.extend(events);
    assert_eq!(whole, r.events, "{at}");
    assert_eq!(got.unwrap(), r.summary, "{at}");
    let i = r
        .points
        .iter()
        .position(|q| (q.events, q.read) == (p.events, p.read))
        .expect("a recorded point");
    // A resume from the pairing point runs the pairing again, so it reaches that point again.
    let later = &r.points[if p.read.is_none() { i } else { i + 1 }..];
    assert_eq!(points.len(), later.len(), "{at}");
    for (mine, theirs) in points.iter().zip(later) {
        assert_eq!(
            (mine.events, mine.read),
            (theirs.events, theirs.read),
            "{at}"
        );
        assert_eq!(
            deterministic(&mine.state),
            deterministic(&theirs.state),
            "{at}: state at {:?}",
            theirs.read
        );
    }
}

/// splitmix64.
fn next(seed: &mut u64) -> u64 {
    *seed = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *seed;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// The first and the last boundary and `extra` seeded ones, plus the point after the pairing.
fn chosen(points: &[Point], extra: usize, seed: u64) -> Vec<Point> {
    let boundaries: Vec<&Point> = points.iter().filter(|p| p.read.is_some()).collect();
    let mut idx = vec![0, boundaries.len() - 1];
    let mut s = seed;
    for _ in 0..extra {
        idx.push((next(&mut s) % boundaries.len() as u64) as usize);
    }
    idx.sort_unstable();
    idx.dedup();
    let mut out: Vec<Point> = idx.into_iter().map(|i| boundaries[i].clone()).collect();
    out.extend(points.iter().filter(|p| p.read.is_none()).cloned());
    out
}

fn all_cards() -> Vec<(&'static str, Card)> {
    vec![
        ("A", cards::a()),
        ("B", cards::b()),
        ("C", cards::c()),
        ("D", cards::d()),
        ("E", cards::e()),
        ("failing", cards::failing()),
    ]
}

#[test]
fn the_cards_exercise_what_a_state_carries() {
    let found = |r: &Recorded, f: fn(&Event) -> bool| r.events.iter().filter(|e| f(e)).count();
    let a = record(&cards::a().source, &opts(8, 256 << 20));
    assert_eq!(found(&a, |e| matches!(e, Event::Updated(_))), 1, "A pairs");
    assert!(found(&a, |e| matches!(e, Event::Unreadable { .. })) >= 3);
    assert!(found(&a, |e| matches!(e, Event::Found(_))) >= 8);
    let b = record(&cards::b().source, &opts(8, 256 << 20));
    assert!(found(&b, |e| matches!(e, Event::Volume { .. })) == 1);
    let c = record(&cards::c().source, &opts(8, 256 << 20));
    assert!(
        found(&c, |e| matches!(e, Event::Updated(_))) >= 2,
        "C names and pairs"
    );
    let d = record(&cards::d().source, &opts(8, 256 << 20));
    let d_found: Vec<_> = d
        .events
        .iter()
        .filter_map(|e| match e {
            Event::Found(f) => Some((f.start, f.kind, f.health)),
            _ => None,
        })
        .collect();
    assert_eq!(d_found.len(), 2, "{d_found:?}");
    assert_eq!(d_found[0].0, MIB - 512);
    let e = cards::e();
    let er = record(&e.source, &opts(8, 256 << 20));
    let video = er
        .events
        .iter()
        .find_map(|ev| match ev {
            Event::Found(f) if Some(f.start) == e.file.map(|f| f.0) => Some(f.clone()),
            _ => None,
        })
        .expect("the video");
    assert_eq!(video.health, crate::formats::Health::Intact);
}

#[test]
fn state_round_trips() {
    for (name, card) in all_cards() {
        let r = record(&card.source, &opts(8, 256 << 20));
        assert!(r.points.len() > 3, "{name}");
        for p in &r.points {
            let state: ScanState = from_bytes(&p.state).unwrap();
            assert_eq!(to_bytes(&state), p.state, "{name} at {:?}", p.read);
            for cut in [0, 1, p.state.len() / 2, p.state.len() - 1] {
                assert!(
                    matches!(
                        from_bytes::<ScanState>(&p.state[..cut]),
                        Err(EngineError::SessionCorrupt)
                    ),
                    "{name} cut at {cut}"
                );
            }
        }
        let listing: Listing = from_bytes(&r.listing).unwrap();
        assert_eq!(to_bytes(&listing), r.listing);
    }
}

#[test]
fn resume_at_any_block_equals_the_whole_scan() {
    let configs = [
        (1, 256 << 20),
        (2, 256 << 20),
        (8, 256 << 20),
        (16, 256 << 20),
        (1, 1 << 20),
        (2, 1 << 20),
        (8, 1 << 20),
        (16, 1 << 20),
    ];
    for (n, (name, card)) in all_cards().into_iter().enumerate() {
        let base = record(&card.source, &opts(1, 256 << 20)).events;
        for (k, &(threads, cache)) in configs.iter().enumerate() {
            let o = opts(threads, cache);
            let r = record(&card.source, &o);
            assert_eq!(r.events, base, "{name}: threads {threads} cache {cache}");
            for p in chosen(&r.points, 6, (n * 16 + k) as u64) {
                check_resume(name, &card.source, &r, &o, &p);
            }
        }
    }
}

#[test]
fn resume_with_other_threads_and_cache() {
    // Saved by a scan with 8 threads and 256 MiB, resumed by one with 1 or 16 threads and a 1 MiB cache.
    for (n, (name, card)) in all_cards().into_iter().enumerate() {
        let r = record(&card.source, &opts(8, 256 << 20));
        for (k, threads) in [1, 16].into_iter().enumerate() {
            for p in chosen(&r.points, 3, (n * 4 + k) as u64 + 1000) {
                check_resume(name, &card.source, &r, &opts(threads, 1 << 20), &p);
            }
        }
    }
}

/// Release builds only (about 10 minutes in a debug build): every boundary of every card, one configuration.
#[cfg(not(debug_assertions))]
#[test]
fn resume_at_every_boundary() {
    for (name, card) in all_cards() {
        let o = opts(8, 1 << 20);
        let r = record(&card.source, &o);
        for p in &r.points {
            check_resume(name, &card.source, &r, &o, p);
        }
    }
}

#[test]
fn resume_between_the_halves_of_a_moov_header() {
    // A's separated `moov` header straddles a block boundary: resumed there, the read stage must still see its first
    // 8 bytes (the finders' tails come back with the state), or the video is never paired.
    let card = cards::a();
    let (moov, _) = card.file.unwrap();
    let boundary = moov.next_multiple_of(MIB);
    assert!(boundary - moov < 16);
    for (threads, cache) in [(1, 256 << 20), (8, 1 << 20)] {
        let o = opts(threads, cache);
        let r = record(&card.source, &o);
        assert!(r.events.iter().any(|e| matches!(e, Event::Updated(_))));
        let p = r
            .points
            .iter()
            .find(|p| p.read == Some(boundary))
            .expect("the boundary inside the header");
        let (got, events) = resume(&card.source, &o, &r.listing, p);
        assert_eq!(got.unwrap(), r.summary);
        assert_eq!(events, r.events[p.events as usize..]);
    }
}

/// Whether `[s, e)` overlaps one of `runs`.
fn overlaps(runs: &[(u64, u64)], s: u64, e: u64) -> bool {
    runs.iter().any(|&(rs, re)| s < re && rs < e)
}

/// `runs` clipped to `[0, end)`.
fn below(runs: &[(u64, u64)], end: u64) -> Vec<(u64, u64)> {
    runs.iter()
        .filter(|&&(s, _)| s < end)
        .map(|&(s, e)| (s, e.min(end)))
        .collect()
}

#[test]
fn resume_never_rereads_known_bad_sectors() {
    // A with a bad sector inside its video's first part: the pairing, run again by every resume, reads around it far
    // below the position (P5's below-position half).
    for (name, card) in [
        ("A", cards::a()),
        ("A, bad in the video", cards::a_bad_in_video()),
        ("B", cards::b()),
        ("C", cards::c()),
    ] {
        let o = opts(8, 1 << 20);
        let r = record(&card.source, &o);
        let boundaries: Vec<Point> = r
            .points
            .iter()
            .filter(|p| p.read.is_some())
            .cloned()
            .collect();
        // Past each unreadable run, and the last boundary (the pairing reads below it).
        let mut picks: Vec<Point> = card
            .bad
            .iter()
            .filter_map(|&(_, e)| boundaries.iter().find(|p| p.read.unwrap() >= e).cloned())
            .collect();
        picks.truncate(3);
        picks.push(boundaries.last().unwrap().clone());
        assert!(!picks.is_empty());
        for p in picks {
            let pos = p.read.unwrap();
            let state: ScanState = from_bytes(&p.state).unwrap();
            // Everything below the position was read: the stored map there is the card's.
            assert_eq!(
                below(state.bad(), pos),
                below(&card.bad, pos),
                "{name} at {pos}"
            );
            card.source.clear_reads();
            let (got, events) = resume(&card.source, &o, &r.listing, &p);
            got.unwrap();
            let mut whole = r.events[..p.events as usize].to_vec();
            whole.extend(events);
            assert_eq!(whole, r.events, "{name} at {pos}");
            for (at, len) in card.source.reads() {
                assert!(
                    !overlaps(state.bad(), at, at + len as u64),
                    "{name} resumed at {pos} read {at}+{len}, a known bad sector"
                );
            }
        }
    }
}

#[test]
fn resume_across_disk_failing_keeps_the_event() {
    let card = cards::failing();
    let o = ScanOptions {
        rates: true,
        ..opts(8, 256 << 20)
    };
    let mut events = Vec::new();
    let mut points = Vec::new();
    let mut listing = Vec::new();
    {
        let mut counted = Vec::new();
        let mut on_event = |e: Event| events.push(e);
        let mut hook = |c: Checkpoint| {
            match c {
                Checkpoint::Listing(l) => listing = to_bytes(l),
                Checkpoint::Boundary {
                    read,
                    events,
                    state,
                } => {
                    assert_eq!(state().events(), events);
                    counted.push(events);
                    points.push(Point {
                        events,
                        read: Some(read),
                        state: to_bytes(&state()),
                    });
                }
                Checkpoint::Paired { .. } => {}
            }
            Ok(())
        };
        scan_with(
            &card.source,
            &all_formats(),
            &o,
            &CancelToken::default(),
            &mut on_event,
            None,
            Some(&mut hook),
        )
        .unwrap();
    }
    let is_rate = |e: &Event| matches!(e, Event::Rate { .. });
    assert!(events.iter().any(is_rate));
    // Each boundary's count is every event but `Rate` emitted before it (its `Progress` and `DiskFailing` included).
    let contract: Vec<Event> = events.iter().filter(|e| !is_rate(e)).cloned().collect();
    for p in &points {
        let k = p.events as usize;
        assert!(matches!(
            contract[k - 1],
            Event::Progress { read, .. } | Event::DiskFailing { read, .. } if Some(read) == p.read
        ));
    }
    let failing = contract
        .iter()
        .position(|e| matches!(e, Event::DiskFailing { .. }))
        .expect("DiskFailing");
    let at = points
        .iter()
        .position(|p| p.events as usize == failing + 1)
        .expect("the boundary that emitted DiskFailing");
    let o = ScanOptions { rates: false, ..o };
    for p in &points[at - 1..=at + 1] {
        let (got, more) = resume(&card.source, &o, &listing, p);
        got.unwrap();
        let mut whole = contract[..p.events as usize].to_vec();
        whole.extend(more);
        assert_eq!(whole, contract, "resumed at {:?}", p.read);
        let n = whole
            .iter()
            .filter(|e| matches!(e, Event::DiskFailing { .. }))
            .count();
        assert_eq!(n, 1);
    }
}

/// Cancels `cancel` at the first read reaching `at`.
struct CancelAt<'a> {
    inner: &'a SimSource,
    at: u64,
    cancel: CancelToken,
}

impl Source for CancelAt<'_> {
    fn len(&self) -> u64 {
        self.inner.len()
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, EngineError> {
        if offset + buf.len() as u64 > self.at {
            self.cancel.cancel();
        }
        self.inner.read_at(offset, buf)
    }
    fn volume(&self) -> Option<VolumeId> {
        None
    }
}

#[test]
fn listing_is_not_checkpointed_after_cancel_or_gone() {
    // The first FAT (FAT32: 32 reserved sectors) of the volume at LBA 8192, 10 sectors in.
    let fat = 8192 * 512 + 42 * 512;
    let run = |source: &dyn Source, cancel: &CancelToken| {
        let mut events = Vec::new();
        let mut listed = false;
        let mut hook = |c: Checkpoint| {
            listed |= matches!(c, Checkpoint::Listing(_));
            Ok(())
        };
        let r = scan_with(
            source,
            &all_formats(),
            &opts(8, 256 << 20),
            cancel,
            &mut |e| events.push(e),
            None,
            Some(&mut hook),
        );
        (r.map(|_| ()).map_err(|e| e.code()), events, listed)
    };
    let gone = cards::b().source.gone_at(fat);
    let (r, events, listed) = run(&gone, &CancelToken::default());
    assert_eq!(r, Err("device_gone"));
    assert!(!listed && events.is_empty(), "{events:?}");

    let card = cards::b();
    let cancel = CancelToken::default();
    let src = CancelAt {
        inner: &card.source,
        at: fat,
        cancel: cancel.clone(),
    };
    let (r, events, listed) = run(&src, &cancel);
    assert_eq!(r, Err("cancelled"));
    assert!(!listed && events.is_empty(), "{events:?}");

    // The same card whole: the listing is checkpointed before the first event (a `Volume`).
    let mut order = Vec::new();
    let mut hook = |c: Checkpoint| {
        if matches!(c, Checkpoint::Listing(_)) {
            order.push("listing");
        }
        Ok(())
    };
    let mut first = None;
    scan_with(
        &card.source,
        &all_formats(),
        &opts(8, 256 << 20),
        &CancelToken::default(),
        &mut |e| {
            first.get_or_insert(e);
        },
        None,
        Some(&mut hook),
    )
    .unwrap();
    assert_eq!(order, ["listing"]);
    assert!(matches!(first, Some(Event::Volume { .. })));
}

#[test]
fn resume_reads_nothing_below_the_checkpoint() {
    let card = cards::a_with(false);
    for cache in [256 << 20, 1 << 20] {
        let o = ScanOptions {
            filesystems: false,
            ..opts(8, cache)
        };
        let r = record(&card.source, &o);
        for p in chosen(&r.points, 6, 7) {
            let state: ScanState = from_bytes(&p.state).unwrap();
            let pos = state.position();
            card.source.clear_reads();
            let (got, events) = resume(&card.source, &o, &r.listing, &p);
            got.unwrap();
            assert_eq!(events, r.events[p.events as usize..]);
            for (at, len) in card.source.reads() {
                assert!(at >= pos, "resumed at {pos}: read {at}+{len}");
            }
        }
    }
}

#[test]
fn resume_does_not_reread_an_intact_file_across_the_checkpoint() {
    let card = cards::e();
    let (start, end) = card.file.unwrap();
    // The block holding the video's end also holds what follows it: read as in any scan.
    let core_end = end / MIB * MIB;
    for cache in [256 << 20, 1 << 20] {
        let o = opts(8, cache);
        let r = record(&card.source, &o);
        let inside: Vec<&Point> = r
            .points
            .iter()
            .filter(|p| p.read.is_some_and(|x| x > start + MIB && x < end - MIB))
            .collect();
        assert!(inside.len() > 30);
        for p in inside.iter().step_by(7) {
            let pos = p.read.unwrap();
            card.source.clear_reads();
            let (got, events) = resume(&card.source, &o, &r.listing, p);
            got.unwrap();
            assert_eq!(events, r.events[p.events as usize..]);
            for (at, len) in card.source.reads() {
                assert!(
                    at + len as u64 <= pos || at >= core_end,
                    "cache {cache}, resumed at {pos}: read {at}+{len} inside the video"
                );
            }
        }
    }
}

#[test]
fn resumed_listing_is_not_read_again() {
    let card = cards::b();
    let o = opts(8, 256 << 20);
    let r = record(&card.source, &o);
    for p in chosen(&r.points, 6, 11) {
        let pos = p.read.unwrap_or(card.source.len());
        let meta = pos.min(card.data_start);
        card.source.clear_reads();
        let (got, events) = resume(&card.source, &o, &r.listing, &p);
        got.unwrap();
        assert_eq!(events, r.events[p.events as usize..]);
        for (at, len) in card.source.reads() {
            assert!(at >= meta, "resumed at {pos}: read {at}+{len} below {meta}");
        }
    }
}

#[test]
fn a_state_off_the_scan_is_corrupt() {
    let card = cards::d();
    let o = opts(2, 256 << 20);
    let r = record(&card.source, &o);
    let p = &r.points[1];
    let state: ScanState = from_bytes(&p.state).unwrap();
    let bad = |f: fn(&mut ScanState)| {
        let mut s = state.clone();
        f(&mut s);
        let point = Point {
            state: to_bytes(&s),
            ..p.clone()
        };
        resume(&card.source, &o, &r.listing, &point)
            .0
            .unwrap_err()
            .code()
    };
    assert_eq!(bad(|s| s.position += 512), "session_corrupt");
    // Saved by a scan of another shape (ruling P44).
    assert_eq!(bad(|s| s.shape.block *= 2), "session_corrupt");
    assert_eq!(bad(|s| s.shape.sector = 4096), "session_corrupt");
    assert_eq!(bad(|s| s.shape.filesystems = false), "session_corrupt");
    assert_eq!(bad(|s| s.shape.total += 512), "session_corrupt");
    let other = |o: ScanOptions| {
        resume(&card.source, &o, &r.listing, p)
            .0
            .unwrap_err()
            .code()
    };
    assert_eq!(
        other(ScanOptions {
            block: 2 << 20,
            ..opts(2, 256 << 20)
        }),
        "session_corrupt"
    );
    assert_eq!(
        other(ScanOptions {
            sector: 4096,
            ..opts(2, 256 << 20)
        }),
        "session_corrupt"
    );
    assert_eq!(
        other(ScanOptions {
            filesystems: false,
            ..opts(2, 256 << 20)
        }),
        "session_corrupt"
    );
    let shorter = SimSource::new(cards::noise(card.source.len() - 512, 1));
    assert_eq!(
        resume(&shorter, &o, &r.listing, p).0.unwrap_err().code(),
        "session_corrupt"
    );
    assert_eq!(bad(|s| s.position = 64 * MIB), "session_corrupt");
    assert_eq!(bad(|s| s.skips = vec![(10, 5)]), "session_corrupt");
    assert_eq!(
        bad(|s| s.bad = vec![(0, 512), (512, 1024)]),
        "session_corrupt"
    );
    assert_eq!(bad(|s| s.tails.moov.0 = vec![0; 16]), "session_corrupt");
    assert_eq!(
        bad(|s| s.tails.orphan = Some((Vec::new(), 0))),
        "session_corrupt"
    );
    // `own_len` must count `own`'s entries.
    let mut s = state.clone();
    s.own_len += 1;
    assert!(matches!(
        from_bytes::<ScanState>(&to_bytes(&s)),
        Err(EngineError::SessionCorrupt)
    ));
}

#[test]
fn the_caps_flags_come_back() {
    // No card reaches a cap; a state saying one was reached keeps saying it after a resume (it lowers a pairing's
    // health at the end).
    let card = cards::d();
    let o = opts(2, 256 << 20);
    let r = record(&card.source, &o);
    let mut state: ScanState = from_bytes(&r.points[0].state).unwrap();
    assert!(!state.capped && !state.orphans_capped);
    state.capped = true;
    state.orphans_capped = true;
    let p = Point {
        state: to_bytes(&state),
        ..r.points[0].clone()
    };
    let (got, _, points) = resume_points(&card.source, &o, &r.listing, &p);
    got.unwrap();
    let next: ScanState = from_bytes(&points[0].state).unwrap();
    assert!(next.capped && next.orphans_capped);
}
