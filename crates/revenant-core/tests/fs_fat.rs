//! The live tree of FAT12/16/32 volumes: names, folders, times, cluster chains, the FAT mirror and hostile volumes.

use std::io::Write;
use std::time::{Duration, Instant};

use revenant_core::EngineError;
use revenant_core::filesystem::{
    FsFile, FsState, FsTime, Listing, MAX_PATH_BYTES, Placement, read,
};
use revenant_core::formats::{FileKind, Fragment};
use revenant_core::scan::CancelToken;
use revenant_core::source::{FileSource, Source, VolumeId};
use revenant_testkit::fs::{FsBuilder, FsStamp, FsType, Partitioning, disk};
use revenant_testkit::{Mp4Options, TestExif, TiffRawOptions, TiffVariant, jpeg, mp4, tiff_raw};

const MIB: usize = 1 << 20;
/// Where every test volume starts (MBR entry at LBA 8192).
const VOL: u64 = 8192 * 512;

/// FAT32, 48 MiB, 512-byte clusters.
fn card() -> FsBuilder {
    FsBuilder::new(FsType::Fat32, 48 * MIB as u64, 512)
}

/// The volume of `b` at LBA 8192 of a disk 8 MiB longer than the volume.
fn placed(b: &FsBuilder) -> Vec<u8> {
    let v = b.build();
    let len = v.len() + 8 * MIB;
    disk(Partitioning::Mbr { first_lba: 8192 }, &v, len)
}

fn source(bytes: &[u8]) -> (tempfile::NamedTempFile, FileSource) {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(bytes).unwrap();
    file.flush().unwrap();
    let src = FileSource::open(file.path()).unwrap();
    (file, src)
}

fn listing(bytes: &[u8]) -> Listing {
    let (_file, src) = source(bytes);
    read(&src, &CancelToken::default())
}

fn file<'a>(l: &'a Listing, path: &str) -> &'a FsFile {
    let found: Vec<_> = l.files.iter().filter(|f| f.origin.path() == path).collect();
    assert_eq!(found.len(), 1, "{path} in {:?}", paths(l));
    found[0]
}

fn paths(l: &Listing) -> Vec<String> {
    l.files.iter().map(|f| f.origin.path()).collect()
}

/// Absolute pieces of `clusters` (file order) for the first `size` bytes, consecutive clusters joined.
fn pieces_of(b: &FsBuilder, clusters: &[u32], size: u64, cluster: u64) -> Vec<Fragment> {
    let mut out: Vec<Fragment> = Vec::new();
    let mut left = size;
    for &c in clusters {
        let len = cluster.min(left);
        if len == 0 {
            break;
        }
        left -= len;
        let start = VOL + b.cluster_offset(c);
        match out.last_mut() {
            Some(p) if p.start + p.len == start => p.len += len,
            _ => out.push(Fragment { start, len }),
        }
    }
    out
}

fn covered(live: &[Fragment], p: &Fragment) -> bool {
    live.iter()
        .any(|r| r.start <= p.start && p.start + p.len <= r.start + r.len)
}

fn overlaps(live: &[Fragment], start: u64, len: u64) -> bool {
    live.iter()
        .any(|r| r.start < start + len && start < r.start + r.len)
}

fn stamp(year: u16, month: u8, day: u8, hour: u8, min: u8, sec: u8) -> FsStamp {
    FsStamp {
        year,
        month,
        day,
        hour,
        min,
        sec,
        utc_offset_min: None,
    }
}

fn arw() -> Vec<u8> {
    tiff_raw(&TiffRawOptions {
        variant: TiffVariant::Arw,
        big_endian: false,
        raw_len: 200_000,
        seed: 11,
        preview: Some((160, 120)),
        preview_sector_aligned: false,
        exif: TestExif::default(),
    })
    .bytes
}

fn clip() -> Vec<u8> {
    mp4(&Mp4Options {
        mdat_len: 300_000,
        moov_at_end: true,
        mov: false,
        mdat_64bit: false,
    })
}

/// A 32-byte short entry.
fn short_entry(name: &[u8; 11], attr: u8, cluster: u32, size: u32) -> [u8; 32] {
    let mut e = [0u8; 32];
    e[..11].copy_from_slice(name);
    e[11] = attr;
    e[20..22].copy_from_slice(&((cluster >> 16) as u16).to_le_bytes());
    e[26..28].copy_from_slice(&(cluster as u16).to_le_bytes());
    e[28..32].copy_from_slice(&size.to_le_bytes());
    e
}

