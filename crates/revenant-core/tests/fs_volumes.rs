//! Volume detection: MBR (with EBR chains), GPT, superfloppy, backup boot sectors, probing, hostile tables.

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use revenant_core::EngineError;
use revenant_core::filesystem::{FsKind, Scheme, Volume, volumes};
use revenant_core::source::{FileSource, Source, VolumeId};
use revenant_testkit::fs::{
    FsBuilder, FsType, HiddenSectors, Partitioning, crc32, disk, disk_with, mbr_disk,
};
use revenant_testkit::{DiskImage, jpeg};

const MIB: usize = 1 << 20;

fn source(bytes: &[u8]) -> (tempfile::NamedTempFile, FileSource) {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(bytes).unwrap();
    file.flush().unwrap();
    let src = FileSource::open(file.path()).unwrap();
    (file, src)
}

fn found(bytes: &[u8]) -> Vec<Volume> {
    let (_file, src) = source(bytes);
    volumes(&src)
}

/// A source that counts the reads made through it and the bytes they asked for.
struct Counting {
    inner: FileSource,
    reads: AtomicU64,
    bytes: AtomicU64,
}

impl Source for Counting {
    fn len(&self) -> u64 {
        self.inner.len()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, EngineError> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(buf.len() as u64, Ordering::Relaxed);
        self.inner.read_at(offset, buf)
    }

    fn volume(&self) -> Option<VolumeId> {
        self.inner.volume()
    }
}

/// The core README's bounds on what one `volumes` call reads ("File systems: volumes", Bounds).
const MAX_READS: u64 = 1362;
const MAX_BYTES: u64 = 16_588_800;

/// `volumes` on `bytes`, within the README's read bounds (and, generously, 10 s).
fn found_quickly(bytes: &[u8]) -> Vec<Volume> {
    let (_file, inner) = source(bytes);
    let src = Counting {
        inner,
        reads: AtomicU64::new(0),
        bytes: AtomicU64::new(0),
    };
    let t = Instant::now();
    let v = volumes(&src);
    assert!(
        t.elapsed() < Duration::from_secs(10),
        "took {:?}",
        t.elapsed()
    );
    let (reads, read) = (src.reads.into_inner(), src.bytes.into_inner());
    assert!(reads <= MAX_READS, "{reads} reads");
    assert!(read <= MAX_BYTES, "{read} bytes read");
    assert!(v.len() <= 16);
    v
}

fn kind(fs: FsType) -> FsKind {
    match fs {
        FsType::Fat12 => FsKind::Fat12,
        FsType::Fat16 => FsKind::Fat16,
        FsType::Fat32 => FsKind::Fat32,
        FsType::Exfat => FsKind::Exfat,
    }
}

fn put32(v: &mut [u8], at: usize, x: u32) {
    v[at..at + 4].copy_from_slice(&x.to_le_bytes());
}

fn put64(v: &mut [u8], at: usize, x: u64) {
    v[at..at + 8].copy_from_slice(&x.to_le_bytes());
}

/// Rewrites the exFAT boot checksum sector (11) of the boot region at `v[0..]` (512-byte sectors), from the spec.
fn exfat_checksum(v: &mut [u8]) {
    exfat_checksum_with(v, 512);
}

/// The same with `bps`-byte sectors.
fn exfat_checksum_with(v: &mut [u8], bps: usize) {
    let mut sum = 0u32;
    for (i, &b) in v[..11 * bps].iter().enumerate() {
        if i != 106 && i != 107 && i != 112 {
            sum = sum.rotate_right(1).wrapping_add(b as u32);
        }
    }
    for i in 0..bps / 4 {
        put32(v, 11 * bps + 4 * i, sum);
    }
}

/// What `volumes` must report for builder `b` placed at `offset`.
fn expected(
    fs: FsType,
    b: &FsBuilder,
    len: usize,
    cluster: u32,
    offset: u64,
    scheme: Scheme,
) -> Volume {
    Volume {
        index: 0,
        fs: kind(fs),
        offset,
        len: len as u64,
        sector_size: 512,
        cluster_size: cluster as u64,
        heap_offset: offset + b.cluster_offset(2),
        cluster_count: b.cluster_count(),
        label: Some("REVENANT".to_string()),
        serial: 0x1234_5678,
        from_backup: false,
        scheme,
    }
}

const LAYOUTS: [(FsType, usize, u32); 4] = [
    (FsType::Fat12, 2 * MIB, 1024),
    (FsType::Fat16, 16 * MIB, 2048),
    (FsType::Fat32, 48 * MIB, 512),
    (FsType::Exfat, 16 * MIB, 32 * 1024),
];

#[test]
fn finds_the_volume_of_each_layout() {
    for (fs, len, cluster) in LAYOUTS {
        let b = FsBuilder::new(fs, len as u64, cluster);
        let volume = b.build();
        for (p, offset, scheme) in [
            (Partitioning::Superfloppy, 0u64, Scheme::None),
            (
                Partitioning::Mbr { first_lba: 8192 },
                8192 * 512,
                Scheme::Mbr,
            ),
            (
                Partitioning::Gpt { first_lba: 2048 },
                2048 * 512,
                Scheme::Gpt,
            ),
        ] {
            let image = disk(p, &volume, len + 8 * MIB);
            assert_eq!(
                found(&image),
                [expected(fs, &b, len, cluster, offset, scheme)],
                "{fs:?} at {offset}"
            );
        }
    }
}

