use revenant_core::formats::{FileKind, Format, Health, IsoBmff, Jpeg, Measurement, all_formats};
use revenant_core::source::{FileSource, Source, VolumeId};
use revenant_core::{CancelToken, EngineError, Event, Fragment, ScanOptions, ScanSummary, scan};
use revenant_testkit as testkit;
use std::sync::atomic::{AtomicU64, Ordering};
use testkit::{DiskImage, Mp4Options, TestExif, TiffRawOptions, TiffVariant};

const MIB: usize = 1024 * 1024;

/// Wraps a source and fails every read that touches `[bad.0, bad.1)`.
struct FailingSource {
    inner: FileSource,
    bad: (u64, u64),
}

impl Source for FailingSource {
    fn len(&self) -> u64 {
        self.inner.len()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, EngineError> {
        if offset < self.bad.1 && offset + buf.len() as u64 > self.bad.0 {
            return Err(EngineError::Read {
                offset,
                cause: std::io::Error::other("injected"),
            });
        }
        self.inner.read_at(offset, buf)
    }

    fn volume(&self) -> Option<VolumeId> {
        None
    }
}

fn run(source: &dyn Source, opts: &ScanOptions) -> (Result<ScanSummary, EngineError>, Vec<Event>) {
    let mut events = Vec::new();
    let r = scan(
        source,
        &all_formats(),
        opts,
        &CancelToken::default(),
        &mut |e| events.push(e),
    );
    (r, events)
}

fn found(events: &[Event]) -> Vec<(u64, FileKind, Health)> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Found(f) => Some((f.start, f.kind, f.health)),
            _ => None,
        })
        .collect()
}

fn mp4_opts(mov: bool) -> Mp4Options {
    Mp4Options {
        mdat_len: 300_000,
        moov_at_end: true,
        mov,
        mdat_64bit: false,
    }
}

#[test]
fn finds_photos_and_videos_on_formatted_card() {
    let photo = testkit::jpeg(64, 64, 1, 2000);
    let video = testkit::mp4(&mp4_opts(false));
    let mov = testkit::mp4(&mp4_opts(true));
    let cut_src = testkit::jpeg(256, 256, 2, 2000);
    let cut = &cut_src[..cut_src.len() / 2];
    let after = testkit::jpeg(64, 64, 3, 2000);
    let after_at = (5 * MIB + cut.len()).div_ceil(512) * 512;

    let mut img = DiskImage::noise(16 * MIB, 99);
    img.put(0, &photo)
        .put(MIB, &video)
        .put(5 * MIB, cut)
        .put(after_at, &after)
        .put(9 * MIB, &mov);
    let file = img.save();
    let source = FileSource::open(file.path()).unwrap();

    let (r, events) = run(&source, &ScanOptions::default());
    assert_eq!(r.unwrap().found, 5);
    assert_eq!(
        found(&events),
        vec![
            (0, FileKind::Jpeg, Health::Intact),
            (MIB as u64, FileKind::Mp4, Health::Intact),
            (5 * MIB as u64, FileKind::Jpeg, Health::Partial),
            (after_at as u64, FileKind::Jpeg, Health::Intact),
            (9 * MIB as u64, FileKind::Mov, Health::Intact),
        ]
    );
    let ids: Vec<u64> = events
        .iter()
        .filter_map(|e| match e {
            Event::Found(f) => Some(f.id),
            _ => None,
        })
        .collect();
    assert_eq!(ids, vec![1, 2, 3, 4, 5]);
}

#[test]
fn jpeg_crossing_blocks() {
    let mut jpeg = testkit::jpeg(256, 256, 7, 300_000);
    // A complete small JPEG on a sector start, after the block boundary and inside the outer file's APP1 padding.
    let nested = testkit::jpeg(8, 8, 9, 0);
    let nested_at = 110 * 1024;
    jpeg[nested_at..nested_at + nested.len()].copy_from_slice(&nested);
    let start = MIB - 100 * 1024;
    assert!(start + nested_at > MIB);
    assert!(start.is_multiple_of(512) && start + jpeg.len() > MIB);
    let file = DiskImage::noise(3 * MIB, 5).put(start, &jpeg).save();
    let source = FileSource::open(file.path()).unwrap();
    let opts = ScanOptions {
        block: MIB,
        ..ScanOptions::default()
    };
    let (r, events) = run(&source, &opts);
    assert_eq!(r.unwrap().found, 1);
    match &events
        .iter()
        .find(|e| matches!(e, Event::Found(_)))
        .unwrap()
    {
        Event::Found(f) => {
            assert_eq!(f.start, start as u64);
            assert_eq!(f.len, jpeg.len() as u64);
            assert_eq!(f.health, Health::Intact);
        }
        _ => unreachable!(),
    }
}

#[test]
fn unreadable_sector_does_not_stop_scan() {
    let a = testkit::jpeg(64, 64, 1, 2000);
    let b = testkit::jpeg(64, 64, 2, 2000);
    let file = DiskImage::noise(4 * MIB, 11)
        .put(0, &a)
        .put(3 * MIB, &b)
        .save();
    let source = FailingSource {
        inner: FileSource::open(file.path()).unwrap(),
        bad: (2 * MIB as u64, 2 * MIB as u64 + 4096),
    };
    let (r, events) = run(&source, &ScanOptions::default());
    let summary = r.unwrap();
    assert_eq!(summary.found, 2);
    assert_eq!(summary.unreadable_bytes, 4096);
    assert_eq!(
        found(&events),
        vec![
            (0, FileKind::Jpeg, Health::Intact),
            (3 * MIB as u64, FileKind::Jpeg, Health::Intact)
        ]
    );
    let unreadable: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, Event::Unreadable { .. }))
        .collect();
    assert_eq!(
        unreadable,
        vec![&Event::Unreadable {
            start: 2 * MIB as u64,
            len: 4096
        }]
    );
}

#[test]
fn unreadable_run_spanning_blocks_is_one_event() {
    let file = DiskImage::noise(3 * MIB, 3).save();
    let source = FailingSource {
        inner: FileSource::open(file.path()).unwrap(),
        bad: (MIB as u64 - 1024, MIB as u64 + 1024),
    };
    let opts = ScanOptions {
        block: MIB,
        ..ScanOptions::default()
    };
    let (r, events) = run(&source, &opts);
    assert_eq!(r.unwrap().unreadable_bytes, 2048);
    let unreadable: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, Event::Unreadable { .. }))
        .collect();
    assert_eq!(
        unreadable,
        vec![&Event::Unreadable {
            start: MIB as u64 - 1024,
            len: 2048
        }]
    );
}

#[test]
fn small_and_unaligned_images() {
    let empty = DiskImage::zeros(0).save();
    let (r, _) = run(
        &FileSource::open(empty.path()).unwrap(),
        &ScanOptions::default(),
    );
    assert_eq!(r.unwrap().found, 0);

    let tiny = DiskImage::noise(100, 1).save();
    let (r, _) = run(
        &FileSource::open(tiny.path()).unwrap(),
        &ScanOptions::default(),
    );
    assert_eq!(r.unwrap().found, 0);

    let jpeg = testkit::jpeg(8, 8, 4, 0);
    assert!(jpeg.len() <= 784, "jpeg is {} bytes", jpeg.len());
    // Ends exactly at the last byte; the image is not a multiple of 512.
    let len = 9216 + jpeg.len();
    assert!(!len.is_multiple_of(512));
    let file = DiskImage::noise(len, 8).put(9216, &jpeg).save();
    let (r, events) = run(
        &FileSource::open(file.path()).unwrap(),
        &ScanOptions::default(),
    );
    assert_eq!(r.unwrap().found, 1);
    assert_eq!(found(&events), vec![(9216, FileKind::Jpeg, Health::Intact)]);
}

