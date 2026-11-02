//! The reassembly step of the scan (part 1.6): fragmented JPEGs put back together after the scan and the pairing, as
//! `Updated` events; never over a live file or another `Intact` file, never re-reading a known bad sector, bounded on a
//! hostile card, cancelled promptly, the same for any `threads` and `cache_bytes`.

mod support;

use std::sync::Mutex;

use revenant_core::formats::{FileKind, Health, all_formats};
use revenant_core::source::{Source, VolumeId};
use revenant_core::{
    CancelToken, EngineError, Event, Finding, Fragment, FsState, ScanOptions, ScanSummary, extract,
    scan,
};
use revenant_testkit::fs::{DeleteStyle, FsBuilder};
use revenant_testkit::{
    ArwCompression, ArwOptions, AvcOptions, TestExif, TiffRawOptions, TiffVariant, VideoCodec,
    VideoLayout, arw, avc_video, camera_video, jpeg, jpeg_smooth, tiff_raw,
};
use support::cards::{ThreePiece, live_in_three_runs, three_piece};
use support::{SimSource, VOLUME_AT, exfat_card, final_view, on_disk};

const KIB: u64 = 1 << 10;
const MIB: u64 = 1 << 20;
const CLUSTER: u64 = 32 * KIB;

fn opts(threads: usize, cache_bytes: usize) -> ScanOptions {
    ScanOptions {
        threads,
        cache_bytes,
        ..ScanOptions::default()
    }
}

fn run(src: &dyn Source, opts: &ScanOptions) -> (Result<ScanSummary, EngineError>, Vec<Event>) {
    let mut events = Vec::new();
    let r = scan(
        src,
        &all_formats(),
        opts,
        &CancelToken::default(),
        &mut |e| events.push(e),
    );
    (r, events)
}

fn events_of(src: &dyn Source, opts: &ScanOptions) -> Vec<Event> {
    let (r, events) = run(src, opts);
    r.unwrap();
    events
}

/// The final finding starting at `start`.
fn at(view: &std::collections::BTreeMap<u64, Finding>, start: u64) -> &Finding {
    view.values()
        .find(|f| f.start == start)
        .unwrap_or_else(|| panic!("a finding at {start}: {view:#?}"))
}

/// The bytes of `finding`, extracted through the public API.
fn extracted(src: &dyn Source, finding: &Finding) -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    let path = extract(src, finding, dir.path()).unwrap();
    std::fs::read(path).unwrap()
}

fn updated(events: &[Event]) -> Vec<&Finding> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Updated(f) => Some(f),
            _ => None,
        })
        .collect()
}

#[test]
fn three_piece_jpeg_on_a_quick_formatted_card_is_reassembled() {
    let ThreePiece {
        bytes,
        r,
        r_pieces,
        q,
        q_pieces,
        ..
    } = three_piece();
    let src = SimSource::new(bytes);
    let events = events_of(&src, &ScanOptions::default());
    let view = final_view(&events);
    // R is put back together in its 3 pieces, by content alone: at most `Partial` (ruling Q37), with R's exact bytes.
    let rf = at(&view, r_pieces[0].start);
    assert_eq!(rf.kind, FileKind::Jpeg);
    assert_eq!(rf.fragments, r_pieces, "{rf:#?}");
    assert_eq!(rf.len, r.len() as u64);
    assert_eq!(rf.health, Health::Partial);
    assert!(
        rf.origin.is_none(),
        "no name is left after the quick format"
    );
    assert!(
        updated(&events).iter().any(|u| u.id == rf.id),
        "R comes out as an Updated"
    );
    assert_eq!(extracted(&src, rf), r);
    // Q, in 2 pieces, is found whole by the 1.3 two-piece join (`Partial`: one cluster per piece). The reassembly finds
    // the same 2 pieces, at most `Partial` by content alone (Q37): nothing better, so no `Updated`.
    let qf = at(&view, q_pieces[0].start);
    assert_eq!(qf.fragments, q_pieces, "{qf:#?}");
    assert_eq!(qf.health, Health::Partial);
    assert!(updated(&events).iter().all(|u| u.id != qf.id));
    assert_eq!(extracted(&src, qf), q);
}

#[test]
fn reassembly_events_identical_across_threads_and_caches() {
    let src = SimSource::new(three_piece().bytes);
    let base = events_of(&src, &opts(1, 256 << 20));
    assert!(!updated(&base).is_empty());
    for threads in [1, 2, 8, 16] {
        for cache in [256 << 20, 1 << 20] {
            assert_eq!(
                events_of(&src, &opts(threads, cache)),
                base,
                "threads {threads}, cache {cache}"
            );
        }
    }
}

/// Absolute pieces of `clusters` (file order) holding `size` bytes of a volume on [`on_disk`], consecutive ones joined.
fn pieces_of(b: &FsBuilder, clusters: &[u32], size: u64) -> Vec<Fragment> {
    let mut out: Vec<Fragment> = Vec::new();
    let mut left = size;
    for &c in clusters {
        let len = CLUSTER.min(left);
        left -= len;
        let start = VOLUME_AT + b.cluster_offset(c);
        match out.last_mut() {
            Some(p) if p.start + p.len == start => p.len += len,
            _ => out.push(Fragment { start, len }),
        }
    }
    out
}

fn overlap(a: &Fragment, b: &Fragment) -> bool {
    a.start < b.start + b.len && b.start < a.start + a.len
}

/// Clusters `next..next + n`, moving `next` past them.
fn take(next: &mut u32, n: u32) -> Vec<u32> {
    let c: Vec<u32> = (*next..*next + n).collect();
    *next += n;
    c
}

fn clusters_for(len: usize) -> u32 {
    (len as u64).div_ceil(CLUSTER) as u32
}

