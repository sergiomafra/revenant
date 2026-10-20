//! The live directory tree of a FAT12/16/32 volume.
//!
//! From Microsoft's "FAT32 File System Specification" 1.03 (fatgen103): FAT entry packing and end-of-chain values (§4),
//! the directory entry (§6) and long names (§7). Live entries only; deleted ones are another walk's job.

use std::collections::HashMap;

use super::alloc::{Claims, ClusterMap, Entry, follow};
use super::boot::{self, Layout, Root};
use super::listing::{Budget, Collector, FsOrigin, FsState, FsTime, Listing, Placement, component};
use super::{FsKind, Limits, Volume, read_exact, u16_at, u32_at};
use crate::scan::CancelToken;
use crate::source::Source;

/// FAT bytes read at once (and cached).
const FAT_BLOCK: u64 = 32 * 1024;
/// FAT blocks kept (8 MiB); the cache is emptied when it fills.
const FAT_CACHE_BLOCKS: usize = 256;
/// Most parts of one long name (255 characters).
const MAX_LFN_PARTS: usize = 20;
/// Where the 13 UTF-16 units of a long-name entry are.
const LFN_UNITS: [usize; 13] = [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30];

/// Lists the live files and directories of FAT volume `v` into `out`.
///
/// The root (FAT12/16: the fixed region; FAT32: the `RootClus` chain) is walked depth first, entries in on-disk order.
/// Every directory and every live file claims its chain; a directory whose first cluster is already claimed is not
/// entered (a loop or a second link). Bounds: [`Limits`] (directories, entries, depth, bytes per directory) and the
/// listing's budget.
pub(super) fn walk(
    source: &dyn Source,
    v: &Volume,
    cancel: &CancelToken,
    limits: &Limits,
    budget: &mut Budget,
    out: &mut Listing,
) {
    let Some(layout) = boot::layout(source, v) else {
        return;
    };
    let mut map = FatMap::new(source, v, &layout);
    let mut claims = Claims::new(v.cluster_count, limits.contested);
    let mut col = Collector::new(v, source.len());
    let dir_clusters = limits.fat_dir_bytes.div_ceil(v.cluster_size).max(1);
    let fat32 = v.fs == FsKind::Fat32;
    let root = match layout.root {
        Root::Fixed { sector, entries } => Region::Fixed {
            offset: sector,
            len: entries * 32,
            sector_size: layout.sector_size,
        },
        Root::Cluster(c) => {
            let chain = follow(&mut map, &mut claims, c, dir_clusters);
            col.dir(budget, &chain);
            Region::Runs(chain.runs)
        }
    };
    let mut stack = vec![Dir {
        path: String::new(),
        depth: 0,
        region: root,
    }];
    let mut dirs = 1usize;
    let mut entries = 0u64;
    while let Some(d) = stack.pop() {
        if cancel.is_cancelled() {
            break;
        }
        let children = read_dir(source, v, &d.region, fat32, &mut entries, limits.entries);
        let mut subdirs = Vec::new();
        for ch in children {
            let path = if d.path.is_empty() {
                ch.name
            } else {
                format!("{}/{}", d.path, ch.name)
            };
            if ch.dir {
                if d.depth >= limits.depth
                    || dirs >= limits.dirs
                    || !claims.in_heap(ch.first)
                    || claims.is_claimed(ch.first)
                {
                    continue;
                }
                dirs += 1;
                let chain = follow(&mut map, &mut claims, ch.first, dir_clusters);
                col.dir(budget, &chain);
                subdirs.push(Dir {
                    path,
                    depth: d.depth + 1,
                    region: Region::Runs(chain.runs),
                });
            } else if ch.size > 0 {
                let need = (ch.size as u64).div_ceil(v.cluster_size);
                let chain = follow(&mut map, &mut claims, ch.first, need);
                let origin = FsOrigin {
                    volume: v.index,
                    path,
                    state: FsState::Live,
                    placement: Placement::Chain,
                    size: ch.size as u64,
                    created: ch.created,
                    modified: ch.modified,
                };
                col.file(budget, origin, chain);
            }
        }
        stack.extend(subdirs.into_iter().rev());
    }
    col.finish(&claims, out);
}

/// A directory waiting to be read.
struct Dir {
    path: String,
    depth: u32,
    region: Region,
}