/// Ruling R1: the boot sector's own record of where it sits (FAT `BPB_HiddSec`, exFAT `PartitionOffset`) is never
/// trusted for placement, whether 0 or wrong.
#[test]
fn hidden_sectors_fields_are_ignored() {
    for (fs, len, cluster) in LAYOUTS {
        let b = FsBuilder::new(fs, len as u64, cluster);
        let volume = b.build();
        let want = [expected(fs, &b, len, cluster, 8192 * 512, Scheme::Mbr)];
        let zero = disk_with(
            Partitioning::Mbr { first_lba: 8192 },
            &volume,
            len + 8 * MIB,
            HiddenSectors::Zero,
        );
        assert_eq!(found(&zero), want, "{fs:?} with 0");
        let mut wrong = disk(
            Partitioning::Mbr { first_lba: 8192 },
            &volume,
            len + 8 * MIB,
        );
        let at = 8192 * 512;
        if fs == FsType::Exfat {
            put64(&mut wrong, at + 64, 999);
            exfat_checksum(&mut wrong[at..]);
            wrong.copy_within(at..at + 12 * 512, at + 12 * 512);
        } else {
            put32(&mut wrong, at + 28, 999);
            if fs == FsType::Fat32 {
                put32(&mut wrong, at + 6 * 512 + 28, 999);
            }
        }
        assert_eq!(found(&wrong), want, "{fs:?} with a wrong value");
    }
}

#[test]
fn damaged_primary_boot_sector_uses_the_backup() {
    for (fs, len, cluster) in [LAYOUTS[2], LAYOUTS[3]] {
        let b = FsBuilder::new(fs, len as u64, cluster);
        let mut want = expected(fs, &b, len, cluster, 8192 * 512, Scheme::Mbr);
        want.from_backup = true;
        let image = disk(
            Partitioning::Mbr { first_lba: 8192 },
            &b.build(),
            len + 8 * MIB,
        );
        let at = 8192 * 512;
        let mut zeroed = image.clone();
        zeroed[at..at + 512].fill(0);
        assert_eq!(found(&zeroed), [want.clone()], "{fs:?} zeroed");
        if fs == FsType::Exfat {
            // Only the checksum disagrees: the backup region replaces the primary.
            let mut flipped = image.clone();
            flipped[at + 512 * 3 + 7] ^= 0xFF;
            assert_eq!(found(&flipped), [want.clone()], "checksum mismatch");
            // The same with the backup region damaged too: the primary's fields are still used.
            flipped[at + 12 * 512..at + 13 * 512].fill(0);
            let mut primary = want.clone();
            primary.from_backup = false;
            assert_eq!(found(&flipped), [primary], "no usable backup");
            // Primary gone and the backup's checksum broken: the backup's fields are still used.
            let mut both = zeroed.clone();
            both[at + 12 * 512 + 512 * 3 + 7] ^= 0xFF;
            assert_eq!(found(&both), [want.clone()], "backup checksum mismatch");
        }
    }
}

#[test]
fn destroyed_partition_table_is_probed() {
    for (fs, len, cluster) in LAYOUTS {
        let b = FsBuilder::new(fs, len as u64, cluster);
        let mut image = disk(
            Partitioning::Mbr { first_lba: 8192 },
            &b.build(),
            len + 8 * MIB,
        );
        image[..512].fill(0);
        assert_eq!(
            found(&image),
            [expected(fs, &b, len, cluster, 8192 * 512, Scheme::None)],
            "{fs:?}"
        );
    }
    // Table and primary boot sector gone: the probe still finds the volume through its backup.
    let b = FsBuilder::new(FsType::Fat32, 48 * MIB as u64, 512);
    let mut image = disk(Partitioning::Mbr { first_lba: 2048 }, &b.build(), 56 * MIB);
    image[..512].fill(0);
    image[2048 * 512..2049 * 512].fill(0);
    let mut want = expected(FsType::Fat32, &b, 48 * MIB, 512, 2048 * 512, Scheme::None);
    want.from_backup = true;
    assert_eq!(found(&image), [want]);
}

#[test]
fn several_partitions_and_an_extended_chain() {
    let fat32 = FsBuilder::new(FsType::Fat32, 48 * MIB as u64, 512).build();
    let exfat = FsBuilder::new(FsType::Exfat, 16 * MIB as u64, 32 * 1024).build();
    let fat16 = FsBuilder::new(FsType::Fat16, 16 * MIB as u64, 2048).build();
    let s32 = (fat32.len() / 512) as u32;
    let sex = (exfat.len() / 512) as u32;
    let s16 = (fat16.len() / 512) as u32;
    let a = 2048 + s32 + sex + 1;
    let b = a + s16 + 1;
    let image = mbr_disk(
        &[(2048, &fat32, 0x0C), (2048 + s32, &exfat, 0x07)],
        &[(a, &fat16, 0x0E), (b, &fat16, 0x0E)],
        (b + s16) as usize * 512 + MIB,
    );
    let v = found(&image);
    let got: Vec<(u32, FsKind, u64, Scheme)> = v
        .iter()
        .map(|v| (v.index, v.fs, v.offset, v.scheme))
        .collect();
    assert_eq!(
        got,
        [
            (0, FsKind::Fat32, 2048 * 512, Scheme::Mbr),
            (1, FsKind::Exfat, (2048 + s32) as u64 * 512, Scheme::Mbr),
            (2, FsKind::Fat16, a as u64 * 512, Scheme::Mbr),
            (3, FsKind::Fat16, b as u64 * 512, Scheme::Mbr),
        ]
    );
}

