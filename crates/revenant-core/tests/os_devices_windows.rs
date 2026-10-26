//! Opt-in: a fixed VHD that `diskpart` creates in this test's own temp dir, formats FAT32 and attaches (read-write, so
//! any write by the engine would show), read as a real `\\.\PhysicalDriveN`: listed, checked as a destination through a
//! directory symlink and a junction, unmounted (volume lock), hashed, checked, copied, scanned, resumed and remounted
//! through `SystemDevices`, hashed again to prove nothing was written. The disk is found from the VHD's own path
//! (`detail vdisk`), never guessed; a drop guard detaches the VHD, also on panic. Never a physical disk. Needs
//! Administrator (diskpart, the raw disk). On CI (`CI=true`) a failing diskpart step fails the test; elsewhere it
//! prints "skipped: …".
//!
//! Run with `REVENANT_OS_DEV=1 cargo test -p revenant-core --test os_devices_windows -- --ignored --test-threads=1`.
#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use revenant_core::device::{DeviceOps, DiskInfo, SystemDevices};
use revenant_core::formats::all_formats;
use revenant_core::preflight::{PreflightRequest, SAMPLE_TIMEOUT, preflight};
use revenant_core::rescue::{CopyEvent, CopyOptions, copy, map_path};
use revenant_core::session::{Session, SessionOptions, scan_session};
use revenant_core::source::{DeviceSource, DiskId, FileSource, Source};
use revenant_core::{CancelToken, Destination, Event, ScanOptions, SourceIdentity, scan};
use revenant_testkit::jpeg;

const MIB: u64 = 1 << 20;
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn on_ci() -> bool {
    std::env::var("CI").as_deref() == Ok("true")
}

/// `%SystemRoot%\System32\diskpart.exe`.
fn diskpart_exe() -> PathBuf {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    Path::new(&root).join(r"System32\diskpart.exe")
}

/// Runs the diskpart script `lines` (written to `dir\name`): its output, or the output of the failure.
fn diskpart(dir: &Path, name: &str, lines: &[String]) -> Result<String, String> {
    let script = dir.join(name);
    std::fs::write(&script, lines.join("\r\n") + "\r\n").map_err(|e| e.to_string())?;
    let out = Command::new(diskpart_exe())
        .arg("/s")
        .arg(&script)
        .output()
        .map_err(|e| format!("diskpart did not start: {e}"))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    if out.status.success() {
        Ok(text)
    } else {
        Err(text)
    }
}

/// The VHD file in this test's temp dir; detached on drop (before the folder is deleted).
struct Vhd {
    path: PathBuf,
    dir: tempfile::TempDir,
}

impl Vhd {
    fn select(&self) -> String {
        format!("select vdisk file=\"{}\"", self.path.display())
    }

    fn run(&self, name: &str, lines: &[&str]) -> Result<String, String> {
        let mut all = vec![self.select()];
        all.extend(lines.iter().map(|l| l.to_string()));
        diskpart(self.dir.path(), name, &all)
    }
}

impl Drop for Vhd {
    fn drop(&mut self) {
        if let Err(text) = self.run("detach.txt", &["detach vdisk"]) {
            eprintln!("WARNING: could not detach {}:\n{text}", self.path.display());
        }
    }
}

/// `N` from the "Associated disk#: N" line of `detail vdisk`.
fn associated_disk(text: &str) -> Option<u32> {
    text.lines()
        .find(|l| l.contains("Associated disk#"))
        .and_then(|l| l.rsplit(':').next())
        .and_then(|n| n.trim().parse().ok())
}

/// The drive letter of the volume labelled `RVT` in a `detail disk` listing ("Volume 3  E  RVT  FAT32 …").
fn letter_of_rvt(text: &str) -> Option<char> {
    text.lines().find_map(|l| {
        let t: Vec<&str> = l.split_whitespace().collect();
        (t.len() >= 4 && t[0] == "Volume" && t[3] == "RVT" && t[2].len() == 1)
            .then(|| t[2].chars().next())
            .flatten()
            .filter(char::is_ascii_alphabetic)
    })
}

