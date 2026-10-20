//! The synthetic FAT/exFAT volumes and MBR/GPT disks, checked against the Microsoft specifications' field offsets.

use revenant_testkit::fs::{
    DeleteStyle, FsBuilder, FsType, Partitioning, crc32, disk, mbr_disk, quick_format_in_place,
};

fn u16_at(v: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([v[at], v[at + 1]])
}

fn u32_at(v: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(v[at..at + 4].try_into().unwrap())
}

fn u64_at(v: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(v[at..at + 8].try_into().unwrap())
}

/// The 32-byte entries of one cluster.
fn entries(v: &[u8], b: &FsBuilder, cluster: u32, cluster_len: usize) -> Vec<[u8; 32]> {
    let at = b.cluster_offset(cluster) as usize;
    v[at..at + cluster_len]
        .chunks(32)
        .map(|e| e.try_into().unwrap())
        .collect()
}

/// FAT32 entry of `c` in the first FAT (32 reserved sectors).
fn fat32_entry(v: &[u8], c: u32) -> u32 {
    u32_at(v, 32 * 512 + 4 * c as usize)
}

fn lfn_checksum(sfn: &[u8]) -> u8 {
    sfn.iter().fold(0u8, |s, &c| {
        (if s & 1 != 0 { 0x80u8 } else { 0 })
            .wrapping_add(s >> 1)
            .wrapping_add(c)
    })
}

fn exfat_set_checksum(set: &[u8]) -> u16 {
    let mut sum = 0u16;
    for (i, &b) in set.iter().enumerate() {
        if i == 2 || i == 3 {
            continue;
        }
        sum = (if sum & 1 != 0 { 0x8000u16 } else { 0 })
            .wrapping_add(sum >> 1)
            .wrapping_add(b as u16);
    }
    sum
}

#[test]
fn fat32_layout_is_as_documented() {
    let mut b = FsBuilder::new(FsType::Fat32, 48 << 20, 512);
    let cs = b.add("DCIM/100TEST/DSC00001.JPG", &[7u8; 1500]);
    assert_eq!(cs.len(), 3);
    assert_eq!(cs[1], cs[0] + 1);
    assert_eq!(cs[2], cs[0] + 2);
    assert_eq!(b.clusters("DCIM/100TEST/DSC00001.JPG"), cs);
    let v = b.build();
    assert_eq!(v.len(), 48 << 20);
    assert_eq!(v[510..512], [0x55, 0xAA]);
    assert_eq!(&v[82..90], b"FAT32   ");
    assert_eq!(u16_at(&v, 11), 512);
    assert_eq!(v[13], 1);
    assert_eq!(u16_at(&v, 14), 32);
    assert_eq!(v[16], 2);
    assert_eq!(u32_at(&v, 44), 2, "root at cluster 2");
    assert_eq!(u16_at(&v, 48), 1, "FSInfo at sector 1");
    assert_eq!(u16_at(&v, 50), 6, "backup boot at sector 6");
    assert_eq!(u32_at(&v, 67), 0x1234_5678);
    assert_eq!(&v[71..82], b"REVENANT   ");
    assert_eq!(u32_at(&v, 512), 0x4161_5252, "FSInfo lead signature");
    assert_eq!(u32_at(&v, 512 + 484), 0x6141_7272);
    assert_eq!(v[6 * 512..7 * 512], v[..512]);
    assert!(b.cluster_count() >= 65_525);
    assert_eq!(fat32_entry(&v, cs[0]), cs[1]);
    assert_eq!(fat32_entry(&v, cs[1]), cs[2]);
    assert!(fat32_entry(&v, cs[2]) & 0x0FFF_FFFF >= 0x0FFF_FFF8);
    // The second FAT is a copy of the first.
    let fat_sectors = u32_at(&v, 36) as usize;
    let (fat1, fat2) = (32 * 512, (32 + fat_sectors) * 512);
    assert_eq!(
        v[fat1..fat1 + fat_sectors * 512],
        v[fat2..fat2 + fat_sectors * 512]
    );
    // The data heap follows the FATs.
    assert_eq!(b.cluster_offset(2), ((32 + 2 * fat_sectors) * 512) as u64);
    let at = b.cluster_offset(cs[0]) as usize;
    assert_eq!(v[at..][..512], [7u8; 512]);
    assert_eq!(v[b.cluster_offset(cs[2]) as usize..][..476], [7u8; 476]);
}

