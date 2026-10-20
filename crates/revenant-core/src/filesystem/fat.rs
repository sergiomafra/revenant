//! The directory tree of a FAT12/16/32 volume: live files and folders, and deleted ones.
//!
//! From Microsoft's "FAT32 File System Specification" 1.03 (fatgen103): FAT entry packing and end-of-chain values (§4),
//! the directory entry (§6) and long names (§7). Deleted entries keep their names (but the first letter, see
//! [`super::fat_deleted`]), sizes and first clusters; where the rest of their clusters were is guessed (ruling D4).

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use super::alloc::{Budget as Steps, Claims, ClusterMap, Cursor, Entry, follow, guess};
use super::boot::{self, Layout, Root};
use super::fat_deleted::{FirstBytes, Gone, Probes, high_word, key, pack, set_first};
use super::listing::{
    Budget, Collector, FsOrigin, FsState, FsTime, Listing, Placement, component, extension,
};
use super::{FsKind, Limits, Volume, read_exact, u16_at, u32_at};
use crate::formats::FileKind;
use crate::scan::CancelToken;
use crate::source::Source;

/// FAT bytes read at once (and cached).
const FAT_BLOCK: u64 = 32 * 1024;
/// FAT blocks kept (8 MiB); the cache is emptied when it fills.
const FAT_CACHE_BLOCKS: usize = 256;
/// A sector of free FAT entries (sectors are at most 4096 bytes).
static ZEROS: [u8; 4096] = [0; 4096];
/// Most parts of one long name (255 characters).
const MAX_LFN_PARTS: usize = 20;
/// Where the 13 UTF-16 units of a long-name entry are.
pub(super) const LFN_UNITS: [usize; 13] = [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30];

/// Lists the files and directories of FAT volume `v` into `out`, live and deleted.
///
/// First the live tree: the root (FAT12/16: the fixed region; FAT32: the `RootClus` chain) is walked depth first,
/// entries in on-disk order. Every live directory and file claims its chain; a directory whose first cluster is already
/// claimed is not entered (a loop or a second link). Deleted photos/videos met on the way are placed by [`guess`];
/// deleted directories wait. Then the first letters still unknown are settled from the whole volume (ruling D9), and
/// the waiting deleted directories are walked, in the order they were met, each depth first. Deleted entries never
/// claim. Bounds: [`Limits`] (directories, entries, depth, bytes per directory, paths, guess steps) and the listing's
/// budget and probe cap; a cap that drops or degrades something marks the listing truncated, and once the entry cap is
/// reached no further directory is read.
pub(super) fn walk(
    source: &dyn Source,
    v: &Volume,
    cancel: &CancelToken,
    limits: &Limits,
    budget: &mut Budget,
    probes: &mut Probes<'_>,
    out: &mut Listing,
) {
    let Some(layout) = boot::layout(source, v) else {
        return;
    };
    let mut w = Walk {
        source,
        v,
        cancel,
        limits,
        budget,
        probes,
        map: FatMap::new(source, v, &layout),
        claims: Claims::new(v.cluster_count, limits.contested),
        col: Collector::new(v, source.len()),
        steps: Steps::new(limits.guess_steps),
        dir_clusters: limits.fat_dir_bytes.div_ceil(v.cluster_size).max(1),
        fat32: v.fs == FsKind::Fat32,
        last: v.cluster_count.saturating_add(1),
        dirs: 1,
        entries: 0,
        known: Vec::new(),
        names: FirstBytes::default(),
        unsettled: Vec::new(),
        waiting: Vec::new(),
        seen: BTreeSet::new(),
    };
    let root = match layout.root {
        Root::Fixed { sector, entries } => Region::Fixed {
            offset: sector,
            len: entries * 32,
            sector_size: layout.sector_size,
        },
        Root::Cluster(c) => w.dir_chain(c),
    };
    let whole = w.live(root);
    w.settle();
    if whole {
        w.deleted();
    }
    let Walk { col, claims, .. } = w;
    col.finish(&claims, out);
}

/// One volume's walk.
struct Walk<'a, 'b, 'p> {
    source: &'a dyn Source,
    v: &'a Volume,
    cancel: &'a CancelToken,
    limits: &'a Limits,
    budget: &'b mut Budget,
    probes: &'b mut Probes<'p>,
    map: FatMap<'a>,
    claims: Claims,
    col: Collector<'a>,
    /// Cluster examinations left for deleted entries (ruling C7).
    steps: Steps,
    dir_clusters: u64,
    fat32: bool,
    /// The last cluster of the heap.
    last: u32,
    /// Directories walked or on a stack, live and deleted (not those in `waiting`).
    dirs: usize,
    entries: u64,
    /// The live tree's known short names ([`pack`]ed): what the volume-wide D9 rule counts once the live walk is done.
    known: Vec<u32>,
    /// The same, counted, from then on.
    names: FirstBytes,
    /// Deleted entries of live directories whose first letter waits for the whole volume, in walk order.
    unsettled: Vec<Unsettled>,
    /// Deleted directories met in live directories, in walk order.
    waiting: Vec<Dir>,
    /// First clusters of the deleted directories walked (a deleted tree has no claims to stop a loop).
    seen: BTreeSet<u32>,
}

/// A first letter to settle (ruling D9) once the live walk is done.
struct Unsettled {
    key: u32,
    lower: bool,
    what: Waiting,
}

enum Waiting {
    /// Pending file `i` of the collector.
    File(usize),
    /// Waiting directory `i`, whose name starts at byte `at` of its folder.
    Dir(usize, usize),
}

