//! A destination folder is checked once against the source: never on the source's volume, never on any volume of the
//! source's disk (an APFS container counts as its physical disks), and refused for a device source when its disks cannot
//! be told. Also the fake device system later tests build on.

mod support;

use std::path::Path;

use revenant_core::device::{DestInfo, DeviceOps};
use revenant_core::formats::{FileKind, Health};
use revenant_core::source::{DeviceSource, DiskId, FileSource, Source};
use revenant_core::{Destination, EngineError, Finding, extract, extract_to};
use revenant_testkit as testkit;
use support::{FakeDevices, SimSource, disk_info};
use testkit::DiskImage;

const MIB: u64 = 1 << 20;

fn id(s: &str) -> DiskId {
    DiskId::parse(s).unwrap()
}

fn dest(disks: &[&str]) -> DestInfo {
    DestInfo {
        disks: disks.iter().map(|d| id(d)).collect(),
        fs: Some("exfat".into()),
        free_bytes: 1 << 30,
        max_file_bytes: None,
    }
}

fn check_code(ops: &FakeDevices, source: &dyn Source, dir: &Path) -> &'static str {
    Destination::check(ops, source, dir).unwrap_err().code()
}

#[test]
fn destination_on_the_source_disk_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let fake = FakeDevices::new();
    let source = SimSource::zeros(MIB).disk("disk4");
    fake.set_destination(dir.path(), Ok(dest(&["disk4"])));
    assert_eq!(
        check_code(&fake, &source, dir.path()),
        "destination_on_source"
    );
    // An APFS volume whose container's physical store is the source disk.
    fake.set_destination(dir.path(), Ok(dest(&["disk5", "disk4"])));
    assert_eq!(
        check_code(&fake, &source, dir.path()),
        "destination_on_source"
    );
    // Another disk: accepted, with what it lives on.
    fake.set_destination(dir.path(), Ok(dest(&["disk2"])));
    let ok = Destination::check(&fake, &source, dir.path()).unwrap();
    assert_eq!(ok.dir(), dir.path());
    assert_eq!(ok.info(), Some(&dest(&["disk2"])));
    assert!(
        fake.log()
            .contains(&format!("destination {}", dir.path().display()))
    );

    // The same through a device opened from the fake system: DeviceSource names its disk.
    fake.add(disk_info("disk4", 0), vec![0u8; MIB as usize]);
    let device = DeviceSource::new(fake.open(&id("disk4")).unwrap(), id("disk4"));
    fake.set_destination(dir.path(), Ok(dest(&["disk4"])));
    assert_eq!(
        check_code(&fake, &device, dir.path()),
        "destination_on_source"
    );
}

#[test]
fn unknown_destination_with_a_device_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let fake = FakeDevices::new();
    let source = SimSource::zeros(MIB).disk("disk4");
    fake.set_destination(dir.path(), Err("system_tool_failed"));
    assert_eq!(
        check_code(&fake, &source, dir.path()),
        "destination_unknown"
    );
    fake.set_destination(dir.path(), Err("unsupported_platform"));
    assert_eq!(
        check_code(&fake, &source, dir.path()),
        "destination_unknown"
    );
    // Answered, but with no disk.
    fake.set_destination(dir.path(), Ok(dest(&[])));
    assert_eq!(
        check_code(&fake, &source, dir.path()),
        "destination_unknown"
    );
    // A folder that does not exist cannot be checked at all.
    let missing = dir.path().join("missing");
    assert_eq!(check_code(&fake, &source, &missing), "open_failed");
}

#[test]
fn image_sources_keep_the_volume_rule() {
    let jpeg = testkit::jpeg(64, 64, 1, 0);
    let mut disk = DiskImage::noise(4 * MIB as usize, 3);
    disk.put(4096, &jpeg);
    let img = disk.save();
    // The image's bytes from a source whose volume is unknown (the temporary folders share the image's volume).
    let source = SimSource::new(std::fs::read(img.path()).unwrap());
    let finding = Finding {
        id: 1,
        start: 4096,
        len: jpeg.len() as u64,
        kind: FileKind::Jpeg,
        health: Health::Intact,
        fragments: Vec::new(),
        origin: None,
    };
    let fake = FakeDevices::new();
    let dir = tempfile::tempdir().unwrap();
    let checked = Destination::check(&fake, &source, dir.path()).unwrap();
    assert_eq!(checked.info(), None);
    assert!(
        !fake.log().iter().any(|l| l.starts_with("destination")),
        "{:?}",
        fake.log()
    );
    let to = extract_to(&source, &finding, &checked, "photo.jpg").unwrap();
    let other = tempfile::tempdir().unwrap();
    let before = extract(&source, &finding, other.path()).unwrap();
    assert_eq!(std::fs::read(&to).unwrap(), jpeg);
    assert_eq!(std::fs::read(&to).unwrap(), std::fs::read(before).unwrap());
    assert_eq!(to, dir.path().join("photo.jpg"));

    // A folder on the image file's own volume: refused, still without asking the system.
    let file = FileSource::open(img.path()).unwrap();
    let own = img.path().parent().unwrap();
    assert_eq!(check_code(&fake, &file, own), "destination_on_source");
    assert!(fake.log().is_empty(), "{:?}", fake.log());
}

#[test]
fn fake_devices_serve_virtual_disks() {
    let fake = FakeDevices::new();
    let eight_gib = 8u64 << 30;
    fake.add_virtual(disk_info("disk7", eight_gib), vec![0xAB; 4096]);
    fake.add(disk_info("disk3", 0), vec![1u8; 8192]);
    let listed = fake.list().unwrap();
    let sizes: Vec<(&str, u64)> = listed
        .iter()
        .map(|d| (d.id.as_str(), d.geometry.size))
        .collect();
    assert_eq!(sizes, [("disk3", 8192), ("disk7", eight_gib)]);
    let device = DeviceSource::new(fake.open(&id("disk7")).unwrap(), id("disk7"));
    assert_eq!(device.len(), eight_gib);
    let mut buf = [7u8; 10];
    assert_eq!(device.read_at(4090, &mut buf).unwrap(), 10);
    assert_eq!(buf, [0xAB, 0xAB, 0xAB, 0xAB, 0xAB, 0xAB, 0, 0, 0, 0]);
    assert_eq!(device.read_at(eight_gib - 4, &mut buf).unwrap(), 4);
    // Removed: gone from the list, and the open device reads DeviceGone.
    fake.remove("disk7");
    assert_eq!(fake.list().unwrap().len(), 1);
    assert!(matches!(
        device.read_at(0, &mut buf),
        Err(EngineError::DeviceGone { .. })
    ));
    assert_eq!(
        fake.open(&id("disk7")).err().map(|e| e.code()),
        Some("device_not_found")
    );
    let log = fake.log();
    assert_eq!(&log[..2], ["list", "open disk7"]);
    assert!(log.contains(&"read disk7 3584".to_string()), "{log:?}");
}
