//! A saved session (ruling D25): a scan through it emits the scan's events; a finished one gives its results back
//! without the source; one interrupted by a crash, a cancel or a vanished card resumes to exactly the uninterrupted
//! events; damaged checkpoints and a torn log fall back to the last good point; another card, a changed rescue map, a
//! folder that is not empty or on the card itself are refused.

mod support;

use std::path::Path;

use revenant_core::device::{DestInfo, DeviceOps, DiskInfo};
use revenant_core::formats::all_formats;
use revenant_core::preflight::{Issue, PreflightRequest, SAMPLE_TIMEOUT, preflight};
use revenant_core::rescue::{MappedSource, RegionState, RescueMap, map_path};
use revenant_core::session::{Session, SessionOptions, SessionState, check_session, scan_session};
use revenant_core::source::{DeviceSource, DiskId, Source, VolumeId};
use revenant_core::{
    CancelToken, Destination, EngineError, Event, ScanOptions, ScanSummary, SourceIdentity, scan,
};
use revenant_testkit::fs::{FsBuilder, FsType, Partitioning, disk};
use support::cards::{self, Card};
use support::{FakeDevices, SimSource, disk_info};

const MIB: u64 = 1 << 20;

fn opts(threads: usize) -> ScanOptions {
    ScanOptions {
        block: MIB as usize,
        threads,
        ..ScanOptions::default()
    }
}

fn sopts() -> SessionOptions {
    SessionOptions {
        checkpoint_bytes: MIB,
        ..SessionOptions::default()
    }
}

/// An identity for an in-memory card (what the session compares; the bytes are not hashed here).
fn identity(source: &dyn Source) -> SourceIdentity {
    SourceIdentity::Image {
        size: source.len(),
        digest: 0xC0FFEE,
    }
}

/// The uninterrupted scan.
fn reference(source: &dyn Source, opts: &ScanOptions) -> (Vec<Event>, ScanSummary) {
    let mut events = Vec::new();
    let summary = scan(
        source,
        &all_formats(),
        opts,
        &CancelToken::default(),
        &mut |e| events.push(e),
    )
    .unwrap();
    (events, summary)
}

/// A checked destination over `dir` for an in-memory source (no disk: only the volume rule).
fn dest(source: &dyn Source, dir: &Path) -> Destination {
    Destination::check(&FakeDevices::new(), source, dir).unwrap()
}

fn new_session(source: &dyn Source, dir: &Path, opts: &ScanOptions) -> Session {
    Session::create(&dest(source, dir), identity(source), opts).unwrap()
}

/// Runs `session` on `source` to the end: the result and every event handed out (the replayed ones first).
fn run(
    source: &dyn Source,
    session: &mut Session,
    opts: &ScanOptions,
) -> (Result<ScanSummary, EngineError>, Vec<Event>) {
    let mut events = Vec::new();
    let r = scan_session(
        source,
        &identity(source),
        &all_formats(),
        opts,
        session,
        &sopts(),
        &CancelToken::default(),
        &mut |e| events.push(e),
    );
    (r, events)
}

/// Runs `session` and cancels it right after the `k`-th event (1-based), first calling `at_k` with the session folder.
fn run_cancelled_at(
    source: &dyn Source,
    session: &mut Session,
    opts: &ScanOptions,
    k: usize,
    at_k: &mut dyn FnMut(&Path),
) -> Result<ScanSummary, EngineError> {
    let cancel = CancelToken::default();
    let dir = session.dir().to_path_buf();
    let mut seen = 0;
    scan_session(
        source,
        &identity(source),
        &all_formats(),
        opts,
        session,
        &sopts(),
        &cancel,
        &mut |_| {
            seen += 1;
            if seen == k {
                at_k(&dir);
                cancel.cancel();
            }
        },
    )
}

/// Copies the regular files of `from` into the empty folder `to` (what a crash leaves on disk at that moment).
fn copy_dir(from: &Path, to: &Path) {
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_file() {
            std::fs::copy(entry.path(), to.join(entry.file_name())).unwrap();
        }
    }
}

/// A deterministic sequence of numbers (an LCG).
fn seeded(seed: u64, n: usize, below: u64) -> Vec<u64> {
    let mut x = seed;
    (0..n)
        .map(|_| {
            x = x
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (x >> 33) % below
        })
        .collect()
}