/// One 16-byte MBR/EBR entry.
fn entry(sector: &mut [u8], slot: usize, kind: u8, start: u32, size: u32) {
    let e = &mut sector[446 + 16 * slot..462 + 16 * slot];
    e[4] = kind;
    put32(e, 8, start);
    put32(e, 12, size);
}

fn sign(sector: &mut [u8]) {
    sector[510] = 0x55;
    sector[511] = 0xAA;
}

/// A 2 MiB FAT12 volume (4096 sectors).
fn small_fat12() -> Vec<u8> {
    FsBuilder::new(FsType::Fat12, 2 * MIB as u64, 1024).build()
}

#[test]
fn ebr_chain_stops_at_a_loop() {
    let fat = small_fat12();
    let mut d = vec![0u8; 16 * MIB];
    // Extended partition at LBA 100. EBR 100: logical at 4000 (relative 3900), next at 100 + 50. EBR 150 links to itself.
    entry(&mut d[..512], 0, 0x0F, 100, 30_000);
    sign(&mut d[..512]);
    let e1 = &mut d[100 * 512..101 * 512];
    entry(e1, 0, 0x01, 3900, 4096);
    entry(e1, 1, 0x05, 50, 1);
    sign(e1);
    let e2 = &mut d[150 * 512..151 * 512];
    entry(e2, 1, 0x05, 50, 1);
    sign(e2);
    d[4000 * 512..4000 * 512 + fat.len()].copy_from_slice(&fat);
    let v = found_quickly(&d);
    assert_eq!(v.len(), 1);
    assert_eq!((v[0].offset, v[0].scheme), (4000 * 512, Scheme::Mbr));
}

#[test]
fn ebr_chain_follows_at_most_64_links() {
    let fat = small_fat12();
    let mut d = vec![0u8; 24 * MIB];
    entry(&mut d[..512], 0, 0x0F, 100, 40_000);
    sign(&mut d[..512]);
    // 1000 EBRs at 100, 101, ...; the 64th (index 63) holds a volume at 5000, the 65th one at 10000.
    for k in 0..1000u32 {
        let lba = 100 + k;
        let s = &mut d[lba as usize * 512..(lba + 1) as usize * 512];
        if k == 63 {
            entry(s, 0, 0x01, 5000 - lba, 4096);
        }
        if k == 64 {
            entry(s, 0, 0x01, 10_000 - lba, 4096);
        }
        entry(s, 1, 0x05, k + 1, 1);
        sign(s);
    }
    d[5000 * 512..5000 * 512 + fat.len()].copy_from_slice(&fat);
    d[10_000 * 512..10_000 * 512 + fat.len()].copy_from_slice(&fat);
    let v = found_quickly(&d);
    let got: Vec<u64> = v.iter().map(|v| v.offset).collect();
    assert_eq!(got, [5000 * 512]);
}

/// A GPT disk whose headers (both) claim `count` entries of `size` bytes, header CRCs made valid.
fn gpt_with_entries(count: u32, size: u32) -> Vec<u8> {
    let b = FsBuilder::new(FsType::Fat16, 16 * MIB as u64, 2048);
    let mut d = disk(Partitioning::Gpt { first_lba: 2048 }, &b.build(), 24 * MIB);
    let last = d.len() / 512 - 1;
    for lba in [1, last] {
        let h = &mut d[lba * 512..lba * 512 + 512];
        put32(h, 80, count);
        put32(h, 84, size);
        put32(h, 16, 0);
        let crc = crc32(&h[..92]);
        put32(h, 16, crc);
    }
    d
}

#[test]
fn hostile_partition_tables_are_bounded() {
    // Huge or zero-sized GPT entry arrays: the table is refused, the probe still finds the volume.
    for size in [128, 0] {
        let v = found_quickly(&gpt_with_entries(0xFFFF_FFFF, size));
        let got: Vec<(u64, Scheme)> = v.iter().map(|v| (v.offset, v.scheme)).collect();
        assert_eq!(got, [(2048 * 512, Scheme::None)], "size {size}");
    }
    // 129 entries, every CRC valid (backup header gone): over the cap, refused.
    let mut d = gpt_with_entries(129, 128);
    let last = d.len() / 512 - 1;
    d[last * 512..].fill(0);
    let entries_crc = crc32(&d[2 * 512..2 * 512 + 129 * 128]);
    let h = &mut d[512..1024];
    put32(h, 88, entries_crc);
    put32(h, 16, 0);
    let crc = crc32(&h[..92]);
    put32(h, 16, crc);
    let v = found_quickly(&d);
    let got: Vec<(u64, Scheme)> = v.iter().map(|v| (v.offset, v.scheme)).collect();
    assert_eq!(got, [(2048 * 512, Scheme::None)], "129 entries");
    // MBR entries starting past the source's end.
    let mut d = vec![0u8; MIB];
    entry(&mut d[..512], 0, 0x0C, 0xFFFF_FF00, 100);
    entry(&mut d[..512], 1, 0x0F, 0x0100_0000, 100);
    entry(&mut d[..512], 2, 0x07, 2048, 0xFFFF_FFFF);
    sign(&mut d[..512]);
    assert_eq!(found_quickly(&d), []);
}

