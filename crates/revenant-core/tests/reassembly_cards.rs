//! Realistic fragmented cards (part 1.6): a camera that writes each shot's RAW and then its JPEG into the next free
//! clusters (the lowest free one, or a rotating next-free pointer that wraps at the end of the card), deletes in camera,
//! quick formats, deletes on a computer and bursts of look-alike photos; and two oracles over every card (ruling Q51):
//! no `Intact` finding is two files glued together, and every file still whole on the card comes back with its exact
//! bytes and the health its proof allows.
//!
//! The cards are built once per test binary (the files are encoded once, in parallel) and scanned once each; the
//! scenario tests and `no_intact_finding_is_glued` share the outcomes.

mod support;

use std::borrow::Cow;
use std::collections::HashSet;
use std::ops::Range;
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::Instant;

use revenant_core::formats::{FileKind, Health, all_formats};
use revenant_core::{CancelToken, Event, Finding, Fragment, ScanOptions, extract, scan};
use revenant_testkit::fs::{Alloc, DeleteStyle, FsBuilder, FsStamp, FsType};
use revenant_testkit::{
    ArwCompression, ArwOptions, AvcOptions, TiffRaw, VideoCodec, VideoLayout, arw, camera_video,
    jpeg, jpeg_burst, jpeg_smooth,
};
use support::{SimSource, VOLUME_AT, final_view, on_disk};

const KIB: u64 = 1 << 10;
const MIB: u64 = 1 << 20;
const DIR: &str = "DCIM/100MSDCF";
/// Shots numbered 1..=SHOTS (their files are encoded once).
const SHOTS: usize = 40;

// ---- The files a camera writes (encoded once: a debug build encodes a 2048×1536 JPEG in about 2 s) ----

/// `f(0), …, f(n - 1)`, each on its own thread.
fn par<T: Send>(n: usize, f: impl Fn(usize) -> T + Sync) -> Vec<T> {
    std::thread::scope(|s| {
        let f = &f;
        let handles: Vec<_> = (0..n).map(|i| s.spawn(move || f(i))).collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    })
}

/// Shot `n`'s ARW: 1536×1024 (lossless: 24 tiles of 256), a 640×480 preview of the camera's one scene shifted by
/// `n % 16` pixels (so shots 16 apart have the same preview, as in a burst). The uncompressed ones are zero-padded to
/// 64 KiB after their sensor data, as Sony's ILCE-7S pads its files (ruling Q54).
fn raw_of(c: ArwCompression, n: u32) -> &'static TiffRaw {
    static LOSSLESS: OnceLock<Vec<TiffRaw>> = OnceLock::new();
    static UNCOMPRESSED: OnceLock<Vec<TiffRaw>> = OnceLock::new();
    let cell = match c {
        ArwCompression::Lossless => &LOSSLESS,
        ArwCompression::Uncompressed => &UNCOMPRESSED,
        ArwCompression::Lossy => unreachable!("the cards use lossless and uncompressed RAWs"),
    };
    let all = cell.get_or_init(|| {
        par(SHOTS, |i| {
            arw(&ArwOptions {
                compression: c,
                width: 1536,
                height: 1024,
                seed: i as u64 + 1,
                pad_to: if c == ArwCompression::Uncompressed {
                    64 * 1024
                } else {
                    0
                },
                ..ArwOptions::default()
            })
        })
    });
    &all[n as usize - 1]
}

/// Shot `n`'s JPEG: a smooth photo (DRI 0) from 1280×960 to 1920×1440, so the shots' sizes differ as a camera's do
/// (the scene's detail changes them) and the new files do not fit the holes exactly.
fn jpeg_of(n: u32) -> &'static [u8] {
    static ALL: OnceLock<Vec<Vec<u8>>> = OnceLock::new();
    let all = ALL.get_or_init(|| {
        par(SHOTS, |i| {
            let w = 1280 + 64 * (i as u32 * 7 % 11);
            jpeg_smooth(w, w / 4 * 3, 1000 + i as u64)
        })
    });
    &all[n as usize - 1]
}

/// Clip `n`: 120 frames of 2–24 KiB, `moov` at the end; H.264 for odd `n`, H.265 for even.
fn clip_of(n: u32) -> &'static [u8] {
    static ALL: OnceLock<Vec<Vec<u8>>> = OnceLock::new();
    let all = ALL.get_or_init(|| {
        par(8, |i| {
            let codec = if i % 2 == 0 {
                VideoCodec::Avc
            } else {
                VideoCodec::Hevc
            };
            camera_video(&video(120, i as u64 + 1, 0), codec, VideoLayout::MoovAtEnd).bytes
        })
    });
    &all[n as usize - 1]
}

fn video(samples: usize, seed: u64, mdat_pad: usize) -> AvcOptions {
    AvcOptions {
        samples,
        seed,
        mov: false,
        mdat_pad,
        small_nals: false,
    }
}

/// The burst of the look-alike card: `jpeg_burst(2048, 1536, 4, 6)`.
fn burst() -> &'static [Vec<u8>] {
    static ALL: OnceLock<Vec<Vec<u8>>> = OnceLock::new();
    ALL.get_or_init(|| jpeg_burst(2048, 1536, 4, 6))
}

/// A noise photo of exactly `k` clusters of `cluster` bytes (k in 1..=3), seed `seed`.
fn small_photo(cluster: u64, k: u64, seed: u64) -> Vec<u8> {
    // About 1.9 bytes per pixel: aim at half a cluster below k clusters.
    let side = ((((2 * k - 1) * cluster / 2) as f64 / 1.9).sqrt() as u32) / 8 * 8;
    let j = jpeg(side, side, seed, 0);
    assert_eq!(
        (j.len() as u64).div_ceil(cluster),
        k,
        "{side}×{side}: {} bytes",
        j.len()
    );
    j
}

/// A long clip of about `len` bytes (its `mdat` padded to it exactly).
fn long_clip(len: u64, seed: u64) -> Vec<u8> {
    let samples = (len / (14 * KIB)).max(1) as usize;
    let mut v = camera_video(
        &video(samples, seed, 0),
        VideoCodec::Avc,
        VideoLayout::MoovAtEnd,
    );
    let mut s = samples;
    while v.bytes.len() as u64 > len {
        s = s * 9 / 10;
        v = camera_video(&video(s, seed, 0), VideoCodec::Avc, VideoLayout::MoovAtEnd);
    }
    let pad = (len - v.bytes.len() as u64) as usize;
    let v = camera_video(
        &video(s, seed, pad),
        VideoCodec::Avc,
        VideoLayout::MoovAtEnd,
    );
    assert_eq!(v.bytes.len() as u64, len);
    v.bytes
}

