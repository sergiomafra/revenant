//! The directory tree of an exFAT volume: live files and folders, and deleted ones.
//!
//! From Microsoft's "exFAT file system specification": the FAT (§4.1), the allocation bitmap (§7.1), the up-case
//! table entry (§7.2), directory entry sets and their checksum (§6.3), the File (§7.4), Stream Extension (§7.6) and
//! File Name (§7.7) entries, timestamps (§7.4.8) and UTC offsets (§7.4.10). A deleted entry set keeps everything (name,
//! sizes, first cluster, NoFatChain flag), and a deleted FAT-chained file usually keeps its FAT entries too: exFAT
//! frees clusters in the bitmap only.

use std::collections::BTreeSet;
use std::sync::Arc;

use super::alloc::{
    Budget as Steps, Claims, ClusterMap, Entry, MAX_RUNS, Unclaimed, claim_run, follow, guess,
};
use super::boot::{self, Root};
use super::fat::{FatMap, fat_time};
use super::listing::{
    Budget, Collector, FsOrigin, FsState, FsTime, Listing, Placement, component, extension,
    note_walked,
};
use super::orphans::OldEntry;
use super::{Limits, Volume, read_exact, u16_at, u32_at, u64_at};
use crate::formats::{FileKind, Fragment};
use crate::scan::CancelToken;
use crate::source::Source;

/// Most bytes of a directory or of the bitmap read at once.
const READ_CHUNK: u64 = 128 * 1024;
/// SecondaryCount of a File entry: a stream, at least one name entry, at most 17 (255 characters) plus vendor ones.
const MIN_SECONDARY: usize = 2;
const MAX_SECONDARY: usize = 18;
/// Deleted placements between two cancel checks.
const CANCEL_EVERY: usize = 1024;

/// Lists the files and directories of exFAT volume `v` into `out`, live and deleted.
///
/// The root is the FAT chain from `FirstClusterOfRootDirectory`; it holds the allocation bitmap entry, which is read
/// as soon as it is met (the live files of the root met before it wait for it). The live tree is walked depth first,
/// entries in on-disk order, each live directory and file claiming its clusters. Deleted entry sets met on the way wait:
/// deleted directories are walked once the live tree is done, then every deleted photo/video is placed against the
/// live claims (ruling R10). Bounds as for FAT ([`Limits`]), with 256 MiB per directory.
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
    let Root::Cluster(root) = layout.root else {
        return;
    };
    let last = v.cluster_count.saturating_add(1);
    let mut w = Walk {
        source,
        v,
        cancel,
        limits,
        budget,
        map: ExfatMap {
            fat: FatMap::new(source, v, &layout),
            bits: None,
            last,
        },
        claims: Claims::new(v.cluster_count, limits.contested),
        col: Collector::new(v, source.len()),
        steps: Steps::new(limits.guess_steps),
        dir_clusters: limits.exfat_dir_bytes.div_ceil(v.cluster_size).max(1),
        last,
        dirs: 1,
        entries: 0,
        resolved: false,
        upcase: false,
        early: Vec::new(),
        early_photos: 0,
        early_dirs: 0,
        waiting: Vec::new(),
        later: Vec::new(),
        seen: BTreeSet::new(),
        walked: Vec::new(),
    };
    let root = w.root_chain(root);
    if w.live(root) {
        w.deleted_dirs();
    }
    w.place();
    let Walk {
        col,
        claims,
        walked,
        ..
    } = w;
    out.walked.extend(walked);
    col.finish(&claims, out);
}

/// One volume's walk.
struct Walk<'a, 'b> {
    source: &'a dyn Source,
    v: &'a Volume,
    cancel: &'a CancelToken,
    limits: &'a Limits,
    budget: &'b mut Budget,
    map: ExfatMap<'a>,
    claims: Claims,
    col: Collector<'a>,
    /// Cluster examinations left for deleted entries (ruling C7).
    steps: Steps,
    dir_clusters: u64,
    /// The last cluster of the heap.
    last: u32,
    /// Directories walked or on a stack, live and deleted (not those in `waiting`).
    dirs: usize,
    entries: u64,
    /// The bitmap entry was met (and the bitmap read, when it could be) or the root ended without one.
    resolved: bool,
    /// The up-case table's clusters were claimed.
    upcase: bool,
    /// Live photos/videos and directories of the root met before the bitmap entry (their trust needs it): at most the
    /// photo slots left and [`Limits::dirs`] directories.
    early: Vec<Child>,
    early_photos: usize,
    early_dirs: usize,
    /// Deleted directories met in live directories, in walk order.
    waiting: Vec<Dir>,
    /// Deleted photos/videos, in walk order, placed by [`Walk::place`] once the live walk is done.
    later: Vec<Later>,
    /// First clusters of the deleted directories walked (a deleted tree has no claims to stop a loop).
    seen: BTreeSet<u32>,
    /// The directory clusters read, live and deleted ([`Listing::walked`]).
    walked: Vec<Fragment>,
}

/// A deleted photo/video waiting to be placed.
struct Later {
    folder: Option<Arc<str>>,
    child: Child,
    kind: FileKind,
}

