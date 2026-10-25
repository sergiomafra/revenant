//! Reading a whole device: any offset and length are aligned inside, 4Kn devices scan like their image, a bad physical
//! sector is read once, a vanished device is `device_gone`, and disk ids parse.

mod support;

use std::path::PathBuf;

use revenant_core::formats::{Fragment, all_formats};
use revenant_core::source::{
    DeviceSource, DiskId, FileSource, Fragmented, Geometry, RawDevice, Source,
};
use revenant_core::{CancelToken, EngineError, Event, FsState, ScanOptions, scan};
use revenant_testkit as testkit;
use support::MemDevice;
use testkit::DiskImage;
use testkit::fs::{DeleteStyle, FsBuilder, FsType, Partitioning, disk};

const MIB: u64 = 1 << 20;

fn noise(len: u64, seed: u64) -> Vec<u8> {
    let file = DiskImage::noise(len as usize, seed).save();
    std::fs::read(file.path()).unwrap()
}

fn id(s: &str) -> DiskId {
    DiskId::parse(s).unwrap()
}

fn device(dev: MemDevice) -> DeviceSource {
    DeviceSource::new(Box::new(dev), id("disk4"))
}

fn events(source: &dyn Source, sector: u64, threads: usize) -> Vec<Event> {
    let opts = ScanOptions {
        sector,
        threads,
        ..ScanOptions::default()
    };
    let mut events = Vec::new();
    scan(
        source,
        &all_formats(),
        &opts,
        &CancelToken::default(),
        &mut |e| events.push(e),
    )
    .unwrap();
    events
}

#[test]
fn unaligned_reads_are_aligned_inside() {
    let data = noise(MIB, 3);
    let len = data.len() as u64;
    let source = device(MemDevice::new(data.clone(), 4096));
    assert_eq!(source.len(), len);
    for (offset, want) in [
        (0, 1),
        (1, 4095),
        (4095, 2),
        (4096, 4096),
        (10_000, 70_000),
        (len - 3, 10),
        (len, 5),
        (len + 4096, 4096),
        (0, len as usize + 100),
    ] {
        let mut buf = vec![0xAAu8; want];
        let n = source.read_at(offset, &mut buf).unwrap();
        let expect = (want as u64).min(len.saturating_sub(offset)) as usize;
        assert_eq!(n, expect, "{offset} {want}");
        let at = offset.min(len) as usize;
        assert!(buf[..n] == data[at..at + n], "{offset} {want}");
    }
}

/// A 56 MiB card: a FAT32 volume of 48 MiB at 1 MiB (MBR), four photos and a deleted one, each starting on a 4096-byte
/// boundary as on a 4Kn card. (Its clusters are 512 bytes: 4 KiB clusters would leave too few for FAT32 at this size.)
fn fat32_card() -> Vec<u8> {
    let mut b = FsBuilder::new(FsType::Fat32, 48 * MIB, 512);
    for seed in 1..=5u64 {
        let jpeg = testkit::jpeg(640, 480, seed, 0);
        let first = (2..)
            .map(|c| c + seed as u32 * 8000)
            .find(|&c| b.cluster_offset(c).is_multiple_of(4096))
            .unwrap();
        let clusters: Vec<u32> = (first..first + jpeg.len().div_ceil(512) as u32).collect();
        let path = format!("DCIM/100CANON/IMG_000{seed}.JPG");
        b.add_at(&path, &jpeg, &clusters);
        if seed == 5 {
            b.delete(&path, DeleteStyle::Camera);
        }
    }
    disk(
        Partitioning::Mbr { first_lba: 2048 },
        &b.build(),
        56 * MIB as usize,
    )
}

#[test]
fn four_kn_device_scans_like_its_image() {
    let card = fat32_card();
    let file = DiskImage::zeros(0).save();
    std::fs::write(file.path(), &card).unwrap();
    let image = FileSource::open(file.path()).unwrap();
    let dev = device(MemDevice::new(card, 4096));
    assert_eq!(dev.scan_sector(), 4096);
    for threads in [1, 8] {
        let want = events(&image, 4096, threads);
        assert!(
            want.iter().filter(|e| matches!(e, Event::Found(_))).count() == 5,
            "{want:?}"
        );
        assert!(want.iter().any(|e| matches!(e, Event::Volume { .. })));
        assert!(want.iter().any(|e| matches!(
            e,
            Event::Found(f) if f.origin.as_ref().is_some_and(|o| o.state == FsState::Deleted)
        )));
        let got = events(&dev, u64::from(dev.scan_sector()), threads);
        assert_eq!(got, want, "threads {threads}");
    }
}