/// A volume in an MBR at LBA 2048, with each of its boot sectors (primary and backup) patched by `f` (exFAT boot
/// checksums refreshed, so only the patched field is wrong).
fn patched(fs: FsType, len: usize, cluster: u32, f: impl Fn(&mut [u8])) -> Vec<u8> {
    let b = FsBuilder::new(fs, len as u64, cluster);
    let mut d = disk(
        Partitioning::Mbr { first_lba: 2048 },
        &b.build(),
        len + 4 * MIB,
    );
    let backups: &[usize] = match fs {
        FsType::Fat32 => &[0, 6 * 512],
        FsType::Exfat => &[0, 12 * 512],
        _ => &[0],
    };
    for &at in backups {
        let at = 2048 * 512 + at;
        f(&mut d[at..at + 512]);
        if fs == FsType::Exfat {
            exfat_checksum(&mut d[at..]);
        }
    }
    d
}

#[test]
fn invalid_boot_sectors_give_no_volume() {
    let fat16 = (FsType::Fat16, 16 * MIB, 2048u32);
    let fat32 = (FsType::Fat32, 48 * MIB, 512u32);
    let exfat = (FsType::Exfat, 16 * MIB, 32 * 1024u32);
    type Patch = Box<dyn Fn(&mut [u8])>;
    let cases: Vec<(&str, (FsType, usize, u32), Patch)> = vec![
        ("FAT16 SecPerClus 0", fat16, Box::new(|s| s[13] = 0)),
        ("FAT16 SecPerClus 3", fat16, Box::new(|s| s[13] = 3)),
        ("FAT32 SecPerClus 0", fat32, Box::new(|s| s[13] = 0)),
        ("FAT32 SecPerClus 3", fat32, Box::new(|s| s[13] = 3)),
        ("FAT16 no signature", fat16, Box::new(|s| s[511] = 0)),
        ("FAT16 no jump", fat16, Box::new(|s| s[0] = 0)),
        (
            "FAT16 BytsPerSec 768",
            fat16,
            Box::new(|s| s[11..13].copy_from_slice(&768u16.to_le_bytes())),
        ),
        ("FAT16 RsvdSecCnt 0", fat16, Box::new(|s| s[14..16].fill(0))),
        ("FAT16 NumFATs 3", fat16, Box::new(|s| s[16] = 3)),
        (
            "FAT16 data past the end",
            fat16,
            Box::new(|s| s[22..24].copy_from_slice(&0xFFFFu16.to_le_bytes())),
        ),
        ("FAT32 RootClus 1", fat32, Box::new(|s| put32(s, 44, 1))),
        (
            "FAT32 RootClus past the end",
            fat32,
            Box::new(|s| put32(s, 44, 0x0FFF_FFF0)),
        ),
        (
            "exFAT FatOffset past the volume",
            exfat,
            Box::new(|s| put32(s, 80, 0x7FFF_FFFF)),
        ),
        ("exFAT FatOffset 23", exfat, Box::new(|s| put32(s, 80, 23))),
        (
            "exFAT ClusterCount 0xFFFF_FFFF",
            exfat,
            Box::new(|s| put32(s, 92, 0xFFFF_FFFF)),
        ),
        ("exFAT ClusterCount 0", exfat, Box::new(|s| put32(s, 92, 0))),
        (
            "exFAT BytesPerSectorShift 31",
            exfat,
            Box::new(|s| s[108] = 31),
        ),
        (
            "exFAT BytesPerSectorShift 8",
            exfat,
            Box::new(|s| s[108] = 8),
        ),
        (
            "exFAT SectorsPerClusterShift 17",
            exfat,
            Box::new(|s| s[109] = 17),
        ),
        ("exFAT NumberOfFats 0", exfat, Box::new(|s| s[110] = 0)),
        (
            "exFAT heap over the FAT",
            exfat,
            Box::new(|s| put32(s, 88, 129)),
        ),
        (
            "exFAT VolumeLength short",
            exfat,
            Box::new(|s| put64(s, 72, 1000)),
        ),
        ("exFAT root cluster 1", exfat, Box::new(|s| put32(s, 96, 1))),
        (
            "exFAT root past the heap",
            exfat,
            Box::new(|s| put32(s, 96, 100_000)),
        ),
        ("exFAT byte 20 set", exfat, Box::new(|s| s[20] = 1)),
        ("exFAT no signature", exfat, Box::new(|s| s[510] = 0)),
    ];
    for (name, (fs, len, cluster), f) in cases {
        assert_eq!(found_quickly(&patched(fs, len, cluster, f)), [], "{name}");
    }
    // The untouched images do give a volume (so each case above is refused for its own field).
    for (fs, len, cluster) in [fat16, fat32, exfat] {
        assert_eq!(found(&patched(fs, len, cluster, |_| {})).len(), 1);
    }
}

