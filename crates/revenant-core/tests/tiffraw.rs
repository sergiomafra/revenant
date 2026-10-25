use revenant_core::formats::{FileKind, Format, Health, TiffRaw, all_formats};
use revenant_core::source::{FileSource, Source, VolumeId};
use revenant_core::{CancelToken, EngineError, Event, ScanOptions, scan};
use revenant_testkit as testkit;
use testkit::{DiskImage, TestExif, TiffRawOptions, TiffVariant};

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

fn opts(variant: TiffVariant, big_endian: bool) -> TiffRawOptions {
    TiffRawOptions {
        variant,
        big_endian,
        raw_len: 120_000,
        seed: 7,
        preview: Some((160, 120)),
        preview_sector_aligned: false,
        exif: TestExif::default(),
    }
}

fn scan_all(source: &dyn Source) -> Vec<(u64, u64, FileKind, Health)> {
    let mut out = Vec::new();
    scan(
        source,
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

fn scan_image(img: &tempfile::NamedTempFile) -> Vec<(u64, u64, FileKind, Health)> {
    scan_all(&FileSource::open(img.path()).unwrap())
}

#[test]
fn every_variant_is_found_with_its_kind() {
    let cases = [
        (TiffVariant::Cr2, FileKind::Cr2),
        (TiffVariant::Nef, FileKind::Nef),
        (TiffVariant::Arw, FileKind::Arw),
        (TiffVariant::Dng, FileKind::Dng),
        (TiffVariant::Orf, FileKind::Orf),
        (TiffVariant::Rw2, FileKind::Rw2),
        (TiffVariant::Plain, FileKind::Tiff),
    ];
    for (variant, kind) in cases {
        for big_endian in [false, true] {
            let raw = testkit::tiff_raw(&opts(variant, big_endian));
            let img = DiskImage::noise(MIB, 3).put(8192, &raw.bytes).save();
            assert_eq!(
                scan_image(&img),
                vec![(8192, raw.bytes.len() as u64, kind, Health::Intact)],
                "{variant:?} be={big_endian}"
            );
        }
    }
}

#[test]
fn raw_with_sector_aligned_preview_is_one_file() {
    for variant in [TiffVariant::Cr2, TiffVariant::Nef, TiffVariant::Dng] {
        let mut o = opts(variant, false);
        o.preview_sector_aligned = true;
        let raw = testkit::tiff_raw(&o);
        assert_eq!(raw.preview.as_ref().unwrap().start % 512, 0);
        let img = DiskImage::noise(MIB, 4).put(4096, &raw.bytes).save();
        let found = scan_image(&img);
        assert_eq!(found.len(), 1, "{variant:?}: {found:?}");
        assert_eq!(found[0].3, Health::Intact);
        assert_eq!(found[0].1, raw.bytes.len() as u64);
    }
}

#[test]
fn raw_cut_by_next_file_is_partial() {
    let raw = testkit::tiff_raw(&opts(TiffVariant::Nef, false));
    let start = 8192usize;
    let cut = (start + raw.raw.start + 30_000).div_ceil(512) * 512;
    let jpeg = testkit::jpeg(64, 64, 9, 0);
    let img = DiskImage::noise(MIB, 5)
        .put(start, &raw.bytes)
        .put(cut, &jpeg)
        .save();
    assert_eq!(
        scan_image(&img),
        vec![
            (
                start as u64,
                (cut - start) as u64,
                FileKind::Nef,
                Health::Partial
            ),
            (
                cut as u64,
                jpeg.len() as u64,
                FileKind::Jpeg,
                Health::Intact
            ),
        ]
    );
}

#[test]
fn raw_cut_by_the_end_of_the_source_is_partial() {
    let raw = testkit::tiff_raw(&opts(TiffVariant::Arw, true));
    let keep = raw.bytes.len() - 5000;
    let img = DiskImage::zeros(8192 + keep)
        .put(8192, &raw.bytes[..keep])
        .save();
    let found = scan_image(&img);
    assert_eq!(
        found,
        vec![(8192, keep as u64, FileKind::Arw, Health::Partial)]
    );
}

#[test]
fn random_head_inside_raw_data_does_not_cut() {
    let raw = testkit::tiff_raw(&opts(TiffVariant::Cr2, false));
    let start = 8192usize;
    let at1 = (start + raw.raw.start + 10_000).div_ceil(512) * 512;
    let at2 = at1 + 1024;
    let at3 = at1 + 2048;
    let mut tiff_like = b"II*\0\x08\0\0\0".to_vec();
    tiff_like.extend([0xAA; 64]);
    let mut jpeg_like = vec![0xFF, 0xD8, 0xFF, 0xE1];
    jpeg_like.extend([0xAA; 64]);
    let mut big = b"MM\0*\0\0\0\x08".to_vec();
    big.extend([0xAA; 64]);
    let img = DiskImage::noise(MIB, 6)
        .put(start, &raw.bytes)
        .put(at1, &tiff_like)
        .put(at2, &jpeg_like)
        .put(at3, &big)
        .save();
    // The planted bytes are inside the file's raw data, so the file is no longer a clean copy of the original, but its
    // structure is complete and its preview still decodes.
    assert_eq!(
        scan_image(&img),
        vec![(
            start as u64,
            raw.bytes.len() as u64,
            FileKind::Cr2,
            Health::Intact
        )]
    );
}

/// A DNG without preview whose raw strip starts on a sector boundary (searching for a Model length that puts it there),
/// with its SubIFD's Compression set to `compression` and a JPEG written at the strip start.
fn dng_with_jpeg_at_an_aligned_strip(compression: u16, j: &[u8]) -> testkit::TiffRaw {
    for n in 0..1200usize {
        let mut o = opts(TiffVariant::Dng, false);
        o.preview = None;
        o.exif.model = Some("M".repeat(n));
        o.raw_len = 20_000;
        let mut raw = testkit::tiff_raw(&o);
        if !raw.raw.start.is_multiple_of(512) {
            continue;
        }
        let ifd0 = u32::from_le_bytes(raw.bytes[4..8].try_into().unwrap()) as usize;
        let sub = entry_value(&raw.bytes, ifd0, 0x014A) as usize;
        patch_entry(&mut raw.bytes, sub, 0x0103, u32::from(compression));
        let at = raw.raw.start;
        raw.bytes[at..at + j.len()].copy_from_slice(j);
        return raw;
    }
    panic!("no model length aligned the raw strip");
}

/// The 4-byte value field of the entry with `tag` in the little-endian IFD at `ifd`.
fn entry_value(bytes: &[u8], ifd: usize, tag: u16) -> u32 {
    let n = u16::from_le_bytes([bytes[ifd], bytes[ifd + 1]]) as usize;
    (0..n)
        .map(|i| ifd + 2 + 12 * i)
        .find(|&e| u16::from_le_bytes([bytes[e], bytes[e + 1]]) == tag)
        .map(|e| u32::from_le_bytes(bytes[e + 8..e + 12].try_into().unwrap()))
        .unwrap_or_else(|| panic!("tag {tag:#x} not found"))
}

#[test]
fn jpeg_compressed_strip_starting_on_a_sector_does_not_cut() {
    // Strips/tiles compressed as JPEG (6, 7, 34892: lossy JPEG tiles of a DNG) start with FF D8. When one starts on a
    // sector boundary, the scan must not take it for another file.
    let j = testkit::jpeg(16, 16, 11, 0);
    for compression in [6, 7, 34892] {
        let raw = dng_with_jpeg_at_an_aligned_strip(compression, &j);
        let img = DiskImage::noise(MIB, 8).put(8192, &raw.bytes).save();
        let found = scan_image(&img);
        // Without a preview the file is Partial (so the JPEG at its strip is reported too); what matters is that the
        // file is not cut there.
        assert_eq!(found[0].0, 8192, "{compression}: {found:?}");
        assert_eq!(
            found[0].1,
            raw.bytes.len() as u64,
            "{compression}: {found:?}"
        );
    }
}

/// An uncompressed strip never starts with a JPEG of its own: a JPEG written exactly at its start is another file, so it
/// cuts the RAW and is found on its own.
#[test]
fn foreign_jpeg_at_an_uncompressed_strip_start_cuts_the_raw() {
    let j = testkit::jpeg(16, 16, 11, 0);
    let raw = dng_with_jpeg_at_an_aligned_strip(1, &j);
    let img = DiskImage::noise(MIB, 8).put(8192, &raw.bytes).save();
    let found = scan_image(&img);
    assert_eq!(
        found[0],
        (8192, raw.raw.start as u64, FileKind::Dng, Health::Partial),
        "{found:?}"
    );
    assert!(
        found.contains(&(
            (8192 + raw.raw.start) as u64,
            j.len() as u64,
            FileKind::Jpeg,
            Health::Intact
        )),
        "{found:?}"
    );
}

#[test]
fn raw_without_preview_is_partial() {
    let mut o = opts(TiffVariant::Nef, false);
    o.preview = None;
    let raw = testkit::tiff_raw(&o);
    let img = DiskImage::noise(MIB, 4).put(4096, &raw.bytes).save();
    assert_eq!(
        scan_image(&img),
        vec![(4096, raw.bytes.len() as u64, FileKind::Nef, Health::Partial)]
    );
}

#[test]
fn preview_that_does_not_decode_cleanly_is_partial() {
    // Same corruptions as the JPEG tests: lenient-only decode, and no decode at all. The raw data may still be fine.
    let corruptions: [(&str, Vec<u8>); 2] = [
        ("lenient only", [0xFF, 0x01].repeat(4)),
        ("undecodable", [0xFF, 0xC4].repeat(32)),
    ];
    for (name, patch) in corruptions {
        let mut o = opts(TiffVariant::Nef, false);
        o.preview = Some((640, 480));
        let raw = testkit::tiff_raw(&o);
        let p = raw.preview.clone().unwrap();
        let mut bytes = raw.bytes.clone();
        let mid = (p.start + p.end) / 2;
        bytes[mid..mid + patch.len()].copy_from_slice(&patch);
        let img = DiskImage::noise(MIB, 4).put(4096, &bytes).save();
        let found = scan_image(&img);
        assert_eq!(found.len(), 1, "{name}: {found:?}");
        assert_eq!(found[0].3, Health::Partial, "{name}");
        assert_eq!(found[0].1, bytes.len() as u64, "{name}");
    }
}

/// Overwrites the 4-byte value of the entry with `tag` in the little-endian IFD at `ifd`.
fn patch_entry(bytes: &mut [u8], ifd: usize, tag: u16, value: u32) {
    let n = u16::from_le_bytes([bytes[ifd], bytes[ifd + 1]]) as usize;
    for i in 0..n {
        let e = ifd + 2 + 12 * i;
        if u16::from_le_bytes([bytes[e], bytes[e + 1]]) == tag {
            bytes[e + 8..e + 12].copy_from_slice(&value.to_le_bytes());
            return;
        }
    }
    panic!("tag {tag:#x} not found");
}

#[test]
fn garbled_structure_behind_a_valid_ifd0_is_damaged() {
    for (variant, ifd0) in [(TiffVariant::Nef, 8usize), (TiffVariant::Cr2, 16)] {
        let raw = testkit::tiff_raw(&opts(variant, false));
        let mut bytes = raw.bytes.clone();
        // The SubIFD pointer points back at IFD0: a cycle. IFD0 itself still confirms.
        patch_entry(&mut bytes, ifd0, 0x014A, ifd0 as u32);
        let img = DiskImage::noise(MIB, 4).put(4096, &bytes).save();
        let source = FileSource::open(img.path()).unwrap();
        assert!(TiffRaw.confirms(&source, 4096));
        let m = TiffRaw.measure(&source, 4096).unwrap();
        assert_eq!(m.health, Health::Damaged, "{variant:?}");
    }
}

#[test]
fn random_tiff_magic_on_noise_is_not_a_finding() {
    for (head, big) in [(b"II*\0", false), (b"MM\0*", true)] {
        let mut start = head.to_vec();
        let ifd0: u32 = 4096;
        start.extend(if big {
            ifd0.to_be_bytes()
        } else {
            ifd0.to_le_bytes()
        });
        let img = DiskImage::noise(MIB, 12).put(8192, &start).save();
        let source = FileSource::open(img.path()).unwrap();
        assert!(TiffRaw.recognizes(&start));
        assert!(TiffRaw.measure(&source, 8192).is_none());
        assert!(scan_all(&source).is_empty(), "{:?}", scan_all(&source));
    }
}

#[test]
fn structure_past_the_end_of_the_source_is_partial_not_damaged() {
    let mut o = opts(TiffVariant::Nef, false);
    o.exif.iso = Some(100);
    let raw = testkit::tiff_raw(&o);
    let mut bytes = raw.bytes.clone();
    // The Exif IFD lies beyond the end of the source.
    let beyond = (bytes.len() + 100_000) as u32;
    patch_entry(&mut bytes, 8, 0x8769, beyond);
    let img = DiskImage::zeros(8192 + bytes.len())
        .put(8192, &bytes)
        .save();
    let source = FileSource::open(img.path()).unwrap();
    let m = TiffRaw.measure(&source, 8192).unwrap();
    assert_eq!((m.len, m.health), (bytes.len() as u64, Health::Partial));
    assert_eq!(m.kind, FileKind::Nef);
}

/// Appends IFD1 (JPEGInterchangeFormat pointing at `second`) and `second` itself to a little-endian file whose IFD0 is
/// at 8, and chains IFD1 after IFD0.
fn with_second_preview(mut bytes: Vec<u8>, second: &[u8]) -> Vec<u8> {
    let n = u16::from_le_bytes([bytes[8], bytes[9]]) as usize;
    let next_at = 8 + 2 + 12 * n;
    let ifd1 = bytes.len() as u32;
    bytes[next_at..next_at + 4].copy_from_slice(&ifd1.to_le_bytes());
    let entry = |tag: u16, v: u32| {
        let mut e = Vec::new();
        e.extend(tag.to_le_bytes());
        e.extend(4u16.to_le_bytes());
        e.extend(1u32.to_le_bytes());
        e.extend(v.to_le_bytes());
        e
    };
    bytes.extend(2u16.to_le_bytes());
    bytes.extend(entry(0x0201, ifd1 + 30));
    bytes.extend(entry(0x0202, second.len() as u32));
    bytes.extend([0; 4]);
    bytes.extend_from_slice(second);
    bytes
}

fn health_with_previews(big: impl Fn(&mut [u8]), small: (u32, u32)) -> Health {
    let mut o = opts(TiffVariant::Nef, false);
    o.preview = Some((640, 480));
    let raw = testkit::tiff_raw(&o);
    let mut bytes = raw.bytes.clone();
    let p = raw.preview.clone().unwrap();
    big(&mut bytes[p]);
    let bytes = with_second_preview(bytes, &testkit::jpeg(small.0, small.1, 31, 0));
    let img = DiskImage::noise(MIB, 4).put(4096, &bytes).save();
    let found = scan_image(&img);
    assert_eq!(found[0].0, 4096, "{found:?}");
    assert_eq!(found[0].1, bytes.len() as u64, "{found:?}");
    found[0].3
}

#[test]
fn the_largest_preview_decides_the_health() {
    // Sanity: two valid previews.
    assert_eq!(health_with_previews(|_| {}, (32, 32)), Health::Intact);
    // (a) Big preview decodes only leniently, small one is valid: the big one decides.
    let lenient = |p: &mut [u8]| {
        let mid = p.len() / 2;
        p[mid..mid + 8].copy_from_slice(&[0xFF, 0x01].repeat(4));
    };
    assert_eq!(health_with_previews(lenient, (32, 32)), Health::Partial);
    // (b) Big preview with an unreadable SOF (size zero), small one valid: no thumbnail may vouch for the file.
    let no_size = |p: &mut [u8]| {
        let sof = p.windows(2).position(|w| w == [0xFF, 0xC0]).unwrap();
        p[sof + 5..sof + 9].fill(0);
    };
    assert_eq!(health_with_previews(no_size, (32, 32)), Health::Partial);
}

#[test]
fn unreadable_sector_in_raw_data_is_partial() {
    let raw = testkit::tiff_raw(&opts(TiffVariant::Nef, false));
    let start = 8192usize;
    let bad = (start + raw.raw.start + 20_000).div_ceil(512) * 512;
    let img = DiskImage::noise(MIB, 4).put(start, &raw.bytes).save();
    let source = FailingSource {
        inner: FileSource::open(img.path()).unwrap(),
        bad: (bad as u64, bad as u64 + 512),
    };
    // Through the detector alone (it must notice the sector while looking for the next file) ...
    let m = TiffRaw.measure(&source, start as u64).unwrap();
    assert_eq!(m.health, Health::Partial);
    assert_eq!(m.len, raw.bytes.len() as u64);
    // ... and through the scan.
    let found = scan_all(&source);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].3, Health::Partial);
}

