use revenant_core::formats::{FileKind, Format, Health, IsoBmff, Jpeg, all_formats};
use revenant_core::source::{FileSource, Source, VolumeId};
use revenant_core::{CancelToken, EngineError, Event, ScanOptions, ScanSummary, scan};
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
    assert!(start % 512 == 0 && start + jpeg.len() > MIB);
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
    assert!(len % 512 != 0);
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
