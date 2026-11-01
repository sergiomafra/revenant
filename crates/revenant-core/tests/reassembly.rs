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
    AvcOptions, VideoCodec, VideoLayout, avc_video, camera_video, jpeg, jpeg_smooth,
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
