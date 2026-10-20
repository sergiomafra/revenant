//! Synthetic FAT12/16/32 and exFAT volumes, MBR and GPT disks, for the file-system tests.
//!
//! Every on-disk structure follows the public specifications: Microsoft's "FAT32 File System Specification" 1.03
//! (fatgen103), Microsoft's "exFAT file system specification" (2019) and the GPT chapter of the UEFI specification. The
//! builder keeps a model of the volume (directory tree, FAT, allocation) and writes the whole volume in [`FsBuilder::build`].
//!
//! Paths are `/`-separated with no leading slash; `""` is the root. Names are matched exactly (case-sensitive).

use std::collections::{HashMap, HashSet};

const SECTOR: u64 = 512;
const LABEL: &str = "REVENANT";
const SERIAL: u32 = 0x1234_5678;
/// Root entries of FAT12/16 (32 sectors).
const FAT_ROOT_ENTRIES: u64 = 512;
/// Up-case table entries (identity).
const UPCASE_ENTRIES: u64 = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsType {
    Fat12,
    Fat16,
    Fat32,
    Exfat,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeleteStyle {
    /// FAT: `0xE5` on byte 0 of the short entry and its long-name entries, chain entries set to 0. exFAT: `InUse` bits
    /// cleared, bitmap bits cleared, FAT entries kept.
    Camera,
    /// As `Camera`, and the short entry's high cluster word set to 0 (as Windows does on FAT32).
    WindowsFat32,
}

/// FAT/exFAT time stamp (2 s resolution); `utc_offset_min` is written only on exFAT (multiple of 15).
#[derive(Clone, Copy, Debug)]
pub struct FsStamp {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub min: u8,
    pub sec: u8,
    pub utc_offset_min: Option<i16>,
}

impl FsStamp {
    /// FAT date and time words.
    fn fat(&self) -> (u16, u16) {
        let date = (self.year - 1980) << 9 | (self.month as u16) << 5 | self.day as u16;
        let time = (self.hour as u16) << 11 | (self.min as u16) << 5 | (self.sec / 2) as u16;
        (date, time)
    }

    /// exFAT 32-bit timestamp.
    fn exfat(&self) -> u32 {
        let (date, time) = self.fat();
        (date as u32) << 16 | time as u32
    }

    /// 10 ms increment past the 2-second resolution (0 or 100).
    fn tenths(&self) -> u8 {
        (self.sec % 2) * 100
    }

    /// exFAT UtcOffset byte: bit 7 = valid, bits 0..7 = signed 15-minute increments.
    fn exfat_offset(&self) -> u8 {
        match self.utc_offset_min {
            Some(m) => {
                assert!(m % 15 == 0, "utc_offset_min must be a multiple of 15");
                0x80 | ((m / 15) as i8 as u8 & 0x7F)
            }
            None => 0,
        }
    }
}

const DEFAULT_STAMP: FsStamp = FsStamp {
    year: 2025,
    month: 12,
    day: 14,
    hour: 7,
    min: 12,
    sec: 4,
    utc_offset_min: None,
};

/// Where everything is in a volume, in sectors of `bps` bytes.
#[derive(Clone, Debug)]
struct Geo {
    fs: FsType,
    bps: u64,
    spc: u64,
    total: u64,
    fat_start: u64,
    fat_len: u64,
    nfats: u64,
    /// FAT12/16 only: the fixed root region.
    root_start: u64,
    root_sectors: u64,
    /// First sector of cluster 2.
    heap: u64,
    count: u32,
    /// FAT32/exFAT: the root's first cluster.
    root_cluster: u32,
}

impl Geo {
    fn cluster_len(&self) -> u64 {
        self.bps * self.spc
    }

    fn cluster_offset(&self, c: u32) -> u64 {
        assert!(
            c >= 2 && c < self.count + 2,
            "cluster {c} outside 2..{}",
            self.count + 2
        );
        (self.heap + (c as u64 - 2) * self.spc) * self.bps
    }

    fn eoc(&self) -> u32 {
        match self.fs {
            FsType::Fat12 => 0xFFF,
            FsType::Fat16 => 0xFFFF,
            FsType::Fat32 => 0x0FFF_FFFF,
            FsType::Exfat => 0xFFFF_FFFF,
        }
    }

    /// FAT entries 0 and 1.
    fn reserved_entries(&self) -> [u32; 2] {
        match self.fs {
            FsType::Fat12 => [0xFF8, 0xFFF],
            FsType::Fat16 => [0xFFF8, 0xFFFF],
            FsType::Fat32 => [0x0FFF_FFF8, 0x0FFF_FFFF],
            FsType::Exfat => [0xFFFF_FFF8, 0xFFFF_FFFF],
        }
    }

    /// Bytes of one FAT holding entries `0..n`.
    fn fat_bytes(fs: FsType, n: u64) -> u64 {
        match fs {
            FsType::Fat12 => (n * 3).div_ceil(2),
            FsType::Fat16 => n * 2,
            FsType::Fat32 | FsType::Exfat => n * 4,
        }
    }

    /// exFAT: bitmap length in bytes and clusters.
    fn bitmap_len(&self) -> (u64, u64) {
        let bytes = (self.count as u64).div_ceil(8);
        (bytes, bytes.div_ceil(self.cluster_len()))
    }

    /// The geometry the builder uses for a new volume.
    fn new(fs: FsType, volume_len: u64, cluster: u32) -> Geo {
        assert!(
            volume_len.is_multiple_of(SECTOR),
            "volume length must be a multiple of 512"
        );
        let cluster = cluster as u64;
        assert!(
            cluster >= SECTOR && cluster.is_power_of_two(),
            "cluster must be a power of two ≥ 512"
        );
        let spc = cluster / SECTOR;
        let total = volume_len / SECTOR;
        let (reserved, nfats, root_sectors) = match fs {
            FsType::Fat12 | FsType::Fat16 => (1, 2, FAT_ROOT_ENTRIES * 32 / SECTOR),
            FsType::Fat32 => (32, 2, 0),
            FsType::Exfat => (128, 1, 0),
        };
        assert!(
            spc <= 128 || fs == FsType::Exfat,
            "FAT clusters are at most 64 KiB"
        );
        // Grow the FAT until it covers the clusters left after it (fatgen103 and exFAT spec §3.1.6).
        let mut fat_len = 1u64;
        let (heap, count) = loop {
            let mut heap = reserved + nfats * fat_len + root_sectors;
            if fs == FsType::Exfat {
                heap = heap.next_multiple_of(spc);
            }
            assert!(total > heap + spc, "volume too small");
            let count = (total - heap) / spc;
            let need = Self::fat_bytes(fs, count + 2).div_ceil(SECTOR);
            if need <= fat_len {
                break (heap, count);
            }
            fat_len = need;
        };
        let fits = match fs {
            FsType::Fat12 => count < 4085,
            FsType::Fat16 => (4085..=65_524).contains(&count),
            FsType::Fat32 => (65_525..0x0FFF_FFF5).contains(&count),
            FsType::Exfat => (1..=0xFFFF_FFF5).contains(&count),
        };
        assert!(fits, "{count} clusters do not fit {fs:?}");
        if fs == FsType::Exfat {
            assert!(volume_len >= 1 << 20, "exFAT volumes are at least 1 MiB");
        }
        let mut geo = Geo {
            fs,
            bps: SECTOR,
            spc,
            total,
            fat_start: reserved,
            fat_len,
            nfats,
            root_start: reserved + nfats * fat_len,
            root_sectors,
            heap,
            count: count as u32,
            root_cluster: 0,
        };
        geo.root_cluster = match fs {
            FsType::Fat12 | FsType::Fat16 => 0,
            FsType::Fat32 => 2,
            // Bitmap, then the up-case table (one cluster), then the root.
            FsType::Exfat => 2 + geo.bitmap_len().1 as u32 + 1,
        };
        geo
    }

    /// The geometry of an existing volume, from its own boot sector.
    fn read(v: &[u8]) -> Geo {
        assert!(v.len() >= 512, "no boot sector");
        let u16_at = |at: usize| u16::from_le_bytes([v[at], v[at + 1]]) as u64;
        let u32_at = |at: usize| u32::from_le_bytes(v[at..at + 4].try_into().unwrap()) as u64;
        if &v[3..11] == b"EXFAT   " {
            let bps = 1u64 << v[108];
            let spc = 1u64 << v[109];
            let fat_start = u32_at(80);
            return Geo {
                fs: FsType::Exfat,
                bps,
                spc,
                total: u64::from_le_bytes(v[72..80].try_into().unwrap()),
                fat_start,
                fat_len: u32_at(84),
                nfats: v[110] as u64,
                root_start: 0,
                root_sectors: 0,
                heap: u32_at(88),
                count: u32_at(92) as u32,
                root_cluster: u32_at(96) as u32,
            };
        }
        let bps = u16_at(11);
        let spc = v[13] as u64;
        assert!(bps >= 512 && spc > 0, "not a FAT boot sector");
        let reserved = u16_at(14);
        let nfats = v[16] as u64;
        let root_entries = u16_at(17);
        let total = match u16_at(19) {
            0 => u32_at(32),
            n => n,
        };
        let fat_len = match u16_at(22) {
            0 => u32_at(36),
            n => n,
        };
        let root_sectors = (root_entries * 32).div_ceil(bps);
        let root_start = reserved + nfats * fat_len;
        let heap = root_start + root_sectors;
        let count = (total - heap) / spc;
        let fs = if count < 4085 {
            FsType::Fat12
        } else if count < 65_525 {
            FsType::Fat16
        } else {
            FsType::Fat32
        };
        Geo {
            fs,
            bps,
            spc,
            total,
            fat_start: reserved,
            fat_len,
            nfats,
            root_start,
            root_sectors,
            heap,
            count: count as u32,
            root_cluster: if fs == FsType::Fat32 {
                u32_at(44) as u32
            } else {
                0
            },
        }
    }

    /// The FAT region as bytes, from entries `0..n` (missing entries are 0).
    fn fat_image(&self, entries: &[u32]) -> Vec<u8> {
        let mut fat = vec![0u8; (self.fat_len * self.bps) as usize];
        for (c, &e) in entries.iter().enumerate() {
            put_fat(self.fs, &mut fat, c as u32, e);
        }
        fat
    }

    /// Writes `fat` into every FAT copy.
    fn write_fats(&self, v: &mut [u8], fat: &[u8]) {
        for i in 0..self.nfats {
            let at = ((self.fat_start + i * self.fat_len) * self.bps) as usize;
            v[at..at + fat.len()].copy_from_slice(fat);
        }
    }
}

fn put_fat(fs: FsType, fat: &mut [u8], c: u32, e: u32) {
    let c = c as usize;
    match fs {
        FsType::Fat12 => {
            let at = c * 3 / 2;
            let w = u16::from_le_bytes([fat[at], fat[at + 1]]);
            let w = if c.is_multiple_of(2) {
                (w & 0xF000) | (e as u16 & 0x0FFF)
            } else {
                (w & 0x000F) | ((e as u16) << 4)
            };
            fat[at..at + 2].copy_from_slice(&w.to_le_bytes());
        }
        FsType::Fat16 => fat[c * 2..c * 2 + 2].copy_from_slice(&(e as u16).to_le_bytes()),
        FsType::Fat32 | FsType::Exfat => fat[c * 4..c * 4 + 4].copy_from_slice(&e.to_le_bytes()),
    }
}

fn put16(v: &mut [u8], at: usize, x: u16) {
    v[at..at + 2].copy_from_slice(&x.to_le_bytes());
}

fn put32(v: &mut [u8], at: usize, x: u32) {
    v[at..at + 4].copy_from_slice(&x.to_le_bytes());
}

fn put64(v: &mut [u8], at: usize, x: u64) {
    v[at..at + 8].copy_from_slice(&x.to_le_bytes());
}

/// The rotate-right-and-add checksum of the exFAT spec (§3.4, §7.2.2), 32-bit.
fn exfat_sum32(sum: u32, byte: u8) -> u32 {
    sum.rotate_right(1).wrapping_add(byte as u32)
}

/// The same, 16-bit (§6.3.3, §7.6.4).
fn exfat_sum16(sum: u16, byte: u8) -> u16 {
    sum.rotate_right(1).wrapping_add(byte as u16)
}

/// The identity up-case table of `UPCASE_ENTRIES` entries.
fn upcase_table() -> Vec<u8> {
    (0..UPCASE_ENTRIES as u16)
        .flat_map(u16::to_le_bytes)
        .collect()
}

/// A FAT boot sector (and, on FAT32, FSInfo and the backup boot record) for `geo`.
fn write_fat_boot(v: &mut [u8], geo: &Geo) {
    let b = &mut v[..512];
    let fat32 = geo.fs == FsType::Fat32;
    b[..3].copy_from_slice(if fat32 {
        &[0xEB, 0x58, 0x90]
    } else {
        &[0xEB, 0x3C, 0x90]
    });
    b[3..11].copy_from_slice(b"REVENANT");
    put16(b, 11, geo.bps as u16);
    b[13] = geo.spc as u8;
    put16(b, 14, geo.fat_start as u16);
    b[16] = geo.nfats as u8;
    put16(b, 17, if fat32 { 0 } else { FAT_ROOT_ENTRIES as u16 });
    if geo.total < 0x10000 && !fat32 {
        put16(b, 19, geo.total as u16);
    } else {
        put32(b, 32, geo.total as u32);
    }
    b[21] = 0xF8;
    put16(b, 24, 63);
    put16(b, 26, 255);
    let ext = if fat32 {
        put32(b, 36, geo.fat_len as u32);
        put32(b, 44, geo.root_cluster);
        put16(b, 48, 1);
        put16(b, 50, 6);
        64
    } else {
        put16(b, 22, geo.fat_len as u16);
        36
    };
    b[ext] = 0x80;
    b[ext + 2] = 0x29;
    put32(b, ext + 3, SERIAL);
    b[ext + 7..ext + 18].copy_from_slice(&label_11());
    b[ext + 18..ext + 26].copy_from_slice(match geo.fs {
        FsType::Fat12 => b"FAT12   ",
        FsType::Fat16 => b"FAT16   ",
        _ => b"FAT32   ",
    });
    b[510] = 0x55;
    b[511] = 0xAA;
    if fat32 {
        let s = &mut v[512..1024];
        put32(s, 0, 0x4161_5252);
        put32(s, 484, 0x6141_7272);
        put32(s, 488, 0xFFFF_FFFF);
        put32(s, 492, 0xFFFF_FFFF);
        put32(s, 508, 0xAA55_0000);
        v[1024 + 510] = 0x55;
        v[1024 + 511] = 0xAA;
    }
}

/// FAT32: FSInfo free count and next-free hint from the FAT entries, then the backup boot record (sectors 6..9).
fn finish_fat32_boot(v: &mut [u8], geo: &Geo, entries: &[u32]) {
    let free = (2..geo.count as usize + 2)
        .filter(|&c| entries.get(c).copied().unwrap_or(0) == 0)
        .count();
    let next = (2..geo.count as usize + 2).find(|&c| entries.get(c).copied().unwrap_or(0) == 0);
    put32(v, 512 + 488, free as u32);
    put32(v, 512 + 492, next.map_or(0xFFFF_FFFF, |c| c as u32));
    v.copy_within(0..3 * 512, 6 * 512);
}

/// The 11-byte volume label.
fn label_11() -> [u8; 11] {
    let mut l = [b' '; 11];
    l[..LABEL.len()].copy_from_slice(LABEL.as_bytes());
    l
}

/// The FAT volume-label directory entry.
fn fat_label_entry() -> [u8; 32] {
    let mut e = [0u8; 32];
    e[..11].copy_from_slice(&label_11());
    e[11] = 0x08;
    let (date, time) = DEFAULT_STAMP.fat();
    put16(&mut e, 22, time);
    put16(&mut e, 24, date);
    e
}

/// The exFAT main boot region (sectors 0..12) for `geo`, copied to the backup region (12..24).
fn write_exfat_boot(v: &mut [u8], geo: &Geo) {
    let b = &mut v[..512];
    b[..3].copy_from_slice(&[0xEB, 0x76, 0x90]);
    b[3..11].copy_from_slice(b"EXFAT   ");
    put64(b, 72, geo.total);
    put32(b, 80, geo.fat_start as u32);
    put32(b, 84, geo.fat_len as u32);
    put32(b, 88, geo.heap as u32);
    put32(b, 92, geo.count);
    put32(b, 96, geo.root_cluster);
    put32(b, 100, SERIAL);
    put16(b, 104, 0x0100);
    b[108] = geo.bps.trailing_zeros() as u8;
    b[109] = geo.spc.trailing_zeros() as u8;
    b[110] = geo.nfats as u8;
    b[111] = 0x80;
    // PercentInUse "not available", so the boot region does not change with the contents.
    b[112] = 0xFF;
    b[510] = 0x55;
    b[511] = 0xAA;
    for s in 1..=8 {
        put32(v, s * 512 + 508, 0xAA55_0000);
    }
    finish_exfat_boot(v);
}

/// Boot checksum sector (11) and the backup boot region (12..24).
fn finish_exfat_boot(v: &mut [u8]) {
    let bps = 1usize << v[108];
    let mut sum = 0u32;
    for (i, &byte) in v[..11 * bps].iter().enumerate() {
        if i != 106 && i != 107 && i != 112 {
            sum = exfat_sum32(sum, byte);
        }
    }
    for i in 0..bps / 4 {
        put32(v, 11 * bps + 4 * i, sum);
    }
    v.copy_within(0..12 * bps, 12 * bps);
}

/// The exFAT root's first three entries: volume label, allocation bitmap, up-case table.
fn exfat_root_entries(geo: &Geo, upcase_cluster: u32) -> [[u8; 32]; 3] {
    let mut label = [0u8; 32];
    label[0] = 0x83;
    label[1] = LABEL.len() as u8;
    for (i, c) in LABEL.encode_utf16().enumerate() {
        put16(&mut label, 2 + 2 * i, c);
    }
    let mut bitmap = [0u8; 32];
    bitmap[0] = 0x81;
    put32(&mut bitmap, 20, 2);
    put64(&mut bitmap, 24, geo.bitmap_len().0);
    let table = upcase_table();
    let mut upcase = [0u8; 32];
    upcase[0] = 0x82;
    put32(
        &mut upcase,
        4,
        table.iter().fold(0, |s, &b| exfat_sum32(s, b)),
    );
    put32(&mut upcase, 20, upcase_cluster);
    put64(&mut upcase, 24, table.len() as u64);
    [label, bitmap, upcase]
}

/// One directory or file of the model.
struct Node {
    name: Vec<u16>,
    parent: usize,
    dir: bool,
    /// FAT: the short name (8 + 3, space padded) and whether long-name entries precede it.
    sfn: [u8; 11],
    lfn: bool,
    data: Vec<u8>,
    clusters: Vec<u32>,
    /// exFAT files: contiguous, so the FAT is not used.
    no_fat_chain: bool,
    created: FsStamp,
    modified: FsStamp,
    deleted: Option<DeleteStyle>,
    /// Directories: entries in order, slots used, live children by name, short names in use, next numeric tail.
    items: Vec<Item>,
    slots: u64,
    live: HashMap<Vec<u16>, usize>,
    sfns: HashSet<[u8; 11]>,
    tails: HashMap<[u8; 11], u32>,
}

enum Item {
    Node(usize),
    Raw([u8; 32]),
}

impl Node {
    fn new(name: Vec<u16>, parent: usize, dir: bool) -> Node {
        Node {
            name,
            parent,
            dir,
            sfn: [b' '; 11],
            lfn: false,
            data: Vec::new(),
            clusters: Vec::new(),
            no_fat_chain: false,
            created: DEFAULT_STAMP,
            modified: DEFAULT_STAMP,
            deleted: None,
            items: Vec::new(),
            slots: 0,
            live: HashMap::new(),
            sfns: HashSet::new(),
            tails: HashMap::new(),
        }
    }
}

/// Builds a synthetic FAT12/16/32 or exFAT volume.
pub struct FsBuilder {
    geo: Geo,
    nodes: Vec<Node>,
    /// FAT entries (index = cluster).
    fat: Vec<u32>,
    /// Allocation (the exFAT bitmap; the FAT allocator).
    alloc: Vec<bool>,
    /// Node index + 1 that last took each cluster (0: none; `u32::MAX`: exFAT bitmap or up-case table).
    owner: Vec<u32>,
    /// No free cluster below this one.
    first_free: u32,
    overrides: Vec<(u32, u32)>,
}

const OWNER_META: u32 = u32::MAX;

impl FsBuilder {
    /// Sector 512. Label "REVENANT" (FAT: a volume-label entry in the root and the BPB label; exFAT: a 0x83 entry in the
    /// root), serial 0x1234_5678. FAT12/16: 1 reserved sector, 2 FATs, 512 root entries;
    /// FAT32: 32 reserved sectors, FSInfo at 1, backup boot at 6, 2 FATs, root at cluster 2. exFAT: FAT at sector 128,
    /// 1 FAT, heap aligned to the cluster, bitmap then a 128-entry identity up-case table then the root, boot region
    /// checksums and backup region (sectors 12–23). Panics unless the cluster count fits the type (FAT12 < 4085,
    /// FAT16 4085..=65524, FAT32 ≥ 65525, as the Microsoft spec and macOS require).
    pub fn new(fs: FsType, volume_len: u64, cluster: u32) -> FsBuilder {
        let geo = Geo::new(fs, volume_len, cluster);
        let n = geo.count as usize + 2;
        let mut b = FsBuilder {
            geo,
            nodes: vec![Node::new(Vec::new(), 0, true)],
            fat: vec![0; n],
            alloc: vec![false; n],
            owner: vec![0; n],
            first_free: 2,
            overrides: Vec::new(),
        };
        match fs {
            FsType::Fat12 | FsType::Fat16 => b.nodes[0].slots = 1,
            FsType::Fat32 => {
                b.nodes[0].slots = 1;
                let c = b.take_free();
                b.claim(0, &[c], true);
            }
            FsType::Exfat => {
                b.nodes[0].slots = 3;
                let meta = b.geo.bitmap_len().1 as u32 + 1;
                let run: Vec<u32> = (2..2 + meta).collect();
                for &c in &run {
                    b.alloc[c as usize] = true;
                    b.owner[c as usize] = OWNER_META;
                }
                // Bitmap chain, then the one-cluster up-case table.
                b.chain(&run[..run.len() - 1]);
                b.chain(&run[run.len() - 1..]);
                b.first_free = 2 + meta;
                let c = b.take_free();
                assert_eq!(c, b.geo.root_cluster);
                b.claim(0, &[c], true);
            }
        }
        b
    }

    fn fat_like(&self) -> bool {
        self.geo.fs != FsType::Exfat
    }

    /// The lowest free cluster (does not mark it).
    fn take_free(&mut self) -> u32 {
        let n = self.geo.count + 2;
        let mut c = self.first_free;
        while c < n && self.alloc[c as usize] {
            c += 1;
        }
        assert!(c < n, "volume full");
        self.first_free = c;
        c
    }

    /// Writes a FAT chain through `run`.
    fn chain(&mut self, run: &[u32]) {
        for (i, &c) in run.iter().enumerate() {
            self.fat[c as usize] = run.get(i + 1).copied().unwrap_or(self.geo.eoc());
        }
    }

    /// Gives `clusters` to node `id` (appended to its chain). `fat_chain`: write the FAT for the whole chain.
    fn claim(&mut self, id: usize, clusters: &[u32], fat_chain: bool) {
        for &c in clusters {
            assert!(
                c >= 2 && c < self.geo.count + 2,
                "cluster {c} outside the heap"
            );
            self.alloc[c as usize] = true;
            self.owner[c as usize] = id as u32 + 1;
        }
        self.nodes[id].clusters.extend_from_slice(clusters);
        if fat_chain {
            let run = self.nodes[id].clusters.clone();
            self.chain(&run);
        }
    }

    fn release(&mut self, c: u32) {
        self.alloc[c as usize] = false;
        self.first_free = self.first_free.min(c);
    }

    /// 32-byte slots a child takes in its directory.
    fn slots_of(&self, id: usize) -> u64 {
        let node = &self.nodes[id];
        let len = node.name.len() as u64;
        if self.fat_like() {
            if node.lfn { len.div_ceil(13) + 1 } else { 1 }
        } else {
            2 + len.div_ceil(15)
        }
    }

    /// Room for `k` more entries in directory `dir`, growing its chain.
    fn reserve(&mut self, dir: usize, k: u64) {
        let fixed_root = dir == 0 && matches!(self.geo.fs, FsType::Fat12 | FsType::Fat16);
        loop {
            let capacity = if fixed_root {
                FAT_ROOT_ENTRIES
            } else {
                self.nodes[dir].clusters.len() as u64 * self.geo.cluster_len() / 32
            };
            if self.nodes[dir].slots + k <= capacity {
                break;
            }
            assert!(!fixed_root, "the FAT12/16 root holds 512 entries");
            let c = self.take_free();
            self.claim(dir, &[c], true);
        }
        self.nodes[dir].slots += k;
    }

    fn split(path: &str) -> Vec<&str> {
        path.split('/').filter(|s| !s.is_empty()).collect()
    }

    /// The live directory at `path`, created (with its parents) when missing.
    fn dir_at(&mut self, path: &str) -> usize {
        let mut at = 0;
        for part in Self::split(path) {
            let name: Vec<u16> = part.encode_utf16().collect();
            at = match self.nodes[at].live.get(&name) {
                Some(&id) => {
                    assert!(self.nodes[id].dir, "{part} is a file");
                    id
                }
                None => self.insert(at, name, true),
            };
        }
        at
    }

    /// The node at `path`: live first, else the latest deleted one of that name.
    fn find(&self, path: &str) -> usize {
        let mut at = 0;
        let parts = Self::split(path);
        for (i, part) in parts.iter().enumerate() {
            let name: Vec<u16> = part.encode_utf16().collect();
            let found = self.nodes[at].live.get(&name).copied().or_else(|| {
                if i + 1 < parts.len() {
                    return None;
                }
                self.nodes[at]
                    .items
                    .iter()
                    .rev()
                    .find_map(|item| match item {
                        Item::Node(id) if self.nodes[*id].name == name => Some(*id),
                        _ => None,
                    })
            });
            at = found.unwrap_or_else(|| panic!("no {path}"));
        }
        at
    }

    /// Adds a child named `name` to directory `parent` (entries reserved, short name chosen, directory cluster taken).
    fn insert(&mut self, parent: usize, name: Vec<u16>, dir: bool) -> usize {
        assert!(
            !name.is_empty() && name.len() <= 255,
            "names are 1..=255 UTF-16 units"
        );
        assert!(
            !self.nodes[parent].live.contains_key(&name),
            "{} exists",
            String::from_utf16_lossy(&name)
        );
        let id = self.nodes.len();
        let mut node = Node::new(name.clone(), parent, dir);
        if self.fat_like() {
            match short_name(&name) {
                Some(sfn) => node.sfn = sfn,
                None => {
                    node.lfn = true;
                    node.sfn = self.numeric_tail(parent, &name);
                }
            }
            self.nodes[parent].sfns.insert(node.sfn);
            node.slots = if dir { 2 } else { 0 };
        }
        self.nodes.push(node);
        self.nodes[parent].live.insert(name, id);
        let k = self.slots_of(id);
        self.reserve(parent, k);
        self.nodes[parent].items.push(Item::Node(id));
        if dir {
            let c = self.take_free();
            self.claim(id, &[c], true);
        }
        id
    }

    /// "BASIS~N.EXT" for a name that is not an upper-case 8.3 name (fatgen103 basis-name generation).
    fn numeric_tail(&mut self, dir: usize, name: &[u16]) -> [u8; 11] {
        let sfn_char = |c: u16| -> u8 {
            match c {
                0x61..=0x7A => c as u8 - 0x20,
                c if c < 0x80 && valid_sfn_char(c as u8) => c as u8,
                _ => b'_',
            }
        };
        let mut start = 0;
        while start < name.len() && (name[start] == b'.' as u16 || name[start] == b' ' as u16) {
            start += 1;
        }
        let name = &name[start..];
        let last_dot = name.iter().rposition(|&c| c == b'.' as u16);
        let primary_end = name
            .iter()
            .position(|&c| c == b'.' as u16)
            .unwrap_or(name.len());
        let primary: Vec<u8> = name[..primary_end]
            .iter()
            .filter(|&&c| c != b' ' as u16)
            .map(|&c| sfn_char(c))
            .take(8)
            .collect();
        let ext: Vec<u8> = match last_dot {
            Some(d) => name[d + 1..]
                .iter()
                .filter(|&&c| c != b' ' as u16 && c != b'.' as u16)
                .map(|&c| sfn_char(c))
                .take(3)
                .collect(),
            None => Vec::new(),
        };
        let mut key = [b' '; 11];
        key[..primary.len()].copy_from_slice(&primary);
        key[8..8 + ext.len()].copy_from_slice(&ext);
        let mut n = self.nodes[dir].tails.get(&key).copied().unwrap_or(0);
        loop {
            n += 1;
            let tail = format!("~{n}");
            assert!(tail.len() < 8, "too many short names for one basis");
            let keep = primary.len().min(8 - tail.len());
            let mut sfn = [b' '; 11];
            sfn[..keep].copy_from_slice(&primary[..keep]);
            sfn[keep..keep + tail.len()].copy_from_slice(tail.as_bytes());
            sfn[8..].copy_from_slice(&key[8..]);
            if !self.nodes[dir].sfns.contains(&sfn) {
                self.nodes[dir].tails.insert(key, n);
                return sfn;
            }
        }
    }

    /// Parents created as needed. A directory's cluster chain grows without a cap other than the free clusters (Task 4's
    /// `DCIM/BIG` holds 70,000 entries).
    pub fn mkdir(&mut self, path: &str) -> &mut Self {
        self.dir_at(path);
        self
    }

    /// `(directory, last component)` of a path.
    fn parent_of(&mut self, path: &str) -> (usize, Vec<u16>) {
        let (dir, name) = match path.rsplit_once('/') {
            Some((d, n)) => (d, n),
            None => ("", path),
        };
        (self.dir_at(dir), name.encode_utf16().collect())
    }

    fn add_file(
        &mut self,
        dir: usize,
        name: Vec<u16>,
        data: &[u8],
        clusters: Option<&[u32]>,
    ) -> Vec<u32> {
        let id = self.insert(dir, name, false);
        let cl = self.geo.cluster_len();
        let need = (data.len() as u64).div_ceil(cl) as usize;
        let clusters = match clusters {
            Some(c) => {
                assert_eq!(c.len(), need, "{} bytes need {need} clusters", data.len());
                c.to_vec()
            }
            None => {
                let mut picked = Vec::with_capacity(need);
                let mut c = self.first_free;
                while picked.len() < need {
                    assert!(c < self.geo.count + 2, "volume full");
                    if !self.alloc[c as usize] {
                        picked.push(c);
                    }
                    c += 1;
                }
                picked
            }
        };
        let contiguous = clusters.windows(2).all(|w| w[1] == w[0] + 1);
        let no_fat_chain = !self.fat_like() && contiguous;
        self.nodes[id].no_fat_chain = no_fat_chain;
        self.nodes[id].data = data.to_vec();
        self.claim(id, &clusters, !no_fat_chain);
        self.take_free_hint();
        clusters
    }

    /// Moves `first_free` past allocated clusters.
    fn take_free_hint(&mut self) {
        let n = self.geo.count + 2;
        while self.first_free < n && self.alloc[self.first_free as usize] {
            self.first_free += 1;
        }
    }

    /// `data` in the next free clusters (first fit, ascending); returns its clusters in file order. On exFAT a
    /// contiguous file is NoFatChain. A name that is not an upper-case 8.3 name gets LFN entries (FAT; SFN "XXXXXX~1.EXT").
    pub fn add(&mut self, path: &str, data: &[u8]) -> Vec<u32> {
        let (dir, name) = self.parent_of(path);
        self.add_file(dir, name, data, None)
    }

    /// `data` in exactly `clusters` (file order); FAT chain written; on exFAT FAT-chained unless consecutive.
    pub fn add_at(&mut self, path: &str, data: &[u8], clusters: &[u32]) -> &mut Self {
        let (dir, name) = self.parent_of(path);
        self.add_file(dir, name, data, Some(clusters));
        self
    }

    /// Like `add` with a raw UTF-16 last component (lone surrogates allowed), in directory `dir`.
    pub fn add_utf16(&mut self, dir: &str, name: &[u16], data: &[u8]) -> Vec<u32> {
        let dir = self.dir_at(dir);
        self.add_file(dir, name.to_vec(), data, None)
    }

    /// Default 2025-12-14 07:12:04 for both.
    pub fn stamp(&mut self, path: &str, created: FsStamp, modified: FsStamp) -> &mut Self {
        let id = self.find(path);
        self.nodes[id].created = created;
        self.nodes[id].modified = modified;
        self
    }

    /// Camera: FAT ← 0xE5 on byte 0 of the SFN and its LFN entries, chain entries ← 0; exFAT ← InUse bits cleared,
    /// bitmap bits cleared, FAT entries kept. WindowsFat32: as Camera and the SFN's high cluster word ← 0.
    /// A directory: its entries first (recursively), then itself.
    pub fn delete(&mut self, path: &str, style: DeleteStyle) -> &mut Self {
        let id = self.find(path);
        assert!(id != 0, "the root cannot be deleted");
        assert!(
            self.nodes[id].deleted.is_none(),
            "{path} is already deleted"
        );
        self.delete_node(id, style);
        self
    }

    fn delete_node(&mut self, id: usize, style: DeleteStyle) {
        let children: Vec<usize> = self.nodes[id].live.values().copied().collect();
        // Entry order, so the result does not depend on hash-map order.
        let mut children = children;
        children.sort_unstable();
        for child in children {
            self.delete_node(child, style);
        }
        let clusters = self.nodes[id].clusters.clone();
        for &c in &clusters {
            if self.owner[c as usize] == id as u32 + 1 {
                self.release(c);
                if self.fat_like() {
                    self.fat[c as usize] = 0;
                }
            }
        }
        let parent = self.nodes[id].parent;
        let name = self.nodes[id].name.clone();
        let sfn = self.nodes[id].sfn;
        self.nodes[parent].live.remove(&name);
        self.nodes[parent].sfns.remove(&sfn);
        self.nodes[id].deleted = Some(style);
    }

    /// Appended to `dir`'s entries.
    pub fn raw_entry(&mut self, dir: &str, entry: [u8; 32]) -> &mut Self {
        let dir = self.dir_at(dir);
        self.reserve(dir, 1);
        self.nodes[dir].items.push(Item::Raw(entry));
        self
    }

    /// FAT entry override (all FAT copies), applied last in `build`.
    pub fn set_fat(&mut self, cluster: u32, value: u32) -> &mut Self {
        assert!(
            cluster < self.geo.count + 2,
            "cluster {cluster} outside the FAT"
        );
        self.overrides.push((cluster, value));
        self
    }

    /// The whole volume in memory: peak memory = volume length (plus the files' data the builder holds).
    pub fn build(&self) -> Vec<u8> {
        let geo = &self.geo;
        let mut v = vec![0u8; (geo.total * geo.bps) as usize];
        let mut fat = self.fat.clone();
        fat[..2].copy_from_slice(&geo.reserved_entries());
        for &(c, e) in &self.overrides {
            fat[c as usize] = e;
        }
        geo.write_fats(&mut v, &geo.fat_image(&fat));
        match geo.fs {
            FsType::Exfat => {
                write_exfat_boot(&mut v, geo);
                let bitmap = geo.cluster_offset(2) as usize;
                for (i, _) in self.alloc.iter().enumerate().skip(2).filter(|(_, a)| **a) {
                    v[bitmap + (i - 2) / 8] |= 1 << ((i - 2) % 8);
                }
                let table = upcase_table();
                let at = geo.cluster_offset(geo.root_cluster - 1) as usize;
                v[at..at + table.len()].copy_from_slice(&table);
            }
            _ => {
                write_fat_boot(&mut v, geo);
                if geo.fs == FsType::Fat32 {
                    finish_fat32_boot(&mut v, geo, &fat);
                }
            }
        }
        for id in 0..self.nodes.len() {
            let content = if self.nodes[id].dir {
                self.dir_bytes(id)
            } else {
                self.nodes[id].data.clone()
            };
            if id == 0 && matches!(geo.fs, FsType::Fat12 | FsType::Fat16) {
                let at = (geo.root_start * geo.bps) as usize;
                v[at..at + content.len()].copy_from_slice(&content);
                continue;
            }
            let cl = geo.cluster_len() as usize;
            for (i, &c) in self.nodes[id].clusters.iter().enumerate() {
                if self.owner[c as usize] != id as u32 + 1 {
                    continue;
                }
                let at = geo.cluster_offset(c) as usize;
                let dst = &mut v[at..at + cl];
                dst.fill(0);
                let src = content.get(i * cl..).unwrap_or(&[]);
                let n = src.len().min(cl);
                dst[..n].copy_from_slice(&src[..n]);
            }
        }
        v
    }

    /// The bytes of directory `id`: the whole fixed root (FAT12/16) or its chain's clusters.
    fn dir_bytes(&self, id: usize) -> Vec<u8> {
        let geo = &self.geo;
        let len = if id == 0 && matches!(geo.fs, FsType::Fat12 | FsType::Fat16) {
            FAT_ROOT_ENTRIES * 32
        } else {
            self.nodes[id].clusters.len() as u64 * geo.cluster_len()
        };
        let mut out = Vec::with_capacity(len as usize);
        let node = &self.nodes[id];
        match (geo.fs, id) {
            (FsType::Exfat, 0) => {
                for e in exfat_root_entries(geo, geo.root_cluster - 1) {
                    out.extend_from_slice(&e);
                }
            }
            (FsType::Exfat, _) => {}
            (_, 0) => out.extend_from_slice(&fat_label_entry()),
            _ => {
                let parent = if node.parent == 0 {
                    0
                } else {
                    self.nodes[node.parent].clusters[0]
                };
                out.extend_from_slice(&self.dot_entry(*b".          ", node.clusters[0], node));
                out.extend_from_slice(&self.dot_entry(*b"..         ", parent, node));
            }
        }
        for item in &node.items {
            match item {
                Item::Raw(e) => out.extend_from_slice(e),
                Item::Node(child) if self.fat_like() => self.fat_entries(*child, &mut out),
                Item::Node(child) => self.exfat_entries(*child, &mut out),
            }
        }
        assert!(out.len() as u64 <= len);
        out.resize(len as usize, 0);
        out
    }

    fn dot_entry(&self, name: [u8; 11], cluster: u32, node: &Node) -> [u8; 32] {
        let mut e = [0u8; 32];
        e[..11].copy_from_slice(&name);
        e[11] = 0x10;
        self.fat_times(&mut e, node);
        if self.geo.fs == FsType::Fat32 {
            put16(&mut e, 20, (cluster >> 16) as u16);
        }
        put16(&mut e, 26, cluster as u16);
        e
    }

    fn fat_times(&self, e: &mut [u8; 32], node: &Node) {
        let (cd, ct) = node.created.fat();
        let (md, mt) = node.modified.fat();
        e[13] = node.created.tenths();
        put16(e, 14, ct);
        put16(e, 16, cd);
        put16(e, 18, md);
        put16(e, 22, mt);
        put16(e, 24, md);
    }

    /// LFN entries (if any) then the short entry of `id`.
    fn fat_entries(&self, id: usize, out: &mut Vec<u8>) {
        let node = &self.nodes[id];
        let deleted = node.deleted;
        if node.lfn {
            let sum = node
                .sfn
                .iter()
                .fold(0u8, |s, &c| s.rotate_right(1).wrapping_add(c));
            let mut units = node.name.clone();
            let n = units.len().div_ceil(13);
            if units.len() < n * 13 {
                units.push(0);
                units.resize(n * 13, 0xFFFF);
            }
            const OFFS: [usize; 13] = [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30];
            for ord in (1..=n).rev() {
                let mut e = [0u8; 32];
                e[0] = ord as u8 | if ord == n { 0x40 } else { 0 };
                e[11] = 0x0F;
                e[13] = sum;
                for (k, &off) in OFFS.iter().enumerate() {
                    put16(&mut e, off, units[(ord - 1) * 13 + k]);
                }
                if deleted.is_some() {
                    e[0] = 0xE5;
                }
                out.extend_from_slice(&e);
            }
        }
        let mut e = [0u8; 32];
        e[..11].copy_from_slice(&node.sfn);
        e[11] = if node.dir { 0x10 } else { 0x20 };
        self.fat_times(&mut e, node);
        let first = node.clusters.first().copied().unwrap_or(0);
        if self.geo.fs == FsType::Fat32 && deleted != Some(DeleteStyle::WindowsFat32) {
            put16(&mut e, 20, (first >> 16) as u16);
        }
        put16(&mut e, 26, first as u16);
        if !node.dir {
            put32(&mut e, 28, node.data.len() as u32);
        }
        if deleted.is_some() {
            e[0] = 0xE5;
        }
        out.extend_from_slice(&e);
    }

    /// The File, Stream Extension and File Name entries of `id`.
    fn exfat_entries(&self, id: usize, out: &mut Vec<u8>) {
        let node = &self.nodes[id];
        let names = node.name.len().div_ceil(15);
        let mut set = vec![0u8; 32 * (2 + names)];
        set[0] = 0x85;
        set[1] = (1 + names) as u8;
        put16(&mut set, 4, if node.dir { 0x10 } else { 0x20 });
        put32(&mut set, 8, node.created.exfat());
        put32(&mut set, 12, node.modified.exfat());
        put32(&mut set, 16, node.modified.exfat());
        set[20] = node.created.tenths();
        set[21] = node.modified.tenths();
        set[22] = node.created.exfat_offset();
        set[23] = node.modified.exfat_offset();
        set[24] = node.modified.exfat_offset();
        let s = &mut set[32..64];
        s[0] = 0xC0;
        s[1] = 0x01 | if node.no_fat_chain { 0x02 } else { 0 };
        s[3] = node.name.len() as u8;
        // NameHash over the up-cased name; the up-case table is the identity.
        let hash = node
            .name
            .iter()
            .flat_map(|c| c.to_le_bytes())
            .fold(0u16, exfat_sum16);
        put16(s, 4, hash);
        let len = if node.dir {
            node.clusters.len() as u64 * self.geo.cluster_len()
        } else {
            node.data.len() as u64
        };
        put64(s, 8, len);
        put32(s, 20, node.clusters.first().copied().unwrap_or(0));
        put64(s, 24, len);
        for (i, chunk) in node.name.chunks(15).enumerate() {
            let e = &mut set[64 + 32 * i..96 + 32 * i];
            e[0] = 0xC1;
            for (k, &c) in chunk.iter().enumerate() {
                put16(e, 2 + 2 * k, c);
            }
        }
        let sum = set
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != 2 && *i != 3)
            .fold(0u16, |s, (_, &b)| exfat_sum16(s, b));
        put16(&mut set, 2, sum);
        if node.deleted.is_some() {
            for e in set.chunks_mut(32) {
                e[0] &= 0x7F;
            }
        }
        out.extend_from_slice(&set);
    }

    /// As after a quick format with the same geometry: fresh boot region(s), FATs zeroed but for the root chain,
    /// empty root at the same first cluster (exFAT: fresh bitmap and up-case table), every other byte untouched.
    pub fn build_quick_formatted(&self) -> Vec<u8> {
        let mut v = self.build();
        quick_format_in_place(&mut v);
        v
    }

    /// From the volume start.
    pub fn cluster_offset(&self, cluster: u32) -> u64 {
        self.geo.cluster_offset(cluster)
    }

    pub fn cluster_count(&self) -> u32 {
        self.geo.count
    }

    /// File order (the live node at `path`, else the latest deleted one).
    pub fn clusters(&self, path: &str) -> Vec<u32> {
        self.nodes[self.find(path)].clusters.clone()
    }
}