/// Where a directory's entries are.
enum Region {
    /// The FAT12/16 root: `len` bytes at absolute `offset`.
    Fixed {
        offset: u64,
        len: u64,
        sector_size: u64,
    },
    /// Cluster runs, in order.
    Runs(Vec<(u32, u32)>),
}

/// A live entry of a directory.
#[derive(Debug)]
struct Child {
    /// One path component (sanitized).
    name: String,
    dir: bool,
    first: u32,
    size: u32,
    created: Option<FsTime>,
    modified: Option<FsTime>,
}

/// The live entries of a directory, up to the end-of-directory entry or the volume's entry budget. An unreadable
/// sector or cluster is skipped (its entries are lost, and a long name across it is not used).
fn read_dir(
    source: &dyn Source,
    v: &Volume,
    region: &Region,
    fat32: bool,
    entries: &mut u64,
    cap: u64,
) -> Vec<Child> {
    let mut r = DirReader {
        fat32,
        entries,
        cap,
        long: LongName::default(),
        out: Vec::new(),
    };
    match *region {
        Region::Fixed {
            offset,
            len,
            sector_size,
        } => match read_exact(source, offset, len as usize) {
            Some(all) => {
                r.feed(Some(&all));
            }
            None => {
                let mut at = 0;
                while at < len {
                    let n = sector_size.min(len - at);
                    if !r.feed(read_exact(source, offset + at, n as usize).as_deref()) {
                        break;
                    }
                    at += n;
                }
            }
        },
        Region::Runs(ref runs) => {
            'runs: for &(first, n) in runs {
                for c in first..first + n {
                    let Some(at) = v.cluster_offset(c) else {
                        break 'runs;
                    };
                    let cluster = read_exact(source, at, v.cluster_size as usize);
                    if !r.feed(cluster.as_deref()) {
                        break 'runs;
                    }
                }
            }
        }
    }
    r.out
}

struct DirReader<'a> {
    fat32: bool,
    entries: &'a mut u64,
    cap: u64,
    long: LongName,
    out: Vec<Child>,
}

impl DirReader<'_> {
    /// Parses one chunk of entries (`None`: unreadable); false once the directory has ended.
    fn feed(&mut self, chunk: Option<&[u8]>) -> bool {
        let Some(chunk) = chunk else {
            self.long = LongName::default();
            return true;
        };
        for e in chunk.as_chunks::<32>().0 {
            if *self.entries >= self.cap {
                return false;
            }
            *self.entries += 1;
            match e[0] {
                0x00 => return false,
                0xE5 => {
                    self.long = LongName::default();
                    continue;
                }
                _ => {}
            }
            let attr = e[11];
            if attr & 0x3F == 0x0F {
                self.long.part(e);
                continue;
            }
            let long = std::mem::take(&mut self.long).name(&e[..11]);
            if attr & 0x08 != 0 || is_dot(e) {
                continue;
            }
            let lo = u16_at(e, 26) as u32;
            let first = if self.fat32 {
                (u16_at(e, 20) as u32) << 16 | lo
            } else {
                lo
            };
            self.out.push(Child {
                name: component(&long.unwrap_or_else(|| short_name(e))),
                dir: attr & 0x10 != 0,
                first,
                size: u32_at(e, 28),
                created: fat_time(u16_at(e, 16), u16_at(e, 14), Some(e[13])),
                modified: fat_time(u16_at(e, 24), u16_at(e, 22), None),
            });
        }
        true
    }
}

/// The `.` and `..` entries.
fn is_dot(e: &[u8; 32]) -> bool {
    e[..11] == *b".          " || e[..11] == *b"..         "
}

/// A long name being collected from its parts (stored last part first, ordinals counting down to 1).
struct LongName {
    units: [u16; MAX_LFN_PARTS * 13],
    parts: usize,
    /// The ordinal the next part must have; 0 once part 1 is in.
    next: usize,
    sum: u8,
    active: bool,
}

impl Default for LongName {
    fn default() -> Self {
        LongName {
            units: [0; MAX_LFN_PARTS * 13],
            parts: 0,
            next: 0,
            sum: 0,
            active: false,
        }
    }
}