#[test]
fn progress_ends_complete() {
    let file = DiskImage::noise(3 * MIB + 100, 2).save();
    let opts = ScanOptions {
        block: MIB,
        ..ScanOptions::default()
    };
    let (r, events) = run(&FileSource::open(file.path()).unwrap(), &opts);
    r.unwrap();
    let progress: Vec<u64> = events
        .iter()
        .filter_map(|e| match e {
            Event::Progress { read, total } => {
                assert_eq!(*total, 3 * MIB as u64 + 100);
                Some(*read)
            }
            _ => None,
        })
        .collect();
    assert!(progress.len() >= 2);
    assert!(progress.windows(2).all(|w| w[0] <= w[1]));
    assert_eq!(*progress.last().unwrap(), 3 * MIB as u64 + 100);
}

#[test]
fn cancel_interrupts() {
    let file = DiskImage::noise(3 * MIB, 2).save();
    let source = FileSource::open(file.path()).unwrap();
    let opts = ScanOptions {
        block: MIB,
        ..ScanOptions::default()
    };
    let cancel = CancelToken::default();
    let token = cancel.clone();
    let mut events = 0;
    let r = scan(&source, &all_formats(), &opts, &cancel, &mut |e| {
        events += 1;
        if matches!(e, Event::Progress { .. }) {
            token.cancel();
        }
    });
    assert_eq!(r.unwrap_err().code(), "cancelled");
    assert_eq!(events, 1);
}

#[test]
fn nested_signature_inside_a_file_is_not_reported() {
    let mut outer = testkit::jpeg(256, 256, 7, 200_000);
    let nested = testkit::jpeg(8, 8, 9, 0);
    outer[512 * 20..512 * 20 + nested.len()].copy_from_slice(&nested);
    let file = DiskImage::noise(2 * MIB, 5).put(MIB / 2, &outer).save();
    let (r, events) = run(
        &FileSource::open(file.path()).unwrap(),
        &ScanOptions::default(),
    );
    assert_eq!(r.unwrap().found, 1);
    assert_eq!(
        found(&events),
        vec![(MIB as u64 / 2, FileKind::Jpeg, Health::Intact)]
    );
}

#[test]
fn unreadable_sector_inside_a_file_makes_it_partial() {
    let video = testkit::mp4(&Mp4Options {
        mdat_len: 200_000,
        moov_at_end: true,
        mov: false,
        mdat_64bit: false,
    });
    let start = MIB;
    let bad = (start + 100 * 512) as u64;
    let file = DiskImage::noise(2 * MIB, 6).put(start, &video).save();
    // Sanity: without the failure the file is Intact.
    let (_, events) = run(
        &FileSource::open(file.path()).unwrap(),
        &ScanOptions::default(),
    );
    assert_eq!(
        found(&events),
        vec![(start as u64, FileKind::Mp4, Health::Intact)]
    );

    let source = FailingSource {
        inner: FileSource::open(file.path()).unwrap(),
        bad: (bad, bad + 512),
    };
    let opts = ScanOptions {
        block: 64 * 1024,
        ..ScanOptions::default()
    };
    let (r, events) = run(&source, &opts);
    assert_eq!(r.unwrap().unreadable_bytes, 512);
    let relevant: Vec<&Event> = events
        .iter()
        .filter(|e| !matches!(e, Event::Progress { .. }))
        .collect();
    assert_eq!(relevant.len(), 2, "{relevant:?}");
    assert_eq!(
        relevant[0],
        &Event::Unreadable {
            start: bad,
            len: 512
        }
    );
    match relevant[1] {
        Event::Found(f) => {
            assert_eq!(
                (f.start, f.kind, f.health),
                (start as u64, FileKind::Mp4, Health::Partial)
            );
            assert_eq!(f.len, video.len() as u64);
        }
        other => panic!("{other:?}"),
    }
}

fn relevant(events: &[Event]) -> Vec<&Event> {
    events
        .iter()
        .filter(|e| !matches!(e, Event::Progress { .. }))
        .collect()
}

#[test]
fn bad_sector_inside_a_jpeg_keeps_it() {
    let jpeg = testkit::jpeg(640, 480, 21, 0);
    let start = 4096;
    let bad = (start + jpeg.len() / 2) / 512 * 512;
    let file = DiskImage::noise(2 * MIB, 22).put(start, &jpeg).save();
    let source = FailingSource {
        inner: FileSource::open(file.path()).unwrap(),
        bad: (bad as u64, bad as u64 + 512),
    };
    let (r, events) = run(&source, &ScanOptions::default());
    assert_eq!(r.unwrap().found, 1, "{:?}", relevant(&events));
    let relevant = relevant(&events);
    assert_eq!(
        relevant[0],
        &Event::Unreadable {
            start: bad as u64,
            len: 512
        }
    );
    match relevant[1] {
        Event::Found(f) => {
            assert_eq!((f.start, f.kind), (start as u64, FileKind::Jpeg));
            assert_eq!(f.len, jpeg.len() as u64);
            assert_ne!(f.health, Health::Intact);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn bad_sector_shortly_after_a_jpeg_does_not_hide_it() {
    let jpeg = testkit::jpeg(64, 64, 23, 2000);
    let bad = 500 * 1024u64;
    let file = DiskImage::noise(2 * MIB, 24).put(0, &jpeg).save();
    let source = FailingSource {
        inner: FileSource::open(file.path()).unwrap(),
        bad: (bad, bad + 512),
    };
    let (r, events) = run(&source, &ScanOptions::default());
    assert_eq!(r.unwrap().unreadable_bytes, 512);
    assert_eq!(found(&events), vec![(0, FileKind::Jpeg, Health::Intact)]);
}

#[test]
fn bad_sector_in_moov_is_damaged_and_keeps_moov() {
    // moov-at-end video whose moov header sits at byte 400 of a sector, so the payload runs into the next sector.
    let video = testkit::mp4(&Mp4Options {
        mdat_len: 200_052,
        moov_at_end: true,
        mov: false,
        mdat_64bit: false,
    });
    let moov_at = 28 + 200_052;
    assert_eq!(moov_at % 512, 400);
    let start = MIB;
    let bad = (start + moov_at - 400 + 512) as u64;
    assert!(bad < (start + video.len()) as u64);
    let file = DiskImage::noise(2 * MIB, 25).put(start, &video).save();
    let source = FailingSource {
        inner: FileSource::open(file.path()).unwrap(),
        bad: (bad, bad + 512),
    };
    let (r, events) = run(&source, &ScanOptions::default());
    r.unwrap();
    let findings: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::Found(f) => Some((f.start, f.len, f.kind, f.health)),
            _ => None,
        })
        .collect();
    assert_eq!(
        findings,
        vec![(
            start as u64,
            video.len() as u64,
            FileKind::Mp4,
            Health::Damaged
        )]
    );
}

#[test]
fn bad_sector_in_jpeg_header_keeps_it() {
    let jpeg = testkit::jpeg(640, 480, 26, 0);
    let start = 4096u64;
    // The second sector of the file holds the end of the header segments (DHT, SOS).
    let sos = jpeg.windows(2).position(|w| w == [0xFF, 0xDA]).unwrap();
    assert!((512..1024).contains(&sos), "SOS at {sos}");
    let file = DiskImage::noise(2 * MIB, 27)
        .put(start as usize, &jpeg)
        .save();
    let source = FailingSource {
        inner: FileSource::open(file.path()).unwrap(),
        bad: (start + 512, start + 1024),
    };
    let m = Jpeg.measure(&source, start).expect("still a JPEG");
    assert_eq!(m.len, jpeg.len() as u64);
    assert_ne!(m.health, Health::Intact);
}

#[test]
fn bad_sector_in_mdat_is_partial() {
    let video = testkit::mp4(&Mp4Options {
        mdat_len: 200_000,
        moov_at_end: false,
        mov: false,
        mdat_64bit: false,
    });
    let start = MIB as u64;
    let bad = start + 100 * 512;
    let file = DiskImage::noise(2 * MIB, 28).put(MIB, &video).save();
    let source = FailingSource {
        inner: FileSource::open(file.path()).unwrap(),
        bad: (bad, bad + 512),
    };
    let m = IsoBmff.measure(&source, start).unwrap();
    assert_eq!((m.len, m.health), (video.len() as u64, Health::Partial));
}

fn findings(events: &[Event]) -> Vec<(u64, u64, FileKind, Health)> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Found(f) => Some((f.start, f.len, f.kind, f.health)),
            _ => None,
        })
        .collect()
}