/// A directory waiting to be read.
struct Dir {
    /// Its own path (the folder of its files); `None` once dropped by the path cap or budget.
    folder: Option<Arc<str>>,
    depth: u32,
    region: Region,
    /// A deleted directory: every entry in it is deleted.
    deleted: bool,
}

/// Where a directory's entries are.
enum Region {
    /// A FAT chain already followed (and claimed): `clusters` clusters from `first`, followed again when read.
    Chain { first: u32, clusters: u64 },
    /// Runs of consecutive clusters (a NoFatChain directory, or a deleted one once placed).
    Runs(Vec<(u32, u32)>),
    /// A deleted directory, placed when it is read.
    Deleted { first: u32, need: u64, nofat: bool },
}

/// Where a deleted file's clusters probably are.
enum Located {
    Found {
        runs: Vec<(u32, u32)>,
        placement: Placement,
        /// The placement alone allows trust.
        ok: bool,
        /// Clusters free today from the first one (a recorded run; every cluster of a guessed one).
        free: u64,
    },
    /// Nothing to list: the first cluster is outside the heap or in use today.
    Skip,
    /// The volume's step budget ran out.
    Exhausted,
}

impl<'a> Walk<'a, '_> {
    /// Follows the root's chain for at most `dir_clusters` clusters and hands it to the collector (trusted, as a FAT
    /// directory is, when it ends within the cap with no cycle or cross-link); a chain that goes on past the cap to a
    /// cluster no chain holds marks the listing truncated.
    fn root_chain(&mut self, first: u32) -> Region {
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

    /// The live tree; false when the walk of the volume must stop there (cancelled, or the entry cap reached).
    fn live(&mut self, root: Region) -> bool {
        let mut stack = vec![Dir {
            folder: Some(Arc::from("")),
            depth: 0,
            region: root,
            deleted: false,
        }];
        let mut is_root = true;
        while let Some(d) = stack.pop() {
            if self.cancel.is_cancelled() {
                return false;
            }
            let mut subdirs = Vec::new();
            let capped = self.read_dir(&d, is_root, &mut subdirs);
            is_root = false;
            if capped {
                self.budget.truncated = true;
                return false;
            }
            stack.extend(subdirs.into_iter().rev());
        }
        true
    }

    /// The deleted directories met in the live tree, each walked depth first.
    fn deleted_dirs(&mut self) {
        for top in std::mem::take(&mut self.waiting) {
            if self.dirs >= self.limits.dirs {
                self.budget.truncated = true;
                return;
            }
            self.dirs += 1;
            let mut stack = vec![top];
            while let Some(mut d) = stack.pop() {
                if self.cancel.is_cancelled() {
                    return;
                }
                if self.steps.exhausted() {
                    self.budget.truncated = true;
                    return;
                }
                let Region::Deleted { first, need, nofat } = d.region else {
                    continue;
                };
                if !self.seen.insert(first) {
                    continue;
                }
                let runs = match self.locate(first, need, nofat) {
                    Located::Found {
                        mut runs,
                        placement,
                        free,
                        ..
                    } => {
                        // A recorded run is read only as far as its clusters are free today.
                        if placement == Placement::Recorded {
                            runs[0].1 = free as u32;
                        }
                        runs
                    }
                    Located::Skip => continue,
                    Located::Exhausted => {
                        self.budget.truncated = true;
                        return;
                    }
                };
                d.region = Region::Runs(runs);
                let mut subdirs = Vec::new();
                if self.read_dir(&d, false, &mut subdirs) {
                    self.budget.truncated = true;
                    return;
                }
                stack.extend(subdirs.into_iter().rev());
            }
        }
    }

    /// Ruling R10: the deleted photos/videos, placed once every live chain of the volume has claimed its clusters (a
    /// claimed cluster is in use, whatever the bitmap says) and the live files took their share of the listing's caps,
    /// in walk order, until the step budget runs out.
    fn place(&mut self) {
        let later = std::mem::take(&mut self.later);
        let cs = self.v.cluster_size;
        for (k, f) in later.into_iter().enumerate() {
            if k % CANCEL_EVERY == 0 && self.cancel.is_cancelled() {
                return;
            }
            let need = f.child.len.div_ceil(cs);
            let (runs, placement, ok) = match self.locate(f.child.first, need, f.child.nofat) {
                Located::Found {
                    runs,
                    placement,
                    ok,
                    ..
                } => (runs, placement, ok),
                Located::Skip => continue,
                Located::Exhausted => {
                    self.budget.truncated = true;
                    return;
                }
            };
            let ok = ok && self.map.bits.is_some() && f.child.valid == f.child.len;
            let origin = self.origin(f.folder, f.child, FsState::Deleted, placement);
            self.col.deleted(self.budget, origin, f.kind, runs, ok);
        }
    }

    /// Where a deleted file (or directory) of `need` clusters from `first` probably is, against the live claims:
    /// - NoFatChain: the run as recorded (cut at the heap's end), trusted when every cluster of it is free today;
    /// - FAT-chained: its old chain when it has exactly `need` clusters, all free today, the last one's entry End
    ///   (trusted when it is one run); otherwise the D4 guess (trusted when one run, not short).
    ///
    /// The old chain, the bitmap check and the guess each spend a step per cluster examined.
    fn locate(&mut self, first: u32, need: u64, nofat: bool) -> Located {
        let last = self.last;
        let mut map = Unclaimed {
            map: &mut self.map,
            claims: &self.claims,
        };
        if need == 0 || first < 2 || first > last || map.allocated(first) != Some(false) {
            return Located::Skip;
        }
        if nofat {
            let n = need.min((last - first) as u64 + 1);
            let Some(free) = free_prefix(&mut map, &mut self.steps, first, n) else {
                return Located::Exhausted;
            };
            return Located::Found {
                runs: vec![(first, n as u32)],
                placement: Placement::Recorded,
                ok: free == need,
                free,
            };
        }
        match old_chain(&mut map, &mut self.steps, first, need, last) {
            Err(()) => Located::Exhausted,
            Ok(Some(runs)) => Located::Found {
                ok: runs.len() == 1,
                placement: Placement::Guessed,
                free: need,
                runs,
            },
            Ok(None) => match guess(&mut map, &mut self.steps, first, need, last) {
                Some(g) => Located::Found {
                    ok: g.runs.len() == 1 && !g.short,
                    placement: Placement::Guessed,
                    free: g.runs.iter().map(|&(_, n)| n as u64).sum(),
                    runs: g.runs,
                },
                None if self.steps.exhausted() => Located::Exhausted,
                None => Located::Skip,
            },
        }
    }

    /// Reads the entries of directory `d` (in the root, also the bitmap and up-case entries), up to its end-of-directory
    /// entry or the volume's entry budget; true when the budget ran out. An unreadable chunk is skipped (an entry set
    /// across it is lost).
    fn read_dir(&mut self, d: &Dir, root: bool, subdirs: &mut Vec<Dir>) -> bool {
        let v = self.v;
        let cs = v.cluster_size;
        let mut sets = Sets::default();
        let mut clusters = Clusters::new(&d.region);
        let mut capped = false;
        'dir: while let Some(c) = clusters.next(&mut self.map) {
            let Some(at) = v.cluster_offset(c) else {
                break;
            };
            note_walked(&mut self.walked, at, cs, self.limits.pieces);
            let mut off = 0;
            while off < cs {
                let n = READ_CHUNK.min(cs - off);
                let chunk = read_exact(self.source, at + off, n as usize);
                off += n;
                let Some(chunk) = chunk else {
                    sets.reset();
                    continue;
                };
                for e in chunk.as_chunks::<32>().0 {
                    if self.entries >= self.limits.entries {
                        capped = true;
                        break 'dir;
                    }
                    self.entries += 1;
                    match sets.push(e) {
                        Step::None => {}
                        Step::End => break 'dir,
                        Step::Set(ch) => self.child(ch, d, root, subdirs),
                        Step::Meta(m) if root => self.meta(&m, d, subdirs),
                        Step::Meta(_) => {}
                    }
                }
            }
        }
        if root && !self.resolved {
            self.resolve(d, subdirs);
        }
        capped
    }

    /// A bitmap (`0x81`) or up-case table (`0x82`) entry of the root: the first bitmap whose BitmapFlags select the
    /// first FAT is read, the up-case table's clusters are claimed.
    fn meta(&mut self, e: &[u8; 32], d: &Dir, subdirs: &mut Vec<Dir>) {
        let first = u32_at(e, 20);
        let len = u64_at(e, 24);
        match e[0] {
            0x81 if !self.resolved && e[1] & 1 == 0 => {
                self.load_bitmap(first, len);
                self.resolve(d, subdirs);
            }
            0x82 if !self.upcase => {
                self.upcase = true;
                follow(
                    &mut self.map,
                    &mut self.claims,
                    first,
                    len.div_ceil(self.v.cluster_size),
                );
            }
            _ => {}
        }
    }

    /// The bitmap is settled: the root's live entries met before it are handled now.
    fn resolve(&mut self, d: &Dir, subdirs: &mut Vec<Dir>) {
        self.resolved = true;
        self.early_photos = 0;
        self.early_dirs = 0;
        for ch in std::mem::take(&mut self.early) {
            self.child(ch, d, true, subdirs);
        }
    }

    /// Reads the allocation bitmap (claiming its clusters): FirstCluster inside the heap, DataLength at least
    /// `ceil(ClusterCount / 8)` and at most that rounded up to whole clusters, its FAT chain holding the clusters those
    /// bytes need, every byte readable. Otherwise there is no bitmap.
    fn load_bitmap(&mut self, first: u32, len: u64) {
        let v = self.v;
        let cs = v.cluster_size;
        let bytes = (v.cluster_count as u64).div_ceil(8);
        if len < bytes || len > bytes.next_multiple_of(cs) || !self.claims.in_heap(first) {
            return;
        }
        let need = bytes.div_ceil(cs);
        let chain = follow(&mut self.map, &mut self.claims, first, need);
        if chain.runs.iter().map(|&(_, n)| n as u64).sum::<u64>() < need {
            return;
        }
        let mut words = vec![0u64; (v.cluster_count as usize).div_ceil(64)];
        let mut k = 0u64;
        for &(f, n) in &chain.runs {
            let Some(start) = v.cluster_offset(f) else {
                return;
            };
            let run = n as u64 * cs;
            let mut off = 0;
            while off < run && k < bytes {
                let len = READ_CHUNK.min(run - off).min(bytes - k);
                let Some(buf) = read_exact(self.source, start + off, len as usize) else {
                    return;
                };
                for (i, &b) in buf.iter().enumerate() {
                    let at = k as usize + i;
                    words[at / 8] |= (b as u64) << (8 * (at % 8));
                }
                off += len;
                k += len;
            }
        }
        self.map.bits = Some(words);
    }

    /// An entry set of directory `d`.
    fn child(&mut self, ch: Child, d: &Dir, root: bool, subdirs: &mut Vec<Dir>) {
        if d.deleted {
            self.deleted_child(ch, d, Some(subdirs));
        } else if ch.deleted {
            self.deleted_child(ch, d, None);
        } else if root && !self.resolved {
            self.wait_for_bitmap(ch, d);
        } else if ch.dir {
            self.live_dir(ch, d, subdirs);
        } else {
            self.live_file(ch, d);
        }
    }

    /// A live entry of the root met before the bitmap entry. A photo/video or a directory, whose trust depends on the
    /// bitmap, waits for it (past the photo slots left or the directory cap it is dropped and the listing truncated);
    /// any other file carries no trust and is handled at once.
    fn wait_for_bitmap(&mut self, ch: Child, d: &Dir) {
        let (count, cap) = if ch.dir {
            (&mut self.early_dirs, self.limits.dirs)
        } else if extension(&ch.name)
            .and_then(FileKind::from_extension)
            .is_some()
        {
            (&mut self.early_photos, self.budget.live_files_left())
        } else {
            self.live_file(ch, d);
            return;
        };
        if *count >= cap {
            self.budget.truncated = true;
            return;
        }
        *count += 1;
        self.early.push(ch);
    }

    /// A live directory: claims its clusters (its DataLength's worth, at most 256 MiB) and waits to be read. Not
    /// entered when its first cluster is outside the heap or already claimed (a loop or a second link). Trusted when
    /// every cluster its DataLength needs is claimed with no conflict (a FAT chain ending there; a NoFatChain run the
    /// bitmap shows in use).
    fn live_dir(&mut self, ch: Child, d: &Dir, subdirs: &mut Vec<Dir>) {
        let cs = self.v.cluster_size;
        let need = ch.len.div_ceil(cs);
        if !ch.alloc
            || need == 0
            || !self.claims.in_heap(ch.first)
            || self.claims.is_claimed(ch.first)
        {
            return;
        }
        if d.depth >= self.limits.depth || self.dirs >= self.limits.dirs {
            self.budget.truncated = true;
            return;
        }
        self.dirs += 1;
        let over = need > self.dir_clusters;
        if over {
            self.budget.truncated = true;
        }
        let need = need.min(self.dir_clusters);
        let (mut chain, region) = if ch.nofat {
            let chain = claim_run(&mut self.claims, ch.first, need);
            let region = Region::Runs(chain.runs.clone());
            (chain, region)
        } else {
            let chain = follow(&mut self.map, &mut self.claims, ch.first, need);
            let clusters = chain.runs.iter().map(|&(_, n)| n as u64).sum();
            (
                chain,
                Region::Chain {
                    first: ch.first,
                    clusters,
                },
            )
        };
        chain.ended = chain.complete && !over && (!ch.nofat || self.in_use(ch.first, need));
        self.col.dir(self.budget, &chain);
        subdirs.push(Dir {
            folder: self.budget.subfolder(d.folder.as_ref(), &ch.name),
            depth: d.depth + 1,
            region,
            deleted: false,
        });
    }

    /// A live file: claims its clusters (NoFatChain: one run of `ceil(DataLength / cluster)`; otherwise its FAT chain).
    /// Not trusted when the run or chain is incomplete or conflicts, a NoFatChain cluster is free in the bitmap, or
    /// ValidDataLength differs from DataLength.
    fn live_file(&mut self, ch: Child, d: &Dir) {
        if !ch.alloc || ch.len == 0 {
            return;
        }
        let need = ch.len.div_ceil(self.v.cluster_size);
        let mut chain = if ch.nofat {
            claim_run(&mut self.claims, ch.first, need)
        } else {
            follow(&mut self.map, &mut self.claims, ch.first, need)
        };
        if (ch.nofat && chain.complete && !self.in_use(ch.first, need)) || ch.valid != ch.len {
            chain.complete = false;
        }
        let origin = self.origin(d.folder.clone(), ch, FsState::Live, Placement::Chain);
        self.col.file(self.budget, origin, chain);
        // Live files take the caps first: a deleted file waiting past the photo slots left is dropped.
        while self.later.len() > self.budget.files_left() {
            self.later.pop();
            self.budget.truncated = true;
        }
    }

    /// Whether the bitmap shows all of `first .. first + n` in use (true without a bitmap).
    fn in_use(&mut self, first: u32, n: u64) -> bool {
        if self.map.bits.is_none() {
            return true;
        }
        self.map.count(first, first + (n - 1) as u32, false) == n
    }

    /// A deleted entry set of directory `d`. A directory waits (`subdirs: None`, the live tree) or is walked next
    /// (`subdirs`); a photo/video waits for [`Walk::place`]; anything else is dropped.
    fn deleted_child(&mut self, ch: Child, d: &Dir, subdirs: Option<&mut Vec<Dir>>) {
        if !ch.alloc || !self.claims.in_heap(ch.first) || ch.len == 0 {
            return;
        }
        if ch.dir {
            let full = match subdirs {
                Some(_) => self.dirs >= self.limits.dirs,
                None => self.waiting.len() >= self.limits.dirs,
            };
            if d.depth >= self.limits.depth || full {
                self.budget.truncated = true;
                return;
            }
            let need = ch.len.div_ceil(self.v.cluster_size);
            if need > self.dir_clusters {
                self.budget.truncated = true;
            }
            let dir = Dir {
                folder: self.budget.subfolder(d.folder.as_ref(), &ch.name),
                depth: d.depth + 1,
                region: Region::Deleted {
                    first: ch.first,
                    need: need.min(self.dir_clusters),
                    nofat: ch.nofat,
                },
                deleted: true,
            };
            match subdirs {
                Some(s) => {
                    self.dirs += 1;
                    s.push(dir);
                }
                None => self.waiting.push(dir),
            }
            return;
        }
        let Some(kind) = extension(&ch.name).and_then(FileKind::from_extension) else {
            return;
        };
        // Placed once the live walk is done (ruling R10): never more waiting than photo slots are left.
        if self.later.len() >= self.budget.files_left() {
            self.budget.truncated = true;
            return;
        }
        self.later.push(Later {
            folder: d.folder.clone(),
            child: ch,
            kind,
        });
    }

    fn origin(
        &self,
        folder: Option<Arc<str>>,
        ch: Child,
        state: FsState,
        placement: Placement,
    ) -> FsOrigin {
        FsOrigin {
            volume: self.v.index,
            folder,
            name: ch.name,
            state,
            placement,
            size: ch.len,
            created: exfat_time(ch.created),
            modified: exfat_time(ch.modified),
        }
    }
}