impl LongName {
    fn part(&mut self, e: &[u8; 32]) {
        let n = (e[0] & 0x3F) as usize;
        if e[0] & 0x40 != 0 {
            if n == 0 || n > MAX_LFN_PARTS {
                *self = LongName::default();
                return;
            }
            self.active = true;
            self.parts = n;
            self.sum = e[13];
        } else if !(self.active && n >= 1 && n == self.next && e[13] == self.sum) {
            *self = LongName::default();
            return;
        }
        for (k, &off) in LFN_UNITS.iter().enumerate() {
            self.units[(n - 1) * 13 + k] = u16_at(e, off);
        }
        self.next = n - 1;
    }

    /// The name, when every part down to 1 came right before this short name and each carries its checksum.
    fn name(self, sfn: &[u8]) -> Option<String> {
        let sum = sfn
            .iter()
            .fold(0u8, |s, &c| s.rotate_right(1).wrapping_add(c));
        if !self.active || self.next != 0 || sum != self.sum {
            return None;
        }
        let units = &self.units[..self.parts * 13];
        let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
        (end > 0).then(|| String::from_utf16_lossy(&units[..end]))
    }
}

/// The 8.3 name: base and extension trimmed of trailing spaces, joined by '.' when the extension is not empty, the NT
/// lower-case bits of byte 12 applied (0x08 base, 0x10 extension), a first byte 0x05 read as 0xE5, and any byte outside
/// printable ASCII (an OEM code page character) read as '_'.
fn short_name(e: &[u8; 32]) -> String {
    let mut raw = [0u8; 11];
    raw.copy_from_slice(&e[..11]);
    if raw[0] == 0x05 {
        raw[0] = 0xE5;
    }
    let text = |part: &[u8], lower: bool| -> String {
        let end = part.iter().rposition(|&c| c != b' ').map_or(0, |i| i + 1);
        part[..end]
            .iter()
            .map(|&c| match c {
                0x20..=0x7E if lower => (c as char).to_ascii_lowercase(),
                0x20..=0x7E => c as char,
                _ => '_',
            })
            .collect()
    };
    let base = text(&raw[..8], e[12] & 0x08 != 0);
    let ext = text(&raw[8..], e[12] & 0x10 != 0);
    if ext.is_empty() {
        base
    } else {
        format!("{base}.{ext}")
    }
}

/// A FAT date and time (local), with the creation time's 10 ms count (0..=199) added and floored to the second.
/// `None` when any field is out of range.
fn fat_time(date: u16, time: u16, centis: Option<u8>) -> Option<FsTime> {
    let year = 1980 + (date >> 9) as u32;
    let month = ((date >> 5) & 0x0F) as u32;
    let day = (date & 0x1F) as u32;
    let hour = (time >> 11) as u32;
    let min = ((time >> 5) & 0x3F) as u32;
    let mut sec = (time & 0x1F) as u32 * 2;
    if !(1..=12).contains(&month)
        || day == 0
        || day > days_in(year, month)
        || hour > 23
        || min > 59
        || sec > 58
    {
        return None;
    }
    if let Some(c) = centis {
        if c > 199 {
            return None;
        }
        sec += c as u32 / 100;
    }
    Some(FsTime {
        local: format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}"),
        utc_offset_minutes: None,
    })
}

