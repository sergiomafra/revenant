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
            health: Health::Intact,
            fragments: Vec::new()
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

/// Offset just past the SOS segment of `j`: where its entropy-coded data starts.
fn scan_data_start(j: &[u8]) -> usize {
    let sos = j.windows(2).position(|w| w == [0xFF, 0xDA]).unwrap();
    sos + 2 + usize::from(u16::from_be_bytes([j[sos + 2], j[sos + 3]]))
}

/// `jpeg(640, 480, 5, 0)` keeping its header and `scan_bytes` of scan data, then `jpeg(64, 64, 6, 0)` at the next sector.
fn cut_by_next_file(scan_bytes: usize) -> Measurement {
    let j1 = testkit::jpeg(640, 480, 5, 0);
    let j2 = testkit::jpeg(64, 64, 6, 0);
    let start = 4096usize;
    let kept = scan_data_start(&j1) + scan_bytes;
    let next = (start + kept).div_ceil(512) * 512;
    let img = DiskImage::zeros(1 << 20)
        .put(start, &j1[..kept])
        .put(next, &j2)
        .save();
    let m = measure(&img, start as u64).unwrap();
    assert_eq!(m.len, (next - start) as u64, "{scan_bytes} bytes");
    m
}

#[test]
fn cut_inside_the_first_mcu_row_is_damaged() {
    // That photo has rows of 80 MCUs, about 9.5 KB each. 200 bytes of scan data decode 1 MCU, 6000 bytes about 50 (the
    // zeros up to the next sector decode as a few more blocks): nothing of the photo would show, so it is not salvageable.
    for scan_bytes in [200, 6000] {
        assert_eq!(
            cut_by_next_file(scan_bytes).health,
            Health::Damaged,
            "{scan_bytes}"
        );
    }
}

#[test]
fn cut_after_the_first_mcu_row_is_partial() {
    // 12000 bytes decode 100 MCUs: the first row shows.
    assert_eq!(cut_by_next_file(12_000).health, Health::Partial);
}

/// `jpeg(640, 480, 5, 0)` keeping its header and `scan_bytes` of scan data, then `gap` bytes of `fill` (rounded up to
/// the next sector), then `jpeg(64, 64, 6, 0)`; or, with `next_file == false`, `fill` to the end of the source.
fn cut_then_gap(scan_bytes: usize, fill: u8, gap: usize, next_file: bool) -> Measurement {
    let j1 = testkit::jpeg(640, 480, 5, 0);
    let j2 = testkit::jpeg(64, 64, 6, 0);
    let start = 4096usize;
    let kept = scan_data_start(&j1) + scan_bytes;
    let next = (start + kept + gap).div_ceil(512) * 512;
    let mut image = DiskImage::zeros(next + if next_file { 1 << 20 } else { 0 });
    image.put(start, &j1[..kept]);
    image.put(start + kept, &vec![fill; next - start - kept]);
    if next_file {
        image.put(next, &j2);
    }
    let img = image.save();
    let m = measure(&img, start as u64).unwrap();
    assert_eq!(m.len, (next - start) as u64);
    m
}

#[test]
fn header_only_jpeg_followed_by_uniform_bytes_is_damaged() {
    // Erased or TRIMmed space reads as 00 (so do unreadable sectors): it decodes as valid blocks under the standard
    // tables, but it is not the photo.
    for (fill, gap, next_file) in [
        (0x00, 4096, true),
        (0x00, 64 << 10, true),
        (0x55, 64 << 10, true),
        (0xFF, 64 << 10, true),
        (0x00, 256 << 10, false),
    ] {
        let m = cut_then_gap(100, fill, gap, next_file);
        assert_eq!(
            m.health,
            Health::Damaged,
            "{fill:#x} x {gap}, next file {next_file}"
        );
    }
}

#[test]
fn cut_file_with_a_row_before_a_uniform_gap_stays_partial() {
    for fill in [0x00, 0x55] {
        assert_eq!(
            cut_then_gap(12_000, fill, 64 << 10, true).health,
            Health::Partial,
            "{fill:#x}"
        );
    }
}

/// Fails every read that touches `[bad_start, bad_end)`.
struct Unreadable {
    inner: FileSource,
    bad_start: u64,
    bad_end: u64,
}