#[test]
fn pieces_never_overlap_live_files_or_another_intact() {
    // Live photos L1..L4 around 3 deleted photos D0..D2 in 3, 2 and 2 FAT-chained runs (exFAT, Camera delete: the
    // chains stay in the FAT, the bitmap frees them), and a photo C left by an earlier use of the card (no entry) in 3
    // runs: after C1 comes L1, then COPY.BIN, a live file holding a copy of C2's bytes, and C's true second run lies
    // further on. The copy is the first candidate after C1; being live, it is never a piece. Each of C's runs ends
    // where a live photo starts (a run followed at once by another smooth photo's run decodes on into it: the walk
    // finds no break there, and the search then only gets a `Partial` path).
    let mut b = exfat_card();
    let lives: Vec<Vec<u8>> = (1..=4).map(|k| jpeg(512, 384, 20 + k, 0)).collect();
    let dels = [
        jpeg_smooth(2048, 1536, 31),
        jpeg_smooth(1600, 1200, 32),
        jpeg_smooth(1600, 1200, 33),
    ];
    let c = jpeg_smooth(2048, 1536, 34);
    let first = b.add("L0.JPG", &jpeg(256, 256, 19, 0));
    let mut next = *first.last().unwrap() + 1;
    let n: Vec<u32> = dels.iter().map(|d| clusters_for(d.len())).collect();
    let nc = clusters_for(c.len());
    let (d1, d2, d3, c1) = (n[0] / 3, n[1] / 2, n[2] / 2, nc / 3);
    let mut dc: [Vec<u32>; 3] = Default::default();
    let mut lc: Vec<Vec<u32>> = Vec::new();
    let mut cc = take(&mut next, c1);
    lc.push(take(&mut next, clusters_for(lives[0].len())));
    let copy = take(&mut next, c1);
    dc[0].extend(take(&mut next, d1));
    lc.push(take(&mut next, clusters_for(lives[1].len())));
    dc[1].extend(take(&mut next, d2));
    dc[0].extend(take(&mut next, d1));
    cc.extend(take(&mut next, c1));
    lc.push(take(&mut next, clusters_for(lives[2].len())));
    dc[2].extend(take(&mut next, d3));
    dc[1].extend(take(&mut next, n[1] - d2));
    cc.extend(take(&mut next, nc - 2 * c1));
    lc.push(take(&mut next, clusters_for(lives[3].len())));
    dc[0].extend(take(&mut next, n[0] - 2 * d1));
    dc[2].extend(take(&mut next, n[2] - d3));
    for (k, d) in dels.iter().enumerate() {
        b.add_at(&format!("D{k}.JPG"), d, &dc[k]);
    }
    for (k, l) in lives.iter().enumerate() {
        b.add_at(&format!("L{}.JPG", k + 1), l, &lc[k]);
    }
    let c2 = (c1 as usize * CLUSTER as usize)..(2 * c1 as usize * CLUSTER as usize);
    b.add_at("COPY.BIN", &c[c2], &copy);
    let mut live_pieces: Vec<Fragment> = lc
        .iter()
        .zip(&lives)
        .flat_map(|(c, l)| pieces_of(&b, c, l.len() as u64))
        .collect();
    live_pieces.extend(pieces_of(&b, &first, first.len() as u64 * CLUSTER));
    live_pieces.extend(pieces_of(&b, &copy, copy.len() as u64 * CLUSTER));
    for k in 0..3 {
        b.delete(&format!("D{k}.JPG"), DeleteStyle::Camera);
    }
    let mut volume = b.build();
    // C's runs, in clusters no entry holds.
    let c_pieces = pieces_of(&b, &cc, c.len() as u64);
    let mut from = 0usize;
    for p in &c_pieces {
        let at = (p.start - VOLUME_AT) as usize;
        volume[at..at + p.len as usize].copy_from_slice(&c[from..from + p.len as usize]);
        from += p.len as usize;
    }
    let src = SimSource::new(on_disk(&volume));
    let events = events_of(&src, &ScanOptions::default());
    let view = final_view(&events);
    let deleted = view
        .values()
        .filter(|f| {
            f.origin
                .as_ref()
                .is_some_and(|o| o.state == FsState::Deleted)
        })
        .count();
    assert_eq!(deleted, 3, "{view:#?}");
    // C is put back together, around the live copy.
    let cf = at(&view, c_pieces[0].start);
    assert!(updated(&events).iter().any(|u| u.id == cf.id), "{cf:#?}");
    assert_eq!(cf.fragments, c_pieces);
    for f in view.values() {
        if f.origin.as_ref().is_some_and(|o| o.state == FsState::Live) {
            continue;
        }
        for p in f.ranges() {
            for l in &live_pieces {
                assert!(!overlap(&p, l), "{f:#?} over the live piece {l:?}");
            }
        }
    }
    let intact: Vec<&Finding> = view
        .values()
        .filter(|f| f.health == Health::Intact)
        .collect();
    for (i, a) in intact.iter().enumerate() {
        for b in &intact[i + 1..] {
            // A RAW and its own nested preview are the same file (ruling Q16); none on this card.
            for p in a.ranges() {
                for q in b.ranges() {
                    assert!(!overlap(&p, &q), "{a:#?} and {b:#?} share bytes");
                }
            }
        }
    }
}

#[test]
fn reassembly_never_rereads_known_bad_sectors() {
    let t = three_piece();
    let r2 = t.r_pieces[1];
    let bad = (r2.start + r2.len / 2) / 512 * 512;
    let src = SimSource::new(t.bytes).bad(bad, 512);
    for (threads, cache) in [(1, 1 << 20), (8, 256 << 20)] {
        src.clear_reads();
        let events = events_of(&src, &opts(threads, cache));
        let view = final_view(&events);
        assert_eq!(at(&view, t.r_pieces[0].start).health, Health::Partial);
        let reads = src.reads();
        let touches = |&(at, len): &(u64, usize)| at < bad + 512 && bad < at + len as u64;
        let first = reads
            .iter()
            .position(|&(at, len)| at == bad && len == 512)
            .expect("the scan resolves the bad sector on its own");
        let again: Vec<_> = reads[first + 1..].iter().filter(|r| touches(r)).collect();
        assert!(
            again.is_empty(),
            "threads {threads}: read again after its resolution: {again:?}"
        );
    }
}

/// The hostile image: 256 MiB, no volume; the first 64 KiB of a `jpeg_smooth(2048, 1536, 3)` at 0, then 4096 sectors
/// each holding the photo's next 510 bytes and `FF D9`; zeros after. Returns the image and where the photo starts.
fn hostile() -> Vec<u8> {
    let photo = jpeg_smooth(2048, 1536, 3);
    let first = 64 * KIB as usize;
    assert!(photo.len() > first + 512);
    let mut img = vec![0u8; 256 * MIB as usize];
    img[..first].copy_from_slice(&photo[..first]);
    let mut sector = [0u8; 512];
    sector[..510].copy_from_slice(&photo[first..first + 510]);
    sector[510..].copy_from_slice(&[0xFF, 0xD9]);
    for k in 0..4096 {
        let at = first + k * 512;
        img[at..at + 512].copy_from_slice(&sector);
    }
    img
}

#[test]
fn hostile_card_stays_within_budget() {
    let src = SimSource::new(hostile());
    let events = events_of(&src, &ScanOptions::default());
    let view = final_view(&events);
    let photo = at(&view, 0);
    assert_eq!(photo.kind, FileKind::Jpeg);
    assert_ne!(photo.health, Health::Intact, "{photo:#?}");
}

/// Trips `cancel` at the first read after one that reached the source's end: the first read of the step after the
/// scan (no volume and no video: the reassembly). Counts the reads after it.
struct CancelAfterEnd<'a> {
    inner: &'a SimSource,
    cancel: CancelToken,
    /// (the end was reached, reads since the trip)
    seen: Mutex<(bool, u64)>,
}

impl Source for CancelAfterEnd<'_> {
    fn len(&self) -> u64 {
        self.inner.len()
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, EngineError> {
        {
            let mut s = self.seen.lock().unwrap();
            if self.cancel.is_cancelled() {
                s.1 += 1;
            } else if s.0 {
                self.cancel.cancel();
                s.1 += 1;
            }
            if offset + buf.len() as u64 >= self.inner.len() {
                s.0 = true;
            }
        }
        self.inner.read_at(offset, buf)
    }
    fn volume(&self) -> Option<VolumeId> {
        None
    }
}

#[test]
fn cancel_during_reassembly_is_prompt() {
    let inner = SimSource::new(hostile());
    let cancel = CancelToken::default();
    let src = CancelAfterEnd {
        inner: &inner,
        cancel: cancel.clone(),
        seen: Mutex::new((false, 0)),
    };
    let mut events = Vec::new();
    let r = scan(&src, &all_formats(), &opts(1, 1 << 20), &cancel, &mut |e| {
        events.push(e)
    });
    assert_eq!(r.map(|_| ()).map_err(|e| e.code()), Err("cancelled"));
    let (ended, after) = *src.seen.lock().unwrap();
    assert!(
        ended && cancel.is_cancelled(),
        "the reassembly read the source"
    );
    assert!(after <= 2, "{after} reads after the cancel");
    assert!(
        events.iter().all(|e| !matches!(e, Event::Updated(_))),
        "nothing updated once cancelled"
    );
}

#[test]
fn live_files_are_never_candidates() {
    // A live photo in 3 runs (FAT-chained), live photos between them, an unreadable sector in its second run: found
    // `Partial` with its chain, and nothing updated (its chain says where it is; ruling D2).
    let (card, pieces) = live_in_three_runs();
    let events = events_of(&card.source, &ScanOptions::default());
    let found = events
        .iter()
        .find_map(|e| match e {
            Event::Found(f) if f.start == pieces[0].start => Some(f),
            _ => None,
        })
        .expect("the live photo");
    assert_eq!(found.health, Health::Partial);
    assert_eq!(found.fragments, pieces);
    assert!(updated(&events).is_empty(), "{:#?}", updated(&events));
}

// Videos in three or more pieces (Task 5): their index places every sample, so each seam is checked where it must lie.

/// Clusters in file order for runs starting at `firsts` of `counts` clusters each.
fn runs(firsts: &[u32], counts: &[u32]) -> Vec<u32> {
    firsts
        .iter()
        .zip(counts)
        .flat_map(|(&f, &n)| f..f + n)
        .collect()
}

