//! The pre-flight check reports every known problem at once, before any long read (permission, a silent device, a failed
//! unmount, a copy destination that cannot hold the card), and a reconnected card is found again and unmounted before
//! anything but its identity is read.

mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use revenant_core::device::{DestInfo, DeviceOps, DiskInfo, FAT_MAX_FILE, VolumeInfo, reconnect};
use revenant_core::preflight::{Issue, PreflightRequest, SAMPLE_TIMEOUT, preflight};
use revenant_core::rescue::{RegionState, RescueMap, map_path};
use revenant_core::session::Session;
use revenant_core::source::{DeviceSource, DiskId};
use revenant_core::{CancelToken, Destination, ScanOptions, SourceIdentity};
use revenant_testkit::fs::{FsBuilder, FsType, Partitioning, disk};
use support::{FakeDevices, disk_info};

const MIB: u64 = 1 << 20;
const SIZE: u64 = 64 * MIB;
const LAST: u64 = SIZE - 512;
const MIDDLE: u64 = SIZE / 2;

fn id(s: &str) -> DiskId {
    DiskId::parse(s).unwrap()
}

/// A 64 MiB card: a FAT32 volume of 48 MiB at sector `first_lba` (MBR).
fn card(first_lba: u32) -> Vec<u8> {
    let volume = FsBuilder::new(FsType::Fat32, 48 * MIB, 512).build();
    disk(Partitioning::Mbr { first_lba }, &volume, SIZE as usize)
}

/// A 64 MiB "SDXC" card `id` with one mounted volume `<id>s1`.
fn info(id: &str, serial: Option<&str>) -> DiskInfo {
    let mut info = disk_info(id, SIZE);
    info.model = Some("SDXC".into());
    info.serial = serial.map(str::to_string);
    info.volumes = vec![VolumeInfo {
        id: format!("{id}s1"),
        fs: Some("msdos".into()),
        label: Some("CARD".into()),
        mount_point: Some(PathBuf::from("/Volumes/CARD")),
        size: 48 * MIB,
    }];
    info
}

fn dest(disks: &[&str], free: u64, max_file: Option<u64>) -> DestInfo {
    DestInfo {
        disks: disks.iter().map(|d| id(d)).collect(),
        fs: Some(if max_file.is_some() { "msdos" } else { "apfs" }.into()),
        free_bytes: free,
        max_file_bytes: max_file,
    }
}

/// disk4 (the card, serial "S1") and a destination folder on disk2 with 1 GiB free; the copy goes to `<tmp>/card.img`.
struct Setup {
    fake: FakeDevices,
    dir: tempfile::TempDir,
    copy_to: PathBuf,
}

fn setup() -> Setup {
    let fake = FakeDevices::new();
    fake.add(info("disk4", Some("S1")), card(2048));
    let dir = tempfile::tempdir().unwrap();
    fake.set_destination(dir.path(), Ok(dest(&["disk2"], 1 << 30, None)));
    let copy_to = dir.path().join("card.img");
    Setup { fake, dir, copy_to }
}

fn request<'a>(disk: &'a DiskId, copy_to: Option<&'a Path>) -> PreflightRequest<'a> {
    PreflightRequest {
        disk,
        unmount: true,
        copy_to,
        session: None,
        sample_timeout: SAMPLE_TIMEOUT,
    }
}

fn issues(ops: &FakeDevices, disk: &str, copy_to: Option<&Path>) -> Vec<Issue> {
    preflight(ops, &request(&id(disk), copy_to)).issues
}

/// The identity the card `disk` shows now.
fn identity_of(ops: &FakeDevices, disk: &str) -> SourceIdentity {
    let info = ops
        .list()
        .unwrap()
        .into_iter()
        .find(|d| d.id == id(disk))
        .unwrap();
    let source = DeviceSource::new(ops.open(&id(disk)).unwrap(), id(disk));
    SourceIdentity::of_device(&info, &source).unwrap()
}