#[test]
fn not_a_tiff_is_none() {
    let img = DiskImage::noise(MIB, 4).save();
    let source = FileSource::open(img.path()).unwrap();
    assert!(TiffRaw.measure(&source, 0).is_none());
    assert!(!TiffRaw.recognizes(b"II*\0\x04\0\0\0"));
    assert!(!TiffRaw.recognizes(b"II*\0"));
    assert!(TiffRaw.recognizes(b"II*\0\x08\0\0\0"));
    assert!(TiffRaw.recognizes(b"MM\0*\x00\x10\0\0"));
    assert!(!TiffRaw.recognizes(b"MM\0*\x10\0\0\0")); // IFD0 at 256 MiB
}

#[test]
fn bad_offset_does_not_claim_the_rest_of_the_source() {
    let raw = testkit::tiff_raw(&opts(TiffVariant::Nef, false));
    let mut bytes = raw.bytes.clone();
    // Find the SubIFD (IFD0 is at 8) and point its only strip far past the end of the source.
    let n = u16::from_le_bytes([bytes[8], bytes[9]]) as usize;
    let sub = (0..n)
        .map(|i| 8 + 2 + 12 * i)
        .find(|&e| u16::from_le_bytes([bytes[e], bytes[e + 1]]) == 0x014A)
        .map(|e| u32::from_le_bytes(bytes[e + 8..e + 12].try_into().unwrap()) as usize)
        .unwrap();
    patch_entry(&mut bytes, sub, 0x0111, 0x7FFF_0000);
    let img = DiskImage::noise(64 * MIB, 13).put(0, &bytes).save();
    let source = FileSource::open(img.path()).unwrap();
    let m = TiffRaw.measure(&source, 0).unwrap();
    // 0x7FFF_0000 lies past the 1 GiB cap: malformed, wherever the source ends.
    assert_eq!(m.health, Health::Damaged);
    // The bad strip is not counted: the file ends where its real structure does, not at the end of the source.
    assert!(m.len <= bytes.len() as u64, "len {}", m.len);
    assert!(m.len >= raw.preview.unwrap().end as u64);
}

