use revenant_core::formats::{FileKind, Format, Health, IsoBmff, Jpeg, Measurement, all_formats};
use revenant_core::source::{FileSource, Source, VolumeId};
use revenant_core::{CancelToken, EngineError, Event, Fragment, ScanOptions, ScanSummary, scan};
use revenant_testkit as testkit;
use testkit::{DiskImage, Mp4Options};

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

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "pieces contract")]
fn a_single_piece_breaks_the_pieces_contract() {
    scan_pieces(1000, &[(4096, 1000)]);
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "pieces contract")]
fn a_first_piece_elsewhere_breaks_the_pieces_contract() {
    scan_pieces(1500, &[(4608, 1000), (20_480, 500)]);
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "pieces contract")]
fn pieces_not_summing_to_len_break_the_pieces_contract() {
    scan_pieces(1600, &[(4096, 1000), (20_480, 500)]);
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "pieces contract")]
fn overlapping_pieces_break_the_pieces_contract() {
    scan_pieces(1500, &[(4096, 1000), (4596, 500)]);
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "pieces contract")]
fn a_piece_past_the_source_breaks_the_pieces_contract() {
    scan_pieces(1500, &[(4096, 1000), (65_400, 500)]);
}
