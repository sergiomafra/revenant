//! What a source is, to recognize it later (ruling D7, P7, P20): a resumed copy, a reconnected card or a reopened session
//! must read the same card, not another one that happens to have the same name.
//!
//! A device is known by its size, model and a digest of its identity sectors (always, with or without a serial
//! number); an image by its size and a digest of three MiB of it. Compare with [`SourceIdentity::same_source`].

use crate::EngineError;
use crate::device::DiskInfo;
use crate::filesystem::{first_volume, volatile_boot_bytes};
use crate::rescue::MappedSource;
use crate::source::{Source, read_full};

const MIB: u64 = 1 << 20;
/// FNV-1a 64-bit offset basis and prime.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x100_0000_01b3;
/// The GPT's sectors after a protective MBR: header (1) and entries (2..=33).
const GPT_SECTORS: u64 = 33;
/// Largest logical sector believed (as the geometry parsers): bounds the identity reads.
const MAX_SECTOR: u64 = 1 << 16;
/// The MBR partition type of a protective MBR.
const GPT_PROTECTIVE: u8 = 0xEE;

/// Folds `bytes` into an FNV-1a 64 `state` (start from 0xcbf2_9ce4_8422_2325).
pub(crate) fn fnv1a64(state: u64, bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .fold(state, |h, &b| (h ^ u64::from(b)).wrapping_mul(FNV_PRIME))
}

/// What a source is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceIdentity {
    /// A whole disk: its size, model and serial as the system reports them, and `table`, the digest of its identity
    /// sectors.
    Device {
        size: u64,
        model: Option<String>,
        serial: Option<String>,
        table: u64,
    },
    /// An image file (or any other source): its size and the digest of its first, middle and last MiB.
    Image { size: u64, digest: u64 },
    /// A rescue copy's image read through its map (ruling P7): its size, the digest `Image` would have with every byte
    /// not rescued counted as zero, and `origin`, the identity of the source the map records. Rescuing more of those
    /// three MiB (a copy resumed meanwhile) changes it.
    Mapped {
        size: u64,
        digest: u64,
        origin: Box<SourceIdentity>,
    },
}

impl SourceIdentity {
    /// A device's identity (D7, P7, P20). `table` is always computed: the FNV-1a 64 of sector 0, of sectors 1..=33 when
    /// sector 0 is a protective MBR (a GPT), and of the boot sector of the first volume the partition table names, each
    /// a logical sector of `info.geometry` (taken as 512 B to 64 KiB). The bytes the system rewrites while a FAT or
    /// exFAT volume is mounted are zeroed first (FAT12/16 byte 37, FAT32 byte 65, exFAT 106, 107 and 112; also in sector 0 when it is itself the
    /// boot sector), so mounting the card does not change it. An unreadable sector counts as zeros; a device that is
    /// gone fails with `device_gone`.
    pub fn of_device(info: &DiskInfo, source: &dyn Source) -> Result<SourceIdentity, EngineError> {
        let sector = u64::from(info.geometry.logical_sector).clamp(512, MAX_SECTOR);
        let mut s0 = read_lenient(source, 0, sector)?;
        let boot_at_zero = !volatile_boot_bytes(&s0).is_empty();
        zero_volatile(&mut s0);
        let mut table = fnv1a64(FNV_OFFSET, &s0);
        if !boot_at_zero {
            if protective(&s0) {
                let gpt = read_lenient(source, sector, GPT_SECTORS * sector)?;
                table = fnv1a64(table, &gpt);
            }
            match first_volume(source).filter(|&at| at != 0) {
                Some(at) => {
                    let mut boot = read_lenient(source, at, sector)?;
                    zero_volatile(&mut boot);
                    table = fnv1a64(table, &boot);
                }
                // No volume found: also what a device gone meanwhile looks like, so make sure it is still there.
                None => {
                    read_lenient(source, 0, sector)?;
                }
            }
        }
        Ok(SourceIdentity::Device {
            size: info.geometry.size,
            model: info.model.clone(),
            serial: info.serial.clone(),
            table,
        })
    }

    /// An image's identity (D7): its size and the FNV-1a 64 of its first MiB, the MiB at its middle (aligned down to
    /// 1 MiB) and its last MiB, in that order (less where the source is smaller). Read errors fail.
    pub fn of_image(source: &dyn Source) -> Result<SourceIdentity, EngineError> {
        let size = source.len();
        let digest = three_mib_digest(size, |at, buf| read_full(source, at, buf))?;
        Ok(SourceIdentity::Image { size, digest })
    }

    /// A rescue copy's image read through its map (P7): the digest of [`SourceIdentity::of_image`], taken with every
    /// byte the map does not call rescued read as zero, and the identity the map records as `origin`.
    pub fn of_mapped(source: &MappedSource) -> Result<SourceIdentity, EngineError> {
        let size = source.len();
        let digest = three_mib_digest(size, |at, buf| {
            let n = source.read_zero_filled(at, buf)?;
            if n < buf.len() {
                return Err(EngineError::DeviceGone {
                    offset: at + n as u64,
                });
            }
            Ok(())
        })?;
        Ok(SourceIdentity::Mapped {
            size,
            digest,
            origin: Box::new(source.map().identity().clone()),
        })
    }