/// Absolute pieces of `clusters` (file order, `cluster` bytes each) holding `size` bytes, consecutive ones joined.
fn pieces_in(b: &FsBuilder, cluster: u64, clusters: &[u32], size: u64) -> Vec<Fragment> {
    let mut out: Vec<Fragment> = Vec::new();
    let mut left = size;
    for &c in clusters {
        let len = cluster.min(left);
        left -= len;
        let start = VOLUME_AT + b.cluster_offset(c);
        match out.last_mut() {
            Some(p) if p.start + p.len == start => p.len += len,
            _ => out.push(Fragment { start, len }),
        }
    }
    out
}

/// An exFAT card (64 MiB, clusters of `cluster` bytes) holding `files` (name, bytes, clusters in file order), each
/// deleted by a computer that clears the chain (`ZeroChain`: the entry keeps the name and size, the FAT and the bitmap
/// forget the clusters). With a camera's delete the exFAT chain survives and places the file by itself (part 1.4; its
/// proof is D14, Task 9): here only the content can. Every other cluster from 64 on holds seeded noise (old data).
/// Returns the disk image and each file's absolute pieces.
fn video_image(cluster: u32, files: &[(&str, &[u8], Vec<u32>)]) -> (Vec<u8>, Vec<Vec<Fragment>>) {
    video_image_as(cluster, files, false)
}

/// [`video_image`], after a quick format when `quick` (no entry is left: no name, no size).
fn video_image_as(
    cluster: u32,
    files: &[(&str, &[u8], Vec<u32>)],
    quick: bool,
) -> (Vec<u8>, Vec<Vec<Fragment>>) {
    let mut b = FsBuilder::new(revenant_testkit::fs::FsType::Exfat, 64 * MIB, cluster);
    for (name, bytes, clusters) in files {
        b.add_at(&format!("DCIM/{name}"), bytes, clusters);
    }
    let pieces: Vec<Vec<Fragment>> = files
        .iter()
        .map(|(_, bytes, clusters)| pieces_in(&b, cluster as u64, clusters, bytes.len() as u64))
        .collect();
    for (name, ..) in files {
        b.delete(&format!("DCIM/{name}"), DeleteStyle::ZeroChain);
    }
    let mut volume = if quick {
        b.build_quick_formatted()
    } else {
        b.build()
    };
    let used: std::collections::HashSet<u32> =
        files.iter().flat_map(|f| f.2.iter().copied()).collect();
    let noise = support::cards::noise(volume.len() as u64, 0xC1A5);
    for c in 64..b.cluster_count() + 2 {
        if used.contains(&c) {
            continue;
        }
        let at = b.cluster_offset(c) as usize;
        let end = (at + cluster as usize).min(volume.len());
        volume[at..end].copy_from_slice(&noise[at..end]);
    }
    (on_disk(&volume), pieces)
}

/// [`video_image`] as a source.
fn video_card(cluster: u32, files: &[(&str, &[u8], Vec<u32>)]) -> (SimSource, Vec<Vec<Fragment>>) {
    let (image, pieces) = video_image(cluster, files);
    (SimSource::new(image), pieces)
}

fn small_video(samples: usize, seed: u64, small_nals: bool) -> AvcOptions {
    AvcOptions {
        samples,
        seed,
        mov: false,
        mdat_pad: 0,
        small_nals,
    }
}

/// The video findings of the final view, by start.
fn videos(view: &std::collections::BTreeMap<u64, Finding>) -> Vec<&Finding> {
    view.values()
        .filter(|f| matches!(f.kind, FileKind::Mp4 | FileKind::Mov))
        .collect()
}

#[test]
fn h264_video_in_five_pieces_with_moov_at_end_is_intact() {
    let v = avc_video(&small_video(300, 1, true));
    let n = clusters_for(v.bytes.len());
    assert_eq!(n, 5, "one cluster per piece");
    let clusters = runs(&[300, 900, 500, 1500, 1100], &[1; 5]);
    let (src, pieces) = video_card(CLUSTER as u32, &[("C0001.MP4", &v.bytes, clusters)]);
    let events = events_of(&src, &ScanOptions::default());
    let view = final_view(&events);
    let f = at(&view, pieces[0][0].start);
    assert_eq!(f.fragments, pieces[0], "{f:#?}");
    assert_eq!(
        (f.kind, f.health, f.len),
        (FileKind::Mp4, Health::Intact, v.bytes.len() as u64)
    );
    assert!(updated(&events).iter().any(|u| u.id == f.id));
    assert_eq!(extracted(&src, f), v.bytes);
}

#[test]
fn h265_video_in_four_pieces_is_intact() {
    let v = camera_video(
        &small_video(300, 2, true),
        VideoCodec::Hevc,
        VideoLayout::MoovAtEnd,
    );
    let n = clusters_for(v.bytes.len());
    assert!(n >= 5, "{n}");
    let clusters = runs(&[700, 200, 1300, 450], &[2, 1, 1, n - 4]);
    let (src, pieces) = video_card(CLUSTER as u32, &[("C0002.MP4", &v.bytes, clusters)]);
    let events = events_of(&src, &ScanOptions::default());
    let view = final_view(&events);
    let f = at(&view, pieces[0][0].start);
    assert_eq!(f.fragments, pieces[0], "{f:#?}");
    assert_eq!(f.health, Health::Intact, "{f:#?}");
    assert_eq!(extracted(&src, f), v.bytes);
}

#[test]
fn tiny_middle_piece_is_found() {
    // 256 samples of 2–24 KiB: the one-cluster middle piece holds fewer than 1/32 of them (C7 of part 1.3).
    let v = avc_video(&small_video(256, 3, false));
    let n = clusters_for(v.bytes.len());
    let first = n / 2;
    let clusters = runs(&[200, 1200, 600], &[first, 1, n - first - 1]);
    let moov = &v.bytes[v.moov.start + 8..v.moov.end];
    let index = revenant_testkit_samples(moov);
    let (lo, hi) = (first as u64 * CLUSTER, (first as u64 + 1) * CLUSTER);
    let inside = index.iter().filter(|&&(o, s)| o < hi && o + s > lo).count();
    assert!(inside < 256 / 32, "{inside} samples in the middle piece");
    let (src, pieces) = video_card(CLUSTER as u32, &[("C0003.MP4", &v.bytes, clusters)]);
    assert_eq!(pieces[0].len(), 3);
    let events = events_of(&src, &ScanOptions::default());
    let view = final_view(&events);
    let f = at(&view, pieces[0][0].start);
    assert_eq!(f.fragments, pieces[0], "{f:#?}");
    assert_eq!(f.health, Health::Intact, "{f:#?}");
    assert_eq!(extracted(&src, f), v.bytes);
}

/// The (offset, size) of every sample of a testkit video's `moov` payload (one track: `stsz`, 4 samples per chunk,
/// `stco`), read the simple way the testkit writes them.
fn revenant_testkit_samples(moov: &[u8]) -> Vec<(u64, u64)> {
    let find = |kind: &[u8; 4]| moov.windows(4).position(|w| w == kind).unwrap() + 4;
    let be = |b: &[u8]| u32::from_be_bytes(b[..4].try_into().unwrap()) as u64;
    let stsz = find(b"stsz");
    let count = be(&moov[stsz + 8..]) as usize;
    let sizes: Vec<u64> = (0..count).map(|i| be(&moov[stsz + 12 + 4 * i..])).collect();
    let stco = find(b"stco");
    let chunks = be(&moov[stco + 4..]) as usize;
    let mut out = Vec::new();
    for c in 0..chunks {
        let mut off = be(&moov[stco + 8 + 4 * c..]);
        for &s in sizes.iter().skip(4 * c).take(4) {
            out.push((off, s));
            off += s;
        }
    }
    out
}

