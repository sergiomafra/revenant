//! Old directories after a quick format (ruling D11). A quick format writes a new, empty root and new FATs (exFAT: a
//! new bitmap) but leaves the old directories' clusters as they were. The scan meets them in the blocks it reads: a FAT
//! directory's first cluster starts with its `.` and `..` entries, an exFAT one with an entry set. Their entries only
//! name the scan's findings (never place data): a file entry whose first cluster is where a finding without origin
//! starts, with the same kind, gives that finding its old name and folder.
//!
//! Entries are parsed with the live walk's rules ([`super::fat::OldDir`], [`super::exfat::OldSets`]); erased entries
//! and deleted entry sets are left out (their names are not whole, and their clusters may have been reused).

use std::collections::BTreeMap;
use std::sync::Arc;

use super::exfat::{OldSets, OldStep, starts_set};
use super::fat::OldDir;
use super::listing::extension;
use super::{
    FsKind, FsTime, MAX_DEPTH, MAX_ENTRIES, MAX_FAT_DIR_BYTES, MAX_PATH_BUDGET, MAX_PATH_BYTES,
    Volume, read_exact, u16_at,
};
use crate::formats::{FileKind, Fragment};
use crate::scan::CancelToken;
use crate::source::Source;

/// Bytes of a cluster start the FAT check looks at: the `.` and `..` entries.
const FAT_HEAD: u64 = 64;
/// Bytes of a cluster start the exFAT check may look at: the longest entry set (19 entries).
const EXFAT_HEAD: u64 = 19 * 32;
/// The longest head any volume needs.
pub(crate) const MAX_HEAD: u64 = EXFAT_HEAD;

/// One intact entry of an old directory.
pub(super) struct OldEntry {
    /// One path component (sanitized).
    pub name: String,
    pub dir: bool,
    pub first: u32,
    /// As recorded (exFAT: DataLength).
    pub size: u64,
    pub created: Option<FsTime>,
    pub modified: Option<FsTime>,
}

/// An old directory's first cluster met by the scan: its absolute offset, its volume (index) and, on FAT, the first
/// cluster its `..` entry names (0 for the root, and always 0 on exFAT). 16 bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct OrphanDir {
    pub at: u64,
    pub volume: u32,
    pub parent: u32,
}

/// The bytes of a cluster start the check of `v` looks at (never more than a cluster).
pub(crate) fn head_len(v: &Volume) -> u64 {
    match v.fs {
        FsKind::Exfat => EXFAT_HEAD.min(v.cluster_size),
        _ => FAT_HEAD.min(v.cluster_size),
    }
}

/// The cluster grid of one volume.
struct Grid {
    volume: u32,
    fs: FsKind,
    heap: u64,
    cluster: u64,
    /// The heap's end.
    end: u64,
    head: u64,
}

/// `Some(parent)` when `w` (the head of a cluster of a volume of type `fs`) starts an old directory: FAT, a `.` then a
/// `..` entry, both with the directory attribute (`parent`: the `..` entry's cluster); exFAT, an entry set
/// ([`starts_set`]; `parent` 0).
fn starts_dir(fs: FsKind, w: &[u8]) -> Option<u32> {
    match fs {
        FsKind::Exfat => starts_set(w).then_some(0),
        _ => {
            let ok = w.len() >= 64
                && w[..11] == *b".          "
                && w[11] & 0x10 != 0
                && w[32..43] == *b"..         "
                && w[43] & 0x10 != 0;
            let lo = || u16_at(w, 58) as u32;
            ok.then(|| match fs {
                FsKind::Fat32 => (u16_at(w, 52) as u32) << 16 | lo(),
                _ => lo(),
            })
        }
    }
}

/// Searches the bytes the read stage reads, in order, for old directories: at each cluster start of a volume, the
/// check of [`starts_dir`] over its first [`head_len`] bytes. Keeps the last `MAX_HEAD - 1` bytes seen, so a head across
/// two windows or blocks is found too; a block that is not read (deferred) breaks the stream. Each candidate is found
/// with the bytes where its head ends. With no volume it does nothing.
pub(crate) struct OrphanFinder {
    grids: Vec<Grid>,
    tail: Vec<u8>,
    tail_end: u64,
}