#[test]
fn fat12_and_fat16_geometry() {
    let mut b = FsBuilder::new(FsType::Fat12, 2 << 20, 1024);
    assert!(b.cluster_count() < 4085);
    let cs = b.add("DCIM/A.JPG", &[3u8; 2500]);
    assert_eq!(cs.len(), 3);
    let v = b.build();
    assert_eq!(&v[54..62], b"FAT12   ");
    assert_eq!(&v[43..54], b"REVENANT   ");
    assert_eq!(u32_at(&v, 39), 0x1234_5678);
    assert_eq!(u16_at(&v, 14), 1);
    assert_eq!(u16_at(&v, 17), 512);
    // 12-bit entries packed by pairs in the first FAT (sector 1).
    let fat12 = |c: u32| {
        let at = 512 + (c as usize * 3) / 2;
        let w = u16_at(&v, at);
        if c.is_multiple_of(2) {
            w & 0xFFF
        } else {
            w >> 4
        }
    };
    assert_eq!(fat12(cs[0]), cs[1] as u16);
    assert_eq!(fat12(cs[1]), cs[2] as u16);
    assert!(fat12(cs[2]) >= 0xFF8);
    // Root region: 32 sectors after the 2 FATs; the label first.
    let fat_sectors = u16_at(&v, 22) as usize;
    let root = (1 + 2 * fat_sectors) * 512;
    assert_eq!(&v[root..root + 11], b"REVENANT   ");
    assert_eq!(v[root + 11], 0x08);
    assert_eq!(&v[root + 32..root + 43], b"DCIM       ");
    assert_eq!(b.cluster_offset(2), (root + 32 * 512) as u64);

    let b = FsBuilder::new(FsType::Fat16, 16 << 20, 2048);
    assert!((4085..=65_524).contains(&b.cluster_count()));
    let v = b.build();
    assert_eq!(&v[54..62], b"FAT16   ");
    assert_eq!(u16_at(&v, 512), 0xFFF8);
}

#[test]
#[should_panic]
fn cluster_count_must_fit_the_type() {
    FsBuilder::new(FsType::Fat32, 16 << 20, 4096);
}

#[test]
fn fat_delete_styles() {
    for style in [DeleteStyle::Camera, DeleteStyle::WindowsFat32] {
        let mut b = FsBuilder::new(FsType::Fat32, 48 << 20, 512);
        b.add_at("DSC00002.JPG", &[9u8; 1500], &[70_000, 70_001, 70_002]);
        let live = b.build();
        let find = |v: &[u8], b: &FsBuilder| {
            entries(v, b, 2, 512)
                .into_iter()
                .find(|e| &e[1..11] == b"SC00002JPG")
                .unwrap()
        };
        let e = find(&live, &b);
        assert_eq!(e[0], b'D');
        assert_eq!(u16_at(&e, 20), 1);
        assert_eq!(u16_at(&e, 26), (70_000 & 0xFFFF) as u16);
        assert_eq!(u32_at(&e, 28), 1500);
        b.delete("DSC00002.JPG", style);
        let v = b.build();
        let e = find(&v, &b);
        assert_eq!(e[0], 0xE5);
        for c in 70_000..70_003 {
            assert_eq!(fat32_entry(&v, c), 0);
        }
        let high = match style {
            DeleteStyle::Camera => [1, 0],
            DeleteStyle::WindowsFat32 => [0, 0],
        };
        assert_eq!(e[20..22], high, "{style:?}");
        assert_eq!(u16_at(&e, 26), (70_000 & 0xFFFF) as u16);
        // The data stays.
        assert_eq!(v[b.cluster_offset(70_000) as usize..][..512], [9u8; 512]);
    }
}