#[test]
fn twenty_logical_volumes_give_sixteen() {
    let fat16 = FsBuilder::new(FsType::Fat16, (5 * MIB / 2) as u64, 512).build();
    let s = (fat16.len() / 512) as u32;
    let logical: Vec<(u32, &[u8], u8)> = (0..20)
        .map(|k| (2049 + k * (s + 1), &fat16[..], 0x0E))
        .collect();
    let end = logical[19].0 + s;
    let image = mbr_disk(&[], &logical, end as usize * 512 + MIB);
    let v = found_quickly(&image);
    let got: Vec<(u32, u64)> = v.iter().map(|v| (v.index, v.offset)).collect();
    let want: Vec<(u32, u64)> = (0..16)
        .map(|k| (k, (2049 + k * (s + 1)) as u64 * 512))
        .collect();
    assert_eq!(got, want);
}

/// A GPT on a disk of 4096-byte logical blocks: header at byte 4096.
#[test]
fn gpt_with_4096_byte_blocks() {
    let b = FsBuilder::new(FsType::Exfat, 16 * MIB as u64, 32 * 1024);
    let volume = b.build();
    let mut d = vec![0u8; 24 * MIB];
    let blocks = (d.len() / 4096) as u64;
    entry(&mut d[..512], 0, 0xEE, 1, (blocks - 1) as u32);
    sign(&mut d[..512]);
    let mut entries = vec![0u8; 128 * 128];
    entries[0] = 0xA2;
    put64(&mut entries, 32, 256);
    put64(&mut entries, 40, 256 + volume.len() as u64 / 4096 - 1);
    let mut h = vec![0u8; 92];
    h[..8].copy_from_slice(b"EFI PART");
    put32(&mut h, 8, 0x0001_0000);
    put32(&mut h, 12, 92);
    put64(&mut h, 24, 1);
    put64(&mut h, 32, blocks - 1);
    put64(&mut h, 72, 2);
    put32(&mut h, 80, 128);
    put32(&mut h, 84, 128);
    put32(&mut h, 88, crc32(&entries));
    let crc = crc32(&h);
    put32(&mut h, 16, crc);
    d[4096..4096 + 92].copy_from_slice(&h);
    d[2 * 4096..2 * 4096 + entries.len()].copy_from_slice(&entries);
    d[256 * 4096..256 * 4096 + volume.len()].copy_from_slice(&volume);
    let want = expected(
        FsType::Exfat,
        &b,
        16 * MIB,
        32 * 1024,
        256 * 4096,
        Scheme::Gpt,
    );
    assert_eq!(found(&d), [want]);
}

#[test]
fn gpt_backup_header_and_entries() {
    let b = FsBuilder::new(FsType::Fat16, 16 * MIB as u64, 2048);
    let image = disk(Partitioning::Gpt { first_lba: 2048 }, &b.build(), 24 * MIB);
    let gpt = expected(FsType::Fat16, &b, 16 * MIB, 2048, 2048 * 512, Scheme::Gpt);
    let mut probed = gpt.clone();
    probed.scheme = Scheme::None;
    let last = image.len() / 512 - 1;
    // Primary header CRC broken: the backup header is used.
    let mut d = image.clone();
    d[512 + 40] ^= 1;
    assert_eq!(found(&d), std::slice::from_ref(&gpt), "header CRC");
    // Primary entries broken: their CRC fails, the backup's entries are used.
    let mut d = image.clone();
    d[2 * 512 + 33] ^= 1;
    assert_eq!(found(&d), std::slice::from_ref(&gpt), "entries CRC");
    // Primary header gone, its AlternateLBA unknown: the last LBA holds the backup.
    let mut d = image.clone();
    d[512..1024].fill(0);
    assert_eq!(found(&d), std::slice::from_ref(&gpt), "no primary");
    // Both broken: no table, the probe finds the volume.
    let mut d = image.clone();
    d[512 + 40] ^= 1;
    d[last * 512 + 40] ^= 1;
    assert_eq!(found(&d), [probed], "no header");
}

#[test]
fn cluster_offsets() {
    let b = FsBuilder::new(FsType::Fat16, 16 * MIB as u64, 2048);
    let v = found(&b.build()).remove(0);
    let last = v.cluster_count + 1;
    assert_eq!(v.cluster_offset(0), None);
    assert_eq!(v.cluster_offset(1), None);
    assert_eq!(v.cluster_offset(2), Some(b.cluster_offset(2)));
    assert_eq!(v.cluster_offset(3), Some(b.cluster_offset(3)));
    assert_eq!(v.cluster_offset(last), Some(b.cluster_offset(last)));
    assert_eq!(v.cluster_offset(last + 1), None);
    assert_eq!(v.cluster_offset(u32::MAX), None);
    assert_eq!(
        (FsKind::Fat12.as_str(), FsKind::Fat16.as_str()),
        ("fat12", "fat16")
    );
    assert_eq!(
        (FsKind::Fat32.as_str(), FsKind::Exfat.as_str()),
        ("fat32", "exfat")
    );
    assert_eq!(
        (
            Scheme::None.as_str(),
            Scheme::Mbr.as_str(),
            Scheme::Gpt.as_str()
        ),
        ("none", "mbr", "gpt")
    );
}