impl OrphanFinder {
    pub(crate) fn new(volumes: &[Volume]) -> Self {
        let grids = volumes
            .iter()
            .filter(|v| v.cluster_size > 0)
            .map(|v| Grid {
                volume: v.index,
                fs: v.fs,
                heap: v.heap_offset,
                cluster: v.cluster_size,
                end: v
                    .heap_offset
                    .saturating_add(u64::from(v.cluster_count).saturating_mul(v.cluster_size)),
                head: head_len(v),
            })
            .collect();
        OrphanFinder {
            grids,
            tail: Vec::new(),
            tail_end: 0,
        }
    }

    /// Forgets the tail: the next bytes do not follow the last ones.
    pub(crate) fn reset(&mut self) {
        self.tail.clear();
    }

    /// Looks at `bytes`, which start at the absolute offset `from`; pushes each candidate whose head ends in them to
    /// `out` (those of one call sorted) until it holds `cap`, then sets `capped`.
    pub(crate) fn feed(
        &mut self,
        from: u64,
        bytes: &[u8],
        out: &mut Vec<OrphanDir>,
        cap: usize,
        capped: &mut bool,
    ) {
        if self.grids.is_empty() || bytes.is_empty() {
            return;
        }
        if self.tail_end != from {
            self.tail.clear();
        }
        let t = self.tail.len();
        let start = from - t as u64;
        let stop = from + bytes.len() as u64;
        let keep = MAX_HEAD as usize - 1;
        // The tail and the first bytes, for the heads that start in the tail.
        let mut joined = [0u8; 2 * (MAX_HEAD as usize - 1)];
        let more = bytes.len().min(keep);
        joined[..t].copy_from_slice(&self.tail);
        joined[t..t + more].copy_from_slice(&bytes[..more]);
        let first_new = out.len();
        for g in &self.grids {
            // Heads ending in `bytes`: p + head in (from, stop], p from `start` on.
            let lo = start.max(g.heap).max((from + 1).saturating_sub(g.head));
            let Some(last) = stop.checked_sub(g.head) else {
                continue;
            };
            let mut p = g.heap + (lo - g.heap).div_ceil(g.cluster) * g.cluster;
            while p <= last && p < g.end {
                let h = g.head as usize;
                let w = if p >= from {
                    let i = (p - from) as usize;
                    &bytes[i..i + h]
                } else {
                    let i = (p - start) as usize;
                    &joined[i..i + h]
                };
                if let Some(parent) = starts_dir(g.fs, w) {
                    if out.len() < cap {
                        out.push(OrphanDir {
                            at: p,
                            volume: g.volume,
                            parent,
                        });
                    } else {
                        *capped = true;
                    }
                }
                p += g.cluster;
            }
        }
        out[first_new..].sort_unstable();
        if bytes.len() >= keep {
            self.tail.clear();
            self.tail.extend_from_slice(&bytes[bytes.len() - keep..]);
        } else {
            self.tail.extend_from_slice(bytes);
            let excess = self.tail.len().saturating_sub(keep);
            self.tail.drain(..excess);
        }
        self.tail_end = stop;
    }
}

/// An old directory's file entry that names a finding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OrphanEntry {
    /// The folder ("?" for a directory whose name is unknown, "?/100MSDCF" under it); `None` when dropped by the path
    /// cap or budget (ruling R6).
    pub folder: Option<Arc<str>>,
    pub name: String,
    /// Absolute offset of the entry's first cluster (on its volume's grid today).
    pub start: u64,
    /// As recorded.
    pub size: u64,
    /// From the extension.
    pub kind: FileKind,
    pub created: Option<FsTime>,
    pub modified: Option<FsTime>,
    pub volume: u32,
    /// The candidate (old directory) holding the entry.
    pub dir: u64,
}

/// The kind as the orphan match compares it: MP4 and MOV are one.
pub(crate) fn kind_class(k: FileKind) -> u8 {
    match k {
        FileKind::Mov => FileKind::Mp4 as u8,
        k => k as u8,
    }
}