#[test]
fn raw_cut_by_source_end_keeps_partial_strip() {
    let raw = testkit::tiff_raw(&opts(TiffVariant::Nef, false));
    let keep = raw.raw.start + 50_000; // the strip starts inside the source and ends past it
    let img = DiskImage::zeros(4096 + keep)
        .put(4096, &raw.bytes[..keep])
        .save();
    let source = FileSource::open(img.path()).unwrap();
    let m = TiffRaw.measure(&source, 4096).unwrap();
    assert_eq!((m.len, m.health), (keep as u64, Health::Partial));
}

fn rw2(raw_format: u16, bits: u16, raw_len: usize) -> testkit::TiffRaw {
    rw2_with_strip_len(raw_format, bits, raw_len, 0)
}

fn rw2_with_strip_len(
    raw_format: u16,
    bits: u16,
    raw_len: usize,
    strip_len: u32,
) -> testkit::TiffRaw {
    testkit::panasonic_rw2(&testkit::Rw2Options {
        width: 280,
        height: 200,
        raw_format,
        bits,
        raw_len,
        strip_len,
        seed: 3,
        exif: TestExif::default(),
    })
}

/// Real RW2s (DMC-LX7, DC-G100, DC-GH5S) give no strip length: StripOffsets is 0xFFFFFFFF, the raw data starts at tag
/// 0x0118 and runs to the end of the file in 16-byte packets of 14 (format 4, 12-bit) or 9 (format 5, 14-bit) pixels,
/// rounded up to 0x4000-byte blocks. The preview (tag 0x002E) sits on a sector start.
#[test]
fn panasonic_rw2_raw_length_comes_from_the_sensor_size() {
    // 280 x 200 = 56000 pixels.
    for (format, bits, raw_len) in [(4, 12, 65_536), (5, 14, 114_688)] {
        let f = rw2(format, bits, raw_len);
        let mut img = DiskImage::noise(8192 + f.bytes.len() + 8192, 21);
        img.put(8192, &f.bytes);
        assert_eq!(
            scan_image(&img.save()),
            vec![(8192, f.bytes.len() as u64, FileKind::Rw2, Health::Intact)],
            "format {format}, {bits} bits"
        );
    }
}

