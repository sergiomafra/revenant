//! `revenant-dev`: developer command line for the Revenant engine. Not the end-user app.

use std::path::PathBuf;
use std::process::ExitCode;

mod bench;
mod copy_cmd;
mod devices;
mod digest;
mod output;
mod scan_cmd;

use clap::{Parser, Subcommand, ValueEnum};
use revenant_core::device::{DeviceOps, SystemDevices};
use revenant_testkit::BenchImage;

#[derive(Parser)]
#[command(
    version,
    about = "Revenant developer command line (not the end-user app)"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Scan a disk image file or a whole disk read-only; one JSON line per event on stdout, progress on stderr.
    Scan(scan_cmd::ScanArgs),
    /// List the connected whole disks (one `disk` line each).
    Disks,
    /// Check a disk before a long read: permission, answers, unmount, a copy's destination, a session (exit 2 on issues).
    Preflight {
        /// The whole disk: "disk4" or "PhysicalDrive2".
        disk: String,
        /// The image file a copy would write (its folder is checked; its map is `<IMAGE>.rvmap`).
        #[arg(long, value_name = "IMAGE")]
        copy_to: Option<PathBuf>,
        /// The folder of a saved session to resume.
        #[arg(long, value_name = "DIR")]
        session: Option<PathBuf>,
        /// Do not unmount the disk's volumes.
        #[arg(long)]
        no_unmount: bool,
    },
    /// Unmount a disk's volumes without ejecting it (never remounted automatically).
    Unmount {
        /// The whole disk: "disk4" or "PhysicalDrive2".
        disk: String,
    },
    /// Mount a disk's volumes again.
    Remount {
        /// The whole disk: "disk4" or "PhysicalDrive2".
        disk: String,
    },
    /// Copy a disk (or an image file) into an image file with its rescue map; resumes when the map exists.
    Copy(copy_cmd::CopyArgs),
    /// The FNV-1a 64 of a whole disk or file (unreadable sectors hashed as zeros and counted).
    Digest {
        /// A whole disk ("disk4", "PhysicalDrive2") or a file.
        source: PathBuf,
    },
    /// Measure scan throughput on synthetic images written to a temp dir (page-cache speed, not a card reader's).
    Bench {
        /// Size of each image in MiB.
        #[arg(long, value_name = "N", default_value_t = 1024, value_parser = clap::value_parser!(u64).range(1..=1_048_576))]
        size_mib: u64,
        /// Scans per image; the best run is reported.
        #[arg(long, value_name = "N", default_value_t = 3, value_parser = clap::value_parser!(u32).range(1..))]
        runs: u32,
        /// Images to measure: noise, dense-jpeg, dense-raw, video, cut-jpeg, exfat-card.
        #[arg(
            long,
            value_delimiter = ',',
            value_parser = parse_bench_image,
            default_values = ["noise", "dense-jpeg", "dense-raw", "video", "cut-jpeg", "exfat-card"]
        )]
        images: Vec<BenchImage>,
        /// Folder for the temporary images (default: the system temp dir).
        #[arg(long, value_name = "DIR")]
        dir: Option<PathBuf>,
        /// Threads measuring candidate files (0 = one per core, at most 16).
        #[arg(long, value_name = "N", default_value_t = 0)]
        threads: usize,
        /// Also scan each image through a saved session (one more line per image, `"session":true`).
        #[arg(long)]
        session: bool,
    },
}

#[derive(Clone, Copy, ValueEnum)]
pub(crate) enum GroupBy {
    Session,
    Day,
    Camera,
    Folder,
}

fn parse_bench_image(s: &str) -> Result<BenchImage, String> {
    BenchImage::parse(s).ok_or_else(|| format!("unknown image '{s}'"))
}

/// How a command that did not fail ended: exit 0, 2 (the pre-flight check found issues) or 3 (stopped by
/// `--stop-after-events`). Errors exit 1.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Exit {
    Done,
    Issues,
    Stopped,
}