#[test]
fn labels() {
    // FAT: no label entry in the root → the BPB label; "NO NAME" → None.
    let b = FsBuilder::new(FsType::Fat16, 16 * MIB as u64, 2048);
    let mut v = b.build();
    // The root's label entry wins over the BPB label.
    v[43..54].copy_from_slice(b"OTHER      ");
    assert_eq!(found(&v)[0].label.as_deref(), Some("REVENANT"));
    let root = (1 + 2 * u16::from_le_bytes([v[22], v[23]]) as usize) * 512;
    v[root..root + 32].fill(0);
    v[43..54].copy_from_slice(b"BPB LABEL  ");
    assert_eq!(found(&v)[0].label.as_deref(), Some("BPB LABEL"));
    v[43..54].copy_from_slice(b"NO NAME    ");
    assert_eq!(found(&v)[0].label, None);
    // exFAT: no 0x83 entry → None.
    let b = FsBuilder::new(FsType::Exfat, 16 * MIB as u64, 32 * 1024);
    let mut v = b.build();
    let at = b.cluster_offset(u32::from_le_bytes(v[96..100].try_into().unwrap())) as usize;
    v[at] = 0x03;
    assert_eq!(found(&v)[0].label, None);
}

#[test]
fn noise_and_photos_are_not_volumes() {
    let mut img = DiskImage::noise(16 * MIB, 3);
    img.put(0, &jpeg(640, 480, 1, 0));
    let file = img.save();
    let src = FileSource::open(file.path()).unwrap();
    assert_eq!(volumes(&src), []);
}

/// A FAT32 boot sector (and FSInfo-less reserved area, FAT entries 0..2 and the backup at sector 6) of 16 MiB in
/// 4096-byte sectors: 32 reserved sectors, 2 FATs of 4 sectors, one sector per cluster, root at cluster 2.
fn fat32_4k() -> Vec<u8> {
    const BPS: usize = 4096;
    let mut v = vec![0u8; 16 * MIB];
    let b = &mut v[..512];
    b[..3].copy_from_slice(&[0xEB, 0x58, 0x90]);
    b[3..11].copy_from_slice(b"REVENANT");
    b[11..13].copy_from_slice(&(BPS as u16).to_le_bytes());
    b[13] = 1;
    b[14] = 32;
    b[16] = 2;
    b[21] = 0xF8;
    put32(b, 32, 4096);
    put32(b, 36, 4);
    put32(b, 44, 2);
    b[48] = 1;
    b[50] = 6;
    b[64] = 0x80;
    b[66] = 0x29;
    put32(b, 67, 0x0BAD_F00D);
    b[71..82].copy_from_slice(b"FOURK      ");
    b[82..90].copy_from_slice(b"FAT32   ");
    b[510] = 0x55;
    b[511] = 0xAA;
    v.copy_within(0..512, 6 * BPS);
    for fat in [32, 36] {
        let at = fat * BPS;
        v[at..at + 12].copy_from_slice(&[
            0xF8, 0xFF, 0xFF, 0x0F, 0xFF, 0xFF, 0xFF, 0x0F, 0xFF, 0xFF, 0xFF, 0x0F,
        ]);
    }
    v
}