/// How many clusters from `first` (of a run of `n`) are free today, spending a step per cluster examined (the scan
/// stops at the first one in use); `None` when the budget runs out first.
fn free_prefix(map: &mut dyn ClusterMap, steps: &mut Steps, first: u32, n: u64) -> Option<u64> {
    let span = n.min(steps.left());
    if span == 0 {
        steps.spend(1);
        return None;
    }
    let free = map.count(first, first + (span - 1) as u32, true);
    steps.spend((free + 1).min(span));
    if free == span && span < n {
        steps.spend(n);
        return None;
    }
    Some(free)
}

/// The FAT chain a deleted file left: from `first`, every cluster free today, exactly `need` of them, the last one's
/// entry End, in at most [`MAX_RUNS`] runs; `Ok(None)` otherwise. One step per cluster; `Err(())` when the budget runs
/// out first.
fn old_chain(
    map: &mut dyn ClusterMap,
    steps: &mut Steps,
    first: u32,
    need: u64,
    last: u32,
) -> Result<Option<Vec<(u32, u32)>>, ()> {
    let mut runs: Vec<(u32, u32)> = Vec::new();
    let mut c = first;
    let mut taken = 0u64;
    loop {
        if !steps.spend(1) {
            return Err(());
        }
        if c < 2 || c > last || map.allocated(c) != Some(false) {
            return Ok(None);
        }
        let extends = runs.last().is_some_and(|&(start, n)| start + n == c);
        if extends {
            runs.last_mut().expect("a last run").1 += 1;
        } else if runs.len() < MAX_RUNS {
            runs.push((c, 1));
        } else {
            return Ok(None);
        }
        taken += 1;
        match map.entry(c) {
            Entry::End => return Ok((taken == need).then_some(runs)),
            Entry::Next(n) if taken < need => c = n,
            _ => return Ok(None),
        }
    }
}

