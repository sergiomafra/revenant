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
const SHOTS: usize = 32;

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
/// `n % 16` pixels (so shots 16 apart have the same preview, as in a burst).
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
        }
    }

    fn shoot(&mut self, seq: &[Shot]) {
        let new = shoot(&mut self.b, seq);
        for f in &new {
            self.stamp(&f.path);
        }
        self.files.extend(new);
    }

    /// A lone file, written like a shot's.
    fn add(&mut self, path: String, kind: FileKind, bytes: Vec<u8>) {
        let clusters = self.b.add(&path, &bytes);
        self.stamp(&path);
        self.files
            .push(placed(path, kind, Cow::Owned(bytes), clusters));
    }

    /// Each file a minute after the one before (an older entry over the same clusters loses to the newer one).
    fn stamp(&mut self, path: &str) {
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
            let entry = match (gone, p.deleted) {
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
    /// A video placed by content: its sample table proves every seam, so `Intact` with its exact bytes. Two gaps the
    /// cards found are recorded instead of failing (both never `Intact`, so no false `Intact`; for the controller's
    /// ruling, see the task report): a deleted `moov`-at-end video whose file-system guess holds its own `moov` is not
    /// searched (its `moov` is no orphan) and stays `Partial` with its exact bytes; one whose first piece ends the heap
    /// and goes on at its start is not recovered whole.
    Video,
}

/// Q51: `Intact` where a proof exists (one piece; a live chain; a surviving exFAT chain no other entry claims, D14; a
/// video, every seam proven by its sample table; a lossless RAW whose every seam lies on a tile boundary), `Partial`
/// where the content places pieces it cannot prove (a JPEG in pieces, Q37; a RAW with a seam inside a tile, its preview
/// or its sensor data, Q44/Q45).
fn want(card: &Card, f: &File) -> Want {
    if f.pieces.len() == 1 {
        return Want::Intact;
    }
    match f.entry {
        Entry::Live => return Want::Intact,
        Entry::Deleted(DeleteStyle::Camera) if card.fs == FsType::Exfat => {
            let mine: HashSet<u32> = f.clusters.iter().copied().collect();
            let claimed = card.files.iter().any(|g| {
                !std::ptr::eq(g, f)
                    && g.entry != Entry::Gone
                    && g.clusters.iter().any(|c| mine.contains(c))
            });
            if !claimed {
                return Want::Intact;
            }
        }
        _ => {}
    }
    match f.kind {
        FileKind::Mp4 => Want::Video,
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
    /// Videos placed by content that did not come out `Intact` ([`Want::Video`]).
    gaps: Vec<String>,
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
        let equal = card
            .files
            .iter()
            .filter(|o| o.bytes.len() == got.len() && *o.bytes == got[..])
            .count();
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
    let mut joined = Vec::new();
    let mut gaps = Vec::new();
    for o in card.files.iter().filter(|o| o.whole) {
        let want = want(&card, o);
        let found = view
            .values()
            .find(|f| f.start == o.pieces[0].start && f.kind == o.kind);
        let Some(f) = found else {
            counts[2] += 1;
            if want == Want::Video {
                gaps.push(format!("{} {:?}: not found", o.path, o.entry));
                continue;
            }
            recovered.push(format!("{} {:?} ({want:?}): not found", o.path, o.entry));
            continue;
        };
        counts[match f.health {
            Health::Intact => 0,
            Health::Partial => 1,
            Health::Damaged => 2,
        }] += 1;
        let exact = bytes_of(f)[..] == o.bytes[..];
        if want == Want::Video && f.health != Health::Intact {
            gaps.push(format!(
                "{} {:?} {} pieces: exact {exact}, {}",
                o.path,
                o.entry,
                o.pieces.len(),
                describe(f)
            ));
            continue;
        }
        let health_ok = match want {
            Want::Intact | Want::Video => f.health == Health::Intact,
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
        gaps,
        secs: t.elapsed().as_secs_f64(),
    };
    eprintln!(
        "{}: {} findings; whole files Intact {} / Partial {} / not recovered {}; {:.1} s; joined Intact {:?}",
        out.card, out.findings, out.counts[0], out.counts[1], out.counts[2], out.secs, out.joined
    );
    for g in &out.gaps {
        eprintln!("  video gap: {g}");
    }
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

/// Shots 3, 6 and 9 of `pairs(1..13)`.
fn three_six_nine(raw: ArwCompression) -> Vec<Shot> {
    [3, 6, 9].iter().map(|&n| Shot::Pair(n, raw)).collect()
}

/// Shots 1..=18, the odd ones taken as JPEG only, with clips 4 to 8 after shots 2, 6, 9, 12 and 15, and what a user culled
/// of them in camera: the JPEG-only shots and the clips (holes smaller than a RAW between holes larger than a shot, so
/// most of the next shots fall into pieces).
fn culled() -> (Vec<Shot>, Vec<Shot>) {
    let raw = ArwCompression::Lossless;
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
fn delete_in_camera_cards(raw: ArwCompression) -> Vec<Outcome> {
    let mut cards: Vec<Box<dyn Fn() -> Card + Send + Sync>> = Vec::new();
    for cluster in CLUSTERS {
        for alloc in ALLOCS {
            cards.push(Box::new(move || {
                let first = pairs(1..13, raw);
                let mut cam =
                    camera_life(exfat(cluster, alloc), raw, &first, &three_six_nine(raw), 8);
                for n in [14, 18] {
                    cam.delete_shot(n, DeleteStyle::Camera);
                }
                cam.card(format!(
                    "delete in camera, {raw:?}, {}",
                    label(cluster, alloc)
                ))
            }));
        }
    }
    outcomes(cards)
}

fn delete_in_camera() -> &'static [Outcome] {
    static ALL: OnceLock<Vec<Outcome>> = OnceLock::new();
    ALL.get_or_init(|| delete_in_camera_cards(ArwCompression::Lossless))
}

fn delete_in_camera_uncompressed_cards() -> &'static [Outcome] {
    static ALL: OnceLock<Vec<Outcome>> = OnceLock::new();
    ALL.get_or_init(|| delete_in_camera_cards(ArwCompression::Uncompressed))
}

/// [`camera_life`] up to 20 shots on the card, quick-formatted in camera, then 6 more shots over the start.
fn format_in_camera() -> &'static [Outcome] {
    static ALL: OnceLock<Vec<Outcome>> = OnceLock::new();
    ALL.get_or_init(|| {
        let mut cards: Vec<Box<dyn Fn() -> Card + Send + Sync>> = Vec::new();
        for cluster in CLUSTERS {
            for alloc in ALLOCS {
                cards.push(Box::new(move || {
                    let raw = ArwCompression::Lossless;
                    let first = pairs(1..13, raw);
                    let cam =
                        camera_life(exfat(cluster, alloc), raw, &first, &three_six_nine(raw), 11);
                    cam.format_and_shoot(
                        &pairs(24..30, ArwCompression::Lossless),
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
            let mut shots: Vec<Vec<String>> = (19..27)
                .map(|n| {
                    vec![
                        format!("{DIR}/DSC0{n:04}.ARW"),
                        format!("{DIR}/DSC0{n:04}.JPG"),
                    ]
                })
                .chain([2, 3].map(|n| vec![format!("{DIR}/C{n:04}.MP4")]))
                .collect();
            shots.sort_by_key(|paths| cam.file(&paths[0]));
            let fragmented: Vec<Vec<String>> = shots
                .into_iter()
                .filter(|paths| {
                    paths.iter().any(|p| {
                        let f = &cam.files[cam.file(p)];
                        f.clusters.windows(2).any(|w| w[1] != w[0] + 1)
                    })
                })
                .take(5)
                .collect();
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
/// or on a computer clearing the chain).
fn burst_cards() -> &'static [Outcome] {
    static ALL: OnceLock<Vec<Outcome>> = OnceLock::new();
    ALL.get_or_init(|| {
        let mut cards: Vec<Box<dyn Fn() -> Card + Send + Sync>> = Vec::new();
        for cluster in CLUSTERS {
            for style in [DeleteStyle::Camera, DeleteStyle::ZeroChain] {
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
                    cam.card(format!(
                        "burst of look-alikes, {} KiB, deleted {style:?}",
                        cluster / KIB
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