#[test]
fn two_clips_of_one_camera_interleaved_are_not_swapped() {
    // Two clips of the same camera (same options, seeds 1 and 2), their mdats padded to the same length so that each
    // moov fits either video by its arithmetic (the strongest case), written in alternating one-cluster runs as a
    // camera writing both at once with a rotating pointer would, both deleted.
    let bare = |seed| avc_video(&small_video(300, seed, true)).moov.start;
    let p = bare(1).max(bare(2)) + 512;
    let clip = |seed| {
        avc_video(&AvcOptions {
            mdat_pad: p - bare(seed),
            ..small_video(300, seed, true)
        })
    };
    let (a, b) = (clip(1), clip(2));
    assert_eq!(a.moov.start, b.moov.start);
    let (na, nb) = (clusters_for(a.bytes.len()), clusters_for(b.bytes.len()));
    let ca: Vec<u32> = (0..na).map(|k| 400 + 2 * k).collect();
    let cb: Vec<u32> = (0..nb).map(|k| 401 + 2 * k).collect();
    let (src, pieces) = video_card(
        CLUSTER as u32,
        &[("C0001.MP4", &a.bytes, ca), ("C0002.MP4", &b.bytes, cb)],
    );
    let events = events_of(&src, &ScanOptions::default());
    let view = final_view(&events);
    for (clip, own, other) in [(&a, &pieces[0], &pieces[1]), (&b, &pieces[1], &pieces[0])] {
        let f = at(&view, own[0].start);
        if f.health == Health::Intact {
            assert_eq!(extracted(&src, f), clip.bytes, "{f:#?}");
            for p in f.ranges() {
                for q in other {
                    assert!(!overlap(&p, q), "{f:#?} holds the other clip's run {q:?}");
                }
            }
        }
    }
    // Both come back whole: each clip's index refutes the other's runs at their first sample.
    for (clip, own) in [(&a, &pieces[0]), (&b, &pieces[1])] {
        let f = at(&view, own[0].start);
        assert_eq!((f.health, &f.fragments), (Health::Intact, own), "{f:#?}");
        assert_eq!(extracted(&src, f), clip.bytes);
    }
    assert_eq!(videos(&view).len(), 2);
}

#[test]
fn large_nal_video_on_small_clusters_is_at_most_partial_when_ambiguous() {
    // NAL units of 2–24 KiB on 4 KiB clusters: a seam inside one unit's payload has no checked byte within a cluster
    // on one side, so another cut a cluster away may fit as well. Whatever comes out, an `Intact` one is the clip.
    const C: u64 = 4096;
    let v = avc_video(&small_video(64, 5, false));
    let n = (v.bytes.len() as u64).div_ceil(C) as u32;
    let q = n / 4;
    let clusters = runs(&[3000, 9000, 6000, 12000], &[q, q, q, n - 3 * q]);
    let (src, pieces) = video_card(C as u32, &[("C0005.MP4", &v.bytes, clusters)]);
    let events = events_of(&src, &ScanOptions::default());
    let view = final_view(&events);
    let f = at(&view, pieces[0][0].start);
    if f.health == Health::Intact {
        assert_eq!(extracted(&src, f), v.bytes, "{f:#?}");
    }
    eprintln!(
        "large NAL units on 4 KiB clusters: {:?}, {} pieces (true {}), its pieces {}, len {} of {}, bytes {}",
        f.health,
        f.fragments.len(),
        pieces[0].len(),
        if f.fragments == pieces[0] {
            "the true ones"
        } else {
            "others"
        },
        f.len,
        v.bytes.len(),
        if extracted(&src, f) == v.bytes {
            "the clip's"
        } else {
            "not the clip's"
        },
    );
}

#[test]
fn a_video_whose_last_piece_lies_twice_is_partial() {
    // The five-piece video with a copy of its last piece (the end of its mdat and its moov) in another free cluster:
    // two orphan moovs each give a complete solution, so neither is unique.
    let v = avc_video(&small_video(300, 1, true));
    let clusters = runs(&[300, 900, 500, 1500, 1100], &[1; 5]);
    let (mut bytes, pieces) = video_image(CLUSTER as u32, &[("C0001.MP4", &v.bytes, clusters)]);
    let last = *pieces[0].last().unwrap();
    let copy = last.start + 200 * CLUSTER;
    bytes.copy_within(
        last.start as usize..(last.start + last.len) as usize,
        copy as usize,
    );
    let src = SimSource::new(bytes);
    let events = events_of(&src, &ScanOptions::default());
    let view = final_view(&events);
    let f = at(&view, pieces[0][0].start);
    assert_eq!(f.health, Health::Partial, "{f:#?}");
    assert_eq!(f.len, v.bytes.len() as u64);
    assert_eq!(
        extracted(&src, f),
        v.bytes,
        "either copy holds the same bytes"
    );
}

// The videos part 1.3 left out (Task 6): the index first, an open-ended `mdat`, fragmented MP4, two copies of a start.

/// `v` in three runs of about a third each at clusters 300, 900 and 500.
fn thirds(len: usize) -> Vec<u32> {
    let n = clusters_for(len);
    assert!(n >= 3, "{n}");
    runs(&[300, 900, 500], &[n / 3, n / 3, n - 2 * (n / 3)])
}

#[test]
fn moov_first_video_cut_into_three_is_intact() {
    let v = camera_video(
        &small_video(300, 11, true),
        VideoCodec::Avc,
        VideoLayout::MoovFirst,
    );
    let (src, pieces) = video_card(
        CLUSTER as u32,
        &[("C0011.MP4", &v.bytes, thirds(v.bytes.len()))],
    );
    assert_eq!(pieces[0].len(), 3);
    let events = events_of(&src, &ScanOptions::default());
    let view = final_view(&events);
    let f = at(&view, pieces[0][0].start);
    assert_eq!(f.fragments, pieces[0], "{f:#?}");
    assert_eq!(
        (f.kind, f.health, f.len),
        (FileKind::Mp4, Health::Intact, v.bytes.len() as u64)
    );
    assert!(updated(&events).iter().any(|u| u.id == f.id));
    assert_eq!(extracted(&src, f), v.bytes);
}

#[test]
fn mdat_size_zero_with_moov_first_is_reassembled() {
    let v = camera_video(
        &small_video(300, 12, true),
        VideoCodec::Avc,
        VideoLayout::MoovFirstMdatSizeZero,
    );
    let (src, pieces) = video_card(
        CLUSTER as u32,
        &[("C0012.MP4", &v.bytes, thirds(v.bytes.len()))],
    );
    let events = events_of(&src, &ScanOptions::default());
    let view = final_view(&events);
    let f = at(&view, pieces[0][0].start);
    assert_eq!(f.len, v.bytes.len() as u64, "{f:#?}");
    assert_eq!(
        (f.health, &f.fragments),
        (Health::Intact, &pieces[0]),
        "{f:#?}"
    );
    assert_eq!(extracted(&src, f), v.bytes);
}

/// The top-level boxes of `bytes`: (type, start, end).
fn top_boxes(bytes: &[u8]) -> Vec<([u8; 4], usize, usize)> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos + 8 <= bytes.len() {
        let size = u32::from_be_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
        out.push((bytes[pos + 4..pos + 8].try_into().unwrap(), pos, pos + size));
        pos += size;
    }
    out
}

#[test]
fn fragmented_mp4_in_three_pieces_is_reassembled() {
    // Fragments of 64 large samples (about 850 KiB each): the cuts at a third and two thirds lie inside a fragment's
    // media data, with samples of that fragment checked within a cluster on both sides and no fragment starting within a
    // cluster after them (ruling Q42).
    let v = camera_video(
        &small_video(300, 13, false),
        VideoCodec::Hevc,
        VideoLayout::Fragmented {
            samples_per_fragment: 64,
        },
    );
    let clusters = thirds(v.bytes.len());
    let n = clusters_for(v.bytes.len());
    let moofs: Vec<usize> = top_boxes(&v.bytes)
        .iter()
        .filter(|b| &b.0 == b"moof")
        .map(|b| b.1)
        .collect();
    for cut in [n / 3, 2 * (n / 3)].map(|c| c as usize * CLUSTER as usize) {
        let before = moofs.iter().rev().find(|&&m| m < cut).unwrap();
        assert!(
            *before + 2 * CLUSTER as usize <= cut,
            "{cut}: a fragment starts just before"
        );
        assert!(
            moofs
                .iter()
                .all(|&m| m <= cut || m >= cut + CLUSTER as usize),
            "{cut}"
        );
    }
    let files = [("C0013.MP4", v.bytes.as_slice(), clusters)];
    // The entry deleted (its size known): the chain of boxes reaches it.
    let (src, pieces) = video_card(CLUSTER as u32, &files);
    let events = events_of(&src, &ScanOptions::default());
    let view = final_view(&events);
    let f = at(&view, pieces[0][0].start);
    assert_eq!(
        (f.health, &f.fragments),
        (Health::Intact, &pieces[0]),
        "{f:#?}"
    );
    assert!(updated(&events).iter().any(|u| u.id == f.id));
    assert_eq!(extracted(&src, f), v.bytes);
    // A quick-formatted card: no size and no `mfra`, nothing says where the chain ends.
    let (image, pieces) = video_image_as(CLUSTER as u32, &files, true);
    let src = SimSource::new(image);
    let events = events_of(&src, &ScanOptions::default());
    let view = final_view(&events);
    let f = at(&view, pieces[0][0].start);
    assert!(f.origin.is_none());
    assert_eq!(
        (f.health, &f.fragments),
        (Health::Partial, &pieces[0]),
        "{f:#?}"
    );
    assert_eq!(extracted(&src, f), v.bytes);
}