fn scan_image(img: &DiskImage) -> Vec<Event> {
    let file = img.save();
    let (r, events) = run(
        &FileSource::open(file.path()).unwrap(),
        &ScanOptions::default(),
    );
    r.unwrap();
    events
}

#[test]
fn size_zero_mdat_ends_at_the_next_file() {
    // Camera battery died mid-recording: ftyp, then an mdat whose size was never written (0), no moov.
    let mut video = testkit::mp4(&mp4_opts(false))[..20].to_vec();
    video.extend_from_slice(&[0, 0, 0, 0]);
    video.extend_from_slice(b"mdat");
    let a = testkit::jpeg(64, 64, 31, 2000);
    let b = testkit::jpeg(64, 64, 32, 2000);
    let mut img = DiskImage::noise(16 * MIB, 33);
    img.put(MIB, &video).put(3 * MIB, &a).put(9 * MIB, &b);
    let events = scan_image(&img);
    assert_eq!(
        findings(&events),
        vec![
            (MIB as u64, 2 * MIB as u64, FileKind::Mp4, Health::Damaged),
            (
                3 * MIB as u64,
                a.len() as u64,
                FileKind::Jpeg,
                Health::Intact
            ),
            (
                9 * MIB as u64,
                b.len() as u64,
                FileKind::Jpeg,
                Health::Intact
            ),
        ]
    );
}

#[test]
fn mdat_declared_larger_than_what_survives_ends_at_the_next_file() {
    let mut video = testkit::mp4(&Mp4Options {
        moov_at_end: false,
        ..mp4_opts(false)
    });
    let mdat = video.windows(4).position(|w| w == b"mdat").unwrap() - 4;
    video[mdat..mdat + 4].copy_from_slice(&(8 * MIB as u32).to_be_bytes());
    let a = testkit::jpeg(64, 64, 34, 2000);
    let b = testkit::jpeg(64, 64, 35, 2000);
    let mut img = DiskImage::noise(16 * MIB, 36);
    img.put(MIB, &video).put(3 * MIB, &a).put(5 * MIB, &b);
    let events = scan_image(&img);
    assert_eq!(
        findings(&events),
        vec![
            (MIB as u64, 2 * MIB as u64, FileKind::Mp4, Health::Partial),
            (
                3 * MIB as u64,
                a.len() as u64,
                FileKind::Jpeg,
                Health::Intact
            ),
            (
                5 * MIB as u64,
                b.len() as u64,
                FileKind::Jpeg,
                Health::Intact
            ),
        ]
    );
}

#[test]
fn photo_written_over_a_video_mdat_is_found() {
    let video = testkit::mp4(&Mp4Options {
        mdat_len: MIB,
        moov_at_end: false,
        mov: false,
        mdat_64bit: false,
    });
    let photo = testkit::jpeg(64, 64, 37, 2000);
    let photo_at = MIB + 1000 * 512;
    let mut img = DiskImage::noise(4 * MIB, 38);
    img.put(MIB, &video).put(photo_at, &photo);
    let events = scan_image(&img);
    assert_eq!(
        findings(&events),
        vec![
            (MIB as u64, 1000 * 512, FileKind::Mp4, Health::Partial),
            (
                photo_at as u64,
                photo.len() as u64,
                FileKind::Jpeg,
                Health::Intact
            ),
        ]
    );
}

#[test]
fn jpeg_like_bytes_off_sector_inside_mdat_do_not_split_a_video() {
    let mut video = testkit::mp4(&Mp4Options {
        moov_at_end: false,
        ..mp4_opts(false)
    });
    let photo = testkit::jpeg(8, 8, 39, 0);
    let start = MIB;
    // Inside the mdat payload, one byte after a sector start.
    let at = 200 * 512 + 1;
    video[at..at + photo.len()].copy_from_slice(&photo);
    let mut img = DiskImage::noise(2 * MIB, 40);
    img.put(start, &video);
    let events = scan_image(&img);
    assert_eq!(
        findings(&events),
        vec![(
            start as u64,
            video.len() as u64,
            FileKind::Mp4,
            Health::Intact
        )]
    );
}

#[test]
fn nested_file_inside_a_damaged_finding_is_reported() {
    let mut outer = testkit::jpeg(256, 256, 7, 200_000);
    let nested = testkit::jpeg(8, 8, 9, 0);
    let nested_at = 512 * 20; // inside the APP1 padding, on a sector start
    outer[nested_at..nested_at + nested.len()].copy_from_slice(&nested);
    // Corrupt the entropy-coded data (stray DHT markers) so no decoder accepts the outer file.
    let mid = outer.len() - 2000;
    for pair in outer[mid..mid + 64].as_chunks_mut::<2>().0 {
        *pair = [0xFF, 0xC4];
    }
    let start = MIB / 2;
    let mut img = DiskImage::noise(2 * MIB, 5);
    img.put(start, &outer);
    let events = scan_image(&img);
    let got = findings(&events);
    assert_eq!(got.len(), 2, "{got:?}");
    assert_eq!((got[0].0, got[0].1), (start as u64, outer.len() as u64));
    assert_ne!(got[0].3, Health::Intact);
    assert_eq!(
        got[1],
        (
            (start + nested_at) as u64,
            nested.len() as u64,
            FileKind::Jpeg,
            Health::Intact
        )
    );
    let ids: Vec<u64> = events
        .iter()
        .filter_map(|e| match e {
            Event::Found(f) => Some(f.id),
            _ => None,
        })
        .collect();
    assert_eq!(ids, vec![1, 2]);
}

#[test]
fn random_head_inside_mdat_does_not_cut() {
    let video = {
        let mut v = testkit::mp4(&Mp4Options {
            mdat_len: 100_000,
            moov_at_end: false,
            ..mp4_opts(false)
        });
        // A JPEG-looking head (SOI + APP1) followed by bytes that form no valid segment chain, on a sector start.
        let at = 100 * 512;
        v[at..at + 4].copy_from_slice(&[0xFF, 0xD8, 0xFF, 0xE1]);
        v[at + 4..at + 16].copy_from_slice(&[0x5A; 12]);
        v
    };
    let mut img = DiskImage::noise(2 * MIB, 41);
    img.put(MIB, &video);
    let events = scan_image(&img);
    assert_eq!(
        findings(&events),
        vec![(
            MIB as u64,
            video.len() as u64,
            FileKind::Mp4,
            Health::Intact
        )]
    );
}

#[test]
fn photo_over_motion_jpeg_mdat_is_still_found() {
    // Motion-JPEG video whose index lists one chunk; a real photo written at a sector start that is not a chunk.
    let photo = testkit::jpeg(64, 64, 42, 2000);
    let mut payload = vec![0x11u8; 484];
    payload.extend_from_slice(&photo);
    payload.resize(payload.len() + 3000, 0x22);
    let video = testkit::mp4_with(
        &Mp4Options {
            mov: true,
            ..mp4_opts(true)
        },
        b"mjpa",
        &payload,
        &[0],
    );
    let mut img = DiskImage::noise(2 * MIB, 43);
    img.put(MIB, &video);
    let events = scan_image(&img);
    assert_eq!(
        findings(&events),
        vec![
            (MIB as u64, 512, FileKind::Mov, Health::Damaged),
            (
                MIB as u64 + 512,
                photo.len() as u64,
                FileKind::Jpeg,
                Health::Intact
            ),
        ]
    );
}