/// An exFAT timestamp (the DOS date in the high 16 bits, the time in the low 16), its 10 ms count added and floored to
/// the second, and its UTC offset when the offset byte's bit 7 marks it valid (bits 0–6: signed 15-minute units).
fn exfat_time((stamp, centis, offset): (u32, u8, u8)) -> Option<FsTime> {
    let mut t = fat_time((stamp >> 16) as u16, stamp as u16, Some(centis))?;
    t.utc_offset_minutes = (offset & 0x80 != 0).then(|| ((offset << 1) as i8 >> 1) as i16 * 15);
    Some(t)
}

/// The clusters of a directory region, in order.
enum Clusters<'r> {
    Chain {
        next: u32,
        left: u64,
    },
    Runs {
        runs: &'r [(u32, u32)],
        at: usize,
        k: u32,
    },
}

impl<'r> Clusters<'r> {
    fn new(region: &'r Region) -> Self {
        match region {
            &Region::Chain { first, clusters } => Clusters::Chain {
                next: first,
                left: clusters,
            },
            Region::Runs(runs) => Clusters::Runs { runs, at: 0, k: 0 },
            Region::Deleted { .. } => Clusters::Chain { next: 0, left: 0 },
        }
    }

    fn next(&mut self, map: &mut dyn ClusterMap) -> Option<u32> {
        match self {
            Clusters::Chain { next, left } => {
                if *left == 0 {
                    return None;
                }
                let c = *next;
                *left -= 1;
                if *left > 0 {
                    match map.entry(c) {
                        Entry::Next(n) => *next = n,
                        _ => *left = 0,
                    }
                }
                Some(c)
            }
            Clusters::Runs { runs, at, k } => {
                let &(first, n) = runs.get(*at)?;
                let c = first + *k;
                *k += 1;
                if *k >= n {
                    *at += 1;
                    *k = 0;
                }
                Some(c)
            }
        }
    }
}