impl Walk<'_, '_, '_> {
    /// The live tree; false when the walk of the volume must stop there (cancelled, or the entry cap reached).
    fn live(&mut self, root: Region) -> bool {
        let mut stack = vec![Dir {
            folder: Some(Arc::from("")),
            depth: 0,
            region: root,
        }];
        while let Some(d) = stack.pop() {
            if self.cancel.is_cancelled() {
                return false;
            }
            let read = self.read_dir(&d.region, false);
            let mut subdirs = Vec::new();
            let children = self.restore(read.children, read.known, true);
            for ch in children {
                if ch.deleted() {
                    self.deleted_child(ch, &d, None);
                    continue;
                }
                if ch.dir {
                    if !self.claims.in_heap(ch.first) || self.claims.is_claimed(ch.first) {
                        continue;
                    }
                    if d.depth >= self.limits.depth || self.dirs >= self.limits.dirs {
                        self.budget.truncated = true;
                        continue;
                    }
                    self.dirs += 1;
                    let region = self.dir_chain(ch.first);
                    subdirs.push(Dir {
                        folder: self.budget.subfolder(d.folder.as_ref(), &ch.name),
                        depth: d.depth + 1,
                        region,
                    });
                } else if ch.size > 0 {
                    let need = (ch.size as u64).div_ceil(self.v.cluster_size);
                    let chain = follow(&mut self.map, &mut self.claims, ch.first, need);
                    let origin = self.origin(&d, ch, FsState::Live, Placement::Chain);
                    self.col.file(self.budget, origin, chain);
                }
            }
            if read.capped {
                // The volume's entry budget is spent: no further directory is read.
                self.budget.truncated = true;
                return false;
            }
            stack.extend(subdirs.into_iter().rev());
        }
        true
    }

    /// Settles, in walk order, the first letters the live directories left unknown, from every known short name of
    /// the volume (ruling D9; those already settled count for the next ones); `_` when none matches.
    fn settle(&mut self) {
        self.names = FirstBytes::new(std::mem::take(&mut self.known));
        for u in std::mem::take(&mut self.unsettled) {
            let Some(first) = self.names.best(u.key) else {
                continue;
            };
            self.names.add(u.key, first);
            match u.what {
                Waiting::File(i) => {
                    if let Some(name) = self.col.name_mut(i) {
                        set_first(name, first, u.lower);
                    }
                }
                Waiting::Dir(i, at) => {
                    let d = &mut self.waiting[i];
                    if let Some(f) = &d.folder {
                        let mut path = f.to_string();
                        let mut name = path.split_off(at);
                        set_first(&mut name, first, u.lower);
                        path.push_str(&name);
                        d.folder = Some(Arc::from(path));
                    }
                }
            }
        }
    }

    /// The deleted directories met in the live tree, each walked depth first.
    fn deleted(&mut self) {
        for top in std::mem::take(&mut self.waiting) {
            if self.dirs >= self.limits.dirs {
                self.budget.truncated = true;
                return;
            }
            self.dirs += 1;
            let mut stack = vec![top];
            while let Some(d) = stack.pop() {
                if self.cancel.is_cancelled() {
                    return;
                }
                if self.steps.exhausted() {
                    self.budget.truncated = true;
                    return;
                }
                let Region::Guessed { first } = d.region else {
                    continue;
                };
                if !self.seen.insert(first) {
                    continue;
                }
                let read = self.read_dir(&d.region, true);
                if !read.is_dir {
                    continue;
                }
                let children = self.restore(read.children, read.known, false);
                let mut subdirs = Vec::new();
                for ch in children {
                    self.deleted_child(ch, &d, Some(&mut subdirs));
                }
                if read.capped {
                    self.budget.truncated = true;
                    return;
                }
                stack.extend(subdirs.into_iter().rev());
            }
        }
    }

    /// Ruling D9 inside one directory: each deleted short name whose long name did not settle its first letter takes
    /// the most common first byte among the directory's known short names with the same bytes 1–3 (those settled
    /// before it included). In the live tree the directory's known names join the volume's, and what is still unknown
    /// waits for [`Walk::settle`]; in a deleted tree the volume's names are asked next, then `_` stays.
    fn restore(&mut self, mut children: Vec<Child>, known: Vec<u32>, live: bool) -> Vec<Child> {
        if live {
            self.known.extend_from_slice(&known);
        } else {
            for &p in &known {
                self.names.add(p >> 8, p as u8);
            }
        }
        if !children.iter().any(Child::pending) {
            return children;
        }
        let mut here = FirstBytes::new(known);
        for ch in children.iter_mut().filter(|c| c.pending()) {
            let k = key(&ch.key_bytes());
            let found = here
                .best(k)
                .or_else(|| (!live).then(|| self.names.best(k)).flatten());
            if let Some(first) = found {
                let lower = ch.lower();
                set_first(&mut ch.name, first, lower);
                here.add(k, first);
                if live {
                    self.known.push(k << 8 | first as u32);
                } else {
                    self.names.add(k, first);
                }
                ch.flags &= !PENDING;
            } else if !live {
                ch.flags &= !PENDING;
            }
        }
        children
    }

    /// A deleted entry of directory `d`. A directory waits (`subdirs: None`, the live tree) or is walked next
    /// (`subdirs`); a photo/video is placed and listed; anything else is dropped.
    fn deleted_child(&mut self, ch: Child, d: &Dir, subdirs: Option<&mut Vec<Dir>>) {
        if ch.dir {
            if !self.claims.in_heap(ch.first) {
                return;
            }
            // A waiting directory counts against the directory cap once it is walked, so the live tree never loses
            // room to deleted ones; the waiting list itself holds at most as many.
            let full = match subdirs {
                Some(_) => self.dirs >= self.limits.dirs,
                None => self.waiting.len() >= self.limits.dirs,
            };
            if d.depth >= self.limits.depth || full {
                self.budget.truncated = true;
                return;
            }
            let waiting = self.waiting.len();
            let folder = self.budget.subfolder(d.folder.as_ref(), &ch.name);
            let at = folder.as_ref().map(|f| f.len() - ch.name.len());
            let dir = Dir {
                folder,
                depth: d.depth + 1,
                region: Region::Guessed { first: ch.first },
            };
            match subdirs {
                Some(s) => {
                    self.dirs += 1;
                    s.push(dir);
                }
                None => {
                    if let (true, Some(at)) = (ch.pending(), at) {
                        self.unsettled.push(Unsettled {
                            key: key(&ch.key_bytes()),
                            lower: ch.lower(),
                            what: Waiting::Dir(waiting, at),
                        });
                    }
                    self.waiting.push(dir);
                }
            }
            return;
        }
        let Some(kind) = extension(&ch.name).and_then(FileKind::from_extension) else {
            return;
        };
        if ch.size == 0 {
            return;
        }
        if self.steps.exhausted() {
            self.budget.truncated = true;
            return;
        }
        let Some(first) = self.first_cluster(&ch, kind) else {
            return;
        };
        let need = (ch.size as u64).div_ceil(self.v.cluster_size);
        let Some(g) = guess(&mut self.map, &mut self.steps, first, need, self.last) else {
            if self.steps.exhausted() {
                self.budget.truncated = true;
            }
            return;
        };
        let (pending, k, lower) = (ch.pending(), key(&ch.key_bytes()), ch.lower());
        let origin = self.origin(d, ch, FsState::Deleted, Placement::Guessed);
        if let Some(i) = self.col.deleted(self.budget, origin, kind, g)
            && pending
        {
            self.unsettled.push(Unsettled {
                key: k,
                lower,
                what: Waiting::File(i),
            });
        }
    }

    /// The first cluster of a deleted file: as recorded, or found by the FAT32 high-word check (ruling D10) when the
    /// recorded high word is 0 on a volume with clusters past 0xFFFF.
    fn first_cluster(&mut self, ch: &Child, kind: FileKind) -> Option<u32> {
        if !(self.fat32 && ch.first >> 16 == 0 && self.last > 0xFFFF) {
            return Some(ch.first);
        }
        let v = self.v;
        high_word(
            &mut self.map,
            self.probes,
            |c| v.cluster_offset(c),
            ch.first,
            self.last,
            kind,
        )
    }

    fn origin(&self, d: &Dir, ch: Child, state: FsState, placement: Placement) -> FsOrigin {
        FsOrigin {
            volume: self.v.index,
            folder: d.folder.clone(),
            name: ch.name,
            state,
            placement,
            size: ch.size as u64,
            created: fat_time(ch.created.0, ch.created.1, Some(ch.created.2)),
            modified: fat_time(ch.modified.0, ch.modified.1, None),
        }
    }

    /// Follows a live directory's chain for at most `dir_clusters` clusters and hands it to the collector; a chain that
    /// goes on past that cap to a cluster no chain holds yet marks the listing truncated (one that goes on to a claimed
    /// cluster, a cycle or a cross-link, would have stopped there anyway).
    fn dir_chain(&mut self, first: u32) -> Region {
        let chain = follow(&mut self.map, &mut self.claims, first, self.dir_clusters);
        let taken: u64 = chain.runs.iter().map(|&(_, n)| n as u64).sum();
        if !chain.ended
            && taken == self.dir_clusters
            && let Some(&(start, n)) = chain.runs.last()
            && let Entry::Next(next) = self.map.entry(start + n - 1)
            && !self.claims.is_claimed(next)
        {
            self.budget.truncated = true;
        }
        self.col.dir(self.budget, &chain);
        Region::Chain {
            first,
            clusters: taken,
        }
    }

    /// The entries of a directory, up to the end-of-directory entry or the volume's entry budget. An unreadable sector
    /// or cluster is skipped (its entries are lost, and a long name across it is not used). A deleted directory
    /// (`deleted`: every entry is deleted, whatever its byte 0 says) is read along the clusters [`guess`] would give it,
    /// for at most `dir_clusters`, and only when its first cluster starts with its own `.` entry.
    fn read_dir(&mut self, region: &Region, deleted: bool) -> DirRead {
        let mut r = DirReader {
            fat32: self.fat32,
            deleted,
            entries: &mut self.entries,
            cap: self.limits.entries,
            capped: false,
            long: LongName::default(),
            gone: Gone::default(),
            known: Vec::new(),
            out: Vec::new(),
        };
        let mut is_dir = true;
        let v = self.v;
        let source = self.source;
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
            Region::Chain { first, clusters } => {
                let mut c = first;
                for k in 0..clusters {
                    let Some(at) = v.cluster_offset(c) else {
                        break;
                    };
                    let cluster = read_exact(source, at, v.cluster_size as usize);
                    if !r.feed(cluster.as_deref()) || k + 1 == clusters {
                        break;
                    }
                    match self.map.entry(c) {
                        Entry::Next(n) => c = n,
                        _ => break,
                    }
                }
            }
            Region::Guessed { first } => {
                is_dir = false;
                if let Some(mut cursor) =
                    Cursor::new(&mut self.map, first, self.dir_clusters, self.last)
                {
                    for k in 0..self.dir_clusters {
                        let Ok(Some((c, _))) = cursor.run(&mut self.map, &mut self.steps, 1) else {
                            break;
                        };
                        let Some(at) = v.cluster_offset(c) else {
                            break;
                        };
                        let cluster = read_exact(source, at, v.cluster_size as usize);
                        if k == 0 {
                            is_dir = cluster
                                .as_deref()
                                .is_some_and(|b| starts_with_dot(b, first, self.fat32));
                            if !is_dir {
                                break;
                            }
                        }
                        if !r.feed(cluster.as_deref()) {
                            break;
                        }
                    }
                }
                if self.steps.exhausted() {
                    self.budget.truncated = true;
                }
            }
        }
        DirRead {
            capped: r.capped,
            children: r.out,
            known: r.known,
            is_dir,
        }
    }
}

