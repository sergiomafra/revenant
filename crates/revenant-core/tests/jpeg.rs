use revenant_core::formats::{FileKind, Format, Health, Jpeg, Measurement};
use revenant_core::source::FileSource;
use revenant_testkit as testkit;
use testkit::DiskImage;

fn measure(img: &tempfile::NamedTempFile, start: u64) -> Option<Measurement> {
    Jpeg.measure(&FileSource::open(img.path()).unwrap(), start)
}

#[test]
fn whole_jpeg_measured_exactly() {
    let j = testkit::jpeg(640, 480, 1, 20_000);
    let img = DiskImage::zeros(1 << 20).put(4096, &j).save();
    let m = measure(&img, 4096).unwrap();
    assert_eq!(
        m,
        Measurement {
            len: j.len() as u64,
            kind: FileKind::Jpeg,
            health: Health::Intact
        }
    );
}

#[test]
fn app1_thumbnail_does_not_end_the_file() {
    let j = testkit::jpeg(320, 200, 2, 60_000);
    let img = DiskImage::zeros(1 << 20).put(8192, &j).save();
    let m = measure(&img, 8192).unwrap();
    assert_eq!(m.len, j.len() as u64);
    assert_eq!(m.health, Health::Intact);
}

#[test]
fn oversized_app1_is_split_and_measured() {
    let j = testkit::jpeg(64, 64, 3, 150_000);
    let img = DiskImage::zeros(1 << 20).put(512, &j).save();
    let m = measure(&img, 512).unwrap();
    assert_eq!(m.len, j.len() as u64);
    assert_eq!(m.health, Health::Intact);
}

#[test]
fn tiny_jpeg_is_small() {
    assert!(testkit::jpeg(8, 8, 4, 0).len() <= 730);
}

#[test]
fn jpeg_cut_by_next_file_is_partial() {
    let j1 = testkit::jpeg(640, 480, 5, 0);
    let j2 = testkit::jpeg(64, 64, 6, 0);
    let start = 4096usize;
    let next = (start + j1.len() / 2).div_ceil(512) * 512;
    let img = DiskImage::zeros(1 << 20)
        .put(start, &j1[..j1.len() / 2])
        .put(next, &j2)
        .save();
    let m = measure(&img, start as u64).unwrap();
    assert_eq!(m.health, Health::Partial);
    assert_eq!(m.len, (next - start) as u64);
}

#[test]
fn jpeg_cut_by_end_of_source_is_partial() {
    let j = testkit::jpeg(640, 480, 7, 0);
    let cut = j.len() / 2;
    let mut image = DiskImage::zeros(4096 + cut);
    image.put(4096, &j[..cut]);
    let img = image.save();
    let m = measure(&img, 4096).unwrap();
    assert_eq!(m.health, Health::Partial);
    assert_eq!(m.len, cut as u64);
}

#[test]
fn corrupted_scan_data_is_damaged() {
    let mut j = testkit::jpeg(640, 480, 8, 0);
    let mid = j.len() / 2;
    // Stray DHT markers in the middle of the entropy-coded data: the structure still reaches EOI, but zune-jpeg
    // rejects the stream.
    for pair in j[mid..mid + 64].chunks_exact_mut(2) {
        pair.copy_from_slice(&[0xFF, 0xC4]);
    }
    let img = DiskImage::zeros(1 << 20).put(4096, &j).save();
    let m = measure(&img, 4096).unwrap();
    assert_eq!(m.len, j.len() as u64);
    assert_eq!(m.health, Health::Damaged);
}

#[test]
fn fake_header_is_not_jpeg() {
    let img = DiskImage::noise(1 << 20, 7)
        .put(0, b"\xFF\xD8\xFF\xE1\x00\x01")
        .save();
    assert_eq!(measure(&img, 0), None);

    // Valid APP0 length followed by garbage without DQT/SOF.
    let mut fake = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
    fake.extend_from_slice(&[0x41; 14]);
    fake.extend_from_slice(&[
        0xFF, 0xDA, 0x00, 0x08, 1, 2, 3, 4, 5, 6, 0x11, 0x22, 0xFF, 0xD9,
    ]);
    let img = DiskImage::noise(1 << 20, 9).put(1024, &fake).save();
    assert_eq!(measure(&img, 1024), None);
}

#[test]
fn recognizes_only_jpeg_signatures() {
    assert!(Jpeg.recognizes(&[0xFF, 0xD8, 0xFF, 0xE1, 0, 0]));
    assert!(Jpeg.recognizes(&[0xFF, 0xD8, 0xFF, 0xDB]));
    assert!(!Jpeg.recognizes(&[0xFF, 0xD8, 0xFF, 0x00]));
    assert!(!Jpeg.recognizes(&[0x89, b'P', b'N', b'G']));
    assert!(!Jpeg.recognizes(&[0xFF, 0xD8]));
}