#[test]
fn fragmented_mp4_cut_between_fragments_is_partial() {
    // Every fragment padded with a `free` box to whole clusters, the clip cut in three runs between fragments: nothing
    // on either side of such a seam belongs to one fragment, so another clip's fragment there could pass every check
    // (ruling Q42). Put back in its exact pieces, at most `Partial`.
    let v = camera_video(
        &small_video(300, 13, true),
        VideoCodec::Hevc,
        VideoLayout::Fragmented {
            samples_per_fragment: 16,
        },
    );
    let c = CLUSTER as usize;
    let pad = |out: &mut Vec<u8>| {
        let r = out.len() % c;
        if r != 0 {
            let p = if c - r < 8 { 2 * c - r } else { c - r };
            out.extend_from_slice(&(p as u32).to_be_bytes());
            out.extend_from_slice(b"free");
            out.resize(out.len() + p - 8, 0);
        }
    };
    let boxes = top_boxes(&v.bytes);
    let first = boxes.iter().position(|b| &b.0 == b"moof").unwrap();
    let mut file = v.bytes[..boxes[first].1].to_vec();
    pad(&mut file);
    for w in boxes[first..].chunks(2) {
        file.extend_from_slice(&v.bytes[w[0].1..w[1].2]);
        pad(&mut file);
    }
    let n = clusters_for(file.len());
    let clusters = runs(&[300, 900, 500], &[n / 3, n / 3, n - 2 * (n / 3)]);
    let (src, pieces) = video_card(CLUSTER as u32, &[("C0016.MP4", &file, clusters)]);
    let events = events_of(&src, &ScanOptions::default());
    let view = final_view(&events);
    let f = at(&view, pieces[0][0].start);
    assert_eq!(
        (f.health, &f.fragments),
        (Health::Partial, &pieces[0]),
        "{f:#?}"
    );
    assert!(updated(&events).iter().any(|u| u.id == f.id));
    assert_eq!(extracted(&src, f), file);
}

#[test]
fn identical_first_pieces_reassemble_once() {
    // On 2 KiB clusters (no grid: pieces lie on 512-byte sectors, forward only), a moov-less video in two runs and a
    // second deleted entry of the same size whose first run is a copy of the video's (the rest written over), lying
    // between the video's two runs: one orphan `moov`, two identical first pieces. Searched apart, the
    // video could go on in the copy's bytes (not unique) and the `moov` would complete both; as one (D17) the lower id
    // comes back whole and the other is left as it was.
    const C: u64 = 2048;
    let v = avc_video(&small_video(300, 14, true));
    let n = (v.bytes.len() as u64).div_ceil(C) as u32;
    let first = n / 3;
    let split = (first as u64 * C) as usize;
    let mut copy = v.bytes[..split].to_vec();
    copy.extend(support::cards::noise((v.bytes.len() - split) as u64, 0xD17));
    let (src, pieces) = video_card(
        C as u32,
        &[
            (
                "C0014.MP4",
                &v.bytes,
                runs(&[3000, 14000], &[first, n - first]),
            ),
            ("C0015.MP4", &copy, runs(&[6000], &[n])),
        ],
    );
    let events = events_of(&src, &ScanOptions::default());
    let view = final_view(&events);
    let (a, b) = (at(&view, pieces[0][0].start), at(&view, pieces[1][0].start));
    assert!(a.id < b.id, "{a:#?} {b:#?}");
    assert_eq!(
        (a.health, &a.fragments),
        (Health::Intact, &pieces[0]),
        "{a:#?}"
    );
    assert_eq!(extracted(&src, a), v.bytes);
    assert_eq!(b.health, Health::Damaged, "{b:#?}");
    assert!(updated(&events).iter().all(|u| u.id != b.id), "{b:#?}");
}

// TIFF-based RAW photos in pieces (Task 8): the TIFF structure, the previews and lossless-JPEG tiles are checked;
// uncompressed or lossy sensor data is blind. A seam is proven only exactly between two checked units anchored at
// their declared places (ruling Q45): tile boundaries, never inside a tile, a preview or sensor data.

/// `got == want`, reporting only where they first differ.
fn same(got: &[u8], want: &[u8]) {
    let first = got.iter().zip(want).position(|(x, y)| x != y);
    assert!(
        got.len() == want.len() && first.is_none(),
        "lengths {} and {}, first difference at {first:?}",
        got.len(),
        want.len()
    );
}

/// A file of an exFAT card on a disk (see [`raw_card`]): name, bytes, clusters in file order, kept live or deleted.
struct OnCard<'a> {
    name: String,
    bytes: &'a [u8],
    clusters: Vec<u32>,
    live: bool,
}

fn deleted<'a>(name: &str, bytes: &'a [u8], clusters: Vec<u32>) -> OnCard<'a> {
    OnCard {
        name: name.into(),
        bytes,
        clusters,
        live: false,
    }
}

/// A small live photo in the one cluster `c`: it takes the cluster, and its start is a finding start.
fn filler(c: u32, jpegs: &mut Vec<Vec<u8>>) -> (String, u32) {
    jpegs.push(jpeg(96, 96, 4000 + u64::from(c), 0));
    assert!(jpegs.last().unwrap().len() as u64 <= CLUSTER);
    (format!("L{c}.JPG"), c)
}

/// An exFAT card (64 MiB, 32 KiB clusters) holding `files` and a one-cluster live photo at each of `fillers`, the
/// deleted files deleted by a computer that clears the chain (`ZeroChain`: the entry keeps the name and size, the FAT
/// and the bitmap forget the clusters), so only the content can place them; every other cluster from 64 on holds
/// seeded noise (old data). `extra`: bytes written over free clusters afterwards (old data of a known kind). Returns the
/// source and each file's absolute pieces.
fn raw_card(
    files: &[OnCard],
    fillers: &[u32],
    extra: &[(u32, &[u8])],
) -> (SimSource, Vec<Vec<Fragment>>) {
    raw_card_as(files, fillers, extra, false)
}

/// [`raw_card`], quick-formatted afterwards when `quick` (no entry is left: no name, no size; the fillers' clusters are
/// free too, but their photos still start there).
fn raw_card_as(
    files: &[OnCard],
    fillers: &[u32],
    extra: &[(u32, &[u8])],
    quick: bool,
) -> (SimSource, Vec<Vec<Fragment>>) {
    let mut b = exfat_card();
    let mut jpegs = Vec::new();
    let fill: Vec<(String, u32)> = fillers.iter().map(|&c| filler(c, &mut jpegs)).collect();
    for f in files {
        b.add_at(&format!("DCIM/{}", f.name), f.bytes, &f.clusters);
    }
    for ((name, c), j) in fill.iter().zip(&jpegs) {
        b.add_at(&format!("DCIM/{name}"), j, &[*c]);
    }
    let pieces: Vec<Vec<Fragment>> = files
        .iter()
        .map(|f| pieces_of(&b, &f.clusters, f.bytes.len() as u64))
        .collect();
    for f in files.iter().filter(|f| !f.live) {
        b.delete(&format!("DCIM/{}", f.name), DeleteStyle::ZeroChain);
    }
    let mut volume = if quick {
        b.build_quick_formatted()
    } else {
        b.build()
    };
    let used: std::collections::HashSet<u32> = files
        .iter()
        .flat_map(|f| f.clusters.iter().copied())
        .chain(fillers.iter().copied())
        .collect();
    let noise = support::cards::noise(volume.len() as u64, 0xA5A5);
    for c in 64..b.cluster_count() + 2 {
        if used.contains(&c) {
            continue;
        }
        let at = b.cluster_offset(c) as usize;
        let end = (at + CLUSTER as usize).min(volume.len());
        volume[at..end].copy_from_slice(&noise[at..end]);
    }
    for &(c, bytes) in extra {
        assert!(!used.contains(&c));
        let at = b.cluster_offset(c) as usize;
        volume[at..at + bytes.len()].copy_from_slice(bytes);
    }
    (SimSource::new(on_disk(&volume)), pieces)
}

