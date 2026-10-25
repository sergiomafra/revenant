//! The tree of exFAT volumes: names, folders, times with their UTC offsets, NoFatChain runs and FAT chains, the
//! allocation bitmap, deleted files and folders, broken entry sets and hostile volumes.

use std::io::Write;
use std::time::{Duration, Instant};

use revenant_core::filesystem::{FsFile, FsState, FsTime, Listing, Placement, read};
use revenant_core::formats::{FileKind, Fragment};
use revenant_core::scan::CancelToken;
use revenant_core::source::FileSource;
use revenant_testkit::fs::{DeleteStyle, FsBuilder, FsStamp, FsType, Partitioning, disk};
use revenant_testkit::{Mp4Options, TestExif, TiffRawOptions, TiffVariant, jpeg, mp4, tiff_raw};

const MIB: usize = 1 << 20;
const CLUSTER: u64 = 32 * 1024;
/// Where every test volume starts (MBR entry at LBA 8192).
const VOL: u64 = 8192 * 512;

/// exFAT, 16 MiB, 32 KiB clusters.
fn card() -> FsBuilder {
    FsBuilder::new(FsType::Exfat, 16 * MIB as u64, CLUSTER as u32)
}

/// The volume of `b` with `patch` applied to its bytes, at LBA 8192 of a disk 8 MiB longer than the volume.
fn placed_with(b: &FsBuilder, patch: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let mut v = b.build();
    patch(&mut v);
    let len = v.len() + 8 * MIB;
    disk(Partitioning::Mbr { first_lba: 8192 }, &v, len)
}

fn placed(b: &FsBuilder) -> Vec<u8> {
    placed_with(b, |_| {})
}

fn listing(bytes: &[u8]) -> Listing {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(bytes).unwrap();
    file.flush().unwrap();
    let src = FileSource::open(file.path()).unwrap();
    read(&src, &CancelToken::default())
}

fn paths(l: &Listing) -> Vec<String> {
    let mut p: Vec<String> = l.files.iter().map(|f| f.origin.path()).collect();
    p.sort();
    p
}

fn file<'a>(l: &'a Listing, path: &str) -> &'a FsFile {
    let found: Vec<_> = l.files.iter().filter(|f| f.origin.path() == path).collect();
    assert_eq!(found.len(), 1, "{path} in {:?}", paths(l));
    found[0]
}

fn deleted(l: &Listing) -> Vec<&FsFile> {
    l.files
        .iter()
        .filter(|f| f.origin.state == FsState::Deleted)
        .collect()
}