fn sfn_checksum(name: &[u8; 11]) -> u8 {
    name.iter()
        .fold(0u8, |s, &c| s.rotate_right(1).wrapping_add(c))
}

/// The long-name entry `ord` (1-based, `last` adds 0x40) holding `units` (13, padded by the caller).
fn lfn_entry(ord: u8, last: bool, sum: u8, units: &[u16; 13]) -> [u8; 32] {
    const OFFS: [usize; 13] = [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30];
    let mut e = [0u8; 32];
    e[0] = ord | if last { 0x40 } else { 0 };
    e[11] = 0x0F;
    e[13] = sum;
    for (k, &off) in OFFS.iter().enumerate() {
        e[off..off + 2].copy_from_slice(&units[k].to_le_bytes());
    }
    e
}

/// The 13 units of part `ord` of `name` (NUL then 0xFFFF padding).
fn lfn_units(name: &str, ord: usize) -> [u16; 13] {
    let mut all: Vec<u16> = name.encode_utf16().collect();
    let n = all.len().div_ceil(13);
    if all.len() < n * 13 {
        all.push(0);
        all.resize(n * 13, 0xFFFF);
    }
    all[(ord - 1) * 13..ord * 13].try_into().unwrap()
}

const EOC32: u32 = 0x0FFF_FFFF;

#[test]
fn lists_live_files_with_paths_names_and_times() {
    let mut b = card();
    let raw = arw();
    let photo = jpeg(640, 480, 3, 0);
    let video = clip();
    let notes = b"camera notes\n".repeat(100);
    let long = jpeg(320, 240, 4, 0);
    let arw_clusters = b.add("DCIM/100MSDCF/DSC01234.ARW", &raw);
    b.add("DCIM/100MSDCF/DSC01235.JPG", &photo);
    b.add("PRIVATE/M4ROOT/CLIP/C0001.MP4", &video);
    let notes_clusters = b.add("MISC/notes.txt", &notes);
    b.add("DCIM/100MSDCF/Long name photo.jpg", &long);
    b.stamp(
        "DCIM/100MSDCF/DSC01234.ARW",
        stamp(2025, 12, 14, 7, 12, 4),
        stamp(2025, 12, 14, 7, 12, 6),
    );
    let l = listing(&placed(&b));

    assert_eq!(l.volumes.len(), 1);
    assert_eq!(l.files.len(), 4, "{:?}", paths(&l));
    let starts: Vec<u64> = l.files.iter().map(|f| f.pieces[0].start).collect();
    assert!(starts.is_sorted(), "{starts:?}");

    let a = file(&l, "DCIM/100MSDCF/DSC01234.ARW");
    assert_eq!(a.origin.name(), "DSC01234.ARW");
    assert_eq!(a.origin.folder(), Some("DCIM/100MSDCF"));
    assert_eq!(a.origin.volume, 0);
    assert_eq!(a.origin.state, FsState::Live);
    assert_eq!(a.origin.placement, Placement::Chain);
    assert_eq!(a.origin.size, raw.len() as u64);
    assert_eq!(a.kind, FileKind::Arw);
    assert_eq!(
        a.pieces,
        vec![Fragment {
            start: VOL + b.cluster_offset(arw_clusters[0]),
            len: raw.len() as u64
        }]
    );
    assert!(a.trusted);
    assert_eq!(
        a.origin.created,
        Some(FsTime {
            local: "2025-12-14T07:12:04".into(),
            utc_offset_minutes: None
        })
    );
    assert_eq!(
        a.origin.modified,
        Some(FsTime {
            local: "2025-12-14T07:12:06".into(),
            utc_offset_minutes: None
        })
    );

    let j = file(&l, "DCIM/100MSDCF/DSC01235.JPG");
    assert_eq!(
        (j.kind, j.origin.size),
        (FileKind::Jpeg, photo.len() as u64)
    );
    let m = file(&l, "PRIVATE/M4ROOT/CLIP/C0001.MP4");
    assert_eq!((m.kind, m.origin.size), (FileKind::Mp4, video.len() as u64));
    assert_eq!(m.origin.folder(), Some("PRIVATE/M4ROOT/CLIP"));
    let n = file(&l, "DCIM/100MSDCF/Long name photo.jpg");
    assert_eq!(n.origin.name(), "Long name photo.jpg");
    assert_eq!(n.kind, FileKind::Jpeg);
    assert!(l.files.iter().all(|f| f.trusted));
    assert!(!paths(&l).iter().any(|p| p.contains("notes")));

    let notes_pieces = pieces_of(&b, &notes_clusters, notes.len() as u64, 512);
    for p in l.files.iter().flat_map(|f| &f.pieces).chain(&notes_pieces) {
        assert!(covered(&l.live, p), "{p:?} not in live");
    }
    assert_eq!(l.others, vec![notes_pieces[0]]);
    // The directories' clusters are live too (the FAT32 root among them).
    for dir in ["", "DCIM", "DCIM/100MSDCF", "MISC"] {
        let c = b.clusters(dir)[0];
        assert!(overlaps(&l.live, VOL + b.cluster_offset(c), 512), "{dir:?}");
    }
    // Merged: sorted, no two touching.
    assert!(
        l.live
            .windows(2)
            .all(|w| w[0].start + w[0].len < w[1].start)
    );
}

