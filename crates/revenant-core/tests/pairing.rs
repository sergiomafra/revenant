//! A camera video whose `moov` ended up away from its `mdat`: paired after the scan and reported as `Updated`.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use revenant_core::formats::{FileKind, Health, all_formats};
use revenant_core::source::{FileSource, Source, VolumeId};
use revenant_core::{
    CancelToken, EngineError, Event, Finding, Fragment, ScanOptions, ScanSummary, extract, scan,
};
use revenant_testkit::{AvcOptions, AvcVideo, DiskImage, avc_video};

const MIB: usize = 1024 * 1024;

fn video(seed: u64, small_nals: bool, mdat_pad: usize) -> AvcVideo {
    avc_video(&AvcOptions {
        samples: 120,
        seed,
        mov: false,
        mdat_pad,
        small_nals,
    })
}

/// The first sector boundary (relative to the file) at or after `fifths`/5 of the `mdat` payload.
fn cut(v: &AvcVideo, fifths: usize) -> usize {
    let p = &v.mdat_payload;
    (p.start + p.len() * fifths / 5).div_ceil(512) * 512
}

/// Wraps a source: unknown volume (so tests may extract next to the image), optional unreadable sector, counts bytes,
/// and once `arm` is set cancels `cancel` at the next read.
struct Src {
    inner: FileSource,
    bad: Option<u64>,
    bytes: AtomicU64,
    arm: AtomicBool,
    cancel: Option<CancelToken>,
}

impl Src {
    fn new(file: &tempfile::NamedTempFile) -> Self {
        Self {
            inner: FileSource::open(file.path()).unwrap(),
            bad: None,
            bytes: AtomicU64::new(0),
            arm: AtomicBool::new(false),
            cancel: None,
        }
    }
}

impl Source for Src {
    fn len(&self) -> u64 {
        self.inner.len()
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, EngineError> {
        if let Some(bad) = self.bad
            && offset < bad + 512
            && offset + buf.len() as u64 > bad
        {
            return Err(EngineError::Read {
                offset,
                cause: std::io::Error::other("injected"),
            });
        }
        if self.arm.load(Ordering::SeqCst)
            && let Some(c) = &self.cancel
        {
            c.cancel();
        }
        let n = self.inner.read_at(offset, buf)?;
        self.bytes.fetch_add(n as u64, Ordering::SeqCst);
        Ok(n)
    }
    fn volume(&self) -> Option<VolumeId> {
        None
    }
}

fn run_with(
    source: &dyn Source,
    opts: &ScanOptions,
    cancel: &CancelToken,
) -> (Result<ScanSummary, EngineError>, Vec<Event>) {
    let mut events = Vec::new();
    let r = scan(source, &all_formats(), opts, cancel, &mut |e| {
        events.push(e)
    });
    (r, events)
}

fn run(source: &dyn Source) -> (ScanSummary, Vec<Event>) {
    let (r, events) = run_with(source, &ScanOptions::default(), &CancelToken::default());
    let summary = r.unwrap();
    for f in updated(&events) {
        assert_pieces(&f, source.len());
    }
    (summary, events)
}

fn found(events: &[Event]) -> Vec<Finding> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Found(f) => Some(f.clone()),
            _ => None,
        })
        .collect()
}

fn updated(events: &[Event]) -> Vec<Finding> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Updated(f) => Some(f.clone()),
            _ => None,
        })
        .collect()
}

/// The pieces contract: at least 2 pieces in file order, the first at `start`, lengths summing to `len`, no overlap,
/// all inside the source.
fn assert_pieces(f: &Finding, source_len: u64) {
    let p = &f.fragments;
    assert!(p.len() >= 2, "{f:?}");
    assert_eq!(p[0].start, f.start, "{f:?}");
    assert_eq!(p.iter().map(|x| x.len).sum::<u64>(), f.len, "{f:?}");
    assert!(p.iter().all(|x| x.start + x.len <= source_len), "{f:?}");
    let mut sorted = p.clone();
    sorted.sort_by_key(|x| x.start);
    assert!(
        sorted
            .windows(2)
            .all(|w| w[0].start + w[0].len <= w[1].start),
        "{f:?}"
    );
}