#[test]
fn raw_photo_among_other_files_hides_its_own_preview_only() {
    let before = testkit::jpeg(64, 64, 21, 0);
    let raw = testkit::tiff_raw(&testkit::TiffRawOptions {
        variant: testkit::TiffVariant::Cr2,
        big_endian: false,
        raw_len: 200_000,
        seed: 22,
        preview: Some((160, 120)),
        preview_sector_aligned: true,
        exif: testkit::TestExif::default(),
    });
    let after = testkit::jpeg(64, 64, 23, 0);
    let raw_at = 2 * MIB;
    let after_at = (raw_at + raw.bytes.len()).div_ceil(512) * 512;
    let file = DiskImage::noise(4 * MIB, 24)
        .put(MIB, &before)
        .put(raw_at, &raw.bytes)
        .put(after_at, &after)
        .save();
    let source = FileSource::open(file.path()).unwrap();
    let (r, events) = run(&source, &ScanOptions::default());
    assert_eq!(r.unwrap().found, 3);
    assert_eq!(
        found(&events),
        vec![
            (MIB as u64, FileKind::Jpeg, Health::Intact),
            (raw_at as u64, FileKind::Cr2, Health::Intact),
            (after_at as u64, FileKind::Jpeg, Health::Intact),
        ]
    );
}

#[test]
fn stray_ftyp_does_not_cut_a_jpeg() {
    let jpeg = testkit::jpeg(640, 480, 31, 0);
    let at = 4096usize;
    // A sector start inside the scan data, past the headers.
    let stray = (at + jpeg.len() / 2).next_multiple_of(512);
    let mut ftyp = 20u32.to_be_bytes().to_vec();
    ftyp.extend_from_slice(b"ftypisom\0\0\0\0isom");
    let mut free = 8u32.to_be_bytes().to_vec();
    free.extend_from_slice(b"free");
    let mut img = DiskImage::zeros(at + jpeg.len() + 8192);
    img.put(at, &jpeg).put(stray, &ftyp).put(stray + 20, &free);
    let img = img.save();
    let (r, events) = run(
        &FileSource::open(img.path()).unwrap(),
        &ScanOptions::default(),
    );
    r.unwrap();
    let files: Vec<(u64, u64)> = events
        .iter()
        .filter_map(|e| match e {
            Event::Found(f) => Some((f.start, f.len)),
            _ => None,
        })
        .collect();
    assert_eq!(files, vec![(at as u64, jpeg.len() as u64)]);
}

/// Test-only format: a sector starting with `PIECES!!` measures as `len` bytes made of `pieces`, whatever they are.
struct Pieces {
    len: u64,
    pieces: Vec<Fragment>,
}

impl Format for Pieces {
    fn name(&self) -> &'static str {
        "pieces"
    }

    fn recognizes(&self, head: &[u8]) -> bool {
        head.starts_with(b"PIECES!!")
    }

    fn confirms(&self, _source: &dyn Source, _start: u64) -> bool {
        true
    }

    fn measure(&self, _source: &dyn Source, _start: u64) -> Option<Measurement> {
        Some(Measurement {
            len: self.len,
            kind: FileKind::Jpeg,
            health: Health::Partial,
            fragments: self.pieces.clone(),
        })
    }
}

/// Scans a 64 KiB zero image with `PIECES!!` at 4096 and returns the findings.
fn scan_pieces(len: u64, pieces: &[(u64, u64)]) -> Vec<revenant_core::Finding> {
    let mut img = DiskImage::zeros(64 * 1024);
    img.put(4096, b"PIECES!!");
    let file = img.save();
    let source = FileSource::open(file.path()).unwrap();
    let formats: Vec<Box<dyn Format>> = vec![Box::new(Pieces {
        len,
        pieces: pieces
            .iter()
            .map(|&(start, len)| Fragment { start, len })
            .collect(),
    })];
    let mut out = Vec::new();
    scan(
        &source,
        &formats,
        &ScanOptions::default(),
        &CancelToken::default(),
        &mut |e| {
            if let Event::Found(f) = e {
                out.push(f);
            }
        },
    )
    .unwrap();
    out
}

#[test]
fn a_finding_carries_its_measured_pieces() {
    let found = scan_pieces(1500, &[(4096, 1000), (20_480, 500)]);
    assert_eq!(found.len(), 1);
    assert_eq!((found[0].start, found[0].len), (4096, 1500));
    assert_eq!(
        found[0].fragments,
        vec![
            Fragment {
                start: 4096,
                len: 1000
            },
            Fragment {
                start: 20_480,
                len: 500
            },
        ]
    );
    // Contiguous: no pieces.
    assert!(scan_pieces(1500, &[])[0].fragments.is_empty());
}

/// Measurements that break the pieces contract: a single piece, the first piece elsewhere, pieces not summing to `len`,
/// overlapping pieces, a piece past the source.
const BROKEN_PIECES: [(u64, &[(u64, u64)]); 5] = [
    (1000, &[(4096, 1000)]),
    (1500, &[(4608, 1000), (20_480, 500)]),
    (1600, &[(4096, 1000), (20_480, 500)]),
    (1500, &[(4096, 1000), (4596, 500)]),
    (1500, &[(4096, 1000), (65_400, 500)]),
];

#[cfg(not(debug_assertions))]
#[test]
fn measurements_breaking_the_pieces_contract_are_ignored() {
    for (len, pieces) in BROKEN_PIECES {
        assert!(scan_pieces(len, pieces).is_empty(), "{len} {pieces:?}");
    }
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "pieces contract")]
fn a_single_piece_breaks_the_pieces_contract() {
    let (len, pieces) = BROKEN_PIECES[0];
    scan_pieces(len, pieces);
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "pieces contract")]
fn a_first_piece_elsewhere_breaks_the_pieces_contract() {
    let (len, pieces) = BROKEN_PIECES[1];
    scan_pieces(len, pieces);
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "pieces contract")]
fn pieces_not_summing_to_len_break_the_pieces_contract() {
    let (len, pieces) = BROKEN_PIECES[2];
    scan_pieces(len, pieces);
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "pieces contract")]
fn overlapping_pieces_break_the_pieces_contract() {
    let (len, pieces) = BROKEN_PIECES[3];
    scan_pieces(len, pieces);
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "pieces contract")]
fn a_piece_past_the_source_breaks_the_pieces_contract() {
    let (len, pieces) = BROKEN_PIECES[4];
    scan_pieces(len, pieces);
}

/// Wraps a source, fails every read that touches any of the `bad` ranges (`[start, end)`) and counts what reaches it.
struct MultiFailingSource {
    inner: FileSource,
    bad: Vec<(u64, u64)>,
    /// Bytes returned by successful reads.
    bytes: AtomicU64,
    /// Read attempts that touched a bad range.
    bad_attempts: AtomicU64,
}

impl MultiFailingSource {
    fn new(path: &std::path::Path, bad: Vec<(u64, u64)>) -> Self {
        Self {
            inner: FileSource::open(path).unwrap(),
            bad,
            bytes: AtomicU64::new(0),
            bad_attempts: AtomicU64::new(0),
        }
    }

    fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::SeqCst)
    }

    fn bad_attempts(&self) -> u64 {
        self.bad_attempts.load(Ordering::SeqCst)
    }
}