/// Absolute pieces of `clusters` (file order) for the first `size` bytes, consecutive clusters joined.
fn pieces_of(b: &FsBuilder, clusters: &[u32], size: u64) -> Vec<Fragment> {
    let mut out: Vec<Fragment> = Vec::new();
    let mut left = size;
    for &c in clusters {
        let len = CLUSTER.min(left);
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

fn overlaps(live: &[Fragment], start: u64, len: u64) -> bool {
    live.iter()
        .any(|r| r.start < start + len && start < r.start + r.len)
}

fn bytes(len: usize, fill: u8) -> Vec<u8> {
    vec![fill; len]
}

fn stamp(hour: u8, min: u8, sec: u8, offset: Option<i16>) -> FsStamp {
    FsStamp {
        year: 2025,
        month: 12,
        day: 14,
        hour,
        min,
        sec,
        utc_offset_min: offset,
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

// ---- Raw entry sets ----

fn sum16(sum: u16, byte: u8) -> u16 {
    sum.rotate_right(1).wrapping_add(byte as u16)
}

/// The SetChecksum of `set` (bytes 2–3 of the primary skipped), every type byte's InUse bit set first.
fn set_checksum(set: &[[u8; 32]]) -> u16 {
    let mut sum = 0u16;
    for (k, e) in set.iter().enumerate() {
        for (i, &b) in e.iter().enumerate() {
            if k == 0 && (i == 2 || i == 3) {
                continue;
            }
            sum = sum16(sum, if i == 0 { b | 0x80 } else { b });
        }
    }
    sum
}

fn seal(set: &mut [[u8; 32]]) {
    let sum = set_checksum(set);
    set[0][2..4].copy_from_slice(&sum.to_le_bytes());
}

/// A File + Stream Extension + File Name set, checksum included, every entry in use. `flags`: the stream's
/// GeneralSecondaryFlags (0x01 AllocationPossible, 0x02 NoFatChain).
fn entry_set(name: &str, attr: u16, flags: u8, first: u32, len: u64, valid: u64) -> Vec<[u8; 32]> {
    let units: Vec<u16> = name.encode_utf16().collect();
    let names = units.len().div_ceil(15);
    let mut set = vec![[0u8; 32]; 2 + names];
    set[0][0] = 0x85;
    set[0][1] = (1 + names) as u8;
    set[0][4..6].copy_from_slice(&attr.to_le_bytes());
    set[1][0] = 0xC0;
    set[1][1] = flags;
    set[1][3] = units.len() as u8;
    set[1][8..16].copy_from_slice(&valid.to_le_bytes());
    set[1][20..24].copy_from_slice(&first.to_le_bytes());
    set[1][24..32].copy_from_slice(&len.to_le_bytes());
    for (i, chunk) in units.chunks(15).enumerate() {
        set[2 + i][0] = 0xC1;
        for (k, &u) in chunk.iter().enumerate() {
            set[2 + i][2 + 2 * k..4 + 2 * k].copy_from_slice(&u.to_le_bytes());
        }
    }
    seal(&mut set);
    set
}

/// The set with every InUse bit cleared (its checksum stays that of the live form).
fn erased(mut set: Vec<[u8; 32]>) -> Vec<[u8; 32]> {
    for e in &mut set {
        e[0] &= 0x7F;
    }
    set
}

fn put_set(b: &mut FsBuilder, dir: &str, set: &[[u8; 32]]) {
    for e in set {
        b.raw_entry(dir, *e);
    }
}

/// The name of the set starting at entry `at` of `dir`, when it is a File set.
fn set_name(dir: &[u8], at: usize) -> Option<String> {
    let e = &dir[at..];
    if e[0] & 0x7F != 0x05 || e[32] & 0x7F != 0x40 {
        return None;
    }
    let len = e[35] as usize;
    let units: Vec<u16> = (0..len)
        .map(|k| {
            let off = 64 + (k / 15) * 32 + 2 + 2 * (k % 15);
            u16::from_le_bytes([e[off], e[off + 1]])
        })
        .collect();
    Some(String::from_utf16_lossy(&units))
}

/// Applies `f` to the entries of the set named `name` in the first cluster of `dir` (in the volume bytes `v`), then
/// recomputes its checksum.
fn patch_set(v: &mut [u8], b: &FsBuilder, dir: &str, name: &str, f: impl FnOnce(&mut [[u8; 32]])) {
    let at = b.cluster_offset(b.clusters(dir)[0]) as usize;
    let cluster = &mut v[at..at + CLUSTER as usize];
    let mut k = 0;
    while k < cluster.len() {
        if set_name(cluster, k).as_deref() == Some(name) {
            let n = cluster[k + 1] as usize + 1;
            let mut set: Vec<[u8; 32]> = (0..n)
                .map(|i| cluster[k + 32 * i..k + 32 * i + 32].try_into().unwrap())
                .collect();
            let live = set[0][0] & 0x80 != 0;
            f(&mut set);
            seal(&mut set);
            if !live {
                set[0][0] &= 0x7F;
            }
            for (i, e) in set.iter().enumerate() {
                cluster[k + 32 * i..k + 32 * i + 32].copy_from_slice(e);
            }
            return;
        }
        k += 32;
    }
    panic!("no set {name} in {dir:?}");
}

fn u64_le(x: u64) -> [u8; 8] {
    x.to_le_bytes()
}

// ---- Tests ----

#[test]
fn lists_exfat_files_with_utc_offsets() {
    let mut b = card();
    let raw = arw();
    let video = clip();
    let photo = jpeg(320, 240, 4, 0);
    let arw_clusters = b.add("DCIM/100MSDCF/DSC01234.ARW", &raw);
    b.add("DCIM/100MSDCF/C0001.MP4", &video);
    b.add("Fotos da corrida/IMG 0001.JPG", &photo);
    b.add("MISC/notes.txt", &b"camera notes\n".repeat(100));
    b.stamp(
        "DCIM/100MSDCF/DSC01234.ARW",
        stamp(7, 12, 4, Some(-180)),
        stamp(7, 12, 6, Some(-180)),
    );
    let l = listing(&placed(&b));

    assert_eq!(l.volumes.len(), 1);
    assert_eq!(
        paths(&l),
        [
            "DCIM/100MSDCF/C0001.MP4",
            "DCIM/100MSDCF/DSC01234.ARW",
            "Fotos da corrida/IMG 0001.JPG"
        ]
    );
    let a = file(&l, "DCIM/100MSDCF/DSC01234.ARW");
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
    let modified = a.origin.modified.clone().unwrap();
    assert_eq!(
        modified,
        FsTime {
            local: "2025-12-14T07:12:06".into(),
            utc_offset_minutes: Some(-180)
        }
    );
    assert_eq!(modified.to_string(), "2025-12-14T07:12:06-03:00");
    assert_eq!(
        a.origin.created,
        Some(FsTime {
            local: "2025-12-14T07:12:04".into(),
            utc_offset_minutes: Some(-180)
        })
    );
    // No offset recorded: none read.
    let j = file(&l, "Fotos da corrida/IMG 0001.JPG");
    assert_eq!(j.origin.modified.as_ref().unwrap().utc_offset_minutes, None);
    assert_eq!(j.origin.name(), "IMG 0001.JPG");
    assert_eq!(j.origin.folder(), Some("Fotos da corrida"));
    let m = file(&l, "DCIM/100MSDCF/C0001.MP4");
    assert_eq!((m.kind, m.origin.size), (FileKind::Mp4, video.len() as u64));
    assert!(l.files.iter().all(|f| f.trusted));
    // Every live file and directory is in `live`; the other file is in `others`.
    for f in &l.files {
        for p in &f.pieces {
            assert!(overlaps(&l.live, p.start, p.len));
        }
    }
    for dir in ["", "DCIM", "DCIM/100MSDCF", "MISC"] {
        let c = b.clusters(dir)[0];
        assert!(overlaps(&l.live, VOL + b.cluster_offset(c), 32), "{dir:?}");
    }
    let notes = b.clusters("MISC/notes.txt");
    assert_eq!(l.others.len(), 1);
    assert_eq!(l.others[0].start, VOL + b.cluster_offset(notes[0]));
    assert!(!l.truncated);
}

#[test]
fn fat_chained_exfat_file_follows_the_fat() {
    let mut b = card();
    b.add("DCIM/A.JPG", &bytes(1000, 1));
    let clusters = [100, 101, 102, 200, 201];
    let data = bytes(4 * CLUSTER as usize + 100, 2);
    b.add_at("DCIM/FRAG.JPG", &data, &clusters);
    let l = listing(&placed(&b));
    let f = file(&l, "DCIM/FRAG.JPG");
    assert_eq!(f.origin.state, FsState::Live);
    assert_eq!(f.origin.placement, Placement::Chain);
    assert_eq!(f.pieces, pieces_of(&b, &clusters, data.len() as u64));
    assert_eq!(f.pieces.len(), 2);
    assert!(f.trusted);
    // A chain one cluster short of the size: not trusted.
    let mut b2 = card();
    b2.add_at("DCIM/FRAG.JPG", &data, &clusters);
    b2.set_fat(102, 0xFFFF_FFFF);
    let l = listing(&placed(&b2));
    let f = file(&l, "DCIM/FRAG.JPG");
    assert_eq!(f.pieces, pieces_of(&b2, &clusters[..3], data.len() as u64));
    assert!(!f.trusted);
}

#[test]
fn deleted_exfat_files() {
    let mut b = card();
    let one = bytes(3 * CLUSTER as usize - 10, 1);
    let two = bytes(5 * CLUSTER as usize - 10, 2);
    b.add("DCIM/KEEP.JPG", &bytes(1000, 9));
    let c1 = b.add("DCIM/ONE.JPG", &one);
    b.add_at("DCIM/TWO.JPG", &two, &[150, 151, 152, 160, 161]);
    let c3 = b.add("DCIM/GONE.JPG", &bytes(2 * CLUSTER as usize, 3));
    // Inside a deleted folder.
    let inner = bytes(CLUSTER as usize + 5, 4);
    let c4 = b.add("OLD/IN.JPG", &inner);
    for p in ["DCIM/ONE.JPG", "DCIM/TWO.JPG", "DCIM/GONE.JPG"] {
        b.delete(p, DeleteStyle::Camera);
    }
    b.delete("OLD", DeleteStyle::Camera);
    // GONE's first cluster now belongs to a live file.
    b.add_at("DCIM/NEW.JPG", &bytes(1000, 5), &[c3[0]]);
    let l = listing(&placed(&b));

    let f = file(&l, "DCIM/ONE.JPG");
    assert_eq!(f.origin.state, FsState::Deleted);
    assert_eq!(f.origin.placement, Placement::Recorded);
    assert_eq!(f.pieces, pieces_of(&b, &c1, one.len() as u64));
    assert_eq!(f.pieces.len(), 1);
    assert!(f.trusted);
    let f = file(&l, "DCIM/TWO.JPG");
    assert_eq!(f.origin.state, FsState::Deleted);
    assert_eq!(f.origin.placement, Placement::Guessed);
    assert_eq!(
        f.pieces,
        pieces_of(&b, &[150, 151, 152, 160, 161], two.len() as u64)
    );
    assert!(!f.trusted);
    assert!(!paths(&l).contains(&"DCIM/GONE.JPG".to_string()));
    let f = file(&l, "OLD/IN.JPG");
    assert_eq!(f.origin.state, FsState::Deleted);
    assert_eq!(f.pieces, pieces_of(&b, &c4, inner.len() as u64));
    assert_eq!(file(&l, "DCIM/NEW.JPG").origin.state, FsState::Live);
    assert_eq!(deleted(&l).len(), 3, "{:?}", paths(&l));
    // Deleted files are never in `live`.
    assert!(!overlaps(&l.live, VOL + b.cluster_offset(c1[0]), 1));
    assert!(!l.truncated);
}

#[test]
fn deleted_folders_read_only_free_clusters_and_never_loop() {
    // GHOST: a deleted NoFatChain folder of two clusters, c (free, holding a deleted G.JPG and a deleted folder SELF
    // pointing back at c) and c + 1, now LIVEDIR's (which holds a deleted X.JPG of its own).
    let mut b = card();
    let free = 300u32;
    let mut entries: Vec<[u8; 32]> = erased(entry_set("G.JPG", 0x20, 0x03, free, 1000, 1000));
    let tmp = b.add("TMP.BIN", &[0u8; 32]);
    let c = tmp[0];
    entries.extend(erased(entry_set("SELF", 0x10, 0x03, c, CLUSTER, CLUSTER)));
    // The rest of c holds unused entries (type 0x01), not an end marker, so a read goes on into c + 1.
    let mut unused = [0u8; 32];
    unused[0] = 0x01;
    entries.resize(CLUSTER as usize / 32, unused);
    let data: Vec<u8> = entries.iter().flatten().copied().collect();
    b.mkdir("LIVEDIR");
    assert_eq!(b.clusters("LIVEDIR")[0], c + 1);
    b.add("LIVEDIR/X.JPG", &bytes(1000, 3));
    b.delete("LIVEDIR/X.JPG", DeleteStyle::Camera);
    b.delete("TMP.BIN", DeleteStyle::Camera);
    put_set(
        &mut b,
        "",
        &erased(entry_set("GHOST", 0x10, 0x03, c, 2 * CLUSTER, 2 * CLUSTER)),
    );
    let l = listing(&placed_with(&b, |v| {
        let at = b.cluster_offset(c) as usize;
        v[at..at + data.len()].copy_from_slice(&data);
    }));
    assert_eq!(paths(&l), ["GHOST/G.JPG", "LIVEDIR/X.JPG"]);
    assert!(deleted(&l).len() == 2);
}

#[test]
fn reused_clusters_make_a_deleted_file_partial() {
    let mut b = card();
    let data = bytes(4 * CLUSTER as usize - 100, 1);
    let a = b.add("DCIM/A.JPG", &data);
    assert_eq!(a.len(), 4);
    b.delete("DCIM/A.JPG", DeleteStyle::Camera);
    b.add_at("DCIM/L.JPG", &bytes(1000, 2), &[a[2]]);
    let l = listing(&placed(&b));
    let f = file(&l, "DCIM/A.JPG");
    assert_eq!(f.origin.state, FsState::Deleted);
    assert_eq!(f.origin.placement, Placement::Recorded);
    assert_eq!(f.pieces, pieces_of(&b, &a, data.len() as u64));
    assert!(!f.trusted);
    assert!(file(&l, "DCIM/L.JPG").trusted);
}

#[test]
fn broken_entry_sets_are_skipped() {
    let mut b = card();
    b.add("DCIM/GOOD1.JPG", &bytes(1000, 1));
    let free = 300u32;
    let good = |name: &str| entry_set(name, 0x20, 0x03, free, 1000, 1000);
    // A wrong checksum.
    let mut bad = good("BAD1.JPG");
    bad[0][2] ^= 1;
    put_set(&mut b, "DCIM", &bad);
    // SecondaryCount 1 (no name entry counted).
    let mut bad = good("BAD2.JPG");
    bad[0][1] = 1;
    seal(&mut bad);
    put_set(&mut b, "DCIM", &bad);
    // SecondaryCount 19: the stream, one name and 17 vendor entries.
    let mut bad = good("BAD3.JPG");
    for _ in 0..17 {
        let mut vendor = [0u8; 32];
        vendor[0] = 0xE0;
        bad.push(vendor);
    }
    bad[0][1] = 19;
    seal(&mut bad);
    put_set(&mut b, "DCIM", &bad);
    // NameLength 0.
    let mut bad = good("BAD4.JPG");
    bad[1][3] = 0;
    seal(&mut bad);
    put_set(&mut b, "DCIM", &bad);
    // NameLength 40 with one name entry.
    let mut bad = good("BAD5.JPG");
    bad[1][3] = 40;
    seal(&mut bad);
    put_set(&mut b, "DCIM", &bad);
    // A File entry followed by a File Name entry (no stream).
    let mut bad = good("BAD6.JPG");
    bad.remove(1);
    let name = bad[1];
    bad.push(name);
    bad[0][1] = 2;
    seal(&mut bad);
    put_set(&mut b, "DCIM", &bad);
    // Vendor entries inside the count are skipped: this one is good.
    let mut ok = good("OK7.JPG");
    let mut vendor = [0u8; 32];
    vendor[0] = 0xE0;
    ok.push(vendor);
    ok[0][1] += 1;
    seal(&mut ok);
    put_set(&mut b, "DCIM", &ok);
    b.add("DCIM/GOOD2.JPG", &bytes(1000, 2));
    let l = listing(&placed(&b));
    assert_eq!(
        paths(&l),
        ["DCIM/GOOD1.JPG", "DCIM/GOOD2.JPG", "DCIM/OK7.JPG"]
    );
    assert!(file(&l, "DCIM/GOOD2.JPG").trusted);
}

#[test]
fn valid_data_length_short_of_data_length_is_not_trusted() {
    let mut b = card();
    let data = bytes(3 * CLUSTER as usize, 1);
    b.add("DCIM/A.JPG", &data);
    b.add("DCIM/B.JPG", &data);
    b.add("DCIM/C.JPG", &data);
    b.add("DCIM/D.JPG", &data);
    b.delete("DCIM/C.JPG", DeleteStyle::Camera);
    b.delete("DCIM/D.JPG", DeleteStyle::Camera);
    let len = data.len() as u64;
    let l = listing(&placed_with(&b, |v| {
        for name in ["A.JPG", "C.JPG"] {
            patch_set(v, &b, "DCIM", name, |s| {
                s[1][8..16].copy_from_slice(&u64_le(len - 4096))
            });
        }
    }));
    let a = file(&l, "DCIM/A.JPG");
    assert_eq!(a.origin.size, len);
    assert!(!a.trusted);
    assert!(file(&l, "DCIM/B.JPG").trusted);
    // Deleted alike.
    let c = file(&l, "DCIM/C.JPG");
    assert_eq!((c.origin.state, c.origin.size), (FsState::Deleted, len));
    assert!(!c.trusted);
    assert!(file(&l, "DCIM/D.JPG").trusted);
}

#[test]
fn root_files_before_the_bitmap_entry_wait_for_it() {
    // The root's entries reordered: label, up-case table, R.JPG's set, then the bitmap; R's clusters read free in the
    // bitmap. R is listed, and checked against the bitmap even though its set came first.
    let mut b = card();
    let r = b.add("R.JPG", &bytes(1000, 1));
    b.add("DCIM/A.JPG", &bytes(1000, 2));
    let l = listing(&placed_with(&b, |v| {
        let root = b.cluster_offset(b.clusters("")[0]) as usize;
        let at = |k: usize| root + 32 * k;
        let bitmap: [u8; 32] = v[at(1)..at(2)].try_into().unwrap();
        assert_eq!(bitmap[0], 0x81);
        assert_eq!(set_name(v, at(3)).as_deref(), Some("R.JPG"));
        v.copy_within(at(2)..at(6), at(1));
        v[at(5)..at(6)].copy_from_slice(&bitmap);
        let map = b.cluster_offset(2) as usize;
        let k = (r[0] - 2) as usize;
        v[map + k / 8] &= !(1 << (k % 8));
    }));
    let f = file(&l, "R.JPG");
    assert_eq!(f.origin.state, FsState::Live);
    assert!(!f.trusted);
    assert!(file(&l, "DCIM/A.JPG").trusted);
}

fn timed(bytes: &[u8]) -> Listing {
    let t = Instant::now();
    let l = listing(bytes);
    assert!(
        t.elapsed() < Duration::from_secs(5),
        "took {:?}",
        t.elapsed()
    );
    l
}

/// Recomputes the main boot region's checksum (sector 11) and copies it to the backup region (sectors 12–23).
fn reseal_boot(v: &mut [u8]) {
    let mut sum = 0u32;
    for (i, &byte) in v[..11 * 512].iter().enumerate() {
        if i != 106 && i != 107 && i != 112 {
            sum = sum.rotate_right(1).wrapping_add(byte as u32);
        }
    }
    for i in 0..128 {
        v[11 * 512 + 4 * i..11 * 512 + 4 * i + 4].copy_from_slice(&sum.to_le_bytes());
    }
    v.copy_within(0..12 * 512, 12 * 512);
}

#[test]
fn hostile_exfat_is_bounded() {
    // The root's FAT chain cycles back to itself, and a subdirectory entry points at the root.
    let mut b = card();
    b.add("DCIM/A.JPG", &bytes(1000, 1));
    let root = b.clusters("")[0];
    b.set_fat(root, root);
    put_set(
        &mut b,
        "DCIM",
        &entry_set("LOOP", 0x10, 0x01, root, CLUSTER, CLUSTER),
    );
    let l = timed(&placed(&b));
    assert_eq!(paths(&l), ["DCIM/A.JPG"]);
    assert!(!overlaps(&l.live, VOL + b.cluster_offset(root), 1));

    // A directory recording 2^40 bytes over a 3-cluster chain: read as far as its chain goes, not trusted.
    let mut b = card();
    b.add("DCIM/A.JPG", &bytes(1000, 1));
    let d = b.clusters("DCIM")[0];
    b.set_fat(d, 400)
        .set_fat(400, 401)
        .set_fat(401, 0xFFFF_FFFF);
    let l = timed(&placed_with(&b, |v| {
        patch_set(v, &b, "", "DCIM", |s| {
            s[1][8..16].copy_from_slice(&u64_le(1 << 40));
            s[1][24..32].copy_from_slice(&u64_le(1 << 40));
        })
    }));
    assert_eq!(paths(&l), ["DCIM/A.JPG"]);
    assert!(!overlaps(&l.live, VOL + b.cluster_offset(d), 1));
    assert!(l.truncated);

    // The bitmap's DataLength is 2^40: no bitmap, so every deleted file is untrusted (live ones stay trusted).
    let mut b = card();
    b.add("DCIM/LIVE.JPG", &bytes(1000, 1));
    b.add("DCIM/GONE.JPG", &bytes(1000, 2));
    b.delete("DCIM/GONE.JPG", DeleteStyle::Camera);
    let control = listing(&placed(&b));
    assert!(file(&control, "DCIM/GONE.JPG").trusted);
    let l = timed(&placed_with(&b, |v| {
        let at = b.cluster_offset(b.clusters("")[0]) as usize;
        let k = (0..CLUSTER as usize / 32)
            .map(|k| at + 32 * k)
            .find(|&e| v[e] == 0x81)
            .unwrap();
        v[k + 24..k + 32].copy_from_slice(&u64_le(1 << 40));
    }));
    assert!(file(&l, "DCIM/LIVE.JPG").trusted);
    let g = file(&l, "DCIM/GONE.JPG");
    assert_eq!(g.origin.state, FsState::Deleted);
    assert!(!g.trusted);

    // ClusterCount 2^30 (FatLength, ClusterHeapOffset and VolumeLength made to agree, the boot checksum redone):
    // the volume is reported, not walked.
    let mut b = card();
    b.add("DCIM/A.JPG", &bytes(1000, 1));
    let l = timed(&placed_with(&b, |v| {
        let count: u64 = 1 << 30;
        let fat_len = ((count + 2) * 4).div_ceil(512);
        let heap = (128 + fat_len).next_multiple_of(64);
        v[72..80].copy_from_slice(&u64_le(heap + count * 64));
        v[84..88].copy_from_slice(&(fat_len as u32).to_le_bytes());
        v[88..92].copy_from_slice(&(heap as u32).to_le_bytes());
        v[92..96].copy_from_slice(&(count as u32).to_le_bytes());
        reseal_boot(v);
    }));
    assert_eq!(l.volumes.len(), 1);
    assert_eq!(l.volumes[0].cluster_count, 1 << 30);
    assert!(l.files.is_empty() && l.live.is_empty());
    assert!(l.truncated);

    // 10,000 deleted NoFatChain sets of 4 GiB each on one free cluster: each bitmap check is bounded.
    let mut b = card();
    b.add("DCIM/A.JPG", &bytes(1000, 1));
    let free = 300u32;
    for i in 0..10_000u32 {
        let set = entry_set(&format!("D{i:05}.JPG"), 0x20, 0x03, free, 4 << 30, 4 << 30);
        put_set(&mut b, "HOSTILE", &erased(set));
    }
    let bytes = placed(&b);
    let l = timed(&bytes);
    let gone = deleted(&l);
    assert_eq!(gone.len(), 10_000);
    let end = bytes.len() as u64;
    for f in gone {
        assert!(!f.trusted);
        assert_eq!(f.origin.placement, Placement::Recorded);
        assert_eq!(f.pieces[0].start, VOL + b.cluster_offset(free));
        assert!(f.pieces.iter().all(|p| p.len > 0 && p.start + p.len <= end));
    }
}
