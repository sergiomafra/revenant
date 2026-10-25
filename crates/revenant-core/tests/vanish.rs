//! A device that vanishes mid-scan or mid-extraction is `device_gone`, never bad sectors, and a cancel is prompt on slow
//! devices.

mod support;

use std::time::{Duration, Instant};

use revenant_core::formats::{FileKind, Health, all_formats};
use revenant_core::source::Source;
use revenant_core::{
    CancelToken, EngineError, Event, Finding, ScanOptions, ScanSummary, extract, scan,
};
use revenant_testkit as testkit;
use support::SimSource;
use testkit::fs::{FsBuilder, FsType};
use testkit::{DiskImage, Mp4Options};

const MIB: u64 = 1 << 20;

fn bytes(img: &DiskImage) -> Vec<u8> {
    let file = img.save();
    std::fs::read(file.path()).unwrap()
}

/// 32 MiB of noise with six JPEGs at sector starts, every 4 MiB from 1 MiB.
fn card() -> Vec<u8> {
    let mut img = DiskImage::noise(32 * MIB as usize, 5);
    for seed in 1..=6u64 {
        let at = (MIB + (seed - 1) * 4 * MIB) as usize;
        img.put(at, &testkit::jpeg(640, 480, seed, 0));
    }
    bytes(&img)
}

fn opts(threads: usize, cache_bytes: usize) -> ScanOptions {
    ScanOptions {
        threads,
        cache_bytes,
        ..ScanOptions::default()
    }
}

fn run(
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

fn code(r: &Result<ScanSummary, EngineError>) -> Option<&'static str> {
    r.as_ref().err().map(EngineError::code)
}

/// Runs the scan on a thread, cancels it `after` its start, and returns its result and how long it took to return once
/// cancelled.
fn cancel_after(
    source: &dyn Source,
    opts: &ScanOptions,
    after: Duration,
) -> (Result<ScanSummary, EngineError>, Duration) {
    let cancel = CancelToken::default();
    std::thread::scope(|s| {
        let handle = s.spawn(|| run(source, opts, &cancel).0);
        std::thread::sleep(after);
        cancel.cancel();
        let at = Instant::now();
        let r = handle.join().unwrap();
        (r, at.elapsed())
    })
}

#[test]
fn vanished_device_is_not_bad_sectors() {
    let default_cache = ScanOptions::default().cache_bytes;
    let (r, full) = run(
        &SimSource::new(card()),
        &opts(1, default_cache),
        &CancelToken::default(),
    );
    assert_eq!(r.unwrap().found, 6);
    // A small cache commits block by block, so the device vanishes after some events were emitted.
    for cache in [default_cache, 8 * MIB as usize] {
        for threads in [1, 8] {
            let source = SimSource::new(card()).gone_at(20 * MIB);
            let (r, events) = run(&source, &opts(threads, cache), &CancelToken::default());
            assert_eq!(code(&r), Some("device_gone"), "{threads} {cache}");
            assert_eq!(events, full[..events.len()], "{threads} {cache}");
            assert!(
                !events.iter().any(|e| matches!(e, Event::Unreadable { .. })),
                "{threads} {cache}"
            );
            if cache < default_cache {
                assert!(
                    events.iter().any(|e| matches!(e, Event::Found(_))),
                    "{threads}: {events:?}"
                );
            }
        }
    }
}

#[test]
fn shrinking_source_is_gone() {
    let (r, full) = run(
        &SimSource::new(card()),
        &opts(1, ScanOptions::default().cache_bytes),
        &CancelToken::default(),
    );
    r.unwrap();
    // The whole scan makes 33 read calls (one per window, plus one): the card shrinks after 8, before the window at
    // 18 MiB is read (call 19).
    for threads in [1, 8] {
        let source = SimSource::new(card()).shrink_after(8, 18 * MIB);
        let (r, events) = run(
            &source,
            &opts(threads, 8 * MIB as usize),
            &CancelToken::default(),
        );
        assert_eq!(code(&r), Some("device_gone"), "{threads}");
        assert_eq!(events, full[..events.len()], "{threads}");
        assert!(!events.iter().any(|e| matches!(e, Event::Unreadable { .. })));
        assert!(
            events.iter().any(|e| matches!(e, Event::Found(_))),
            "{threads}"
        );
    }
}