/// Absolute offset and length of the fixed root of the FAT12/16 volume `v` placed at `VOL`.
fn fixed_root(v: &[u8]) -> (u64, u64) {
    let u16_at = |at: usize| u16::from_le_bytes([v[at], v[at + 1]]) as u64;
    let bps = u16_at(11);
    let start = u16_at(14) + v[16] as u64 * u16_at(22);
    (VOL + start * bps, u16_at(17) * 32)
}

#[test]
fn fat12_and_fat16_roots_are_read() {
    for (fs, len, cluster) in [
        (FsType::Fat12, 2 * MIB as u64, 1024u32),
        (FsType::Fat16, 16 * MIB as u64, 2048),
    ] {
        let mut b = FsBuilder::new(fs, len, cluster);
        let raw = arw();
        let photo = jpeg(640, 480, 3, 0);
        let video = clip();
        let arw_clusters = b.add("DCIM/100MSDCF/DSC01234.ARW", &raw);
        b.add("DCIM/100MSDCF/DSC01235.JPG", &photo);
        b.add("PRIVATE/M4ROOT/CLIP/C0001.MP4", &video);
        b.add("ROOT.JPG", &photo);
        let v = b.build();
        let l = listing(&disk(
            Partitioning::Mbr { first_lba: 8192 },
            &v,
            v.len() + 8 * MIB,
        ));
        let mut got = paths(&l);
        got.sort();
        assert_eq!(
            got,
            [
                "DCIM/100MSDCF/DSC01234.ARW",
                "DCIM/100MSDCF/DSC01235.JPG",
                "PRIVATE/M4ROOT/CLIP/C0001.MP4",
                "ROOT.JPG"
            ],
            "{fs:?}"
        );
        let a = file(&l, "DCIM/100MSDCF/DSC01234.ARW");
        assert_eq!(
            a.pieces,
            pieces_of(&b, &arw_clusters, raw.len() as u64, cluster as u64),
            "{fs:?}"
        );
        assert!(l.files.iter().all(|f| f.trusted), "{fs:?}");
        assert_eq!(file(&l, "ROOT.JPG").origin.folder(), Some(""));
        // Ruling C10: the fixed root region is not in `live`; the directories in the heap are.
        let (root, root_len) = fixed_root(&v);
        assert!(!overlaps(&l.live, root, root_len), "{fs:?}");
        let dcim = b.clusters("DCIM")[0];
        assert!(overlaps(&l.live, VOL + b.cluster_offset(dcim), 1), "{fs:?}");
    }
}

/// A JPEG of exactly `len` bytes.
fn jpeg_of_len(len: usize, seed: u64) -> Vec<u8> {
    let base = jpeg(64, 64, seed, 0).len();
    let j = jpeg(64, 64, seed, len - base - 4);
    assert_eq!(j.len(), len);
    j
}

/// `len` bytes of `fill` (content does not matter to the listing).
fn bytes(len: usize, fill: u8) -> Vec<u8> {
    vec![fill; len]
}

