//! Spec §5: the source is never written. The image's bytes and modification time are the same before and after a full
//! scan that extracts every finding.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::Path;
use std::time::SystemTime;

use revenant_core::formats::all_formats;
use revenant_core::source::{FileSource, Source, VolumeId};
use revenant_core::{CancelToken, EngineError, Event, ScanOptions, extract, scan};
use revenant_testkit as testkit;
use testkit::{DiskImage, Mp4Options};

const MIB: usize = 1024 * 1024;

/// A source whose volume is unknown, so the test may extract next to the image (both live in temporary directories).
struct UnknownVolume(FileSource);

impl Source for UnknownVolume {
    fn len(&self) -> u64 {
        self.0.len()
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, EngineError> {
        self.0.read_at(offset, buf)
    }
    fn volume(&self) -> Option<VolumeId> {
        None
    }
}

fn fingerprint(path: &Path) -> (u64, SystemTime) {
    let mut h = DefaultHasher::new();
    std::fs::read(path).unwrap().hash(&mut h);
    (
        h.finish(),
        std::fs::metadata(path).unwrap().modified().unwrap(),
    )
}

#[test]
fn scan_and_extract_leave_the_source_untouched() {
    let video = testkit::mp4(&Mp4Options {
        mdat_len: 300_000,
        moov_at_end: true,
        mov: false,
        mdat_64bit: false,
    });
    let image = DiskImage::noise(4 * MIB, 41)
        .put(0, &testkit::jpeg(64, 64, 42, 2000))
        .put(MIB, &video)
        .put(3 * MIB, &testkit::jpeg(128, 96, 43, 0))
        .save();
    let before = fingerprint(image.path());

    let source = UnknownVolume(FileSource::open(image.path()).unwrap());
    let dest = tempfile::tempdir().unwrap();
    let mut extracted = 0;
    let summary = scan(
        &source,
        &all_formats(),
        &ScanOptions::default(),
        &CancelToken::default(),
        &mut |e| {
            if let Event::Found(f) = e {
                extract(&source, &f, dest.path()).unwrap();
                extracted += 1;
            }
        },
    )
    .unwrap();
    drop(source);

    assert_eq!(summary.found, 3);
    assert_eq!(extracted, 3);
    assert_eq!(fingerprint(image.path()), before);
}