#[test]
fn healthy_card_has_no_issues() {
    let s = setup();
    let disk4 = id("disk4");
    let report = preflight(&s.fake, &request(&disk4, Some(&s.copy_to)));
    assert_eq!(report.issues, []);
    assert!(report.guard.is_some());
    assert_eq!(report.disk, Some(info("disk4", Some("S1"))));
    assert!(!s.fake.mounted("disk4"));
    let log = s.fake.log();
    assert_eq!(
        log[..6],
        [
            "list",
            "open disk4",
            "unmount disk4",
            "read disk4 0",
            &format!("read disk4 {MIDDLE}"),
            &format!("read disk4 {LAST}"),
        ],
        "{log:?}"
    );
    assert!(
        log[6..].iter().all(|e| e.starts_with("destination ")),
        "{log:?}"
    );
    // Nothing is written: no image, no map.
    assert_eq!(std::fs::read_dir(s.dir.path()).unwrap().count(), 0);

    // Without an unmount nor a copy: no guard, no destination asked.
    let fresh = setup();
    let report = preflight(
        &fresh.fake,
        &PreflightRequest {
            unmount: false,
            ..request(&disk4, None)
        },
    );
    assert_eq!(report.issues, []);
    assert!(report.guard.is_none());
    assert!(fresh.fake.mounted("disk4"));
    assert_eq!(fresh.fake.log().len(), 5, "{:?}", fresh.fake.log());
}

#[test]
fn each_problem_has_its_code() {
    let s = setup();
    s.fake.deny_open("disk4");
    assert_eq!(
        issues(&s.fake, "disk4", Some(&s.copy_to)),
        [Issue::PermissionDenied]
    );
    assert!(
        s.fake.mounted("disk4"),
        "nothing unmounted when it cannot be read"
    );

    let s = setup();
    assert_eq!(
        issues(&s.fake, "disk9", Some(&s.copy_to)),
        [Issue::DeviceNotFound]
    );
    assert_eq!(s.fake.log(), ["list"]);

    let s = setup();
    s.fake.bad("disk4", LAST, 512);
    assert_eq!(
        issues(&s.fake, "disk4", Some(&s.copy_to)),
        [Issue::SampleUnreadable { offset: LAST }]
    );

    let s = setup();
    s.fake.fail_unmount("disk4", &["disk4s1"]);
    let report = preflight(&s.fake, &request(&id("disk4"), Some(&s.copy_to)));
    assert_eq!(
        report.issues,
        [Issue::UnmountFailed {
            volumes: vec!["disk4s1".into()]
        }]
    );
    assert!(report.guard.is_none());
    assert!(
        s.fake.log().contains(&format!("read disk4 {LAST}")),
        "the samples are still read"
    );

    let s = setup();
    s.fake
        .set_destination(s.dir.path(), Ok(dest(&["disk2"], 10 * MIB, None)));
    assert_eq!(
        issues(&s.fake, "disk4", Some(&s.copy_to)),
        [Issue::DestinationTooSmall {
            need: 64 * MIB,
            free: 10 * MIB
        }]
    );

    let s = setup();
    s.fake
        .set_destination(s.dir.path(), Ok(dest(&["disk4"], 1 << 30, None)));
    assert_eq!(
        issues(&s.fake, "disk4", Some(&s.copy_to)),
        [Issue::DestinationOnSource]
    );

    let s = setup();
    s.fake
        .set_destination(s.dir.path(), Err("system_tool_failed"));
    assert_eq!(
        issues(&s.fake, "disk4", Some(&s.copy_to)),
        [Issue::DestinationUnknown]
    );

    // An 8 GiB card (virtual, nothing allocated) onto FAT32: the image would be over 4 GiB − 1.
    let s = setup();
    s.fake.add_virtual(
        DiskInfo {
            geometry: revenant_core::source::Geometry {
                size: 8 << 30,
                logical_sector: 512,
                physical_sector: 512,
            },
            ..info("disk6", None)
        },
        card(2048),
    );
    s.fake.set_destination(
        s.dir.path(),
        Ok(dest(&["disk2"], 1 << 40, Some(FAT_MAX_FILE))),
    );
    assert_eq!(
        issues(&s.fake, "disk6", Some(&s.copy_to)),
        [Issue::DestinationFileTooLarge {
            limit: 4_294_967_295
        }]
    );

    // A resume needs only what is not rescued yet: 48 MiB rescued → 16 MiB needed.
    let s = setup();
    let mut map = RescueMap::new(SIZE, 512, identity_of(&s.fake, "disk4"));
    assert!(map.set(0, 48 * MIB, RegionState::Rescued));
    map.save(&map_path(&s.copy_to)).unwrap();
    s.fake
        .set_destination(s.dir.path(), Ok(dest(&["disk2"], 20 * MIB, None)));
    assert_eq!(issues(&s.fake, "disk4", Some(&s.copy_to)), []);
    s.fake
        .set_destination(s.dir.path(), Ok(dest(&["disk2"], 15 * MIB, None)));
    assert_eq!(
        issues(&s.fake, "disk4", Some(&s.copy_to)),
        [Issue::DestinationTooSmall {
            need: 16 * MIB,
            free: 15 * MIB
        }]
    );
    // A map of another card counts as a fresh copy (the copy itself refuses it).
    let other = FakeDevices::new();
    other.add(info("disk4", Some("S1")), card(4096));
    let mut map = RescueMap::new(SIZE, 512, identity_of(&other, "disk4"));
    assert!(map.set(0, 48 * MIB, RegionState::Rescued));
    map.save(&map_path(&s.copy_to)).unwrap();
    s.fake
        .set_destination(s.dir.path(), Ok(dest(&["disk2"], 20 * MIB, None)));
    assert_eq!(
        issues(&s.fake, "disk4", Some(&s.copy_to)),
        [Issue::DestinationTooSmall {
            need: 64 * MIB,
            free: 20 * MIB
        }]
    );
}