#[test]
fn fragmented_live_file_follows_its_chain() {
    let mut b = card();
    let data = jpeg_of_len(40_000, 7);
    // 79 clusters: 40 + 30 + 9.
    let clusters: Vec<u32> = (600..640).chain(300..330).chain(900..909).collect();
    b.add_at("DCIM/100/FRAG.JPG", &data, &clusters);
    let l = listing(&placed(&b));
    let f = file(&l, "DCIM/100/FRAG.JPG");
    let at = |c: u32| VOL + b.cluster_offset(c);
    assert_eq!(
        f.pieces,
        vec![
            Fragment {
                start: at(600),
                len: 40 * 512
            },
            Fragment {
                start: at(300),
                len: 30 * 512
            },
            Fragment {
                start: at(900),
                len: 40_000 - 70 * 512
            },
        ]
    );
    assert_eq!(f.pieces.iter().map(|p| p.len).sum::<u64>(), 40_000);
    assert!(f.trusted);
}

#[test]
fn hostile_directory_loop_and_chain_cycle_are_bounded() {
    let mut b = card();
    let a = b.add("DCIM/100/A.JPG", &bytes(5 * 512, 1));
    let bb = b.add("DCIM/100/B.JPG", &bytes(4 * 512, 2));
    let c = b.add("DCIM/100/C.JPG", &bytes(4 * 512, 3));
    b.add("DCIM/100/OK.JPG", &bytes(3 * 512, 4));
    let root = b.clusters("")[0];
    let own = b.clusters("DCIM/100")[0];
    b.raw_entry("DCIM/100", short_entry(b"LOOP       ", 0x10, root, 0));
    b.raw_entry("DCIM/100", short_entry(b"SELF       ", 0x10, own, 0));
    b.set_fat(a[2], a[1]);
    b.set_fat(c[c.len() - 2], bb[bb.len() - 1]);
    // DCIM/BIG: 70,000 entries, more than the 2 MiB (65,536 entries) a FAT directory may have.
    b.add("DCIM/BIG/EARLY.JPG", &bytes(1024, 5));
    for i in 0..69_996u32 {
        let name: [u8; 11] = format!("Z{i:07}TXT").as_bytes().try_into().unwrap();
        b.raw_entry("DCIM/BIG", short_entry(&name, 0x20, 0, 0));
    }
    b.add("DCIM/BIG/LATE.JPG", &bytes(1024, 6));
    let bytes = placed(&b);

    let t = Instant::now();
    let l = listing(&bytes);
    assert!(
        t.elapsed() < Duration::from_secs(5),
        "took {:?}",
        t.elapsed()
    );

    assert!(
        !l.files
            .iter()
            .any(|f| f.origin.path().contains("LOOP/") || f.origin.path().contains("SELF/")),
        "{:?}",
        paths(&l)
    );
    let mut in_100: Vec<String> = paths(&l)
        .into_iter()
        .filter(|p| p.starts_with("DCIM/100/"))
        .collect();
    in_100.sort();
    assert_eq!(
        in_100,
        [
            "DCIM/100/A.JPG",
            "DCIM/100/B.JPG",
            "DCIM/100/C.JPG",
            "DCIM/100/OK.JPG"
        ]
    );
    for p in ["DCIM/100/A.JPG", "DCIM/100/B.JPG", "DCIM/100/C.JPG"] {
        assert!(!file(&l, p).trusted, "{p}");
    }
    assert!(file(&l, "DCIM/100/OK.JPG").trusted);
    // The cycle: A stops where it loops back.
    assert_eq!(
        file(&l, "DCIM/100/A.JPG").pieces,
        pieces_of(&b, &a[..3], 3 * 512, 512)
    );
    // The loop entries are not a cross-link: the root and DCIM/100 stay trusted (live).
    assert!(overlaps(&l.live, VOL + b.cluster_offset(root), 512));
    assert!(overlaps(&l.live, VOL + b.cluster_offset(own), 512));
    // DCIM/BIG is parsed up to its 2 MiB cap only, and is not trusted.
    let big = b.clusters("DCIM/BIG");
    assert!(!overlaps(&l.live, VOL + b.cluster_offset(big[0]), 512));
    assert!(paths(&l).iter().any(|p| p == "DCIM/BIG/EARLY.JPG"));
    assert!(!paths(&l).iter().any(|p| p == "DCIM/BIG/LATE.JPG"));
}

/// A source whose reads fail when they touch any of `bad` (absolute byte ranges).
struct Failing {
    inner: FileSource,
    bad: Vec<(u64, u64)>,
}