// ---- Writing them ----

/// What a camera writes, in order.
#[derive(Clone, Copy, Debug)]
enum Shot {
    /// Shot `n`: its ARW (`DSC0nnnn.ARW`), then its JPEG (`DSC0nnnn.JPG`).
    Pair(u32, ArwCompression),
    /// Clip `n` (`C000n.MP4`).
    Clip(u32),
    /// Photo `k` of the burst (`DSC1000k.JPG`).
    Burst(usize),
    /// Shot `n` taken as a JPEG only (`DSC0nnnn.JPG`).
    Photo(u32),
}

/// A file as written on the card.
struct Placed {
    path: String,
    kind: FileKind,
    bytes: Cow<'static, [u8]>,
    /// Clusters in file order.
    clusters: Vec<u32>,
    /// A RAW's preview (its own nested file, Q16).
    preview: Option<Range<usize>>,
    /// A lossless RAW's tiles (unpadded: the testkit pads only the tiles it is told to).
    tiles: Vec<Range<usize>>,
    /// How it was deleted, if it was.
    deleted: Option<DeleteStyle>,
    /// Deleted, and its entry's slots taken by a later one ([`FsBuilder::reuse_slots`]): no entry is left.
    overwritten: bool,
}

fn placed(path: String, kind: FileKind, bytes: Cow<'static, [u8]>, clusters: Vec<u32>) -> Placed {
    Placed {
        path,
        kind,
        bytes,
        clusters,
        preview: None,
        tiles: Vec::new(),
        deleted: None,
        overwritten: false,
    }
}

/// Writes `seq` in order through the builder's allocation policy (the RAW of a pair, then its JPEG): the files of one
/// shot follow each other, and each takes the free clusters it is given, holes first or wherever the pointer is.
fn shoot(b: &mut FsBuilder, seq: &[Shot]) -> Vec<Placed> {
    let mut out = Vec::new();
    for &shot in seq {
        match shot {
            Shot::Pair(n, c) => {
                let raw = raw_of(c, n);
                let path = format!("{DIR}/DSC0{n:04}.ARW");
                let clusters = b.add(&path, &raw.bytes);
                let mut p = placed(path, FileKind::Arw, Cow::Borrowed(&raw.bytes), clusters);
                p.preview = raw.preview.clone();
                p.tiles = raw.tiles.clone();
                out.push(p);
                let path = format!("{DIR}/DSC0{n:04}.JPG");
                let j = jpeg_of(n);
                let clusters = b.add(&path, j);
                out.push(placed(path, FileKind::Jpeg, Cow::Borrowed(j), clusters));
            }
            Shot::Clip(n) => {
                let path = format!("{DIR}/C{n:04}.MP4");
                let v = clip_of(n);
                let clusters = b.add(&path, v);
                out.push(placed(path, FileKind::Mp4, Cow::Borrowed(v), clusters));
            }
            Shot::Photo(n) => {
                let path = format!("{DIR}/DSC0{n:04}.JPG");
                let j = jpeg_of(n);
                let clusters = b.add(&path, j);
                out.push(placed(path, FileKind::Jpeg, Cow::Borrowed(j), clusters));
            }
            Shot::Burst(k) => {
                let path = format!("{DIR}/DSC1{k:04}.JPG");
                let j = &burst()[k];
                let clusters = b.add(&path, j);
                out.push(placed(path, FileKind::Jpeg, Cow::Borrowed(j), clusters));
            }
        }
    }
    out
}

fn pairs(ns: Range<u32>, c: ArwCompression) -> Vec<Shot> {
    ns.map(|n| Shot::Pair(n, c)).collect()
}

/// A card being used: the builder and every file written on it.
struct Cam {
    b: FsBuilder,
    fs: FsType,
    len: u64,
    cluster: u64,
    alloc: Alloc,
    files: Vec<Placed>,
    /// The first cluster after the file system's own (bitmap, up-case table, root) and the DCIM folders.
    first: u32,
    /// Minutes since the first file.
    clock: u32,
    /// Each file stamped a minute after the one before; `false`: every file keeps the builder's one default stamp (a
    /// camera whose clock was never set).
    stamps: bool,
    /// [`Cam::reuse_slots`] is on.
    reuse: bool,
    /// The files' entries in directory order (indexes in `files`).
    entries: Vec<usize>,
}

impl Cam {
    fn new(fs: FsType, len: u64, cluster: u64, alloc: Alloc) -> Cam {
        let mut b = FsBuilder::new(fs, len, cluster as u32);
        b.alloc(alloc);
        b.mkdir(DIR);
        let first = *b.clusters(DIR).last().unwrap() + 1;
        Cam {
            b,
            fs,
            len,
            cluster,
            alloc,
            files: Vec::new(),
            first,
            clock: 0,
            stamps: true,
            reuse: false,
            entries: Vec::new(),
        }
    }

    fn shoot(&mut self, seq: &[Shot]) {
        let new = shoot(&mut self.b, seq);
        for f in new {
            self.stamp(&f.path);
            self.entered(f);
        }
    }

    /// A lone file, written like a shot's.
    fn add(&mut self, path: String, kind: FileKind, bytes: Vec<u8>) {
        let clusters = self.b.add(&path, &bytes);
        self.stamp(&path);
        self.entered(placed(path, kind, Cow::Owned(bytes), clusters));
    }

    /// Keeps `f`, its entry where the builder put it: with reused slots, over the first deleted entry of as many
    /// slots (the exFAT set of a name: 2 + one per 15 characters), which is gone then.
    fn entered(&mut self, f: Placed) {
        let i = self.files.len();
        let slots = |p: &Placed| 2 + p.path.rsplit('/').next().unwrap().len().div_ceil(15);
        let taken = self.reuse.then(|| {
            self.entries.iter().position(|&j| {
                let g = &self.files[j];
                g.deleted.is_some() && !g.overwritten && slots(g) == slots(&f)
            })
        });
        match taken.flatten() {
            Some(pos) => {
                let j = self.entries[pos];
                self.files[j].overwritten = true;
                self.entries[pos] = i;
            }
            None => self.entries.push(i),
        }
        self.files.push(f);
    }

    /// A lone file at `clusters` (where an allocator put it), written like a shot's.
    fn add_at(
        &mut self,
        path: String,
        kind: FileKind,
        bytes: Cow<'static, [u8]>,
        clusters: Vec<u32>,
    ) {
        self.b.add_at(&path, &bytes, &clusters);
        self.stamp(&path);
        self.entered(placed(path, kind, bytes, clusters));
    }