#[test]
fn labels_are_written() {
    let v = FsBuilder::new(FsType::Fat32, 48 << 20, 512).build();
    let b = FsBuilder::new(FsType::Fat32, 48 << 20, 512);
    assert!(
        entries(&v, &b, 2, 512)
            .iter()
            .any(|e| e[11] == 0x08 && &e[..11] == b"REVENANT   ")
    );

    let b = FsBuilder::new(FsType::Exfat, 16 << 20, 32 << 10);
    let v = b.build();
    let root = u32_at(&v, 96);
    let label = entries(&v, &b, root, 32 << 10)
        .into_iter()
        .find(|e| e[0] == 0x83)
        .unwrap();
    assert_eq!(label[1], 8);
    let name: Vec<u16> = (0..8).map(|i| u16_at(&label, 2 + 2 * i)).collect();
    assert_eq!(String::from_utf16(&name).unwrap(), "REVENANT");
    assert_eq!(u32_at(&v, 100), 0x1234_5678);
}

#[test]
fn long_names_get_lfn_entries() {
    let mut b = FsBuilder::new(FsType::Fat32, 48 << 20, 512);
    let name = "Long name photo.jpg";
    b.add(name, &[1u8; 100]);
    let v = b.build();
    let e = entries(&v, &b, 2, 512);
    // Slot 0 holds the label.
    let (l2, l1, sfn) = (e[1], e[2], e[3]);
    assert_eq!(&sfn[..11], b"LONGNA~1JPG");
    assert_eq!(sfn[11], 0x20);
    assert_eq!(u32_at(&sfn, 28), 100);
    assert_eq!((l2[0], l1[0]), (0x42, 0x01));
    let sum = lfn_checksum(&sfn[..11]);
    for l in [l2, l1] {
        assert_eq!(l[11], 0x0F);
        assert_eq!(l[12], 0);
        assert_eq!(l[13], sum);
        assert_eq!(u16_at(&l, 26), 0);
    }
    let chars = |l: &[u8; 32]| -> Vec<u16> {
        let offs = [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30];
        offs.iter().map(|&o| u16_at(l, o)).collect()
    };
    let mut all = chars(&l1);
    all.extend(chars(&l2));
    let want: Vec<u16> = name.encode_utf16().collect();
    assert_eq!(all[..19], want[..]);
    assert_eq!(all[19], 0);
    assert!(all[20..].iter().all(|&c| c == 0xFFFF));

    b.delete(name, DeleteStyle::Camera);
    let v = b.build();
    let e = entries(&v, &b, 2, 512);
    assert_eq!([e[1][0], e[2][0], e[3][0]], [0xE5; 3]);
    assert_eq!(e[0][0], b'R', "the label stays");
}

#[test]
fn short_name_tails_do_not_collide() {
    let mut b = FsBuilder::new(FsType::Fat16, 16 << 20, 2048);
    b.add("DCIM/Long name photo.jpg", &[1]);
    b.add("DCIM/Long name photo 2.jpg", &[2]);
    b.add("DCIM/lower.jpg", &[3]);
    let v = b.build();
    let dcim = b.clusters("DCIM")[0];
    let sfns: Vec<[u8; 11]> = entries(&v, &b, dcim, 2048)
        .iter()
        .filter(|e| e[0] != 0 && e[11] != 0x0F)
        .map(|e| e[..11].try_into().unwrap())
        .collect();
    assert_eq!(
        sfns,
        [
            *b".          ",
            *b"..         ",
            *b"LONGNA~1JPG",
            *b"LONGNA~2JPG",
            *b"LOWER~1 JPG"
        ]
    );
}