/// A DMC-FZ28 writes StripOffsets 0xFFFFFFFF with a nonzero StripByteCounts; with the raw data offset in 0x0118 the strip
/// tags mean nothing (otherwise the range past the source makes the file partial, or damaged on a source over 1 GiB).
#[test]
fn panasonic_rw2_ignores_the_placeholder_strip() {
    let f = rw2_with_strip_len(4, 12, 65_536, 15_447_152);
    let mut img = DiskImage::noise(8192 + f.bytes.len() + 8192, 21);
    img.put(8192, &f.bytes);
    assert_eq!(
        scan_image(&img.save()),
        vec![(8192, f.bytes.len() as u64, FileKind::Rw2, Health::Intact)]
    );
}

/// Upper bound the engine claims for Panasonic raw data of an unknown packing (ruling P16): 2 bytes per pixel, rounded up
/// to the 0x4000-byte block. 280 x 200 = 56,000 pixels -> 112,000 -> 114,688.
const RW2_UNKNOWN_BOUND: u64 = 114_688;

/// Scan result of an RW2 with an unknown packing: the RW2 itself, claimed up to the upper bound (ruling P16), alone.
/// The scan keeps examining heads inside a `Partial` finding (one broken file must not hide others), but the preview,
/// which starts on a sector, is the RW2's own JpgFromRaw and is not reported on its own (ruling D12).
fn assert_unknown_packing_keeps_the_raw_data(
    f: &testkit::TiffRaw,
    found: &[(u64, u64, FileKind, Health)],
) {
    assert_eq!(
        found,
        [(
            8192,
            f.raw.start as u64 + RW2_UNKNOWN_BOUND,
            FileKind::Rw2,
            Health::Partial
        )]
    );
}