fn days_in(year: u32, month: u32) -> u32 {
    match month {
        2 if (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400) => {
            29
        }
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// A FAT read through a block cache; a sector the first copy cannot give is read from the next copy.
struct FatMap<'a> {
    source: &'a dyn Source,
    fs: FsKind,
    count: u32,
    layout: Layout,
    cache: HashMap<u64, Block>,
}

struct Block {
    data: Vec<u8>,
    /// Sectors no copy could give.
    bad: Vec<bool>,
}

impl<'a> FatMap<'a> {
    fn new(source: &'a dyn Source, v: &Volume, layout: &Layout) -> Self {
        FatMap {
            source,
            fs: v.fs,
            count: v.cluster_count,
            layout: layout.clone(),
            cache: HashMap::new(),
        }
    }

    fn load(&self, i: u64) -> Block {
        let l = &self.layout;
        let start = i * FAT_BLOCK;
        let len = FAT_BLOCK.min(l.fat_bytes - start);
        let sectors = len.div_ceil(l.sector_size) as usize;
        if let Some(data) = read_exact(self.source, l.fat_offset + start, len as usize) {
            return Block {
                data,
                bad: vec![false; sectors],
            };
        }
        let mut data = vec![0u8; len as usize];
        let mut bad = vec![false; sectors];
        for (s, bad) in bad.iter_mut().enumerate() {
            let off = s as u64 * l.sector_size;
            let n = l.sector_size.min(len - off);
            let copy = (0..l.fat_copies).find_map(|k| {
                let at = l.fat_offset + k * l.fat_bytes + start + off;
                read_exact(self.source, at, n as usize)
            });
            match copy {
                Some(b) => data[off as usize..(off + n) as usize].copy_from_slice(&b),
                None => *bad = true,
            }
        }
        Block { data, bad }
    }

    /// `N` bytes of the FAT at `off` (one cache lookup unless they straddle two blocks); `None` past the FAT or when a
    /// sector they lie in could not be read from any copy.
    fn bytes<const N: usize>(&mut self, off: u64) -> Option<[u8; N]> {
        let end = off.checked_add(N as u64)?;
        if end > self.layout.fat_bytes {
            return None;
        }
        let (i, r) = (off / FAT_BLOCK, (off % FAT_BLOCK) as usize);
        if r + N > FAT_BLOCK as usize {
            let mut out = [0u8; N];
            for (k, x) in out.iter_mut().enumerate() {
                *x = self.bytes::<1>(off + k as u64)?[0];
            }
            return Some(out);
        }
        if !self.cache.contains_key(&i) {
            if self.cache.len() >= FAT_CACHE_BLOCKS {
                self.cache.clear();
            }
            let block = self.load(i);
            self.cache.insert(i, block);
        }
        let block = &self.cache[&i];
        let bps = self.layout.sector_size as usize;
        if block.bad[r / bps] || block.bad[(r + N - 1) / bps] {
            return None;
        }
        block.data[r..r + N].try_into().ok()
    }

    /// The raw entry of cluster `c` (FAT32: its low 28 bits).
    fn raw(&mut self, c: u32) -> Option<u32> {
        let c64 = c as u64;
        match self.fs {
            FsKind::Fat12 => {
                let v = u16::from_le_bytes(self.bytes(c64 + c64 / 2)?) as u32;
                Some(if c & 1 == 1 { v >> 4 } else { v & 0x0FFF })
            }
            FsKind::Fat16 => Some(u16::from_le_bytes(self.bytes(2 * c64)?) as u32),
            FsKind::Fat32 | FsKind::Exfat => {
                Some(u32::from_le_bytes(self.bytes(4 * c64)?) & 0x0FFF_FFFF)
            }
        }
    }

    fn in_heap(&self, c: u32) -> bool {
        c >= 2 && c as u64 <= self.count as u64 + 1
    }
}

impl ClusterMap for FatMap<'_> {
    fn entry(&mut self, c: u32) -> Entry {
        if !self.in_heap(c) {
            return Entry::Invalid;
        }
        let Some(v) = self.raw(c) else {
            return Entry::Invalid;
        };
        let (bad, eoc) = match self.fs {
            FsKind::Fat12 => (0xFF7, 0xFF8),
            FsKind::Fat16 => (0xFFF7, 0xFFF8),
            _ => (0x0FFF_FFF7, 0x0FFF_FFF8),
        };
        match v {
            0 => Entry::Free,
            v if v >= eoc => Entry::End,
            v if v == bad => Entry::Bad,
            v if self.in_heap(v) => Entry::Next(v),
            _ => Entry::Invalid,
        }
    }

    fn allocated(&mut self, c: u32) -> Option<bool> {
        if !self.in_heap(c) {
            return None;
        }
        self.raw(c).map(|v| v != 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filesystem::listing::read_with;
    use crate::formats::tests::Mem;
    use revenant_testkit::fs::{FsBuilder, FsType, Partitioning, disk};

    const VOL: u64 = 8192 * 512;

    fn card() -> FsBuilder {
        FsBuilder::new(FsType::Fat32, 48 << 20, 512)
    }

    fn list(b: &FsBuilder, limits: &Limits) -> Listing {
        let v = b.build();
        let len = v.len() + (8 << 20);
        let src = Mem(disk(Partitioning::Mbr { first_lba: 8192 }, &v, len));
        read_with(&src, &CancelToken::default(), limits)
    }

    fn paths(l: &Listing) -> Vec<&str> {
        let mut p: Vec<&str> = l.files.iter().map(|f| f.origin.path.as_str()).collect();
        p.sort();
        p
    }

    fn in_live(l: &Listing, b: &FsBuilder, c: u32) -> bool {
        let at = VOL + b.cluster_offset(c);
        l.live.iter().any(|r| r.start <= at && at < r.start + r.len)
    }

    #[test]
    fn contested_overflow_makes_the_volume_untrusted() {
        let mut b = card();
        b.add("DCIM/OK.JPG", &[1; 1024]);
        // Three cross-links, each contesting one cluster.
        for i in 0..3 {
            let x = b.add(&format!("DCIM/X{i}.JPG"), &[2; 1024]);
            let y = b.add(&format!("DCIM/Y{i}.JPG"), &[3; 1024]);
            b.set_fat(y[0], x[1]);
        }
        let dcim = b.clusters("DCIM")[0];
        let ok = b.clusters("DCIM/OK.JPG")[0];
        // Room for the three: only the cross-linked files are untrusted.
        let l = list(
            &b,
            &Limits {
                contested: 3,
                ..Limits::DEFAULT
            },
        );
        assert_eq!(l.files.len(), 7);
        let trusted: Vec<&str> = l
            .files
            .iter()
            .filter(|f| f.trusted)
            .map(|f| f.origin.path.as_str())
            .collect();
        assert_eq!(trusted, ["DCIM/OK.JPG"]);
        assert!(in_live(&l, &b, dcim));
        // The set fills: every live file of the volume is untrusted, and so are its directories.
        let l = list(
            &b,
            &Limits {
                contested: 2,
                ..Limits::DEFAULT
            },
        );
        assert_eq!(l.files.len(), 7);
        assert!(l.files.iter().all(|f| !f.trusted));
        assert!(!in_live(&l, &b, dcim));
        assert!(in_live(&l, &b, ok));
    }

    #[test]
    fn piece_cap_bounds_the_listing() {
        let mut b = card();
        b.add("DCIM/A.JPG", &[1; 512]);
        b.add_at("DCIM/FRAG.JPG", &[2; 1536], &[3000, 3002, 3004]);
        b.add("DCIM/C.JPG", &[3; 512]);
        b.add("DCIM/D.TXT", &[4; 512]);
        let e = b.add("DCIM/E.JPG", &[5; 512]);
        // The root and DCIM take one piece each, A one: two are left, FRAG needs three.
        let l = list(
            &b,
            &Limits {
                pieces: 5,
                ..Limits::DEFAULT
            },
        );
        assert_eq!(paths(&l), ["DCIM/A.JPG", "DCIM/C.JPG"]);
        assert_eq!(l.others.len(), 1);
        assert!(!in_live(&l, &b, 3000) && !in_live(&l, &b, e[0]));
        let pieces = l.files.iter().map(|f| f.pieces.len()).sum::<usize>() + l.others.len() + 2;
        assert!(pieces <= 5);
        // Nothing left at all: no file, no panic.
        let l = list(
            &b,
            &Limits {
                pieces: 0,
                ..Limits::DEFAULT
            },
        );
        assert!(l.files.is_empty() && l.others.is_empty() && l.live.is_empty());
        assert_eq!(l.volumes.len(), 1);
    }

    #[test]
    fn photo_cap_bounds_the_listing() {
        let mut b = card();
        for name in ["A.JPG", "B.TXT", "C.JPG", "D.JPG"] {
            b.add(&format!("DCIM/{name}"), &[1; 512]);
        }
        let l = list(
            &b,
            &Limits {
                files: 2,
                ..Limits::DEFAULT
            },
        );
        assert_eq!(paths(&l), ["DCIM/A.JPG", "DCIM/C.JPG"]);
        assert_eq!(l.others.len(), 1);
        assert!(!in_live(&l, &b, b.clusters("DCIM/D.JPG")[0]));
    }

    #[test]
    fn directory_depth_count_and_entries_are_capped() {
        let mut b = card();
        b.add("TOP.JPG", &[1; 512]);
        b.add("A/A1.JPG", &[1; 512]);
        b.add("A/B/B1.JPG", &[1; 512]);
        b.add("A/B/C/C1.JPG", &[1; 512]);
        b.add("Z/Z1.JPG", &[1; 512]);
        let all = [
            "A/A1.JPG",
            "A/B/B1.JPG",
            "A/B/C/C1.JPG",
            "TOP.JPG",
            "Z/Z1.JPG",
        ];
        assert_eq!(paths(&list(&b, &Limits::DEFAULT)), all);
        // Depth 2: A/B is walked, A/B/C is not.
        let l = list(
            &b,
            &Limits {
                depth: 2,
                ..Limits::DEFAULT
            },
        );
        assert_eq!(paths(&l), ["A/A1.JPG", "A/B/B1.JPG", "TOP.JPG", "Z/Z1.JPG"]);
        // Three directories: the root, A and Z (found while reading the root), not A/B.
        let l = list(
            &b,
            &Limits {
                dirs: 3,
                ..Limits::DEFAULT
            },
        );
        assert_eq!(paths(&l), ["A/A1.JPG", "TOP.JPG", "Z/Z1.JPG"]);
        // Entries: the root's label, TOP, A, Z and its end marker, then A's `.`, `..` and A1.
        let l = list(
            &b,
            &Limits {
                entries: 8,
                ..Limits::DEFAULT
            },
        );
        assert_eq!(paths(&l), ["A/A1.JPG", "TOP.JPG"]);
        let l = list(
            &b,
            &Limits {
                entries: 7,
                ..Limits::DEFAULT
            },
        );
        assert_eq!(paths(&l), ["TOP.JPG"]);
        // A directory of at most 2 clusters: a third cluster's entries are not read, and it is untrusted.
        let mut b = card();
        for i in 0..40 {
            b.add(&format!("D/F{i:02}.JPG"), &[1; 512]);
        }
        let d = b.clusters("D");
        assert_eq!(d.len(), 3);
        let l = list(
            &b,
            &Limits {
                fat_dir_bytes: 1024,
                ..Limits::DEFAULT
            },
        );
        assert_eq!(l.files.len(), 16 + 14);
        assert!(!in_live(&l, &b, d[0]));
    }

    fn entry(name: &[u8; 11], case: u8) -> [u8; 32] {
        let mut e = [0u8; 32];
        e[..11].copy_from_slice(name);
        e[12] = case;
        e
    }

    #[test]
    fn short_names() {
        assert_eq!(short_name(&entry(b"DSC01234ARW", 0)), "DSC01234.ARW");
        assert_eq!(short_name(&entry(b"README     ", 0)), "README");
        assert_eq!(short_name(&entry(b"DSC01234ARW", 0x08)), "dsc01234.ARW");
        assert_eq!(short_name(&entry(b"DSC01234ARW", 0x10)), "DSC01234.arw");
        assert_eq!(short_name(&entry(b"DSC01234ARW", 0x18)), "dsc01234.arw");
        assert_eq!(short_name(&entry(b"\x05BC     JPG", 0)), "_BC.JPG");
        assert_eq!(short_name(&entry(b"A\x7FB\x80    JPG", 0)), "A_B_.JPG");
        assert_eq!(short_name(&entry(b"A B     JPG", 0)), "A B.JPG");
    }

    #[test]
    fn times() {
        let t = |d, t, c| fat_time(d, t, c).map(|x| x.local);
        let d = |y: u16, m: u16, day: u16| (y - 1980) << 9 | m << 5 | day;
        let h = |h: u16, m: u16, s: u16| h << 11 | m << 5 | (s / 2);
        assert_eq!(
            t(d(1980, 1, 1), 0, None).as_deref(),
            Some("1980-01-01T00:00:00")
        );
        assert_eq!(
            t(d(2107, 12, 31), h(23, 59, 58), Some(100)).as_deref(),
            Some("2107-12-31T23:59:59")
        );
        assert_eq!(
            t(d(2000, 2, 29), 0, None).as_deref(),
            Some("2000-02-29T00:00:00")
        );
        assert_eq!(t(d(2100, 2, 29), 0, None), None);
        assert_eq!(t(d(2025, 6, 31), 0, None), None);
        assert_eq!(t(d(2025, 6, 30), h(24, 0, 0), None), None);
        assert_eq!(t(d(2025, 6, 30), 0, Some(200)), None);
        assert_eq!(
            t(d(2025, 6, 30), h(1, 2, 3), Some(99)).as_deref(),
            Some("2025-06-30T01:02:02")
        );
    }
}