    /// Whether `other` is the same card (or image). Devices: sizes, models and `table`s equal, and the serials too when
    /// both sides have one (an extra check, never a substitute for `table`: readers often report their own serial).
    /// Images, and mapped images: equal. A device, an image and a mapped image are never the same as one another.
    pub fn same_source(&self, other: &SourceIdentity) -> bool {
        match (self, other) {
            (
                SourceIdentity::Device {
                    size,
                    model,
                    serial,
                    table,
                },
                SourceIdentity::Device {
                    size: size2,
                    model: model2,
                    serial: serial2,
                    table: table2,
                },
            ) => {
                let serials_agree = match (serial, serial2) {
                    (Some(a), Some(b)) => a == b,
                    _ => true,
                };
                size == size2 && model == model2 && table == table2 && serials_agree
            }
            (a @ SourceIdentity::Image { .. }, b @ SourceIdentity::Image { .. })
            | (a @ SourceIdentity::Mapped { .. }, b @ SourceIdentity::Mapped { .. }) => a == b,
            _ => false,
        }
    }
}

/// The FNV-1a 64 of the first MiB of a source of `size` bytes, the MiB at its middle (aligned down to 1 MiB) and its last
/// MiB, in that order (less where the source is smaller), each filled by `read`.
fn three_mib_digest(
    size: u64,
    read: impl Fn(u64, &mut [u8]) -> Result<(), EngineError>,
) -> Result<u64, EngineError> {
    let mut digest = FNV_OFFSET;
    let mut buf = Vec::new();
    for start in [0, (size / 2) & !(MIB - 1), size.saturating_sub(MIB)] {
        buf.resize((size - start.min(size)).min(MIB) as usize, 0);
        read(start, &mut buf)?;
        digest = fnv1a64(digest, &buf);
    }
    Ok(digest)
}

/// Whether sector 0 is a protective MBR: signed, with a partition of type 0xEE.
fn protective(s0: &[u8]) -> bool {
    s0[510..512] == [0x55, 0xAA] && (0..4).any(|i| s0[446 + 16 * i + 4] == GPT_PROTECTIVE)
}

fn zero_volatile(sector: &mut [u8]) {
    for &i in volatile_boot_bytes(sector) {
        sector[i] = 0;
    }
}