    /// The review's C-1 scenario (ruling Q53), with JPEG-only shots 28..=38: D and A shot, D then A deleted; B, shorter
    /// than A in as many clusters, written over A's clusters (where a next-free pointer that wrapped would put it) takes
    /// D's slots, the first deleted; B deleted; C shot takes B's slots. A's entry stands alone over B's bytes and the
    /// zero-filled rest of its last cluster.
    fn stale_entry_over_a_shorter_photo(&mut self) {
        let need = |n: u32| self.need(jpeg_of(n).len());
        let (a, b) = (28..39)
            .flat_map(|a| (28..39).map(move |b| (a, b)))
            .find(|&(a, b)| a != b && need(a) == need(b) && jpeg_of(b).len() < jpeg_of(a).len())
            .expect("two photos of as many clusters, one shorter");
        let mut rest = (28..39).filter(|&n| n != a && n != b);
        let (d, c) = (rest.next().unwrap(), rest.next().unwrap());
        self.shoot(&[Shot::Photo(d), Shot::Photo(a)]);
        let path = |n: u32| format!("{DIR}/DSC0{n:04}.JPG");
        self.delete(&path(d), DeleteStyle::Camera);
        self.delete(&path(a), DeleteStyle::Camera);
        let clusters = self.files[self.file(&path(a))].clusters.clone();
        self.add_at(path(b), FileKind::Jpeg, Cow::Borrowed(jpeg_of(b)), clusters);
        self.delete(&path(b), DeleteStyle::Camera);
        self.shoot(&[Shot::Photo(c)]);
        let gone = |n: u32| self.files[self.file(&path(n))].overwritten;
        assert!(gone(b) && !gone(a), "B's slots taken again, A's left");
    }

    /// Free clusters now (no live file holds them), ascending, but those of files a computer deleted (kept whole for the
    /// reassembly).
    fn free_clusters(&self) -> Vec<u32> {
        let live: HashSet<u32> = self
            .files
            .iter()
            .filter(|f| matches!(f.deleted, None | Some(DeleteStyle::ZeroChain)))
            .flat_map(|f| f.clusters.iter().copied())
            .collect();
        (self.first..self.b.cluster_count() + 2)
            .filter(|c| !live.contains(c))
            .collect()
    }

    /// Q50 N-1 with reused slots: X, shot `x`'s RAW alone in 2 runs of free clusters (FAT-chained), deleted in camera;
    /// then an edited copy Y (a longer name: an entry set of more slots, so it takes no deleted one) written over X's
    /// first run and on, its own chain through X's clusters: X's entry stays over a chain that is no longer its own.
    fn chain_rewritten_under_an_entry(&mut self, x: u32, y: u32) {
        let free = self.free_clusters();
        let raw = &raw_of(ArwCompression::Lossless, x).bytes;
        let nx = self.need(raw.len()) as usize;
        let ny = self.need(jpeg_of(y).len()) as usize;
        let h = nx / 2;
        assert!(free.len() > nx + ny + 4, "room for X and Y");
        // X: the first h free clusters, then 2 free ones skipped, then the rest.
        let xc: Vec<u32> = free[..h]
            .iter()
            .chain(&free[h + 2..nx + 2])
            .copied()
            .collect();
        let px = format!("{DIR}/DSC0{x:04}.ARW");
        self.add_at(px.clone(), FileKind::Arw, Cow::Borrowed(raw), xc);
        let k = self.file(&px);
        self.files[k].preview = raw_of(ArwCompression::Lossless, x).preview.clone();
        self.files[k].tiles = raw_of(ArwCompression::Lossless, x).tiles.clone();
        self.delete(&px, DeleteStyle::Camera);
        // Y: X's first run, the 2 skipped clusters, then free clusters after X.
        let yc: Vec<u32> = free[..h + 2]
            .iter()
            .chain(&free[nx + 2..])
            .take(ny)
            .copied()
            .collect();
        let py = format!("{DIR}/DSC0{y:04}-EDITED.JPG");
        self.add_at(py, FileKind::Jpeg, Cow::Borrowed(jpeg_of(y)), yc);
        assert!(!self.files[self.file(&px)].overwritten, "X's entry stays");
    }

    /// New entries take the slots of deleted ones, as cameras do.
    fn reuse_slots(&mut self) {
        assert_eq!(self.fs, FsType::Exfat, "slots counted as exFAT sets");
        self.b.reuse_slots(true);
        self.reuse = true;
    }

    /// Each file a minute after the one before (an older entry over the same clusters loses to the newer one).
    fn stamp(&mut self, path: &str) {
        if !self.stamps {
            return;
        }
        self.clock += 1;
        let k = self.clock;
        let t = FsStamp {
            year: 2026,
            month: 10,
            day: 1 + (k / 1440) as u8,
            hour: (k / 60 % 24) as u8,
            min: (k % 60) as u8,
            sec: 0,
            utc_offset_min: None,
        };
        self.b.stamp(path, t, t);
    }

    fn file(&self, path: &str) -> usize {
        self.files
            .iter()
            .rposition(|f| f.path == path)
            .unwrap_or_else(|| panic!("no {path}"))
    }

    fn delete(&mut self, path: &str, style: DeleteStyle) {
        self.b.delete(path, style);
        let k = self.file(path);
        self.files[k].deleted = Some(style);
    }

    /// The new shots `ns` (pairs) and clips 2 and 3, in the order they were taken, that have a file in pieces: each as
    /// the paths of its files, at most `n` of them.
    fn in_pieces(&self, ns: Range<u32>, n: usize) -> Vec<Vec<String>> {
        let mut shots: Vec<Vec<String>> = ns
            .map(|k| {
                vec![
                    format!("{DIR}/DSC0{k:04}.ARW"),
                    format!("{DIR}/DSC0{k:04}.JPG"),
                ]
            })
            .chain([2, 3].map(|k| vec![format!("{DIR}/C{k:04}.MP4")]))
            .collect();
        shots.sort_by_key(|paths| self.file(&paths[0]));
        shots
            .into_iter()
            .filter(|paths| {
                paths.iter().any(|p| {
                    let f = &self.files[self.file(p)];
                    f.deleted.is_none() && f.clusters.windows(2).any(|w| w[1] != w[0] + 1)
                })
            })
            .take(n)
            .collect()
    }

    /// Deletes both files of shot `n`.
    fn delete_shot(&mut self, n: u32, style: DeleteStyle) {
        for ext in ["ARW", "JPG"] {
            self.delete(&format!("{DIR}/DSC0{n:04}.{ext}"), style);
        }
    }

