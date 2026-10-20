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

#[test]
fn pairing_events_do_not_depend_on_threads_or_cache() {
    let v = video(1, true, 0);
    let l1 = cut(&v, 2);
    let file = split_image(&v, l1).save();
    let src = Src::new(&file);
    let base = run_with(&src, &ScanOptions::default(), &CancelToken::default());
    assert_eq!(updated(&base.1).len(), 1);
    for (threads, cache) in [(1, 1 << 20), (4, 3 << 20), (16, 256 << 20)] {
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