#[test]
fn exfat_boot_region_checksums() {
    let b = FsBuilder::new(FsType::Exfat, 16 << 20, 32 << 10);
    let v = b.build();
    assert_eq!(&v[3..11], b"EXFAT   ");
    assert_eq!(v[510..512], [0x55, 0xAA]);
    assert_eq!(u64_at(&v, 72), (16 << 20) / 512);
    assert_eq!(u32_at(&v, 80), 128, "FatOffset");
    assert_eq!(v[108], 9);
    assert_eq!(v[109], 6);
    assert_eq!(v[110], 1);
    assert_eq!(u16_at(&v, 104), 0x0100);
    let heap = u32_at(&v, 88);
    assert_eq!(heap % 64, 0, "heap aligned to the cluster");
    assert!(heap >= 128 + u32_at(&v, 84));
    assert_eq!(u32_at(&v, 92), b.cluster_count());
    assert!(u32_at(&v, 84) as u64 * 512 >= (b.cluster_count() as u64 + 2) * 4);
    assert_eq!(b.cluster_offset(2), heap as u64 * 512);
    for s in 1..=8 {
        assert_eq!(v[s * 512 + 508..s * 512 + 512], [0, 0, 0x55, 0xAA]);
    }
    let mut sum = 0u32;
    for (i, &byte) in v[..11 * 512].iter().enumerate() {
        if i == 106 || i == 107 || i == 112 {
            continue;
        }
        sum = (if sum & 1 != 0 { 0x8000_0000u32 } else { 0 })
            .wrapping_add(sum >> 1)
            .wrapping_add(byte as u32);
    }
    for i in 0..128 {
        assert_eq!(u32_at(&v, 11 * 512 + 4 * i), sum);
    }
    assert_eq!(v[12 * 512..24 * 512], v[..12 * 512]);
}

#[test]
fn exfat_entry_sets() {
    let cluster = 32 << 10;
    let mut b = FsBuilder::new(FsType::Exfat, 16 << 20, cluster as u32);
    let cs = b.add("DCIM/A.JPG", &[5u8; 1500]);
    b.add_at("DCIM/B.JPG", &vec![6u8; 2 * cluster], &[300, 200]);
    let v = b.build();
    let dcim = b.clusters("DCIM")[0];
    let fat = u32_at(&v, 80) as usize * 512;
    let fat_entry = |v: &[u8], c: u32| u32_at(v, fat + 4 * c as usize);
    let root = u32_at(&v, 96);
    let bitmap = entries(&v, &b, root, cluster)
        .into_iter()
        .find(|e| e[0] == 0x81)
        .unwrap();
    let bitmap_at = b.cluster_offset(u32_at(&bitmap, 20)) as usize;
    let bit = |v: &[u8], c: u32| v[bitmap_at + (c as usize - 2) / 8] >> ((c - 2) % 8) & 1;
    let upcase = entries(&v, &b, root, cluster)
        .into_iter()
        .find(|e| e[0] == 0x82)
        .unwrap();
    assert_eq!(u64_at(&upcase, 24), 256);

    let e = entries(&v, &b, dcim, cluster);
    assert_eq!([e[0][0], e[1][0], e[2][0]], [0x85, 0xC0, 0xC1]);
    assert_eq!(e[0][1], 2, "SecondaryCount");
    assert_eq!(u16_at(&e[0], 4), 0x20);
    let set: Vec<u8> = e[..3].concat();
    assert_eq!(u16_at(&e[0], 2), exfat_set_checksum(&set));
    let stream = e[1];
    assert_eq!(stream[1], 0x03);
    assert_eq!(stream[3], 5, "NameLength");
    assert_eq!(u64_at(&stream, 8), 1500);
    assert_eq!(u64_at(&stream, 24), 1500);
    assert_eq!(u32_at(&stream, 20), cs[0]);
    let name: Vec<u16> = (0..5).map(|i| u16_at(&e[2], 2 + 2 * i)).collect();
    assert_eq!(String::from_utf16(&name).unwrap(), "A.JPG");
    // NameHash over the (identity up-cased) name.
    let mut hash = 0u16;
    for byte in "A.JPG".encode_utf16().flat_map(u16::to_le_bytes) {
        hash = (if hash & 1 != 0 { 0x8000u16 } else { 0 })
            .wrapping_add(hash >> 1)
            .wrapping_add(byte as u16);
    }
    assert_eq!(u16_at(&stream, 4), hash);
    // The non-consecutive file is FAT-chained.
    assert_eq!([e[3][0], e[4][0], e[5][0]], [0x85, 0xC0, 0xC1]);
    assert_eq!(e[4][1], 0x01);
    assert_eq!(u32_at(&e[4], 20), 300);
    assert_eq!(fat_entry(&v, 300), 200);
    assert_eq!(fat_entry(&v, 200), 0xFFFF_FFFF);
    assert_eq!(bit(&v, cs[0]), 1);
    assert_eq!(bit(&v, 300), 1);

    b.delete("DCIM/A.JPG", DeleteStyle::Camera);
    b.delete("DCIM/B.JPG", DeleteStyle::Camera);
    let d = b.build();
    let e = entries(&d, &b, dcim, cluster);
    assert_eq!([e[0][0], e[1][0], e[2][0]], [0x05, 0x40, 0x41]);
    assert_eq!([e[3][0], e[4][0], e[5][0]], [0x05, 0x40, 0x41]);
    assert_eq!(bit(&d, cs[0]), 0);
    assert_eq!(bit(&d, 300), 0);
    assert_eq!(bit(&d, 200), 0);
    assert_eq!(bit(&d, dcim), 1);
    assert_eq!(fat_entry(&d, cs[0]), fat_entry(&v, cs[0]));
    assert_eq!(fat_entry(&d, 300), 200);
    assert_eq!(fat_entry(&d, 200), 0xFFFF_FFFF);
}