/// `len` bytes from `offset`: what lies past the source's end and any sector that cannot be read are zeros. Fails only
/// when the source is gone (or the read is cancelled).
fn read_lenient(source: &dyn Source, offset: u64, len: u64) -> Result<Vec<u8>, EngineError> {
    let mut buf = vec![0u8; len as usize];
    let avail = source.len().saturating_sub(offset).min(len) as usize;
    let lenient = |e: EngineError| match e {
        EngineError::Read { .. } => Ok(()),
        e => Err(e),
    };
    if let Err(e) = read_full(source, offset, &mut buf[..avail]) {
        lenient(e)?;
        for (i, piece) in buf[..avail].chunks_mut(512).enumerate() {
            if let Err(e) = read_full(source, offset + i as u64 * 512, piece) {
                lenient(e)?;
                piece.fill(0);
            }
        }
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::{DiskId, Geometry};
    use crate::test_support::SimSource;
    use revenant_testkit::fs::{FsBuilder, FsType, Partitioning, disk};

    fn info(size: u64, serial: Option<&str>) -> DiskInfo {
        DiskInfo {
            id: DiskId::parse("disk4").unwrap(),
            geometry: Geometry {
                size,
                logical_sector: 512,
                physical_sector: 512,
            },
            vendor: None,
            model: Some("SD Card Reader".into()),
            serial: serial.map(String::from),
            removable: true,
            internal: false,
            system: false,
            virtual_disk: false,
            solid_state: None,
            volumes: Vec::new(),
        }
    }

    fn table_of(data: &[u8], serial: Option<&str>) -> u64 {
        let source = SimSource::new(data.to_vec());
        match SourceIdentity::of_device(&info(data.len() as u64, serial), &source).unwrap() {
            SourceIdentity::Device { table, .. } => table,
            other => panic!("{other:?}"),
        }
    }

    fn card(p: Partitioning) -> Vec<u8> {
        let volume = FsBuilder::new(FsType::Exfat, 8 * MIB, 4096).build();
        disk(p, &volume, 16 * MIB as usize)
    }

    fn device(size: u64, model: &str, serial: Option<&str>, table: u64) -> SourceIdentity {
        SourceIdentity::Device {
            size,
            model: Some(model.into()),
            serial: serial.map(String::from),
            table,
        }
    }

    #[test]
    fn device_identity_always_has_the_table() {
        let boot = MIB as usize; // the exFAT volume at LBA 2048
        for (p, scheme) in [
            (Partitioning::Mbr { first_lba: 2048 }, "mbr"),
            (Partitioning::Gpt { first_lba: 2048 }, "gpt"),
        ] {
            let base = card(p);
            let t = table_of(&base, Some("TESTSERIAL"));
            assert_eq!(
                t,
                table_of(&base, None),
                "the serial is not part of the table"
            );
            assert_ne!(
                t,
                fnv1a64(FNV_OFFSET, &base[..512]),
                "the volume is in the table"
            );
            // Mounting rewrites the exFAT flags and use percentage: same table.
            let mut mounted = base.clone();
            for i in [106, 107, 112] {
                mounted[boot + i] ^= 0x5A;
            }
            assert_eq!(table_of(&mounted, None), t, "{scheme}");
            // Another volume serial number, other boot code: another card.
            for i in [100, 200] {
                let mut other = base.clone();
                other[boot + i] ^= 1;
                assert_ne!(table_of(&other, None), t, "{scheme} byte {i}");
            }
            // Sector 0 counts.
            let mut other = base.clone();
            other[440] ^= 1;
            assert_ne!(table_of(&other, None), t, "{scheme}");
        }
        // The GPT entries count (sectors 2..=33).
        let gpt = card(Partitioning::Gpt { first_lba: 2048 });
        let mut renamed = gpt.clone();
        renamed[2 * 512 + 56] ^= 1; // the first entry's name
        assert_ne!(table_of(&renamed, None), table_of(&gpt, None));
        // A card without a partition table: sector 0 is the boot sector, its volatile bytes are zeroed too.
        let flat = card(Partitioning::Superfloppy);
        let mut mounted = flat.clone();
        mounted[106] ^= 1;
        assert_eq!(table_of(&mounted, None), table_of(&flat, None));
        mounted[100] ^= 1;
        assert_ne!(table_of(&mounted, None), table_of(&flat, None));

        // An unreadable sector reads as zeros; a gone device fails.
        let mbr = card(Partitioning::Mbr { first_lba: 2048 });
        let bad = SimSource::new(mbr.clone()).bad(MIB, 512);
        assert!(SourceIdentity::of_device(&info(16 * MIB, None), &bad).is_ok());
        // An implausible sector size is capped at 64 KiB: the reads stay bounded.
        let mut huge = info(16 * MIB, None);
        huge.geometry.logical_sector = 1 << 30;
        let capped = SimSource::new(card(Partitioning::Gpt { first_lba: 2048 }));
        assert!(SourceIdentity::of_device(&huge, &capped).is_ok());
        let most = capped.reads().iter().map(|&(_, l)| l).max().unwrap();
        assert_eq!(most, 33 << 16, "{:?}", capped.reads());
        let gone = SimSource::new(mbr).gone_at(MIB);
        let err = SourceIdentity::of_device(&info(16 * MIB, None), &gone).unwrap_err();
        assert_eq!(err.code(), "device_gone");

        // same_source: the serial is an extra check only.
        let a = device(16 * MIB, "Reader", Some("A"), 7);
        assert!(a.same_source(&a.clone()));
        assert!(!a.same_source(&device(16 * MIB, "Reader", Some("B"), 7)));
        assert!(a.same_source(&device(16 * MIB, "Reader", None, 7)));
        assert!(device(16 * MIB, "Reader", None, 7).same_source(&a));
        assert!(!a.same_source(&device(16 * MIB, "Reader", Some("A"), 8)));
        assert!(!a.same_source(&device(32 * MIB, "Reader", Some("A"), 7)));
        assert!(!a.same_source(&device(16 * MIB, "Other", Some("A"), 7)));
        let image = SourceIdentity::Image {
            size: 16 * MIB,
            digest: 7,
        };
        assert!(!a.same_source(&image) && !image.same_source(&a));
        assert!(image.same_source(&image.clone()));
    }

    #[test]
    fn image_identity_reads_three_mib() {
        let len = 64 * MIB;
        let source = SimSource::new(vec![0u8; len as usize]);
        let id = SourceIdentity::of_image(&source).unwrap();
        let mut reads = source.reads();
        reads.sort();
        let m = MIB as usize;
        assert_eq!(reads, [(0, m), (32 * MIB, m), (63 * MIB, m)]);
        let mut data = vec![0u8; len as usize];
        data[(32 * MIB + 5) as usize] = 1;
        let other = SourceIdentity::of_image(&SimSource::new(data)).unwrap();
        assert!(!id.same_source(&other));
        assert!(matches!(id, SourceIdentity::Image { size, .. } if size == len));
        // A source smaller than a MiB reads what there is.
        let small = SimSource::new(vec![1u8; 1000]);
        assert!(SourceIdentity::of_image(&small).is_ok());
        assert!(small.reads().iter().all(|&(o, l)| o == 0 && l == 1000));
    }
}