/// Whether `[start, end)` overlaps one of `ranges` (sorted, disjoint).
fn overlaps(ranges: &[Fragment], start: u64, end: u64) -> bool {
    let i = ranges.partition_point(|r| r.start.saturating_add(r.len) <= start);
    ranges.get(i).is_some_and(|r| r.start < end)
}

/// A cluster after an old directory's first one still reads as directory entries: up to the first free entry (`0x00`,
/// which may not be the first), every FAT entry is a long-name part (cluster word 0) or a short entry with printable
/// name bytes, no reserved attribute bit and no `.` (another directory's start); every exFAT entry has a known type.
fn plausible(fs: FsKind, b: &[u8]) -> bool {
    let entries = b.as_chunks::<32>().0;
    if entries.first().is_none_or(|e| e[0] == 0) {
        return false;
    }
    for e in entries {
        if e[0] == 0 {
            break;
        }
        let ok = match fs {
            FsKind::Exfat => matches!(
                e[0],
                0x85 | 0x05
                    | 0xC0
                    | 0x40
                    | 0xC1
                    | 0x41
                    | 0xA0
                    | 0xA1
                    | 0xA2
                    | 0x20
                    | 0x21
                    | 0x22
                    | 0xE0
                    | 0xE1
                    | 0x60
                    | 0x61
            ),
            _ if e[11] & 0x3F == 0x0F => e[26] == 0 && e[27] == 0,
            _ => {
                e[11] & 0xC0 == 0
                    && (e[0] >= 0x20 || e[0] == 0x05)
                    && e[1..11].iter().all(|&c| c >= 0x20)
                    && e[..11] != *b".          "
            }
        };
        if !ok {
            return false;
        }
    }
    true
}

/// Reads the old directory starting at `at` on `v` and hands each intact entry to `f`, in on-disk order: its first
/// cluster, then the next ones while they lie in the heap, are no candidate's start, are neither walked nor live,
/// can be read and are [`plausible`], at most [`MAX_FAT_DIR_BYTES`] in all. `entries` counts the volume's slots against
/// [`MAX_ENTRIES`]; true when that cap stopped the directory.
#[allow(clippy::too_many_arguments)]
fn read_dir(
    source: &dyn Source,
    v: &Volume,
    at: u64,
    starts: &[u64],
    walked: &[Fragment],
    live: &[Fragment],
    entries: &mut u64,
    f: &mut dyn FnMut(OldEntry),
) -> bool {
    let cs = v.cluster_size;
    let heap_end = v
        .heap_offset
        .saturating_add(u64::from(v.cluster_count).saturating_mul(cs))
        .min(source.len());
    let exfat = v.fs == FsKind::Exfat;
    let mut fat = OldDir::new(v.fs == FsKind::Fat32, entries, MAX_ENTRIES);
    let mut sets = OldSets::default();
    let mut exfat_entries = 0u64;
    let mut left = MAX_FAT_DIR_BYTES;
    let mut off = at;
    let mut capped = false;
    let mut first = true;
    'dir: while left > 0 {
        let n = cs.min(left);
        if off.saturating_add(n) > heap_end {
            break;
        }
        if !first
            && (starts.binary_search(&off).is_ok()
                || overlaps(walked, off, off + n)
                || overlaps(live, off, off + n))
        {
            break;
        }
        let Some(b) = read_exact(source, off, n as usize) else {
            break;
        };
        if !first && !plausible(v.fs, &b) {
            break;
        }
        first = false;
        if exfat {
            for e in b.as_chunks::<32>().0 {
                if exfat_entries >= MAX_ENTRIES {
                    capped = true;
                    break 'dir;
                }
                exfat_entries += 1;
                match sets.push(e) {
                    OldStep::None => {}
                    OldStep::End => break 'dir,
                    OldStep::Entry(x) => f(x),
                }
            }
        } else {
            let go = fat.feed(Some(&b));
            for x in fat.take() {
                f(x);
            }
            if !go {
                capped = fat.capped();
                break;
            }
        }
        left -= n;
        off += n;
    }
    drop(fat);
    if exfat {
        *entries = entries.saturating_add(exfat_entries);
    }
    capped
}