#[test]
fn exfat_stamps_and_offsets() {
    use revenant_testkit::fs::FsStamp;
    let mut b = FsBuilder::new(FsType::Exfat, 16 << 20, 32 << 10);
    b.add("A.JPG", &[1]);
    let created = FsStamp {
        year: 2024,
        month: 3,
        day: 9,
        hour: 22,
        min: 15,
        sec: 7,
        utc_offset_min: Some(-180),
    };
    let modified = FsStamp {
        year: 2025,
        month: 12,
        day: 14,
        hour: 7,
        min: 12,
        sec: 6,
        utc_offset_min: None,
    };
    b.stamp("A.JPG", created, modified);
    let v = b.build();
    let root = u32_at(&v, 96);
    let e = entries(&v, &b, root, 32 << 10);
    let file = e.iter().find(|e| e[0] == 0x85).unwrap();
    let ts = |y: u32, mo: u32, d: u32, h: u32, mi: u32, s: u32| {
        (y - 1980) << 25 | mo << 21 | d << 16 | h << 11 | mi << 5 | (s / 2)
    };
    assert_eq!(u32_at(file, 8), ts(2024, 3, 9, 22, 15, 7));
    assert_eq!(u32_at(file, 12), ts(2025, 12, 14, 7, 12, 6));
    assert_eq!(file[20], 100, "odd second in the 10 ms increment");
    assert_eq!(file[21], 0);
    assert_eq!(file[22], 0x80 | (-12i8 as u8 & 0x7F));
    assert_eq!(file[23], 0);
}

#[test]
fn fat_default_stamp() {
    let mut b = FsBuilder::new(FsType::Fat32, 48 << 20, 512);
    b.add("A.JPG", &[1]);
    let v = b.build();
    let e = entries(&v, &b, 2, 512)[1];
    assert_eq!(&e[..11], b"A       JPG");
    let time = 7 << 11 | 12 << 5 | 2;
    let date = (2025 - 1980) << 9 | 12 << 5 | 14;
    assert_eq!(u16_at(&e, 14), time);
    assert_eq!(u16_at(&e, 16), date);
    assert_eq!(u16_at(&e, 22), time);
    assert_eq!(u16_at(&e, 24), date);
}