/// Characters allowed in a short name besides A–Z and 0–9 (fatgen103 §6.1).
fn valid_sfn_char(c: u8) -> bool {
    c.is_ascii_uppercase() || c.is_ascii_digit() || b"$%'-_@~`!(){}^#&".contains(&c)
}

/// The short name of an upper-case 8.3 name, or `None`.
fn short_name(name: &[u16]) -> Option<[u8; 11]> {
    if name.iter().any(|&c| c >= 0x80) {
        return None;
    }
    let s: Vec<u8> = name.iter().map(|&c| c as u8).collect();
    let (base, ext) = match s.iter().position(|&c| c == b'.') {
        Some(d) => (&s[..d], &s[d + 1..]),
        None => (&s[..], &[][..]),
    };
    let ok = |part: &[u8], max| part.len() <= max && part.iter().all(|&c| valid_sfn_char(c));
    if base.is_empty() || !ok(base, 8) || !ok(ext, 3) || (s.contains(&b'.') && ext.is_empty()) {
        return None;
    }
    let mut sfn = [b' '; 11];
    sfn[..base.len()].copy_from_slice(base);
    sfn[8..8 + ext.len()].copy_from_slice(ext);
    Some(sfn)
}

/// Quick-formats a FAT or exFAT volume in place, reading its type and geometry from its own boot sector (also used
/// by Task 10's opt-in test on an image macOS made).
pub fn quick_format_in_place(volume: &mut [u8]) {
    let geo = Geo::read(volume);
    let bps = geo.bps as usize;
    let cl = geo.cluster_len() as usize;
    let mut fat = vec![0u32; 2];
    fat.copy_from_slice(&geo.reserved_entries());
    match geo.fs {
        FsType::Fat12 | FsType::Fat16 => {
            geo.write_fats(volume, &geo.fat_image(&fat));
            let at = (geo.root_start * geo.bps) as usize;
            let root = &mut volume[at..at + geo.root_sectors as usize * bps];
            root.fill(0);
            root[..32].copy_from_slice(&fat_label_entry());
        }
        FsType::Fat32 => {
            let root = geo.root_cluster;
            fat.resize(root as usize + 1, 0);
            fat[root as usize] = geo.eoc();
            geo.write_fats(volume, &geo.fat_image(&fat));
            let at = geo.cluster_offset(root) as usize;
            volume[at..at + cl].fill(0);
            volume[at..at + 32].copy_from_slice(&fat_label_entry());
            // FSInfo: every cluster but the root's is free.
            let fsinfo = u16::from_le_bytes([volume[48], volume[49]]) as usize * bps;
            if fsinfo > 0 && fsinfo + bps <= volume.len() {
                put32(volume, fsinfo + 488, geo.count - 1);
                put32(volume, fsinfo + 492, 0xFFFF_FFFF);
            }
            let backup = u16::from_le_bytes([volume[50], volume[51]]) as usize * bps;
            if backup > 0 && backup + 3 * bps <= volume.len() {
                volume.copy_within(0..3 * bps, backup);
            }
        }
        FsType::Exfat => {
            let bitmap_clusters = geo.bitmap_len().1;
            let root = geo.root_cluster;
            let mut upcase = 2 + bitmap_clusters as u32;
            if upcase == root {
                upcase += 1;
            }
            let last = root.max(upcase) as usize;
            fat.resize(last + 1, 0);
            let mut used = Vec::new();
            for c in 2..2 + bitmap_clusters as u32 {
                fat[c as usize] = if c + 1 < 2 + bitmap_clusters as u32 {
                    c + 1
                } else {
                    geo.eoc()
                };
                used.push(c);
            }
            fat[upcase as usize] = geo.eoc();
            fat[root as usize] = geo.eoc();
            used.extend([upcase, root]);
            geo.write_fats(volume, &geo.fat_image(&fat));
            let at = geo.cluster_offset(2) as usize;
            volume[at..at + bitmap_clusters as usize * cl].fill(0);
            for c in used {
                let i = c as usize - 2;
                volume[at + i / 8] |= 1 << (i % 8);
            }
            let table = upcase_table();
            let at = geo.cluster_offset(upcase) as usize;
            volume[at..at + cl].fill(0);
            volume[at..at + table.len()].copy_from_slice(&table);
            let at = geo.cluster_offset(root) as usize;
            volume[at..at + cl].fill(0);
            for (i, e) in exfat_root_entries(&geo, upcase).iter().enumerate() {
                volume[at + 32 * i..at + 32 * (i + 1)].copy_from_slice(e);
            }
            put16(volume, 106, 0);
            finish_exfat_boot(volume);
        }
    }
}