fn assert_events(got: &[Event], want: &[Event], what: &str) {
    assert_eq!(got.len(), want.len(), "{what}: event count");
    if let Some(i) = (0..got.len()).find(|&i| got[i] != want[i]) {
        panic!("{what}: event {i} differs: {:?} vs {:?}", got[i], want[i]);
    }
}

#[test]
fn uninterrupted_session_matches_scan() {
    for (name, card) in [("A", cards::a()), ("B", cards::b()), ("C", cards::c())] {
        let o = opts(4);
        let (expected, summary) = reference(&card.source, &o);
        let dir = tempfile::tempdir().unwrap();
        let mut session = new_session(&card.source, dir.path(), &o);
        assert_eq!(session.state(), &SessionState::Fresh);
        let (r, events) = run(&card.source, &mut session, &o);
        assert_eq!(r.unwrap(), summary, "{name}");
        assert_events(&events, &expected, name);
        assert_eq!(session.state(), &SessionState::Finished(summary.clone()));
        assert_eq!(session.replay_len(), expected.len() as u64);
        assert_events(&session.events().unwrap(), &expected, name);
        let reopened = Session::open(dir.path()).unwrap();
        assert_eq!(reopened.state(), &SessionState::Finished(summary));
        assert_eq!(reopened.identity(), &identity(&card.source));
        assert_events(&reopened.events().unwrap(), &expected, name);
    }
}

/// A source of `len` bytes that must never be read.
struct NoReads(u64);

impl Source for NoReads {
    fn len(&self) -> u64 {
        self.0
    }
    fn read_at(&self, offset: u64, _buf: &mut [u8]) -> Result<usize, EngineError> {
        panic!("a finished session read the source at {offset}");
    }
    fn volume(&self) -> Option<VolumeId> {
        None
    }
}

#[test]
fn finished_session_returns_results_without_reading() {
    let card = cards::c();
    let o = opts(4);
    let (expected, summary) = reference(&card.source, &o);
    let dir = tempfile::tempdir().unwrap();
    let mut session = new_session(&card.source, dir.path(), &o);
    run(&card.source, &mut session, &o).0.unwrap();
    let before: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().metadata().unwrap().modified().unwrap())
        .collect();
    let mut reopened = Session::open(dir.path()).unwrap();
    let gone = NoReads(card.source.len());
    let (r, events) = run(&gone, &mut reopened, &o);
    assert_eq!(r.unwrap(), summary);
    assert_events(&events, &expected, "replayed");
    // Twice, and nothing written.
    let (r, events) = run(&gone, &mut reopened, &o);
    assert_eq!(r.unwrap(), summary);
    assert_events(&events, &expected, "replayed again");
    let after: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().metadata().unwrap().modified().unwrap())
        .collect();
    assert_eq!(before, after);
}

#[test]
fn resume_from_a_crash_copy_equals_the_whole_scan() {
    let cards = [("A", cards::a()), ("B", cards::b()), ("C", cards::c())];
    // One thread per card and thread count: the checkpoints' syncs dominate, so they overlap.
    std::thread::scope(|s| {
        for (name, card) in &cards {
            for threads in [1, 8] {
                s.spawn(move || crash_and_resume(name, card, threads));
            }
        }
    });
}

/// For 5 seeded event indexes k: a session cancelled right after event k, its folder copied just before (what a crash
/// leaves), reopened from the copy and run to the end gives the uninterrupted events.
fn crash_and_resume(name: &str, card: &Card, threads: usize) {
    let o = opts(threads);
    let (expected, summary) = reference(&card.source, &o);
    let seed = threads as u64 * 31 + name.as_bytes()[0] as u64;
    for (i, k) in seeded(seed, 5, expected.len() as u64)
        .into_iter()
        .enumerate()
    {
        let k = k as usize + 1;
        let what = format!("card {name}, {threads} threads, crash at event {k} (seed {i})");
        let dir = tempfile::tempdir().unwrap();
        let crash = tempfile::tempdir().unwrap();
        let mut session = new_session(&card.source, dir.path(), &o);
        let r = run_cancelled_at(&card.source, &mut session, &o, k, &mut |d| {
            copy_dir(d, crash.path())
        });
        assert_eq!(r.unwrap_err().code(), "cancelled", "{what}");
        let mut resumed = Session::open(crash.path()).unwrap();
        assert!(resumed.replay_len() <= k as u64, "{what}");
        let (r, events) = run(&card.source, &mut resumed, &o);
        assert_eq!(r.unwrap(), summary, "{what}");
        assert_events(&events, &expected, &what);
        // What the resumed run saved is the whole scan.
        let reopened = Session::open(crash.path()).unwrap();
        assert_eq!(reopened.state(), &SessionState::Finished(summary.clone()));
        assert_events(&reopened.events().unwrap(), &expected, &what);
    }
}