/// One file or directory entry set, decoded.
#[derive(Debug)]
struct Child {
    /// One path component (sanitized).
    name: String,
    dir: bool,
    /// The File entry's InUse bit is clear.
    deleted: bool,
    /// AllocationPossible.
    alloc: bool,
    /// NoFatChain.
    nofat: bool,
    first: u32,
    /// DataLength.
    len: u64,
    /// ValidDataLength.
    valid: u64,
    /// Creation and modification: (timestamp, 10 ms count, UTC offset byte).
    created: (u32, u8, u8),
    modified: (u32, u8, u8),
}

/// What one directory entry gave.
enum Step {
    None,
    /// `0x00`: the directory ends.
    End,
    Set(Child),
    /// A bitmap or up-case table entry.
    Meta([u8; 32]),
}

/// Collects entry sets one entry at a time: a File entry (`0x85`, or `0x05` deleted) with SecondaryCount in 2..=18,
/// then the Stream Extension (`0xC0`/`0x40`; NameLength 1..=255 and `1 + ceil(NameLength / 15)` ≤ SecondaryCount),
/// then the File Name entries (`0xC1`/`0x41`), then any other secondary entries the count holds. An entry that breaks
/// the set drops it and is read afresh (it may start the next set); a complete set whose SetChecksum does not hold is
/// dropped.
struct Sets {
    buf: [[u8; 32]; MAX_SECONDARY + 1],
    len: usize,
    want: usize,
    names: usize,
}