    fn need(&self, len: usize) -> u64 {
        (len as u64).div_ceil(self.cluster)
    }

    /// Clusters `seq` will take.
    fn clusters_of(&self, seq: &[Shot]) -> u64 {
        seq.iter()
            .map(|&s| match s {
                Shot::Pair(n, c) => {
                    self.need(raw_of(c, n).bytes.len()) + self.need(jpeg_of(n).len())
                }
                Shot::Clip(n) => self.need(clip_of(n).len()),
                Shot::Burst(k) => self.need(burst()[k].len()),
                Shot::Photo(n) => self.need(jpeg_of(n).len()),
            })
            .sum()
    }

    /// Free clusters now.
    fn free(&self) -> u64 {
        let live: u64 = self
            .files
            .iter()
            .filter(|f| f.deleted.is_none())
            .map(|f| f.clusters.len() as u64)
            .sum();
        u64::from(self.b.cluster_count()) - u64::from(self.first - 2) - live
    }

    /// A long clip that leaves `leave` clusters free (the card nearly full).
    fn fill(&mut self, leave: u64, n: u32) {
        let clusters = self.free() - leave;
        let bytes = long_clip(clusters * self.cluster - 1000, 100 + u64::from(n));
        self.add(format!("{DIR}/C{n:04}.MP4"), FileKind::Mp4, bytes);
    }

    /// The volume: every cluster no file ever took holds old data (seeded noise).
    fn volume(&self) -> Vec<u8> {
        assert_eq!(self.b.clusters(DIR).len(), 1, "the folder fits one cluster");
        let mut v = self.b.build();
        old_data(&self.b, &mut v, self.first, &self.files, self.cluster);
        v
    }
}

/// Seeded noise (old data) over every cluster from `first` on that none of `files` took.
fn old_data(b: &FsBuilder, v: &mut [u8], first: u32, files: &[Placed], cluster: u64) {
    let used: HashSet<u32> = files
        .iter()
        .flat_map(|f| f.clusters.iter().copied())
        .collect();
    let cl = cluster as usize;
    let end = b.cluster_count() + 2;
    let noise = support::cards::noise(u64::from(end - first) * cluster, 0x01D_CA2D);
    for c in first..end {
        if used.contains(&c) {
            continue;
        }
        let at = b.cluster_offset(c) as usize;
        let from = (c - first) as usize * cl;
        v[at..at + cl].copy_from_slice(&noise[from..from + cl]);
    }
}

/// Where a file's entry stands when the card is scanned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Entry {
    Live,
    Deleted(DeleteStyle),
    /// Quick-formatted away: no name, no size, no chain.
    Gone,
}

/// A file of a finished card.
struct File {
    path: String,
    kind: FileKind,
    bytes: Cow<'static, [u8]>,
    clusters: Vec<u32>,
    /// Absolute pieces on the disk, consecutive clusters joined.
    pieces: Vec<Fragment>,
    preview: Option<Range<usize>>,
    tiles: Vec<Range<usize>>,
    entry: Entry,
    /// Every byte of it is still on the card, in its clusters.
    whole: bool,
    /// Its logical offsets where a piece ends and the next begins.
    seams: Vec<u64>,
}

/// A finished card: the disk and what was ever written on it.
struct Card {
    name: String,
    fs: FsType,
    cluster: u64,
    disk: Vec<u8>,
    files: Vec<File>,
}

/// The finished card of `files` on `volume` (built by `b`), each file with whether a quick format took its entry.
fn finish(
    name: String,
    fs: FsType,
    b: &FsBuilder,
    cluster: u64,
    volume: Vec<u8>,
    files: Vec<(Placed, bool)>,
) -> Card {
    let disk = on_disk(&volume);
    drop(volume);
    let files = files
        .into_iter()
        .map(|(p, gone)| {
            let mut pieces: Vec<Fragment> = Vec::new();
            let mut seams = Vec::new();
            let mut left = p.bytes.len() as u64;
            for (i, &c) in p.clusters.iter().enumerate() {
                let len = cluster.min(left);
                left -= len;
                let start = VOLUME_AT + b.cluster_offset(c);
                match pieces.last_mut() {
                    Some(q) if q.start + q.len == start => q.len += len,
                    Some(_) => {
                        seams.push(i as u64 * cluster);
                        pieces.push(Fragment { start, len });
                    }
                    None => pieces.push(Fragment { start, len }),
                }
            }
            let whole = pieces
                .iter()
                .scan(0usize, |at, q| {
                    let from = *at;
                    *at += q.len as usize;
                    Some((from, q))
                })
                .all(|(from, q)| {
                    disk[q.start as usize..(q.start + q.len) as usize]
                        == p.bytes[from..from + q.len as usize]
                });
            let entry = match (gone || p.overwritten, p.deleted) {
                (true, _) => Entry::Gone,
                (false, None) => Entry::Live,
                (false, Some(s)) => Entry::Deleted(s),
            };
            File {
                path: p.path,
                kind: p.kind,
                bytes: p.bytes,
                clusters: p.clusters,
                pieces,
                preview: p.preview,
                tiles: p.tiles,
                entry,
                whole,
                seams,
            }
        })
        .collect();
    Card {
        name,
        fs,
        cluster,
        disk,
        files,
    }
}

impl Cam {
    /// The card as it is now.
    fn card(self, name: String) -> Card {
        let v = self.volume();
        let files = self.files.into_iter().map(|p| (p, false)).collect();
        finish(name, self.fs, &self.b, self.cluster, v, files)
    }

    /// Quick-formats the card in the camera, then shoots `seq` on it: the new files go from the start of the heap (the
    /// formatted file system's allocation), over the old ones.
    fn format_and_shoot(self, seq: &[Shot], name: String) -> Card {
        let mut v = self.b.build_quick_formatted();
        old_data(&self.b, &mut v, self.first, &self.files, self.cluster);
        let mut fresh = Cam::new(self.fs, self.len, self.cluster, self.alloc);
        assert_eq!(fresh.first, self.first);
        fresh.shoot(seq);
        let new = fresh.b.build();
        // The new file system (boot region, FAT, bitmap, up-case table, root, DCIM folders), then the new files.
        let meta = fresh.b.cluster_offset(fresh.first) as usize;
        v[..meta].copy_from_slice(&new[..meta]);
        for f in &fresh.files {
            for &c in &f.clusters {
                let at = fresh.b.cluster_offset(c) as usize;
                let cl = self.cluster as usize;
                v[at..at + cl].copy_from_slice(&new[at..at + cl]);
            }
        }
        let files = self
            .files
            .into_iter()
            .map(|p| (p, true))
            .chain(fresh.files.into_iter().map(|p| (p, false)))
            .collect();
        finish(name, self.fs, &fresh.b, self.cluster, v, files)
    }
}

