//! A whole disk's geometry from what macOS tools print (ruling D5): `diskutil info -plist diskN` gives the size
//! (`TotalSize`, else `Size`) and the logical sector (`DeviceBlockSize`); `ioreg -a -r -c IOBlockStorageDevice -l` gives
//! the physical sector of the device whose whole-disk media is `diskN`, when it says.

use super::plist::Plist;
use crate::source::Geometry;

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

#[cfg(test)]
mod tests {
    use super::super::plist::parse;
    use super::*;

    const INFO_SD: &str = include_str!("../../tests/fixtures/diskutil/info-sdcard.plist");
    const INFO_DISK0: &str = include_str!("../../tests/fixtures/diskutil/info-disk0.plist");
    const IOREG: &str = include_str!("../../tests/fixtures/diskutil/ioreg-sdcard.plist");

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
}