/// How a volume sits on a disk.
pub enum Partitioning {
    /// The volume starts at byte 0 (no partition table).
    Superfloppy,
    Mbr {
        first_lba: u32,
    },
    Gpt {
        first_lba: u64,
    },
}

/// The MBR partition type for a volume, from its own boot sector.
fn mbr_type(volume: &[u8]) -> u8 {
    match Geo::read(volume).fs {
        FsType::Fat12 => 0x01,
        FsType::Fat16 => 0x0E,
        FsType::Fat32 => 0x0C,
        FsType::Exfat => 0x07,
    }
}

/// One 16-byte MBR partition entry (CHS fields set to the "beyond CHS" marker).
fn mbr_entry(sector: &mut [u8], slot: usize, kind: u8, first_lba: u32, sectors: u32) {
    let e = &mut sector[446 + 16 * slot..462 + 16 * slot];
    e[0] = 0;
    e[1..4].copy_from_slice(&[0xFE, 0xFF, 0xFF]);
    e[4] = kind;
    e[5..8].copy_from_slice(&[0xFE, 0xFF, 0xFF]);
    put32(e, 8, first_lba);
    put32(e, 12, sectors);
}

fn sectors_of(volume: &[u8]) -> u64 {
    assert!(
        volume.len().is_multiple_of(512),
        "volume length must be a multiple of 512"
    );
    volume.len() as u64 / SECTOR
}

