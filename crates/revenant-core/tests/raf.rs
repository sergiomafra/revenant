use revenant_core::EngineError;
use revenant_core::formats::{FileKind, Format, Health, Measurement, Raf, all_formats};
use revenant_core::source::{FileSource, Source, VolumeId};
use revenant_core::{CancelToken, Event, ScanOptions, scan};
use revenant_testkit as testkit;
use testkit::{DiskImage, TestExif};

const AT: usize = 8192;

fn exif() -> TestExif {
    TestExif {
        make: Some("FUJIFILM".into()),
        model: Some("X-T5".into()),
        iso: Some(800),
        ..TestExif::default()
    }
}

fn measure(img: &tempfile::NamedTempFile) -> Option<Measurement> {
    Raf.measure(&FileSource::open(img.path()).unwrap(), AT as u64)
}

fn scan_image(img: &tempfile::NamedTempFile) -> Vec<(u64, u64, FileKind, Health)> {
    let source = FileSource::open(img.path()).unwrap();
    let mut out = Vec::new();
    scan(
        &source,
        &all_formats(),
        &ScanOptions::default(),
        &CancelToken::default(),
        &mut |e| {
            if let Event::Found(f) = e {
                out.push((f.start, f.len, f.kind, f.health));
            }
        },
    )
    .unwrap();
    out
}

/// Overwrites a big-endian header field of a RAF.
fn put_field(bytes: &mut [u8], at: usize, v: u32) {
    bytes[at..at + 4].copy_from_slice(&v.to_be_bytes());
}

#[test]
fn raf_is_found_intact() {
    let raf = testkit::raf(120_000, 7, &exif());
    let img = DiskImage::noise(1 << 20, 3).put(AT, &raf).save();
    assert_eq!(
        scan_image(&img),
        vec![(AT as u64, raf.len() as u64, FileKind::Raf, Health::Intact)]
    );
    assert_eq!(FileKind::Raf.as_str(), "raf");
    assert_eq!(FileKind::Raf.extension(), "raf");
}

#[test]
fn raf_cut_is_partial() {
    let raf = testkit::raf(120_000, 7, &exif());
    let keep = raf.len() - 5000;
    let img = DiskImage::zeros(AT + keep).put(AT, &raf[..keep]).save();
    assert_eq!(
        measure(&img),
        Some(Measurement {
            len: keep as u64,
            kind: FileKind::Raf,
            health: Health::Partial
        })
    );
}

#[test]
fn raf_cut_by_the_next_file_is_partial() {
    let raf = testkit::raf(120_000, 7, &exif());
    let cut = (AT + raf.len() - 60_000).next_multiple_of(512);
    let jpeg = testkit::jpeg(64, 64, 9, 0);
    let img = DiskImage::noise(1 << 20, 5)
        .put(AT, &raf)
        .put(cut, &jpeg)
        .save();
    let found = scan_image(&img);
    assert_eq!(
        found[0],
        (AT as u64, (cut - AT) as u64, FileKind::Raf, Health::Partial)
    );
    assert_eq!(found[1].0, cut as u64);
}

#[test]
fn raf_with_a_broken_preview_is_partial() {
    let mut raf = testkit::raf(120_000, 7, &exif());
    // The JPEG sits at 0x94; undecodable bytes in its middle.
    let len = u32::from_be_bytes(raf[0x58..0x5C].try_into().unwrap()) as usize;
    let mid = 0x94 + len / 2;
    raf[mid..mid + 64].copy_from_slice(&[0xFF, 0xC4].repeat(32));
    let img = DiskImage::noise(1 << 20, 3).put(AT, &raf).save();
    let m = measure(&img).unwrap();
    assert_eq!((m.len, m.health), (raf.len() as u64, Health::Partial));
}

#[test]
fn raf_without_a_preview_is_partial() {
    let mut raf = testkit::raf(120_000, 7, &exif());
    put_field(&mut raf, 0x58, 0);
    let img = DiskImage::noise(1 << 20, 3).put(AT, &raf).save();
    assert_eq!(measure(&img).unwrap().health, Health::Partial);
}

#[test]
fn raf_offset_past_the_source_does_not_claim_the_rest_of_it() {
    let mut raf = testkit::raf(120_000, 7, &exif());
    let original_len = raf.len();
    put_field(&mut raf, 0x64, 900_000_000);
    let img = DiskImage::noise(1 << 20, 3).put(AT, &raf).save();
    let m = measure(&img).unwrap();
    assert_eq!(m.health, Health::Partial);
    // Only the header, JPEG and CFA header count: the CFA block (120,000 bytes) is gone from the extent.
    assert_eq!(m.len, (original_len - 120_000) as u64);
}

#[test]
fn raf_range_past_the_one_gib_cap_is_not_a_file() {
    let mut raf = testkit::raf(120_000, 7, &exif());
    put_field(&mut raf, 0x68, 0xFFFF_FFF0);
    let img = DiskImage::noise(1 << 20, 3).put(AT, &raf).save();
    assert_eq!(measure(&img), None);
    assert!(scan_image(&img).iter().all(|f| f.2 != FileKind::Raf));
}

#[test]
fn random_bytes_after_the_magic_are_not_a_raf() {
    let mut img = DiskImage::noise(1 << 20, 3);
    img.put(AT, b"FUJIFILMCCD-RAW ");
    let img = img.save();
    assert_eq!(measure(&img), None);
}

#[test]
fn unreadable_sector_in_the_raw_data_is_partial() {
    struct Failing(FileSource, u64);
    impl Source for Failing {
        fn len(&self) -> u64 {
            self.0.len()
        }
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, EngineError> {
            if offset < self.1 + 512 && offset + buf.len() as u64 > self.1 {
                return Err(EngineError::Read {
                    offset,
                    cause: std::io::Error::other("injected"),
                });
            }
            self.0.read_at(offset, buf)
        }
        fn volume(&self) -> Option<VolumeId> {
            None
        }
    }
    let raf = testkit::raf(120_000, 7, &exif());
    let img = DiskImage::noise(1 << 20, 3).put(AT, &raf).save();
    let bad = (AT + raf.len() - 40_000).next_multiple_of(512) as u64;
    let source = Failing(FileSource::open(img.path()).unwrap(), bad);
    let m = Raf.measure(&source, AT as u64).unwrap();
    assert_eq!((m.len, m.health), (raf.len() as u64, Health::Partial));
}

#[test]
fn raf_block_starting_with_a_jpeg_at_a_sector_start_is_not_cut() {
    // Move the embedded JPEG to offset 512: its head is then a sector-aligned "JPEG starts here", but it is the
    // RAF's own.
    let raf = testkit::raf(120_000, 7, &exif());
    let jpeg_len = u32::from_be_bytes(raf[0x58..0x5C].try_into().unwrap()) as usize;
    let shift = 512 - 0x94;
    let mut moved = raf[..0x94].to_vec();
    moved.resize(512, 0);
    moved.extend_from_slice(&raf[0x94..]);
    for at in [0x54, 0x5C, 0x64] {
        let off = u32::from_be_bytes(moved[at..at + 4].try_into().unwrap());
        put_field(&mut moved, at, off + shift as u32);
    }
    assert_eq!(&moved[512..514], &[0xFF, 0xD8]);
    assert!(jpeg_len > 0);
    let img = DiskImage::noise(1 << 20, 3).put(AT, &moved).save();
    assert_eq!(
        scan_image(&img),
        vec![(AT as u64, moved.len() as u64, FileKind::Raf, Health::Intact)]
    );
}
