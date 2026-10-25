//! A session whose source vanishes keeps no bad sector it could not have seen and resumes to the uninterrupted events
//! (these look inside the saved checkpoints, so they live in the crate; the other session tests are in
//! `tests/session.rs`).

use super::*;
use crate::formats::all_formats;
use crate::scan::scan;
use crate::test_support::cards::{self, Card};
use crate::test_support::{FakeDevices, SimSource};

const MIB: u64 = 1 << 20;

/// A small cache: the read-ahead (4 blocks) stays well behind the offsets where the source vanishes.
fn opts() -> ScanOptions {
    ScanOptions {
        block: MIB as usize,
        threads: 4,
        cache_bytes: 8 * MIB as usize,
        ..ScanOptions::default()
    }
}

fn sopts() -> SessionOptions {
    SessionOptions {
        checkpoint_bytes: MIB,
        ..SessionOptions::default()
    }
}

fn identity(source: &dyn Source) -> SourceIdentity {
    SourceIdentity::Image {
        size: source.len(),
        digest: 7,
    }
}

fn reference(source: &dyn Source) -> (Vec<Event>, ScanSummary) {
    let mut events = Vec::new();
    let summary = scan(
        source,
        &all_formats(),
        &opts(),
        &CancelToken::default(),
        &mut |e| events.push(e),
    )
    .unwrap();
    (events, summary)
}

/// Runs the session on `source`: its result and every event handed out (the replayed ones first).
fn run(
    source: &SimSource,
    session: &mut Session,
) -> (Result<ScanSummary, EngineError>, Vec<Event>) {
    let mut events = Vec::new();
    let r = scan_session(
        source,
        &identity(source),
        &all_formats(),
        &opts(),
        session,
        &sopts(),
        &CancelToken::default(),
        &mut |e| events.push(e),
    );
    (r, events)
}

/// Every checkpoint file of `dir` that is valid.
fn checkpoints(dir: &Path) -> Vec<Saved> {
    CHECKPOINTS
        .iter()
        .filter_map(|name| std::fs::read(dir.join(name)).ok())
        .filter_map(|b| parse_checkpoint(&b).ok())
        .collect()
}

fn new_session(source: &SimSource, dir: &Path) -> Session {
    let dest = Destination::check(&FakeDevices::new(), source, dir).unwrap();
    Session::create(&dest, identity(source), &opts()).unwrap()
}

#[test]
fn session_resumes_after_device_gone() {
    let card = cards::a();
    let (expected, summary) = reference(&card.source);
    let Card { source, .. } = card;
    // Seeded offsets past what the scan reads before its first block (a read at 33 MiB at the start), off any block or
    // sector boundary.
    let mut gone_ats = Vec::new();
    let mut seed = 0x5EED_u64;
    for _ in 0..3 {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        gone_ats.push(35 * MIB + (seed >> 33) % (12 * MIB) + 100);
    }
    let mut source = Some(source);
    for gone_at in gone_ats {
        let src = source.take().unwrap().gone_at(gone_at);
        let dir = tempfile::tempdir().unwrap();
        let mut session = new_session(&src, dir.path());
        let (r, _) = run(&src, &mut session);
        assert_eq!(r.unwrap_err().code(), "device_gone", "gone at {gone_at}");
        let saved = checkpoints(dir.path());
        assert!(
            !saved.is_empty(),
            "gone at {gone_at}: a checkpoint before it"
        );
        for s in &saved {
            let state = s.state.as_ref().expect("not finished");
            assert!(state.position() <= gone_at);
            for &(start, end) in state.bad() {
                assert!(
                    end <= gone_at,
                    "bad run {start}..{end} past the gone offset {gone_at}"
                );
            }
        }
        src.revive();
        let mut reopened = Session::open(dir.path()).unwrap();
        assert!(matches!(reopened.state(), SessionState::Interrupted { .. }));
        let (r, events) = run(&src, &mut reopened);
        assert_eq!(r.unwrap(), summary, "gone at {gone_at}");
        assert!(events == expected, "gone at {gone_at}: events differ");
        assert_eq!(reopened.state(), &SessionState::Finished(summary.clone()));
        // A source that comes back revived stays so: rebuild one for the next offset.
        source = Some(cards::a().source);
    }
}

#[test]
fn gone_inside_the_listing_saves_no_listing() {
    // Card B with the source gone inside its first FAT (ruling P3): no listing, no checkpoint, the session stays fresh;
    // after the card comes back the resume reads the listing and the events are the uninterrupted ones.
    let card = cards::b();
    let (expected, summary) = reference(&card.source);
    let fat = 8192 * 512 + 40 * 512;
    let src = card.source.gone_at(fat);
    let dir = tempfile::tempdir().unwrap();
    let mut session = new_session(&src, dir.path());
    let (r, events) = run(&src, &mut session);
    assert_eq!(r.unwrap_err().code(), "device_gone");
    assert!(events.is_empty());
    assert!(!dir.path().join(LISTING).exists());
    assert!(checkpoints(dir.path()).is_empty());
    assert_eq!(session.state(), &SessionState::Fresh);
    src.revive();
    let mut reopened = Session::open(dir.path()).unwrap();
    assert_eq!(reopened.state(), &SessionState::Fresh);
    assert_eq!(reopened.replay_len(), 0);
    let (r, events) = run(&src, &mut reopened);
    assert_eq!(r.unwrap(), summary);
    assert!(events == expected, "events differ");
    assert!(dir.path().join(LISTING).exists());
}

#[test]
fn a_cancel_at_a_boundary_saves_a_checkpoint_there() {
    // Checkpoints only every 1 GiB: the one saved is the boundary where the cancel was seen.
    let card = cards::e();
    let dir = tempfile::tempdir().unwrap();
    let mut session = new_session(&card.source, dir.path());
    let cancel = CancelToken::default();
    let mut progress = 0;
    let r = scan_session(
        &card.source,
        &identity(&card.source),
        &all_formats(),
        &opts(),
        &mut session,
        &SessionOptions {
            checkpoint_bytes: 1 << 30,
            checkpoint_every: Duration::from_secs(3600),
        },
        &cancel,
        &mut |e| {
            if matches!(e, Event::Progress { .. }) {
                progress += 1;
                if progress == 5 {
                    cancel.cancel();
                }
            }
        },
    );
    assert_eq!(r.unwrap_err().code(), "cancelled");
    let saved = checkpoints(dir.path());
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].state.as_ref().unwrap().position(), 5 * MIB);
    assert_eq!(
        session.state(),
        &SessionState::Interrupted {
            read: 5 * MIB,
            total: 48 * MIB
        }
    );
}