/// Clusters (file order) of runs at `firsts` of the `cuts` (in clusters of the file; the last run takes the rest).
fn cut_runs(firsts: &[u32], cuts: &[u32], total: u32) -> Vec<u32> {
    let mut counts = Vec::new();
    let mut prev = 0;
    for &c in cuts.iter().chain(std::iter::once(&total)) {
        counts.push(c - prev);
        prev = c;
    }
    assert_eq!(counts.len(), firsts.len());
    runs(firsts, &counts)
}

/// The final finding at `start`, which must be a RAW of `kind` with `len` bytes.
fn raw_at(
    view: &std::collections::BTreeMap<u64, Finding>,
    start: u64,
    kind: FileKind,
    len: usize,
) -> &Finding {
    let f = at(view, start);
    assert_eq!((f.kind, f.len), (kind, len as u64), "{f:#?}");
    f
}

fn lossless_arw(seed: u64) -> revenant_testkit::TiffRaw {
    arw(&ArwOptions {
        seed,
        ..ArwOptions::default()
    })
}

/// [`lossless_arw`] whose tiles 2, 5 and 8 end on the 32 KiB cluster grid, their coded data sized to it (no fill bytes:
/// a padded tile proves nothing, Q46); the clusters (of the file) where they end.
fn tile_aligned_arw(seed: u64) -> (revenant_testkit::TiffRaw, Vec<u32>) {
    let a = arw(&ArwOptions {
        seed,
        align: CLUSTER as u32,
        sized_tiles: vec![2, 5, 8],
        ..ArwOptions::default()
    });
    let cuts = [2, 5, 8]
        .iter()
        .map(|&k| (a.tiles[k].end as u64 / CLUSTER) as u32)
        .collect();
    (a, cuts)
}

#[test]
fn lossless_arw_in_four_pieces_is_reassembled() {
    // Cuts at cluster boundaries inside tiles: each continuation decodes its tile to the end, but a lossless-JPEG check
    // is evidence, not proof (Q44): Partial, with the exact pieces and bytes.
    let a = lossless_arw(1);
    let n = clusters_for(a.bytes.len());
    assert!(
        a.tiles
            .iter()
            .all(|t| !(t.start as u64).is_multiple_of(CLUSTER))
    );
    let clusters = cut_runs(&[300, 900, 500, 1500], &[4, 8, 12], n);
    let (src, pieces) = raw_card(
        &[deleted("DSC00001.ARW", &a.bytes, clusters)],
        &[304, 904, 504],
        &[],
    );
    assert_eq!(pieces[0].len(), 4);
    let events = events_of(&src, &ScanOptions::default());
    let view = final_view(&events);
    let f = raw_at(&view, pieces[0][0].start, FileKind::Arw, a.bytes.len());
    assert_eq!(f.fragments, pieces[0], "{f:#?}");
    assert_eq!(f.health, Health::Partial);
    assert!(updated(&events).iter().any(|u| u.id == f.id));
    same(&extracted(&src, f), &a.bytes);
}

#[test]
fn lossless_arw_cut_at_tile_boundaries_is_intact() {
    // Every seam lies exactly between two whole tiles, each decoding to its FF D9 at its own declared end: proven.
    let (a, cuts) = tile_aligned_arw(1);
    let n = clusters_for(a.bytes.len());
    let clusters = cut_runs(&[300, 900, 500, 1500], &cuts, n);
    let (src, pieces) = raw_card(
        &[deleted("DSC00002.ARW", &a.bytes, clusters)],
        &[
            300 + cuts[0],
            900 + cuts[1] - cuts[0],
            500 + cuts[2] - cuts[1],
        ],
        &[],
    );
    assert_eq!(pieces[0].len(), 4);
    let events = events_of(&src, &ScanOptions::default());
    let view = final_view(&events);
    let f = raw_at(&view, pieces[0][0].start, FileKind::Arw, a.bytes.len());
    assert_eq!(f.fragments, pieces[0], "{f:#?}");
    assert_eq!(f.health, Health::Intact, "{f:#?}");
    assert!(updated(&events).iter().any(|u| u.id == f.id));
    same(&extracted(&src, f), &a.bytes);
}

/// `bytes` in three runs from cluster 300, each run but the last followed by a one-cluster photo, on a quick-formatted
/// card: "nothing in between" (Q14), the free run of each piece ends where the next photo starts. (With the deleted
/// entry left, the file system's guess, contiguous over the free clusters, is already these pieces.)
fn blind_in_three(name: &str, bytes: &[u8], kind: FileKind) {
    let n = clusters_for(bytes.len());
    let (k1, k2) = (n / 3, n / 3);
    let firsts = [300, 300 + k1 + 1, 300 + k1 + k2 + 2];
    let clusters = cut_runs(&firsts, &[k1, k1 + k2], n);
    let (src, pieces) = raw_card_as(
        &[deleted(name, bytes, clusters)],
        &[300 + k1, 301 + k1 + k2],
        &[],
        true,
    );
    assert_eq!(pieces[0].len(), 3);
    let events = events_of(&src, &ScanOptions::default());
    let view = final_view(&events);
    let f = raw_at(&view, pieces[0][0].start, kind, bytes.len());
    // Placed by the D4 order (the next free cluster), never proven: Partial.
    assert_eq!(f.fragments, pieces[0], "{f:#?}");
    assert_eq!(f.health, Health::Partial);
    assert!(updated(&events).iter().any(|u| u.id == f.id));
    same(&extracted(&src, f), bytes);
}

#[test]
fn uncompressed_arw_in_three_pieces_is_partial_with_bytes_in_place() {
    let a = arw(&ArwOptions {
        compression: ArwCompression::Uncompressed,
        ..ArwOptions::default()
    });
    blind_in_three("DSC00003.ARW", &a.bytes, FileKind::Arw);
    // The other TIFF RAWs go through the same seams.
    let nef = tiff_raw(&TiffRawOptions {
        variant: TiffVariant::Nef,
        big_endian: true,
        raw_len: 1 << 20,
        seed: 3,
        preview: Some((320, 240)),
        preview_sector_aligned: false,
        exif: TestExif::default(),
    });
    blind_in_three("DSC00004.NEF", &nef.bytes, FileKind::Nef);
}

#[test]
fn lossy_arw_is_never_intact_when_split_in_sensor_data() {
    let a = arw(&ArwOptions {
        compression: ArwCompression::Lossy,
        ..ArwOptions::default()
    });
    let n = clusters_for(a.bytes.len());
    let k = n / 2;
    assert!(k as u64 * CLUSTER > a.raw.start as u64);
    let clusters = cut_runs(&[300, 301 + k], &[k], n);
    // Quick-formatted: no entry is left to guess from, only the content places the second piece.
    let (src, pieces) = raw_card_as(
        &[deleted("DSC00005.ARW", &a.bytes, clusters)],
        &[300 + k],
        &[],
        true,
    );
    let events = events_of(&src, &ScanOptions::default());
    let view = final_view(&events);
    let f = raw_at(&view, pieces[0][0].start, FileKind::Arw, a.bytes.len());
    assert_eq!(f.fragments, pieces[0], "{f:#?}");
    assert_eq!(f.health, Health::Partial);
    assert!(updated(&events).iter().any(|u| u.id == f.id));
    same(&extracted(&src, f), &a.bytes);
}