/// The bytes `extract` writes for `f`.
fn extracted(source: &dyn Source, f: &Finding) -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    let path = extract(source, f, dir.path()).unwrap();
    std::fs::read(path).unwrap()
}

/// 24 MiB of noise with `v`'s first `l1` bytes at 1 MiB and the rest at 12 MiB.
fn split_image(v: &AvcVideo, l1: usize) -> DiskImage {
    let mut img = DiskImage::noise(24 * MIB, 7);
    img.put(MIB, &v.bytes[..l1]).put(12 * MIB, &v.bytes[l1..]);
    img
}

#[test]
fn split_video_is_reassembled_intact() {
    let v = video(1, true, 0);
    let l1 = cut(&v, 2);
    let file = split_image(&v, l1).save();
    let src = Src::new(&file);
    let (summary, events) = run(&src);

    let f = found(&events);
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(
        (f[0].id, f[0].start, f[0].kind, f[0].health),
        (1, MIB as u64, FileKind::Mp4, Health::Damaged)
    );
    let last = events
        .iter()
        .rfind(|e| !matches!(e, Event::Progress { .. }))
        .unwrap();
    let expected = Finding {
        id: 1,
        start: MIB as u64,
        len: v.bytes.len() as u64,
        kind: FileKind::Mp4,
        health: Health::Intact,
        fragments: vec![
            Fragment {
                start: MIB as u64,
                len: l1 as u64,
            },
            Fragment {
                start: 12 * MIB as u64,
                len: (v.bytes.len() - l1) as u64,
            },
        ],
    };
    assert_eq!(last, &Event::Updated(expected.clone()));
    assert_eq!(extracted(&src, &expected), v.bytes);
    assert_eq!(summary.updated, 1);
    assert_eq!(summary.found, 1);
}

#[test]
fn split_just_before_moov_is_reassembled() {
    let v = video(1, true, 0);
    let l1 = v.moov.start / 512 * 512;
    let file = split_image(&v, l1).save();
    let src = Src::new(&file);
    let (_, events) = run(&src);
    let u = updated(&events);
    assert_eq!(u.len(), 1, "{events:?}");
    assert_eq!(u[0].health, Health::Intact);
    assert_eq!(u[0].fragments[0].len, l1 as u64);
    assert_eq!(extracted(&src, &u[0]), v.bytes);
}

#[test]
fn ambiguous_break_point_is_partial() {
    // Large NAL units: a break point a sector off reads noise inside one NAL unit's payload and still passes.
    let v = video(1, false, 0);
    let l1 = cut(&v, 2);
    let file = split_image(&v, l1).save();
    let src = Src::new(&file);
    let (_, events) = run(&src);
    let u = updated(&events);
    assert_eq!(u.len(), 1, "{events:?}");
    assert_eq!(u[0].health, Health::Partial);
    let chosen = u[0].fragments[0].len;
    assert!(
        chosen <= l1 as u64 && chosen.is_multiple_of(512),
        "{chosen}"
    );
    assert_eq!(u[0].len, v.bytes.len() as u64);
}

#[test]
fn moov_pairs_with_its_own_mdat_not_a_sibling() {
    let v1 = video(1, true, 0);
    // Pad v2's mdat so both moovs would sit at the same offset modulo 512: each moov passes the arithmetic filter for
    // both videos, so only the sample checks tell them apart.
    let bare = video(2, true, 0).moov.start;
    let pad = (v1.moov.start + 512 - bare % 512) % 512 + 512;
    let v2 = video(2, true, pad);
    assert_eq!(v1.moov.start % 512, v2.moov.start % 512);
    let (c1, c2) = (cut(&v1, 2), cut(&v2, 2));
    let mut img = DiskImage::noise(24 * MIB, 7);
    img.put(MIB, &v1.bytes[..c1])
        .put(4 * MIB, &v2.bytes[..c2])
        .put(10 * MIB, &v2.bytes[c2..])
        .put(16 * MIB, &v1.bytes[c1..]);
    let file = img.save();
    let src = Src::new(&file);
    let (summary, events) = run(&src);
    let u = updated(&events);
    assert_eq!(u.len(), 2, "{events:?}");
    assert_eq!(summary.updated, 2);
    assert_eq!((u[0].id, u[0].start), (1, MIB as u64));
    assert_eq!((u[1].id, u[1].start), (2, 4 * MIB as u64));
    assert_eq!(u[0].fragments[1].start, (16 * MIB) as u64);
    assert_eq!(u[1].fragments[1].start, (10 * MIB) as u64);
    assert_eq!(extracted(&src, &u[0]), v1.bytes);
    assert_eq!(extracted(&src, &u[1]), v2.bytes);
}