/// Folders of the candidates, built from the names their parents give them and held once each.
struct Folders {
    /// For each candidate: the candidate whose entry names it, and that name.
    names: Vec<Option<(usize, String)>>,
    memo: Vec<Option<Option<Arc<str>>>>,
    visiting: Vec<bool>,
    /// Bytes of folder strings still allowed ([`MAX_PATH_BUDGET`]).
    left: usize,
    truncated: bool,
    unknown: Arc<str>,
}

impl Folders {
    /// The folder of candidate `i` (its own path): "?" when no candidate names it (or a loop, or past [`MAX_DEPTH`]
    /// levels), else its parent's folder and its name; `None` once the path cap or budget dropped it.
    fn get(&mut self, i: usize, depth: u32) -> Option<Arc<str>> {
        if let Some(f) = &self.memo[i] {
            return f.clone();
        }
        if self.visiting[i] || depth >= MAX_DEPTH {
            return Some(self.unknown.clone());
        }
        self.visiting[i] = true;
        let f = match self.names[i].as_ref().map(|(p, _)| *p) {
            None => Some(self.unknown.clone()),
            Some(p) => {
                let parent = self.get(p, depth + 1);
                let name = &self.names[i].as_ref().expect("named").1;
                match parent {
                    None => None,
                    Some(parent) => {
                        let len = parent.len() + 1 + name.len();
                        if len > MAX_PATH_BYTES || len > self.left {
                            self.truncated = true;
                            None
                        } else {
                            self.left -= len;
                            Some(Arc::from(format!("{parent}/{name}")))
                        }
                    }
                }
            }
        };
        self.visiting[i] = false;
        self.memo[i] = Some(f.clone());
        f
    }
}

/// Whether `a` is a later `modified` than `b` (a missing one loses to a present one).
fn later(a: &Option<FsTime>, b: &Option<FsTime>) -> bool {
    a.as_ref().map(|t| t.local.as_str()) > b.as_ref().map(|t| t.local.as_str())
}