/// A new session folder `dir` for disk4 of `ops`, recording `identity` as its source.
fn session_of(ops: &FakeDevices, dir: &Path, identity: SourceIdentity) {
    std::fs::create_dir(dir).unwrap();
    let source = DeviceSource::new(ops.open(&id("disk4")).unwrap(), id("disk4"));
    let dest = Destination::check(ops, &source, dir).unwrap();
    Session::create(&dest, identity, &ScanOptions::default()).unwrap();
}

#[test]
fn session_issues_are_reported() {
    let s = setup();
    let ask = |dir: &Path| {
        preflight(
            &s.fake,
            &PreflightRequest {
                session: Some(dir),
                ..request(&id("disk4"), None)
            },
        )
        .issues
    };
    // No session there (an empty folder, no folder): nothing to report.
    assert_eq!(ask(s.dir.path()), []);
    assert_eq!(ask(&s.dir.path().join("missing")), []);
    // A session of this card; of another card (same model and size, other identity sectors).
    let mine = s.dir.path().join("mine");
    session_of(&s.fake, &mine, identity_of(&s.fake, "disk4"));
    assert_eq!(ask(&mine), []);
    let other = FakeDevices::new();
    other.add(info("disk4", Some("S1")), card(4096));
    let theirs = s.dir.path().join("theirs");
    session_of(&s.fake, &theirs, identity_of(&other, "disk4"));
    assert_eq!(ask(&theirs), [Issue::SessionOtherSource]);
    // A manifest damaged, or of a newer version: session_corrupt (C11).
    let manifest = mine.join("session.rvs");
    let good = std::fs::read(&manifest).unwrap();
    let mut b = good.clone();
    *b.last_mut().unwrap() ^= 1;
    std::fs::write(&manifest, &b).unwrap();
    assert_eq!(ask(&mine), [Issue::SessionCorrupt]);
    let mut b = good.clone();
    b[9] = 2;
    std::fs::write(&manifest, &b).unwrap();
    assert_eq!(ask(&mine), [Issue::SessionCorrupt]);
    // After the other checks, in order.
    std::fs::write(&manifest, &good).unwrap();
    s.fake.fail_unmount("disk4", &["disk4s1"]);
    assert_eq!(
        preflight(
            &s.fake,
            &PreflightRequest {
                session: Some(&theirs),
                ..request(&id("disk4"), Some(&s.copy_to))
            },
        )
        .issues,
        [
            Issue::UnmountFailed {
                volumes: vec!["disk4s1".into()]
            },
            Issue::SessionOtherSource
        ]
    );
}