#[test]
fn moov_matching_two_videos_is_not_paired() {
    let v = video(1, true, 0);
    let l1 = cut(&v, 2);
    let mut img = DiskImage::noise(24 * MIB, 7);
    img.put(MIB, &v.bytes[..l1])
        .put(5 * MIB, &v.bytes[..l1])
        .put(12 * MIB, &v.bytes[l1..]);
    let file = img.save();
    let (summary, events) = run(&Src::new(&file));
    assert!(updated(&events).is_empty(), "{events:?}");
    assert_eq!(summary.updated, 0);
    let f = found(&events);
    assert_eq!(f.len(), 2);
    assert!(
        f.iter()
            .all(|f| f.health == Health::Damaged && f.kind == FileKind::Mp4)
    );
}

#[test]
fn video_split_in_three_is_not_paired() {
    let v = video(1, true, 0);
    let (c1, c2) = (cut(&v, 2), cut(&v, 3));
    let mut img = DiskImage::noise(24 * MIB, 7);
    img.put(MIB, &v.bytes[..c1])
        .put(6 * MIB, &v.bytes[c1..c2])
        .put(12 * MIB, &v.bytes[c2..]);
    let file = img.save();
    let (_, events) = run(&Src::new(&file));
    assert!(updated(&events).is_empty(), "{events:?}");
    assert_eq!(found(&events).len(), 1);
}

/// The split layout with other data written over some samples of the first piece (512 bytes at a tenth of the
/// payload: one or two samples the search for the break point never reads) or of the second (8 KiB, about 16 samples,
/// at four fifths): the break point still checks out, the evenly spaced verification does not.
fn overwritten(second: bool) -> Vec<Event> {
    let v = video(1, true, 0);
    let l1 = cut(&v, 2);
    let mut img = split_image(&v, l1);
    let p = &v.mdat_payload;
    let (at, len) = if second {
        (12 * MIB + (cut(&v, 4) - l1), 8192)
    } else {
        (MIB + p.start + p.len() / 10, 512)
    };
    img.put(at, &vec![0x5A; len]);
    let file = img.save();
    run(&Src::new(&file)).1
}

#[test]
fn first_piece_overwritten_inside_is_not_paired() {
    let events = overwritten(false);
    assert!(updated(&events).is_empty(), "{events:?}");
}

#[test]
fn second_piece_overwritten_inside_is_not_paired() {
    let events = overwritten(true);
    assert!(updated(&events).is_empty(), "{events:?}");
}

/// A 4000-sample video split at 2/5 (first piece at 1 MiB, the rest at 12 MiB) with a photo written over `n` evenly
/// spaced places of its second piece (`second`) or of its first piece's samples: the 32 samples checked on each side
/// miss it at most of them. For each place: the photo's health, the `Damaged` video's length and the `Updated` video.
fn photo_over_split_video(second: bool, n: usize) -> Vec<(Option<Health>, u64, Option<Finding>)> {
    let v = avc_video(&AvcOptions {
        samples: 4000,
        seed: 1,
        mov: false,
        mdat_pad: 0,
        small_nals: true,
    });
    let l1 = cut(&v, 2);
    let photo = revenant_testkit::jpeg(128, 96, 3, 0);
    let p = &v.mdat_payload;
    let (lo, hi) = if second {
        (12 * MIB, 12 * MIB + p.end - l1 - photo.len())
    } else {
        (MIB + p.start + 4096, MIB + l1 - photo.len())
    };
    (0..n)
        .map(|i| {
            let at = (lo + (hi - lo) * i / (n - 1)) / 512 * 512;
            let mut img = split_image(&v, l1);
            img.put(at, &photo);
            let file = img.save();
            let (_, events) = run(&Src::new(&file));
            let f = found(&events);
            let photo = f.iter().find(|f| f.start == at as u64).map(|f| f.health);
            let video = f.iter().find(|f| f.start == MIB as u64).expect("the video");
            assert_eq!(video.health, Health::Damaged);
            (photo, video.len, updated(&events).pop())
        })
        .collect()
}