/// Packings not checked against a real file (ruling P14), here format 5 at 12 bits (it would be 10 pixels per packet,
/// 98,304 bytes), give an unknown length: Partial, but the raw data is kept up to the upper bound (ruling P16).
#[test]
fn panasonic_rw2_with_an_unverified_packing_is_partial() {
    let f = rw2(5, 12, 98_304);
    let mut img = DiskImage::noise(8192 + f.bytes.len() + 32_768, 21);
    img.put(8192, &f.bytes);
    assert_unknown_packing_keeps_the_raw_data(&f, &scan_image(&img.save()));
}

/// Overwrites tag `tag` of the little-endian IFD at `ifd` with a LONG holding `value`.
fn patch_long(bytes: &mut [u8], ifd: usize, tag: u16, value: u32) {
    let n = u16::from_le_bytes([bytes[ifd], bytes[ifd + 1]]) as usize;
    for i in 0..n {
        let e = ifd + 2 + 12 * i;
        if u16::from_le_bytes([bytes[e], bytes[e + 1]]) == tag {
            bytes[e + 2..e + 4].copy_from_slice(&4u16.to_le_bytes());
            bytes[e + 4..e + 8].copy_from_slice(&1u32.to_le_bytes());
            bytes[e + 8..e + 12].copy_from_slice(&value.to_le_bytes());
            return;
        }
    }
    panic!("tag {tag:#x} not found");
}

