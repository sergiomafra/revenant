//! Windows disks and destinations assembled from what the system queries return (rulings D9, D12; P16). Pure: the FFI
//! in `source/windows.rs` fills the plain structs below (and hands over the raw bytes of variable-length IOCTL outputs,
//! decoded here), so all of this is tested on every platform.

use std::path::PathBuf;

use super::{DestInfo, DiskInfo, FAT_MAX_FILE, VolumeInfo, push_unique, sort_disks};
use crate::source::{DiskId, Geometry};

/// `STORAGE_BUS_TYPE` values that mean an external or removable bus: 1394, USB, SD, MMC.
const EXTERNAL_BUSES: [i32; 4] = [4, 7, 12, 13];
/// `STORAGE_BUS_TYPE` values of disks the system makes from files: `BusTypeVirtual`, `BusTypeFileBackedVirtual`.
const VIRTUAL_BUSES: [i32; 2] = [14, 15];
/// Bytes of one `DISK_EXTENT` (`DiskNumber` u32 at 0, padding, `StartingOffset` i64 at 8, `ExtentLength` i64 at 16).
const EXTENT_SIZE: usize = 24;
/// Where `VOLUME_DISK_EXTENTS.Extents` starts (after `NumberOfDiskExtents` and padding to 8).
const EXTENTS_AT: usize = 8;

/// The decoded `STORAGE_DEVICE_DESCRIPTOR` (`IOCTL_STORAGE_QUERY_PROPERTY`, `StorageDeviceProperty`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct WinDescriptor {
    pub(crate) vendor: Option<String>,
    pub(crate) product: Option<String>,
    pub(crate) serial: Option<String>,
    pub(crate) removable: bool,
    pub(crate) bus: i32,
}

/// One `\\.\PhysicalDriveN` as queried.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WinDiskRaw {
    pub(crate) number: u32,
    pub(crate) geometry: Geometry,
    pub(crate) descriptor: WinDescriptor,
    /// `StorageDeviceSeekPenaltyProperty`, when the device answers.
    pub(crate) seek_penalty: Option<bool>,
}

/// A piece of a volume on a disk (`DISK_EXTENT`; its starting offset is not needed).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WinExtent {
    pub(crate) disk: u32,
    pub(crate) len: u64,
}

/// One volume of `FindFirstVolumeW`/`FindNextVolumeW`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WinVolumeRaw {
    /// The volume GUID path, `\\?\Volume{…}\`.
    pub(crate) name: String,
    pub(crate) fs: Option<String>,
    pub(crate) label: Option<String>,
    pub(crate) mount_point: Option<PathBuf>,
    pub(crate) extents: Vec<WinExtent>,
}

/// A destination folder's volume as queried.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WinDestRaw {
    pub(crate) fs: Option<String>,
    pub(crate) free_bytes: u64,
    pub(crate) extents: Vec<WinExtent>,
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