impl Default for Sets {
    fn default() -> Self {
        Sets {
            buf: [[0; 32]; MAX_SECONDARY + 1],
            len: 0,
            want: 0,
            names: 0,
        }
    }
}

impl Sets {
    /// An unreadable stretch: the set being collected is lost.
    fn reset(&mut self) {
        self.len = 0;
    }

    fn push(&mut self, e: &[u8; 32]) -> Step {
        if self.len > 0 {
            let t = e[0] & 0x7F;
            let fits = match self.len {
                1 => {
                    let name_len = e[3] as usize;
                    self.names = name_len.div_ceil(15);
                    t == 0x40 && name_len >= 1 && 1 + self.names < self.want
                }
                k if k < 2 + self.names => t == 0x41,
                _ => t & 0x40 != 0,
            };
            if fits {
                self.buf[self.len] = *e;
                self.len += 1;
                if self.len < self.want {
                    return Step::None;
                }
                self.len = 0;
                return match self.finish() {
                    Some(ch) => Step::Set(ch),
                    None => Step::None,
                };
            }
            self.len = 0;
        }
        match e[0] {
            0x00 => Step::End,
            0x85 | 0x05 => {
                let count = e[1] as usize;
                if (MIN_SECONDARY..=MAX_SECONDARY).contains(&count) {
                    self.buf[0] = *e;
                    self.len = 1;
                    self.want = count + 1;
                }
                Step::None
            }
            0x81 | 0x82 => Step::Meta(*e),
            _ => Step::None,
        }
    }

    /// The complete set in `buf[..want]`, when its checksum holds.
    fn finish(&self) -> Option<Child> {
        let set = &self.buf[..self.want];
        if set_checksum(set) != u16_at(&set[0], 2) {
            return None;
        }
        let (p, s) = (&set[0], &set[1]);
        let name_len = s[3] as usize;
        let mut units = [0u16; 255];
        for (k, u) in units[..name_len].iter_mut().enumerate() {
            *u = u16_at(&set[2 + k / 15], 2 + 2 * (k % 15));
        }
        Some(Child {
            name: component(&String::from_utf16_lossy(&units[..name_len])),
            dir: u16_at(p, 4) & 0x10 != 0,
            deleted: p[0] & 0x80 == 0,
            alloc: s[1] & 0x01 != 0,
            nofat: s[1] & 0x02 != 0,
            first: u32_at(s, 20),
            len: u64_at(s, 24),
            valid: u64_at(s, 8),
            created: (u32_at(p, 8), p[20], p[22]),
            modified: (u32_at(p, 12), p[21], p[23]),
        })
    }
}