/// A FAT32 superfloppy holding one JPEG; its first FAT starts at 16 KiB (32 reserved sectors).
fn fat_card() -> (Vec<u8>, u64) {
    let mut b = FsBuilder::new(FsType::Fat32, 48 * MIB, 512);
    b.add("DCIM/100CANON/IMG_0001.JPG", &testkit::jpeg(640, 480, 1, 0));
    (b.build(), 32 * 512)
}

#[test]
fn vanished_device_during_the_listing_is_gone() {
    let (vol, fat) = fat_card();
    let (r, events) = run(
        &SimSource::new(vol.clone()),
        &opts(1, ScanOptions::default().cache_bytes),
        &CancelToken::default(),
    );
    r.unwrap();
    assert!(matches!(events[0], Event::Volume { .. }));
    for threads in [1, 8] {
        let source = SimSource::new(vol.clone()).gone_at(fat + 4096);
        let (r, events) = run(
            &source,
            &opts(threads, ScanOptions::default().cache_bytes),
            &CancelToken::default(),
        );
        assert_eq!(code(&r), Some("device_gone"), "{threads}");
        assert!(events.is_empty(), "{threads}: {events:?}");

        let source = SimSource::new(vol.clone()).slow(fat, 64 * 1024, Duration::from_millis(500));
        let cancel = CancelToken::default();
        let mut events = Vec::new();
        let r = std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(100));
                cancel.cancel();
            });
            scan(
                &source,
                &all_formats(),
                &opts(threads, ScanOptions::default().cache_bytes),
                &cancel,
                &mut |e| events.push(e),
            )
        });
        assert_eq!(code(&r), Some("cancelled"), "{threads}");
        assert!(events.is_empty(), "{threads}: {events:?}");
    }
}

#[test]
fn extract_stops_on_a_vanished_device() {
    let source = SimSource::new(card()).gone_after(2);
    let finding = Finding {
        id: 1,
        start: MIB,
        len: 8 * MIB,
        kind: FileKind::Jpeg,
        health: Health::Intact,
        fragments: Vec::new(),
        origin: None,
    };
    let dest = tempfile::tempdir().unwrap();
    let err = extract(&source, &finding, dest.path()).unwrap_err();
    assert_eq!(err.code(), "device_gone");
    assert_eq!(std::fs::read_dir(dest.path()).unwrap().count(), 0);
}

#[test]
fn cancel_is_prompt_while_a_window_fails_sector_by_sector() {
    for threads in [1, 8] {
        let source = SimSource::new(bytes(&DiskImage::noise(8 * MIB as usize, 3)))
            .bad(0, MIB)
            .slow(0, MIB, Duration::from_millis(5));
        let (r, took) = cancel_after(
            &source,
            &opts(threads, ScanOptions::default().cache_bytes),
            Duration::from_millis(100),
        );
        assert_eq!(code(&r), Some("cancelled"), "{threads}");
        assert!(took < Duration::from_secs(1), "{threads}: {took:?}");
    }
}

#[test]
fn cancel_is_prompt_during_a_long_measurement() {
    let video = testkit::mp4(&Mp4Options {
        mdat_len: 60 * MIB as usize,
        moov_at_end: false,
        mov: false,
        mdat_64bit: false,
    });
    let mdat = video.windows(4).position(|w| w == b"mdat").unwrap() as u64 - 4;
    let mut img = DiskImage::noise(64 * MIB as usize, 8);
    img.put(0, &video);
    let data = bytes(&img);
    // An 8 MiB cache reads one block ahead, so the video is measured (≈ 3 s of slow window loads) while the read stage
    // waits; the cancel comes once the listing (5 slow windows) and the first block are read.
    for threads in [1, 8] {
        let source = SimSource::new(data.clone()).slow(mdat, 60 * MIB, Duration::from_millis(50));
        let (r, took) = cancel_after(
            &source,
            &opts(threads, 8 * MIB as usize),
            Duration::from_secs(1),
        );
        assert_eq!(code(&r), Some("cancelled"), "{threads}");
        assert!(took < Duration::from_secs(1), "{threads}: {took:?}");
    }
}