#[test]
fn raw_entries_utf16_names_and_fat_overrides() {
    let mut b = FsBuilder::new(FsType::Fat32, 48 << 20, 512);
    let mut raw = [0u8; 32];
    raw[..11].copy_from_slice(b"LOOP       ");
    raw[11] = 0x10;
    raw[26] = 2;
    b.mkdir("DCIM");
    b.raw_entry("DCIM", raw);
    let cs = b.add_utf16("DCIM", &[0x0041, 0xD800, 0x0042], &[4u8; 10]);
    b.set_fat(cs[0], 0x0FFF_FFF7);
    let v = b.build();
    let dcim = b.clusters("DCIM")[0];
    let e = entries(&v, &b, dcim, 512);
    assert_eq!(e[2], raw);
    assert_eq!(e[3][0], 0x41, "one LFN entry");
    assert_eq!(
        [u16_at(&e[3], 1), u16_at(&e[3], 3), u16_at(&e[3], 5)],
        [0x41, 0xD800, 0x42]
    );
    assert_eq!(&e[4][..11], b"A_B~1      ");
    let fat2 = 32 * 512 + u32_at(&v, 36) as usize * 512;
    assert_eq!(fat32_entry(&v, cs[0]), 0x0FFF_FFF7);
    assert_eq!(u32_at(&v, fat2 + 4 * cs[0] as usize), 0x0FFF_FFF7);
}

#[test]
fn deleted_clusters_are_reused_first_fit() {
    let mut b = FsBuilder::new(FsType::Fat32, 48 << 20, 512);
    let a = b.add("A.JPG", &[1u8; 5 * 512]);
    b.add("B.JPG", &[2u8; 512]);
    b.delete("A.JPG", DeleteStyle::Camera);
    let c = b.add("C.JPG", &[3u8; 7 * 512]);
    assert_eq!(c[..5], a[..]);
    assert_eq!(c[5], a[4] + 2, "skips B's cluster");
    let v = b.build();
    assert_eq!(v[b.cluster_offset(a[0]) as usize], 3, "the newer file wins");
    assert_eq!(fat32_entry(&v, a[4]), c[5]);
}

#[test]
fn directories_grow_their_chain() {
    let mut b = FsBuilder::new(FsType::Fat32, 48 << 20, 512);
    for i in 0..40 {
        b.add(&format!("DCIM/BIG/F{i:07}.JPG"), &[]);
    }
    // 2 dot entries + 40 = 42 entries of 32 bytes: 3 clusters of 512.
    let big = b.clusters("DCIM/BIG");
    assert_eq!(big.len(), 3);
    let v = b.build();
    assert_eq!(fat32_entry(&v, big[0]), big[1]);
    assert_eq!(fat32_entry(&v, big[1]), big[2]);
    let last = entries(&v, &b, big[2], 512);
    assert_eq!(&last[9][..11], b"F0000039JPG");
}