impl Source for Failing {
    fn len(&self) -> u64 {
        self.inner.len()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, EngineError> {
        let end = offset + buf.len() as u64;
        if self.bad.iter().any(|&(s, e)| offset < e && s < end) {
            return Err(EngineError::Read {
                offset,
                cause: std::io::Error::other("bad sector"),
            });
        }
        self.inner.read_at(offset, buf)
    }

    fn volume(&self) -> Option<VolumeId> {
        None
    }
}

#[test]
fn fat_mirror_covers_an_unreadable_fat_sector() {
    let mut b = card();
    let raw = arw();
    let clusters = b.add("DCIM/100MSDCF/DSC01234.ARW", &raw);
    let bytes = placed(&b);
    let clean = listing(&bytes);
    let want = file(&clean, "DCIM/100MSDCF/DSC01234.ARW").clone();
    assert!(want.trusted);

    // The builder's FAT32: 32 reserved sectors, then two FATs of FATSz32 sectors.
    let fat_len = u32::from_le_bytes(
        bytes[VOL as usize + 36..VOL as usize + 40]
            .try_into()
            .unwrap(),
    ) as u64
        * 512;
    let fat0 = VOL + 32 * 512;
    let first = 4 * *clusters.first().unwrap() as u64 / 512 * 512;
    let last = (4 * *clusters.last().unwrap() as u64 / 512 + 1) * 512;
    let (_file, inner) = source(&bytes);
    let src = Failing {
        inner,
        bad: vec![(fat0 + first, fat0 + last)],
    };
    let l = read(&src, &CancelToken::default());
    assert_eq!(file(&l, "DCIM/100MSDCF/DSC01234.ARW"), &want);

    // Both copies unreadable there: the chain cannot be followed, so the file is not trusted.
    let (_file, inner) = source(&bytes);
    let src = Failing {
        inner,
        bad: vec![
            (fat0 + first, fat0 + last),
            (fat0 + fat_len + first, fat0 + fat_len + last),
        ],
    };
    let l = read(&src, &CancelToken::default());
    assert!(!file(&l, "DCIM/100MSDCF/DSC01234.ARW").trusted);
}

/// A FAT date word.
fn date(y: u16, m: u16, d: u16) -> u16 {
    (y - 1980) << 9 | m << 5 | d
}

/// A FAT time word (`s` in seconds, stored halved).
fn time(h: u16, m: u16, s: u16) -> u16 {
    h << 11 | m << 5 | (s / 2)
}

/// (short name, created date, created time, 10 ms count, modified date, modified time, created valid, modified valid)
type DateCase = (&'static [u8; 11], u16, u16, u8, u16, u16, bool, bool);

#[test]
fn invalid_dates_are_none() {
    let mut b = card();
    b.mkdir("DCIM");
    let cases: [DateCase; 8] = [
        (
            b"MONTH13 JPG",
            date(2025, 13, 14),
            time(25, 0, 0),
            0,
            date(2025, 13, 1),
            time(25, 0, 0),
            false,
            false,
        ),
        (
            b"GOOD    JPG",
            date(2024, 2, 29),
            time(23, 59, 58),
            199,
            date(2025, 1, 1),
            time(0, 0, 0),
            true,
            true,
        ),
        (
            b"FEB30   JPG",
            date(2025, 2, 30),
            time(1, 0, 0),
            0,
            date(2023, 2, 29),
            time(1, 0, 0),
            false,
            false,
        ),
        (
            b"DAY0    JPG",
            date(2025, 1, 0),
            time(1, 0, 0),
            0,
            date(2025, 4, 31),
            time(1, 0, 0),
            false,
            false,
        ),
        (
            b"MIN60   JPG",
            date(2025, 1, 1),
            time(1, 60, 0),
            0,
            date(2025, 1, 1),
            time(1, 1, 60),
            false,
            false,
        ),
        (
            b"TENTHS  JPG",
            date(2025, 1, 1),
            time(1, 0, 0),
            200,
            date(2025, 1, 1),
            0,
            false,
            true,
        ),
        (b"ZERO    JPG", 0, 0, 0, 0, 0, false, false),
        (
            b"MONTH0  JPG",
            date(2025, 0, 1),
            0,
            0,
            date(2025, 12, 31),
            time(23, 59, 58),
            false,
            true,
        ),
    ];
    for (i, &(name, cd, ct, tenths, md, mt, _, _)) in cases.iter().enumerate() {
        let cluster = 5000 + i as u32;
        let mut e = short_entry(name, 0x20, cluster, 512);
        e[13] = tenths;
        e[14..16].copy_from_slice(&ct.to_le_bytes());
        e[16..18].copy_from_slice(&cd.to_le_bytes());
        e[22..24].copy_from_slice(&mt.to_le_bytes());
        e[24..26].copy_from_slice(&md.to_le_bytes());
        b.raw_entry("DCIM", e);
        b.set_fat(cluster, EOC32);
    }
    let l = listing(&placed(&b));
    for &(name, .., created_ok, modified_ok) in &cases {
        let base = std::str::from_utf8(&name[..8]).unwrap().trim_end();
        let f = file(&l, &format!("DCIM/{base}.JPG"));
        assert_eq!(f.origin.created.is_some(), created_ok, "{base} created");
        assert_eq!(f.origin.modified.is_some(), modified_ok, "{base} modified");
    }
    let good = file(&l, "DCIM/GOOD.JPG");
    // 58 s + 199 tenths of 10 ms (1.99 s, floored to 1 s).
    assert_eq!(
        good.origin.created.as_ref().unwrap().local,
        "2024-02-29T23:59:59"
    );
    assert_eq!(
        good.origin.modified.as_ref().unwrap().local,
        "2025-01-01T00:00:00"
    );
    assert_eq!(
        file(&l, "DCIM/MONTH0.JPG")
            .origin
            .modified
            .as_ref()
            .unwrap()
            .local,
        "2025-12-31T23:59:58"
    );
}

#[test]
fn hostile_names_with_slash_and_nul_are_flattened() {
    let mut b = card();
    let photo = jpeg(64, 64, 9, 0);
    let slash: Vec<u16> = "a/b.jpg".encode_utf16().collect();
    b.add_utf16("DCIM", &slash, &photo);
    // A NUL ends a long name.
    let nul: Vec<u16> = "c.jpg\0/x".encode_utf16().collect();
    b.add_utf16("DCIM", &nul, &photo);
    // A short name holding '/'.
    b.raw_entry("DCIM", short_entry(b"D/E     JPG", 0x20, 7000, 512));
    b.set_fat(7000, EOC32);
    let l = listing(&placed(&b));
    let mut got: Vec<(&str, &str)> = l
        .files
        .iter()
        .map(|f| (f.origin.folder().unwrap(), f.origin.name()))
        .collect();
    got.sort();
    assert_eq!(
        got,
        [("DCIM", "D_E.JPG"), ("DCIM", "a_b.jpg"), ("DCIM", "c.jpg")]
    );
}

#[test]
fn short_names_case_bits_and_long_name_checks() {
    let mut b = card();
    b.mkdir("DCIM");
    let mut next = 8000u32;
    let mut entry = |b: &mut FsBuilder, name: &[u8; 11], case: u8| {
        let mut e = short_entry(name, 0x20, next, 512);
        e[12] = case;
        b.set_fat(next, EOC32);
        next += 1;
        e
    };
    let e = entry(&mut b, b"DSC00001JPG", 0x08);
    b.raw_entry("DCIM", e);
    let e = entry(&mut b, b"DSC00002JPG", 0x18);
    b.raw_entry("DCIM", e);
    let e = entry(&mut b, b"DSC00003JPG", 0x10);
    b.raw_entry("DCIM", e);
    // 0x05 stands for a first byte 0xE5, outside printable ASCII.
    let e = entry(&mut b, b"\x05SC00004JPG", 0);
    b.raw_entry("DCIM", e);
    // A good two-part long name.
    let long = "A long photo name.jpg";
    let sfn = *b"ALONGP~1JPG";
    let sum = sfn_checksum(&sfn);
    b.raw_entry("DCIM", lfn_entry(2, true, sum, &lfn_units(long, 2)));
    b.raw_entry("DCIM", lfn_entry(1, false, sum, &lfn_units(long, 1)));
    let e = entry(&mut b, &sfn, 0);
    b.raw_entry("DCIM", e);
    // A checksum that does not match: the short name is used.
    let sfn = *b"BLONGP~1JPG";
    b.raw_entry(
        "DCIM",
        lfn_entry(2, true, 0x55, &lfn_units("B long photo name.jpg", 2)),
    );
    b.raw_entry(
        "DCIM",
        lfn_entry(1, false, 0x55, &lfn_units("B long photo name.jpg", 1)),
    );
    let e = entry(&mut b, &sfn, 0);
    b.raw_entry("DCIM", e);
    // Ordinals that do not run down to 1 (part 1 missing): the short name is used.
    let sfn = *b"CLONGP~1JPG";
    let sum = sfn_checksum(&sfn);
    b.raw_entry(
        "DCIM",
        lfn_entry(2, true, sum, &lfn_units("C long photo name.jpg", 2)),
    );
    let e = entry(&mut b, &sfn, 0);
    b.raw_entry("DCIM", e);
    // Parts out of order: the short name is used.
    let sfn = *b"DLONGP~1JPG";
    let sum = sfn_checksum(&sfn);
    b.raw_entry(
        "DCIM",
        lfn_entry(1, true, sum, &lfn_units("D long photo name.jpg", 1)),
    );
    b.raw_entry(
        "DCIM",
        lfn_entry(2, false, sum, &lfn_units("D long photo name.jpg", 2)),
    );
    let e = entry(&mut b, &sfn, 0);
    b.raw_entry("DCIM", e);
    // An ordinal skipped (3, then 1): the short name is used.
    let sfn = *b"FLONGP~1JPG";
    let sum = sfn_checksum(&sfn);
    let f = "F long photo name with three parts.jpg";
    b.raw_entry("DCIM", lfn_entry(3, true, sum, &lfn_units(f, 3)));
    b.raw_entry("DCIM", lfn_entry(1, false, sum, &lfn_units(f, 1)));
    let e = entry(&mut b, &sfn, 0);
    b.raw_entry("DCIM", e);
    // A long name followed by a volume label, then a short entry: the label breaks the sequence.
    let sfn = *b"ELONGP~1JPG";
    let sum = sfn_checksum(&sfn);
    b.raw_entry(
        "DCIM",
        lfn_entry(2, true, sum, &lfn_units("E long photo name.jpg", 2)),
    );
    b.raw_entry(
        "DCIM",
        lfn_entry(1, false, sum, &lfn_units("E long photo name.jpg", 1)),
    );
    // (A label entry with a chain and a size, still not a file.)
    let mut label = entry(&mut b, b"LABEL   JPG", 0);
    label[11] = 0x08;
    b.raw_entry("DCIM", label);
    let e = entry(&mut b, &sfn, 0);
    b.raw_entry("DCIM", e);
    // A deleted entry and the end-of-directory marker: nothing after it is read.
    let mut gone = entry(&mut b, b"GONE    JPG", 0);
    gone[0] = 0xE5;
    b.raw_entry("DCIM", gone);
    b.raw_entry("DCIM", [0u8; 32]);
    let e = entry(&mut b, b"AFTER   JPG", 0);
    b.raw_entry("DCIM", e);

    let l = listing(&placed(&b));
    let mut got: Vec<&str> = l.files.iter().map(|f| f.origin.name()).collect();
    got.sort();
    let mut want = [
        "A long photo name.jpg",
        "BLONGP~1.JPG",
        "CLONGP~1.JPG",
        "DLONGP~1.JPG",
        "ELONGP~1.JPG",
        "FLONGP~1.JPG",
        "_SC00004.JPG",
        "dsc00001.JPG",
        "dsc00002.jpg",
        "DSC00003.jpg",
    ];
    want.sort();
    assert_eq!(got, want);
}

#[test]
fn directories_are_trusted_only_with_a_complete_chain() {
    let mut b = card();
    let photo = bytes(512, 1);
    // 20 files: two clusters of entries (16 per cluster) in each directory.
    for dir in ["GOOD", "FREED", "CYCLE", "CROSS"] {
        for i in 0..20 {
            b.add(&format!("DCIM/{dir}/F{i:02}.JPG"), &photo);
        }
    }
    // Zeros: read as a directory cluster, it ends the directory at once.
    let other = b.add("DCIM/OTHER.JPG", &bytes(2 * 512, 0));
    let good = b.clusters("DCIM/GOOD");
    let freed = b.clusters("DCIM/FREED");
    let cycle = b.clusters("DCIM/CYCLE");
    let cross = b.clusters("DCIM/CROSS");
    assert!(good.len() == 2 && freed.len() == 2 && cycle.len() == 2 && cross.len() == 2);
    b.set_fat(freed[1], 0);
    b.set_fat(cycle[1], cycle[0]);
    b.set_fat(cross[1], other[1]);
    let l = listing(&placed(&b));
    let at = |c: u32| VOL + b.cluster_offset(c);

    assert!(overlaps(&l.live, at(good[0]), 512) && overlaps(&l.live, at(good[1]), 512));
    for (dir, chain) in [("FREED", &freed), ("CYCLE", &cycle), ("CROSS", &cross)] {
        assert!(!overlaps(&l.live, at(chain[0]), 512), "{dir}");
        assert!(!overlaps(&l.live, at(chain[1]), 512), "{dir}");
        // Still walked: every file of both clusters is listed.
        let n = paths(&l)
            .iter()
            .filter(|p| p.starts_with(&format!("DCIM/{dir}/")))
            .count();
        assert_eq!(n, 20, "{dir}");
    }
    // OTHER.JPG shares a cluster with CROSS's chain.
    assert!(!file(&l, "DCIM/OTHER.JPG").trusted);
    assert!(file(&l, "DCIM/GOOD/F00.JPG").trusted);
    // The root and DCIM end cleanly.
    assert!(overlaps(&l.live, at(b.clusters("")[0]), 512));
    assert!(overlaps(&l.live, at(b.clusters("DCIM")[0]), 512));
}

#[test]
fn cancelled_read_lists_volumes_only() {
    let mut b = card();
    b.add("DCIM/A.JPG", &jpeg(64, 64, 1, 0));
    let (_file, src) = source(&placed(&b));
    let cancel = CancelToken::default();
    cancel.cancel();
    let l = read(&src, &cancel);
    assert_eq!(l.volumes.len(), 1);
    assert!(l.files.is_empty() && l.live.is_empty() && l.others.is_empty());
}

#[test]
fn pieces_past_the_source_end_are_dropped() {
    let mut b = card();
    let photo = bytes(3 * 512, 3);
    let near = b.add("DCIM/A.JPG", &photo);
    let far: Vec<u32> = (90_000..90_003).collect();
    b.add_at("DCIM/FAR.JPG", &photo, &far);
    let mut bytes = placed(&b);
    // The source ends inside FAR's second cluster.
    bytes.truncate((VOL + b.cluster_offset(far[1]) + 100) as usize);
    let l = listing(&bytes);
    assert!(file(&l, "DCIM/A.JPG").trusted);
    assert_eq!(
        file(&l, "DCIM/A.JPG").pieces,
        pieces_of(&b, &near, 1536, 512)
    );
    let f = file(&l, "DCIM/FAR.JPG");
    assert_eq!(
        f.pieces,
        vec![Fragment {
            start: VOL + b.cluster_offset(far[0]),
            len: 612
        }]
    );
    assert!(l.live.iter().all(|r| r.start + r.len <= bytes.len() as u64));
}

#[test]
fn deep_hostile_paths_keep_the_name_and_drop_the_folder() {
    // Ruling R6's probe: 32 nested directories named with 255 U+FFFD (765 UTF-8 bytes each).
    let mut b = card();
    let part = "\u{FFFD}".repeat(255);
    let mut folder = String::new();
    for depth in 1..=32 {
        if !folder.is_empty() {
            folder.push('/');
        }
        folder.push_str(&part);
        if matches!(depth, 5 | 6 | 32) {
            b.add(&format!("{folder}/A.JPG"), &bytes(512, depth as u8));
            b.add(&format!("{folder}/B.JPG"), &bytes(512, depth as u8));
        }
    }
    let l = listing(&placed(&b));
    assert_eq!(
        l.files.len(),
        6,
        "{:?}",
        l.files.iter().map(|f| f.origin.name()).collect::<Vec<_>>()
    );
    assert!(l.truncated);
    for f in &l.files {
        assert!(matches!(f.origin.name(), "A.JPG" | "B.JPG"));
        let path = f.origin.path();
        assert!(path.len() <= MAX_PATH_BYTES, "{} bytes", path.len());
    }
    // Depth 5: 5 × 765 + 4 separators + "/A.JPG" = 3,835 bytes, kept; depth 6 and 32: past 4,096, folder dropped.
    let kept: Vec<&FsFile> = l
        .files
        .iter()
        .filter(|f| f.origin.folder().is_some())
        .collect();
    assert_eq!(kept.len(), 2);
    let want = [part.as_str(); 5].join("/");
    assert!(
        kept.iter()
            .all(|f| f.origin.folder() == Some(want.as_str()))
    );
    // The files of one folder share its string.
    let (x, y) = (&kept[0].origin.folder, &kept[1].origin.folder);
    assert!(std::sync::Arc::ptr_eq(
        x.as_ref().unwrap(),
        y.as_ref().unwrap()
    ));
    // A clean card is not truncated.
    let mut b = card();
    b.add("DCIM/A.JPG", &bytes(512, 1));
    assert!(!listing(&placed(&b)).truncated);
}