/// Hostile sensor sizes (LONG 0xFFFFFFFF, or a product past any real sensor) must not overflow: the length is unknown.
#[test]
fn panasonic_rw2_with_a_hostile_sensor_size_is_partial() {
    for (w, h) in [(u32::MAX, u32::MAX), (100_000, 100_000)] {
        let mut f = rw2(4, 12, 65_536);
        let ifd0 = u32::from_le_bytes(f.bytes[4..8].try_into().unwrap()) as usize;
        patch_long(&mut f.bytes, ifd0, 0x0002, w);
        patch_long(&mut f.bytes, ifd0, 0x0003, h);
        let mut img = DiskImage::noise(8192 + f.bytes.len() + 8192, 21);
        img.put(8192, &f.bytes);
        let found = scan_image(&img.save());
        assert_eq!(
            (found[0].0, found[0].2),
            (8192, FileKind::Rw2),
            "{w}x{h}: {found:?}"
        );
        assert_eq!(found[0].3, Health::Partial, "{w}x{h}: {found:?}");
    }
}

/// The newer bodies' raw formats (6, 7) are not verified: the RW2 is Partial, but its raw data is claimed up to the upper
/// bound (ruling P16), so the recovered file keeps it.
#[test]
fn panasonic_rw2_with_an_unknown_raw_format_is_partial() {
    for format in [6, 7] {
        let f = rw2(format, 14, 100_000);
        let mut img = DiskImage::noise(8192 + f.bytes.len() + 32_768, 21);
        img.put(8192, &f.bytes);
        assert_unknown_packing_keeps_the_raw_data(&f, &scan_image(&img.save()));
    }
}