#[test]
fn quick_format_keeps_data_clusters() {
    for (fs, len, cluster) in [
        (FsType::Fat32, 48 << 20, 512u32),
        (FsType::Exfat, 16 << 20, 32 << 10),
    ] {
        let mut b = FsBuilder::new(fs, len, cluster);
        let mut kept = Vec::new();
        for i in 1..=3 {
            kept.extend(b.add(
                &format!("DCIM/100TEST/DSC0000{i}.JPG"),
                &vec![i as u8; 3 * cluster as usize],
            ));
        }
        kept.extend(b.clusters("DCIM"));
        kept.extend(b.clusters("DCIM/100TEST"));
        let v = b.build();
        let q = b.build_quick_formatted();
        let mut again = v.clone();
        quick_format_in_place(&mut again);
        assert_eq!(again, q, "{fs:?}");
        let cl = cluster as usize;
        let root = match fs {
            FsType::Exfat => u32_at(&q, 96),
            _ => 2,
        };
        for e in entries(&q, &b, root, cl) {
            match fs {
                FsType::Exfat => assert!(!matches!(e[0], 0x85 | 0x05 | 0xC0 | 0xC1), "{fs:?}"),
                _ => assert!(e[0] == 0 || e[11] == 0x08, "{fs:?}"),
            }
        }
        for &c in &kept {
            let at = b.cluster_offset(c) as usize;
            assert_eq!(q[at..at + cl], v[at..at + cl], "{fs:?} cluster {c}");
        }
        let fat_at = match fs {
            FsType::Exfat => u32_at(&q, 80) as usize * 512,
            _ => 32 * 512,
        };
        for &c in &kept {
            assert_eq!(u32_at(&q, fat_at + 4 * c as usize), 0, "{fs:?}");
        }
        assert!(u32_at(&q, fat_at + 4 * root as usize) >= 0x0FFF_FFF8);
        assert_eq!(q[..512], v[..512]);
        if fs == FsType::Exfat {
            assert_eq!(q[12 * 512..24 * 512], q[..12 * 512]);
            let bitmap = b.cluster_offset(2) as usize;
            // Bitmap (cluster 2), up-case table, root: the first three clusters only.
            assert_eq!(q[bitmap], 0b111);
            assert!(q[bitmap + 1..bitmap + 64].iter().all(|&x| x == 0));
        }
    }
}

#[test]
fn disk_layouts() {
    let vol = FsBuilder::new(FsType::Fat32, 48 << 20, 512).build();
    let len = (48 << 20) + (8 << 20);

    let d = disk(Partitioning::Mbr { first_lba: 2048 }, &vol, len);
    assert_eq!(d.len(), len);
    assert_eq!(d[446 + 4], 0x0C);
    assert_eq!(u32_at(&d, 446 + 8), 2048);
    assert_eq!(u32_at(&d, 446 + 12), (48 << 20) / 512);
    assert_eq!(d[510..512], [0x55, 0xAA]);
    assert_eq!(d[1 << 20..(1 << 20) + vol.len()], vol[..]);
    assert!(d[462..510].iter().all(|&x| x == 0));

    let exfat = FsBuilder::new(FsType::Exfat, 16 << 20, 32 << 10).build();
    let fat16 = FsBuilder::new(FsType::Fat16, 16 << 20, 2048).build();
    let fat12 = FsBuilder::new(FsType::Fat12, 2 << 20, 1024).build();
    for (v, t) in [(&exfat, 0x07), (&fat16, 0x0E), (&fat12, 0x01)] {
        let d = disk(Partitioning::Mbr { first_lba: 63 }, v, 20 << 20);
        assert_eq!(d[446 + 4], t);
    }

    let d = disk(Partitioning::Gpt { first_lba: 2048 }, &vol, len);
    let last = (len / 512 - 1) as u64;
    assert_eq!(d[446 + 4], 0xEE);
    assert_eq!(u32_at(&d, 446 + 8), 1);
    assert_eq!(&d[512..520], b"EFI PART");
    let header = |at: usize| {
        let mut h = d[at..at + 92].to_vec();
        h[16..20].fill(0);
        assert_eq!(u32_at(&d, at + 16), crc32(&h));
        assert_eq!(u32_at(&d, at + 12), 92);
        assert_eq!(u32_at(&d, at + 80), 128);
        assert_eq!(u32_at(&d, at + 84), 128);
    };
    header(512);
    assert_eq!(u64_at(&d, 512 + 24), 1);
    assert_eq!(u64_at(&d, 512 + 32), last);
    assert_eq!(u64_at(&d, 512 + 72), 2);
    assert_eq!(u32_at(&d, 512 + 88), crc32(&d[1024..1024 + 128 * 128]));
    let entry = &d[1024..1024 + 128];
    assert_eq!(
        entry[..16],
        [
            0xA2, 0xA0, 0xD0, 0xEB, 0xE5, 0xB9, 0x33, 0x44, 0x87, 0xC0, 0x68, 0xB6, 0xB7, 0x26,
            0x99, 0xC7
        ]
    );
    assert_eq!(u64_at(entry, 32), 2048);
    assert_eq!(u64_at(entry, 40), 2048 + (48 << 20) / 512 - 1);
    assert_eq!(d[1 << 20..(1 << 20) + vol.len()], vol[..]);
    let backup = last as usize * 512;
    assert_eq!(&d[backup..backup + 8], b"EFI PART");
    header(backup);
    assert_eq!(u64_at(&d, backup + 24), last);
    assert_eq!(u64_at(&d, backup + 32), 1);
    let entries_lba = u64_at(&d, backup + 72) as usize;
    assert_eq!(entries_lba as u64, last - 32);
    assert_eq!(
        d[entries_lba * 512..entries_lba * 512 + 128 * 128],
        d[1024..1024 + 128 * 128]
    );

    let d = disk(Partitioning::Superfloppy, &vol, len);
    assert_eq!(d[..vol.len()], vol[..]);
    assert!(d[vol.len()..].iter().all(|&x| x == 0));

    assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    assert_eq!(crc32(b""), 0);
}