impl Source for MultiFailingSource {
    fn len(&self) -> u64 {
        self.inner.len()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, EngineError> {
        let end = offset + buf.len() as u64;
        if self.bad.iter().any(|&(s, e)| offset < e && end > s) {
            self.bad_attempts.fetch_add(1, Ordering::SeqCst);
            return Err(EngineError::Read {
                offset,
                cause: std::io::Error::other("injected"),
            });
        }
        let n = self.inner.read_at(offset, buf)?;
        self.bytes.fetch_add(n as u64, Ordering::SeqCst);
        Ok(n)
    }

    fn volume(&self) -> Option<VolumeId> {
        None
    }
}

/// A RAW with a JPEG preview (about 2 MiB).
fn busy_raw() -> Vec<u8> {
    testkit::tiff_raw(&TiffRawOptions {
        variant: TiffVariant::Arw,
        big_endian: false,
        raw_len: 2 * MIB,
        seed: 31,
        preview: Some((160, 120)),
        preview_sector_aligned: false,
        exif: TestExif::default(),
    })
    .bytes
}

/// 32 MiB of noise holding 6 JPEGs (at 0, 3, .., 15 MiB), a RAW at 18 MiB and an MP4 at 22 MiB (6 MiB `mdat`).
fn busy_image() -> (DiskImage, Vec<u8>) {
    let mut img = DiskImage::noise(32 * MIB, 41);
    let first = testkit::jpeg(640, 480, 42, 0);
    img.put(0, &first);
    for i in 1..6 {
        img.put(3 * i * MIB, &testkit::jpeg(640, 480, 42 + i as u64, 0));
    }
    img.put(18 * MIB, &busy_raw());
    img.put(
        22 * MIB,
        &testkit::mp4(&Mp4Options {
            mdat_len: 6 * MIB,
            moov_at_end: true,
            mov: false,
            mdat_64bit: false,
        }),
    );
    (img, first)
}

#[test]
fn each_byte_is_read_once() {
    let (img, _) = busy_image();
    let file = img.save();
    let source = MultiFailingSource::new(file.path(), Vec::new());
    let (r, events) = run(&source, &ScanOptions::default());
    assert_eq!(r.unwrap().found, 8, "{:?}", relevant(&events));
    let read = source.bytes();
    assert!(read <= 33 * MIB as u64, "inner bytes read: {read}");
}

#[test]
fn big_intact_video_is_read_once() {
    let video = testkit::mp4(&Mp4Options {
        mdat_len: 40 * MIB,
        moov_at_end: true,
        mov: false,
        mdat_64bit: false,
    });
    let file = DiskImage::noise(48 * MIB, 43).put(MIB, &video).save();
    let source = MultiFailingSource::new(file.path(), Vec::new());
    let opts = ScanOptions {
        cache_bytes: 4 * MIB,
        block: MIB,
        ..ScanOptions::default()
    };
    let (r, events) = run(&source, &opts);
    r.unwrap();
    assert_eq!(
        findings(&events),
        vec![(
            MIB as u64,
            video.len() as u64,
            FileKind::Mp4,
            Health::Intact
        )]
    );
    // The read-ahead (2 blocks here) fits in the cache, so at most it and the block being examined are read twice. The
    // file-system step before the scan looks for a volume at sector 0 and the 7 probe offsets, loading their cache
    // windows (5 distinct ones: 0, 1, 4, 16 and 32 MiB), which a 4 MiB cache may evict before the scan reaches them.
    let read = source.bytes();
    let bound = (48 + (opts.lookahead_blocks() as u64 + 1) + 5) * MIB as u64;
    eprintln!("inner bytes read: {read} (bound {bound})");
    assert!(read <= bound, "inner bytes read: {read}");
}

#[test]
fn bad_sector_is_read_once_across_measures() {
    // A RAW, a JPEG and a video in the same 1 MiB, with one unreadable sector inside the JPEG: the RAW's and the JPEG's
    // measures and the scan's own block read all load the window holding it.
    let raw = testkit::tiff_raw(&TiffRawOptions {
        variant: TiffVariant::Arw,
        big_endian: false,
        raw_len: 100_000,
        seed: 32,
        preview: Some((160, 120)),
        preview_sector_aligned: false,
        exif: TestExif::default(),
    })
    .bytes;
    let jpeg = testkit::jpeg(320, 240, 33, 0);
    let video = testkit::mp4(&mp4_opts(false));
    let jpeg_at = 256 * 1024;
    let video_at = 768 * 1024;
    assert!(raw.len() < jpeg_at && jpeg_at + jpeg.len() < video_at);
    let bad = ((jpeg_at + jpeg.len() / 2) / 512 * 512) as u64;
    let file = DiskImage::noise(4 * MIB, 34)
        .put(0, &raw)
        .put(jpeg_at, &jpeg)
        .put(video_at, &video)
        .save();
    let source = MultiFailingSource::new(file.path(), vec![(bad, bad + 512)]);
    let (r, events) = run(&source, &ScanOptions::default());
    assert_eq!(r.unwrap().found, 3, "{:?}", relevant(&events));
    let attempts = source.bad_attempts();
    assert!(
        attempts <= 3,
        "attempts touching the bad sector: {attempts}"
    );
}

#[test]
fn cache_size_does_not_change_events() {
    let (img, first) = busy_image();
    let file = img.save();
    let in_jpeg = ((3 * MIB + first.len() / 2) / 512 * 512) as u64;
    let in_mdat = (25 * MIB) as u64;
    let between = (10 * MIB + MIB / 2) as u64;
    let bad = vec![
        (in_jpeg, in_jpeg + 512),
        (in_mdat, in_mdat + 4096),
        (between, between + 1024),
    ];
    let small = MultiFailingSource::new(file.path(), bad.clone());
    let (r_small, small_events) = run(
        &small,
        &ScanOptions {
            cache_bytes: MIB,
            ..ScanOptions::default()
        },
    );
    let default = MultiFailingSource::new(file.path(), bad);
    let (r_default, default_events) = run(&default, &ScanOptions::default());
    assert_eq!(r_small.unwrap(), r_default.unwrap());
    assert_eq!(small_events, default_events);
    assert_eq!(
        small_events
            .iter()
            .filter(|e| matches!(e, Event::Unreadable { .. }))
            .count(),
        3
    );
}

/// Test-only format: a sector starting with `READALL!` is a `len`-byte `Intact` file whose measure reads all of it
/// (ignoring read errors), as a detector that walks a file to its end does.
struct ReadAll {
    len: u64,
}

impl Format for ReadAll {
    fn name(&self) -> &'static str {
        "read-all"
    }

    fn recognizes(&self, head: &[u8]) -> bool {
        head.starts_with(b"READALL!")
    }

    fn confirms(&self, _source: &dyn Source, _start: u64) -> bool {
        true
    }

    fn measure(&self, source: &dyn Source, start: u64) -> Option<Measurement> {
        let mut buf = vec![0u8; 64 * 1024];
        for at in (start..start + self.len).step_by(buf.len()) {
            let _ = source.read_at(at, &mut buf);
        }
        Some(Measurement {
            len: self.len,
            kind: FileKind::Jpeg,
            health: Health::Intact,
            fragments: Vec::new(),
        })
    }
}

#[test]
fn block_not_read_again_still_reports_its_unreadable_sectors() {
    let bad = 2 * MIB as u64 + 3 * 512;
    let mut img = DiskImage::zeros(4 * MIB);
    img.put(0, b"READALL!");
    let file = img.save();
    let source = MultiFailingSource::new(file.path(), vec![(bad, bad + 1024)]);
    let formats: Vec<Box<dyn Format>> = vec![Box::new(ReadAll {
        len: 3 * MIB as u64,
    })];
    let opts = ScanOptions {
        block: 64 * 1024,
        ..ScanOptions::default()
    };
    let mut events = Vec::new();
    scan(
        &source,
        &formats,
        &opts,
        &CancelToken::default(),
        &mut |e| events.push(e),
    )
    .unwrap();
    assert_eq!(
        relevant(&events),
        vec![
            &Event::Unreadable {
                start: bad,
                len: 1024
            },
            &Event::Found(revenant_core::Finding {
                id: 1,
                start: 0,
                len: 3 * MIB as u64,
                kind: FileKind::Jpeg,
                health: Health::Partial,
                fragments: Vec::new(),
                origin: None,
            }),
        ]
    );
    // Every sector went through the cache once: the scan did not read the measured file again.
    assert!(source.bytes() <= 4 * MIB as u64, "{}", source.bytes());
}

/// Scans with `formats` and `opts`; every event in order.
fn run_with(
    source: &dyn Source,
    formats: &[Box<dyn Format>],
    opts: &ScanOptions,
) -> (Result<ScanSummary, EngineError>, Vec<Event>) {
    let mut events = Vec::new();
    let r = scan(source, formats, opts, &CancelToken::default(), &mut |e| {
        events.push(e)
    });
    (r, events)
}

