//! Opt-in: disk images that macOS formatted and wrote to, attached READ-WRITE by `hdiutil` (so any write by the engine
//! would show in them) and read as real devices: listed, unmounted, opened, checked, scanned, copied, resumed and
//! remounted through `SystemDevices`, with the whole device hashed before and after to prove nothing was written.
//! Only images this test created in its own temp dirs are attached (`revenant_testkit::os_fs`); the engine calls
//! `diskutil` only on the disk `hdiutil attach` printed for them. Never a physical disk, never `sudo`: when the attached
//! image's `/dev/rdiskN` cannot be read without root, the test prints "skipped: needs read access to /dev/rdiskN".
//!
//! Run with `REVENANT_OS_DEV=1 cargo test -p revenant-core --test os_devices -- --ignored --test-threads=1 --nocapture`.
#![cfg(target_os = "macos")]

use std::path::Path;
use std::time::Instant;

use revenant_core::device::{DeviceOps, DiskInfo, SystemDevices};
use revenant_core::formats::all_formats;
use revenant_core::preflight::{PreflightRequest, SAMPLE_TIMEOUT, preflight};
use revenant_core::rescue::{CopyEvent, CopyOptions, copy, map_path};
use revenant_core::session::{Session, SessionOptions, scan_session};
use revenant_core::source::{DeviceSource, DiskId, FileSource, Source};
use revenant_core::{CancelToken, Destination, Event, ScanOptions, SourceIdentity, scan};
use revenant_testkit::os_fs::{
    AttachedDisk, OsFs, OsImage, OsLayout, dev_available, with_attached,
};
use revenant_testkit::{Mp4Options, TestExif, TiffRawOptions, TiffVariant, jpeg, mp4, tiff_raw};

const MIB: u64 = 1 << 20;
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// The FNV-1a 64 of the whole source, read in 1 MiB chunks (the core's `fnv1a64` is crate-private, ruling C12), and
/// the seconds it took.
fn digest(source: &dyn Source) -> (u64, f64) {
    let started = Instant::now();
    let mut h = FNV_OFFSET;
    let mut buf = vec![0u8; MIB as usize];
    let mut pos = 0;
    while pos < source.len() {
        let want = (source.len() - pos).min(MIB) as usize;
        let n = source.read_at(pos, &mut buf[..want]).unwrap();
        assert_eq!(n, want, "short read at {pos}");
        h = buf[..n]
            .iter()
            .fold(h, |h, &b| (h ^ u64::from(b)).wrapping_mul(FNV_PRIME));
        pos += n as u64;
    }
    (h, started.elapsed().as_secs_f64())
}

fn listed(disk: &DiskId) -> DiskInfo {
    SystemDevices
        .list()
        .unwrap()
        .into_iter()
        .find(|d| &d.id == disk)
        .unwrap_or_else(|| panic!("{} not listed", disk.as_str()))
}