// ---- The oracles ----

/// The health a file whole on the card must come out with (Q51).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Want {
    Intact,
    /// Its exact bytes, `Partial`: placed by content where no proof exists.
    Partial,
    /// A JPEG in two pieces placed by content: the 1.3 two-piece join (`Jpeg::measure`, kept by Q37) may make it
    /// `Intact` when its continuation is the only one that passes in its window; else the reassembly, `Partial`. Its
    /// exact bytes either way.
    Joined,
}

/// Q51: `Intact` where a proof exists (one piece; a live chain; a surviving exFAT chain no other entry claims, D14; a
/// video, every seam proven by its sample table ([`sample_proven`]); a lossless RAW whose every seam lies on a tile boundary), `Partial`
/// where the content places pieces it cannot prove (a JPEG in pieces, Q37; a RAW with a seam inside a tile, its preview
/// or its sensor data, Q44/Q45).
fn want(card: &Card, f: &File) -> Want {
    if f.pieces.len() == 1 || placed_by_the_file_system(card, f) {
        return Want::Intact;
    }
    match f.kind {
        FileKind::Mp4
            if f.seams
                .iter()
                .all(|&at| sample_proven(&f.bytes, at, card.cluster)) =>
        {
            Want::Intact
        }
        FileKind::Arw if !f.tiles.is_empty() => {
            let starts: HashSet<u64> = f.tiles.iter().skip(1).map(|t| t.start as u64).collect();
            if f.seams.iter().all(|s| starts.contains(s)) {
                Want::Intact
            } else {
                Want::Partial
            }
        }
        FileKind::Jpeg if f.pieces.len() == 2 => Want::Joined,
        _ => Want::Partial,
    }
}

/// Whether a seam at logical `at` of the camera clip `bytes` (`ftyp`, `mdat`, `moov`) is one its sample table proves
/// (Task 5, Q39): a NAL unit starts within one cluster before it and within one cluster from it, so the bytes checked on
/// both sides lie within one unit of the seam. A seam past the `mdat` lies in the `moov`, which the search places by
/// itself (its orphan or own `moov`).
fn sample_proven(bytes: &[u8], at: u64, cluster: u64) -> bool {
    let be = |p: usize| u32::from_be_bytes(bytes[p..p + 4].try_into().unwrap()) as usize;
    let mut pos = 0;
    while &bytes[pos + 4..pos + 8] != b"mdat" {
        pos += be(pos);
    }
    let end = pos + be(pos);
    if at >= end as u64 {
        return true;
    }
    let mut nals = Vec::new();
    let mut q = pos + 8;
    while q + 4 <= end {
        nals.push(q as u64);
        q += 4 + be(q);
    }
    let before = nals.iter().any(|&n| n < at && at - n <= cluster);
    let after = nals.iter().any(|&n| n >= at && n - at < cluster);
    before && after
}

/// Whether `got` is file `o`: its exact bytes, or for a RAW its bytes but for the camera's zero padding at the end, which
/// no IFD references, so a carving stops before it (as `real_samples` allows).
fn same_file(got: &[u8], o: &File) -> bool {
    let want = &o.bytes[..];
    got == want
        || (o.kind == FileKind::Arw
            && got.len() < want.len()
            && want[..got.len()] == *got
            && want[got.len()..].iter().all(|&b| b == 0))
}

/// Whether the file system itself places `f`'s pieces: a live chain, or a surviving exFAT chain (deleted in camera)
/// that no other entry's clusters touch (D14, Q49). Otherwise only the content can: the entry is gone, its chain was
/// zeroed (a computer, or any FAT delete) or another entry claims some of its clusters.
fn placed_by_the_file_system(card: &Card, f: &File) -> bool {
    match f.entry {
        Entry::Live => true,
        Entry::Deleted(DeleteStyle::Camera) if card.fs == FsType::Exfat => {
            let mine: HashSet<u32> = f.clusters.iter().copied().collect();
            !card.files.iter().any(|g| {
                !std::ptr::eq(g, f)
                    && g.entry != Entry::Gone
                    && g.clusters.iter().any(|c| mine.contains(c))
            })
        }
        _ => false,
    }
}

/// The absolute ranges of logical `r` of a file in `pieces`.
fn absolute(pieces: &[Fragment], r: &Range<usize>) -> Vec<Fragment> {
    let mut out: Vec<Fragment> = Vec::new();
    let mut at = 0u64;
    for p in pieces {
        let (s, e) = ((r.start as u64).max(at), (r.end as u64).min(at + p.len));
        if s < e {
            out.push(Fragment {
                start: p.start + (s - at),
                len: e - s,
            });
        }
        at += p.len;
    }
    out
}

/// What one card's scan gave.
struct Outcome {
    card: String,
    /// Files whole on the card: `Intact`, `Partial`, not recovered (no finding at their start, or `Damaged`).
    counts: [usize; 3],
    findings: usize,
    /// `Intact` findings that are not exactly one file (Q51: never relaxed).
    glued: Vec<String>,
    /// Files whole on the card that did not come out with their exact bytes and the health their proof allows.
    recovered: Vec<String>,
    /// Two-piece JPEGs placed by content that the 1.3 join made `Intact` (exact bytes).
    joined: Vec<String>,
    secs: f64,
}

fn describe(f: &Finding) -> String {
    format!(
        "#{} {:?} {:?} at {} len {} pieces {:?} origin {:?}",
        f.id,
        f.kind,
        f.health,
        f.start,
        f.len,
        f.fragments,
        f.origin
            .as_ref()
            .map(|o| (o.path(), o.state, o.placement, o.size))
    )
}