/// Scans with every format, cancelling from the callback when the `k`-th `Found` arrives.
fn run_cancelling_at(
    source: &dyn Source,
    opts: &ScanOptions,
    k: usize,
) -> (Result<ScanSummary, EngineError>, Vec<Event>) {
    let cancel = CancelToken::default();
    let token = cancel.clone();
    let mut events = Vec::new();
    let mut seen = 0;
    let r = scan(source, &all_formats(), opts, &cancel, &mut |e| {
        if matches!(e, Event::Found(_)) {
            seen += 1;
            if seen == k {
                token.cancel();
            }
        }
        events.push(e);
    });
    (r, events)
}

fn threads(n: usize) -> ScanOptions {
    ScanOptions {
        threads: n,
        ..ScanOptions::default()
    }
}

/// 48 MiB of noise (seed 42) holding every kind of file the scan finds, and three unreadable ranges: inside a JPEG,
/// inside an `mdat` and between files.
fn mixed_image() -> (DiskImage, Vec<(u64, u64)>) {
    let mut img = DiskImage::noise(48 * MIB, 42);
    let mut in_jpeg = 0;
    for i in 0..6 {
        let jpeg = testkit::jpeg(640, 480, 60 + i as u64, 0);
        assert!(jpeg.len() < 2 * MIB);
        img.put(2 * i * MIB, &jpeg);
        if i == 1 {
            in_jpeg = ((2 * MIB + jpeg.len() / 2) / 512 * 512) as u64;
        }
    }
    for i in 0..2 {
        let cut = testkit::jpeg(256, 256, 70 + i as u64, 2000);
        img.put((12 + 2 * i) * MIB, &cut[..cut.len() / 2]);
    }
    let video = testkit::mp4(&Mp4Options {
        mdat_len: 3 * MIB,
        moov_at_end: true,
        mov: false,
        mdat_64bit: false,
    });
    img.put(16 * MIB, &video);
    // Motion-JPEG MOV whose index lists one chunk, with a photo at a sector start that is not a chunk: a Damaged video
    // with a JPEG inside.
    let photo = testkit::jpeg(64, 64, 72, 2000);
    let mut payload = vec![0x11u8; 484];
    payload.extend_from_slice(&photo);
    payload.resize(payload.len() + 3000, 0x22);
    img.put(
        21 * MIB,
        &testkit::mp4_with(&mp4_opts(true), b"mjpa", &payload, &[0]),
    );
    let arw = testkit::tiff_raw(&TiffRawOptions {
        variant: TiffVariant::Arw,
        big_endian: false,
        raw_len: 2 * MIB,
        seed: 73,
        preview: Some((160, 120)),
        preview_sector_aligned: true,
        exif: TestExif::default(),
    });
    img.put(23 * MIB, &arw.bytes);
    img.put(27 * MIB, &testkit::raf(MIB, 74, &TestExif::default()));
    img.put(30 * MIB, &testkit::cr3(75, &TestExif::default()));
    img.put(33 * MIB, &testkit::heic(200_000, 76, &TestExif::default()));
    let in_mdat = 17 * MIB as u64;
    let between = 40 * MIB as u64 + 512;
    let bad = vec![
        (in_jpeg, in_jpeg + 512),
        (in_mdat, in_mdat + 4096),
        (between, between + 1024),
    ];
    (img, bad)
}

#[test]
fn parallel_scan_matches_sequential_events() {
    let (img, bad) = mixed_image();
    let file = img.save();
    let events_with = |opts: &ScanOptions| {
        let source = MultiFailingSource::new(file.path(), bad.clone());
        let (r, events) = run(&source, opts);
        r.unwrap();
        events
    };
    let sequential = events_with(&threads(1));
    let kinds: Vec<FileKind> = found(&sequential).iter().map(|f| f.1).collect();
    for kind in [
        FileKind::Mp4,
        FileKind::Mov,
        FileKind::Arw,
        FileKind::Raf,
        FileKind::Cr3,
        FileKind::Heic,
    ] {
        assert!(kinds.contains(&kind), "{kind:?} missing: {kinds:?}");
    }
    assert!(kinds.iter().filter(|&&k| k == FileKind::Jpeg).count() >= 9);
    assert_eq!(
        sequential
            .iter()
            .filter(|e| matches!(e, Event::Unreadable { .. }))
            .count(),
        3
    );
    for n in [2, 8, 16] {
        assert_eq!(events_with(&threads(n)), sequential, "threads {n}");
    }
    let small_cache = ScanOptions {
        cache_bytes: MIB,
        ..threads(8)
    };
    assert_eq!(events_with(&small_cache), sequential, "cache 1 MiB");
    for _ in 0..3 {
        assert_eq!(events_with(&threads(8)), sequential);
    }
}

#[test]
fn bench_images_give_the_same_events_for_any_thread_count() {
    for kind in testkit::BenchImage::ALL {
        let mut bytes = Vec::new();
        testkit::write_bench_image(kind, 16 * MIB as u64, &mut bytes).unwrap();
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), &bytes).unwrap();
        let bad = vec![
            (5 * MIB as u64 + 512, 5 * MIB as u64 + 1024),
            (11 * MIB as u64, 11 * MIB as u64 + 4096),
        ];
        let events_with = |opts: &ScanOptions, cancel_at: Option<usize>| {
            let source = MultiFailingSource::new(file.path(), bad.clone());
            match cancel_at {
                None => {
                    let (r, events) = run(&source, opts);
                    r.unwrap();
                    events
                }
                Some(k) => run_cancelling_at(&source, opts, k).1,
            }
        };
        let sequential = events_with(&threads(1), None);
        let cancelled = events_with(&threads(1), Some(3));
        for n in [2, 8] {
            assert_eq!(
                events_with(&threads(n), None),
                sequential,
                "{} threads {n}",
                kind.name()
            );
            assert_eq!(
                events_with(&threads(n), Some(3)),
                cancelled,
                "{} cancelled, threads {n}",
                kind.name()
            );
        }
    }
}

#[test]
fn lookahead_fits_the_cache() {
    let small = ScanOptions {
        cache_bytes: 2 * MIB,
        block: MIB,
        ..ScanOptions::default()
    };
    let big = ScanOptions {
        block: MIB,
        ..ScanOptions::default()
    };
    assert_eq!(small.lookahead_blocks(), 1);
    assert_eq!(big.lookahead_blocks(), 16);
    assert_eq!(ScanOptions::default().lookahead_blocks(), 16);
    let (img, bad) = mixed_image();
    let file = img.save();
    let events_with = |opts: &ScanOptions| {
        let source = MultiFailingSource::new(file.path(), bad.clone());
        let (r, events) = run(&source, opts);
        r.unwrap();
        events
    };
    assert_eq!(events_with(&small), events_with(&big));
}

#[test]
fn no_event_after_cancel() {
    let (img, _) = mixed_image();
    let file = img.save();
    let source = FileSource::open(file.path()).unwrap();
    let (r, events) = run_cancelling_at(&source, &threads(8), 1);
    assert_eq!(r.unwrap_err().code(), "cancelled");
    assert!(
        matches!(events.last(), Some(Event::Found(f)) if f.id == 1),
        "{events:?}"
    );
    assert_eq!(found(&events).len(), 1);
}

/// Wraps a source and sleeps for the given time in every read.
struct SlowSource(FileSource, std::time::Duration);