#[test]
fn preview_split_across_a_seam_is_validated() {
    // The first cluster ends inside the preview. Right after the live photo that follows it lies the second cluster of
    // a burst neighbour (the same scene shifted by a pixel): its preview tail and sensor data. It is the first
    // candidate; the preview decoded across the seam refutes it, and the true continuation follows.
    let opts = |seed| ArwOptions {
        compression: ArwCompression::Uncompressed,
        seed,
        ..ArwOptions::default()
    };
    let (a, b) = (arw(&opts(1)), arw(&opts(2)));
    let p = a.preview.clone().unwrap();
    assert!(p.start < CLUSTER as usize && p.end > CLUSTER as usize);
    let n = clusters_for(a.bytes.len());
    let clusters = cut_runs(&[300, 303], &[1], n);
    let tail = &b.bytes[CLUSTER as usize..2 * CLUSTER as usize];
    let (src, pieces) = raw_card(
        &[deleted("DSC00006.ARW", &a.bytes, clusters)],
        &[301],
        &[(302, tail)],
    );
    let events = events_of(&src, &ScanOptions::default());
    let view = final_view(&events);
    let f = raw_at(&view, pieces[0][0].start, FileKind::Arw, a.bytes.len());
    assert_eq!(f.fragments, pieces[0], "{f:#?}");
    // The seam lies inside the preview (a content-only JPEG seam) and the sensor data is blind: Partial.
    assert_eq!(f.health, Health::Partial);
    let got = extracted(&src, f);
    same(&got[p.clone()], &a.bytes[p.clone()]);
    same(&got, &a.bytes);
    // The preview, out of the extracted file, is a whole JPEG.
    let preview = SimSource::new(got[p].to_vec());
    let found = final_view(&events_of(&preview, &ScanOptions::default()));
    assert!(
        found
            .values()
            .any(|j| j.start == 0 && j.kind == FileKind::Jpeg && j.health == Health::Intact),
        "{found:#?}"
    );
}

/// The ARW findings of the final view.
fn raws(view: &std::collections::BTreeMap<u64, Finding>) -> Vec<&Finding> {
    view.values().filter(|f| f.kind == FileKind::Arw).collect()
}

/// Two RAWs of one camera and settings (seeds 1 and 2) written interleaved, next free cluster first: A1 B1 A2 B2 ...
/// from cluster 300; both deleted. No `Intact` finding is made of the other's bytes.
fn burst_interleaved(a: &[u8], b: &[u8], cuts_a: &[u32], cuts_b: &[u32]) -> Vec<Health> {
    let (na, nb) = (clusters_for(a.len()), clusters_for(b.len()));
    let counts = |cuts: &[u32], n: u32| {
        let mut out = Vec::new();
        let mut prev = 0;
        for &c in cuts.iter().chain(std::iter::once(&n)) {
            out.push(c - prev);
            prev = c;
        }
        out
    };
    let (ca, cb) = (counts(cuts_a, na), counts(cuts_b, nb));
    assert_eq!(ca.len(), cb.len());
    let (mut fa, mut fb) = (Vec::new(), Vec::new());
    let mut next = 300;
    for (x, y) in ca.iter().zip(&cb) {
        fa.push(next);
        next += x;
        fb.push(next);
        next += y;
    }
    let (src, pieces) = raw_card(
        &[
            deleted("DSC00010.ARW", a, runs(&fa, &ca)),
            deleted("DSC00011.ARW", b, runs(&fb, &cb)),
        ],
        &[],
        &[],
    );
    let events = events_of(&src, &ScanOptions::default());
    let view = final_view(&events);
    let mut out = Vec::new();
    for p in &pieces {
        let f = at(&view, p[0].start);
        assert_eq!(f.kind, FileKind::Arw, "{f:#?}");
        out.push(f.health);
    }
    for f in raws(&view) {
        if f.health == Health::Intact {
            let own = if f.start == pieces[0][0].start {
                a
            } else {
                assert_eq!(
                    f.start, pieces[1][0].start,
                    "an Intact RAW elsewhere: {f:#?}"
                );
                b
            };
            same(&extracted(&src, f), own);
        }
    }
    out
}

#[test]
fn burst_arw_neighbours_are_not_glued_intact() {
    let (a, b) = (lossless_arw(1), lossless_arw(2));
    let h = burst_interleaved(&a.bytes, &b.bytes, &[4, 8, 12], &[4, 8, 12]);
    assert!(
        h.iter().all(|&h| h == Health::Partial),
        "in-tile seams: {h:?}"
    );
    // Cut at tile boundaries, where Intact is possible: each RAW's own bytes only. The neighbours' tiles at a candidate
    // start do not end where the seam's next tile is declared to (their byte counts differ), so both are proven.
    let ((a, ca), (b, cb)) = (tile_aligned_arw(1), tile_aligned_arw(2));
    let h = burst_interleaved(&a.bytes, &b.bytes, &ca, &cb);
    assert_eq!(h, [Health::Intact, Health::Intact]);
}

#[test]
fn raw_preview_found_in_a_later_piece_is_its_own() {
    // The IFDs fill the first cluster; the thumbnail (IFD1) and the preview right after it open the second piece. The
    // scan finds both as Intact photos, claimed, their ranges touching (one merged range): they are the RAW's own files
    // at the places its structure gives, so they neither stop its free run nor its candidates (D10, Q7).
    let a = arw(&ArwOptions {
        thumbnail: true,
        jpegs_at: CLUSTER as u32,
        ..ArwOptions::default()
    });
    let (t, p) = (a.thumbnail.clone().unwrap(), a.preview.clone().unwrap());
    assert_eq!((t.start as u64, t.end), (CLUSTER, p.start));
    let n = clusters_for(a.bytes.len());
    let clusters = cut_runs(&[300, 900, 500], &[1, 8], n);
    let (src, pieces) = raw_card(
        &[deleted("DSC00007.ARW", &a.bytes, clusters)],
        &[301, 908],
        &[],
    );
    let events = events_of(&src, &ScanOptions::default());
    let view = final_view(&events);
    let f = raw_at(&view, pieces[0][0].start, FileKind::Arw, a.bytes.len());
    assert_eq!(f.fragments, pieces[0], "{f:#?}");
    assert_eq!(f.health, Health::Partial, "a seam inside a tile");
    same(&extracted(&src, f), &a.bytes);
    // The two JPEG findings stay, nested in the RAW at its own offsets (Q16: the same file, recorded once as its own).
    let second = pieces[0][1].start;
    for (range, what) in [(&t, "thumbnail"), (&p, "preview")] {
        let at_abs = second + (range.start as u64 - CLUSTER);
        let j = at(&view, at_abs);
        assert_eq!(
            (j.kind, j.health, j.len),
            (FileKind::Jpeg, Health::Intact, range.len() as u64),
            "{what}: {j:#?}"
        );
        assert!(
            f.ranges()
                .iter()
                .any(|r| r.start <= at_abs && at_abs + j.len <= r.start + r.len),
            "{what} nested in the RAW"
        );
    }
}

// Ruling Q46, both sides of a seam: a tile padded with fill bytes before its FF D9 ends where it is declared to whatever
// same-frame data fills it, so it anchors no seam, before or after it (the re-review's probes, as full scans).
/// Tiles 2, 3, 5 and 8 padded with fill bytes to end on the cluster grid.
fn padded(seed: u64) -> revenant_testkit::TiffRaw {
    arw(&ArwOptions {
        seed,
        align: CLUSTER as u32,
        aligned_tiles: vec![2, 3, 5, 8],
        ..ArwOptions::default()
    })
}

/// A with its tile 3 bytes replaced by B's (same padded length), A, and the tile-3 cluster bounds.
fn a_with_b3() -> (Vec<u8>, Vec<u8>, u32, u32) {
    let (a, b) = (padded(1), padded(2));
    let (ta, tb) = (a.tiles[3].clone(), b.tiles[3].clone());
    assert_eq!(ta, tb);
    assert_eq!(ta.len() as u64, 2 * CLUSTER);
    let mut x = a.bytes.clone();
    x[ta.clone()].copy_from_slice(&b.bytes[tb]);
    assert_ne!(x, a.bytes);
    (
        x,
        a.bytes.clone(),
        (ta.start as u64 / CLUSTER) as u32,
        (ta.end as u64 / CLUSTER) as u32,
    )
}