fn scanned(mut card: Card) -> Outcome {
    let t = Instant::now();
    let src = SimSource::new(std::mem::take(&mut card.disk));
    let mut events = Vec::new();
    scan(
        &src,
        &all_formats(),
        &ScanOptions::default(),
        &CancelToken::default(),
        &mut |e: Event| events.push(e),
    )
    .unwrap();
    let view = final_view(&events);
    let dir = tempfile::tempdir().unwrap();
    let bytes_of = |f: &Finding| {
        let d = dir.path().join(f.id.to_string());
        std::fs::create_dir(&d).unwrap();
        let path = extract(&src, f, &d).unwrap();
        let got = std::fs::read(&path).unwrap();
        std::fs::remove_dir_all(&d).unwrap();
        got
    };
    let mut glued = Vec::new();
    for f in view.values().filter(|f| f.health == Health::Intact) {
        let got = bytes_of(f);
        let matching: Vec<&File> = card.files.iter().filter(|o| same_file(&got, o)).collect();
        let equal = matching.len();
        // The name too (review M-1): a finding named by an entry that still stands is that entry's file.
        if let ([o], Some(origin)) = (matching.as_slice(), &f.origin)
            && o.entry != Entry::Gone
            && origin.path() != o.path
        {
            glued.push(format!(
                "{} (the bytes of {}, under another name)",
                describe(f),
                o.path
            ));
        }
        // A RAW's own preview, found at its place in the RAW (Q16): the same file.
        let nested = card.files.iter().any(|o| {
            o.preview.as_ref().is_some_and(|p| {
                o.whole && o.bytes[p.clone()] == got[..] && f.ranges() == absolute(&o.pieces, p)
            })
        });
        if equal != 1 && !nested {
            glued.push(format!("{} (equals {equal} files)", describe(f)));
        }
    }
    let mut counts = [0; 3];
    let mut recovered = Vec::new();
    // The reassembly must matter on every card (review I-1, M-5): at least 3 files whole on it, in pieces, that the
    // file system does not place ([`placed_by_the_file_system`]).
    let placed_by_content = card
        .files
        .iter()
        .filter(|o| o.whole && o.pieces.len() > 1 && !placed_by_the_file_system(&card, o))
        .count();
    if placed_by_content < 3 {
        recovered.push(format!(
            "only {placed_by_content} files whole in pieces the file system does not place (3 wanted)"
        ));
    }
    let mut joined = Vec::new();
    for o in card.files.iter().filter(|o| o.whole) {
        let want = want(&card, o);
        let found = view
            .values()
            .find(|f| f.start == o.pieces[0].start && f.kind == o.kind);
        let Some(f) = found else {
            counts[2] += 1;
            recovered.push(format!("{} {:?} ({want:?}): not found", o.path, o.entry));
            continue;
        };
        counts[match f.health {
            Health::Intact => 0,
            Health::Partial => 1,
            Health::Damaged => 2,
        }] += 1;
        let exact = same_file(&bytes_of(f), o);
        let health_ok = match want {
            Want::Intact => f.health == Health::Intact,
            Want::Partial => f.health == Health::Partial,
            Want::Joined => f.health != Health::Damaged,
        };
        if want == Want::Joined && f.health == Health::Intact {
            joined.push(o.path.clone());
        }
        if !exact || !health_ok {
            recovered.push(format!(
                "{} {:?} {} pieces, seams {:?} (want {want:?}): exact {exact}, {}",
                o.path,
                o.entry,
                o.pieces.len(),
                o.seams,
                describe(f)
            ));
        }
    }
    let out = Outcome {
        card: card.name,
        counts,
        findings: view.len(),
        glued,
        recovered,
        joined,
        secs: t.elapsed().as_secs_f64(),
    };
    eprintln!(
        "{}: {} findings; whole files Intact {} / Partial {} / not recovered {}; {:.1} s; joined Intact {:?}",
        out.card, out.findings, out.counts[0], out.counts[1], out.counts[2], out.secs, out.joined
    );
    out
}

/// Cards built and scanned at once in this test binary (each holds about 0.5 GiB while it is).
const AT_ONCE: usize = 4;

/// Runs `f` once fewer than [`AT_ONCE`] others run.
fn limited<T>(f: impl FnOnce() -> T) -> T {
    static SLOTS: (Mutex<usize>, Condvar) = (Mutex::new(0), Condvar::new());
    let (lock, cv) = &SLOTS;
    {
        let mut busy = lock.lock().unwrap_or_else(|e| e.into_inner());
        while *busy >= AT_ONCE {
            busy = cv.wait(busy).unwrap_or_else(|e| e.into_inner());
        }
        *busy += 1;
    }
    /// Gives the slot back, even when `f` panics.
    struct Slot;
    impl Drop for Slot {
        fn drop(&mut self) {
            let (lock, cv) = &SLOTS;
            *lock.lock().unwrap_or_else(|e| e.into_inner()) -= 1;
            cv.notify_one();
        }
    }
    let _slot = Slot;
    f()
}

/// Every card of a scenario, built and scanned in parallel (at most [`AT_ONCE`] in the binary).
fn outcomes(cards: Vec<Box<dyn Fn() -> Card + Send + Sync>>) -> Vec<Outcome> {
    par(cards.len(), |i| limited(|| scanned(cards[i]())))
}

fn assert_recovered(all: &[Outcome]) {
    let bad: Vec<String> = all
        .iter()
        .flat_map(|o| o.recovered.iter().map(move |r| format!("{}: {r}", o.card)))
        .collect();
    assert!(bad.is_empty(), "{} files:\n{}", bad.len(), bad.join("\n"));
}

// ---- The cards ----

const CARD_LEN: u64 = 128 * MIB;
const CLUSTERS: [u64; 2] = [32 * KIB, 128 * KIB];
const ALLOCS: [Alloc; 2] = [Alloc::FirstFit, Alloc::NextFree];

fn label(cluster: u64, alloc: Alloc) -> String {
    format!("{} KiB {alloc:?}", cluster / KIB)
}

/// A camera's life on a card: `first` (shots, maybe clips among them), a long clip that leaves just the room
/// the next files need, `deleted` deleted in camera, then `more` shots (numbered on) with 2 clips among them (after the first and the
/// second third). With the lowest free cluster first the new files fill the holes; with the next-free pointer they go
/// to the end of the card first, then wrap into the holes. The JPEGs' sizes differ from shot to shot, so the new files
/// straddle holes.
fn camera_life(
    mut cam: Cam,
    raw: ArwCompression,
    first: &[Shot],
    deleted: &[Shot],
    more: u32,
) -> Cam {
    cam.shoot(first);
    let from = 1 + first
        .iter()
        .filter_map(|s| match s {
            Shot::Pair(n, _) | Shot::Photo(n) => Some(*n),
            _ => None,
        })
        .max()
        .unwrap();
    let third = more / 3;
    let mut next = pairs(from..from + third, raw);
    next.push(Shot::Clip(2));
    next.extend(pairs(from + third..from + 2 * third, raw));
    next.push(Shot::Clip(3));
    next.extend(pairs(from + 2 * third..from + more, raw));
    let leave = (cam.clusters_of(&next) + 2).saturating_sub(cam.clusters_of(deleted));
    cam.fill(leave, 1);
    for &shot in deleted {
        match shot {
            Shot::Pair(n, _) => cam.delete_shot(n, DeleteStyle::Camera),
            Shot::Clip(n) => cam.delete(&format!("{DIR}/C{n:04}.MP4"), DeleteStyle::Camera),
            Shot::Photo(n) => cam.delete(&format!("{DIR}/DSC0{n:04}.JPG"), DeleteStyle::Camera),
            Shot::Burst(_) => unreachable!(),
        }
    }
    cam.shoot(&next);
    cam
}