/// The full long path of the folder `dir` (8.3 short names expanded), without the `\\?\` prefix diskpart does not take.
fn long_path(dir: &Path) -> Result<PathBuf, String> {
    let full = std::fs::canonicalize(dir).map_err(|e| e.to_string())?;
    let text = full.to_string_lossy();
    let plain = text.strip_prefix(r"\\?\").unwrap_or(&text);
    if plain.starts_with(r"\\") {
        return Err(format!("unexpected temp path {}", full.display()));
    }
    Ok(PathBuf::from(plain))
}

/// The created, attached and formatted VHD, its disk number and its drive letter.
fn make_vhd() -> Result<(Vhd, u32, char), String> {
    let dir = tempfile::Builder::new()
        .prefix("revenant-vhd-")
        .tempdir()
        .map_err(|e| e.to_string())?;
    // diskpart attaches under the long path: an 8.3 short name in %TEMP% (`RUNNER~1`) would select another vdisk.
    let long = long_path(dir.path())?;
    let vhd = Vhd {
        path: long.join("card.vhd"),
        dir,
    };
    let create = format!(
        "create vdisk file=\"{}\" maximum=64 type=fixed",
        vhd.path.display()
    );
    diskpart(
        vhd.dir.path(),
        "create.txt",
        &[
            create,
            vhd.select(),
            "attach vdisk".into(),
            "convert mbr".into(),
            "create partition primary".into(),
            "format fs=fat32 quick label=RVT".into(),
            "assign".into(),
        ],
    )?;
    let detail = vhd.run("detail.txt", &["detail vdisk"])?;
    let n = associated_disk(&detail).ok_or_else(|| format!("no disk number in:\n{detail}"))?;
    let listing = diskpart(
        vhd.dir.path(),
        "volumes.txt",
        &[format!("select disk {n}"), "detail disk".into()],
    )?;
    let letter =
        letter_of_rvt(&listing).ok_or_else(|| format!("no RVT volume letter in:\n{listing}"))?;
    Ok((vhd, n, letter))
}

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
        h = fnv(h, &buf[..n]);
        pos += n as u64;
    }
    (h, started.elapsed().as_secs_f64())
}