/// The file entries of the old directories `dirs` (in any order) that name one of `wanted` (sorted `(start, kind
/// class)` of the findings without origin): at most one per wanted key, the latest `modified` (a missing one loses),
/// then the lowest candidate offset. Directories are read in offset order ([`read_dir`]); a directory entry naming
/// another candidate (FAT: one whose `..` is this directory's cluster; exFAT: any) gives it its name, the first one in
/// that order. Also returns whether a cap (entries per volume, the path cap or budget) dropped or degraded something.
/// Cancel is checked per directory.
pub(crate) fn orphan_entries(
    source: &dyn Source,
    volumes: &[Volume],
    dirs: &[OrphanDir],
    walked: &[Fragment],
    live: &[Fragment],
    wanted: &[(u64, u8)],
    cancel: &CancelToken,
) -> (Vec<OrphanEntry>, bool) {
    let mut dirs: Vec<OrphanDir> = dirs
        .iter()
        .copied()
        .filter(|c| {
            volumes
                .get(c.volume as usize)
                .is_some_and(|v| c.at >= v.heap_offset)
        })
        .collect();
    dirs.sort_unstable();
    dirs.dedup_by_key(|c| (c.at, c.volume));
    let cluster_of = |c: &OrphanDir| {
        let v = &volumes[c.volume as usize];
        ((c.at - v.heap_offset) / v.cluster_size + 2) as u32
    };
    let by_cluster: BTreeMap<(u32, u32), usize> = dirs
        .iter()
        .enumerate()
        .map(|(i, c)| ((c.volume, cluster_of(c)), i))
        .collect();
    let mut starts: Vec<u64> = dirs.iter().map(|c| c.at).collect();
    starts.dedup();
    let mut names: Vec<Option<(usize, String)>> = (0..dirs.len()).map(|_| None).collect();
    let mut best: BTreeMap<(u64, u8), (usize, OldEntry, FileKind)> = BTreeMap::new();
    let mut counts = vec![0u64; volumes.len()];
    let mut truncated = false;
    for (i, c) in dirs.iter().enumerate() {
        if cancel.is_cancelled() {
            break;
        }
        let v = &volumes[c.volume as usize];
        let count = &mut counts[c.volume as usize];
        if *count >= MAX_ENTRIES {
            truncated = true;
            continue;
        }
        let own = cluster_of(c);
        let exfat = v.fs == FsKind::Exfat;
        let mut take = |e: OldEntry| {
            if e.dir {
                if let Some(&x) = by_cluster.get(&(c.volume, e.first))
                    && x != i
                    && names[x].is_none()
                    && (exfat || dirs[x].parent == own)
                {
                    names[x] = Some((i, e.name));
                }
                return;
            }
            let Some(kind) = extension(&e.name).and_then(FileKind::from_extension) else {
                return;
            };
            let Some(start) = v.cluster_offset(e.first) else {
                return;
            };
            let key = (start, kind_class(kind));
            if e.size == 0 || wanted.binary_search(&key).is_err() {
                return;
            }
            if best
                .get(&key)
                .is_none_or(|(_, old, _)| later(&e.modified, &old.modified))
            {
                best.insert(key, (i, e, kind));
            }
        };
        truncated |= read_dir(source, v, c.at, &starts, walked, live, count, &mut take);
    }
    let mut folders = Folders {
        memo: vec![None; dirs.len()],
        visiting: vec![false; dirs.len()],
        names,
        left: MAX_PATH_BUDGET,
        truncated: false,
        unknown: Arc::from("?"),
    };
    let mut out = Vec::with_capacity(best.len());
    for ((start, _), (i, e, kind)) in best {
        let mut folder = folders.get(i, 0);
        if folder
            .as_ref()
            .is_some_and(|f| f.len() + 1 + e.name.len() > MAX_PATH_BYTES)
        {
            folder = None;
            truncated = true;
        }
        out.push(OrphanEntry {
            folder,
            name: e.name,
            start,
            size: e.size,
            kind,
            created: e.created,
            modified: e.modified,
            volume: dirs[i].volume,
            dir: dirs[i].at,
        });
    }
    (out, truncated || folders.truncated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filesystem::Scheme;
    use crate::formats::tests::Mem;
    use revenant_testkit::fs::{FsBuilder, FsType};

    fn volume(fs: FsKind, heap: u64, cluster: u64, count: u32) -> Volume {
        Volume {
            index: 0,
            fs,
            offset: 0,
            len: heap + cluster * count as u64,
            sector_size: 512,
            cluster_size: cluster,
            heap_offset: heap,
            cluster_count: count,
            label: None,
            serial: 0,
            from_backup: false,
            scheme: Scheme::None,
        }
    }

    fn dot_dot(b: &mut [u8], parent: u32) {
        b[..11].copy_from_slice(b".          ");
        b[11] = 0x10;
        b[32..43].copy_from_slice(b"..         ");
        b[43] = 0x10;
        b[52..54].copy_from_slice(&((parent >> 16) as u16).to_le_bytes());
        b[58..60].copy_from_slice(&(parent as u16).to_le_bytes());
    }

    /// Feeds `data` split at `cuts`.
    fn find(
        f: &mut OrphanFinder,
        data: &[u8],
        cuts: &[usize],
        cap: usize,
    ) -> (Vec<OrphanDir>, bool) {
        let (mut out, mut capped) = (Vec::new(), false);
        let mut from = 0;
        for &to in cuts.iter().chain([data.len()].iter()) {
            f.feed(from as u64, &data[from..to], &mut out, cap, &mut capped);
            from = to;
        }
        (out, capped)
    }

    #[test]
    fn fat_heads_are_found_on_the_grid_across_chunk_boundaries() {
        let v = volume(FsKind::Fat32, 1000, 512, 100);
        let mut data = vec![7u8; 1000 + 512 * 100 + 4096];
        // Cluster 2 (at 1000), cluster 12 (at 6120) and the last cluster (101) hold directories.
        for (c, parent) in [(2u32, 0u32), (12, 0x0001_0005), (101, 2)] {
            let at = v.cluster_offset(c).unwrap() as usize;
            dot_dot(&mut data[at..], parent);
        }
        // Off the grid, a `..` without the directory bit, past the heap: not candidates.
        dot_dot(&mut data[2000..], 0);
        let at = v.cluster_offset(20).unwrap() as usize;
        dot_dot(&mut data[at..], 0);
        data[at + 43] = 0x20;
        dot_dot(&mut data[1000 + 512 * 100..], 0);
        let want = vec![
            OrphanDir {
                at: 1000,
                volume: 0,
                parent: 0,
            },
            OrphanDir {
                at: 6120,
                volume: 0,
                parent: 0x0001_0005,
            },
            OrphanDir {
                at: 1000 + 512 * 99,
                volume: 0,
                parent: 2,
            },
        ];
        let fresh = || OrphanFinder::new(std::slice::from_ref(&v));
        assert_eq!(find(&mut fresh(), &data, &[], 10), (want.clone(), false));
        for d in 1..64 {
            assert_eq!(find(&mut fresh(), &data, &[6120 + d], 10).0, want, "{d}");
        }
        let tiny: Vec<usize> = (1..data.len()).step_by(7).collect();
        assert_eq!(find(&mut fresh(), &data, &tiny, 10).0, want);
        // A gap breaks the stream; the cap.
        let mut f = fresh();
        let (mut out, mut capped) = (Vec::new(), false);
        f.feed(0, &data[..6150], &mut out, 10, &mut capped);
        f.feed(9000, &data[6150..6200], &mut out, 10, &mut capped);
        assert_eq!(out, want[..1].to_vec());
        assert_eq!(
            find(&mut fresh(), &data, &[], 2),
            (want[..2].to_vec(), true)
        );
        // FAT16: the `..` cluster's high word is not read.
        let v16 = volume(FsKind::Fat16, 1000, 512, 100);
        let got = find(&mut OrphanFinder::new(&[v16]), &data, &[], 10).0;
        assert_eq!(got[1].parent, 5);
        // No volume: nothing.
        assert_eq!(find(&mut OrphanFinder::new(&[]), &data, &[], 10).0, vec![]);
    }

    /// The old directories of a whole image, fed in one go.
    fn all_dirs(img: &Mem, v: &[Volume]) -> Vec<OrphanDir> {
        let (mut out, mut capped) = (Vec::new(), false);
        OrphanFinder::new(v).feed(0, &img.0, &mut out, usize::MAX, &mut capped);
        out
    }

    /// A FAT short entry: `attr`, first cluster, size, modification date word (time: the builder's 07:12:04).
    fn raw(name: &[u8; 11], attr: u8, first: u32, size: u32, date: u16) -> [u8; 32] {
        let mut e = [0u8; 32];
        e[22..24].copy_from_slice(&(7u16 << 11 | 12 << 5 | 2).to_le_bytes());
        e[..11].copy_from_slice(name);
        e[11] = attr;
        e[20..22].copy_from_slice(&((first >> 16) as u16).to_le_bytes());
        e[24..26].copy_from_slice(&date.to_le_bytes());
        e[26..28].copy_from_slice(&(first as u16).to_le_bytes());
        e[28..32].copy_from_slice(&size.to_le_bytes());
        e
    }

    fn entries_of(b: &FsBuilder, files: &[&str]) -> (Vec<OrphanEntry>, bool) {
        let img = Mem(b.build_quick_formatted());
        let v = crate::filesystem::volumes(&img);
        let dirs = all_dirs(&img, &v);
        let mut wanted: Vec<(u64, u8)> = files
            .iter()
            .map(|f| {
                (
                    b.cluster_offset(b.clusters(f)[0]),
                    kind_class(FileKind::Jpeg),
                )
            })
            .collect();
        wanted.sort_unstable();
        orphan_entries(&img, &v, &dirs, &[], &[], &wanted, &CancelToken::default())
    }

    #[test]
    fn old_directories_name_files_and_their_folders() {
        let mut b = FsBuilder::new(FsType::Fat32, 48 << 20, 512);
        let x = b.add("DCIM/100MSDCF/X.JPG", &[1; 1000]);
        let y = b.add("DCIM/100MSDCF/Y.JPG", &[2; 1000]);
        b.mkdir("TOP");
        let top = b.clusters("TOP")[0];
        // 2025-12-14 07:12:04 is the builder's time; X2 is a day later, Y2 the same time.
        let (day, next) = (45 << 9 | 12 << 5 | 14, 45 << 9 | 12 << 5 | 15);
        b.raw_entry("TOP", raw(b"X2      JPG", 0x20, x[0], 1000, next));
        b.raw_entry("TOP", raw(b"Y2      JPG", 0x20, y[0], 1000, day));
        // 100MSDCF claims TOP as its subfolder, but TOP's `..` is the root: no name from it.
        b.raw_entry("DCIM/100MSDCF", raw(b"FAKE       ", 0x10, top, 0, day));
        let (e, truncated) = entries_of(&b, &["DCIM/100MSDCF/X.JPG", "DCIM/100MSDCF/Y.JPG"]);
        assert!(!truncated);
        let got: Vec<(String, Option<&str>, u64)> = e
            .iter()
            .map(|e| (e.name.clone(), e.folder.as_deref(), e.size))
            .collect();
        assert_eq!(
            got,
            vec![
                // The later `modified` wins; at the same time, the lower directory offset.
                ("X2.JPG".to_string(), Some("?"), 1000),
                ("Y.JPG".to_string(), Some("?/100MSDCF"), 1000),
            ]
        );
        assert_eq!(e[1].dir, b.cluster_offset(b.clusters("DCIM/100MSDCF")[0]));
        assert_eq!(e[0].start, b.cluster_offset(x[0]));
        assert_eq!(e[0].modified.as_ref().unwrap().local, "2025-12-15T07:12:04");
    }

    #[test]
    fn long_paths_drop_the_folder_and_deep_trees_start_unknown() {
        let mut b = FsBuilder::new(FsType::Fat32, 48 << 20, 512);
        // 18 levels of 250-character names (the top one unknown): past 4096 bytes.
        let mut path = String::new();
        for i in 0..18 {
            path.push_str(&format!("{}{i:02}/", "a".repeat(248)));
        }
        b.add(&format!("{path}LONG.JPG"), &[1; 1000]);
        // 34 levels of short names: the top two are past the 32 levels followed.
        let deep: String = (0..34).map(|i| format!("D{i}/")).collect();
        b.add(&format!("{deep}DEEP.JPG"), &[2; 1000]);
        let (e, truncated) = entries_of(
            &b,
            &[&format!("{path}LONG.JPG"), &format!("{deep}DEEP.JPG")],
        );
        assert!(truncated);
        assert_eq!(
            (e[0].name.as_str(), e[0].folder.as_deref()),
            ("LONG.JPG", None)
        );
        let want: String = std::iter::once("?".to_string())
            .chain((2..34).map(|i| format!("D{i}")))
            .collect::<Vec<_>>()
            .join("/");
        assert_eq!(
            (e[1].name.as_str(), e[1].folder.as_deref()),
            ("DEEP.JPG", Some(want.as_str()))
        );
    }

    #[test]
    fn later_modified_wins_and_absent_loses() {
        let t = |s: &str| {
            Some(FsTime {
                local: s.into(),
                utc_offset_minutes: None,
            })
        };
        assert!(later(&t("2025-01-02T00:00:00"), &t("2025-01-01T00:00:00")));
        assert!(!later(&t("2025-01-01T00:00:00"), &t("2025-01-01T00:00:00")));
        assert!(later(&t("2000-01-01T00:00:00"), &None));
        assert!(!later(&None, &t("2000-01-01T00:00:00")));
    }

    #[test]
    fn plausible_entries() {
        let mut b = vec![0u8; 512];
        assert!(
            !plausible(FsKind::Fat32, &b),
            "a free first entry ends the directory"
        );
        b[..11].copy_from_slice(b"DSC01234JPG");
        b[11] = 0x20;
        assert!(plausible(FsKind::Fat32, &b));
        b[32..43].copy_from_slice(b"DSC\x0101235JP");
        assert!(!plausible(FsKind::Fat32, &b));
        b[32..43].copy_from_slice(b".          ");
        assert!(!plausible(FsKind::Fat32, &b));
        b[32..64].fill(0);
        b[11] = 0x40;
        assert!(!plausible(FsKind::Fat32, &b));
        let mut e = vec![0u8; 512];
        e[0] = 0x85;
        e[32] = 0xC0;
        e[64] = 0xC1;
        assert!(plausible(FsKind::Exfat, &e));
        e[96] = 0x13;
        assert!(!plausible(FsKind::Exfat, &e));
    }
}