fn events_of(source: &dyn Source, opts: &ScanOptions) -> Vec<Event> {
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

fn assert_events(got: &[Event], want: &[Event], what: &str) {
    assert_eq!(got.len(), want.len(), "{what}: event count");
    if let Some(i) = (0..got.len()).find(|&i| got[i] != want[i]) {
        panic!("{what}: event {i} differs: {:?} vs {:?}", got[i], want[i]);
    }
}

/// The photos, the RAW and the video written to `DCIM/100TEST` through the OS driver; one JPEG deleted.
fn fill(mount: &Path) {
    let dir = mount.join("DCIM/100TEST");
    std::fs::create_dir_all(&dir).unwrap();
    for i in 0..4 {
        std::fs::write(
            dir.join(format!("DSC0000{}.JPG", i + 1)),
            jpeg(640, 480, 80 + i, 0),
        )
        .unwrap();
    }
    let arw = tiff_raw(&TiffRawOptions {
        variant: TiffVariant::Arw,
        big_endian: false,
        raw_len: 200_000,
        seed: 11,
        preview: Some((160, 120)),
        preview_sector_aligned: false,
        exif: TestExif::default(),
    })
    .bytes;
    std::fs::write(dir.join("DSC00005.ARW"), arw).unwrap();
    let video = mp4(&Mp4Options {
        mdat_len: 300_000,
        moov_at_end: false,
        mov: false,
        mdat_64bit: false,
    });
    std::fs::write(dir.join("MOV00006.MP4"), video).unwrap();
    std::fs::remove_file(dir.join("DSC00002.JPG")).unwrap();
}

/// For an exFAT card and a FAT32 card (MBR, 64 MiB), each with its copy and its session written to an attached image
/// of the other file system (ruling P33): list → unmount → hash → pre-flight → scan == scan of the image's bytes →
/// copy (cancelled, then resumed from its map) == those bytes → session (cancelled at its middle event, then resumed)
/// == the scan → hash again, unchanged → remount.
#[test]
#[ignore]
fn os_device_round_trip() {
    if !dev_available() {
        return;
    }
    for (card_fs, dest_fs, label) in [
        (OsFs::Exfat, OsFs::Fat32, "RVTDEST32"),
        (OsFs::Fat32, OsFs::Exfat, "RVTDESTX"),
    ] {
        let card = OsImage::create(card_fs, OsLayout::Mbr, 64).expect("hdiutil create");
        card.with_mounted(fill);
        let dest =
            OsImage::create_labeled(dest_fs, OsLayout::Mbr, 160, label).expect("hdiutil create");
        let case = format!("{card_fs:?} card, {dest_fs:?} destination");
        with_attached(&dest.path, true, |out| {
            let out_mount = out.mount.clone().expect("the destination is mounted");
            with_attached(&card.path, true, |att| {
                round_trip(&case, &card.path, att, &out_mount)
            });
        });
    }
}

fn round_trip(case: &str, image: &Path, att: &AttachedDisk, out: &Path) {
    let d = DiskId::parse(&att.disk).unwrap();
    assert_eq!(
        d.as_str(),
        att.disk,
        "{case}: the whole disk hdiutil printed"
    );
    assert!(
        att.devices.contains(&format!("/dev/{}", att.disk)),
        "{case}"
    );

    // 1. Listed: a virtual disk of 64 MiB with a mounted volume.
    let info = listed(&d);
    assert!(info.virtual_disk, "{case}: {info:?}");
    assert!(!info.system, "{case}");
    assert_eq!(info.geometry.size, 64 * MIB, "{case}");
    assert!(
        info.volumes.iter().any(|v| v.mount_point.is_some()),
        "{case}: no mounted volume in {info:?}"
    );

    // Open it read-only (or skip without root).
    att.confirm();
    let raw = match SystemDevices.open(&d) {
        Ok(raw) => raw,
        Err(e) if e.code() == "permission_denied" => {
            eprintln!("skipped: needs read access to /dev/r{}", att.disk);
            return;
        }
        Err(e) => panic!("{case}: open: {e:?}"),
    };
    let dev = DeviceSource::new(raw, d.clone());
    // A destination on the card's mounted volume is refused.
    let on_card = info
        .volumes
        .iter()
        .find_map(|v| v.mount_point.clone())
        .unwrap();
    assert_eq!(
        Destination::check(&SystemDevices, &dev, &on_card)
            .unwrap_err()
            .code(),
        "destination_on_source",
        "{case}"
    );

    // 2. Unmounted, not ejected.
    att.confirm();
    let guard = SystemDevices.unmount(&d).unwrap();
    let info = listed(&d);
    assert!(
        info.volumes.iter().all(|v| v.mount_point.is_none()),
        "{case}: still mounted: {info:?}"
    );
    let work = tempfile::tempdir().unwrap();
    let copy_of_image = work.path().join("card-bytes.img");
    std::fs::copy(image, &copy_of_image).unwrap();
    let file = FileSource::open(&copy_of_image).unwrap();
    assert_eq!(file.len(), dev.len(), "{case}");

    // 3. The whole device hashed.
    let (h1, dev_secs) = digest(&dev);
    let (h_file, file_secs) = digest(&file);
    assert_eq!(h1, h_file, "{case}: the device reads the image's bytes");

    // 4. Pre-flight with the copy and the session on the other image's volume.
    let copy_dir = out.join(format!("copy-{}", att.disk));
    let session_dir = out.join(format!("session-{}", att.disk));
    std::fs::create_dir(&copy_dir).unwrap();
    std::fs::create_dir(&session_dir).unwrap();
    let copy_to = copy_dir.join("card.img");
    let pf = preflight(
        &SystemDevices,
        &PreflightRequest {
            disk: &d,
            unmount: false,
            copy_to: Some(&copy_to),
            session: Some(&session_dir),
            sample_timeout: SAMPLE_TIMEOUT,
        },
    );
    assert_eq!(pf.issues, [], "{case}");

    // 5. The device scans like the image's bytes.
    let opts = ScanOptions {
        sector: u64::from(dev.scan_sector()),
        block: MIB as usize,
        ..ScanOptions::default()
    };
    let want = events_of(&file, &opts);
    let found = want.iter().filter(|e| matches!(e, Event::Found(_))).count();
    assert!(found >= 5, "{case}: {found} found");
    assert_events(&events_of(&dev, &opts), &want, case);

    // 6. The copy, cancelled after its first progress, resumes from its saved map to the image's bytes.
    let identity = SourceIdentity::of_device(&info, &dev).unwrap();
    let copy_dest = Destination::check(&SystemDevices, &dev, &copy_dir).unwrap();
    assert!(!copy_dest.info().unwrap().disks.contains(&d), "{case}");
    let copts = CopyOptions {
        sector: dev.scan_sector(),
        ..CopyOptions::default()
    };
    let cancel = CancelToken::default();
    let first = copy(
        &dev,
        &identity,
        &copy_dest,
        "card.img",
        &copts,
        &cancel,
        &mut |e| {
            if matches!(e, CopyEvent::Progress { .. }) {
                cancel.cancel();
            }
        },
    );
    assert_eq!(first.unwrap_err().code(), "cancelled", "{case}");
    assert!(map_path(&copy_to).is_file(), "{case}: the map was saved");
    let done = copy(
        &dev,
        &identity,
        &copy_dest,
        "card.img",
        &copts,
        &CancelToken::default(),
        &mut |_| {},
    )
    .unwrap();
    assert!(done.complete, "{case}");
    assert_eq!(
        std::fs::read(&copy_to).unwrap(),
        std::fs::read(&copy_of_image).unwrap(),
        "{case}: the copy"
    );

    // 7. A session cancelled at its middle event resumes to the same events.
    let session_dest = Destination::check(&SystemDevices, &dev, &session_dir).unwrap();
    let sopts = SessionOptions {
        checkpoint_bytes: MIB,
        ..SessionOptions::default()
    };
    let mut session = Session::create(&session_dest, identity.clone(), &opts).unwrap();
    let cancel = CancelToken::default();
    let middle = want.len() / 2;
    let mut seen = 0;
    let r = scan_session(
        &dev,
        &identity,
        &all_formats(),
        &opts,
        &mut session,
        &sopts,
        &cancel,
        &mut |_| {
            seen += 1;
            if seen == middle {
                cancel.cancel();
            }
        },
    );
    assert_eq!(r.unwrap_err().code(), "cancelled", "{case}");
    drop(session);
    let mut session = Session::open_checked(&session_dest).unwrap();
    let mut resumed = Vec::new();
    scan_session(
        &dev,
        &identity,
        &all_formats(),
        &opts,
        &mut session,
        &sopts,
        &CancelToken::default(),
        &mut |e| resumed.push(e),
    )
    .unwrap();
    assert_events(&resumed, &want, &format!("{case}: resumed session"));

    // 8. Nothing was written to the device.
    let (h2, _) = digest(&dev);
    assert_eq!(h2, h1, "{case}: the device changed");
    assert_eq!(
        digest(&FileSource::open(&copy_of_image).unwrap()).0,
        h1,
        "{case}"
    );

    // 9. Remounted.
    att.confirm();
    SystemDevices.remount(&d, Some(guard)).unwrap();
    let info = listed(&d);
    assert!(
        info.volumes.iter().any(|v| v.mount_point.is_some()),
        "{case}: not mounted again: {info:?}"
    );
    // Informational: whether macOS itself wrote to the volume when mounting it again (not the engine).
    let (h3, _) = digest(&dev);
    let mbps = |secs: f64| 64.0 * MIB as f64 / 1e6 / secs.max(1e-9);
    eprintln!(
        "{case}: device read {:.0} MB/s, image file read {:.0} MB/s; after the remount the device is {}",
        mbps(dev_secs),
        mbps(file_secs),
        if h3 == h1 {
            "unchanged"
        } else {
            "changed (written by macOS when mounting)"
        }
    );
}