fn run(ops: &dyn DeviceOps, command: Command) -> Result<Exit, revenant_core::EngineError> {
    match command {
        Command::Bench {
            size_mib,
            runs,
            images,
            dir,
            threads,
            session,
        } => bench::run_bench(size_mib, runs, &images, dir.as_deref(), threads, session)
            .map(|()| Exit::Done),
        Command::Scan(args) => scan_cmd::run_scan(ops, &args),
        Command::Disks => devices::run_disks(ops),
        Command::Preflight {
            disk,
            copy_to,
            session,
            no_unmount,
        } => devices::run_preflight(
            ops,
            &disk,
            copy_to.as_deref(),
            session.as_deref(),
            no_unmount,
        ),
        Command::Unmount { disk } => devices::run_unmount(ops, &disk),
        Command::Remount { disk } => devices::run_remount(ops, &disk),
        Command::Copy(args) => copy_cmd::run_copy(ops, &args),
        Command::Digest { source } => digest::run_digest(ops, &source),
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&SystemDevices, cli.command) {
        Ok(Exit::Done) => ExitCode::SUCCESS,
        Ok(Exit::Issues) => ExitCode::from(2),
        Ok(Exit::Stopped) => ExitCode::from(3),
        Err(e) => {
            eprintln!("error: {}", e.code());
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    use revenant_core::device::{DestInfo, DiskInfo, UnmountGuard};
    use revenant_core::formats::{FileKind, Health};
    use revenant_core::source::{DiskId, FileSource, Geometry, RawDevice, Source, VolumeId};
    use revenant_core::{Destination, EngineError, Finding, Fragment, Inspection, Preview};

    use super::*;
    use crate::copy_cmd::{CopyArgs, run_copy};
    use crate::output::{Outputs, found_line};
    use crate::scan_cmd::{ScanArgs, Targets, UnknownVolume, run_scan, write_preview};

    const CARD: u64 = 16 << 20;

    fn disk4() -> DiskId {
        DiskId::parse("disk4").unwrap()
    }

    /// An in-memory whole disk (512-byte sectors); after `gone_after` reads, it is gone (`device_gone`).
    struct MemDisk(Vec<u8>, Option<u64>, std::sync::atomic::AtomicU64);

    impl RawDevice for MemDisk {
        fn geometry(&self) -> Geometry {
            Geometry {
                size: self.0.len() as u64,
                logical_sector: 512,
                physical_sector: 512,
            }
        }
        fn read_aligned(&self, offset: u64, buf: &mut [u8]) -> Result<usize, EngineError> {
            let reads = self.2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.1.is_some_and(|n| reads >= n) {
                return Err(EngineError::DeviceGone { offset });
            }
            let start = (offset as usize).min(self.0.len());
            let n = buf.len().min(self.0.len() - start);
            buf[..n].copy_from_slice(&self.0[start..start + n]);
            Ok(n)
        }
    }

    /// A machine with one card, `disk4`, and folders that live on it (under `card_root`) or on `disk2` (anything
    /// else). Logs every call.
    struct StubDevices {
        card_root: PathBuf,
        free_bytes: u64,
        max_file_bytes: Option<u64>,
        /// The first device opened is gone after this many reads (a pulled cable); later opens are fine.
        gone_after: Option<u64>,
        opened: std::sync::atomic::AtomicU64,
        log: Mutex<Vec<String>>,
    }

    impl StubDevices {
        fn new(card_root: &Path) -> StubDevices {
            StubDevices {
                card_root: std::fs::canonicalize(card_root).unwrap(),
                free_bytes: 1 << 40,
                max_file_bytes: None,
                gone_after: None,
                opened: std::sync::atomic::AtomicU64::new(0),
                log: Mutex::new(Vec::new()),
            }
        }
        fn log(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }
        fn note(&self, s: String) {
            self.log.lock().unwrap().push(s);
        }
    }

    impl DeviceOps for StubDevices {
        fn list(&self) -> Result<Vec<DiskInfo>, EngineError> {
            self.note("list".into());
            Ok(vec![DiskInfo {
                id: disk4(),
                geometry: mem_disk(None).geometry(),
                vendor: None,
                model: Some("SDXC".into()),
                serial: None,
                removable: true,
                internal: false,
                system: false,
                virtual_disk: false,
                solid_state: None,
                volumes: Vec::new(),
            }])
        }
        fn open(&self, disk: &DiskId) -> Result<Box<dyn RawDevice>, EngineError> {
            self.note(format!("open {}", disk.as_str()));
            if *disk != disk4() {
                return Err(EngineError::DeviceNotFound);
            }
            let first = self
                .opened
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                == 0;
            Ok(Box::new(mem_disk(self.gone_after.filter(|_| first))))
        }
        fn destination(&self, dir: &Path) -> Result<DestInfo, EngineError> {
            self.note(format!("destination {}", dir.display()));
            let on_card = std::fs::canonicalize(dir)
                .map(|d| d.starts_with(&self.card_root))
                .unwrap_or(false);
            Ok(DestInfo {
                disks: vec![DiskId::parse(if on_card { "disk4" } else { "disk2" }).unwrap()],
                fs: Some("exfat".into()),
                free_bytes: self.free_bytes,
                max_file_bytes: self.max_file_bytes,
            })
        }
        fn unmount(&self, disk: &DiskId) -> Result<UnmountGuard, EngineError> {
            self.note(format!("unmount {}", disk.as_str()));
            Ok(UnmountGuard::empty())
        }
        fn remount(&self, disk: &DiskId, _: Option<UnmountGuard>) -> Result<(), EngineError> {
            self.note(format!("remount {}", disk.as_str()));
            Ok(())
        }
    }

    fn mem_disk(gone_after: Option<u64>) -> MemDisk {
        MemDisk(vec![0x5A; CARD as usize], gone_after, Default::default())
    }

    /// A source reading the whole disk `disk4` (its volume unknown, as a device's).
    struct StubCard;

    impl Source for StubCard {
        fn len(&self) -> u64 {
            CARD
        }
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, EngineError> {
            let n = buf.len().min(CARD.saturating_sub(offset) as usize);
            buf[..n].fill(0);
            Ok(n)
        }
        fn volume(&self) -> Option<VolumeId> {
            None
        }
        fn disk(&self) -> Option<DiskId> {
            Some(disk4())
        }
    }

    fn entries(dir: &Path) -> usize {
        std::fs::read_dir(dir).unwrap().count()
    }

    #[test]
    fn device_source_refuses_allow_same_volume() {
        let card = tempfile::tempdir().unwrap();
        let ops = StubDevices::new(card.path());
        let args = ScanArgs {
            source: "disk4".into(),
            allow_same_volume: true,
            ..ScanArgs::default()
        };
        assert_eq!(
            run_scan(&ops, &args).unwrap_err().code(),
            "destination_on_source"
        );
        let copy = CopyArgs {
            source: "disk4".into(),
            image: card.path().join("x.img"),
            allow_same_volume: true,
            ..CopyArgs::default()
        };
        assert_eq!(
            run_copy(&ops, &copy).unwrap_err().code(),
            "destination_on_source"
        );
        // Refused before the disk is even looked at.
        assert!(ops.log().is_empty(), "{:?}", ops.log());
        assert_eq!(entries(card.path()), 0);
    }

    #[test]
    fn extract_previews_and_session_are_checked_against_the_device() {
        let card = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let ops = StubDevices::new(card.path());
        let on_card = card.path().join("out");
        std::fs::create_dir(&on_card).unwrap();
        let absent_on_card = card.path().join("session");
        for (extract, previews, session) in [
            (Some(on_card.as_path()), None, None),
            (None, Some(on_card.as_path()), None),
            (None, None, Some(on_card.as_path())),
            (None, None, Some(absent_on_card.as_path())),
        ] {
            let err = Targets::check(&ops, &StubCard, extract, previews, session)
                .err()
                .expect("refused");
            assert_eq!(err.code(), "destination_on_source");
        }
        assert!(
            !absent_on_card.exists(),
            "a refused session folder is never created"
        );
        assert_eq!(entries(&on_card), 0);
        assert_eq!(entries(card.path()), 1);

        // The same through the whole command: refused before the card is unmounted or read.
        let args = ScanArgs {
            source: "disk4".into(),
            previews: Some(on_card.clone()),
            ..ScanArgs::default()
        };
        assert_eq!(
            run_scan(&ops, &args).unwrap_err().code(),
            "destination_on_source"
        );
        assert!(
            !ops.log().iter().any(|l| l.starts_with("unmount")),
            "{:?}",
            ops.log()
        );

        // Folders on another disk pass, each checked once; the session folder is created.
        let elsewhere = other.path().join("out");
        std::fs::create_dir(&elsewhere).unwrap();
        let session = other.path().join("session");
        let before = ops.log().len();
        let targets = Targets::check(
            &ops,
            &StubCard,
            Some(&elsewhere),
            Some(&elsewhere),
            Some(&session),
        )
        .unwrap();
        assert!(
            targets.extract.is_some() && targets.previews.is_some() && targets.session.is_some()
        );
        assert!(session.is_dir());
        let asked = ops.log()[before..]
            .iter()
            .filter(|l| l.starts_with("destination"))
            .count();
        // extract, previews, the session's parent before creating it, the session folder.
        assert_eq!(asked, 4);
    }

    #[test]
    fn device_session_resumes_with_the_callers_devices() {
        // A device's session stopped, then resumed: its folder is checked with the caller's ops (the stub), never with
        // the running system.
        let card = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let ops = StubDevices::new(card.path());
        let session = other.path().join("session");
        let args = |stop| ScanArgs {
            source: "disk4".into(),
            session: Some(session.clone()),
            stop_after_events: stop,
            checkpoint_mib: 1,
            ..ScanArgs::default()
        };
        assert_eq!(run_scan(&ops, &args(Some(1))).unwrap(), Exit::Stopped);
        assert!(session.join("session.rvs").exists());
        assert_eq!(run_scan(&ops, &args(None)).unwrap(), Exit::Done);
        assert_eq!(run_scan(&ops, &args(None)).unwrap(), Exit::Done);
        let log = ops.log();
        assert_eq!(
            log.iter().filter(|l| l.starts_with("unmount")).count(),
            3,
            "{log:?}"
        );
        assert!(!log.iter().any(|l| l.starts_with("remount")), "{log:?}");
    }

    #[test]
    fn a_pulled_card_is_waited_for_and_the_session_goes_on() {
        let card = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let mut ops = StubDevices::new(card.path());
        // Past the identity reads, inside the scan.
        ops.gone_after = Some(6);
        let args = ScanArgs {
            source: "disk4".into(),
            session: Some(other.path().join("session")),
            ..ScanArgs::default()
        };
        assert_eq!(run_scan(&ops, &args).unwrap(), Exit::Done);
        let log = ops.log();
        // The first open, then the reconnect's probe and its open after the unmount.
        assert!(
            log.iter().filter(|l| l.starts_with("open")).count() >= 3,
            "{log:?}"
        );
        assert_eq!(
            log.iter().filter(|l| l.starts_with("unmount")).count(),
            2,
            "{log:?}"
        );
        // Without a session, the scan stops with the error.
        let ops = StubDevices {
            gone_after: Some(6),
            ..StubDevices::new(card.path())
        };
        let args = ScanArgs {
            source: "disk4".into(),
            ..ScanArgs::default()
        };
        assert_eq!(run_scan(&ops, &args).unwrap_err().code(), "device_gone");
    }

    #[test]
    fn source_names_are_classified_on_every_platform() {
        use crate::devices::{Named, classify};
        let code = |s: &str| classify(s).err().map(|e| e.code());
        // Whole disks.
        for (s, id) in [
            ("disk4", "disk4"),
            ("/dev/rdisk4", "disk4"),
            (r"\\.\PhysicalDrive2", "PhysicalDrive2"),
            ("PhysicalDrive2", "PhysicalDrive2"),
        ] {
            assert!(
                matches!(classify(s), Ok(Named::Disk(d)) if d.as_str() == id),
                "{s}"
            );
        }
        // Files, including a Windows long path to a file (what canonicalize gives there) and a UNC long path.
        for s in [
            "card.img",
            "./disk4",
            "/Volumes/Backup/card.img",
            r"C:\x\card.img",
            r"\\?\C:\x\card.img",
            r"\\?\c:\x\card.img",
            r"\\?\UNC\server\share\card.img",
            r"\\server\share\card.img",
        ] {
            assert!(matches!(classify(s), Ok(Named::File)), "{s}");
        }
        // Any other device path: refused whether or not it exists.
        for s in [
            "/dev/disk4s1",
            "/dev/null",
            "/DEV/disk4s1",
            r"\\.\E:",
            r"\\.\HarddiskVolume3",
            "//./E:",
            r"\\?\Volume{0b1c2d3e-0000-0000-0000-000000000000}\",
            r"\\?\GLOBALROOT\Device\Harddisk1\Partition1",
            r"\\?\PhysicalDrive2",
            r"\??\E:",
        ] {
            assert_eq!(code(s), Some("not_whole_disk"), "{s}");
        }
    }

    /// A bare disk name while a file of that name sits in the folder is ambiguous (`open_failed`); the device path
    /// still names the disk and a path with a folder the file.
    #[test]
    fn a_bare_disk_name_shadowed_by_a_file_is_refused() {
        use crate::devices::device_name_in;
        let here = tempfile::tempdir().unwrap();
        let name = |s: &str| {
            device_name_in(Path::new(s), here.path()).map(|d| d.map(|d| d.as_str().to_string()))
        };
        assert_eq!(name("disk4").unwrap().as_deref(), Some("disk4"));
        assert_eq!(
            name("PhysicalDrive2").unwrap().as_deref(),
            Some("PhysicalDrive2")
        );
        std::fs::write(here.path().join("disk4"), b"x").unwrap();
        std::fs::create_dir(here.path().join("PhysicalDrive2")).unwrap();
        for s in ["disk4", "PhysicalDrive2"] {
            assert_eq!(name(s).unwrap_err().code(), "open_failed", "{s}");
        }
        assert_eq!(name("/dev/rdisk4").unwrap().as_deref(), Some("disk4"));
        assert_eq!(
            name(r"\\.\PhysicalDrive2").unwrap().as_deref(),
            Some("PhysicalDrive2")
        );
        assert_eq!(name("./disk4").unwrap(), None);
    }

    #[test]
    fn unknown_volume_forwards_the_disk() {
        let wrapped = UnknownVolume(&StubCard);
        assert_eq!(wrapped.disk(), Some(disk4()));
        assert_eq!(wrapped.volume(), None);
        assert_eq!(wrapped.len(), CARD);
        // So a wrapped device is still refused on its own disk.
        let card = tempfile::tempdir().unwrap();
        let ops = StubDevices::new(card.path());
        assert_eq!(
            Destination::check(&ops, &wrapped, card.path())
                .unwrap_err()
                .code(),
            "destination_on_source"
        );
    }

    #[test]
    fn copy_refuses_a_destination_that_cannot_hold_the_device() {
        let card = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        let args = CopyArgs {
            source: "disk4".into(),
            image: dest.path().join("card.img"),
            ..CopyArgs::default()
        };
        let mut small = StubDevices::new(card.path());
        small.free_bytes = CARD - 1;
        let mut fat = StubDevices::new(card.path());
        fat.max_file_bytes = Some(CARD - 1);
        for ops in [&small, &fat] {
            assert_eq!(run_copy(ops, &args).unwrap(), Exit::Issues);
            assert_eq!(entries(dest.path()), 0, "nothing created");
        }
        // An image folder on the card: refused before the card is unmounted or read, nothing created.
        let on_card = CopyArgs {
            source: "disk4".into(),
            image: card.path().join("card.img"),
            ..CopyArgs::default()
        };
        let ops = StubDevices::new(card.path());
        assert_eq!(
            run_copy(&ops, &on_card).unwrap_err().code(),
            "destination_on_source"
        );
        assert!(
            !ops.log()
                .iter()
                .any(|l| l.starts_with("unmount") || l.starts_with("open")),
            "{:?}",
            ops.log()
        );
        assert_eq!(entries(card.path()), 0);
        // With room, the copy runs and the image is the card.
        let roomy = StubDevices::new(card.path());
        assert_eq!(run_copy(&roomy, &args).unwrap(), Exit::Done);
        assert_eq!(
            std::fs::read(dest.path().join("card.img")).unwrap(),
            vec![0x5A; CARD as usize]
        );
        assert!(dest.path().join("card.img.rvmap").exists());
    }

    fn json(f: &Finding) -> serde_json::Value {
        let line = found_line(f, Inspection::default(), Outputs::default());
        serde_json::to_value(&line).unwrap()
    }

    #[test]
    fn fragments_key_only_for_pieces() {
        let contiguous = Finding {
            id: 1,
            start: 8192,
            len: 2500,
            kind: FileKind::Jpeg,
            health: Health::Intact,
            fragments: Vec::new(),
            origin: None,
        };
        let v = json(&contiguous);
        assert_eq!(v["event"], "found");
        assert!(v.get("fragments").is_none(), "{v}");

        let pieces = Finding {
            fragments: vec![
                Fragment {
                    start: 8192,
                    len: 1000,
                },
                Fragment {
                    start: 4096,
                    len: 1500,
                },
            ],
            ..contiguous.clone()
        };
        let v = json(&pieces);
        assert_eq!(
            v["fragments"],
            serde_json::json!([{"start": 8192, "len": 1000}, {"start": 4096, "len": 1500}])
        );

        // The preview follows the same rule.
        let preview = |fragments: Vec<Fragment>| {
            let details = Inspection {
                preview: Some(Preview {
                    start: 8192,
                    len: 2500,
                    width: 64,
                    height: 48,
                    fragments,
                }),
                ..Inspection::default()
            };
            serde_json::to_value(found_line(&contiguous, details, Outputs::default())).unwrap()
        };
        assert!(preview(Vec::new())["preview"].get("fragments").is_none());
        assert_eq!(
            preview(pieces.fragments.clone())["preview"]["fragments"]
                .as_array()
                .map(Vec::len),
            Some(2)
        );
    }

    #[test]
    fn previews_write_every_piece_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("image.bin");
        let bytes: Vec<u8> = (0..8192u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&image, &bytes).unwrap();
        let source = FileSource::open(&image).unwrap();
        let f = Finding {
            id: 1,
            start: 4096,
            len: 3000,
            kind: FileKind::Jpeg,
            health: Health::Intact,
            fragments: Vec::new(),
            origin: None,
        };
        let p = Preview {
            start: 4096,
            len: 1500,
            width: 64,
            height: 48,
            fragments: vec![
                Fragment {
                    start: 4096,
                    len: 1000,
                },
                Fragment {
                    start: 512,
                    len: 500,
                },
            ],
        };
        let out = tempfile::tempdir().unwrap();
        // The image and the folder share the temp volume: checked as a source whose volume is unknown.
        let dest = Destination::check(&SystemDevices, &UnknownVolume(&source), out.path()).unwrap();
        let path = write_preview(&source, &f, &p, &dest, "revenant_000000001000.jpg").unwrap();
        let mut expected = bytes[4096..5096].to_vec();
        expected.extend_from_slice(&bytes[512..1012]);
        assert_eq!(std::fs::read(path).unwrap(), expected);
    }

    /// A preview read coming back short means the source vanished: `device_gone` (so a session run waits for the card to
    /// come back), and no preview file is left.
    #[test]
    fn a_short_preview_read_is_device_gone() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("image.bin");
        std::fs::write(&image, vec![1u8; 8192]).unwrap();
        let source = FileSource::open(&image).unwrap();
        let f = Finding {
            id: 1,
            start: 4096,
            len: 3000,
            kind: FileKind::Jpeg,
            health: Health::Intact,
            fragments: Vec::new(),
            origin: None,
        };
        let p = Preview {
            start: 8000,
            len: 500,
            width: 64,
            height: 48,
            fragments: Vec::new(),
        };
        let out = tempfile::tempdir().unwrap();
        let dest = Destination::check(&SystemDevices, &UnknownVolume(&source), out.path()).unwrap();
        let e = write_preview(&source, &f, &p, &dest, "a.jpg").unwrap_err();
        assert!(
            matches!(e, EngineError::DeviceGone { offset: 8192 }),
            "{e:?}"
        );
        // Which stops the scan (as an extraction's would), so a session run reconnects instead of going on.
        assert!(crate::scan_cmd::stops_the_scan(&e));
        assert!(!crate::scan_cmd::stops_the_scan(&EngineError::Write {
            cause: std::io::ErrorKind::Other.into()
        }));
        assert_eq!(std::fs::read_dir(out.path()).unwrap().count(), 0);
    }
}