#[test]
fn the_system_disk_is_never_touched() {
    let s = setup();
    let mut system = info("disk0", None);
    system.system = true;
    s.fake.add(system, card(2048));
    let report = preflight(&s.fake, &request(&id("disk0"), Some(&s.copy_to)));
    assert_eq!(report.issues, [Issue::SystemDisk]);
    assert!(report.guard.is_none());
    assert!(report.disk.is_some_and(|d| d.system));
    // Listed, then nothing: no open, no unmount, no read, no destination.
    assert_eq!(s.fake.log(), ["list"]);
    assert!(s.fake.mounted("disk0"));
}

#[test]
fn a_disk_that_cannot_be_listed_or_opened_is_open_failed() {
    let s = setup();
    s.fake.fail_list(Some("system_tool_failed"));
    assert_eq!(
        issues(&s.fake, "disk4", Some(&s.copy_to)),
        [Issue::OpenFailed]
    );
    s.fake.fail_list(None);
    s.fake.fail_open("disk4", "open_failed");
    assert_eq!(
        issues(&s.fake, "disk4", Some(&s.copy_to)),
        [Issue::OpenFailed]
    );
    s.fake.fail_open("disk4", "system_tool_failed");
    assert_eq!(
        issues(&s.fake, "disk4", Some(&s.copy_to)),
        [Issue::OpenFailed]
    );
    // Gone between the listing and the open: not found.
    s.fake.fail_open("disk4", "device_not_found");
    assert_eq!(
        issues(&s.fake, "disk4", Some(&s.copy_to)),
        [Issue::DeviceNotFound]
    );
    assert!(
        s.fake.mounted("disk4"),
        "nothing unmounted when it cannot be read"
    );
}

#[test]
fn bad_bytes_of_a_map_count_as_still_to_copy() {
    // 40 MiB rescued, 8 MiB bad, 16 MiB untried: retries are not known yet, so 24 MiB are needed.
    let s = setup();
    let mut map = RescueMap::new(SIZE, 512, identity_of(&s.fake, "disk4"));
    assert!(map.set(0, 40 * MIB, RegionState::Rescued));
    assert!(map.set(40 * MIB, 8 * MIB, RegionState::Bad));
    map.save(&map_path(&s.copy_to)).unwrap();
    s.fake
        .set_destination(s.dir.path(), Ok(dest(&["disk2"], 20 * MIB, None)));
    assert_eq!(
        issues(&s.fake, "disk4", Some(&s.copy_to)),
        [Issue::DestinationTooSmall {
            need: 24 * MIB,
            free: 20 * MIB
        }]
    );
    s.fake
        .set_destination(s.dir.path(), Ok(dest(&["disk2"], 24 * MIB, None)));
    assert_eq!(issues(&s.fake, "disk4", Some(&s.copy_to)), []);
}

#[test]
fn everything_is_reported_at_once() {
    let s = setup();
    s.fake.add_virtual(
        DiskInfo {
            geometry: revenant_core::source::Geometry {
                size: 8 << 30,
                logical_sector: 512,
                physical_sector: 512,
            },
            ..info("disk6", None)
        },
        card(2048),
    );
    s.fake.fail_unmount("disk6", &["disk6s1"]);
    s.fake.set_destination(
        s.dir.path(),
        Ok(dest(&["disk2"], 10 * MIB, Some(FAT_MAX_FILE))),
    );
    assert_eq!(
        issues(&s.fake, "disk6", Some(&s.copy_to)),
        [
            Issue::UnmountFailed {
                volumes: vec!["disk6s1".into()]
            },
            Issue::DestinationTooSmall {
                need: 8 << 30,
                free: 10 * MIB
            },
            Issue::DestinationFileTooLarge {
                limit: FAT_MAX_FILE
            },
        ]
    );
}