/// What one entry of an old directory gave (ruling D11).
pub(super) enum OldStep {
    None,
    /// `0x00`: the directory ends.
    End,
    /// An entry set whose File entry is in use.
    Entry(OldEntry),
}

/// An old directory met by the scan (ruling D11), parsed with the live walk's rules ([`Sets`]); deleted sets are left
/// out.
#[derive(Default)]
pub(super) struct OldSets(Sets);

impl OldSets {
    pub(super) fn push(&mut self, e: &[u8; 32]) -> OldStep {
        match self.0.push(e) {
            Step::End => OldStep::End,
            Step::Set(ch) if !ch.deleted => OldStep::Entry(OldEntry {
                created: exfat_time(ch.created),
                modified: exfat_time(ch.modified),
                name: ch.name,
                dir: ch.dir,
                first: ch.first,
                size: ch.len,
            }),
            _ => OldStep::None,
        }
    }
}

/// Whether `b` (a cluster's first bytes) starts with a whole entry set: a File entry (`0x85`, or `0x05` deleted) whose
/// SecondaryCount is 2..=18 and whose set, inside `b`, holds together with a valid SetChecksum (InUse bits forced).
pub(super) fn starts_set(b: &[u8]) -> bool {
    if !matches!(b.first(), Some(0x85 | 0x05)) {
        return false;
    }
    let count = b[1] as usize;
    let need = (count + 1) * 32;
    if !(MIN_SECONDARY..=MAX_SECONDARY).contains(&count) || need > b.len() {
        return false;
    }
    let mut sets = Sets::default();
    let entries = b[..need].as_chunks::<32>().0;
    let (last, rest) = entries.split_last().expect("at least 3 entries");
    for (k, e) in rest.iter().enumerate() {
        // Each entry must extend the set that started at byte 0 (not drop it and start another).
        if !matches!(sets.push(e), Step::None) || sets.len != k + 1 {
            return false;
        }
    }
    matches!(sets.push(last), Step::Set(_))
}

/// SetChecksum (spec §6.3.3) over every entry of `set`, bytes 2–3 of the primary skipped, with each type byte's InUse
/// bit set first: a deleted set keeps the checksum of its live form.
fn set_checksum(set: &[[u8; 32]]) -> u16 {
    let mut sum = 0u16;
    for (k, e) in set.iter().enumerate() {
        for (i, &b) in e.iter().enumerate() {
            if k == 0 && (i == 2 || i == 3) {
                continue;
            }
            let b = if i == 0 { b | 0x80 } else { b };
            sum = sum.rotate_right(1).wrapping_add(b as u16);
        }
    }
    sum
}

/// exFAT's allocation: the FAT for chains, the bitmap for what is in use. Without a readable bitmap every cluster of
/// the heap reads free, so the deleted-file placements see only the live claims ([`Unclaimed`]).
struct ExfatMap<'a> {
    fat: FatMap<'a>,
    /// Bit `k` = cluster `k + 2` in use.
    bits: Option<Vec<u64>>,
    last: u32,
}

impl ClusterMap for ExfatMap<'_> {
    fn entry(&mut self, c: u32) -> Entry {
        self.fat.entry(c)
    }

    fn allocated(&mut self, c: u32) -> Option<bool> {
        if c < 2 || c > self.last {
            return None;
        }
        let k = (c - 2) as usize;
        Some(
            self.bits
                .as_ref()
                .is_some_and(|w| w[k / 64] >> (k % 64) & 1 != 0),
        )
    }

    fn count(&mut self, from: u32, to: u32, free: bool) -> u64 {
        match &self.bits {
            Some(w) => count_bits(w, self.last, from, to, free),
            None if free && from >= 2 && from <= to.min(self.last) => {
                (to.min(self.last) - from) as u64 + 1
            }
            None => 0,
        }
    }
}