fn place(d: &mut [u8], lba: u64, volume: &[u8]) {
    let at = (lba * SECTOR) as usize;
    assert!(
        at + volume.len() <= d.len(),
        "the volume does not fit the disk"
    );
    d[at..at + volume.len()].copy_from_slice(volume);
}

/// Zeros of `disk_len` bytes holding `volume` at the partitioning's offset. MBR: one primary entry, type 0x01 (FAT12),
/// 0x0E (FAT16), 0x0C (FAT32), 0x07 (exFAT), read from the volume's own boot sector. GPT: protective MBR (0xEE),
/// header at LBA 1 and entries at LBA 2 (128 × 128 B), backup entries and header at the end, CRC32s set, one Basic
/// Data partition.
pub fn disk(p: Partitioning, volume: &[u8], disk_len: usize) -> Vec<u8> {
    assert!(
        disk_len.is_multiple_of(512),
        "disk length must be a multiple of 512"
    );
    let mut d = vec![0u8; disk_len];
    let sectors = sectors_of(volume);
    match p {
        Partitioning::Superfloppy => place(&mut d, 0, volume),
        Partitioning::Mbr { first_lba } => {
            assert!(first_lba >= 1, "the MBR takes sector 0");
            mbr_entry(&mut d, 0, mbr_type(volume), first_lba, sectors as u32);
            d[510] = 0x55;
            d[511] = 0xAA;
            place(&mut d, first_lba as u64, volume);
        }
        Partitioning::Gpt { first_lba } => {
            let total = disk_len as u64 / SECTOR;
            let last = total - 1;
            let (first_usable, last_usable) = (34, last - 33);
            assert!(
                first_lba >= first_usable && first_lba + sectors - 1 <= last_usable,
                "the volume must sit in the usable LBAs {first_usable}..={last_usable}"
            );
            mbr_entry(&mut d, 0, 0xEE, 1, last.min(u32::MAX as u64) as u32);
            d[510] = 0x55;
            d[511] = 0xAA;
            let mut entries = vec![0u8; 128 * 128];
            // Basic Data partition type EBD0A0A2-B9E5-4433-87C0-68B6B72699C7 (mixed-endian on disk).
            entries[..16].copy_from_slice(&[
                0xA2, 0xA0, 0xD0, 0xEB, 0xE5, 0xB9, 0x33, 0x44, 0x87, 0xC0, 0x68, 0xB6, 0xB7, 0x26,
                0x99, 0xC7,
            ]);
            entries[16..32].copy_from_slice(&[
                0x52, 0x45, 0x56, 0x4E, 0x41, 0x4E, 0x54, 0x40, 0x80, 0x01, 0x02, 0x03, 0x04, 0x05,
                0x06, 0x07,
            ]);
            put64(&mut entries, 32, first_lba);
            put64(&mut entries, 40, first_lba + sectors - 1);
            for (i, c) in LABEL.encode_utf16().enumerate() {
                put16(&mut entries, 56 + 2 * i, c);
            }
            let entries_crc = crc32(&entries);
            let header = |my: u64, alternate: u64, entries_lba: u64| {
                let mut h = vec![0u8; 512];
                h[..8].copy_from_slice(b"EFI PART");
                put32(&mut h, 8, 0x0001_0000);
                put32(&mut h, 12, 92);
                put64(&mut h, 24, my);
                put64(&mut h, 32, alternate);
                put64(&mut h, 40, first_usable);
                put64(&mut h, 48, last_usable);
                h[56..72].copy_from_slice(&[
                    0x52, 0x45, 0x56, 0x4E, 0x41, 0x4E, 0x54, 0x40, 0x80, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0x01,
                ]);
                put64(&mut h, 72, entries_lba);
                put32(&mut h, 80, 128);
                put32(&mut h, 84, 128);
                put32(&mut h, 88, entries_crc);
                let crc = crc32(&h[..92]);
                put32(&mut h, 16, crc);
                h
            };
            place(&mut d, 1, &header(1, last, 2));
            place(&mut d, 2, &entries);
            place(&mut d, last - 32, &entries);
            place(&mut d, last, &header(last, 1, last - 32));
            place(&mut d, first_lba, volume);
        }
    }
    d
}