#[test]
fn extended_partitions_chain_their_ebrs() {
    let fat32 = FsBuilder::new(FsType::Fat32, 48 << 20, 512).build();
    let exfat = FsBuilder::new(FsType::Exfat, 16 << 20, 32 << 10).build();
    let a = FsBuilder::new(FsType::Fat16, 16 << 20, 2048).build();
    let fat32_s = (fat32.len() / 512) as u32;
    let exfat_at = 2048 + fat32_s;
    let a_at = exfat_at + (exfat.len() / 512) as u32 + 2048;
    let b_at = a_at + (a.len() / 512) as u32 + 2048;
    let len = (b_at as usize + a.len() / 512 + 2048) * 512;
    let d = mbr_disk(
        &[(2048, &fat32, 0x0C), (exfat_at, &exfat, 0x07)],
        &[(a_at, &a, 0x0E), (b_at, &a, 0x0E)],
        len,
    );
    assert_eq!(d[510..512], [0x55, 0xAA]);
    assert_eq!((d[446 + 4], u32_at(&d, 446 + 8)), (0x0C, 2048));
    assert_eq!((d[462 + 4], u32_at(&d, 462 + 8)), (0x07, exfat_at));
    assert_eq!(d[478 + 4], 0x0F);
    let ext = u32_at(&d, 478 + 8);
    assert_eq!(
        ext,
        a_at - 1,
        "the first EBR is the sector before its volume"
    );
    assert_eq!(u32_at(&d, 478 + 12), b_at + (a.len() / 512) as u32 - ext);
    let ebr1 = ext as usize * 512;
    assert_eq!(d[ebr1 + 510..ebr1 + 512], [0x55, 0xAA]);
    assert_eq!((d[ebr1 + 446 + 4], u32_at(&d, ebr1 + 446 + 8)), (0x0E, 1));
    assert_eq!(u32_at(&d, ebr1 + 446 + 12), (a.len() / 512) as u32);
    assert_eq!(d[ebr1 + 462 + 4], 0x05);
    let next = ext + u32_at(&d, ebr1 + 462 + 8);
    assert_eq!(next, b_at - 1);
    let ebr2 = next as usize * 512;
    assert_eq!((d[ebr2 + 446 + 4], u32_at(&d, ebr2 + 446 + 8)), (0x0E, 1));
    assert_eq!(d[ebr2 + 462 + 4], 0, "end of the chain");
    assert_eq!(d[2048 * 512..2048 * 512 + fat32.len()], fat32[..]);
    assert_eq!(d[a_at as usize * 512..][..a.len()], a[..]);
    assert_eq!(d[b_at as usize * 512..][..a.len()], a[..]);
}