#[test]
fn a_photo_inside_the_second_piece_makes_the_pairing_partial() {
    // The bytes of both would be claimed twice: extracting the video would copy the photo into it.
    for (i, (photo, _, u)) in photo_over_split_video(true, 20).into_iter().enumerate() {
        assert_eq!(photo, Some(Health::Intact), "place {i}");
        assert!(
            u.as_ref().is_none_or(|u| u.health != Health::Intact),
            "place {i}: {u:?}"
        );
    }
}

#[test]
fn a_photo_inside_the_first_piece_ends_it() {
    // The scan cut the Damaged video at the photo: the first piece cannot run past it (ruling Q13).
    for (i, (photo, len, u)) in photo_over_split_video(false, 12).into_iter().enumerate() {
        assert_eq!(photo, Some(Health::Intact), "place {i}");
        assert!(
            u.as_ref()
                .is_none_or(|u| u.health != Health::Intact && u.fragments[0].len <= len),
            "place {i}: {u:?}"
        );
    }
}

#[test]
fn motion_jpeg_frames_inside_the_pieces_are_the_videos_own() {
    // Every frame starts on a sector, so the scan finds each one as a JPEG, in both pieces (and cuts the Damaged video
    // at its first frame): those are the video's own frames, not other files.
    let v = revenant_testkit::mjpeg_video(40, 3);
    let frames: Vec<usize> = (512..v.moov.start)
        .step_by(512)
        .filter(|&o| v.bytes[o..o + 3] == [0xFF, 0xD8, 0xFF])
        .collect();
    assert_eq!(frames.len(), 40);
    let l1 = frames[16]; // a sector boundary between two frames
    let file = split_image(&v, l1).save();
    let src = Src::new(&file);
    let (_, events) = run(&src);
    let f = found(&events);
    assert!(
        f.iter().filter(|f| f.kind == FileKind::Jpeg).count() >= 40,
        "{f:?}"
    );
    let u = updated(&events);
    assert_eq!(u.len(), 1, "{events:?}");
    assert_eq!(u[0].health, Health::Intact, "{:?}", u[0]);
    assert_eq!(u[0].fragments[0].len, l1 as u64);
    assert_eq!(extracted(&src, &u[0]), v.bytes);
    // A photo written over frame 20's coded data (its first and last bytes, which the sample check reads, untouched)
    // is another file: the pairing stands, at most Partial.
    let photo = revenant_testkit::jpeg(16, 16, 77, 0);
    let at = 12 * MIB + frames[20] - l1 + 5120;
    assert!(at + photo.len() < 12 * MIB + frames[21] - l1 - 2);
    let mut img = split_image(&v, l1);
    img.put(at, &photo);
    let file = img.save();
    let (_, events) = run(&Src::new(&file));
    let photo = found(&events).into_iter().find(|f| f.start == at as u64);
    assert_eq!(photo.map(|f| f.health), Some(Health::Intact));
    let u = updated(&events);
    assert_eq!(u.len(), 1, "{events:?}");
    assert_eq!(u[0].health, Health::Partial);
}

/// A copy of `v`'s first piece at 1 MiB, and the whole `v` at 12 MiB (with an unreadable sector in its `mdat`
/// padding when `bad`). The whole video's `moov` would pair with the copy, but it is its own.
fn copy_and_whole(bad: bool) -> (Vec<Event>, Vec<Finding>) {
    let v = video(1, true, 4096);
    let l1 = cut(&v, 2);
    let mut img = DiskImage::noise(24 * MIB, 7);
    img.put(MIB, &v.bytes[..l1]).put(12 * MIB, &v.bytes);
    let file = img.save();
    let mut src = Src::new(&file);
    if bad {
        src.bad = Some((12 * MIB + v.mdat_payload.end - 1024) as u64 / 512 * 512);
    }
    let (_, events) = run(&src);
    let f = found(&events);
    (events, f)
}