/// MBR with `primary` entries `(first_lba, volume, type)` and, when `logical` is not empty, an extended partition
/// (type 0x0F) after them holding one EBR per logical volume, chained. Each EBR is the sector just before its volume.
pub fn mbr_disk(
    primary: &[(u32, &[u8], u8)],
    logical: &[(u32, &[u8], u8)],
    disk_len: usize,
) -> Vec<u8> {
    assert!(
        disk_len.is_multiple_of(512),
        "disk length must be a multiple of 512"
    );
    let slots = primary.len() + usize::from(!logical.is_empty());
    assert!(slots <= 4, "an MBR holds 4 entries");
    let mut d = vec![0u8; disk_len];
    for (i, &(lba, volume, kind)) in primary.iter().enumerate() {
        mbr_entry(&mut d, i, kind, lba, sectors_of(volume) as u32);
        place(&mut d, lba as u64, volume);
    }
    if let Some(&(first, _, _)) = logical.first() {
        let ext = first - 1;
        let (end_lba, end_vol, _) = logical[logical.len() - 1];
        let end = end_lba + sectors_of(end_vol) as u32;
        mbr_entry(&mut d, primary.len(), 0x0F, ext, end - ext);
        let mut previous_end = 0u32;
        for (k, &(lba, volume, kind)) in logical.iter().enumerate() {
            let ebr = lba - 1;
            assert!(
                ebr >= previous_end && ebr > 0,
                "logical volumes need a free sector before them"
            );
            previous_end = lba + sectors_of(volume) as u32;
            let at = ebr as usize * 512;
            let sector = &mut d[at..at + 512];
            mbr_entry(sector, 0, kind, 1, sectors_of(volume) as u32);
            if let Some(&(next, next_vol, _)) = logical.get(k + 1) {
                mbr_entry(
                    sector,
                    1,
                    0x05,
                    next - 1 - ext,
                    1 + sectors_of(next_vol) as u32,
                );
            }
            sector[510] = 0x55;
            sector[511] = 0xAA;
            place(&mut d, lba as u64, volume);
        }
    }
    d[510] = 0x55;
    d[511] = 0xAA;
    d
}

const CRC_TABLE: [u32; 256] = {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
};

/// IEEE 802.3, reflected, init/xorout 0xFFFF_FFFF.
pub fn crc32(data: &[u8]) -> u32 {
    !data.iter().fold(0xFFFF_FFFFu32, |c, &b| {
        CRC_TABLE[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8)
    })
}