/// The upper bound is only a bound: the next file inside it, or the end of the source, ends the RW2.
#[test]
fn panasonic_rw2_upper_bound_is_trimmed_by_the_next_file_and_the_source_end() {
    let f = rw2(6, 12, 100_000);
    let next = (8192 + f.bytes.len()).next_multiple_of(512);
    assert!(((next - 8192) as u64) < f.raw.start as u64 + RW2_UNKNOWN_BOUND);
    let jpeg = testkit::jpeg(32, 32, 5, 0);
    let mut img = DiskImage::noise(next + 65_536, 21);
    img.put(8192, &f.bytes).put(next, &jpeg);
    let found = scan_image(&img.save());
    assert_eq!(
        found[0],
        (8192, (next - 8192) as u64, FileKind::Rw2, Health::Partial),
        "{found:?}"
    );
    assert_eq!(
        found.last(),
        Some(&(
            next as u64,
            jpeg.len() as u64,
            FileKind::Jpeg,
            Health::Intact
        )),
        "{found:?}"
    );

    let mut img = DiskImage::noise(8192 + f.bytes.len() + 1000, 21);
    img.put(8192, &f.bytes);
    let found = scan_image(&img.save());
    assert_eq!(
        found[0],
        (
            8192,
            f.bytes.len() as u64 + 1000,
            FileKind::Rw2,
            Health::Partial
        ),
        "{found:?}"
    );
}

/// Real ORFs (E-PL2, E-M10 Mark II, TG-7) keep a thumbnail and the preview inside the MakerNote, both on sector starts.
#[test]
fn olympus_makernote_jpegs_do_not_cut_the_orf() {
    for om_system in [false, true] {
        let f = testkit::olympus_orf(om_system, 50_000, 4, &TestExif::default());
        let mut img = DiskImage::noise(8192 + f.bytes.len() + 8192, 22);
        img.put(8192, &f.bytes);
        assert_eq!(
            scan_image(&img.save()),
            vec![(8192, f.bytes.len() as u64, FileKind::Orf, Health::Intact)],
            "om_system {om_system}"
        );
    }
}

/// A JPEG written inside one of the RAW's own smaller JPEGs (here the MakerNote thumbnail, clear of the preview) breaks
/// that JPEG: it is another file, so it cuts the ORF and is found on its own (ruling P13, narrowed).
#[test]
fn foreign_jpeg_inside_an_own_thumbnail_cuts_the_orf() {
    let mut f = testkit::olympus_orf(false, 50_000, 4, &TestExif::default());
    // The thumbnail is the testkit's 64x48 JPEG for this seed, the first JPEG on a sector start in the file.
    let thumb_len = testkit::jpeg(64, 48, 4, 0).len();
    let thumb = (0..f.bytes.len())
        .step_by(512)
        .find(|&p| f.bytes[p..].starts_with(&[0xFF, 0xD8, 0xFF]))
        .unwrap();
    let foreign = testkit::jpeg(8, 8, 77, 0);
    let at = thumb + 512;
    assert!(
        at + foreign.len() < thumb + thumb_len,
        "fits inside the thumbnail"
    );
    f.bytes[at..at + foreign.len()].copy_from_slice(&foreign);
    let mut img = DiskImage::noise(8192 + f.bytes.len() + 8192, 22);
    img.put(8192, &f.bytes);
    let found = scan_image(&img.save());
    assert_eq!(
        found[0],
        (8192, at as u64, FileKind::Orf, Health::Partial),
        "{found:?}"
    );
    assert!(
        found.contains(&(
            (8192 + at) as u64,
            foreign.len() as u64,
            FileKind::Jpeg,
            Health::Intact
        )),
        "{found:?}"
    );
}