#[test]
fn a_neighbours_padded_tile_alone_in_a_piece_is_not_intact() {
    // Reviewer's probe: A1 | B's tile 3 alone | A from tile 4, each in its own run; A's true tile 3 gone.
    let (x, _, s3, e3) = a_with_b3();
    let n = clusters_for(x.len());
    let clusters = cut_runs(&[300, 900, 500], &[s3, e3], n);
    let (src, pieces) = raw_card(
        &[deleted("DSC00020.ARW", &x, clusters)],
        &[300 + s3, 900 + (e3 - s3)],
        &[],
    );
    let events = events_of(&src, &ScanOptions::default());
    let view = final_view(&events);
    let f = at(&view, pieces[0][0].start);
    assert_ne!(f.health, Health::Intact, "{f:#?}");
}

#[test]
fn a_neighbours_padded_tile_ending_the_first_piece_is_not_intact() {
    // Variant: B's padded tile 3 lies right after A's first run (contiguous), then A from tile 4 elsewhere. The seam is
    // at the END of the foreign padded tile.
    let (x, _, _, e3) = a_with_b3();
    let n = clusters_for(x.len());
    let clusters = cut_runs(&[300, 500], &[e3], n);
    let (src, pieces) = raw_card(&[deleted("DSC00021.ARW", &x, clusters)], &[300 + e3], &[]);
    let events = events_of(&src, &ScanOptions::default());
    let view = final_view(&events);
    let f = at(&view, pieces[0][0].start);
    assert_ne!(f.health, Health::Intact, "{f:#?}");
}

#[test]
fn a_neighbours_padded_tile_ending_the_first_piece_is_not_intact_after_a_quick_format() {
    // Same as B on a quick-formatted card (no entry size).
    let (x, _, _, e3) = a_with_b3();
    let n = clusters_for(x.len());
    let clusters = cut_runs(&[300, 500], &[e3], n);
    let (src, pieces) = raw_card_as(
        &[deleted("DSC00022.ARW", &x, clusters)],
        &[300 + e3],
        &[],
        true,
    );
    let events = events_of(&src, &ScanOptions::default());
    let view = final_view(&events);
    let f = at(&view, pieces[0][0].start);
    assert_ne!(f.health, Health::Intact, "{f:#?}");
}

#[test]
fn a_padded_tile_of_a_reported_neighbour_is_not_taken_intact() {
    // B (deleted, FAT chain kept: placed by the file system, Intact, claimed) in two runs, its second run starting at
    // its padded tile 3, right after A's first run (A[..tile 3]); A's tile 3 gone (noise), A from tile 4 elsewhere.
    let (a, b) = (padded(1), padded(2));
    let ta = a.tiles[3].clone();
    let (s3, e3) = (
        (ta.start as u64 / CLUSTER) as u32,
        (ta.end as u64 / CLUSTER) as u32,
    );
    let (na, nb) = (clusters_for(a.bytes.len()), clusters_for(b.bytes.len()));
    // A: [0, s3) at 300 - s3 .. 300 ; tile 3 at 1500 (overwritten below) ; rest at 500.
    let ca = cut_runs(&[300 - s3, 1500, 500], &[s3, e3], na);
    // B: [0, s3) at 800 ; [s3, ..) at 300.
    let cb = cut_runs(&[800, 300], &[s3], nb);
    let mut fs = exfat_card();
    fs.add_at("DCIM/DSC00030.ARW", &a.bytes, &ca);
    fs.add_at("DCIM/DSC00031.ARW", &b.bytes, &cb);
    let pa = pieces_of(&fs, &ca, a.bytes.len() as u64);
    let pb = pieces_of(&fs, &cb, b.bytes.len() as u64);
    fs.delete("DCIM/DSC00030.ARW", DeleteStyle::ZeroChain);
    fs.delete("DCIM/DSC00031.ARW", DeleteStyle::Camera);
    let mut volume = fs.build();
    let used: std::collections::HashSet<u32> = ca
        .iter()
        .chain(cb.iter())
        .copied()
        .filter(|c| !(1500..1502).contains(c))
        .collect();
    let noise = support::cards::noise(volume.len() as u64, 0xA5A5);
    for c in 64..fs.cluster_count() + 2 {
        if used.contains(&c) {
            continue;
        }
        let at = fs.cluster_offset(c) as usize;
        let end = (at + CLUSTER as usize).min(volume.len());
        volume[at..end].copy_from_slice(&noise[at..end]);
    }
    let src = SimSource::new(on_disk(&volume));
    let events = events_of(&src, &ScanOptions::default());
    let view = final_view(&events);
    let fb = at(&view, pb[0].start);
    let fa = at(&view, pa[0].start);
    let _ = fb;
    assert_ne!(fa.health, Health::Intact, "{fa:#?}");
}

#[test]
fn deleted_video_whose_guess_holds_its_moov_is_intact() {
    // A clip in 2 runs with exactly a live photo between them, deleted by a computer (the chain cleared): the file
    // system's guess (the free clusters from its first one) is already right, and its own `moov`, at the end of the
    // guess, is no orphan. The reassembly takes that `moov` and the guess as its first hypothesis: the samples prove
    // the seam, Intact (Q52).
    for small_nals in [false, true] {
        let v = avc_video(&small_video(120, 3, small_nals));
        let n = clusters_for(v.bytes.len());
        let k = n / 3;
        let clusters = cut_runs(&[300, 301 + k], &[k], n);
        let (src, pieces) = raw_card(&[deleted("C0001.MP4", &v.bytes, clusters)], &[300 + k], &[]);
        assert_eq!(pieces[0].len(), 2);
        let events = events_of(&src, &ScanOptions::default());
        let view = final_view(&events);
        let f = at(&view, pieces[0][0].start);
        assert_eq!(f.fragments, pieces[0], "{f:#?}");
        assert_eq!(
            (f.kind, f.health),
            (FileKind::Mp4, Health::Intact),
            "{f:#?}"
        );
        same(&extracted(&src, f), &v.bytes);
    }
}

#[test]
fn a_stale_entry_inside_a_deleted_video_does_not_stop_it() {
    // As above, and an old photo's entry, deleted before the clip was written over its cluster (inside the clip's second
    // run): the scan reports a finding there for the entry, but no format recognizes a head at that cluster, so it is
    // no file start that would stop the clip's piece (Q52).
    let v = avc_video(&small_video(120, 3, false));
    let n = clusters_for(v.bytes.len());
    let k = n / 3;
    let clusters = cut_runs(&[300, 301 + k], &[k], n);
    let old_at = 301 + k + 4;
    let old = jpeg(96, 96, 77, 0);
    let (src, pieces) = raw_card(
        &[
            deleted("DSC00001.JPG", &old, vec![old_at]),
            deleted("C0001.MP4", &v.bytes, clusters),
        ],
        &[300 + k],
        &[],
    );
    let events = events_of(&src, &ScanOptions::default());
    let view = final_view(&events);
    let f = at(&view, pieces[1][0].start);
    assert_eq!(f.fragments, pieces[1], "{f:#?}");
    assert_eq!(
        (f.kind, f.health),
        (FileKind::Mp4, Health::Intact),
        "{f:#?}"
    );
    same(&extracted(&src, f), &v.bytes);
}

#[test]
fn deleted_video_wrapping_past_the_heap_end_is_intact() {
    // A rotating allocator wrapped: the clip's first run is the heap's last clusters, the rest goes on near its start
    // (deleted by a computer, the chain cleared). The candidate order looks from the heap start after the window past
    // the piece ("first hole"), so the clip is put back with its orphan `moov`, the samples proving the seam.
    let v = avc_video(&small_video(120, 3, false));
    let n = clusters_for(v.bytes.len());
    let last = exfat_card().cluster_count() + 2;
    let k = 4;
    let clusters = cut_runs(&[last - k, 300], &[k], n);
    let (src, pieces) = raw_card(&[deleted("C0001.MP4", &v.bytes, clusters)], &[], &[]);
    assert_eq!(pieces[0].len(), 2);
    let events = events_of(&src, &ScanOptions::default());
    let view = final_view(&events);
    let f = at(&view, pieces[0][0].start);
    assert_eq!(f.fragments, pieces[0], "{f:#?}");
    assert_eq!(
        (f.kind, f.health),
        (FileKind::Mp4, Health::Intact),
        "{f:#?}"
    );
    same(&extracted(&src, f), &v.bytes);
}