/// [`ClusterMap::count`] over bitmap words `w` (bit `k` = cluster `k + 2` in use), 64 clusters at a time.
fn count_bits(w: &[u64], last: u32, from: u32, to: u32, free: bool) -> u64 {
    let to = to.min(last);
    if from < 2 || from > to {
        return 0;
    }
    let (start, end) = ((from - 2) as u64, (to - 2) as u64);
    let mut k = start;
    while k <= end {
        let word = w[(k / 64) as usize];
        let differs = (if free { word } else { !word }) >> (k % 64);
        if differs != 0 {
            return (k + differs.trailing_zeros() as u64).min(end + 1) - start;
        }
        k = (k / 64 + 1) * 64;
    }
    end + 1 - start
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walked_holds_every_directory_read() {
        use crate::filesystem::listing::read_limited;
        use crate::filesystem::{Limits, SourceProbe};
        use crate::formats::tests::Mem;
        use revenant_testkit::fs::{DeleteStyle, FsBuilder, FsType, Partitioning, disk};
        let mut b = FsBuilder::new(FsType::Exfat, 32 << 20, 4096);
        let jpg = b.add("DCIM/100MSDCF/A.JPG", &[1; 10_000]);
        b.add("DCIM/OLD/B.JPG", &[2; 1024]);
        b.delete("DCIM/OLD", DeleteStyle::Camera);
        let v = b.build();
        let len = v.len() + (8 << 20);
        let src = Mem(disk(Partitioning::Mbr { first_lba: 8192 }, &v, len));
        let l = read_limited(
            &src,
            &SourceProbe::new(&src),
            &CancelToken::default(),
            &Limits::DEFAULT,
        );
        let inside = |set: &[Fragment], c: u32| {
            let at = 8192 * 512 + b.cluster_offset(c);
            set.iter().any(|r| r.start <= at && at < r.start + r.len)
        };
        for dir in ["", "DCIM", "DCIM/100MSDCF", "DCIM/OLD"] {
            for c in b.clusters(dir) {
                assert!(inside(&l.walked, c), "{dir:?} cluster {c}");
            }
        }
        assert!(!inside(&l.live, b.clusters("DCIM/OLD")[0]));
        assert!(
            jpg.iter()
                .all(|&c| inside(&l.live, c) && !inside(&l.walked, c))
        );
    }

    #[test]
    fn utc_offsets_and_ten_millisecond_counts() {
        // 2025-12-14 07:12:04: date (45 << 9 | 12 << 5 | 14), time (7 << 11 | 12 << 5 | 2).
        let stamp = (45u32 << 9 | 12 << 5 | 14) << 16 | (7 << 11 | 12 << 5 | 2);
        let t = |centis, offset| exfat_time((stamp, centis, offset)).unwrap();
        assert_eq!(t(0, 0).local, "2025-12-14T07:12:04");
        assert_eq!(t(0, 0).utc_offset_minutes, None);
        assert_eq!(t(199, 0).local, "2025-12-14T07:12:05");
        assert_eq!(t(0, 0x80 | 0x74).utc_offset_minutes, Some(-180));
        assert_eq!(t(0, 0x80 | 23).utc_offset_minutes, Some(345));
        assert_eq!(t(0, 0x80).utc_offset_minutes, Some(0));
        assert_eq!(t(0, 0x80 | 0x40).utc_offset_minutes, Some(-960));
        // Without bit 7 the offset is unknown, whatever bits 0–6 hold.
        assert_eq!(t(0, 0x74).utc_offset_minutes, None);
        assert!(exfat_time((stamp, 200, 0)).is_none());
    }

    #[test]
    fn checksum_ignores_its_own_field_and_the_in_use_bits() {
        let mut set = [[0u8; 32]; 3];
        set[0][0] = 0x85;
        set[1][0] = 0xC0;
        set[2][0] = 0xC1;
        set[2][2] = b'A';
        let sum = set_checksum(&set);
        set[0][2] = 0x12;
        set[0][3] = 0x34;
        assert_eq!(set_checksum(&set), sum);
        for e in &mut set {
            e[0] &= 0x7F;
        }
        assert_eq!(set_checksum(&set), sum);
        set[2][3] = 1;
        assert_ne!(set_checksum(&set), sum);
    }

    #[test]
    fn bitmap_count_matches_cluster_by_cluster() {
        let last = 300u32;
        let mut w = vec![0u64; 5];
        for c in [2u32, 3, 70, 71, 72, 130, 131, 200, 260, 261, 262, 263, 300] {
            let k = (c - 2) as usize;
            w[k / 64] |= 1 << (k % 64);
        }
        let used = |c: u32| {
            let k = (c - 2) as usize;
            w[k / 64] >> (k % 64) & 1 != 0
        };
        let slow = |from: u32, to: u32, free: bool| {
            let mut n = 0;
            let mut c = from;
            while c <= to.min(last) && used(c) != free {
                n += 1;
                c += 1;
            }
            n
        };
        for from in 2..=last {
            for to in [from, from + 1, from + 63, from + 64, from + 200, 400] {
                for free in [true, false] {
                    assert_eq!(
                        count_bits(&w, last, from, to, free),
                        slow(from, to, free),
                        "{from}..={to} {free}"
                    );
                }
            }
        }
        assert_eq!(count_bits(&w, last, 0, 10, false), 0);
        assert_eq!(count_bits(&w, last, 301, 400, true), 0);
    }

    /// The core README's memory figures.
    #[test]
    fn waiting_entries_are_small() {
        assert!(size_of::<Later>() <= 100, "{}", size_of::<Later>());
        assert!(size_of::<Child>() <= 64, "{}", size_of::<Child>());
        assert!(size_of::<Dir>() <= 64, "{}", size_of::<Dir>());
        assert_eq!(size_of::<[[u8; 32]; MAX_SECONDARY + 1]>(), 608);
    }

    #[test]
    fn broken_sets_restart_on_the_entry_that_broke_them() {
        let mut sets = Sets::default();
        let mut file = [0u8; 32];
        file[0] = 0x85;
        file[1] = 2;
        // A File entry, then another File entry: the second starts a set.
        assert!(matches!(sets.push(&file), Step::None));
        assert_eq!(sets.len, 1);
        assert!(matches!(sets.push(&file), Step::None));
        assert_eq!(sets.len, 1);
        // An end marker inside a set ends the directory.
        assert!(matches!(sets.push(&[0u8; 32]), Step::End));
        assert_eq!(sets.len, 0);
    }
}