/// An exFAT boot region (main and backup, checksums set) of 16 MiB in 4096-byte sectors: FAT at 24 (4 sectors),
/// heap at 32, one sector per cluster, 4064 clusters, root at cluster 2 (empty, so no label).
fn exfat_4k() -> Vec<u8> {
    const BPS: usize = 4096;
    let mut v = vec![0u8; 16 * MIB];
    let b = &mut v[..512];
    b[..3].copy_from_slice(&[0xEB, 0x76, 0x90]);
    b[3..11].copy_from_slice(b"EXFAT   ");
    put64(b, 72, 4096);
    put32(b, 80, 24);
    put32(b, 84, 4);
    put32(b, 88, 32);
    put32(b, 92, 4064);
    put32(b, 96, 2);
    put32(b, 100, 0x0BAD_F00D);
    b[104..106].copy_from_slice(&0x0100u16.to_le_bytes());
    b[108] = 12;
    b[109] = 0;
    b[110] = 1;
    b[111] = 0x80;
    b[112] = 0xFF;
    b[510] = 0x55;
    b[511] = 0xAA;
    for s in 1..=8 {
        put32(&mut v, s * BPS + BPS - 4, 0xAA55_0000);
    }
    exfat_checksum_with(&mut v, BPS);
    v.copy_within(0..12 * BPS, 12 * BPS);
    v[24 * BPS..24 * BPS + 8].copy_from_slice(&[0xF8, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
    v
}

/// What `volumes` reports for `fat32_4k` / `exfat_4k` at `offset`.
fn expected_4k(fs: FsKind, offset: u64, scheme: Scheme) -> Volume {
    let (heap, count, label) = match fs {
        FsKind::Fat32 => (40, 4056, Some("FOURK".to_string())),
        _ => (32, 4064, None),
    };
    Volume {
        index: 0,
        fs,
        offset,
        len: 16 * MIB as u64,
        sector_size: 4096,
        cluster_size: 4096,
        heap_offset: offset + heap * 4096,
        cluster_count: count,
        label,
        serial: 0x0BAD_F00D,
        from_backup: false,
        scheme,
    }
}

/// `volume` at LBA 2048 of a 24 MiB disk (512-byte MBR sectors), entry type `kind`.
fn in_mbr_2048(volume: &[u8], kind: u8) -> Vec<u8> {
    let mut d = vec![0u8; 24 * MIB];
    entry(&mut d[..512], 0, kind, 2048, (volume.len() / 512) as u32);
    sign(&mut d[..512]);
    d[MIB..MIB + volume.len()].copy_from_slice(volume);
    d
}

#[test]
fn backups_of_4096_byte_sector_volumes() {
    for (fs, volume, kind) in [
        (FsKind::Fat32, fat32_4k(), 0x0C),
        (FsKind::Exfat, exfat_4k(), 0x07),
    ] {
        let d = in_mbr_2048(&volume, kind);
        let mut want = expected_4k(fs, MIB as u64, Scheme::Mbr);
        assert_eq!(found(&d), [want.clone()], "{fs:?}");
        want.from_backup = true;
        let mut zeroed = d.clone();
        zeroed[MIB..MIB + 512].fill(0);
        assert_eq!(found(&zeroed), [want.clone()], "{fs:?} zeroed");
        if fs == FsKind::Exfat {
            // Only the main region's checksum disagrees: the backup (checksum over 4096-byte sectors) replaces it.
            let mut flipped = d.clone();
            flipped[MIB + 3 * 4096 + 7] ^= 0xFF;
            assert_eq!(found(&flipped), [want.clone()], "checksum mismatch");
        }
    }
}

/// Ruling R3: an MBR on a disk of 4096-byte sectors counts its LBAs in those sectors.
#[test]
fn mbr_on_a_4096_byte_sector_disk() {
    let (fat32, exfat) = (fat32_4k(), exfat_4k());
    let mut d = vec![0u8; 40 * MIB];
    // FAT32 at LBA 256 (1 MiB); extended at LBA 4352 (17 MiB), its EBR's logical exFAT 16 sectors further (LBA 4368).
    entry(&mut d[..512], 0, 0x0C, 256, 4096);
    entry(&mut d[..512], 1, 0x0F, 4352, 4096 + 16);
    sign(&mut d[..512]);
    let ebr = &mut d[4352 * 4096..4352 * 4096 + 512];
    entry(ebr, 0, 0x07, 16, 4096);
    sign(ebr);
    d[256 * 4096..256 * 4096 + fat32.len()].copy_from_slice(&fat32);
    d[4368 * 4096..4368 * 4096 + exfat.len()].copy_from_slice(&exfat);
    let mut second = expected_4k(FsKind::Exfat, 4368 * 4096, Scheme::Mbr);
    second.index = 1;
    assert_eq!(
        found_quickly(&d),
        [expected_4k(FsKind::Fat32, MIB as u64, Scheme::Mbr), second]
    );
    // A 512-byte-sector volume where `start × 4096` points is not the MBR's: only the probe finds it.
    let fat16 = FsBuilder::new(FsType::Fat16, 16 * MIB as u64, 2048).build();
    let mut d = vec![0u8; 24 * MIB];
    entry(&mut d[..512], 0, 0x0E, 256, 4096);
    sign(&mut d[..512]);
    d[MIB..MIB + fat16.len()].copy_from_slice(&fat16);
    let v = found_quickly(&d);
    let got: Vec<(u64, Scheme)> = v.iter().map(|v| (v.offset, v.scheme)).collect();
    assert_eq!(got, [(MIB as u64, Scheme::None)]);
}

/// fatgen103: a BPB with FATSz16 = 0 or RootEntCnt = 0 is no FAT12/16 one. A small FAT32 (12,092 clusters, under the
/// FAT16 limit) whose primary RootClus is corrupt is read from its backup, not as a FAT16.
#[test]
fn fat32_with_a_corrupt_root_cluster_uses_its_backup() {
    let b = FsBuilder::new(FsType::Fat32, 48 * MIB as u64, 512);
    let mut d = disk(Partitioning::Mbr { first_lba: 2048 }, &b.build(), 56 * MIB);
    let at = 2048 * 512;
    for boot in [at, at + 6 * 512] {
        d[boot + 13] = 8;
    }
    put32(&mut d, at + 44, 0);
    let v = found(&d);
    assert_eq!(v.len(), 1);
    assert_eq!(
        (v[0].fs, v[0].from_backup, v[0].cluster_count),
        (FsKind::Fat32, true, 12_092)
    );
}

/// An exFAT primary with a bad checksum gives way only to an exFAT backup, never to a FAT32 boot sector at sector 6.
#[test]
fn exfat_primary_never_gives_way_to_a_fat_backup() {
    let b = FsBuilder::new(FsType::Exfat, 16 * MIB as u64, 32 * 1024);
    let mut d = disk(Partitioning::Mbr { first_lba: 2048 }, &b.build(), 24 * MIB);
    let fat32 = FsBuilder::new(FsType::Fat32, 48 * MIB as u64, 512).build();
    let at = 2048 * 512;
    d[at + 6 * 512..at + 7 * 512].copy_from_slice(&fat32[..512]);
    d[at + 12 * 512..at + 13 * 512].fill(0);
    assert_eq!(
        found(&d),
        [expected(
            FsType::Exfat,
            &b,
            16 * MIB,
            32 * 1024,
            at as u64,
            Scheme::Mbr
        )]
    );
}

/// A probe that lands on a volume's backup boot sector (sector 6 of a FAT32, the backup region at 12 of an exFAT)
/// reports the volume where it starts, not at the probe.
#[test]
fn probe_does_not_misplace_a_volume_on_its_backup() {
    for (fs, len, cluster, back) in [
        (FsType::Fat32, 48 * MIB, 512u32, 6u64),
        (FsType::Exfat, 16 * MIB, 32 * 1024, 12),
    ] {
        let b = FsBuilder::new(fs, len as u64, cluster);
        let lba = 2048 - back;
        let mut d = disk(
            Partitioning::Mbr {
                first_lba: lba as u32,
            },
            &b.build(),
            len + 4 * MIB,
        );
        d[..512].fill(0);
        // Table gone, primary intact: found at its start, from its primary.
        let want = expected(fs, &b, len, cluster, lba * 512, Scheme::None);
        assert_eq!(found_quickly(&d), std::slice::from_ref(&want), "{fs:?}");
        // The same with its first FAT sector damaged: the primary alone still places it.
        let fat = (lba as usize + if fs == FsType::Fat32 { 32 } else { 128 }) * 512;
        let mut damaged = d.clone();
        damaged[fat..fat + 512].fill(0);
        assert_eq!(
            found_quickly(&damaged),
            std::slice::from_ref(&want),
            "{fs:?} with a damaged FAT"
        );
        // Table and primary gone: found at its start, from its backup.
        d[lba as usize * 512..lba as usize * 512 + 512].fill(0);
        let mut want = want;
        want.from_backup = true;
        assert_eq!(found_quickly(&d), [want], "{fs:?} without its primary");
    }
}

/// A hybrid MBR whose GPT headers both fail: its own (non-0xEE) entries are used before probing.
#[test]
fn hybrid_mbr_falls_back_to_its_entries() {
    let b = FsBuilder::new(FsType::Fat16, 16 * MIB as u64, 2048);
    let mut d = disk(Partitioning::Gpt { first_lba: 2048 }, &b.build(), 24 * MIB);
    let last = d.len() / 512 - 1;
    d[512 + 40] ^= 1;
    d[last * 512 + 40] ^= 1;
    entry(&mut d[..512], 1, 0x0E, 2048, 32_768);
    assert_eq!(
        found_quickly(&d),
        [expected(
            FsType::Fat16,
            &b,
            16 * MIB,
            2048,
            2048 * 512,
            Scheme::Mbr
        )]
    );
}

/// A GPT header must hold its own LBA (MyLBA), even with a matching CRC.
#[test]
fn gpt_header_at_the_wrong_lba_is_refused() {
    let b = FsBuilder::new(FsType::Fat16, 16 * MIB as u64, 2048);
    let mut d = disk(Partitioning::Gpt { first_lba: 2048 }, &b.build(), 24 * MIB);
    let last = d.len() / 512 - 1;
    for lba in [1, last] {
        let h = &mut d[lba * 512..lba * 512 + 512];
        put64(h, 24, lba as u64 + 5);
        put32(h, 16, 0);
        let crc = crc32(&h[..92]);
        put32(h, 16, crc);
    }
    let v = found_quickly(&d);
    let got: Vec<(u64, Scheme)> = v.iter().map(|v| (v.offset, v.scheme)).collect();
    assert_eq!(got, [(2048 * 512, Scheme::None)]);
}

/// A volume genuinely at a probe offset stays there, even when the sector where an earlier volume's FAT would start
/// (its own reserved sector 26) happens to look like one.
#[test]
fn probe_keeps_a_volume_whose_fat_is_where_it_says() {
    let b = FsBuilder::new(FsType::Fat32, 48 * MIB as u64, 512);
    let mut d = disk(Partitioning::Mbr { first_lba: 2048 }, &b.build(), 56 * MIB);
    d[..512].fill(0);
    let s26 = (2048 + 26) * 512;
    d[s26..s26 + 4].copy_from_slice(&[0xF8, 0xFF, 0xFF, 0x0F]);
    assert_eq!(
        found_quickly(&d),
        [expected(
            FsType::Fat32,
            &b,
            48 * MIB,
            512,
            2048 * 512,
            Scheme::None
        )]
    );
}