/// A preview inside the MakerNote that does not decode cleanly makes the ORF partial: the MakerNote preview is the
/// content check.
#[test]
fn olympus_makernote_preview_decides_the_health() {
    let mut f = testkit::olympus_orf(false, 50_000, 4, &TestExif::default());
    let p = f.preview.clone().unwrap();
    // Lenient-only decode, as in `preview_that_does_not_decode_cleanly_is_partial`.
    let mid = (p.start + p.end) / 2;
    f.bytes[mid..mid + 8].copy_from_slice(&[0xFF, 0x01].repeat(4));
    let mut img = DiskImage::noise(8192 + f.bytes.len() + 8192, 22);
    img.put(8192, &f.bytes);
    let found = scan_image(&img.save());
    assert_eq!(found[0].0, 8192, "{found:?}");
    assert_eq!(found[0].1, f.bytes.len() as u64, "{found:?}");
    assert_eq!(found[0].3, Health::Partial, "{found:?}");
}

/// Bytes from `data`, zeros past it, up to `len` (a multi-GiB source without the disk space).
struct SparseSource {
    data: Vec<u8>,
    len: u64,
}

impl Source for SparseSource {
    fn len(&self) -> u64 {
        self.len
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, EngineError> {
        let n = (self.len.saturating_sub(offset) as usize).min(buf.len());
        for (i, b) in buf[..n].iter_mut().enumerate() {
            *b = self
                .data
                .get(offset as usize + i)
                .copied()
                .unwrap_or_default();
        }
        Ok(n)
    }

    fn volume(&self) -> Option<VolumeId> {
        None
    }
}

/// An ARW whose raw StripOffsets is patched to `offset`.
fn arw_with_strip_offset(offset: u32) -> Vec<u8> {
    let raw = testkit::tiff_raw(&opts(TiffVariant::Arw, false));
    let mut bytes = raw.bytes.clone();
    let n = u16::from_le_bytes([bytes[8], bytes[9]]) as usize;
    let sub = (0..n)
        .map(|i| 8 + 2 + 12 * i)
        .find(|&e| u16::from_le_bytes([bytes[e], bytes[e + 1]]) == 0x014A)
        .map_or(8, |e| {
            u32::from_le_bytes(bytes[e + 8..e + 12].try_into().unwrap()) as usize
        });
    patch_entry(&mut bytes, sub, 0x0111, offset);
    bytes
}

#[test]
fn corrupt_offset_past_the_cap_is_damaged_wherever_the_source_ends() {
    let bytes = arw_with_strip_offset(0x5000_0000); // 1.25 GiB, past the 1 GiB cap
    for len in [bytes.len() as u64 + 4096, 3 << 30] {
        let source = SparseSource {
            data: bytes.clone(),
            len,
        };
        let m = TiffRaw.measure(&source, 0).unwrap();
        assert_eq!(m.health, Health::Damaged, "source of {len} bytes");
    }
}

#[test]
fn offset_past_the_source_end_below_the_cap_is_partial() {
    let bytes = arw_with_strip_offset(bytes_len_plus(1 << 20));
    let source = SparseSource {
        data: bytes.clone(),
        len: bytes.len() as u64 + 4096,
    };
    let m = TiffRaw.measure(&source, 0).unwrap();
    assert_eq!(m.health, Health::Partial);
}

fn bytes_len_plus(extra: u32) -> u32 {
    testkit::tiff_raw(&opts(TiffVariant::Arw, false))
        .bytes
        .len() as u32
        + extra
}

#[test]
fn ifd_offset_past_the_cap_is_damaged_wherever_the_source_ends() {
    let raw = testkit::tiff_raw(&opts(TiffVariant::Arw, false));
    let mut bytes = raw.bytes.clone();
    // IFD0's SubIFD pointer (or its next-IFD link) now points to an IFD past the 1 GiB cap.
    patch_entry(&mut bytes, 8, 0x014A, 0x5000_0000);
    for len in [bytes.len() as u64 + 4096, 3 << 30] {
        let source = SparseSource {
            data: bytes.clone(),
            len,
        };
        let m = TiffRaw.measure(&source, 0).unwrap();
        assert_eq!(m.health, Health::Damaged, "source of {len} bytes");
    }
}