#[test]
fn moov_of_a_whole_video_is_not_an_orphan() {
    let (events, f) = copy_and_whole(false);
    assert_eq!(f[1].health, Health::Intact);
    assert!(updated(&events).is_empty(), "{events:?}");
}

#[test]
fn moov_of_a_partial_video_is_not_an_orphan() {
    let (events, f) = copy_and_whole(true);
    assert_eq!(f[1].health, Health::Partial);
    assert!(updated(&events).is_empty(), "{events:?}");
}

#[test]
fn moov_inside_an_intact_photo_is_not_an_orphan() {
    // The second piece sits inside a photo's APP1 padding: that photo is Intact, so its blocks may never be read and a
    // candidate there is dropped (whether it was seen or not).
    let v = video(1, true, 0);
    let l1 = cut(&v, 2);
    let mut photo = revenant_testkit::jpeg(64, 64, 9, 60_000);
    photo[8192..8192 + v.bytes.len() - l1].copy_from_slice(&v.bytes[l1..]);
    let mut img = DiskImage::noise(24 * MIB, 7);
    img.put(MIB, &v.bytes[..l1]).put(12 * MIB, &photo);
    let file = img.save();
    let (_, events) = run(&Src::new(&file));
    let f = found(&events);
    assert_eq!(
        (f[1].start, f[1].kind, f[1].health),
        (12 * MIB as u64, FileKind::Jpeg, Health::Intact)
    );
    assert!(updated(&events).is_empty(), "{events:?}");
}

#[test]
fn orphan_moov_alone_is_not_reported() {
    let v = video(1, true, 0);
    let l1 = cut(&v, 2);
    let mut img = DiskImage::noise(24 * MIB, 7);
    img.put(MIB, &v.bytes[l1..]);
    let file = img.save();
    let (summary, events) = run(&Src::new(&file));
    assert!(updated(&events).is_empty());
    assert!(
        found(&events)
            .iter()
            .all(|f| !matches!(f.kind, FileKind::Mp4 | FileKind::Mov))
    );
    assert_eq!(summary.updated, 0);
}

#[test]
fn reassembled_video_with_unreadable_sector_is_partial() {
    // The bad sector lies in the mdat padding after the last sample, inside the second piece: no sample is read there,
    // so the downgrade comes from the bad-sector map, not from a failed check.
    let v = video(1, true, 4096);
    let l1 = cut(&v, 2);
    let samples_end = v.mdat_payload.end - 4096;
    let bad = (12 * MIB + samples_end - l1).div_ceil(512) * 512;
    assert!(bad + 512 <= 12 * MIB + v.moov.start - l1);
    let file = split_image(&v, l1).save();
    let mut src = Src::new(&file);
    src.bad = Some(bad as u64);
    let (_, events) = run(&src);
    assert!(events.contains(&Event::Unreadable {
        start: bad as u64,
        len: 512
    }));
    let u = updated(&events);
    assert_eq!(u.len(), 1, "{events:?}");
    assert_eq!(u[0].health, Health::Partial);
    assert_eq!(u[0].fragments[0].len, l1 as u64);
}

/// A fake `moov` box header: `size "moov" 0000006C "mvhd"`.
fn fake_moov(size: u32) -> Vec<u8> {
    let mut h = size.to_be_bytes().to_vec();
    h.extend_from_slice(b"moov");
    h.extend_from_slice(&0x6Cu32.to_be_bytes());
    h.extend_from_slice(b"mvhd");
    h
}