/// The NUL-terminated ASCII string at `offset` of `bytes` (0: none), trimmed; `None` when empty or out of range.
fn c_string(bytes: &[u8], offset: u32) -> Option<String> {
    if offset == 0 {
        return None;
    }
    let tail = bytes.get(offset as usize..)?;
    let end = tail.iter().position(|&b| b == 0).unwrap_or(tail.len());
    let s = String::from_utf8_lossy(&tail[..end]).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Decodes the `STORAGE_DEVICE_DESCRIPTOR` the device returned (`bytes`: exactly what it returned). `None` when too short
/// for the fixed fields.
pub(crate) fn descriptor(bytes: &[u8]) -> Option<WinDescriptor> {
    let bus = u32_at(bytes, 28)? as i32;
    Some(WinDescriptor {
        vendor: c_string(bytes, u32_at(bytes, 12)?),
        product: c_string(bytes, u32_at(bytes, 16)?),
        serial: c_string(bytes, u32_at(bytes, 24)?),
        removable: bytes[10] != 0,
        bus,
    })
}

/// Decodes `VOLUME_DISK_EXTENTS`: as many extents as it declares and `bytes` holds (a buffer too small for all of them,
/// `ERROR_MORE_DATA`, still gives the first ones).
pub(crate) fn extents(bytes: &[u8]) -> Vec<WinExtent> {
    let Some(count) = u32_at(bytes, 0) else {
        return Vec::new();
    };
    bytes
        .get(EXTENTS_AT..)
        .unwrap_or(&[])
        .as_chunks::<EXTENT_SIZE>()
        .0
        .iter()
        .take(count as usize)
        .filter_map(|e| {
            Some(WinExtent {
                disk: u32_at(e, 0)?,
                len: u64_at(e, 16)?,
            })
        })
        .collect()
}

fn drive(number: u32) -> Option<DiskId> {
    DiskId::parse(&format!("PhysicalDrive{number}")).ok()
}

/// Volume GUID paths compared without case and trailing backslash.
fn same_volume(a: &str, b: &str) -> bool {
    a.trim_end_matches('\\')
        .eq_ignore_ascii_case(b.trim_end_matches('\\'))
}

/// The connected whole disks (ruling D9): each queried disk with the volumes that have an extent on it (a dynamic volume
/// spanning several disks is listed under each), in volume order. The system disks are those holding an extent of
/// `system_volume` (the volume GUID path of `%SystemDrive%`), so a system volume spanning disks marks them all. Sorted
/// by id.
pub(crate) fn disks(
    raw: &[WinDiskRaw],
    volumes: &[WinVolumeRaw],
    system_volume: Option<&str>,
) -> Vec<DiskInfo> {
    let system: Vec<u32> = volumes
        .iter()
        .filter(|v| system_volume.is_some_and(|s| same_volume(&v.name, s)))
        .flat_map(|v| v.extents.iter().map(|e| e.disk))
        .collect();
    let mut out: Vec<DiskInfo> = Vec::new();
    for d in raw {
        let Some(id) = drive(d.number) else { continue };
        if out.iter().any(|o| o.id == id) {
            continue;
        }
        let desc = &d.descriptor;
        let virtual_disk = VIRTUAL_BUSES.contains(&desc.bus);
        let volumes = volumes
            .iter()
            .filter(|v| v.extents.iter().any(|e| e.disk == d.number))
            .map(|v| VolumeInfo {
                id: v.name.clone(),
                fs: v.fs.clone(),
                label: v.label.clone(),
                mount_point: v.mount_point.clone(),
                size: v.extents.iter().fold(0u64, |s, e| s.saturating_add(e.len)),
            })
            .collect();
        out.push(DiskInfo {
            id,
            geometry: d.geometry,
            vendor: desc.vendor.clone(),
            model: desc.product.clone(),
            serial: desc.serial.clone(),
            removable: desc.removable,
            internal: !desc.removable && !virtual_disk && !EXTERNAL_BUSES.contains(&desc.bus),
            system: system.contains(&d.number),
            virtual_disk,
            solid_state: d.seek_penalty.map(|p| !p),
            volumes,
        });
    }
    sort_disks(&mut out);
    out
}

/// Where a folder lives (ruling D12): the disks of its volume's extents, its file system (`FAT`/`FAT32` cap a file at
/// 4 GiB − 1) and free space. `None` without any extent (the disks cannot be told).
pub(crate) fn destination(raw: &WinDestRaw) -> Option<DestInfo> {
    let mut disks = Vec::new();
    for e in &raw.extents {
        push_unique(&mut disks, drive(e.disk)?);
    }
    if disks.is_empty() {
        return None;
    }
    let fat = raw
        .fs
        .as_deref()
        .is_some_and(|f| f.eq_ignore_ascii_case("FAT") || f.eq_ignore_ascii_case("FAT32"));
    Some(DestInfo {
        disks,
        fs: raw.fs.clone(),
        free_bytes: raw.free_bytes,
        max_file_bytes: fat.then_some(FAT_MAX_FILE),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> DiskId {
        DiskId::parse(s).unwrap()
    }

    fn geometry(size: u64) -> Geometry {
        Geometry {
            size,
            logical_sector: 512,
            physical_sector: 512,
        }
    }

    fn disk(number: u32, bus: i32, removable: bool, seek_penalty: Option<bool>) -> WinDiskRaw {
        WinDiskRaw {
            number,
            geometry: geometry(u64::from(number + 1) << 30),
            descriptor: WinDescriptor {
                vendor: Some("Vendor".into()),
                product: Some(format!("Model {number}")),
                serial: (number == 1).then(|| "TESTSERIAL".into()),
                removable,
                bus,
            },
            seek_penalty,
        }
    }

    fn extent(disk: u32, len: u64) -> WinExtent {
        WinExtent { disk, len }
    }

    fn volume(name: &str, fs: &str, mount: Option<&str>, extents: Vec<WinExtent>) -> WinVolumeRaw {
        WinVolumeRaw {
            name: format!(r"\\?\Volume{{{name}}}\"),
            fs: Some(fs.into()),
            label: None,
            mount_point: mount.map(PathBuf::from),
            extents,
        }
    }

    #[test]
    fn windows_disks_are_assembled_from_raw_structs() {
        // 0: the system disk (NVMe, bus 17); 1: a USB card reader with one FAT32 volume; 2: a VHD; 3 and 4: a dynamic
        // volume spanning both.
        let raw = [
            disk(2, 15, false, Some(false)),
            disk(0, 17, false, Some(false)),
            disk(1, 7, true, None),
            disk(3, 11, false, Some(true)),
            disk(4, 11, false, Some(true)),
        ];
        let volumes = [
            volume("sys", "NTFS", Some(r"C:\"), vec![extent(0, 100 << 30)]),
            volume("card", "FAT32", Some(r"E:\"), vec![extent(1, 1 << 30)]),
            volume(
                "span",
                "NTFS",
                Some(r"F:\"),
                vec![extent(3, 5), extent(4, 7)],
            ),
        ];
        let system = r"\\?\VOLUME{SYS}";
        let listed = disks(&raw, &volumes, Some(system));
        let ids: Vec<&str> = listed.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "PhysicalDrive0",
                "PhysicalDrive1",
                "PhysicalDrive2",
                "PhysicalDrive3",
                "PhysicalDrive4"
            ]
        );
        let [sys, usb, vhd, d3, d4] = &listed[..] else {
            unreachable!()
        };
        assert!(sys.system && sys.internal && !sys.removable && !sys.virtual_disk);
        assert_eq!(sys.solid_state, Some(true));
        assert_eq!(sys.volumes[0].mount_point, Some(PathBuf::from(r"C:\")));
        assert!(usb.removable && !usb.internal && !usb.system && !usb.virtual_disk);
        assert_eq!(usb.serial.as_deref(), Some("TESTSERIAL"));
        assert_eq!(usb.model.as_deref(), Some("Model 1"));
        assert_eq!(usb.solid_state, None);
        assert_eq!(
            usb.volumes,
            [VolumeInfo {
                id: r"\\?\Volume{card}\".into(),
                fs: Some("FAT32".into()),
                label: None,
                mount_point: Some(PathBuf::from(r"E:\")),
                size: 1 << 30,
            }]
        );
        assert!(vhd.virtual_disk && !vhd.internal && vhd.volumes.is_empty());
        assert_eq!(vhd.geometry, geometry(3 << 30));
        // The spanning volume is on both disks, whole size.
        assert_eq!(d3.volumes, d4.volumes);
        assert_eq!(d3.volumes[0].size, 12);
        assert_eq!(d3.solid_state, Some(false));
        assert!(!d3.system && !d4.system);

        // A system volume spanning two disks marks both (D9).
        let disks2 = disks(&raw, &volumes, Some(r"\\?\Volume{span}\"));
        let system: Vec<&str> = disks2
            .iter()
            .filter(|d| d.system)
            .map(|d| d.id.as_str())
            .collect();
        assert_eq!(system, ["PhysicalDrive3", "PhysicalDrive4"]);
        // No system volume known: nothing is the system disk.
        assert!(disks(&raw, &volumes, None).iter().all(|d| !d.system));
    }

    #[test]
    fn windows_fat_destination_caps_the_file_size() {
        let dest = |fs: &str| WinDestRaw {
            fs: Some(fs.into()),
            free_bytes: 123,
            extents: vec![extent(2, 10), extent(5, 10), extent(2, 20)],
        };
        for fs in ["FAT32", "FAT", "fat32"] {
            let d = destination(&dest(fs)).unwrap();
            assert_eq!(d.max_file_bytes, Some(4_294_967_295), "{fs}");
        }
        for fs in ["NTFS", "exFAT", "ReFS"] {
            assert_eq!(destination(&dest(fs)).unwrap().max_file_bytes, None, "{fs}");
        }
        let d = destination(&dest("NTFS")).unwrap();
        assert_eq!(d.disks, [id("PhysicalDrive2"), id("PhysicalDrive5")]);
        assert_eq!(d.free_bytes, 123);
        assert_eq!(d.fs.as_deref(), Some("NTFS"));
        let none = WinDestRaw {
            extents: Vec::new(),
            ..dest("NTFS")
        };
        assert_eq!(destination(&none), None);
    }

    #[test]
    fn ioctl_outputs_are_decoded() {
        // STORAGE_DEVICE_DESCRIPTOR: removable USB, vendor at 40, product at 48, no revision, serial at 60.
        let mut d = vec![0u8; 72];
        d[10] = 1;
        d[12..16].copy_from_slice(&40u32.to_le_bytes());
        d[16..20].copy_from_slice(&48u32.to_le_bytes());
        d[24..28].copy_from_slice(&60u32.to_le_bytes());
        d[28..32].copy_from_slice(&7u32.to_le_bytes());
        d[40..47].copy_from_slice(b"Generic");
        d[48..58].copy_from_slice(b"SD Reader ");
        d[60..70].copy_from_slice(b"TESTSERIAL");
        let got = descriptor(&d).unwrap();
        assert_eq!(
            got,
            WinDescriptor {
                vendor: Some("Generic".into()),
                product: Some("SD Reader".into()),
                serial: Some("TESTSERIAL".into()),
                removable: true,
                bus: 7,
            }
        );
        // Offsets out of range or pointing at nothing are no string; too short is no descriptor.
        d[24..28].copy_from_slice(&500u32.to_le_bytes());
        d[16..20].copy_from_slice(&71u32.to_le_bytes());
        let got = descriptor(&d).unwrap();
        assert_eq!((got.serial, got.product), (None, None));
        assert_eq!(descriptor(&d[..31]), None);

        // VOLUME_DISK_EXTENTS with two extents; a buffer cut after the first (ERROR_MORE_DATA) gives one.
        let mut e = vec![0u8; 8 + 2 * 24];
        e[0..4].copy_from_slice(&2u32.to_le_bytes());
        for (i, (disk, off, len)) in [(1u32, 1u64 << 20, 5u64), (3, 0, 9)]
            .into_iter()
            .enumerate()
        {
            let at = 8 + i * 24;
            e[at..at + 4].copy_from_slice(&disk.to_le_bytes());
            e[at + 8..at + 16].copy_from_slice(&off.to_le_bytes());
            e[at + 16..at + 24].copy_from_slice(&len.to_le_bytes());
        }
        assert_eq!(
            extents(&e),
            [WinExtent { disk: 1, len: 5 }, WinExtent { disk: 3, len: 9 }]
        );
        assert_eq!(extents(&e[..8 + 24 + 10]).len(), 1);
        e[0..4].copy_from_slice(&1u32.to_le_bytes());
        assert_eq!(extents(&e).len(), 1, "never more than declared");
        assert!(extents(&e[..3]).is_empty());
    }
}