#[test]
fn not_responding_device_is_reported_within_the_timeout() {
    let s = setup();
    s.fake.hang_reads("disk4", MIDDLE);
    let disk4 = id("disk4");
    let start = Instant::now();
    let report = preflight(
        &s.fake,
        &PreflightRequest {
            sample_timeout: Duration::from_millis(200),
            ..request(&disk4, Some(&s.copy_to))
        },
    );
    let took = start.elapsed();
    assert_eq!(report.issues, [Issue::NotResponding { offset: MIDDLE }]);
    assert!(took >= Duration::from_millis(200), "{took:?}");
    assert!(took < Duration::from_secs(1), "{took:?}");
}

#[test]
fn issue_codes_are_stable() {
    let codes: Vec<&str> = [
        Issue::PermissionDenied,
        Issue::DeviceNotFound,
        Issue::SystemDisk,
        Issue::OpenFailed,
        Issue::NotResponding { offset: 0 },
        Issue::SampleUnreadable { offset: 0 },
        Issue::UnmountFailed { volumes: vec![] },
        Issue::DestinationTooSmall { need: 1, free: 0 },
        Issue::DestinationOnSource,
        Issue::DestinationUnknown,
        Issue::DestinationFileTooLarge { limit: 0 },
        Issue::SessionOtherSource,
        Issue::SessionCorrupt,
    ]
    .iter()
    .map(Issue::code)
    .collect();
    assert_eq!(
        codes,
        [
            "permission_denied",
            "device_not_found",
            "system_disk",
            "open_failed",
            "device_not_responding",
            "sample_unreadable",
            "unmount_failed",
            "destination_too_small",
            "destination_on_source",
            "destination_unknown",
            "destination_file_too_large",
            "session_other_source",
            "session_corrupt",
        ]
    );
}

/// The reads of `disk` logged before `until` (an exact log entry), as offsets, and whether `until` was logged at all.
fn reads_before(log: &[String], disk: &str, until: &str) -> Vec<u64> {
    let prefix = format!("read {disk} ");
    log.iter()
        .take_while(|e| *e != until)
        .filter_map(|e| e.strip_prefix(&prefix).map(|o| o.parse().unwrap()))
        .collect()
}

/// Identity sectors of a 512-byte-sector MBR card whose first volume starts at sector `first_lba`: sector 0, the GPT
/// area 1..=33 and the volume's boot sector.
fn identity_offset(offset: u64, first_lba: u64) -> bool {
    offset <= 33 * 512 || offset == first_lba * 512
}