/// An interrupted session of card A (cancelled two thirds in): its folder, the card and the reference events.
struct Interrupted {
    dir: tempfile::TempDir,
    card: Card,
    expected: Vec<Event>,
    summary: ScanSummary,
    replay_len: u64,
}

fn interrupted() -> Interrupted {
    let card = cards::a();
    let o = opts(4);
    let (expected, summary) = reference(&card.source, &o);
    let dir = tempfile::tempdir().unwrap();
    let mut session = new_session(&card.source, dir.path(), &o);
    let k = expected.len() * 2 / 3;
    let r = run_cancelled_at(&card.source, &mut session, &o, k, &mut |_| {});
    assert_eq!(r.unwrap_err().code(), "cancelled");
    assert!(matches!(session.state(), SessionState::Interrupted { .. }));
    let replay_len = session.replay_len();
    assert!(replay_len > 0);
    assert_eq!(Session::open(dir.path()).unwrap().replay_len(), replay_len);
    Interrupted {
        dir,
        card,
        expected,
        summary,
        replay_len,
    }
}

/// A checkpoint file's generation (D25: after the 11-byte header and the 4-byte checksum).
fn generation(path: &Path) -> u64 {
    let b = std::fs::read(path).unwrap();
    u64::from_le_bytes(b[15..23].try_into().unwrap())
}

fn flip_last_byte(path: &Path) {
    let mut b = std::fs::read(path).unwrap();
    *b.last_mut().unwrap() ^= 0x01;
    std::fs::write(path, b).unwrap();
}

#[test]
fn torn_checkpoint_falls_back_to_the_previous_one() {
    let it = interrupted();
    let (a, b) = (
        it.dir.path().join("checkpoint-a.rvs"),
        it.dir.path().join("checkpoint-b.rvs"),
    );
    assert!(a.exists() && b.exists(), "two checkpoints");
    // The newest is found by its generation, whatever its file name: swapped, the same point is opened.
    let swap = it.dir.path().join("swap");
    std::fs::rename(&a, &swap).unwrap();
    std::fs::rename(&b, &a).unwrap();
    std::fs::rename(&swap, &b).unwrap();
    assert_eq!(
        Session::open(it.dir.path()).unwrap().replay_len(),
        it.replay_len
    );
    let newest = if generation(&a) > generation(&b) {
        &a
    } else {
        &b
    };
    flip_last_byte(newest);
    let mut session = Session::open(it.dir.path()).unwrap();
    assert!(session.replay_len() < it.replay_len);
    let (r, events) = run(&it.card.source, &mut session, &opts(4));
    assert_eq!(r.unwrap(), it.summary);
    assert_events(&events, &it.expected, "resumed from the older checkpoint");

    // Both damaged: nothing to resume from.
    let fresh = interrupted();
    for name in ["checkpoint-a.rvs", "checkpoint-b.rvs"] {
        flip_last_byte(&fresh.dir.path().join(name));
    }
    assert_eq!(
        Session::open(fresh.dir.path()).unwrap_err().code(),
        "session_corrupt"
    );
}

#[test]
fn garbage_after_the_last_checkpoint_is_dropped() {
    let it = interrupted();
    let log = it.dir.path().join("events.rvs");
    let mut bytes = std::fs::read(&log).unwrap();
    bytes.extend(seeded(9, 1000, 256).into_iter().map(|b| b as u8));
    std::fs::write(&log, bytes).unwrap();
    let mut session = Session::open(it.dir.path()).unwrap();
    assert_eq!(session.replay_len(), it.replay_len);
    let (r, events) = run(&it.card.source, &mut session, &opts(4));
    assert_eq!(r.unwrap(), it.summary);
    assert_events(&events, &it.expected, "resumed after garbage");
    // The garbage is gone from the log: it holds exactly the scan's events.
    assert_events(&session.events().unwrap(), &it.expected, "saved");
    assert_events(
        &Session::open(it.dir.path()).unwrap().events().unwrap(),
        &it.expected,
        "reopened",
    );
}