/// The split layout (first piece at 1 MiB, the rest at 92 MiB) plus an 80 MiB Intact MP4 at 8 MiB whose block at
/// 68 MiB holds 4200 fake `moov` headers: more than a block's cap, but all inside an Intact file. Whether that block is
/// read or skipped depends on timing; the events must not.
fn capped_block_image() -> DiskImage {
    let v = video(1, true, 0);
    let l1 = cut(&v, 2);
    let mut whole = revenant_testkit::mp4(&revenant_testkit::Mp4Options {
        mdat_len: 80 * MIB - 4096,
        moov_at_end: true,
        mov: false,
        mdat_64bit: false,
    });
    for i in 0..4200 {
        let at = 60 * MIB + i * 512 + 100;
        whole[at..at + 16].copy_from_slice(&fake_moov(4096));
    }
    let mut img = DiskImage::noise(100 * MIB, 7);
    img.put(MIB, &v.bytes[..l1])
        .put(8 * MIB, &whole)
        .put(92 * MIB, &v.bytes[l1..]);
    img
}

#[test]
fn pairing_events_do_not_depend_on_threads_or_cache() {
    let v = video(1, true, 0);
    let l1 = cut(&v, 2);
    for img in [split_image(&v, l1), capped_block_image()] {
        let file = img.save();
        let src = Src::new(&file);
        let base = run_with(&src, &ScanOptions::default(), &CancelToken::default());
        let u = updated(&base.1);
        assert_eq!(u.len(), 1);
        assert_eq!(u[0].health, Health::Intact);
        for (threads, cache) in [(1, 1 << 20), (2, 1 << 20), (8, 1 << 20), (4, 3 << 20)] {
            let opts = ScanOptions {
                threads,
                cache_bytes: cache,
                ..ScanOptions::default()
            };
            let other = run_with(&src, &opts, &CancelToken::default());
            assert_eq!(other.1, base.1, "threads {threads} cache {cache}");
            assert_eq!(other.0.unwrap(), base.0.as_ref().unwrap().clone());
        }
    }
}

/// The split layout plus `fakes` fake `moov` headers of `size` bytes, 512 bytes apart from 14 MiB on, congruent to the
/// video's `P` modulo 512 (so the pre-filter loads them) or not. Returns the one `Updated`.
fn split_with_fakes(fakes: usize, size: u32, congruent: bool) -> Finding {
    split_with_fakes_every(fakes, size, congruent, 512)
}

/// [`split_with_fakes`] with the fakes `spacing` bytes apart.
fn split_with_fakes_every(fakes: usize, size: u32, congruent: bool, spacing: usize) -> Finding {
    let v = video(1, true, 0);
    let l1 = cut(&v, 2);
    let mut img = split_image(&v, l1);
    let residue = if congruent {
        v.moov.start % 512
    } else {
        (v.moov.start + 100) % 512
    };
    for i in 0..fakes {
        img.put(14 * MIB + i * spacing + residue, &fake_moov(size));
    }
    let file = img.save();
    let (_, events) = run(&Src::new(&file));
    let u = updated(&events);
    assert_eq!(u.len(), 1, "{events:?}");
    assert_eq!(extracted(&Src::new(&file), &u[0]), v.bytes);
    u[0].clone()
}

#[test]
fn the_kept_candidate_cap_alone_makes_a_pairing_partial() {
    // 4200 fakes 1024 bytes apart: at most 4096 in any 4 MiB block (no block overflows), but 4201 candidates in all.
    assert_eq!(
        split_with_fakes_every(4200, 4096, false, 1024).health,
        Health::Partial
    );
}

#[test]
fn the_per_block_cap_alone_makes_a_pairing_partial() {
    // 4096 fakes 512 bytes apart from 14 MiB fill [14, 16) MiB: with the real moov, block [12, 16) MiB holds 4097
    // candidates, one over its cap, while the 4096 it keeps fit the per-scan cap.
    assert_eq!(
        split_with_fakes_every(4096, 4096, false, 512).health,
        Health::Partial
    );
}

#[test]
fn more_candidates_than_kept_make_a_pairing_partial() {
    // 4200 candidates after the real one: some were never looked at, so the pairing cannot be shown unique.
    assert_eq!(split_with_fakes(4200, 4096, false).health, Health::Partial);
    // The same layout under the caps stays Intact.
    assert_eq!(split_with_fakes(1000, 4096, false).health, Health::Intact);
}