/// Whether a cluster starts with the `.` entry of a directory whose first cluster is `first`.
fn starts_with_dot(cluster: &[u8], first: u32, fat32: bool) -> bool {
    let Some(e) = cluster.first_chunk::<32>() else {
        return false;
    };
    let lo = u16_at(e, 26) as u32;
    let at = if fat32 {
        (u16_at(e, 20) as u32) << 16 | lo
    } else {
        lo
    };
    e[..11] == *b".          " && e[11] & 0x10 != 0 && at == first
}

/// A directory waiting to be read.
struct Dir {
    /// Its own path (the folder of its files), shared with them; `None` once dropped by the path cap or budget.
    folder: Option<Arc<str>>,
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
    /// A chain already followed (and claimed): `clusters` clusters from `first`, followed again through the FAT when
    /// the directory is read, so a directory waiting on the walk's stack holds no run list.
    Chain { first: u32, clusters: u64 },
    /// A deleted directory: its clusters are guessed from `first` when it is read.
    Guessed { first: u32 },
}

/// What reading a directory gave.
struct DirRead {
    children: Vec<Child>,
    /// The [`pack`]ed short names whose first byte is known: intact entries, and deleted ones their long name restored.
    known: Vec<u32>,
    /// The volume's entry budget ran out before the directory ended.
    capped: bool,
    /// A deleted directory's first cluster starts with its own `.` entry (always true for a live one).
    is_dir: bool,
}