const CARD: u64 = 64 * MIB;

/// A 64 MiB card: a FAT32 volume of 48 MiB at sector 2048 (MBR).
fn card_bytes() -> Vec<u8> {
    let volume = FsBuilder::new(FsType::Fat32, 48 * MIB, 512).build();
    disk(
        Partitioning::Mbr { first_lba: 2048 },
        &volume,
        CARD as usize,
    )
}

fn sdxc(serial: Option<&str>) -> DiskInfo {
    let mut info = disk_info("disk4", CARD);
    info.model = Some("SDXC".into());
    info.serial = serial.map(str::to_string);
    info
}

fn dest_info(disks: &[&str]) -> DestInfo {
    DestInfo {
        disks: disks.iter().map(|d| DiskId::parse(d).unwrap()).collect(),
        fs: Some("apfs".into()),
        free_bytes: 1 << 30,
        max_file_bytes: None,
    }
}

fn device(fake: &FakeDevices) -> DeviceSource {
    let id = DiskId::parse("disk4").unwrap();
    DeviceSource::new(fake.open(&id).unwrap(), id)
}

#[test]
fn resume_on_another_card_is_refused() {
    let fake = FakeDevices::new();
    fake.add(sdxc(Some("S1")), card_bytes());
    let dir = tempfile::tempdir().unwrap();
    fake.set_destination(dir.path(), Ok(dest_info(&["disk2"])));
    let source = device(&fake);
    let s1 = SourceIdentity::of_device(&sdxc(Some("S1")), &source).unwrap();
    let dest = Destination::check(&fake, &source, dir.path()).unwrap();
    let o = opts(2);
    let mut session = Session::create(&dest, s1.clone(), &o).unwrap();
    let SourceIdentity::Device { table, .. } = s1.clone() else {
        panic!("a device identity");
    };
    let with = |serial: Option<&str>, table: u64| SourceIdentity::Device {
        size: CARD,
        model: Some("SDXC".into()),
        serial: serial.map(str::to_string),
        table,
    };
    let attempt = |session: &mut Session, id: &SourceIdentity| {
        scan_session(
            &source,
            id,
            &all_formats(),
            &o,
            session,
            &sopts(),
            &CancelToken::default(),
            &mut |_| {},
        )
    };
    let image = SourceIdentity::Image {
        size: CARD,
        digest: table,
    };
    // Another serial (an extra check), another table, the card's image: refused before anything is read or written.
    for other in [
        with(Some("S2"), table),
        with(Some("S1"), table ^ 1),
        with(None, table ^ 1),
        image,
    ] {
        assert_eq!(
            attempt(&mut session, &other).unwrap_err().code(),
            "session_other_source",
            "{other:?}"
        );
        assert_eq!(
            check_session(dir.path(), &other).unwrap_err().code(),
            "session_other_source"
        );
    }
    assert_eq!(session.state(), &SessionState::Fresh);
    assert!(!dir.path().join("listing.rvs").exists());
    // No serial on one side, the same table: the same card (P7).
    check_session(dir.path(), &with(None, table)).unwrap();
    check_session(dir.path(), &s1).unwrap();
    assert!(matches!(
        attempt(&mut session, &with(None, table)).unwrap(),
        ScanSummary { .. }
    ));
    assert!(matches!(session.state(), SessionState::Finished(_)));

    // The pre-flight: the same card with another serial is another card; without a serial it is the same.
    let request = |disk| PreflightRequest {
        disk,
        unmount: false,
        copy_to: None,
        session: Some(dir.path()),
        sample_timeout: SAMPLE_TIMEOUT,
    };
    let disk4 = DiskId::parse("disk4").unwrap();
    assert_eq!(preflight(&fake, &request(&disk4)).issues, []);
    fake.add(sdxc(Some("S2")), card_bytes());
    assert_eq!(
        preflight(&fake, &request(&disk4)).issues,
        [Issue::SessionOtherSource]
    );
    fake.add(sdxc(None), card_bytes());
    assert_eq!(preflight(&fake, &request(&disk4)).issues, []);
}