impl Source for SlowSource {
    fn len(&self) -> u64 {
        self.0.len()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, EngineError> {
        std::thread::sleep(self.1);
        self.0.read_at(offset, buf)
    }

    fn volume(&self) -> Option<VolumeId> {
        None
    }
}

#[test]
fn cancel_from_another_thread_stops_the_scan() {
    use std::time::{Duration, Instant};
    let file = DiskImage::noise(64 * MIB, 44).save();
    let source = SlowSource(
        FileSource::open(file.path()).unwrap(),
        Duration::from_millis(2),
    );
    let cancel = CancelToken::default();
    let cancelled_at = std::sync::Mutex::new(None);
    let r = std::thread::scope(|s| {
        s.spawn(|| {
            std::thread::sleep(Duration::from_millis(100));
            *cancelled_at.lock().unwrap() = Some(Instant::now());
            cancel.cancel();
        });
        scan(&source, &all_formats(), &threads(8), &cancel, &mut |_| {})
    });
    let returned = Instant::now();
    assert_eq!(r.unwrap_err().code(), "cancelled");
    let at = cancelled_at.lock().unwrap().expect("cancelled");
    assert!(
        returned - at < Duration::from_secs(2),
        "{:?}",
        returned - at
    );
}

#[test]
fn cancel_during_the_first_read_ahead_stops_the_scan() {
    use std::time::{Duration, Instant};
    // 16 blocks of 4 MiB read ahead at 30 ms per 1 MiB window would take about 2 s.
    let file = DiskImage::noise(64 * MIB, 45).save();
    let source = SlowSource(
        FileSource::open(file.path()).unwrap(),
        Duration::from_millis(30),
    );
    let cancel = CancelToken::default();
    let started = Instant::now();
    let r = std::thread::scope(|s| {
        s.spawn(|| {
            std::thread::sleep(Duration::from_millis(20));
            cancel.cancel();
        });
        scan(&source, &all_formats(), &threads(8), &cancel, &mut |_| {})
    });
    let took = started.elapsed();
    assert_eq!(r.unwrap_err().code(), "cancelled");
    // Generous for loaded CI runners; still half of what the read-ahead alone would take.
    assert!(took < Duration::from_secs(1), "{took:?}");
}

/// Test-only format: a sector starting with `TWOPIECES-HEAD!!` is a 2048-byte `Intact` file in two 1 KiB pieces, the
/// second 1 MiB after the first (the second piece is `second_len` bytes).
struct TwoPieces {
    second_len: u64,
}

impl Format for TwoPieces {
    fn name(&self) -> &'static str {
        "two-pieces"
    }

    fn recognizes(&self, head: &[u8]) -> bool {
        head.starts_with(b"TWOPIECES-HEAD!!")
    }

    fn confirms(&self, _source: &dyn Source, _start: u64) -> bool {
        true
    }

    fn measure(&self, _source: &dyn Source, start: u64) -> Option<Measurement> {
        Some(Measurement {
            len: 2048,
            kind: FileKind::Raf,
            health: Health::Intact,
            fragments: vec![
                Fragment { start, len: 1024 },
                Fragment {
                    start: start + MIB as u64,
                    len: self.second_len,
                },
            ],
        })
    }
}

/// A two-piece file at 4096 and three small JPEGs: inside piece 1, in the gap, inside piece 2. Scanned with `bad`.
fn scan_two_pieces(second_len: u64, bad: Vec<(u64, u64)>) -> (Vec<Event>, Vec<u8>) {
    let gap_jpeg = testkit::jpeg(64, 64, 81, 0);
    let mut img = DiskImage::zeros(3 * MIB);
    img.put(4096, b"TWOPIECES-HEAD!!")
        .put(4096 + 512, &testkit::jpeg(64, 64, 80, 0))
        .put(4096 + 64 * 1024, &gap_jpeg)
        .put(4096 + MIB + 512, &testkit::jpeg(64, 64, 82, 0));
    let file = img.save();
    let source = MultiFailingSource::new(file.path(), bad);
    let formats: Vec<Box<dyn Format>> = vec![Box::new(TwoPieces { second_len }), Box::new(Jpeg)];
    let (r, events) = run_with(&source, &formats, &ScanOptions::default());
    r.unwrap();
    (events, gap_jpeg)
}

#[test]
fn fragmented_measurements_skip_only_their_pieces() {
    let two = |health| {
        Event::Found(revenant_core::Finding {
            id: 1,
            start: 4096,
            len: 2048,
            kind: FileKind::Raf,
            health,
            fragments: vec![
                Fragment {
                    start: 4096,
                    len: 1024,
                },
                Fragment {
                    start: 4096 + MIB as u64,
                    len: 1024,
                },
            ],
            origin: None,
        })
    };
    let gap = |len: usize| {
        Event::Found(revenant_core::Finding {
            id: 2,
            start: 4096 + 64 * 1024,
            len: len as u64,
            kind: FileKind::Jpeg,
            health: Health::Intact,
            fragments: Vec::new(),
            origin: None,
        })
    };
    let (events, gap_jpeg) = scan_two_pieces(1024, Vec::new());
    assert_eq!(
        relevant(&events),
        vec![&two(Health::Intact), &gap(gap_jpeg.len())]
    );

    let in_piece = 4096 + MIB as u64 + 512;
    let (events, _) = scan_two_pieces(1024, vec![(in_piece, in_piece + 512)]);
    assert_eq!(
        relevant(&events),
        vec![
            &Event::Unreadable {
                start: in_piece,
                len: 512
            },
            &two(Health::Partial),
            &gap(gap_jpeg.len())
        ]
    );

    let in_gap = 4096 + 128 * 1024;
    let (events, _) = scan_two_pieces(1024, vec![(in_gap, in_gap + 512)]);
    assert_eq!(
        found(&events),
        vec![
            (4096, FileKind::Raf, Health::Intact),
            (4096 + 64 * 1024, FileKind::Jpeg, Health::Intact)
        ]
    );
}

#[cfg(not(debug_assertions))]
#[test]
fn inconsistent_fragments_are_ignored() {
    let (events, _) = scan_two_pieces(1023, Vec::new());
    assert!(found(&events).iter().all(|f| f.0 != 4096), "{events:?}");
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "pieces contract")]
fn inconsistent_fragments_break_the_pieces_contract() {
    scan_two_pieces(1023, Vec::new());
}

/// Test-only format: every sector start is a candidate whose `measure` panics.
struct Panics;

impl Format for Panics {
    fn name(&self) -> &'static str {
        "panics"
    }

    fn recognizes(&self, head: &[u8]) -> bool {
        head.starts_with(b"PANIC!!!")
    }

    fn confirms(&self, _source: &dyn Source, _start: u64) -> bool {
        true
    }

    fn measure(&self, _source: &dyn Source, _start: u64) -> Option<Measurement> {
        panic!("detector panicked");
    }
}

#[test]
#[should_panic(expected = "detector panicked")]
fn a_panicking_detector_panics_the_scan_without_hanging() {
    let mut img = DiskImage::zeros(16 * MIB);
    for at in (0..16 * MIB).step_by(MIB / 2) {
        img.put(at + 4096, b"PANIC!!!");
    }
    let file = img.save();
    let source = FileSource::open(file.path()).unwrap();
    let formats: Vec<Box<dyn Format>> = vec![Box::new(Panics)];
    let _ = run_with(&source, &formats, &threads(8));
}

#[test]
fn scan_reports_the_reassembled_jpeg_and_the_file_in_the_gap() {
    // As in tests/jpeg.rs: a smooth photo split at c, a small JPEG right after the first piece, the rest at 3 MiB.
    let a = testkit::jpeg_smooth(1024, 768, 1);
    let c = (a.len() / 2).next_multiple_of(512);
    let small = testkit::jpeg(64, 64, 9, 0);
    let start = 64 * 1024;
    let file = DiskImage::noise(8 * MIB, 5)
        .put(start, &a[..c])
        .put(start + c, &small)
        .put(3 * MIB, &a[c..])
        .save();
    let source = SlowSource(
        FileSource::open(file.path()).unwrap(),
        std::time::Duration::ZERO,
    );
    for n in [1, 8] {
        let (r, events) = run(&source, &threads(n));
        r.unwrap();
        let found: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Event::Found(f) => Some(f.clone()),
                _ => None,
            })
            .collect();
        let summary: Vec<_> = found
            .iter()
            .map(|f| (f.start, f.len, f.kind, f.health, f.fragments.clone()))
            .collect();
        assert_eq!(
            summary,
            vec![
                (
                    start as u64,
                    a.len() as u64,
                    FileKind::Jpeg,
                    Health::Intact,
                    vec![
                        Fragment {
                            start: start as u64,
                            len: c as u64
                        },
                        Fragment {
                            start: 3 * MIB as u64,
                            len: (a.len() - c) as u64
                        },
                    ]
                ),
                (
                    (start + c) as u64,
                    small.len() as u64,
                    FileKind::Jpeg,
                    Health::Intact,
                    vec![]
                ),
            ],
            "threads {n}"
        );
        let dir = tempfile::tempdir().unwrap();
        let path = revenant_core::extract(&source, &found[0], dir.path()).unwrap();
        assert!(std::fs::read(path).unwrap() == a, "byte-identical");
    }
}