/// The entry is deleted.
const DELETED: u8 = 1;
/// A deleted short name whose first letter is still unknown (the name holds `_` there).
const PENDING: u8 = 2;
/// The NT bit for a lower-case base name (byte 12, 0x08).
const LOWER: u8 = 4;

/// An entry of a directory (`.`, `..`, the volume label and long-name parts excluded).
#[derive(Debug)]
struct Child {
    /// One path component (sanitized).
    name: String,
    dir: bool,
    first: u32,
    size: u32,
    /// Creation (date, time, 10 ms count) and modification (date, time) words, formatted only for a file.
    created: (u16, u16, u8),
    modified: (u16, u16),
    /// Bytes 1–3 of the short name (ruling D9).
    key: [u8; 3],
    flags: u8,
}

impl Child {
    fn deleted(&self) -> bool {
        self.flags & DELETED != 0
    }

    fn pending(&self) -> bool {
        self.flags & PENDING != 0
    }

    fn lower(&self) -> bool {
        self.flags & LOWER != 0
    }

    /// A short name with these bytes 1–3 ([`key`] reads them).
    fn key_bytes(&self) -> [u8; 4] {
        [0, self.key[0], self.key[1], self.key[2]]
    }
}

struct DirReader<'a> {
    fat32: bool,
    /// Every entry is deleted (a deleted directory).
    deleted: bool,
    entries: &'a mut u64,
    cap: u64,
    capped: bool,
    long: LongName,
    gone: Gone,
    known: Vec<u32>,
    out: Vec<Child>,
}