#[test]
fn session_on_a_mapped_image_notices_map_changes() {
    let dir = tempfile::tempdir().unwrap();
    let image = dir.path().join("card.img");
    let size = 4 * MIB;
    std::fs::write(&image, cards::noise(size, 3)).unwrap();
    let origin = SourceIdentity::Image { size, digest: 1 };
    let mut map = RescueMap::new(size, 512, origin);
    assert!(map.set(0, 64 * 512, RegionState::Rescued));
    assert!(map.set(MIB, 3 * MIB, RegionState::Rescued));
    map.save(&map_path(&image)).unwrap();
    let before = SourceIdentity::of_mapped(&MappedSource::open(&image).unwrap()).unwrap();

    let session_dir = tempfile::tempdir().unwrap();
    let anywhere = SimSource::zeros(size);
    let dest = Destination::check(&FakeDevices::new(), &anywhere, session_dir.path()).unwrap();
    let mut session = Session::create(&dest, before.clone(), &opts(2)).unwrap();
    check_session(session_dir.path(), &before).unwrap();

    // The copy rescues more of the first MiB meanwhile: another source as far as the session is concerned (P7).
    assert!(map.set(64 * 512, 64 * 512, RegionState::Rescued));
    map.save(&map_path(&image)).unwrap();
    let mapped = MappedSource::open(&image).unwrap();
    let after = SourceIdentity::of_mapped(&mapped).unwrap();
    assert_ne!(after, before);
    let r = scan_session(
        &mapped,
        &after,
        &all_formats(),
        &opts(2),
        &mut session,
        &sopts(),
        &CancelToken::default(),
        &mut |_| {},
    );
    assert_eq!(r.unwrap_err().code(), "session_other_source");
    assert_eq!(
        check_session(session_dir.path(), &after)
            .unwrap_err()
            .code(),
        "session_other_source"
    );
}

#[test]
fn session_in_a_non_empty_dir_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("photo.jpg"), b"x").unwrap();
    let source = SimSource::zeros(MIB);
    let err = Session::create(&dest(&source, dir.path()), identity(&source), &opts(1)).unwrap_err();
    assert_eq!(err.code(), "session_not_empty");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    // A folder holding a session is not empty either.
    let other = tempfile::tempdir().unwrap();
    new_session(&source, other.path(), &opts(1));
    let err =
        Session::create(&dest(&source, other.path()), identity(&source), &opts(1)).unwrap_err();
    assert_eq!(err.code(), "session_not_empty");
}