/// Shots 1..=18, the odd ones taken as JPEG only, with clips 4 to 8 after shots 2, 6, 9, 12 and 15, and what a user culled
/// of them in camera: the JPEG-only shots and the clips (holes smaller than a RAW between holes larger than a shot, so
/// most of the next shots fall into pieces).
fn culled() -> (Vec<Shot>, Vec<Shot>) {
    culled_with(ArwCompression::Lossless)
}

/// [`culled`] with RAWs of `raw`.
fn culled_with(raw: ArwCompression) -> (Vec<Shot>, Vec<Shot>) {
    let mut first = Vec::new();
    let mut deleted = Vec::new();
    for n in 1..19 {
        let shot = if n % 2 == 1 {
            Shot::Photo(n)
        } else {
            Shot::Pair(n, raw)
        };
        first.push(shot);
        if n % 2 == 1 {
            deleted.push(shot);
        }
        let clip = match n {
            2 => Some(4),
            6 => Some(5),
            9 => Some(6),
            12 => Some(7),
            15 => Some(8),
            _ => None,
        };
        if let Some(c) = clip {
            first.push(Shot::Clip(c));
            deleted.push(Shot::Clip(c));
        }
    }
    (first, deleted)
}

/// A 128 MiB exFAT card.
fn exfat(cluster: u64, alloc: Alloc) -> Cam {
    Cam::new(FsType::Exfat, CARD_LEN, cluster, alloc)
}

/// [`camera_life`] with 8 more shots, then 2 of them deleted in camera as well (exFAT keeps their chains, D14).
fn delete_in_camera_cards(raw: ArwCompression, variants: &[(u64, Alloc, Camera)]) -> Vec<Outcome> {
    let mut cards: Vec<Box<dyn Fn() -> Card + Send + Sync>> = Vec::new();
    for &(cluster, alloc, how) in variants {
        cards.push(Box::new(move || {
            let mut cam = exfat(cluster, alloc);
            if how.reuse_slots {
                cam.reuse_slots();
            }
            cam.stamps = how.stamps;
            let (first, deleted) = culled_with(raw);
            let mut cam = camera_life(cam, raw, &first, &deleted, 8);
            // The first new shots in pieces deleted in camera too.
            let gone = cam.in_pieces(19..27, 3);
            assert!(gone.len() >= 3, "3 new shots in pieces");
            for p in gone.iter().flatten() {
                cam.delete(p, DeleteStyle::Camera);
            }
            if !how.stamps {
                // Shot 19 went first into the first hole, its RAW from the very cluster the deleted photo 1 did:
                // deleted too, the two entries start at one cluster with one stamp (Q52's tie).
                for p in [format!("{DIR}/DSC00019.ARW"), format!("{DIR}/DSC00019.JPG")] {
                    if cam.files[cam.file(&p)].deleted.is_none() {
                        cam.delete(&p, DeleteStyle::Camera);
                    }
                }
            }
            if how.reuse_slots {
                // One more shot (the first deleted entry's slots, the first free clusters), a deleted entry left over a
                // chain another file rewrote (Q50 N-1), then the review's lone stale entry (Q53).
                cam.shoot(&pairs(27..28, raw));
                cam.stale_entry_over_a_shorter_photo();
                // Later, on a computer that clears the chain, the next shots in pieces: only the content places them.
                for p in cam.in_pieces(19..28, 3).iter().flatten() {
                    cam.delete(p, DeleteStyle::ZeroChain);
                }
                cam.chain_rewritten_under_an_entry(39, 40);
            }
            cam.card(format!(
                "delete in camera, {raw:?}, {}{}{}",
                label(cluster, alloc),
                if how.reuse_slots {
                    ", slots reused"
                } else {
                    ""
                },
                if how.stamps { "" } else { ", clock unset" },
            ))
        }));
    }
    outcomes(cards)
}

/// How the camera writes its entries.
#[derive(Clone, Copy)]
struct Camera {
    /// A new entry takes a deleted entry's slots ([`FsBuilder::reuse_slots`]).
    reuse_slots: bool,
    /// Files stamped a minute apart; `false`: a clock never set, every stamp equal.
    stamps: bool,
}

const PLAIN: Camera = Camera {
    reuse_slots: false,
    stamps: true,
};

/// Every cluster size with both allocations, plainly.
fn every_variant() -> Vec<(u64, Alloc, Camera)> {
    CLUSTERS
        .iter()
        .flat_map(|&c| ALLOCS.iter().map(move |&a| (c, a, PLAIN)))
        .collect()
}

fn delete_in_camera() -> &'static [Outcome] {
    static ALL: OnceLock<Vec<Outcome>> = OnceLock::new();
    ALL.get_or_init(|| {
        let mut v = every_variant();
        // A camera that reuses the deleted entries' slots (so old chains can prove a placement, D14, or be rewritten,
        // Q50 N-1), one per cluster size, and one whose clock was never set (every stamp equal, Q52).
        let reuse = Camera {
            reuse_slots: true,
            stamps: true,
        };
        v.push((32 * KIB, Alloc::FirstFit, reuse));
        v.push((128 * KIB, Alloc::NextFree, reuse));
        v.push((
            32 * KIB,
            Alloc::FirstFit,
            Camera {
                reuse_slots: false,
                stamps: false,
            },
        ));
        delete_in_camera_cards(ArwCompression::Lossless, &v)
    })
}

fn delete_in_camera_uncompressed_cards() -> &'static [Outcome] {
    static ALL: OnceLock<Vec<Outcome>> = OnceLock::new();
    ALL.get_or_init(|| delete_in_camera_cards(ArwCompression::Uncompressed, &every_variant()))
}

/// [`camera_life`] with the [`culled`] holes (the new shots in pieces among them, up to 17 shots on the card),
/// quick-formatted in camera, then 6 more shots over the start: they cover the first holes only, so old files in pieces
/// stay whole further on, with no entry left (review I-1).
fn format_in_camera() -> &'static [Outcome] {
    static ALL: OnceLock<Vec<Outcome>> = OnceLock::new();
    ALL.get_or_init(|| {
        let mut cards: Vec<Box<dyn Fn() -> Card + Send + Sync>> = Vec::new();
        for cluster in CLUSTERS {
            for alloc in ALLOCS {
                cards.push(Box::new(move || {
                    let raw = ArwCompression::Lossless;
                    let (first, deleted) = culled();
                    let cam = camera_life(exfat(cluster, alloc), raw, &first, &deleted, 8);
                    cam.format_and_shoot(
                        &pairs(27..33, ArwCompression::Lossless),
                        format!("format in camera, {}", label(cluster, alloc)),
                    )
                }));
            }
        }
        outcomes(cards)
    })
}