impl DirReader<'_> {
    /// Parses one chunk of entries (`None`: unreadable); false once the directory has ended.
    fn feed(&mut self, chunk: Option<&[u8]>) -> bool {
        let Some(chunk) = chunk else {
            self.long = LongName::default();
            self.gone.clear();
            return true;
        };
        for e in chunk.as_chunks::<32>().0 {
            if *self.entries >= self.cap {
                self.capped = true;
                return false;
            }
            *self.entries += 1;
            if e[0] == 0x00 {
                return false;
            }
            let attr = e[11];
            let lfn = attr & 0x3F == 0x0F;
            let erased = e[0] == 0xE5;
            if erased {
                self.long = LongName::default();
                if lfn {
                    self.gone.push(e);
                    continue;
                }
            } else if lfn {
                self.gone.clear();
                self.long.part(e);
                continue;
            }
            let long = std::mem::take(&mut self.long).name(&e[..11]);
            let restored = if erased {
                self.gone.restore(&e[..11])
            } else {
                None
            };
            self.gone.clear();
            if attr & 0x08 != 0 || is_dot(e) {
                continue;
            }
            let lo = u16_at(e, 26) as u32;
            let first = if self.fat32 {
                (u16_at(e, 20) as u32) << 16 | lo
            } else {
                lo
            };
            let lower = if e[12] & 0x08 != 0 { LOWER } else { 0 };
            let mut flags = if erased || self.deleted { DELETED } else { 0 };
            let name = match (erased, restored) {
                (true, Some((byte, text))) => {
                    self.known.push(pack(e, byte));
                    text
                }
                (true, None) => {
                    flags |= PENDING | lower;
                    let mut raw = *e;
                    raw[0] = b'_';
                    short_name(&raw)
                }
                (false, _) => {
                    self.known
                        .push(pack(e, if e[0] == 0x05 { 0xE5 } else { e[0] }));
                    long.unwrap_or_else(|| short_name(e))
                }
            };
            self.out.push(Child {
                name: component(&name),
                dir: attr & 0x10 != 0,
                first,
                size: u32_at(e, 28),
                created: (u16_at(e, 16), u16_at(e, 14), e[13]),
                modified: (u16_at(e, 24), u16_at(e, 22)),
                key: [e[1], e[2], e[3]],
                flags,
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
        let sum = checksum(sfn);
        if !self.active || self.next != 0 || sum != self.sum {
            return None;
        }
        let units = &self.units[..self.parts * 13];
        let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
        (end > 0).then(|| String::from_utf16_lossy(&units[..end]))
    }
}

/// The long-name checksum of an 11-byte short name (fatgen103 §7.2).
pub(super) fn checksum(sfn: &[u8]) -> u8 {
    sfn[..11]
        .iter()
        .fold(0u8, |s, &c| s.rotate_right(1).wrapping_add(c))
}

/// A short-name byte as a character: printable ASCII as is (lower-cased when `lower`), anything else `_`.
pub(super) fn sfn_char(c: u8, lower: bool) -> char {
    match c {
        0x20..=0x7E if lower => (c as char).to_ascii_lowercase(),
        0x20..=0x7E => c as char,
        _ => '_',
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
        part[..end].iter().map(|&c| sfn_char(c, lower)).collect()
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
    let mut local = format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}");
    local.shrink_to_fit();
    Some(FsTime {
        local,
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
        self.ensure(i);
        let block = &self.cache[&i];
        let bps = self.layout.sector_size as usize;
        if block.bad[r / bps] || block.bad[(r + N - 1) / bps] {
            return None;
        }
        block.data[r..r + N].try_into().ok()
    }

    /// FAT block `i` in the cache.
    fn ensure(&mut self, i: u64) {
        if !self.cache.contains_key(&i) {
            if self.cache.len() >= FAT_CACHE_BLOCKS {
                self.cache.clear();
            }
            let block = self.load(i);
            self.cache.insert(i, block);
        }
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

    /// FAT16/32: whole FAT blocks at a time (the deleted-file guesses examine up to 2^26 clusters per volume).
    fn count(&mut self, from: u32, to: u32, free: bool) -> u64 {
        let width: u64 = match self.fs {
            FsKind::Fat16 => 2,
            FsKind::Fat32 | FsKind::Exfat => 4,
            FsKind::Fat12 => 0,
        };
        if width == 0 || !self.in_heap(from) {
            // FAT12 volumes are small (fewer than 4085 clusters): one entry at a time.
            let mut n = 0;
            let mut c = from as u64;
            while c <= to as u64 && (self.allocated(c as u32) == Some(false)) == free {
                n += 1;
                c += 1;
            }
            return n;
        }
        let to = to.min(self.count.saturating_add(1)) as u64;
        let mut c = from as u64;
        let mut n = 0;
        while c <= to {
            let off = c * width;
            if off + width > self.layout.fat_bytes {
                return if free { n } else { n + (to - c + 1) };
            }
            let (i, r) = (off / FAT_BLOCK, (off % FAT_BLOCK) as usize);
            self.ensure(i);
            let block = &self.cache[&i];
            let bps = self.layout.sector_size as usize;
            let in_block = ((block.data.len() - r) as u64 / width).min(to - c + 1);
            let w = width as usize;
            let mut at = r;
            let stop = r + in_block as usize * w;
            while at < stop {
                // The rest of this sector (entries never straddle one: the sector size is a multiple of 4).
                let end = ((at / bps + 1) * bps).min(stop);
                let entries = ((end - at) / w) as u64;
                if block.bad[at / bps] {
                    if free {
                        return n;
                    }
                    n += entries;
                } else if free && block.data[at..end] == ZEROS[..end - at] {
                    n += entries;
                } else {
                    for e in block.data[at..end].chunks_exact(w) {
                        let is_free = match w {
                            2 => e == [0, 0],
                            _ => {
                                u32::from_le_bytes(e.try_into().expect("4 bytes")) & 0x0FFF_FFFF
                                    == 0
                            }
                        };
                        if is_free != free {
                            return n;
                        }
                        n += 1;
                    }
                }
                at = end;
            }
            c += in_block;
        }
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filesystem::MAX_WALKED_CLUSTERS;
    use crate::filesystem::listing::read_limited;
    use crate::filesystem::{FsState, SourceProbe};
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
        read_limited(
            &src,
            &SourceProbe::new(&src),
            &CancelToken::default(),
            limits,
        )
    }

    fn paths(l: &Listing) -> Vec<String> {
        let mut p: Vec<String> = l.files.iter().map(|f| f.origin.path()).collect();
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
        let trusted: Vec<String> = l
            .files
            .iter()
            .filter(|f| f.trusted)
            .map(|f| f.origin.path())
            .collect();
        assert_eq!(trusted, ["DCIM/OK.JPG"]);
        assert!(in_live(&l, &b, dcim));
        assert!(!l.truncated);
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
        assert!(l.truncated);
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
        assert!(l.truncated);
        assert!(!list(&b, &Limits::DEFAULT).truncated);
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
        assert!(l.truncated);
        let l = list(
            &b,
            &Limits {
                files: 3,
                ..Limits::DEFAULT
            },
        );
        assert_eq!(l.files.len(), 3);
        assert!(!l.truncated);
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
        let l = list(&b, &Limits::DEFAULT);
        assert_eq!(paths(&l), all);
        assert!(!l.truncated);
        // Depth 2: A/B is walked, A/B/C is not.
        let l = list(
            &b,
            &Limits {
                depth: 2,
                ..Limits::DEFAULT
            },
        );
        assert_eq!(paths(&l), ["A/A1.JPG", "A/B/B1.JPG", "TOP.JPG", "Z/Z1.JPG"]);
        assert!(l.truncated);
        // Three directories: the root, A and Z (found while reading the root), not A/B.
        let l = list(
            &b,
            &Limits {
                dirs: 3,
                ..Limits::DEFAULT
            },
        );
        assert_eq!(paths(&l), ["A/A1.JPG", "TOP.JPG", "Z/Z1.JPG"]);
        assert!(l.truncated);
        // Entries: the root's label, TOP, A, Z and its end marker, then A's `.`, `..` and A1.
        let l = list(
            &b,
            &Limits {
                entries: 8,
                ..Limits::DEFAULT
            },
        );
        assert_eq!(paths(&l), ["A/A1.JPG", "TOP.JPG"]);
        assert!(l.truncated);
        let l = list(
            &b,
            &Limits {
                entries: 7,
                ..Limits::DEFAULT
            },
        );
        assert_eq!(paths(&l), ["TOP.JPG"]);
        assert!(l.truncated);
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
        assert!(l.truncated);
        // Exactly two clusters that end there: within the cap, nothing truncated.
        let mut b = card();
        for i in 0..30 {
            b.add(&format!("D/F{i:02}.JPG"), &[1; 512]);
        }
        assert_eq!(b.clusters("D").len(), 2);
        let l = list(
            &b,
            &Limits {
                fat_dir_bytes: 1024,
                ..Limits::DEFAULT
            },
        );
        assert_eq!(l.files.len(), 30);
        assert!(!l.truncated);
        // Past the cap the chain goes on to a claimed cluster (the root's, then its own first): it would have stopped
        // there anyway, so nothing is truncated.
        for to_claimed in [b.clusters("")[0], d[0]] {
            let mut b = card();
            for i in 0..40 {
                b.add(&format!("D/F{i:02}.JPG"), &[1; 512]);
            }
            let d = b.clusters("D");
            b.set_fat(d[1], to_claimed);
            let l = list(
                &b,
                &Limits {
                    fat_dir_bytes: 1024,
                    ..Limits::DEFAULT
                },
            );
            assert_eq!(l.files.len(), 30);
            assert!(!l.truncated, "{to_claimed}");
        }
        // Three clusters, the chain broken (free) at the second: it stops at the cap but nothing lies past it.
        let mut b = card();
        for i in 0..40 {
            b.add(&format!("D/F{i:02}.JPG"), &[1; 512]);
        }
        let d = b.clusters("D");
        b.set_fat(d[1], 0);
        let l = list(
            &b,
            &Limits {
                fat_dir_bytes: 1024,
                ..Limits::DEFAULT
            },
        );
        assert_eq!(l.files.len(), 30);
        assert!(!l.truncated);
    }

    /// A source recording the offset of every read.
    struct Watch {
        inner: Mem,
        reads: std::sync::Mutex<Vec<u64>>,
    }

    impl Source for Watch {
        fn len(&self) -> u64 {
            self.inner.len()
        }

        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, crate::EngineError> {
            self.reads.lock().unwrap().push(offset);
            self.inner.read_at(offset, buf)
        }

        fn volume(&self) -> Option<crate::source::VolumeId> {
            None
        }
    }

    #[test]
    fn the_walk_stops_reading_once_the_entry_cap_is_reached() {
        let mut b = card();
        b.add("TOP.JPG", &[1; 512]);
        b.add("A/A1.JPG", &[1; 512]);
        b.add("A/B/B1.JPG", &[1; 512]);
        b.add("Z/Z1.JPG", &[1; 512]);
        let v = b.build();
        let len = v.len() + (8 << 20);
        let src = Watch {
            inner: Mem(disk(Partitioning::Mbr { first_lba: 8192 }, &v, len)),
            reads: Default::default(),
        };
        // The root's 5 slots and A's `.`, `..`, A1: the cap is reached inside A.
        let l = read_limited(
            &src,
            &SourceProbe::new(&src),
            &CancelToken::default(),
            &Limits {
                entries: 8,
                ..Limits::DEFAULT
            },
        );
        assert_eq!(paths(&l), ["A/A1.JPG", "TOP.JPG"]);
        assert!(l.truncated);
        let reads = src.reads.lock().unwrap();
        for dir in ["A/B", "Z"] {
            let at = VOL + b.cluster_offset(b.clusters(dir)[0]);
            assert!(!reads.contains(&at), "{dir} was read");
        }
        assert!(reads.contains(&(VOL + b.cluster_offset(b.clusters("A")[0]))));
    }

    #[test]
    fn path_cap_and_budget_drop_folders() {
        let mut b = card();
        b.add("DCIM/A/X.JPG", &[1; 512]);
        b.add("DCIM/B/Y.JPG", &[1; 512]);
        b.add("DCIM/LONGNAME.JPG", &[1; 512]);
        let folder = |l: &Listing, name: &str| {
            let f = l.files.iter().find(|f| f.origin.name() == name).unwrap();
            f.origin.folder().map(str::to_string)
        };
        // Budget: "DCIM" (4) and "DCIM/A" (6) fit in 10 bytes, "DCIM/B" does not.
        let l = list(
            &b,
            &Limits {
                path_budget: 10,
                ..Limits::DEFAULT
            },
        );
        assert_eq!(l.files.len(), 3);
        assert_eq!(folder(&l, "X.JPG").as_deref(), Some("DCIM/A"));
        assert_eq!(folder(&l, "Y.JPG"), None);
        assert_eq!(folder(&l, "LONGNAME.JPG").as_deref(), Some("DCIM"));
        assert!(l.truncated);
        // Cap of 16 bytes: "DCIM/LONGNAME.JPG" (17) loses its folder, "DCIM/A/X.JPG" (12) keeps it.
        let l = list(
            &b,
            &Limits {
                path_bytes: 16,
                ..Limits::DEFAULT
            },
        );
        assert_eq!(l.files.len(), 3);
        assert_eq!(folder(&l, "LONGNAME.JPG"), None);
        assert_eq!(folder(&l, "X.JPG").as_deref(), Some("DCIM/A"));
        assert!(l.truncated);
        assert!(!list(&b, &Limits::DEFAULT).truncated);
    }

    #[test]
    fn a_volume_too_large_to_walk_is_reported_and_truncates() {
        let mut b = card();
        b.add("DCIM/A.JPG", &[1; 512]);
        let mut v = b.build();
        // TotSec32 such that the cluster count passes 2^28 (the boot sector does not need the volume to fit the source).
        let total: u32 = 0x1100_0000;
        v[32..36].copy_from_slice(&total.to_le_bytes());
        let len = v.len() + (8 << 20);
        let src = Mem(disk(Partitioning::Mbr { first_lba: 8192 }, &v, len));
        let l = read_limited(
            &src,
            &SourceProbe::new(&src),
            &CancelToken::default(),
            &Limits::DEFAULT,
        );
        assert_eq!(l.volumes.len(), 1);
        assert!(l.volumes[0].cluster_count > MAX_WALKED_CLUSTERS);
        assert!(l.files.is_empty() && l.live.is_empty());
        assert!(l.truncated);
    }

    /// A source over shared bytes whose reads fail when they touch `bad`; it counts the bytes it returns and the
    /// attempts on `bad`.
    struct Counting {
        data: std::sync::Arc<Vec<u8>>,
        bad: (u64, u64),
        bytes: std::sync::atomic::AtomicU64,
        bad_attempts: std::sync::atomic::AtomicU64,
    }

    impl Counting {
        fn new(data: std::sync::Arc<Vec<u8>>, bad: (u64, u64)) -> Self {
            Counting {
                data,
                bad,
                bytes: Default::default(),
                bad_attempts: Default::default(),
            }
        }
    }

    impl Source for Counting {
        fn len(&self) -> u64 {
            self.data.len() as u64
        }

        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, crate::EngineError> {
            use std::sync::atomic::Ordering::SeqCst;
            let end = offset + buf.len() as u64;
            if offset < self.bad.1 && self.bad.0 < end {
                self.bad_attempts.fetch_add(1, SeqCst);
                return Err(crate::EngineError::Read {
                    offset,
                    cause: std::io::Error::other("injected"),
                });
            }
            let start = (offset as usize).min(self.data.len());
            let n = buf.len().min(self.data.len() - start);
            buf[..n].copy_from_slice(&self.data[start..start + n]);
            self.bytes.fetch_add(n as u64, SeqCst);
            Ok(n)
        }

        fn volume(&self) -> Option<crate::source::VolumeId> {
            None
        }
    }

    /// Rulings D10 and C2: every head check is one 512-byte read through the probe, outside the cache, counted against
    /// one cap per listing; a range the cache knows to be unreadable is refused without a read (and recognizes
    /// nothing).
    #[test]
    fn high_word_probes_are_capped() {
        use crate::cache::CachedSource;
        use revenant_testkit::{fs::DeleteStyle, jpeg};
        use std::sync::atomic::Ordering::SeqCst;
        let mut b = card();
        // A live file over clusters 4,000..=5,000 holds every `lo` below: one candidate per file (h = 1), one probe.
        b.add_at(
            "FILL.BIN",
            &[1; 1001 * 512],
            &(4000..5001).collect::<Vec<_>>(),
        );
        let mut firsts = Vec::new();
        for i in 0..5u32 {
            let data = jpeg(160, 120, i as u64, 0);
            let n = (data.len() as u32).div_ceil(512);
            assert!(n < 100);
            let at = 70_000 + 100 * i;
            let path = format!("DCIM/W{i}.JPG");
            b.add_at(&path, &data, &(at..at + n).collect::<Vec<_>>());
            b.delete(&path, DeleteStyle::WindowsFat32);
            firsts.push(VOL + b.cluster_offset(at));
        }
        let v = b.build();
        let len = v.len() + (8 << 20);
        let bytes = std::sync::Arc::new(disk(Partitioning::Mbr { first_lba: 8192 }, &v, len));
        // W1's head sector is unreadable, and the cache already knows it.
        let bad = (firsts[1], firsts[1] + 512);
        let list = |probes: usize| {
            let meta = Counting::new(bytes.clone(), bad);
            let raw = Counting::new(bytes.clone(), bad);
            let cache = CachedSource::new(&meta, 512, 64 << 20);
            cache.visit(bad.0, bad.1, |_, _, _| {});
            assert!(!cache.bad_runs(bad.0, bad.1).is_empty());
            let probe = SourceProbe::beside(&raw, &cache);
            let limits = Limits {
                probes,
                ..Limits::DEFAULT
            };
            let l = read_limited(&cache, &probe, &CancelToken::default(), &limits);
            let mut starts: Vec<u64> = l
                .files
                .iter()
                .filter(|f| f.origin.state == FsState::Deleted)
                .map(|f| f.pieces[0].start)
                .collect();
            starts.sort();
            (
                starts,
                l.truncated,
                raw.bytes.load(SeqCst),
                raw.bad_attempts.load(SeqCst),
            )
        };
        // Cap 4: W0, the refused W1, W2, W3; W4 finds the cap spent.
        let (starts, truncated, read, bad_reads) = list(4);
        assert_eq!(starts, [firsts[0], firsts[2], firsts[3]]);
        assert!(truncated);
        assert_eq!(read, 3 * 512);
        assert_eq!(bad_reads, 0);
        // Cap 5: room for every check.
        let (starts, truncated, read, bad_reads) = list(5);
        assert_eq!(starts, [firsts[0], firsts[2], firsts[3], firsts[4]]);
        assert!(!truncated);
        assert_eq!((read, bad_reads), (4 * 512, 0));
    }

    #[test]
    fn deleted_directories_are_bounded() {
        use revenant_testkit::{fs::DeleteStyle, jpeg};
        let mut b = card();
        b.add("A/X.JPG", &jpeg(16, 16, 1, 0));
        b.add("D1/SUB/Y.JPG", &jpeg(16, 16, 2, 0));
        b.add("D1/Z.JPG", &jpeg(16, 16, 3, 0));
        b.add("LIVE/W.JPG", &jpeg(16, 16, 4, 0));
        // Inside D1: deleted entries leading back to D1 itself and to the live root.
        let d1 = b.clusters("D1")[0];
        let root = b.clusters("")[0];
        for (name, at) in [(b"\xE5OOP       ", d1), (b"\xE5OOT       ", root)] {
            let mut e = [0u8; 32];
            e[..11].copy_from_slice(name);
            e[11] = 0x10;
            e[20..22].copy_from_slice(&((at >> 16) as u16).to_le_bytes());
            e[26..28].copy_from_slice(&(at as u16).to_le_bytes());
            b.raw_entry("D1", e);
        }
        b.delete("D1", DeleteStyle::Camera);
        let state = |l: &Listing| {
            let mut p: Vec<(String, FsState)> = l
                .files
                .iter()
                .map(|f| (f.origin.path(), f.origin.state))
                .collect();
            p.sort_by(|a, b| a.0.cmp(&b.0));
            p
        };
        let live = [
            ("A/X.JPG".to_string(), FsState::Live),
            ("LIVE/W.JPG".to_string(), FsState::Live),
        ];
        let l = list(&b, &Limits::DEFAULT);
        let mut all = live.to_vec();
        // Y and Z lost their only letter; "A", "X" and "W" share their (blank) bytes 1–3, once each: the lowest wins.
        all.extend([
            ("_1/A.JPG".to_string(), FsState::Deleted),
            ("_1/_UB/A.JPG".to_string(), FsState::Deleted),
        ]);
        all.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(state(&l), all);
        assert!(!l.truncated);
        // Depth 1: D1 is read, D1/SUB is not.
        let l = list(
            &b,
            &Limits {
                depth: 1,
                ..Limits::DEFAULT
            },
        );
        assert_eq!(state(&l).len(), 3);
        assert!(l.files.iter().any(|f| f.origin.path() == "_1/A.JPG"));
        assert!(l.truncated);
        // Three directories: the root, A and LIVE; the deleted D1, met before LIVE, does not take LIVE's room.
        let l = list(
            &b,
            &Limits {
                dirs: 3,
                ..Limits::DEFAULT
            },
        );
        assert_eq!(state(&l), live);
        assert!(l.truncated);
        // No guess step: nothing deleted, the live tree intact.
        let l = list(
            &b,
            &Limits {
                guess_steps: 0,
                ..Limits::DEFAULT
            },
        );
        assert_eq!(state(&l), live);
        assert!(l.truncated);
    }

    /// One entry at a time, as the trait's default does.
    struct Slow<'m, 'a>(&'m mut FatMap<'a>);

    impl ClusterMap for Slow<'_, '_> {
        fn entry(&mut self, c: u32) -> Entry {
            self.0.entry(c)
        }

        fn allocated(&mut self, c: u32) -> Option<bool> {
            self.0.allocated(c)
        }
    }

    /// Fails reads that touch `bad`.
    struct Holes(Mem, Vec<(u64, u64)>);

    impl Source for Holes {
        fn len(&self) -> u64 {
            self.0.len()
        }

        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, crate::EngineError> {
            let end = offset + buf.len() as u64;
            if self.1.iter().any(|&(s, e)| offset < e && s < end) {
                return Err(crate::EngineError::Read {
                    offset,
                    cause: std::io::Error::other("injected"),
                });
            }
            self.0.read_at(offset, buf)
        }

        fn volume(&self) -> Option<crate::source::VolumeId> {
            None
        }
    }

    #[test]
    fn fat_block_counts_agree_with_one_entry_at_a_time() {
        for (fs, size, reserved, fat_len_at) in [
            (FsType::Fat16, 16u64 << 20, 1u64, 22usize),
            (FsType::Fat32, 48 << 20, 32, 36),
        ] {
            let mut b = FsBuilder::new(fs, size, 512);
            b.add("A.JPG", &[1; 3000]);
            b.add_at("B.JPG", &[2; 2048], &[9000, 9001, 9005, 9006]);
            b.add_at("C.JPG", &[3; 512], &[20_000]);
            if fs == FsType::Fat32 {
                // Reserved high bits only: still free.
                b.set_fat(15_000, 0xF000_0000);
            }
            let v = b.build();
            let fat_sectors = if fs == FsType::Fat16 {
                u16_at(&v, fat_len_at) as u64
            } else {
                u32_at(&v, fat_len_at) as u64
            };
            let len = v.len() + (8 << 20);
            let data = disk(Partitioning::Mbr { first_lba: 8192 }, &v, len);
            // The FAT sector holding cluster 9,000's entry is unreadable in both copies.
            let width = if fs == FsType::Fat16 { 2 } else { 4 };
            let sector = (9000 * width / 512) * 512;
            let fat = VOL + reserved * 512;
            let holes: Vec<(u64, u64)> = [fat, fat + fat_sectors * 512]
                .iter()
                .map(|&f| (f + sector, f + sector + 512))
                .collect();
            let src = Holes(Mem(data), holes);
            let vol = crate::filesystem::volumes(&src).remove(0);
            let layout = boot::layout(&src, &vol).unwrap();
            let last = vol.cluster_count + 1;
            let mut fast = FatMap::new(&src, &vol, &layout);
            let mut slow_map = FatMap::new(&src, &vol, &layout);
            let mut slow = Slow(&mut slow_map);
            let mut froms: Vec<u32> = (2..=last).step_by(997).collect();
            froms.extend([
                2, 3, 8990, 9000, 9002, 9007, 14_999, 15_000, 19_999, 20_000, last,
            ]);
            for from in froms.into_iter().filter(|&c| c <= last) {
                for to in [from, from + 1, from + 40, from + 5000, last].map(|t| t.min(last)) {
                    for free in [true, false] {
                        assert_eq!(
                            fast.count(from, to, free),
                            slow.count(from, to, free),
                            "{fs:?} {from}..={to} free {free}"
                        );
                    }
                }
            }
        }
    }

    fn entry(name: &[u8; 11], case: u8) -> [u8; 32] {
        let mut e = [0u8; 32];
        e[..11].copy_from_slice(name);
        e[12] = case;
        e
    }

    #[test]
    fn a_directory_entry_waiting_to_be_walked_is_small() {
        // Up to 65,536 per directory read: its name and a few words, no formatted times.
        assert!(
            std::mem::size_of::<Child>() <= 48,
            "{}",
            std::mem::size_of::<Child>()
        );
    }

    #[test]
    fn a_waiting_directory_is_constant_size() {
        // The walk's stack holds up to 65,536 of these: no run list, only the first cluster and the count.
        assert!(
            std::mem::size_of::<Dir>() <= 64,
            "{}",
            std::mem::size_of::<Dir>()
        );
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
