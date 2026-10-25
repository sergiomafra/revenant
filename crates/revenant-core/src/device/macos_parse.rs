//! What macOS tools print about disks, parsed (pure; tested on every platform).
//!
//! - Geometry (ruling D5): `diskutil info -plist diskN` gives the size (`TotalSize`, else `Size`) and the logical sector
//!   (`DeviceBlockSize`); `ioreg -a -r -c IOBlockStorageDevice -l` gives the physical sector of the device whose whole-disk
//!   media is `diskN`, when it says.
//! - The disk list (D8): whole disks of `diskutil list -plist` without the synthesized APFS containers; the system disk
//!   is the one holding `/` (`ParentWholeDisk` and the disks of the `APFSPhysicalStores` of `diskutil info -plist /`).
//! - A destination (D12): `df -P -k <folder>` (the volume's node and free space) and `diskutil info -plist <node>`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use super::plist::Plist;
use super::{DestInfo, DiskInfo, FAT_MAX_FILE, VolumeInfo, push_unique, sort_disks};
use crate::source::{DiskId, Geometry};

/// Largest sector size believed (anything above, or not a power of two from 512, is a misread).
const MAX_SECTOR: i64 = 1 << 16;

/// The geometry of the whole disk `bsd` ("disk4") from its `diskutil info` plist and, when available, the `ioreg` plist
/// of the block storage devices. `None` when `info` is not about `bsd`, lacks the size or the sector size, or holds
/// implausible values. The physical sector is the device's `Physical Block Size` (in its `Device Characteristics`, or
/// among its own properties) when it is plausible, else the logical sector.
pub(crate) fn geometry(info: &Plist, ioreg: Option<&Plist>, bsd: &str) -> Option<Geometry> {
    if let Some(id) = info.get("DeviceIdentifier")
        && id.as_str() != Some(bsd)
    {
        return None;
    }
    let size = info
        .get("TotalSize")
        .or_else(|| info.get("Size"))
        .and_then(Plist::as_int)
        .filter(|&s| s > 0)?;
    let logical = sector(info.get("DeviceBlockSize")?)?;
    let physical = ioreg
        .and_then(|r| device_of(r, bsd))
        .and_then(|dev| {
            dev.get("Device Characteristics")
                .and_then(|c| c.get("Physical Block Size"))
                .or_else(|| dev.get("Physical Block Size"))
        })
        .and_then(sector)
        .filter(|&p| p >= logical)
        .unwrap_or(logical);
    Some(Geometry {
        size: size as u64,
        logical_sector: logical,
        physical_sector: physical,
    })
}

fn sector(v: &Plist) -> Option<u32> {
    v.as_int()
        .filter(|&s| (512..=MAX_SECTOR).contains(&s) && (s as u64).is_power_of_two())
        .map(|s| s as u32)
}

fn children(p: &Plist) -> &[Plist] {
    p.get("IORegistryEntryChildren")
        .and_then(Plist::as_array)
        .unwrap_or(&[])
}

/// The block storage device whose whole-disk media is `bsd`: in `ioreg -r -c IOBlockStorageDevice`, each device's
/// children are its drivers and their children its whole-disk media (partitions and volumes sit deeper, never matched).
fn device_of<'a>(ioreg: &'a Plist, bsd: &str) -> Option<&'a Plist> {
    let devices = match ioreg {
        Plist::Array(a) => a.as_slice(),
        d @ Plist::Dict(_) => std::slice::from_ref(d),
        _ => return None,
    };
    devices.iter().find(|dev| {
        children(dev).iter().any(|driver| {
            children(driver)
                .iter()
                .any(|media| media.get("BSD Name").and_then(Plist::as_str) == Some(bsd))
        })
    })
}

/// A non-empty, trimmed string value.
fn text(v: Option<&Plist>) -> Option<String> {
    let s = v?.as_str()?.trim();
    (!s.is_empty()).then(|| s.to_string())
}

fn flag(info: &Plist, key: &str) -> bool {
    info.get(key).and_then(Plist::as_bool).unwrap_or(false)
}