/// [`camera_life`] with the [`culled`] holes, then 5 shots whose files lie in pieces deleted on a computer: exFAT clearing the chain (ZeroChain),
/// and FAT32 (260 MiB, 4 KiB clusters, the smallest FAT32 on a cluster grid) as Windows does (WindowsFat32).
fn delete_on_a_computer_cards() -> &'static [Outcome] {
    static ALL: OnceLock<Vec<Outcome>> = OnceLock::new();
    ALL.get_or_init(|| {
        let mut cards: Vec<Box<dyn Fn() -> Card + Send + Sync>> = Vec::new();
        let computer = |mut cam: Cam, style: DeleteStyle, name: String| {
            // The first 5 new shots (a pair or a clip) with a file in pieces, in the order they were taken.
            let fragmented = cam.in_pieces(19..27, 5);
            assert_eq!(fragmented.len(), 5, "{name}: 5 shots in pieces");
            for p in fragmented.iter().flatten() {
                cam.delete(p, style);
            }
            cam.card(name)
        };
        for cluster in CLUSTERS {
            for alloc in ALLOCS {
                cards.push(Box::new(move || {
                    let (first, deleted) = culled();
                    let cam = camera_life(
                        exfat(cluster, alloc),
                        ArwCompression::Lossless,
                        &first,
                        &deleted,
                        8,
                    );
                    computer(
                        cam,
                        DeleteStyle::ZeroChain,
                        format!("delete on a computer, exFAT {}", label(cluster, alloc)),
                    )
                }));
            }
        }
        for alloc in ALLOCS {
            cards.push(Box::new(move || {
                let fat32 = Cam::new(FsType::Fat32, 260 * MIB, 4 * KIB, alloc);
                let (first, deleted) = culled();
                let cam = camera_life(fat32, ArwCompression::Lossless, &first, &deleted, 8);
                computer(
                    cam,
                    DeleteStyle::WindowsFat32,
                    format!("delete on a computer, FAT32 4 KiB {alloc:?}"),
                )
            }));
        }
        outcomes(cards)
    })
}

/// Small photos of 1, 2, 3, 1, 2, 3, … clusters, every other one deleted in camera (holes of 1–3 clusters between live
/// photos), a long clip after them filling the card, then the burst written next-free: the pointer wraps into the holes,
/// each photo of the burst in pieces of 1–3 clusters, the next one where it stops. Then the burst is deleted (in camera,
/// or on a computer clearing the chain), and on one card the photos between the holes too.
fn burst_cards() -> &'static [Outcome] {
    static ALL: OnceLock<Vec<Outcome>> = OnceLock::new();
    ALL.get_or_init(|| {
        let mut cards: Vec<Box<dyn Fn() -> Card + Send + Sync>> = Vec::new();
        // The last: the photos left between the holes deleted on the computer too, so the file system's guess runs on
        // into the deleted photos' clusters and the burst neighbours' pieces; the search chooses among look-alikes.
        let variants = [
            (32 * KIB, DeleteStyle::ZeroChain, true),
            (128 * KIB, DeleteStyle::Camera, false),
            (128 * KIB, DeleteStyle::ZeroChain, false),
            (128 * KIB, DeleteStyle::ZeroChain, true),
        ];
        for (cluster, style, all) in variants {
            {
                cards.push(Box::new(move || {
                    let mut cam = Cam::new(FsType::Exfat, CARD_LEN, cluster, Alloc::NextFree);
                    let shots: Vec<Shot> = (0..6).map(Shot::Burst).collect();
                    let want = cam.clusters_of(&shots);
                    // Holes of 1, 2, 3 clusters: 2 clusters on average, every other small photo.
                    let count = (want as usize + 8).next_multiple_of(3);
                    let small: Vec<Vec<u8>> = par(2 * count, |i| {
                        small_photo(cluster, 1 + (i / 2 % 3) as u64, 7000 + i as u64)
                    });
                    for (i, j) in small.into_iter().enumerate() {
                        cam.add(format!("{DIR}/DSC2{i:04}.JPG"), FileKind::Jpeg, j);
                    }
                    cam.fill(0, 9);
                    for i in (0..2 * count).step_by(2) {
                        cam.delete(&format!("{DIR}/DSC2{i:04}.JPG"), DeleteStyle::Camera);
                    }
                    cam.shoot(&shots);
                    for k in 0..6 {
                        cam.delete(&format!("{DIR}/DSC1{k:04}.JPG"), style);
                    }
                    if all {
                        for i in (1..2 * count).step_by(2) {
                            cam.delete(&format!("{DIR}/DSC2{i:04}.JPG"), style);
                        }
                    }
                    cam.card(format!(
                        "burst of look-alikes, {} KiB, deleted {style:?}{}",
                        cluster / KIB,
                        if all { ", the photos between too" } else { "" }
                    ))
                }));
            }
        }
        outcomes(cards)
    })
}

// ---- The tests ----

#[test]
fn delete_in_camera_then_shoot_again() {
    assert_recovered(delete_in_camera());
}

#[test]
fn delete_in_camera_uncompressed_raw() {
    assert_recovered(delete_in_camera_uncompressed_cards());
}

#[test]
fn format_in_camera_then_shoot_again() {
    assert_recovered(format_in_camera());
}

#[test]
fn delete_on_a_computer() {
    assert_recovered(delete_on_a_computer_cards());
}

#[test]
fn burst_of_look_alikes_interleaved() {
    let all = burst_cards();
    assert_recovered(all);
}

#[test]
fn no_intact_finding_is_glued() {
    let all: Vec<&Outcome> = [
        delete_in_camera(),
        delete_in_camera_uncompressed_cards(),
        format_in_camera(),
        delete_on_a_computer_cards(),
        burst_cards(),
    ]
    .into_iter()
    .flatten()
    .collect();
    let bad: Vec<String> = all
        .iter()
        .flat_map(|o| o.glued.iter().map(move |g| format!("{}: {g}", o.card)))
        .collect();
    assert!(bad.is_empty(), "{} glued:\n{}", bad.len(), bad.join("\n"));
}