#[test]
fn session_on_the_source_is_refused() {
    let fake = FakeDevices::new();
    let dir = tempfile::tempdir().unwrap();
    fake.set_destination(dir.path(), Ok(dest_info(&["disk4"])));
    let source = SimSource::zeros(MIB).disk("disk4");
    let err = Destination::check(&fake, &source, dir.path()).unwrap_err();
    assert_eq!(err.code(), "destination_on_source");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn reopened_session_folder_on_the_source_disk_is_refused() {
    // A device session interrupted, then reopened: its folder is checked with the caller's `DeviceOps` (here the fake),
    // never with the running system behind the caller's back.
    let fake = FakeDevices::new();
    fake.add(sdxc(Some("S1")), card_bytes());
    let dir = tempfile::tempdir().unwrap();
    fake.set_destination(dir.path(), Ok(dest_info(&["disk2"])));
    let source = device(&fake);
    let id = SourceIdentity::of_device(&sdxc(Some("S1")), &source).unwrap();
    let o = opts(2);
    let mut session = Session::create(
        &Destination::check(&fake, &source, dir.path()).unwrap(),
        id.clone(),
        &o,
    )
    .unwrap();
    let cancel = CancelToken::default();
    let mut seen = 0;
    let r = scan_session(
        &source,
        &id,
        &all_formats(),
        &o,
        &mut session,
        &sopts(),
        &cancel,
        &mut |e| {
            if matches!(e, Event::Progress { .. }) {
                seen += 1;
                if seen == 3 {
                    cancel.cancel();
                }
            }
        },
    );
    assert_eq!(r.unwrap_err().code(), "cancelled");
    let files = |d: &Path| {
        let mut v: Vec<_> = std::fs::read_dir(d)
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                (e.file_name(), std::fs::read(e.path()).unwrap())
            })
            .collect();
        v.sort();
        v
    };
    let before = files(dir.path());
    let run = |session: &mut Session| {
        scan_session(
            &source,
            &id,
            &all_formats(),
            &o,
            session,
            &sopts(),
            &CancelToken::default(),
            &mut |_| {},
        )
    };

    // Reopened with no checked folder, a device session cannot tell where its folder lives: refused, nothing written.
    let mut bare = Session::open(dir.path()).unwrap();
    assert_eq!(run(&mut bare).unwrap_err().code(), "destination_unknown");
    assert_eq!(files(dir.path()), before);

    // The folder now answers as the card itself (the caller's ops say so): it cannot be checked, so cannot be reopened.
    fake.set_destination(dir.path(), Ok(dest_info(&["disk4"])));
    assert_eq!(
        Destination::check(&fake, &source, dir.path())
            .unwrap_err()
            .code(),
        "destination_on_source"
    );
    assert_eq!(files(dir.path()), before);

    // Checked against another disk, it resumes to the end.
    fake.set_destination(dir.path(), Ok(dest_info(&["disk2"])));
    let dest = Destination::check(&fake, &source, dir.path()).unwrap();
    let mut checked = Session::open_checked(&dest).unwrap();
    assert!(matches!(checked.state(), SessionState::Interrupted { .. }));
    run(&mut checked).unwrap();
    assert!(matches!(checked.state(), SessionState::Finished(_)));
}

#[test]
fn options_come_from_the_session() {
    let card = cards::a();
    let (expected, summary) = reference(&card.source, &opts(4));
    let dir = tempfile::tempdir().unwrap();
    let mut session = new_session(&card.source, dir.path(), &opts(4));
    // Interrupted with the 1 MiB block, resumed asking for 4 MiB blocks and no file systems: the session's win.
    let k = expected.len() / 2;
    run_cancelled_at(&card.source, &mut session, &opts(4), k, &mut |_| {}).unwrap_err();
    let other = ScanOptions {
        block: 4 * MIB as usize,
        filesystems: false,
        threads: 2,
        ..ScanOptions::default()
    };
    let mut reopened = Session::open(dir.path()).unwrap();
    let (r, events) = run(&card.source, &mut reopened, &other);
    assert_eq!(r.unwrap(), summary);
    assert_events(&events, &expected, "resumed with other options");
    // A fresh session created with 1 MiB blocks also scans with them.
    let fresh = tempfile::tempdir().unwrap();
    let mut session = new_session(&card.source, fresh.path(), &opts(4));
    let (r, events) = run(&card.source, &mut session, &other);
    assert_eq!(r.unwrap(), summary);
    assert_events(&events, &expected, "fresh with other options");
}

#[test]
fn a_damaged_or_newer_manifest_is_refused() {
    let source = SimSource::zeros(MIB);
    let dir = tempfile::tempdir().unwrap();
    new_session(&source, dir.path(), &opts(1));
    let manifest = dir.path().join("session.rvs");
    let good = std::fs::read(&manifest).unwrap();
    let mut newer = good.clone();
    newer[9] = 2;
    std::fs::write(&manifest, &newer).unwrap();
    assert_eq!(
        Session::open(dir.path()).unwrap_err().code(),
        "session_version"
    );
    assert_eq!(
        check_session(dir.path(), &identity(&source))
            .unwrap_err()
            .code(),
        "session_version"
    );
    std::fs::write(&manifest, &good).unwrap();
    Session::open(dir.path()).unwrap();
    flip_last_byte(&manifest);
    assert_eq!(
        Session::open(dir.path()).unwrap_err().code(),
        "session_corrupt"
    );
    // No session at all: nothing to check; opening is `open_failed`.
    let empty = tempfile::tempdir().unwrap();
    check_session(empty.path(), &identity(&source)).unwrap();
    check_session(&empty.path().join("missing"), &identity(&source)).unwrap();
    assert_eq!(
        Session::open(empty.path()).unwrap_err().code(),
        "open_failed"
    );
}