#[test]
fn an_exhausted_load_budget_makes_a_pairing_partial() {
    // 1100 candidates the pre-filter keeps, each charged a 1 MiB cache window: the 1 GiB budget runs out after the real
    // one was loaded.
    assert_eq!(split_with_fakes(1100, 4096, true).health, Health::Partial);
    assert_eq!(split_with_fakes(900, 4096, true).health, Health::Intact);
}

/// A tiny but valid-looking `moov` listing 4,000,000 one-byte H.264 samples in one chunk at file offset 28: about 300
/// bytes on disk, about 64 MiB once listed.
fn huge_listing_moov() -> Vec<u8> {
    fn bx(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        out
    }
    let words = |w: &[u32]| w.iter().flat_map(|x| x.to_be_bytes()).collect::<Vec<u8>>();
    let mut avc1 = vec![0u8; 78];
    avc1.extend(bx(b"avcC", &[1, 0x64, 0, 0x28, 0xFF, 0xE0, 0]));
    let stsd = [words(&[0, 1]), bx(b"avc1", &avc1)].concat();
    let stbl = [
        bx(b"stsd", &stsd),
        bx(b"stsz", &words(&[0, 1, 4_000_000])),
        bx(b"stsc", &words(&[0, 1, 1, 4_000_000, 1])),
        bx(b"stco", &words(&[0, 1, 28])),
    ]
    .concat();
    let mut hdlr = vec![0u8; 8];
    hdlr.extend_from_slice(b"vide");
    hdlr.extend_from_slice(&[0; 13]);
    let mdia = bx(
        b"mdia",
        &[bx(b"hdlr", &hdlr), bx(b"minf", &bx(b"stbl", &stbl))].concat(),
    );
    bx(
        b"moov",
        &[bx(b"mvhd", &[0; 100]), bx(b"trak", &mdia)].concat(),
    )
}

#[test]
fn moovs_listing_millions_of_samples_are_bounded() {
    // A moov-less video whose mdat declares 64 MiB (so 4,000,000 one-byte samples fit before its P), and 1100 tiny
    // moovs that pass the arithmetic filter for it, each listing 4,000,000 samples: only 16,000,000 samples per scan are
    // listed.
    let v = video(1, true, 0);
    let l1 = cut(&v, 2);
    let mut head = v.bytes[..l1].to_vec();
    head[20..24].copy_from_slice(&(64u32 << 20).to_be_bytes());
    let p = 20 + (64 << 20);
    let moov = huge_listing_moov();
    assert!(moov.len() < 512);
    let mut img = DiskImage::noise(32 * MIB, 7);
    img.put(MIB, &head);
    for i in 0..1100 {
        img.put(8 * MIB + i * 512 + p % 512, &moov);
    }
    let file = img.save();
    let started = Instant::now();
    let (_, events) = run(&Src::new(&file));
    let elapsed = started.elapsed();
    assert!(updated(&events).is_empty());
    let bound = if cfg!(debug_assertions) { 60 } else { 5 };
    assert!(elapsed.as_secs() < bound, "{elapsed:?}");
}

#[test]
fn cancel_during_pairing_stops_without_updated() {
    let v = video(1, true, 0);
    let l1 = cut(&v, 2);
    let file = split_image(&v, l1).save();
    let cancel = CancelToken::default();
    let mut src = Src::new(&file);
    src.cancel = Some(cancel.clone());
    let total = src.len();
    let mut events = Vec::new();
    // A 1 MiB cache: the pairing reads reach the source, where the armed source cancels.
    let opts = ScanOptions {
        cache_bytes: 1 << 20,
        threads: 1,
        ..ScanOptions::default()
    };
    let r = scan(&src, &all_formats(), &opts, &cancel, &mut |e| {
        if e == (Event::Progress { read: total, total }) {
            src.arm.store(true, Ordering::SeqCst);
        }
        events.push(e);
    });
    assert!(src.arm.load(Ordering::SeqCst), "the scan reached its end");
    assert!(matches!(r, Err(EngineError::Cancelled)), "{r:?}");
    assert!(updated(&events).is_empty());
}