#[test]
fn bad_physical_sector_fails_only_its_own_bytes() {
    let data = noise(MIB, 9);
    // A 4Kn device, and a 512e one (logical 512, physical 4096): both scan in 4096-byte sectors.
    for (logical, physical) in [(4096, 4096), (512, 4096)] {
        let mem = || {
            MemDevice::new(data.clone(), logical)
                .physical(physical)
                .bad(8192, 4096)
        };
        let source = device(mem());
        assert_eq!(source.scan_sector(), 4096);
        let mut buf = [0u8; 512];
        let err = source.read_at(8192 + 512, &mut buf).unwrap_err();
        assert_eq!(err.code(), "read_failed");
        match err {
            EngineError::Read { offset, .. } => assert!((8192..8192 + 1024).contains(&offset)),
            e => panic!("{e:?}"),
        }
        let mut buf = [0u8; 4096];
        assert_eq!(source.read_at(4096, &mut buf).unwrap(), 4096);
        assert!(buf[..] == data[4096..8192]);

        let raw = std::sync::Arc::new(mem());
        let source = DeviceSource::new(Box::new(Shared(raw.clone())), id("disk4"));
        let got = events(&source, u64::from(source.scan_sector()), 1);
        let unreadable: Vec<_> = got
            .iter()
            .filter(|e| matches!(e, Event::Unreadable { .. }))
            .collect();
        assert_eq!(
            unreadable,
            [&Event::Unreadable {
                start: 8192,
                len: 4096
            }],
            "{logical}"
        );
        // Besides the window load that failed, the bad sector is read exactly once (P8: never eight 512-byte reads).
        let touching: Vec<_> = raw
            .reads()
            .into_iter()
            .filter(|&(o, l)| o < 8192 + 4096 && o + l as u64 > 8192)
            .collect();
        let small: Vec<_> = touching.iter().filter(|&&(_, l)| l <= 4096).collect();
        assert_eq!(small, [&(8192, 4096)], "{logical}: {touching:?}");
        assert_eq!(touching.len(), 2, "{logical}: {touching:?}");
    }
}

/// Shares a [`MemDevice`] with the test, which then reads its log.
struct Shared(std::sync::Arc<MemDevice>);

impl RawDevice for Shared {
    fn geometry(&self) -> Geometry {
        self.0.geometry()
    }
    fn read_aligned(&self, offset: u64, buf: &mut [u8]) -> Result<usize, EngineError> {
        self.0.read_aligned(offset, buf)
    }
}

#[test]
fn gone_device_reports_device_gone() {
    let source = device(MemDevice::new(noise(MIB, 4), 4096).gone_after(3));
    let mut buf = [0u8; 100];
    for i in 0..3u64 {
        source.read_at(i * 4096 + 7, &mut buf).unwrap();
    }
    let err = source.read_at(5 * 4096 + 7, &mut buf).unwrap_err();
    assert_eq!(err.code(), "device_gone");
    // The device reports the aligned read's offset (5 * 4096); the source never points before what was asked.
    assert!(
        matches!(err, EngineError::DeviceGone { offset } if offset == 5 * 4096 + 7),
        "{err:?}"
    );
}

#[test]
fn device_source_reports_its_disk() {
    let source = DeviceSource::new(
        Box::new(MemDevice::new(noise(MIB, 5), 512).physical(4096)),
        id("/dev/rdisk7"),
    );
    assert_eq!(source.disk(), Some(id("disk7")));
    assert_eq!(source.disk_id().as_str(), "disk7");
    assert_eq!(source.volume(), None);
    assert_eq!(source.geometry().physical_sector, 4096);
    assert_eq!(source.geometry().logical_sector, 512);
    assert_eq!(source.geometry().size, MIB);
    let view = Fragmented::new(&source, vec![Fragment { start: 0, len: 10 }]);
    assert_eq!(view.disk(), Some(id("disk7")));
}

#[test]
fn disk_ids_parse() {
    for s in ["disk4", "/dev/disk4", "/dev/rdisk4"] {
        let d = id(s);
        assert_eq!(d.as_str(), "disk4", "{s}");
        assert_eq!(d.device_path(), PathBuf::from("/dev/rdisk4"));
    }
    for s in ["PhysicalDrive2", r"\\.\PhysicalDrive2"] {
        let d = id(s);
        assert_eq!(d.as_str(), "PhysicalDrive2", "{s}");
        assert_eq!(d.device_path(), PathBuf::from(r"\\.\PhysicalDrive2"));
    }
    assert_eq!(id("disk10").as_str(), "disk10");
    for s in [
        "disk4s1",
        "",
        "/dev/sda",
        "disk",
        r"\\.\C:",
        "/dev/rdisk4s2",
        "disk04",
        "rdisk4",
        "PhysicalDrive",
        "PhysicalDrive2x",
        "disk-1",
        "disk99999999999999999999",
    ] {
        let err = DiskId::parse(s).unwrap_err();
        assert_eq!(err.code(), "not_whole_disk", "{s:?}");
    }
}
