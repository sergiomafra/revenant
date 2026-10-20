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

// A baseline JPEG is judged by Revenant's entropy decoder (rulings Q6, Q9, Q10): corrupt entropy-coded data after the
// first row of MCUs is `Partial` (that much of the photo shows), within the first row `Damaged`. zune-jpeg is not run for
// these (it used to decide: lenient-only → `Partial`, rejected by both modes → `Damaged`).
#[test]
fn lenient_only_decode_is_partial() {
    let mut j = testkit::jpeg(640, 480, 8, 0);
    let mid = j.len() / 2;
    // Invalid markers in the middle of the entropy-coded data (zune-jpeg 0.5.x decodes it leniently, strict mode
    // rejects it): about half the rows show.
    j[mid..mid + 8].copy_from_slice(&[0xFF, 0x01, 0xFF, 0x01, 0xFF, 0x01, 0xFF, 0x01]);
    let img = DiskImage::zeros(1 << 20).put(4096, &j).save();
    let m = measure(&img, 4096).unwrap();
    assert_eq!(m.len, j.len() as u64);
    assert_eq!(m.health, Health::Partial);
}

/// `jpeg(640, 480, 8, 0)` with 64 bytes of stray DHT markers at `at` (zune-jpeg rejects that stream in both modes).
fn with_stray_dht(at: usize) -> Measurement {
    let mut j = testkit::jpeg(640, 480, 8, 0);
    for pair in j[at..at + 64].as_chunks_mut::<2>().0 {
        *pair = [0xFF, 0xC4];
    }
    let img = DiskImage::zeros(1 << 20).put(4096, &j).save();
    let m = measure(&img, 4096).unwrap();
    assert_eq!(m.len, j.len() as u64);
    m
}

#[test]
fn corrupted_scan_data_is_damaged() {
    // Within the first row of MCUs (80 MCUs, about 9.5 KB): nothing of the photo would show (Q9).
    let j = testkit::jpeg(640, 480, 8, 0);
    assert_eq!(
        with_stray_dht(scan_data_start(&j) + 200).health,
        Health::Damaged
    );
}