/// What `reconnect` ended with: the disk found and its unmount (locks held, or the error code), or the error code.
type Outcome = Result<(String, Result<usize, &'static str>), &'static str>;

/// Runs `reconnect` on another thread (it waits), returning its outcome.
fn reconnect_in_background(
    fake: &Arc<FakeDevices>,
    identity: SourceIdentity,
    cancel: &CancelToken,
) -> std::thread::JoinHandle<Outcome> {
    let fake = fake.clone();
    let cancel = cancel.clone();
    std::thread::spawn(move || {
        reconnect(&*fake, &identity, &cancel, Duration::from_millis(50))
            .map(|r| {
                (
                    r.source.disk_id().as_str().to_string(),
                    r.unmount.map(|g| g.held()).map_err(|e| e.code()),
                )
            })
            .map_err(|e| e.code())
    })
}

#[test]
fn reconnect_unmounts_before_reading() {
    let fake = Arc::new(FakeDevices::new());
    fake.add(info("disk4", Some("S1")), card(2048));
    let identity = identity_of(&fake, "disk4");
    fake.remove("disk4");
    // A look-alike without a serial (same size and model, another table): a candidate, opened for its identity, never
    // unmounted. One with another serial is not even a candidate.
    fake.add(info("disk6", None), card(4096));
    fake.add(info("disk7", Some("S2")), card(2048));
    let cancel = CancelToken::default();
    let waiting = reconnect_in_background(&fake, identity, &cancel);
    std::thread::sleep(Duration::from_millis(300));
    assert!(!waiting.is_finished(), "nothing matches yet");
    fake.add(info("disk5", Some("S1")), card(2048));
    let (disk, unmount) = waiting.join().unwrap().unwrap();
    assert_eq!(disk, "disk5");
    assert_eq!(unmount, Ok(0));
    assert!(!fake.mounted("disk5"));
    assert!(fake.mounted("disk6"));
    assert!(fake.mounted("disk7"));

    let log = fake.log();
    let opens: Vec<usize> = (0..log.len()).filter(|&i| log[i] == "open disk5").collect();
    let unmount_at = log.iter().position(|e| e == "unmount disk5").unwrap();
    assert!(opens.len() == 2, "{log:?}");
    assert!(opens[0] < unmount_at && unmount_at < opens[1], "{log:?}");
    let before = reads_before(&log, "disk5", "unmount disk5");
    assert!(
        before.contains(&0) && before.contains(&(2048 * 512)),
        "{before:?}"
    );
    assert!(
        before.iter().all(|&o| identity_offset(o, 2048)),
        "{before:?}"
    );
    assert!(log.iter().all(|e| e != "unmount disk6"), "{log:?}");
    assert!(log.contains(&"open disk6".to_string()), "{log:?}");
    assert!(!log.iter().any(|e| e.ends_with("disk7")), "{log:?}");
    assert!(!log.iter().any(|e| e == "unmount disk4"), "{log:?}");

    // Cancelled while waiting for a card that never comes back.
    let fake = Arc::new(FakeDevices::new());
    fake.add(info("disk4", Some("S1")), card(2048));
    let identity = identity_of(&fake, "disk4");
    fake.remove("disk4");
    let cancel = CancelToken::default();
    let waiting = reconnect_in_background(&fake, identity, &cancel);
    std::thread::sleep(Duration::from_millis(120));
    let start = Instant::now();
    cancel.cancel();
    assert_eq!(waiting.join().unwrap(), Err("cancelled"));
    assert!(start.elapsed() < Duration::from_millis(500));
}

#[test]
fn reconnect_matches_a_card_without_serial() {
    let fake = Arc::new(FakeDevices::new());
    fake.add(info("disk4", None), card(2048));
    let identity = identity_of(&fake, "disk4");
    fake.remove("disk4");
    // Another card with the same size and model but another table: opened, skipped, never unmounted.
    fake.add(info("disk5", None), card(4096));
    fake.add(info("disk6", None), card(2048));
    let (disk, unmount) = reconnect_in_background(&fake, identity, &CancelToken::default())
        .join()
        .unwrap()
        .unwrap();
    assert_eq!(disk, "disk6");
    assert_eq!(unmount, Ok(0));
    let log = fake.log();
    assert!(log.contains(&"open disk5".to_string()), "{log:?}");
    assert!(!log.contains(&"unmount disk5".to_string()), "{log:?}");
    assert!(fake.mounted("disk5"));
    assert!(!fake.mounted("disk6"));
}

#[test]
fn a_hanging_look_alike_neither_blocks_nor_piles_up() {
    let fake = Arc::new(FakeDevices::new());
    fake.add(info("disk4", Some("S1")), card(2048));
    let identity = identity_of(&fake, "disk4");
    fake.remove("disk4");
    // A serial-less look-alike whose reads never return: a candidate, probed once, never waited for.
    fake.add(info("disk3", None), card(4096));
    fake.hang_reads("disk3", 0);
    let cancel = CancelToken::default();
    let waiting = reconnect_in_background(&fake, identity, &cancel);
    std::thread::sleep(Duration::from_millis(400));
    assert!(!waiting.is_finished());
    fake.add(info("disk5", Some("S1")), card(2048));
    let start = Instant::now();
    let (disk, unmount) = waiting.join().unwrap().unwrap();
    assert_eq!(disk, "disk5");
    assert_eq!(unmount, Ok(0));
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "{:?}",
        start.elapsed()
    );
    let log = fake.log();
    let opened = log.iter().filter(|e| *e == "open disk3").count();
    assert_eq!(opened, 1, "{log:?}");
    assert!(fake.mounted("disk3"));
}