impl revenant_core::source::Source for Unreadable {
    fn len(&self) -> u64 {
        self.inner.len()
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, revenant_core::EngineError> {
        if offset < self.bad_end && offset + buf.len() as u64 > self.bad_start {
            return Err(revenant_core::EngineError::Read {
                offset,
                cause: std::io::Error::other("injected"),
            });
        }
        self.inner.read_at(offset, buf)
    }
    fn volume(&self) -> Option<revenant_core::source::VolumeId> {
        None
    }
}

#[test]
fn header_only_jpeg_followed_by_unreadable_sectors_is_damaged() {
    // The header and 100 bytes of scan data, then real scan data on sectors that cannot be read, then another photo.
    let j1 = testkit::jpeg(640, 480, 5, 0);
    let j2 = testkit::jpeg(64, 64, 6, 0);
    let start = 4096usize;
    let first_bad = (start + scan_data_start(&j1) + 100).div_ceil(512) * 512;
    let next = first_bad + (64 << 10);
    let img = DiskImage::zeros(next + (1 << 20))
        .put(start, &j1[..next - start])
        .put(next, &j2)
        .save();
    let source = Unreadable {
        inner: FileSource::open(img.path()).unwrap(),
        bad_start: first_bad as u64,
        bad_end: next as u64,
    };
    let m = Jpeg.measure(&source, start as u64).unwrap();
    assert_eq!((m.len, m.health), ((next - start) as u64, Health::Damaged));
}

#[test]
fn header_only_jpeg_cut_by_end_of_source_is_damaged() {
    let j = testkit::jpeg(640, 480, 7, 0);
    let cut = scan_data_start(&j) + 100;
    let mut image = DiskImage::zeros(4096 + cut);
    image.put(4096, &j[..cut]);
    let img = image.save();
    let m = measure(&img, 4096).unwrap();
    assert_eq!((m.len, m.health), (cut as u64, Health::Damaged));
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

// Health tiers depend on how zune-jpeg 0.5.x treats corrupt input: strict mode rejects what the default (lenient) mode
// tolerates. Re-verify these two tests when upgrading zune-jpeg.
#[test]
fn lenient_only_decode_is_partial() {
    let mut j = testkit::jpeg(640, 480, 8, 0);
    let mid = j.len() / 2;
    // Invalid markers in the entropy-coded data: lenient zune-jpeg 0.5.x still decodes, strict mode rejects.
    j[mid..mid + 8].copy_from_slice(&[0xFF, 0x01, 0xFF, 0x01, 0xFF, 0x01, 0xFF, 0x01]);
    let img = DiskImage::zeros(1 << 20).put(4096, &j).save();
    let m = measure(&img, 4096).unwrap();
    assert_eq!(m.len, j.len() as u64);
    assert_eq!(m.health, Health::Partial);
}

#[test]
fn corrupted_scan_data_is_damaged() {
    let mut j = testkit::jpeg(640, 480, 8, 0);
    let mid = j.len() / 2;
    // Stray DHT markers in the middle of the entropy-coded data: the structure still reaches EOI, but zune-jpeg
    // rejects the stream in both modes.
    for pair in j[mid..mid + 64].as_chunks_mut::<2>().0 {
        *pair = [0xFF, 0xC4];
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

// ---- scan-walk risk paths (hand-built streams; they need not decode, only be measured) ----

/// SOI, DQT, SOF (`sof` marker byte), DHT, SOS header. Scan data and EOI are appended by the caller.
fn header(sof: u8, width: u16, height: u16) -> Vec<u8> {
    let mut v = vec![0xFF, 0xD8, 0xFF, 0xDB, 0x00, 0x43, 0x00];
    v.extend_from_slice(&[1; 64]);
    v.extend_from_slice(&[0xFF, sof, 0x00, 0x0B, 0x08]);
    v.extend_from_slice(&height.to_be_bytes());
    v.extend_from_slice(&width.to_be_bytes());
    v.extend_from_slice(&[0x01, 0x01, 0x11, 0x00]);
    v.extend_from_slice(&[0xFF, 0xC4, 0x00, 0x14, 0x00, 0x01]);
    v.extend_from_slice(&[0; 16]);
    v.extend_from_slice(&sos());
    v
}

fn sos() -> Vec<u8> {
    vec![0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3F, 0x00]
}

const MIB: usize = 1 << 20;

fn measure_stream(stream: &[u8], image_len: usize) -> Measurement {
    let img = DiskImage::zeros(image_len).put(4096, stream).save();
    measure(&img, 4096).expect("measurable")
}

#[test]
fn eoi_at_a_read_window_boundary_is_found() {
    // The FF of the final FF D9 lands on every byte around the end of the 1st and of the 2nd 1 MiB read window
    // (the exact window anchor is an implementation detail, so sweep a few bytes either side of each boundary).
    for windows in [1, 2] {
        for delta in 0..8 {
            let mut stream = header(0xC0, 64, 64);
            stream.resize(windows * MIB - 4 + delta, 0x11);
            stream.extend_from_slice(&[0xFF, 0xD9]);
            let m = measure_stream(&stream, 3 * MIB + 8192);
            assert_eq!(
                m.len,
                stream.len() as u64,
                "windows = {windows}, delta = {delta}"
            );
        }
    }
}

#[test]
fn progressive_scans_are_walked_to_the_last_eoi() {
    let mut stream = header(0xC2, 64, 64);
    stream.extend_from_slice(&[0x12, 0x34, 0xFF, 0x00, 0x56]);
    // Second scan: a new DHT and SOS inside the entropy-coded stream.
    stream.extend_from_slice(&[0xFF, 0xC4, 0x00, 0x14, 0x10, 0x01]);
    stream.extend_from_slice(&[0; 16]);
    stream.extend_from_slice(&sos());
    stream.extend_from_slice(&[0x78, 0x9A, 0xBC]);
    stream.extend_from_slice(&[0xFF, 0xD9]);
    let m = measure_stream(&stream, MIB);
    assert_eq!(m.len, stream.len() as u64);
}

#[test]
fn stuffing_and_restart_markers_do_not_end_the_scan() {
    let mut stream = header(0xC0, 64, 64);
    stream.extend_from_slice(&[0x11, 0xFF, 0x00, 0x22]);
    for rst in 0xD0..=0xD7 {
        stream.extend_from_slice(&[0x33, 0xFF, rst, 0x44, 0xFF, 0x00]);
    }
    stream.extend_from_slice(&[0xFF, 0xD9]);
    let m = measure_stream(&stream, MIB);
    assert_eq!(m.len, stream.len() as u64);
}

#[test]
fn eoi_beyond_the_maximum_length_is_cut_at_the_maximum() {
    use revenant_core::formats::JPEG_MAX_LEN;
    let max = JPEG_MAX_LEN as usize;
    let mut stream = header(0xC0, 64, 64);
    // FF at offset MAX - 1, D9 at offset MAX: the file would be MAX + 1 bytes long.
    stream.resize(max - 1, 0x11);
    stream.extend_from_slice(&[0xFF, 0xD9]);
    let m = measure_stream(&stream, max + 8192);
    assert_eq!(m.len, JPEG_MAX_LEN);
    assert_eq!(m.health, Health::Partial);
}

#[test]
fn huge_declared_size_is_partial() {
    let mut j = testkit::jpeg(64, 64, 9, 0);
    let sof = j.windows(2).position(|w| w == [0xFF, 0xC0]).unwrap();
    // SOF0 payload: length(2) precision(1) height(2) width(2).
    j[sof + 5..sof + 9].copy_from_slice(&[0x40, 0x00, 0x40, 0x00]); // 16384 x 16384 (zune-jpeg's own limit; 268 MP is over our cap)
    let img = DiskImage::zeros(MIB).put(4096, &j).save();
    let t = std::time::Instant::now();
    let m = measure(&img, 4096).unwrap();
    assert_eq!(m.len, j.len() as u64);
    assert_eq!(m.health, Health::Partial);
    assert!(t.elapsed().as_secs() < 5);
}

/// `j` with `insert` placed right after its first segment (so the head still looks like a JPEG).
fn with_after_first_segment(j: &[u8], insert: &[u8]) -> Vec<u8> {
    assert_eq!(j[2], 0xFF);
    let first_end = 2 + 2 + usize::from(u16::from_be_bytes([j[4], j[5]]));
    [&j[..first_end], insert, &j[first_end..]].concat()
}

#[test]
fn fill_bytes_before_a_marker_are_accepted() {
    let v = with_after_first_segment(&testkit::jpeg(64, 64, 12, 0), &[0xFF, 0xFF, 0xFF]);
    let img = DiskImage::zeros(MIB).put(4096, &v).save();
    let m = measure(&img, 4096).unwrap();
    assert_eq!((m.len, m.health), (v.len() as u64, Health::Intact));
}

#[test]
fn dac_and_fill_bytes_before_sos_are_measured() {
    // FF CC is DAC (arithmetic coding conditioning): length 4 and two bytes.
    let v = with_after_first_segment(
        &testkit::jpeg(64, 64, 12, 0),
        &[0xFF, 0xFF, 0xFF, 0xCC, 0x00, 0x04, 0x00, 0x00, 0xFF, 0xFF],
    );
    let img = DiskImage::zeros(MIB).put(4096, &v).save();
    let m = measure(&img, 4096).unwrap();
    // The walk follows the DAC segment and the fill bytes to the exact end. zune-jpeg cannot decode DAC (arithmetic
    // coding): the structure is complete but not verifiable, so Partial, never Intact without evidence.
    assert_eq!((m.len, m.health), (v.len() as u64, Health::Partial));
}

/// Counts the bytes requested through `read_at`.
struct Counting {
    inner: FileSource,
    bytes: std::sync::atomic::AtomicU64,
}

impl revenant_core::source::Source for Counting {
    fn len(&self) -> u64 {
        self.inner.len()
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, revenant_core::EngineError> {
        self.bytes
            .fetch_add(buf.len() as u64, std::sync::atomic::Ordering::Relaxed);
        self.inner.read_at(offset, buf)
    }
    fn volume(&self) -> Option<revenant_core::source::VolumeId> {
        None
    }
}

#[test]
fn confirms_walks_at_most_one_mib_of_header() {
    // SOI, then valid APP1 segments for about 1.3 MiB, never reaching SOS.
    let mut v = vec![0xFF, 0xD8];
    for _ in 0..20 {
        v.extend_from_slice(&[0xFF, 0xE1, 0xFF, 0xFF]);
        v.resize(v.len() + 65_533, 0);
    }
    let img = DiskImage::zeros(4 * MIB).put(4096, &v).save();
    let source = Counting {
        inner: FileSource::open(img.path()).unwrap(),
        bytes: Default::default(),
    };
    assert!(!Jpeg.confirms(&source, 4096));
    assert!(source.bytes.load(std::sync::atomic::Ordering::Relaxed) <= MIB as u64 + 4096);
}