#[test]
fn corrupted_scan_data_after_the_first_row_is_partial() {
    // In the middle: the rows above it show (Q10). This layout was `Damaged` while zune-jpeg decided.
    let j = testkit::jpeg(640, 480, 8, 0);
    assert_eq!(with_stray_dht(j.len() / 2).health, Health::Partial);
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

// Two-piece JPEGs (bifragment gap carving).

const KIB: usize = 1024;
/// Where the photo under test starts.
const START: usize = 64 * KIB;

/// `a = jpeg_smooth(1024, 768, 1)` and its split point `c`: the first multiple of 512 at or after half of it.
fn smooth_photo() -> (Vec<u8>, usize) {
    let a = testkit::jpeg_smooth(1024, 768, 1);
    let c = (a.len() / 2).next_multiple_of(512);
    (a, c)
}

/// The pieces contract of a measurement at `start` on a source of `source_len` bytes: at least 2 pieces, the first at
/// `start`, lengths summing to `len`, none overlapping, all inside the source.
fn assert_pieces(m: &Measurement, start: u64, source_len: u64) {
    let p = &m.fragments;
    assert!(p.len() >= 2, "{m:?}");
    assert_eq!(p[0].start, start, "{m:?}");
    assert_eq!(p.iter().map(|f| f.len).sum::<u64>(), m.len, "{m:?}");
    assert!(
        p.iter().all(|f| f.len > 0 && f.start + f.len <= source_len),
        "{m:?}"
    );
    let mut sorted = p.clone();
    sorted.sort_by_key(|f| f.start);
    assert!(
        sorted
            .windows(2)
            .all(|w| w[0].start + w[0].len <= w[1].start),
        "{m:?}"
    );
}

/// The measurement of `a` reassembled from `a[..c]` at `START` and `a[c..]` at `tail`.
fn reassembled(a: &[u8], c: usize, tail: usize, health: Health) -> Measurement {
    Measurement {
        len: a.len() as u64,
        kind: FileKind::Jpeg,
        health,
        fragments: vec![
            revenant_core::Fragment {
                start: START as u64,
                len: c as u64,
            },
            revenant_core::Fragment {
                start: tail as u64,
                len: (a.len() - c) as u64,
            },
        ],
    }
}

/// Measures at `START` and checks the pieces contract when the result has pieces.
fn measure_at_start(img: &DiskImage) -> Measurement {
    let file = img.save();
    let len = std::fs::metadata(file.path()).unwrap().len();
    let m = measure(&file, START as u64).expect("a JPEG");
    if !m.fragments.is_empty() {
        assert_pieces(&m, START as u64, len);
    }
    m
}

/// 8 MiB of noise (seed 5), `a[..c]` at `START` and `a[c..]` at 3 MiB.
fn split_photo_image() -> (DiskImage, Vec<u8>, usize) {
    let (a, c) = smooth_photo();
    let mut img = DiskImage::noise(8 * MIB, 5);
    img.put(START, &a[..c]).put(3 * MIB, &a[c..]);
    (img, a, c)
}

#[test]
fn photo_ending_early_in_an_eoi_is_not_intact() {
    // Half a photo, then zeros and an EOI: zune-jpeg's strict mode accepts it (it neither counts MCUs nor rejects data
    // before EOI), our entropy decoder does not (EOI before the last MCU).
    let (a, c) = smooth_photo();
    let mut bytes = a[..c].to_vec();
    bytes.extend_from_slice(&[0; 510]);
    bytes.extend_from_slice(&[0xFF, 0xD9]);
    let img = DiskImage::zeros(START + bytes.len() + 4096)
        .put(START, &bytes)
        .save();
    let m = measure(&img, START as u64).unwrap();
    assert_eq!((m.len, m.health), (bytes.len() as u64, Health::Partial));
}

#[test]
fn bifragment_jpeg_is_reassembled_intact() {
    let (mut img, a, c) = split_photo_image();
    img.put(START + c, &testkit::jpeg(64, 64, 9, 0));
    assert_eq!(
        measure_at_start(&img),
        reassembled(&a, c, 3 * MIB, Health::Intact)
    );
}

#[test]
fn fragment_followed_by_noise_is_reassembled() {
    let (img, a, c) = split_photo_image();
    assert_eq!(
        measure_at_start(&img),
        reassembled(&a, c, 3 * MIB, Health::Intact)
    );
}

#[test]
fn fragment_followed_by_zeros_is_reassembled() {
    let (a, c) = smooth_photo();
    let mut img = DiskImage::zeros(8 * MIB);
    img.put(START, &a[..c]).put(3 * MIB, &a[c..]);
    assert_eq!(
        measure_at_start(&img),
        reassembled(&a, c, 3 * MIB, Health::Intact)
    );
}

#[test]
fn a_first_piece_ending_in_ff_is_reassembled() {
    // The first piece's last byte is an `FF` whose stuffed `00` starts the tail: the decoder reports the break at that
    // `FF` (one byte before the sector boundary where the photo really breaks), and that boundary must still be tried.
    let (a, c) = (1..64)
        .find_map(|seed| {
            let a = testkit::jpeg(640, 480, seed, 0);
            let from = (scan_data_start(&a) + 16 * KIB).next_multiple_of(512);
            let c = (from..a.len() - 4096)
                .step_by(512)
                .find(|&c| a[c - 1] == 0xFF)?;
            Some((a, c))
        })
        .expect("a photo with an FF right before a sector boundary");
    let mut img = DiskImage::noise(8 * MIB, 5);
    // The gap's first byte makes `FF` + it an impossible marker.
    img.put(START, &a[..c])
        .put(START + c, &[0x5A])
        .put(3 * MIB, &a[c..]);
    assert_eq!(
        measure_at_start(&img),
        reassembled(&a, c, 3 * MIB, Health::Intact)
    );
}

#[test]
fn two_passing_continuations_are_partial() {
    let (mut img, a, c) = split_photo_image();
    img.put(5 * MIB, &a[c..]);
    assert_eq!(
        measure_at_start(&img),
        reassembled(&a, c, 3 * MIB, Health::Partial)
    );
}

#[test]
fn search_cut_short_by_the_cap_is_partial() {
    // After the real tail, 1100 sectors each ending in FF D9: 4 tries each, more than MAX_JOIN_TRIES (4096) in all, so
    // the search stops before the candidates run out.
    let (mut img, a, c) = split_photo_image();
    let after = (3 * MIB + a.len() - c).next_multiple_of(512);
    let mut sector = [0u8; 512];
    sector[510..].copy_from_slice(&[0xFF, 0xD9]);
    for i in 0..1100 {
        img.put(after + i * 512, &sector);
    }
    assert_eq!(
        measure_at_start(&img),
        reassembled(&a, c, 3 * MIB, Health::Partial)
    );
}

#[test]
fn glued_halves_of_two_photos_are_not_intact() {
    let (a, c) = smooth_photo();
    let b = testkit::jpeg_smooth(1024, 768, 2);
    let mut img = DiskImage::noise(8 * MIB, 5);
    img.put(START, &a[..c]).put(START + c, &b);
    let m = measure_at_start(&img);
    assert_ne!(m.health, Health::Intact, "{m:?}");
    // b itself is found whole.
    let file = img.save();
    let mb = measure(&file, (START + c) as u64).unwrap();
    assert_eq!(
        (mb.len, mb.health, mb.fragments.len()),
        (b.len() as u64, Health::Intact, 0)
    );
}

#[test]
fn foreign_tail_after_the_gap_is_not_glued_intact() {
    let (a, c) = smooth_photo();
    let b = testkit::jpeg_smooth(1024, 768, 2);
    let mut img = DiskImage::noise(8 * MIB, 5);
    img.put(START, &a[..c]).put(3 * MIB, &b[c..]);
    let m = measure_at_start(&img);
    assert_ne!(m.health, Health::Intact, "{m:?}");
}

#[test]
fn three_pieces_are_not_reassembled() {
    let (a, c) = smooth_photo();
    let mut img = DiskImage::noise(8 * MIB, 5);
    img.put(START, &a[..c])
        .put(2 * MIB, &a[c..c + 4096])
        .put(4 * MIB, &a[c + 4096..]);
    let m = measure_at_start(&img);
    assert_eq!((m.fragments.len(), m.health), (0, Health::Partial), "{m:?}");
}

#[test]
fn hostile_gap_search_is_bounded() {
    // a[..c] then 65 MiB of sectors each `[00; 510] FF D9`: every sector is a distinct EOI, so the window holds far more
    // candidates than MAX_JOIN_TRIES.
    let a = testkit::jpeg(640, 480, 3, 0);
    let c = (a.len() / 2).next_multiple_of(512);
    let mut img = DiskImage::zeros(c + 65 * MIB);
    img.put(0, &a[..c]);
    let mut sector = [0u8; 512];
    sector[510..].copy_from_slice(&[0xFF, 0xD9]);
    for i in 0..(65 * MIB / 512) {
        img.put(c + i * 512, &sector);
    }
    let file = img.save();
    let source = Counting {
        inner: FileSource::open(file.path()).unwrap(),
        bytes: Default::default(),
    };
    let t = std::time::Instant::now();
    let m = Jpeg.measure(&source, 0).expect("a JPEG");
    let took = t.elapsed();
    if !m.fragments.is_empty() {
        assert_pieces(&m, 0, revenant_core::source::Source::len(&source));
    }
    assert_ne!(m.health, Health::Intact, "{m:?}");
    assert!(took < std::time::Duration::from_secs(10), "{took:?}");
    let read = source.bytes.load(std::sync::atomic::Ordering::Relaxed);
    assert!(read <= 68 * MIB as u64, "{} MiB read", read / MIB as u64);
}

/// Offset in `j` of its `k`-th restart marker (1-based).
fn restart_marker(j: &[u8], k: usize) -> usize {
    let data = scan_data_start(j);
    let at = j[data..]
        .windows(2)
        .enumerate()
        .filter(|(_, w)| w[0] == 0xFF && (0xD0..=0xD7).contains(&w[1]))
        .nth(k - 1)
        .expect("restart marker")
        .0;
    data + at
}

#[test]
fn tail_spliced_at_a_restart_marker_needs_the_dc_seam() {
    // Restart markers every MCU row: DC predictors reset there, so any photo of the same geometry and tables continues
    // validly at its own k-th marker with the exact MCU count. The DC seam tells the photo's own tail from this other one,
    // but a seam right on a restart boundary is never Intact (ruling Q15).
    // Rows of 8191 blocks whose DC alternates by 1600 take about 24 KiB each, so a first piece of one row is long enough
    // to be searched; vertically the DC changes by 3 per row, and photo b sits about 50 above photo a.
    let alt = |bx: u32| if bx.is_multiple_of(2) { 800 } else { -800 };
    let dc_a = |bx: u32, by: u32| alt(bx) + by as i16 * 3 - 50;
    let dc_b = |bx: u32, by: u32| alt(bx) - by as i16 * 3 + 100;
    let a = testkit::jpeg_gray_dc(65528, 256, 8191, &dc_a);
    let b = testkit::jpeg_gray_dc(65528, 256, 8191, &dc_b);
    // At the 16th marker the seam is judged right after its row (against the rows above); at the 1st, with no row above,
    // only once the whole image is decoded.
    for k in [16, 1] {
        let (pa, pb) = (restart_marker(&a, k), restart_marker(&b, k));
        // The first piece ends right before the marker, on a sector boundary.
        let start = 64 * KIB - pa % 512 + 512;
        assert_eq!((start + pa) % 512, 0);
        assert!(pa >= 16 * KIB, "{pa}");
        let image = |tail: &[u8]| {
            let mut img = DiskImage::noise(8 * MIB, 5);
            img.put(start, &a[..pa]).put(3 * MIB, tail);
            let file = img.save();
            let m = measure(&file, start as u64).unwrap();
            if !m.fragments.is_empty() {
                assert_pieces(&m, start as u64, 8 * MIB as u64);
            }
            m
        };
        // Q15: the seam falls exactly on a restart boundary, so the photo is joined but at most Partial.
        let own = image(&a[pa..]);
        assert_eq!(
            (own.len, own.health, own.fragments.len()),
            (a.len() as u64, Health::Partial, 2),
            "marker {k}"
        );
        let foreign = image(&b[pb..]);
        assert!(foreign.fragments.is_empty(), "marker {k}: {foreign:?}");
        assert_ne!(foreign.health, Health::Intact, "marker {k}: {foreign:?}");
    }
}

#[test]
fn photo_glued_to_a_wrong_tail_is_not_intact() {
    // Half a photo straight followed by another photo's tail, or by its own tail with a sector missing: zune-jpeg's
    // strict mode decodes both (an MCU count it does not check comes out wrong), our entropy decoder does not.
    let (a, c) = smooth_photo();
    let b = testkit::jpeg_smooth(1024, 768, 2);
    for (name, tail) in [("other tail", &b[c..]), ("sector missing", &a[c + 512..])] {
        let mut bytes = a[..c].to_vec();
        bytes.extend_from_slice(tail);
        let img = DiskImage::zeros(START + bytes.len() + 4096)
            .put(START, &bytes)
            .save();
        let m = measure(&img, START as u64).unwrap();
        assert_eq!(
            (m.len, m.health, m.fragments.len()),
            (bytes.len() as u64, Health::Partial, 0),
            "{name}"
        );
    }
}

#[test]
fn two_photos_written_interleaved_are_never_intact() {
    // Photos a and b of one frame (restart marker every MCU, DC values k apart), written a[..c] | b[..c] | a[c..] | b[c..]:
    // a's own tail lies behind b's first piece, whose sectors share a's EOI, so a search that stopped after the first
    // few of them would find only b's tail. Glued on a restart marker below the seam threshold, it passes; it must not be
    // called Intact (ruling Q7).
    let dc_a = |bx: u32, by: u32| (bx as i16 / 4) + (by as i16 / 4) - 100;
    for k in [3i16, 5, 7] {
        let dc_b = move |bx: u32, by: u32| dc_a(bx, by) + k;
        let a = testkit::jpeg_gray_dc(1024, 768, 1, &dc_a);
        let b = testkit::jpeg_gray_dc(1024, 768, 1, &dc_b);
        for c in [3072usize, 7168, 16384, 20480, 24576, 28672] {
            let pb = START + c;
            let pa2 = pb + c;
            let pb2 = (pa2 + a.len() - c).next_multiple_of(512);
            let mut img = DiskImage::zeros(pb2 + b.len() + 8192);
            img.put(START, &a[..c])
                .put(pb, &b[..c])
                .put(pa2, &a[c..])
                .put(pb2, &b[c..]);
            let m = measure_at_start(&img);
            assert_ne!(m.health, Health::Intact, "k {k}, c {c}: {m:?}");
        }
    }
}

#[test]
fn a_search_reads_at_most_its_window_and_its_budget() {
    // Half a photo, then 64 MiB of bytes that are not entropy-coded data but hold no marker, with an EOI every 128 KiB:
    // every candidate's second piece is long, so each try's reader loads a full read window before the decode fails.
    // Unbounded, 4096 tries would read 256 MiB; the search's budget stops the tries at 64 MiB.
    let (a, c) = smooth_photo();
    let mut img = a[..c].to_vec();
    let mut block = vec![0xFEu8; 128 * KIB];
    for (i, b) in block.iter_mut().enumerate() {
        if i % 509 == 0 {
            *b = 0xFD; // no sector of one repeated value
        }
    }
    let n = block.len();
    block[n - 2..].copy_from_slice(&[0xFF, 0xD9]);
    while img.len() < c + 65 * MIB {
        img.extend_from_slice(&block);
    }
    let file = DiskImage::zeros(img.len()).put(0, &img).save();
    let source = Counting {
        inner: FileSource::open(file.path()).unwrap(),
        bytes: Default::default(),
    };
    let m = Jpeg.measure(&source, 0).expect("a JPEG");
    assert_ne!(m.health, Health::Intact, "{m:?}");
    let read = source.bytes.load(std::sync::atomic::Ordering::Relaxed);
    // The contiguous walk's first window (1 MiB), the 64 MiB window pass, the tries' 64 MiB budget and one try's last
    // load past it.
    assert!(
        read <= (1 + 64 + 64 + 1) * MIB as u64,
        "{} MiB read",
        read / MIB as u64
    );
}