/// The whole disk of a BSD name: "disk0s2" → disk0, "/dev/disk3s1s1" → disk3, "disk4" → disk4.
pub(crate) fn whole_of(bsd: &str) -> Option<DiskId> {
    let name = bsd.strip_prefix("/dev/").unwrap_or(bsd);
    let digits = name.strip_prefix("disk")?;
    let n = digits.bytes().take_while(u8::is_ascii_digit).count();
    let rest = &digits[n..];
    if !(rest.is_empty() || rest.starts_with('s')) {
        return None;
    }
    DiskId::parse(&name[..4 + n]).ok()
}

/// Whether `s` is a BSD disk name diskutil printed ("disk4", "disk4s1", "disk3s1s1"): safe to pass back to it.
pub(crate) fn is_bsd_name(s: &str) -> bool {
    s.strip_prefix("disk").is_some_and(|rest| {
        !rest.is_empty()
            && rest
                .split('s')
                .all(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    })
}

/// The disks a volume's `diskutil info` names: its `ParentWholeDisk`, then the whole disk of each APFS physical store.
fn disks_of(info: &Plist) -> Vec<DiskId> {
    let mut out = Vec::new();
    if let Some(parent) = info.get("ParentWholeDisk").and_then(Plist::as_str)
        && let Some(id) = whole_of(parent)
    {
        push_unique(&mut out, id);
    }
    let stores = info
        .get("APFSPhysicalStores")
        .and_then(Plist::as_array)
        .unwrap_or(&[]);
    for store in stores {
        if let Some(id) = store
            .get("APFSPhysicalStore")
            .and_then(Plist::as_str)
            .and_then(whole_of)
        {
            push_unique(&mut out, id);
        }
    }
    out
}

/// The entries of `diskutil list -plist` that are whole disks a user can pick: those with an id, without
/// `APFSPhysicalStores` (a synthesized APFS container, shown through its physical disks).
pub(crate) fn whole_disks(list: &Plist) -> Vec<&Plist> {
    list.get("AllDisksAndPartitions")
        .and_then(Plist::as_array)
        .unwrap_or(&[])
        .iter()
        .filter(|e| e.get("APFSPhysicalStores").is_none())
        .filter(|e| {
            e.get("DeviceIdentifier")
                .and_then(Plist::as_str)
                .is_some_and(|id| whole_of(id).is_some_and(|w| w.as_str() == id))
        })
        .collect()
}

/// The partitions' ids of a `diskutil list` entry.
pub(crate) fn partition_ids(entry: &Plist) -> Vec<&str> {
    entry
        .get("Partitions")
        .and_then(Plist::as_array)
        .unwrap_or(&[])
        .iter()
        .filter_map(|p| p.get("DeviceIdentifier").and_then(Plist::as_str))
        .filter(|id| is_bsd_name(id))
        .collect()
}

/// The connected whole disks (ruling D8) from `diskutil list -plist`, the `diskutil info -plist` of each whole disk and
/// partition (`infos`, by id), `diskutil info -plist /` (`root`) and the `ioreg` plist. A whole disk without its info or
/// a plausible geometry is left out; a partition without info keeps what the list says. Sorted by id. `None` when `root`
/// names no disk: the system disk must never silently go unmarked.
pub(crate) fn disks(
    list: &Plist,
    infos: &BTreeMap<String, Plist>,
    root: &Plist,
    ioreg: Option<&Plist>,
) -> Option<Vec<DiskInfo>> {
    let system = disks_of(root);
    if system.is_empty() {
        return None;
    }
    let mut out = Vec::new();
    for entry in whole_disks(list) {
        let Some(bsd) = entry.get("DeviceIdentifier").and_then(Plist::as_str) else {
            continue;
        };
        let Some(info) = infos.get(bsd) else { continue };
        if info.get("APFSPhysicalStores").is_some() {
            continue;
        }
        let (Some(id), Some(geometry)) = (whole_of(bsd), geometry(info, ioreg, bsd)) else {
            continue;
        };
        let device = ioreg.and_then(|r| device_of(r, bsd));
        let characteristic = |key: &str| {
            device.and_then(|d| {
                text(d.get("Device Characteristics").and_then(|c| c.get(key)))
                    .or_else(|| text(d.get(key)))
            })
        };
        let volumes = entry
            .get("Partitions")
            .and_then(Plist::as_array)
            .unwrap_or(&[])
            .iter()
            .filter_map(|p| volume(p, infos))
            .collect();
        out.push(DiskInfo {
            system: system.contains(&id),
            id,
            geometry,
            vendor: characteristic("Vendor Name"),
            model: characteristic("Product Name").or_else(|| text(info.get("MediaName"))),
            serial: characteristic("Serial Number"),
            removable: flag(info, "Removable") || flag(info, "RemovableMedia"),
            internal: flag(info, "Internal"),
            virtual_disk: info.get("VirtualOrPhysical").and_then(Plist::as_str) == Some("Virtual")
                || info.get("BusProtocol").and_then(Plist::as_str) == Some("Disk Image"),
            solid_state: info.get("SolidState").and_then(Plist::as_bool),
            volumes,
        });
    }
    sort_disks(&mut out);
    Some(out)
}

/// A partition of a `diskutil list` entry, completed by its `diskutil info` when there is one.
fn volume(part: &Plist, infos: &BTreeMap<String, Plist>) -> Option<VolumeInfo> {
    let id = part.get("DeviceIdentifier").and_then(Plist::as_str)?;
    let info = infos.get(id);
    let either = |key: &str| text(info.and_then(|i| i.get(key))).or_else(|| text(part.get(key)));
    let size = part
        .get("Size")
        .or_else(|| info.and_then(|i| i.get("TotalSize").or_else(|| i.get("Size"))))
        .and_then(Plist::as_int)
        .filter(|&s| s >= 0)
        .unwrap_or(0);
    Some(VolumeInfo {
        id: id.to_string(),
        fs: text(info.and_then(|i| i.get("FilesystemType"))),
        label: either("VolumeName"),
        mount_point: either("MountPoint").map(PathBuf::from),
        size: size as u64,
    })
}

/// The volume's node and its available 1024-byte blocks, from `df -P -k` output (header line, then one line:
/// `Filesystem 1024-blocks Used Available Capacity Mounted-on`). The node may hold spaces (a network share), so the
/// numbers are found as three integers followed by a percentage.
pub(crate) fn df_line(df: &str) -> Option<(String, u64)> {
    let mut lines = df.lines();
    lines.next()?;
    let fields: Vec<&str> = lines.next()?.split_whitespace().collect();
    let numeric = |s: &str| s.parse::<u64>().ok();
    (1..fields.len().saturating_sub(4)).find_map(|i| {
        let ok = numeric(fields[i]).is_some()
            && numeric(fields[i + 1]).is_some()
            && fields[i + 3].ends_with('%');
        ok.then(|| Some((fields[..i].join(" "), numeric(fields[i + 2])?)))
            .flatten()
    })
}

/// Where a folder lives (ruling D12), from its `df -P -k` output and the `diskutil info -plist` of the node `df` names.
/// `None` when `info` is about another node or names no whole disk.
pub(crate) fn destination(df: &str, info: &Plist) -> Option<DestInfo> {
    let (node, available) = df_line(df)?;
    if info.get("DeviceNode").and_then(Plist::as_str) != Some(node.as_str()) {
        return None;
    }
    let disks = disks_of(info);
    if disks.is_empty() {
        return None;
    }
    let fs = text(info.get("FilesystemType"));
    let max_file_bytes = (fs.as_deref() == Some("msdos")).then_some(FAT_MAX_FILE);
    Some(DestInfo {
        disks,
        fs,
        free_bytes: available.checked_mul(1024)?,
        max_file_bytes,
    })
}

/// The `diskutil info` keys that tell one medium from another (none of them changes while a disk stays connected).
const MEDIA_KEYS: [&str; 13] = [
    "DeviceIdentifier",
    "TotalSize",
    "Size",
    "DeviceBlockSize",
    "MediaName",
    "IORegistryEntryName",
    "BusProtocol",
    "Content",
    "DiskUUID",
    "Internal",
    "Removable",
    "RemovableMedia",
    "VirtualOrPhysical",
];

/// Whether two `diskutil info` plists of a whole disk describe the same medium (the check after opening a device that
/// the disk was not swapped since its geometry was read).
pub(crate) fn same_media(a: &Plist, b: &Plist) -> bool {
    matches!((a, b), (Plist::Dict(_), Plist::Dict(_)))
        && MEDIA_KEYS.iter().all(|k| a.get(k) == b.get(k))
}

/// Whether `diskutil unmountDisk` left `disk` unmounted (ruling D10): `ok` is whether the tool succeeded, `after` a
/// listing taken afterwards. `Err` with the volumes of `disk` still mounted (empty when none is but the tool failed, or
/// the disk is no longer listed): an unmount counts only when the tool says so and the system agrees.
pub(crate) fn unmount_result(
    ok: bool,
    after: &[DiskInfo],
    disk: &DiskId,
) -> Result<(), Vec<String>> {
    let Some(info) = after.iter().find(|d| d.id == *disk) else {
        return Err(Vec::new());
    };
    let mounted: Vec<String> = info
        .volumes
        .iter()
        .filter(|v| v.mount_point.is_some())
        .map(|v| v.id.clone())
        .collect();
    if ok && mounted.is_empty() {
        Ok(())
    } else {
        Err(mounted)
    }
}

#[cfg(test)]
mod tests {
    use super::super::plist::parse;
    use super::*;

    const INFO_SD: &str = include_str!("../../tests/fixtures/diskutil/info-sdcard.plist");
    const INFO_DISK0: &str = include_str!("../../tests/fixtures/diskutil/info-disk0.plist");
    const IOREG: &str = include_str!("../../tests/fixtures/diskutil/ioreg-sdcard.plist");
    const LIST: &str = include_str!("../../tests/fixtures/diskutil/list.plist");
    const INFO_ROOT: &str = include_str!("../../tests/fixtures/diskutil/info-root.plist");
    const INFO_APFS: &str = include_str!("../../tests/fixtures/diskutil/info-apfs-volume.plist");
    const INFO_EXFAT_DISK: &str =
        include_str!("../../tests/fixtures/diskutil/info-exfat-disk.plist");
    const INFO_EXFAT_VOLUME: &str =
        include_str!("../../tests/fixtures/diskutil/info-exfat-volume.plist");
    const INFO_MSDOS: &str = include_str!("../../tests/fixtures/diskutil/info-msdos-volume.plist");
    const DF_ROOT: &str = include_str!("../../tests/fixtures/diskutil/df-root.txt");
    const DF_MSDOS: &str = include_str!("../../tests/fixtures/diskutil/df-msdos.txt");

    fn id(s: &str) -> DiskId {
        DiskId::parse(s).unwrap()
    }

    /// The infos of the listing fixture: disk0 (internal SSD), disk1 (its synthesized APFS container), disk2 (an
    /// attached image with an exFAT volume) and that volume.
    fn infos() -> BTreeMap<String, Plist> {
        let mut container = parse(INFO_EXFAT_DISK).unwrap();
        // disk1: what diskutil says of a synthesized container (APFSPhysicalStores) — never listed.
        if let Plist::Dict(d) = &mut container {
            d.retain(|(k, _)| k != "DeviceIdentifier");
            d.push(("DeviceIdentifier".into(), Plist::Str("disk1".into())));
            d.push((
                "APFSPhysicalStores".into(),
                Plist::Array(vec![Plist::Dict(vec![(
                    "APFSPhysicalStore".into(),
                    Plist::Str("disk0s1".into()),
                )])]),
            ));
        }
        [
            ("disk0", parse(INFO_DISK0).unwrap()),
            ("disk1", container),
            ("disk1s1", parse(INFO_APFS).unwrap()),
            ("disk2", parse(INFO_EXFAT_DISK).unwrap()),
            ("disk2s1", parse(INFO_EXFAT_VOLUME).unwrap()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
    }

    #[test]
    fn lists_whole_disks_and_flags_the_system_disk() {
        let list = parse(LIST).unwrap();
        let root = parse(INFO_ROOT).unwrap();
        let ioreg = parse(IOREG).unwrap();
        let listed = disks(&list, &infos(), &root, Some(&ioreg)).unwrap();
        let ids: Vec<&str> = listed.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(ids, ["disk0", "disk2"]);
        let (d0, d2) = (&listed[0], &listed[1]);
        assert!(d0.system && d0.internal && !d0.removable && !d0.virtual_disk);
        assert_eq!(d0.solid_state, Some(true));
        assert_eq!(d0.geometry.logical_sector, 4096);
        assert_eq!(d0.model.as_deref(), Some("APPLE SSD AP1024N"));
        assert_eq!(d0.serial.as_deref(), Some("TESTSERIAL"));
        assert_eq!(d0.vendor, None, "an empty Vendor Name is no vendor");
        assert_eq!(d0.volumes.len(), 1);
        assert_eq!(d0.volumes[0].id, "disk0s1");
        assert_eq!(d0.volumes[0].fs, None, "a partition without info has no fs");

        assert!(d2.virtual_disk && d2.removable && !d2.internal && !d2.system);
        assert_eq!(d2.solid_state, None);
        assert_eq!(d2.geometry.size, 64 << 20);
        assert_eq!(d2.vendor.as_deref(), Some("Apple"));
        assert_eq!(d2.model.as_deref(), Some("Disk Image"));
        assert_eq!(
            d2.volumes,
            [VolumeInfo {
                id: "disk2s1".into(),
                fs: Some("exfat".into()),
                label: Some("TESTCARD".into()),
                mount_point: Some(PathBuf::from("/Volumes/TESTCARD")),
                size: 67_108_352,
            }]
        );

        // Without ioreg: no serial, the model from MediaName; the system disk is still found.
        let plain = disks(&list, &infos(), &root, None).unwrap();
        assert_eq!(plain[0].serial, None);
        assert_eq!(plain[0].model.as_deref(), Some("APPLE SSD AP1024N"));
        assert!(plain[0].system);
        // Another root (on disk2) moves the system flag.
        let root2 = parse(INFO_EXFAT_VOLUME).unwrap();
        let moved = disks(&list, &infos(), &root2, None).unwrap();
        assert!(!moved[0].system && moved[1].system);
        // A whole disk without info is left out; garbage lists nothing.
        let mut some = infos();
        some.remove("disk0");
        assert_eq!(disks(&list, &some, &root, None).unwrap().len(), 1);
        assert_eq!(
            disks(&Plist::Bool(true), &infos(), &root, None),
            Some(Vec::new())
        );
        // A root info naming no disk (no ParentWholeDisk, no physical store): no listing, never one without a system disk.
        let orphan = without(&root, &["ParentWholeDisk", "APFSPhysicalStores"]);
        assert_eq!(disks(&list, &infos(), &orphan, None), None);
        assert_eq!(disks(&list, &infos(), &Plist::Bool(true), None), None);
    }

    #[test]
    fn destination_on_an_apfs_container_maps_to_its_physical_disk() {
        let info = parse(INFO_APFS).unwrap();
        let dest = destination(DF_ROOT, &info).unwrap();
        assert_eq!(dest.disks, [id("disk1"), id("disk0")]);
        assert_eq!(dest.fs.as_deref(), Some("apfs"));
        assert_eq!(dest.free_bytes, 64_688 * 1024);
        assert_eq!(dest.max_file_bytes, None);
        // Info about another node is refused, as is df output without a data line.
        let exfat = parse(INFO_EXFAT_VOLUME).unwrap();
        assert_eq!(destination(DF_ROOT, &exfat), None);
        assert_eq!(destination("Filesystem 1024-blocks\n", &info), None);
        assert_eq!(destination("", &info), None);
    }

    #[test]
    fn fat32_destination_caps_the_file_size() {
        let info = parse(INFO_MSDOS).unwrap();
        let dest = destination(DF_MSDOS, &info).unwrap();
        assert_eq!(dest.max_file_bytes, Some(4_294_967_295));
        assert_eq!(dest.fs.as_deref(), Some("msdos"));
        assert_eq!(dest.disks, [id("disk5")]);
        assert_eq!(dest.free_bytes, 64_509 * 1024);
    }

    #[test]
    fn df_lines_and_bsd_names() {
        let df = "Filesystem 1024-blocks Used Available Capacity Mounted on\n\
                  map auto home   0    0    7    100%    /System/Volumes/Data/home\n";
        assert_eq!(df_line(df), Some(("map auto home".into(), 7)));
        assert_eq!(df_line(DF_ROOT), Some(("/dev/disk1s1".into(), 64_688)));
        assert_eq!(df_line("h\n/dev/disk1 1 2 x 3% /"), None);
        assert_eq!(whole_of("disk0s2"), Some(id("disk0")));
        assert_eq!(whole_of("/dev/disk3s1s1"), Some(id("disk3")));
        assert_eq!(whole_of("disk12"), Some(id("disk12")));
        assert_eq!(whole_of("diskx"), None);
        assert_eq!(whole_of("disk1x"), None);
        for ok in ["disk4", "disk4s1", "disk3s1s1"] {
            assert!(is_bsd_name(ok), "{ok}");
        }
        for bad in [
            "disk",
            "disks1",
            "disk4s",
            "-disk4",
            "disk4 s1",
            "/dev/disk4",
            "disk4s1;x",
        ] {
            assert!(!is_bsd_name(bad), "{bad}");
        }
    }

    #[test]
    fn swapped_media_is_told_apart() {
        let a = parse(INFO_SD).unwrap();
        assert!(same_media(&a, &a.clone()));
        // Volatile keys do not matter; a different size or name does.
        assert!(same_media(&a, &with(&a, "FreeSpace", Plist::Int(7))));
        assert!(!same_media(&a, &with(&a, "TotalSize", Plist::Int(1 << 30))));
        assert!(!same_media(
            &a,
            &with(&a, "MediaName", Plist::Str("Other".into()))
        ));
        assert!(!same_media(
            &a,
            &with(&a, "DiskUUID", Plist::Str("x".into()))
        ));
        assert!(!same_media(&Plist::Bool(true), &Plist::Bool(true)));
    }

    fn without(p: &Plist, keys: &[&str]) -> Plist {
        match p {
            Plist::Dict(d) => Plist::Dict(
                d.iter()
                    .filter(|(k, _)| !keys.contains(&k.as_str()))
                    .cloned()
                    .collect(),
            ),
            _ => panic!("not a dict"),
        }
    }

    fn with(p: &Plist, key: &str, v: Plist) -> Plist {
        match without(p, &[key]) {
            Plist::Dict(mut d) => {
                d.push((key.to_string(), v));
                Plist::Dict(d)
            }
            _ => unreachable!(),
        }
    }

    /// The ioreg fixture with the disk image's `Device Characteristics` reporting a physical sector of `physical`.
    fn ioreg_512e(physical: i64) -> Plist {
        let Plist::Array(mut devices) = parse(IOREG).unwrap() else {
            panic!("not an array")
        };
        let image = &devices[3];
        let chars = with(
            image.get("Device Characteristics").unwrap(),
            "Physical Block Size",
            Plist::Int(physical),
        );
        devices[3] = with(image, "Device Characteristics", chars);
        Plist::Array(devices)
    }

    #[test]
    fn geometry_from_diskutil_and_ioreg() {
        let info = parse(INFO_SD).unwrap();
        let ioreg = parse(IOREG).unwrap();
        let sd = Geometry {
            size: 64 << 20,
            logical_sector: 512,
            physical_sector: 512,
        };
        assert_eq!(geometry(&info, Some(&ioreg), "disk2"), Some(sd));
        assert_eq!(geometry(&info, None, "disk2"), Some(sd));
        // A 512e device: the physical sector comes from ioreg, and only for its own media.
        let e512 = ioreg_512e(4096);
        let want = Geometry {
            physical_sector: 4096,
            ..sd
        };
        assert_eq!(geometry(&info, Some(&e512), "disk2"), Some(want));
        assert_eq!(geometry(&info, None, "disk2"), Some(sd));
        for implausible in [0, 1000, 256, 1 << 20] {
            assert_eq!(
                geometry(&info, Some(&ioreg_512e(implausible)), "disk2"),
                Some(sd),
                "{implausible}"
            );
        }
        // The internal 4Kn NVMe disk: its physical sector is among the device's own properties.
        let disk0 = parse(INFO_DISK0).unwrap();
        let internal = Geometry {
            size: 1_000_555_581_440,
            logical_sector: 4096,
            physical_sector: 4096,
        };
        assert_eq!(geometry(&disk0, Some(&ioreg), "disk0"), Some(internal));
        assert_eq!(geometry(&disk0, None, "disk0"), Some(internal));

        // Only `Size`: used. Neither size: no geometry.
        let only_size = without(&info, &["TotalSize"]);
        assert_eq!(geometry(&only_size, None, "disk2"), Some(sd));
        let only_size = with(&only_size, "Size", Plist::Int(1 << 30));
        assert_eq!(
            geometry(&only_size, None, "disk2").map(|g| g.size),
            Some(1 << 30)
        );
        assert_eq!(
            geometry(&without(&info, &["TotalSize", "Size"]), None, "disk2"),
            None
        );
        assert_eq!(
            geometry(&without(&info, &["DeviceBlockSize"]), None, "disk2"),
            None
        );
        for bad in [0, 1000, 256, -512] {
            let info = with(&info, "DeviceBlockSize", Plist::Int(bad));
            assert_eq!(geometry(&info, Some(&ioreg), "disk2"), None, "{bad}");
        }
        assert_eq!(
            geometry(&with(&info, "TotalSize", Plist::Int(0)), None, "disk2"),
            None
        );
        // Info about another disk is refused; a disk ioreg does not list keeps physical == logical.
        assert_eq!(geometry(&info, Some(&ioreg), "disk3"), None);
        let renamed = with(&info, "DeviceIdentifier", Plist::Str("disk9".into()));
        assert_eq!(geometry(&renamed, Some(&e512), "disk9"), Some(sd));
        assert_eq!(geometry(&Plist::Bool(true), Some(&ioreg), "disk2"), None);
    }

    #[test]
    fn unmount_failure_lists_busy_volumes() {
        let list = parse(LIST).unwrap();
        let root = parse(INFO_ROOT).unwrap();
        let mounted = disks(&list, &infos(), &root, None).unwrap();
        let mut unmounted = mounted.clone();
        for v in &mut unmounted[1].volumes {
            v.mount_point = None;
        }
        let disk2 = id("disk2");
        // The tool failed and disk2s1 is still mounted: that volume is named.
        assert_eq!(
            unmount_result(false, &mounted, &disk2),
            Err(vec!["disk2s1".to_string()])
        );
        // The tool succeeded but the system still shows it mounted (remounted meanwhile): not unmounted.
        assert_eq!(
            unmount_result(true, &mounted, &disk2),
            Err(vec!["disk2s1".to_string()])
        );
        // Success and no mount point left.
        assert_eq!(unmount_result(true, &unmounted, &disk2), Ok(()));
        // Other disks' mount points do not count (disk0s1 has none in the fixture; give it one).
        let mut other = unmounted.clone();
        other[0].volumes[0].mount_point = Some(PathBuf::from("/"));
        assert_eq!(unmount_result(true, &other, &disk2), Ok(()));
        // The tool failed though nothing is mounted, or the disk left: never taken as unmounted.
        assert_eq!(unmount_result(false, &unmounted, &disk2), Err(Vec::new()));
        assert_eq!(
            unmount_result(true, &unmounted, &id("disk7")),
            Err(Vec::new())
        );
    }
}