/// splitmix64, for the hostile test's offsets.
fn mix(x: &mut u64) -> u64 {
    *x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Offset of the `kind` box type inside `moov` bytes.
fn box_at(moov: &[u8], kind: &[u8; 4]) -> usize {
    moov.windows(4).position(|w| w == kind).unwrap()
}

#[test]
fn hostile_moov_candidates_are_bounded() {
    const LEN: usize = 32 * MIB;
    const BIG: u32 = 16 << 20;
    let v = video(1, true, 0);
    let l1 = cut(&v, 2);
    let p = v.moov.start;
    let mut img = DiskImage::noise(LEN, 11);
    // 10,000 fake headers `size "moov" 0000006C "mvhd"`: every 25th declares 16 MiB at an offset the arithmetic
    // pre-filter keeps (congruent to the video's P modulo 512), the rest 4 KiB or 16 MiB at seeded offsets.
    let mut seed = 5u64;
    for i in 0..10_000u32 {
        let (at, size) = if i % 25 == 0 {
            (
                (mix(&mut seed) as usize % (LEN / 512 - 1)) * 512 + p % 512,
                BIG,
            )
        } else {
            let size = if i % 2 == 0 { 4096 } else { BIG };
            (mix(&mut seed) as usize % (LEN - 16), size)
        };
        let mut h = size.to_be_bytes().to_vec();
        h.extend_from_slice(b"moov");
        h.extend_from_slice(&0x6Cu32.to_be_bytes());
        h.extend_from_slice(b"mvhd");
        img.put(at, &h);
    }
    // One Damaged moov-less video, and a real orphan moov (where its own would pair) whose stsz and stco counts are
    // 0xFFFF_FFFF with tables far too short for them.
    let mut rest = v.bytes[l1..].to_vec();
    let m = v.moov.start - l1;
    let stsz = m + box_at(&v.bytes[v.moov.clone()], b"stsz");
    rest[stsz + 12..stsz + 16].copy_from_slice(&[0xFF; 4]);
    let stco = m + box_at(&v.bytes[v.moov.clone()], b"stco");
    rest[stco + 8..stco + 12].copy_from_slice(&[0xFF; 4]);
    img.put(MIB, &v.bytes[..l1]).put(20 * MIB, &rest);
    let bytes = {
        let file = img.save();
        std::fs::read(file.path()).unwrap()
    };

    // Without the 1 GiB load budget the kept candidates would load far more than that.
    let mut candidates: Vec<usize> = (0..LEN - 16)
        .filter(|&q| {
            &bytes[q + 4..q + 8] == b"moov"
                && &bytes[q + 12..q + 16] == b"mvhd"
                && (24..=(16 << 20) + 8)
                    .contains(&u32::from_be_bytes(bytes[q..q + 4].try_into().unwrap()))
        })
        .collect();
    candidates.truncate(4096);
    let unbudgeted: u64 = candidates
        .iter()
        .filter(|&&q| q % 512 == p % 512)
        .map(|&q| {
            let size = u64::from(u32::from_be_bytes(bytes[q..q + 4].try_into().unwrap()));
            if q as u64 + size <= LEN as u64 {
                size - 8
            } else {
                0
            }
        })
        .sum();
    assert!(
        unbudgeted > 3 << 29,
        "the test must exceed the budget: {unbudgeted}"
    );

    let file = img.save();
    let src = Src::new(&file);
    let opts = ScanOptions {
        cache_bytes: 1 << 20,
        ..ScanOptions::default()
    };
    let started = Instant::now();
    let (r, events) = run_with(&src, &opts, &CancelToken::default());
    let elapsed = started.elapsed();
    r.unwrap();
    assert!(updated(&events).is_empty(), "{:?}", updated(&events));
    assert_eq!(
        found(&events)
            .iter()
            .filter(|f| f.kind == FileKind::Mp4 && f.health == Health::Damaged)
            .count(),
        1
    );
    let read = src.bytes.load(Ordering::SeqCst);
    assert!(
        read <= (1 << 30) + 2 * LEN as u64,
        "inner bytes read: {read}"
    );
    assert!(elapsed.as_secs() < 10, "{elapsed:?}");
}