#[test]
fn cancel_stops_a_jpeg_gap_search() {
    use std::time::{Duration, Instant};
    // A photo cut in half, then 64 MiB of sectors each ending in FF D9: its gap search reads the whole window and tries
    // MAX_JOIN_TRIES joins. At 20 ms per 1 MiB read that is well over a second.
    let a = testkit::jpeg(640, 480, 3, 0);
    let c = (a.len() / 2).next_multiple_of(512);
    let mut img = DiskImage::zeros(c + 64 * MIB);
    img.put(0, &a[..c]);
    let mut sector = [0u8; 512];
    sector[510..].copy_from_slice(&[0xFF, 0xD9]);
    for i in 0..(64 * MIB / 512) {
        img.put(c + i * 512, &sector);
    }
    let file = img.save();
    let source = SlowSource(
        FileSource::open(file.path()).unwrap(),
        Duration::from_millis(20),
    );
    let cancel = CancelToken::default();
    let cancelled_at = std::sync::Mutex::new(None);
    let opts = ScanOptions {
        threads: 2,
        // A small cache and read-ahead, so the scan does not read the window for the search first.
        cache_bytes: 8 * MIB,
        ..ScanOptions::default()
    };
    let r = std::thread::scope(|s| {
        s.spawn(|| {
            std::thread::sleep(Duration::from_millis(300));
            *cancelled_at.lock().unwrap() = Some(Instant::now());
            cancel.cancel();
        });
        scan(&source, &all_formats(), &opts, &cancel, &mut |_| {})
    });
    let returned = Instant::now();
    assert_eq!(r.unwrap_err().code(), "cancelled");
    let at = cancelled_at.lock().unwrap().expect("cancelled");
    assert!(
        returned - at < Duration::from_millis(700),
        "{:?}",
        returned - at
    );
}

/// A Panasonic RW2 whose packing (raw format 5 at 12 bits) the detector does not check: `Partial`, its raw data claimed
/// up to an upper bound. Its JpgFromRaw (the preview) starts at offset 512, a sector start.
fn unchecked_rw2() -> testkit::TiffRaw {
    testkit::panasonic_rw2(&testkit::Rw2Options {
        width: 280,
        height: 200,
        raw_format: 5,
        bits: 12,
        raw_len: 98_304,
        strip_len: 0,
        seed: 3,
        exif: TestExif::default(),
    })
}

fn scan_found(img: &DiskImage) -> Vec<(u64, FileKind, Health)> {
    let file = img.save();
    let (r, events) = run(
        &FileSource::open(file.path()).unwrap(),
        &ScanOptions::default(),
    );
    r.unwrap();
    found(&events)
}

/// A `Partial` RAW hides the JPEGs it references itself (ruling D12): its preview is no separate photo, and the preview,
/// measuring `Intact`, still skips its range, so the thumbnail inside it is none either (ruling R10). A foreign photo
/// inside the RAW's raw data is still found.
#[test]
fn partial_raw_hides_its_own_jpegs_only() {
    let f = unchecked_rw2();
    let p = f.preview.clone().unwrap();
    assert_eq!(p.start % 512, 0);
    let mut img = DiskImage::noise(8192 + f.bytes.len() + 32_768, 21);
    img.put(8192, &f.bytes);
    assert_eq!(
        scan_found(&img),
        vec![(8192, FileKind::Rw2, Health::Partial)]
    );

    let foreign = testkit::jpeg(32, 32, 5, 0);
    let at = 8192 + (f.raw.start + 8192).next_multiple_of(512);
    img.put(at, &foreign);
    assert_eq!(
        scan_found(&img),
        vec![
            (8192, FileKind::Rw2, Health::Partial),
            (at as u64, FileKind::Jpeg, Health::Intact)
        ]
    );
}

/// Overwrites the value of tag `tag` of the little-endian IFD at `ifd` with `value` (its type kept).
fn patch_le_entry(bytes: &mut [u8], ifd: usize, tag: u16, value: u32) {
    let n = u16::from_le_bytes([bytes[ifd], bytes[ifd + 1]]) as usize;
    let e = (0..n)
        .map(|i| ifd + 2 + 12 * i)
        .find(|&e| u16::from_le_bytes([bytes[e], bytes[e + 1]]) == tag)
        .expect("tag present");
    bytes[e + 8..e + 12].copy_from_slice(&value.to_le_bytes());
}

/// A `Damaged` RAW hides nothing (ruling D12): its sector-aligned preview is still a finding.
#[test]
fn damaged_raw_keeps_its_preview_as_a_finding() {
    let raw = testkit::tiff_raw(&TiffRawOptions {
        variant: TiffVariant::Arw,
        big_endian: false,
        raw_len: 120_000,
        seed: 7,
        preview: Some((160, 120)),
        preview_sector_aligned: true,
        exif: TestExif::default(),
    });
    let mut bytes = raw.bytes.clone();
    // The SubIFD pointer now points past the 1 GiB TIFF cap (`1 << 30`): a structural error.
    patch_le_entry(&mut bytes, 8, 0x014A, (1 << 30) + 0x1000);
    let p = raw.preview.clone().unwrap();
    assert_eq!(p.start % 512, 0);
    let mut img = DiskImage::noise(8192 + bytes.len() + 8192, 21);
    img.put(8192, &bytes);
    let found = scan_found(&img);
    assert_eq!(
        found[0],
        (8192, FileKind::Arw, Health::Damaged),
        "{found:?}"
    );
    assert!(
        found.contains(&((8192 + p.start) as u64, FileKind::Jpeg, Health::Intact)),
        "{found:?}"
    );
}

/// A CR3 whose `PRVW` is broken is `Partial`; its full-size JPEG, at a sector start in `mdat`, is its own (ruling D12).
#[test]
fn partial_cr3_hides_its_own_jpegs() {
    let mut v = testkit::cr3(7, &TestExif::default());
    let find = |needle: &[u8]| v.windows(needle.len()).position(|w| w == needle).unwrap();
    let prvw = find(b"PRVW");
    let full = find(b"mdat") + 4;
    assert_eq!(full % 512, 0);
    assert_eq!(&v[full..full + 3], &[0xFF, 0xD8, 0xFF]);
    let size = u32::from_be_bytes(v[prvw + 16..prvw + 20].try_into().unwrap()) as usize;
    let mid = prvw + 20 + size / 2;
    v[mid..mid + 64].copy_from_slice(&[0xFF, 0xC4].repeat(32));
    let mut img = DiskImage::noise(8192 + v.len() + 8192, 21);
    img.put(8192, &v);
    assert_eq!(
        scan_found(&img),
        vec![(8192, FileKind::Cr3, Health::Partial)]
    );
}

/// A foreign JPEG written at the start of a `Partial` RAW's own JpgFromRaw that runs past the RAW's end is not inside
/// the RAW: it is found (ruling D12 needs the file wholly inside its owner).
#[test]
fn foreign_jpeg_at_an_own_start_running_past_the_raw_is_found() {
    let f = unchecked_rw2();
    let p = f.preview.clone().unwrap();
    let foreign = testkit::jpeg(1024, 768, 9, 0);
    let mut img = DiskImage::noise(8192 + p.start + foreign.len() + 65_536, 21);
    img.put(8192, &f.bytes).put(8192 + p.start, &foreign);
    let found = scan_found(&img);
    assert_eq!(
        found[0],
        (8192, FileKind::Rw2, Health::Partial),
        "{found:?}"
    );
    assert!(
        found.contains(&((8192 + p.start) as u64, FileKind::Jpeg, Health::Intact)),
        "{found:?}"
    );
}