fn fnv(h: u64, bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .fold(h, |h, &b| (h ^ u64::from(b)).wrapping_mul(FNV_PRIME))
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

/// Waits up to 10 s for `f` to hold.
fn wait_for(mut f: impl FnMut() -> bool) -> bool {
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        if f() {
            return true;
        }
        if Instant::now() > until {
            return false;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// `cmd /C mklink /J link target`.
fn junction(link: &Path, target: &Path) -> bool {
    Command::new("cmd")
        .arg("/C")
        .arg("mklink")
        .arg("/J")
        .arg(link)
        .arg(target)
        .output()
        .is_ok_and(|o| o.status.success())
}

/// A FAT32 VHD (64 MiB, MBR) with four photos written through its drive letter, one deleted: list → destination
/// links refused, a plain folder elsewhere accepted → unmount → hash → pre-flight → copy (cancelled, resumed from its
/// map) → the device scans like its copy → session cancelled at its middle event and resumed → hash again, unchanged →
/// remount.
#[test]
#[ignore]
fn windows_vhd_round_trip() {
    if std::env::var("REVENANT_OS_DEV").as_deref() != Ok("1") {
        // On CI this job exists to run the check: a lost variable must fail it, not pass it silently.
        if on_ci() {
            panic!("REVENANT_OS_DEV=1 is missing on CI: the VHD check would be skipped");
        }
        eprintln!("skipped: set REVENANT_OS_DEV=1 (as Administrator) to check against a VHD");
        return;
    }
    let (vhd, n, letter) = match make_vhd() {
        Ok(v) => v,
        Err(text) if on_ci() => panic!("diskpart could not create and attach the VHD:\n{text}"),
        Err(text) => {
            eprintln!("skipped: diskpart could not create and attach the VHD:\n{text}");
            return;
        }
    };
    let root = PathBuf::from(format!(r"{letter}:\"));
    eprintln!(
        "VHD {} attached as PhysicalDrive{n}, volume {}",
        vhd.path.display(),
        root.display()
    );
    assert!(
        wait_for(|| root.is_dir()),
        "{} never mounted",
        root.display()
    );

    // The photos, written through the drive letter; one deleted.
    let card = root.join(r"DCIM\100TEST");
    std::fs::create_dir_all(&card).unwrap();
    for i in 0..4u64 {
        std::fs::write(
            card.join(format!("DSC0000{}.JPG", i + 1)),
            jpeg(640, 480, 90 + i, 0),
        )
        .unwrap();
    }
    std::fs::remove_file(card.join("DSC00002.JPG")).unwrap();

    // 1. Listed: a virtual disk of 64 MiB with the mounted volume.
    let d = DiskId::parse(&format!("PhysicalDrive{n}")).unwrap();
    let info = listed(&d);
    assert!(info.virtual_disk, "{info:?}");
    assert!(!info.system, "{info:?}");
    assert_eq!(info.geometry.size, 64 * MIB, "{info:?}");
    assert!(
        info.volumes
            .iter()
            .any(|v| v.mount_point.as_deref() == Some(root.as_path())),
        "{} not among the volumes of {info:?}",
        root.display()
    );
    let raw = SystemDevices.open(&d).unwrap();
    let dev = DeviceSource::new(raw, d.clone());

    // Destinations: a directory symlink and a junction into the VHD's volume are the card; a plain folder on another
    // volume is not (and `\\?\` paths from canonicalize work there, ruling P28).
    let work = tempfile::tempdir().unwrap();
    let sym = work.path().join("sym");
    let junc = work.path().join("junction");
    std::os::windows::fs::symlink_dir(&card, &sym).expect("a directory symlink (Administrator)");
    assert!(junction(&junc, &card), "mklink /J failed");
    for link in [&sym, &junc] {
        let r = Destination::check(&SystemDevices, &dev, link);
        assert_eq!(
            r.as_ref().unwrap_err().code(),
            "destination_on_source",
            "{}: {r:?}",
            link.display()
        );
    }
    let out = work.path().join("out");
    let session_dir = work.path().join("session");
    std::fs::create_dir(&out).unwrap();
    std::fs::create_dir(&session_dir).unwrap();
    let copy_dest = Destination::check(&SystemDevices, &dev, &out).unwrap();
    let dest_disks = &copy_dest.info().unwrap().disks;
    assert!(
        !dest_disks.is_empty() && !dest_disks.contains(&d),
        "{dest_disks:?}"
    );

    // 2. Unmounted (locked), still listed; the volume cannot be opened while locked.
    let guard = SystemDevices.unmount(&d).unwrap();
    assert!(guard.held() >= 1, "no volume lock held");
    let info = listed(&d);
    eprintln!(
        "after the unmount: mount points {:?}; whether the lock and dismount took a read-only or a read-write volume \
         handle is not observable here",
        info.volumes
            .iter()
            .map(|v| &v.mount_point)
            .collect::<Vec<_>>(),
    );
    let locked = std::fs::read_dir(&card);
    assert!(locked.is_err(), "the volume is still readable while locked");

    // 3. The whole device hashed.
    let (h1, dev_secs) = digest(&dev);
    // The VHD file itself (data, then a 512-byte footer), when Windows lets it be read while attached.
    match std::fs::read(&vhd.path) {
        Ok(bytes) => {
            assert_eq!(bytes.len() as u64, 64 * MIB + 512, "fixed VHD size");
            assert_eq!(
                fnv(FNV_OFFSET, &bytes[..64 * MIB as usize]),
                h1,
                "the device reads the VHD's bytes"
            );
        }
        Err(e) => eprintln!("note: the VHD file cannot be read while attached ({e})"),
    }

    // 4. Pre-flight with the copy and the session in plain folders on another volume.
    let copy_to = out.join("card.img");
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
    assert_eq!(pf.issues, []);

    // 5. The copy, cancelled after its first progress, resumes from its saved map to the device's bytes.
    let identity = SourceIdentity::of_device(&info, &dev).unwrap();
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
    assert_eq!(first.unwrap_err().code(), "cancelled");
    assert!(map_path(&copy_to).is_file(), "the map was saved");
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
    assert!(done.complete);
    let file = FileSource::open(&copy_to).unwrap();
    let (h_copy, file_secs) = digest(&file);
    assert_eq!(h_copy, h1, "the copy");

    // 6. The device scans like its copy.
    let opts = ScanOptions {
        sector: u64::from(dev.scan_sector()),
        block: MIB as usize,
        ..ScanOptions::default()
    };
    let want = events_of(&file, &opts);
    let found = want.iter().filter(|e| matches!(e, Event::Found(_))).count();
    assert!(found >= 3, "{found} found");
    assert_events(&events_of(&dev, &opts), &want, "device scan");

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
    assert_eq!(r.unwrap_err().code(), "cancelled");
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
    assert_events(&resumed, &want, "resumed session");

    // 8. Nothing was written to the device.
    assert_eq!(digest(&dev).0, h1, "the device changed");

    // 9. Remounted: the photos are readable again.
    SystemDevices.remount(&d, Some(guard)).unwrap();
    assert!(
        wait_for(|| std::fs::read_dir(&card).is_ok_and(|r| r.count() == 3)),
        "{} not readable again",
        card.display()
    );
    let (h3, _) = digest(&dev);
    let mbps = |secs: f64| 64.0 * MIB as f64 / 1e6 / secs.max(1e-9);
    eprintln!(
        "device read {:.0} MB/s, image file read {:.0} MB/s; after the remount the device is {}",
        mbps(dev_secs),
        mbps(file_secs),
        if h3 == h1 {
            "unchanged"
        } else {
            "changed (written by Windows when mounting)"
        }
    );
    drop(dev);
    drop(vhd);
}

#[test]
fn diskpart_output_is_read() {
    let detail = "Device type ID: 2 (VHD)\r\n  Associated disk#: 3\r\n";
    assert_eq!(associated_disk(detail), Some(3));
    assert_eq!(associated_disk("Associated disk#: Not found"), None);
    let listing = "  Volume ###  Ltr  Label        Fs     Type        Size     Status     Info\r\n  \
                   ----------  ---  -----------  -----  ----------  -------  ---------  --------\r\n  \
                   Volume 4     E   RVT          FAT32  Partition     63 MB  Healthy\r\n";
    assert_eq!(letter_of_rvt(listing), Some('E'));
    assert_eq!(
        letter_of_rvt("  Volume 4         RVT          FAT32  Partition     63 MB  Healthy"),
        None
    );
}
