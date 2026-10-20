use revenant_core::EngineError;
use revenant_core::formats::{FileKind, Format, Health, IsoBmff, Measurement};
use revenant_core::source::{FileSource, Source, VolumeId};
use revenant_testkit as testkit;
use testkit::{DiskImage, Mp4Options};

const AT: usize = 8192;

fn opts() -> Mp4Options {
    Mp4Options {
        mdat_len: 50_000,
        moov_at_end: true,
        mov: false,
        mdat_64bit: false,
    }
}

fn measure(img: &tempfile::NamedTempFile) -> Option<Measurement> {
    IsoBmff.measure(&FileSource::open(img.path()).unwrap(), AT as u64)
}

#[test]
fn mp4_moov_at_end_is_intact() {
    let v = testkit::mp4(&opts());
    let img = DiskImage::noise(AT + v.len() + 20_000, 1)
        .put(AT, &v)
        .save();
    assert_eq!(
        measure(&img),
        Some(Measurement {
            len: v.len() as u64,
            kind: FileKind::Mp4,
            health: Health::Intact
        })
    );
}

#[test]
fn moov_first_is_intact() {
    let v = testkit::mp4(&Mp4Options {
        moov_at_end: false,
        ..opts()
    });
    let img = DiskImage::noise(AT + v.len() + 20_000, 2)
        .put(AT, &v)
        .save();
    let m = measure(&img).unwrap();
    assert_eq!((m.len, m.health), (v.len() as u64, Health::Intact));
}

#[test]
fn mov_is_recognized_as_mov() {
    let v = testkit::mp4(&Mp4Options {
        mov: true,
        ..opts()
    });
    let img = DiskImage::noise(AT + v.len() + 20_000, 3)
        .put(AT, &v)
        .save();
    let m = measure(&img).unwrap();
    assert_eq!((m.kind, m.len), (FileKind::Mov, v.len() as u64));
}

#[test]
fn largesize_64_bit() {
    let v = testkit::mp4(&Mp4Options {
        mdat_64bit: true,
        ..opts()
    });
    let img = DiskImage::noise(AT + v.len() + 20_000, 4)
        .put(AT, &v)
        .save();
    let m = measure(&img).unwrap();
    assert_eq!((m.len, m.health), (v.len() as u64, Health::Intact));
}

#[test]
fn missing_moov_is_damaged() {
    let v = testkit::mp4(&opts());
    // ftyp is 20 bytes, then mdat (8-byte header); the moov that follows is cut away.
    let mdat_end = 20 + 8 + 50_000;
    let img = DiskImage::noise(AT + v.len() + 20_000, 5)
        .put(AT, &v[..mdat_end])
        .save();
    let m = measure(&img).unwrap();
    assert_eq!((m.len, m.health), (mdat_end as u64, Health::Damaged));
}

#[test]
fn truncated_mdat_is_partial() {
    let v = testkit::mp4(&Mp4Options {
        moov_at_end: false,
        ..opts()
    });
    let cut = v.len() - 20_000;
    let img = DiskImage::zeros(AT + cut).put(AT, &v[..cut]).save();
    let m = measure(&img).unwrap();
    assert_eq!((m.len, m.health), (cut as u64, Health::Partial));
}

#[test]
fn chunk_past_measured_end_is_partial() {
    // Complete boxes, but the track claims more sample data than the file holds.
    let mut bytes = testkit::mp4(&opts());
    let n = bytes.len();
    let at = bytes.windows(4).position(|w| w == b"stsz").unwrap() + 4 + 4 + 4 + 4;
    bytes[at..at + 4].copy_from_slice(&900_000u32.to_be_bytes());
    let img = DiskImage::noise(AT + n + 20_000, 6).put(AT, &bytes).save();
    let m = measure(&img).unwrap();
    assert_eq!((m.len, m.health), (n as u64, Health::Partial));
}

#[test]
fn stray_ftyp_is_not_video() {
    let v = testkit::mp4(&opts());
    let img = DiskImage::noise(AT + 100_000, 7).put(AT, &v[..20]).save();
    assert_eq!(measure(&img), None);
}

#[test]
fn hostile_sizes_do_not_hang_or_allocate() {
    // ftyp, then boxes of size 1 with a huge largesize, and a moov of 4 GiB-ish.
    let mut v = testkit::mp4(&opts())[..20].to_vec();
    v.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xF0]);
    v.extend_from_slice(b"moov");
    v.extend_from_slice(&[0; 64]);
    let img = DiskImage::zeros(AT + 4096).put(AT, &v).save();
    assert_eq!(measure(&img), None);
}

#[test]
fn oversized_moov_is_damaged() {
    let mut v = testkit::mp4(&opts())[..28 + 50_000].to_vec();
    v.extend_from_slice(&(20u32 << 20).to_be_bytes());
    v.extend_from_slice(b"moov");
    let img = DiskImage::zeros(AT + v.len() + (20 << 20))
        .put(AT, &v)
        .save();
    assert_eq!(measure(&img).unwrap().health, Health::Damaged);
}

fn find(bytes: &[u8], tag: &[u8; 4]) -> usize {
    bytes.windows(4).position(|w| w == tag).unwrap()
}

fn sector_aligned(o: &Mp4Options) -> Vec<u8> {
    let first = testkit::mp4(o);
    let pad = (512 - first.len() % 512) % 512;
    let v = testkit::mp4(&Mp4Options {
        mdat_len: o.mdat_len + pad,
        ..*o
    });
    assert_eq!(v.len() % 512, 0);
    v
}

#[test]
fn back_to_back_videos_are_separate() {
    let a = sector_aligned(&opts());
    for mov in [false, true] {
        let b = testkit::mp4(&Mp4Options {
            mov,
            mdat_len: 20_000,
            ..opts()
        });
        let img = DiskImage::noise(AT + a.len() + b.len() + 4096, 8)
            .put(AT, &a)
            .put(AT + a.len(), &b)
            .save();
        let m = measure(&img).unwrap();
        assert_eq!(
            (m.len, m.kind, m.health),
            (a.len() as u64, FileKind::Mp4, Health::Intact)
        );
    }
}

#[test]
fn slack_spelling_free_with_huge_size_is_ignored() {
    let v = testkit::mp4(&opts());
    let mut tail = 0x7FFF_FFF0u32.to_be_bytes().to_vec();
    tail.extend_from_slice(b"free");
    let img = DiskImage::noise(AT + v.len() + 8, 9)
        .put(AT, &v)
        .put(AT + v.len(), &tail)
        .save();
    let m = measure(&img).unwrap();
    assert_eq!((m.len, m.health), (v.len() as u64, Health::Intact));
}

#[test]
fn chunk_offset_at_end_alone_is_partial() {
    // Zero-size sample so only the `last >= end` clause can fire.
    let mut v = testkit::mp4(&opts());
    let n = v.len();
    let stsz = find(&v, b"stsz");
    v[stsz + 16..stsz + 20].copy_from_slice(&0u32.to_be_bytes());
    let stco = find(&v, b"stco");
    v[stco + 12..stco + 16].copy_from_slice(&(n as u32).to_be_bytes());
    let img = DiskImage::noise(AT + n + 4096, 10).put(AT, &v).save();
    assert_eq!(measure(&img).unwrap().health, Health::Partial);
}

#[test]
fn co64_offset_past_end_is_partial() {
    let mut v = testkit::mp4(&Mp4Options {
        mdat_64bit: true,
        ..opts()
    });
    let n = v.len();
    let co64 = find(&v, b"co64");
    v[co64 + 12..co64 + 20].copy_from_slice(&(n as u64 + 1_000_000).to_be_bytes());
    let img = DiskImage::noise(AT + n + 4096, 11).put(AT, &v).save();
    assert_eq!(measure(&img).unwrap().health, Health::Partial);
}

fn ftyp_mdat_prefix() -> Vec<u8> {
    testkit::mp4(&opts())[..28 + 50_000].to_vec()
}

#[test]
fn largesize_below_16_stops_the_walk() {
    let mut v = ftyp_mdat_prefix();
    let valid = v.len();
    v.extend_from_slice(&1u32.to_be_bytes());
    v.extend_from_slice(b"moov");
    v.extend_from_slice(&8u64.to_be_bytes());
    v.extend_from_slice(&[0; 64]);
    let img = DiskImage::zeros(AT + v.len()).put(AT, &v).save();
    let m = measure(&img).unwrap();
    assert_eq!((m.len, m.health), (valid as u64, Health::Damaged));
}

#[test]
fn huge_largesize_counts_only_to_source_end() {
    let mut v = ftyp_mdat_prefix();
    v.extend_from_slice(&1u32.to_be_bytes());
    v.extend_from_slice(b"moov");
    v.extend_from_slice(&(u64::MAX - 15).to_be_bytes());
    v.extend_from_slice(&[0; 64]);
    let img = DiskImage::zeros(AT + v.len()).put(AT, &v).save();
    let m = measure(&img).unwrap();
    assert_eq!((m.len, m.health), (v.len() as u64, Health::Partial));
}

#[test]
fn child_larger_than_parent_is_damaged_without_panic() {
    let mut v = testkit::mp4(&opts());
    let trak = find(&v, b"trak");
    v[trak - 4..trak].copy_from_slice(&0x00FF_FFFFu32.to_be_bytes());
    let img = DiskImage::noise(AT + v.len() + 4096, 12).put(AT, &v).save();
    // A garbled index is unusable: Damaged, with the length still exact.
    let m = measure(&img).unwrap();
    assert_eq!((m.len, m.health), (v.len() as u64, Health::Damaged));
}

#[test]
fn moov_without_sample_tables_is_damaged() {
    let ftyp = bx(b"ftyp", b"isom\0\0\0\0isom");
    let v = [
        ftyp,
        bx(b"mdat", &[7u8; 400]),
        bx(b"moov", &bx(b"mvhd", &[0; 100])),
    ]
    .concat();
    let img = DiskImage::noise(AT + v.len() + 4096, 14).put(AT, &v).save();
    let m = measure(&img).unwrap();
    assert_eq!((m.len, m.health), (v.len() as u64, Health::Damaged));
}

fn bx(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut o = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
    o.extend_from_slice(kind);
    o.extend_from_slice(payload);
    o
}

fn trak(chunks: [u32; 2], sizes: [u32; 2]) -> Vec<u8> {
    let mut stco = vec![0; 4];
    stco.extend_from_slice(&2u32.to_be_bytes());
    chunks
        .iter()
        .for_each(|c| stco.extend_from_slice(&c.to_be_bytes()));
    let mut stsz = vec![0; 8];
    stsz.extend_from_slice(&2u32.to_be_bytes());
    sizes
        .iter()
        .for_each(|s| stsz.extend_from_slice(&s.to_be_bytes()));
    let stbl = bx(b"stbl", &[bx(b"stsz", &stsz), bx(b"stco", &stco)].concat());
    bx(b"trak", &bx(b"mdia", &bx(b"minf", &stbl)))
}

#[test]
fn interleaved_two_track_file_is_intact() {
    let ftyp = bx(b"ftyp", b"isom\0\0\0\0isom");
    let moov_len = bx(
        b"moov",
        &[trak([0; 2], [0; 2]), trak([0; 2], [0; 2])].concat(),
    )
    .len() as u32;
    let base = (ftyp.len() as u32) + moov_len + 8; // payload offset when moov precedes mdat
    // video, audio, video, audio; 100 bytes each
    let moov = bx(
        b"moov",
        &[
            trak([base, base + 200], [100, 100]),
            trak([base + 100, base + 300], [100, 100]),
        ]
        .concat(),
    );
    let v = [ftyp, moov, bx(b"mdat", &[7u8; 400])].concat();
    let img = DiskImage::noise(AT + v.len() + 4096, 13).put(AT, &v).save();
    let m = measure(&img).unwrap();
    assert_eq!((m.len, m.health), (v.len() as u64, Health::Intact));
}

/// Appends `extra` inside a box and fixes its size field.
fn pad_box(b: &[u8], extra: &[u8]) -> Vec<u8> {
    let mut o = b.to_vec();
    o.extend_from_slice(extra);
    let n = o.len() as u32;
    o[..4].copy_from_slice(&n.to_be_bytes());
    o
}

fn video_with_trailing(extra: &[u8]) -> Vec<u8> {
    let ftyp = bx(b"ftyp", b"isom\0\0\0\0isom");
    let t = pad_box(&trak([0, 0], [0, 0]), &[]);
    let moov_len = pad_box(&bx(b"moov", &pad_box(&t, extra)), extra).len() as u32;
    let base = ftyp.len() as u32 + moov_len + 8;
    let t = pad_box(&trak([base, base + 100], [100, 100]), extra);
    let moov = pad_box(&bx(b"moov", &t), extra);
    [ftyp, moov, bx(b"mdat", &[7u8; 200])].concat()
}

#[test]
fn zero_terminator_in_moov_is_tolerated() {
    let v = video_with_trailing(&[0; 4]);
    let img = DiskImage::noise(AT + v.len() + 4096, 15).put(AT, &v).save();
    let m = measure(&img).unwrap();
    assert_eq!((m.len, m.health), (v.len() as u64, Health::Intact));
}

#[test]
fn nonzero_trailing_bytes_in_moov_are_damaged() {
    let v = video_with_trailing(&[0, 0, 0, 1]);
    let img = DiskImage::noise(AT + v.len() + 4096, 16).put(AT, &v).save();
    let m = measure(&img).unwrap();
    assert_eq!((m.len, m.health), (v.len() as u64, Health::Damaged));
}

/// Fails every read that touches `[bad, bad + 512)`.
struct BadSector {
    inner: FileSource,
    bad: u64,
}

impl Source for BadSector {
    fn len(&self) -> u64 {
        self.inner.len()
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, EngineError> {
        if offset < self.bad + 512 && offset + buf.len() as u64 > self.bad {
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

#[test]
fn complete_video_followed_by_unreadable_sector_is_intact() {
    let v = sector_aligned(&opts());
    let img = DiskImage::zeros(AT + v.len() + 4096).put(AT, &v).save();
    let source = BadSector {
        inner: FileSource::open(img.path()).unwrap(),
        bad: (AT + v.len()) as u64,
    };
    let m = IsoBmff.measure(&source, AT as u64).unwrap();
    assert_eq!((m.len, m.health), (v.len() as u64, Health::Intact));
}

/// A moov-at-end MOV whose mdat holds a JPEG frame; one frame starts on a sector start of the image
/// and is listed as a chunk (payload offset 484) in the index.
fn mjpeg_layout(codec: &[u8; 4], extra_chunk: Option<u64>) -> (Vec<u8>, Vec<u8>) {
    let frame = testkit::jpeg(64, 64, 21, 2000);
    // The payload starts at file offset 28; AT is a sector start, so payload offset 484 is a sector start too.
    let mut payload = vec![0x11u8; 484];
    payload.extend_from_slice(&frame);
    payload.resize(payload.len() + 3000, 0x22);
    let video = testkit::mp4_with(
        &Mp4Options {
            mov: true,
            ..opts()
        },
        codec,
        &payload,
        &[&[0, 484][..], extra_chunk.as_slice()].concat(),
    );
    (video, frame)
}

#[test]
fn motion_jpeg_frames_do_not_cut_the_video() {
    let (video, _) = mjpeg_layout(b"mjpa", None);
    let img = DiskImage::noise(AT + video.len() + 20_000, 11)
        .put(AT, &video)
        .save();
    let m = measure(&img).unwrap();
    assert_eq!(
        (m.len, m.kind, m.health),
        (video.len() as u64, FileKind::Mov, Health::Intact)
    );
}

#[test]
fn h264_video_is_still_cut_by_a_real_jpeg() {
    let (video, _) = mjpeg_layout(b"avc1", None);
    let img = DiskImage::noise(AT + video.len() + 20_000, 12)
        .put(AT, &video)
        .save();
    let m = measure(&img).unwrap();
    // The cut lands before the moov, so the index is gone.
    assert_eq!((m.len, m.health), (512, Health::Damaged));
}

#[test]
fn motion_jpeg_exception_needs_an_intact_index() {
    // Same MJPEG video, but the index also lists a chunk far past the end of the file: not Intact, so the cut stays.
    let (video, _) = mjpeg_layout(b"mjpa", Some(10_000_000));
    let img = DiskImage::noise(AT + video.len() + 20_000, 13)
        .put(AT, &video)
        .save();
    let m = measure(&img).unwrap();
    assert_eq!((m.len, m.health), (512, Health::Damaged));
}

#[test]
fn confirms_a_header_in_an_unreadable_sector_like_jpeg_does() {
    // ftyp of 512 bytes, so the next box header is the first thing in the following sector.
    let mut v = 512u32.to_be_bytes().to_vec();
    v.extend_from_slice(b"ftypisom");
    v.resize(512, 0);
    let mut moov = 8u32.to_be_bytes().to_vec();
    moov.extend_from_slice(b"free");
    v.extend_from_slice(&moov);
    let img = DiskImage::zeros(AT + 4096).put(AT, &v).save();
    let source = BadSector {
        inner: FileSource::open(img.path()).unwrap(),
        bad: (AT + 512) as u64,
    };
    // Not provably a non-file: an unreadable sector does not refute it.
    assert!(IsoBmff.confirms(&source, AT as u64));
    // Readable, an ftyp + free alone is not a video (`measure` rejects it); with an mdat it is; readable zeros are not.
    assert!(!IsoBmff.confirms(&FileSource::open(img.path()).unwrap(), AT as u64));
    let mut with_mdat = v.clone();
    with_mdat.splice(512.., bx(b"mdat", &[1; 32]));
    let img2 = DiskImage::zeros(AT + 4096).put(AT, &with_mdat).save();
    assert!(IsoBmff.confirms(&FileSource::open(img2.path()).unwrap(), AT as u64));
    let img = DiskImage::zeros(AT + 4096).put(AT, &v[..512]).save();
    assert!(!IsoBmff.confirms(&FileSource::open(img.path()).unwrap(), AT as u64));
}

fn exif() -> testkit::TestExif {
    testkit::TestExif {
        make: Some("Canon".into()),
        model: Some("EOS R6".into()),
        iso: Some(400),
        ..testkit::TestExif::default()
    }
}

fn place(bytes: &[u8]) -> tempfile::NamedTempFile {
    DiskImage::noise(AT + bytes.len() + 40_000, 11)
        .put(AT, bytes)
        .save()
}

fn pos(bytes: &[u8], needle: &[u8]) -> usize {
    bytes
        .windows(needle.len())
        .position(|w| w == needle)
        .unwrap()
}

fn scan_kinds(img: &tempfile::NamedTempFile) -> Vec<(u64, u64, FileKind, Health)> {
    use revenant_core::formats::all_formats;
    use revenant_core::{CancelToken, Event, ScanOptions, scan};
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

#[test]
fn mp4_and_mov_kinds_unchanged() {
    for (mov, kind) in [(false, FileKind::Mp4), (true, FileKind::Mov)] {
        let v = testkit::mp4(&Mp4Options { mov, ..opts() });
        let m = measure(&place(&v)).unwrap();
        assert_eq!(
            (m.kind, m.len, m.health),
            (kind, v.len() as u64, Health::Intact)
        );
    }
    assert_eq!(FileKind::Cr3.extension(), "cr3");
    assert_eq!(FileKind::Heic.as_str(), "heic");
}

#[test]
fn mp4_listing_mif1_as_compatible_stays_mp4() {
    let mut v = testkit::mp4(&opts());
    assert_eq!(&v[16..20], b"isom"); // the compatible brand
    v[16..20].copy_from_slice(b"mif1");
    let m = measure(&place(&v)).unwrap();
    assert_eq!((m.kind, m.health), (FileKind::Mp4, Health::Intact));
}

#[test]
fn cr3_is_found_intact_with_kind_cr3() {
    let v = testkit::cr3(7, &exif());
    let img = place(&v);
    assert_eq!(
        measure(&img),
        Some(Measurement {
            len: v.len() as u64,
            kind: FileKind::Cr3,
            health: Health::Intact
        })
    );
    // The full-size JPEG in mdat starts at a sector boundary; it is the file's own, and the scan reports one file.
    assert_eq!(
        scan_kinds(&img),
        vec![(AT as u64, v.len() as u64, FileKind::Cr3, Health::Intact)]
    );
}

/// Both the `PRVW` and the full-size JPEG must strict-decode for `Intact` (ruling P18, revised): the walk cannot see file
/// heads inside the `PRVW` box, so a broken `PRVW` is the only sign that something was written over it. A broken `PRVW`
/// next to a clean full-size JPEG is `Partial`, as is a broken full-size JPEG next to a clean `PRVW`; with no full-size
/// JPEG (the first chunk is not a JPEG) the `PRVW` decides alone.
#[test]
fn cr3_needs_both_previews_to_decode_to_be_intact() {
    let mut v = testkit::cr3(7, &exif());
    let prvw = pos(&v, b"PRVW");
    let size = u32::from_be_bytes(v[prvw + 16..prvw + 20].try_into().unwrap()) as usize;
    let mid = prvw + 20 + size / 2;
    v[mid..mid + 64].copy_from_slice(&[0xFF, 0xC4].repeat(32));
    let m = measure(&place(&v)).unwrap();
    assert_eq!(
        (m.kind, m.len, m.health),
        (FileKind::Cr3, v.len() as u64, Health::Partial)
    );

    let mut v = testkit::cr3(7, &exif());
    let full = pos(&v, b"mdat") + 4;
    assert_eq!(&v[full..full + 3], &[0xFF, 0xD8, 0xFF]);
    v[full + 1000..full + 1064].copy_from_slice(&[0xFF, 0xC4].repeat(32));
    let m = measure(&place(&v)).unwrap();
    assert_eq!(
        (m.kind, m.len, m.health),
        (FileKind::Cr3, v.len() as u64, Health::Partial)
    );

    // No full-size JPEG (the first chunk is not a JPEG): the clean `PRVW` decides alone.
    let mut v = testkit::cr3(7, &exif());
    let full = pos(&v, b"mdat") + 4;
    v[full..full + 2].copy_from_slice(&[0, 0]);
    let m = measure(&place(&v)).unwrap();
    assert_eq!(
        (m.kind, m.len, m.health),
        (FileKind::Cr3, v.len() as u64, Health::Intact)
    );
}

/// A small file written inside the `PRVW` JPEG is not seen by the CR3 walk (it only checks heads inside `mdat`), so the
/// broken `PRVW` keeps the CR3 `Partial`, the scan keeps looking inside it, and the file is found.
#[test]
fn file_written_inside_cr3_prvw_is_found() {
    let v = testkit::cr3(7, &exif());
    let foreign = testkit::jpeg(8, 8, 77, 0);
    let at = 512;
    let prvw = pos(&v, b"PRVW");
    let size = u32::from_be_bytes(v[prvw + 16..prvw + 20].try_into().unwrap()) as usize;
    assert!(
        prvw < at && at + foreign.len() < prvw + 20 + size,
        "inside the PRVW JPEG"
    );
    let mut v = v;
    v[at..at + foreign.len()].copy_from_slice(&foreign);
    let img = place(&v);
    let found = scan_kinds(&img);
    assert!(
        found.contains(&(AT as u64, v.len() as u64, FileKind::Cr3, Health::Partial)),
        "{found:?}"
    );
    assert!(
        found.contains(&(
            (AT + at) as u64,
            foreign.len() as u64,
            FileKind::Jpeg,
            Health::Intact
        )),
        "{found:?}"
    );
}

#[test]
fn cr3_without_a_preview_is_partial() {
    let mut v = testkit::cr3(7, &exif());
    let prvw = pos(&v, b"PRVW");
    v[prvw..prvw + 4].copy_from_slice(b"PRVX");
    assert_eq!(measure(&place(&v)).unwrap().health, Health::Partial);
}

#[test]
fn cr3_keeps_the_moov_index_rules() {
    // A chunk table that points past the end of the file: Partial; a track without `stsz`: Damaged.
    let mut v = testkit::cr3(7, &exif());
    let stsz = pos(&v, b"stsz");
    v[stsz + 16..stsz + 20].copy_from_slice(&90_000_000u32.to_be_bytes());
    assert_eq!(measure(&place(&v)).unwrap().health, Health::Partial);

    let mut v = testkit::cr3(7, &exif());
    let stsz = pos(&v, b"stsz");
    v[stsz..stsz + 4].copy_from_slice(b"stsx");
    let m = measure(&place(&v)).unwrap();
    assert_eq!((m.kind, m.health), (FileKind::Cr3, Health::Damaged));
}

#[test]
fn cr3_cut_is_partial() {
    let v = testkit::cr3(7, &exif());
    let keep = v.len() - 10_000;
    let img = DiskImage::zeros(AT + keep).put(AT, &v[..keep]).save();
    let m = measure(&img).unwrap();
    assert_eq!(
        (m.kind, m.len, m.health),
        (FileKind::Cr3, keep as u64, Health::Partial)
    );
}

#[test]
fn heic_is_found_intact_with_kind_heic() {
    let v = testkit::heic(60_000, 5, &exif());
    let img = place(&v);
    assert_eq!(
        measure(&img),
        Some(Measurement {
            len: v.len() as u64,
            kind: FileKind::Heic,
            health: Health::Intact
        })
    );
    assert_eq!(
        scan_kinds(&img),
        vec![(AT as u64, v.len() as u64, FileKind::Heic, Health::Intact)]
    );
}

/// Offset of the first iloc extent's length field (the testkit layout: version 1, 4-byte offsets and lengths).
fn first_extent_len_at(v: &[u8]) -> usize {
    pos(v, b"iloc") + 4 + 20
}

#[test]
fn heic_extent_past_end_is_partial() {
    let mut v = testkit::heic(60_000, 5, &exif());
    let at = first_extent_len_at(&v);
    v[at..at + 4].copy_from_slice(&5_000_000u32.to_be_bytes());
    let m = measure(&place(&v)).unwrap();
    assert_eq!(
        (m.kind, m.len, m.health),
        (FileKind::Heic, v.len() as u64, Health::Partial)
    );
}

/// Offset of the length field of the second iloc extent (the Exif item, which ends at the end of the file).
fn second_extent_len_at(v: &[u8]) -> usize {
    // After the tag: version/flags (4), sizes (2), count (2), the 16-byte first item, then id, method, reference, count
    // (8 bytes), offset (4), length.
    pos(v, b"iloc") + 4 + 4 + 2 + 2 + 16 + 8 + 4
}

#[test]
fn heic_extent_ending_exactly_at_the_end_is_intact() {
    let v = testkit::heic(60_000, 5, &exif());
    let at = second_extent_len_at(&v);
    let off_at = at - 4;
    let end = u32::from_be_bytes(v[off_at..off_at + 4].try_into().unwrap())
        + u32::from_be_bytes(v[at..at + 4].try_into().unwrap());
    assert_eq!(end as usize, v.len(), "the Exif extent ends the file");
    assert_eq!(measure(&place(&v)).unwrap().health, Health::Intact);
}

#[test]
fn heic_extent_one_byte_past_the_end_is_partial() {
    let mut v = testkit::heic(60_000, 5, &exif());
    let at = second_extent_len_at(&v);
    let len = u32::from_be_bytes(v[at..at + 4].try_into().unwrap());
    v[at..at + 4].copy_from_slice(&(len + 1).to_be_bytes());
    assert_eq!(measure(&place(&v)).unwrap().health, Health::Partial);
}

#[test]
fn avif_major_with_mif1_is_not_heic() {
    let mut v = testkit::heic(60_000, 5, &exif());
    v[8..12].copy_from_slice(b"avif");
    v[16..24].copy_from_slice(b"mif1miaf");
    let m = measure(&place(&v)).unwrap();
    assert_eq!(m.kind, FileKind::Mp4);
}

#[test]
fn heic_with_mif1_major_and_heic_compatible_is_heic() {
    let mut v = testkit::heic(60_000, 5, &exif());
    v[8..12].copy_from_slice(b"mif1");
    v[16..24].copy_from_slice(b"heicheic");
    let m = measure(&place(&v)).unwrap();
    assert_eq!((m.kind, m.health), (FileKind::Heic, Health::Intact));
    // A HEIC brand only in the compatible list is enough too.
    v[8..12].copy_from_slice(b"test");
    let m = measure(&place(&v)).unwrap();
    assert_eq!((m.kind, m.health), (FileKind::Heic, Health::Intact));
}

#[test]
fn cr3_preview_size_running_past_its_box_is_partial() {
    let mut v = testkit::cr3(7, &exif());
    let at = pos(&v, b"PRVW") + 16;
    let size = u32::from_be_bytes(v[at..at + 4].try_into().unwrap());
    v[at..at + 4].copy_from_slice(&(size + 1000).to_be_bytes());
    let m = measure(&place(&v)).unwrap();
    assert_eq!(
        (m.kind, m.len, m.health),
        (FileKind::Cr3, v.len() as u64, Health::Partial)
    );
}

#[test]
fn heic_without_iloc_is_damaged() {
    let mut v = testkit::heic(60_000, 5, &exif());
    let at = pos(&v, b"iloc");
    v[at..at + 4].copy_from_slice(b"ilox");
    let m = measure(&place(&v)).unwrap();
    assert_eq!((m.kind, m.health), (FileKind::Heic, Health::Damaged));
}

#[test]
fn heic_with_a_garbled_meta_is_damaged() {
    let mut v = testkit::heic(60_000, 5, &exif());
    // The size of hdlr (the first child of meta) becomes larger than meta.
    let at = pos(&v, b"hdlr") - 4;
    v[at..at + 4].copy_from_slice(&0x0100_0000u32.to_be_bytes());
    let m = measure(&place(&v)).unwrap();
    assert_eq!((m.kind, m.health), (FileKind::Heic, Health::Damaged));
}

#[test]
fn heic_with_data_we_cannot_verify_is_partial() {
    // construction_method 1 (idat) and a data reference outside the file are not checked against the file.
    let base = testkit::heic(60_000, 5, &exif());
    let item = pos(&base, b"iloc") + 4 + 4 + 2 + 2;
    for (name, at, value) in [("idat", item + 2, 1u16), ("external", item + 4, 1u16)] {
        let mut v = base.clone();
        v[at..at + 2].copy_from_slice(&value.to_be_bytes());
        assert_eq!(
            measure(&place(&v)).unwrap().health,
            Health::Partial,
            "{name}"
        );
    }
}

#[test]
fn heic_cut_is_partial() {
    let v = testkit::heic(60_000, 5, &exif());
    let keep = v.len() - 20_000;
    let img = DiskImage::zeros(AT + keep).put(AT, &v[..keep]).save();
    let m = measure(&img).unwrap();
    assert_eq!(
        (m.kind, m.len, m.health),
        (FileKind::Heic, keep as u64, Health::Partial)
    );
}

#[test]
fn heic_cut_by_the_next_file_is_partial() {
    let v = testkit::heic(60_000, 5, &exif());
    let cut = (AT + 30_000).next_multiple_of(512);
    let jpeg = testkit::jpeg(64, 64, 9, 0);
    let img = DiskImage::noise(AT + v.len() + 40_000, 5)
        .put(AT, &v)
        .put(cut, &jpeg)
        .save();
    let found = scan_kinds(&img);
    assert_eq!(
        found[0],
        (
            AT as u64,
            (cut - AT) as u64,
            FileKind::Heic,
            Health::Partial
        )
    );
}

#[test]
fn heic_item_that_is_a_jpeg_at_a_sector_start_does_not_cut() {
    // An item whose data is a JPEG (a thumbnail item) at a sector boundary inside mdat is the file's own.
    let mut v = testkit::heic(60_000, 5, &exif());
    let mdat = pos(&v, b"mdat") + 4;
    let jpeg_at = (mdat + 100).next_multiple_of(512);
    let jpeg = testkit::jpeg(64, 64, 9, 0);
    v[jpeg_at..jpeg_at + jpeg.len()].copy_from_slice(&jpeg);
    // Point the first extent at it.
    let off_at = pos(&v, b"iloc") + 4 + 16;
    v[off_at..off_at + 4].copy_from_slice(&(jpeg_at as u32).to_be_bytes());
    v[off_at + 4..off_at + 8].copy_from_slice(&(jpeg.len() as u32).to_be_bytes());
    let m = measure(&place(&v)).unwrap();
    assert_eq!(
        (m.kind, m.len, m.health),
        (FileKind::Heic, v.len() as u64, Health::Intact)
    );
}

#[test]
fn heic_without_meta_or_mdat_is_not_a_file() {
    let v = testkit::heic(60_000, 5, &exif());
    let ftyp_len = u32::from_be_bytes(v[..4].try_into().unwrap()) as usize;
    let img = DiskImage::zeros(AT + 4096).put(AT, &v[..ftyp_len]).save();
    assert_eq!(measure(&img), None);
}

#[test]
fn heic_iloc_with_hostile_fields_is_damaged() {
    let base = testkit::heic(60_000, 5, &exif());
    let iloc = pos(&base, b"iloc") + 4;
    // Offset size 3 is not a legal size; an item count far beyond the table's data.
    for (name, at, bytes) in [
        ("size", iloc + 4, vec![0x34u8]),
        ("count", iloc + 6, vec![0xFF, 0xFF]),
    ] {
        let mut v = base.clone();
        v[at..at + bytes.len()].copy_from_slice(&bytes);
        let m = measure(&place(&v)).unwrap();
        assert_eq!(
            (m.kind, m.health),
            (FileKind::Heic, Health::Damaged),
            "{name}"
        );
    }
}

#[test]
fn cr3_cut_by_a_foreign_jpeg_inside_mdat_keeps_the_cut() {
    let v = testkit::cr3(7, &exif());
    // The full-size JPEG is the file's own; a different file written over the mdat further in ends the CR3 there.
    let cut = (AT + v.len() - 200_000).next_multiple_of(512);
    let jpeg = testkit::jpeg(64, 64, 9, 0);
    let img = DiskImage::noise(AT + v.len() + 40_000, 11)
        .put(AT, &v)
        .put(cut, &jpeg)
        .save();
    let found = scan_kinds(&img);
    assert_eq!(
        found[0],
        (AT as u64, (cut - AT) as u64, FileKind::Cr3, Health::Partial)
    );
}

#[test]
fn zero_size_iloc_fields_are_bounded() {
    // offset, length and index sizes of 0: every extent takes 0 bytes, so 65,535 items x 65,535 extents would be 4
    // billion iterations out of a 525 KB table.
    let mut iloc = vec![1, 0, 0, 0, 0x00, 0x00];
    iloc.extend_from_slice(&0xFFFFu16.to_be_bytes());
    for id in 0..0xFFFFu16 {
        iloc.extend_from_slice(&id.to_be_bytes());
        iloc.extend_from_slice(&[0, 0, 0, 0]); // construction method, data reference
        iloc.extend_from_slice(&0xFFFFu16.to_be_bytes()); // extent count
    }
    let mut meta = vec![0u8; 4];
    meta.extend(bx(b"iloc", &iloc));
    let mut v = bx(b"ftyp", b"heic\0\0\0\0mif1");
    v.extend(bx(b"meta", &meta));
    v.extend(bx(b"mdat", &[7u8; 1000]));
    let img = place(&v);
    let t = std::time::Instant::now();
    let m = measure(&img).unwrap();
    assert!(t.elapsed().as_secs_f32() < 1.0, "took {:?}", t.elapsed());
    assert_eq!(m.kind, FileKind::Heic);
    assert_ne!(m.health, Health::Intact);
}

/// In-memory source that counts the bytes it hands out.
struct Counting {
    bytes: Vec<u8>,
    read: std::sync::atomic::AtomicU64,
}

impl Source for Counting {
    fn len(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, EngineError> {
        let start = (offset as usize).min(self.bytes.len());
        let n = buf.len().min(self.bytes.len() - start);
        buf[..n].copy_from_slice(&self.bytes[start..start + n]);
        self.read
            .fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
        Ok(n)
    }

    fn volume(&self) -> Option<VolumeId> {
        None
    }
}

/// A moov-first video whose `mdat` has size 0 (it runs to the end of the source), a JPEG on the next sector after it, and
/// `tail` bytes of noise after that. Returns the measurement, the bytes read and where the JPEG is.
fn measure_size_zero_mdat(codec: &[u8; 4], tail: usize) -> (Measurement, u64, usize) {
    let mut v = testkit::mp4_with(
        &Mp4Options {
            moov_at_end: false,
            ..opts()
        },
        codec,
        &[0x33; 3000],
        &[0],
    );
    let mdat = find(&v, b"mdat") - 4;
    v[mdat..mdat + 4].copy_from_slice(&0u32.to_be_bytes());
    let jpeg_at = (AT + v.len()).next_multiple_of(512);
    let jpeg = testkit::jpeg(32, 32, 3, 0);
    let mut img = DiskImage::noise(jpeg_at + jpeg.len() + tail, 31);
    img.put(AT, &v).put(jpeg_at, &jpeg);
    let file = img.save();
    let src = Counting {
        bytes: std::fs::read(file.path()).unwrap(),
        read: Default::default(),
    };
    let m = IsoBmff.measure(&src, AT as u64).unwrap();
    (m, src.read.into_inner(), jpeg_at)
}

/// A size-0 `mdat` cut by the next file is not walked again to the end of the source: the work does not grow with what
/// lies after the cut (a card full of recordings cut this way made the scan quadratic).
#[test]
fn size_zero_mdat_cut_does_not_rewalk_the_rest_of_the_source() {
    for codec in [b"avc1", b"mjpa"] {
        let started = std::time::Instant::now();
        let (small, small_read, jpeg_at) = measure_size_zero_mdat(codec, 8 << 20);
        let (large, large_read, _) = measure_size_zero_mdat(codec, 64 << 20);
        let name = String::from_utf8_lossy(codec);
        assert!(
            started.elapsed().as_secs_f32() < 10.0,
            "{name}: took {:?}",
            started.elapsed()
        );
        for m in [small, large] {
            assert_eq!(
                (m.len, m.health),
                ((jpeg_at - AT) as u64, Health::Intact),
                "{name}"
            );
        }
        assert!(
            large_read < 3 * small_read,
            "{name}: read {small_read} bytes with an 8 MiB tail, {large_read} with 64 MiB"
        );
    }
}

/// The Motion-JPEG exception is for video tracks: an `mjpa` sample entry in a track whose handler is not `vide` does not
/// make its chunks the file's own frames.
#[test]
fn motion_jpeg_exception_needs_a_video_track() {
    let (mut video, _) = mjpeg_layout(b"mjpa", None);
    let vide = find(&video, b"vide");
    video[vide..vide + 4].copy_from_slice(b"soun");
    let img = DiskImage::noise(AT + video.len() + 20_000, 11)
        .put(AT, &video)
        .save();
    let m = measure(&img).unwrap();
    assert_eq!((m.len, m.health), (512, Health::Damaged));
}

/// A CR3 whose full-size JPEG (the first chunk, on a sector start) carries an EXIF thumbnail 512 bytes in, also on a
/// sector start.
fn cr3_with_aligned_thumbnail() -> Vec<u8> {
    let full = testkit::jpeg_with_thumbnail_at(640, 480, 7, 512);
    testkit::cr3_with_full_jpeg(7, &exif(), &full)
}

/// The thumbnail inside the CR3's own full-size JPEG is its own data while that JPEG still decodes (ruling P13).
#[test]
fn thumbnail_inside_the_cr3_full_size_jpeg_does_not_cut_it() {
    let v = cr3_with_aligned_thumbnail();
    let img = place(&v);
    assert_eq!(
        scan_kinds(&img),
        vec![(AT as u64, v.len() as u64, FileKind::Cr3, Health::Intact)]
    );
}

/// A file written over part of the full-size JPEG breaks its decode, so no head inside that JPEG is exempt any more: the
/// CR3 is cut at the first one (the thumbnail) and the foreign file is found on its own.
#[test]
fn foreign_jpeg_inside_the_cr3_full_size_jpeg_cuts_it() {
    let mut v = cr3_with_aligned_thumbnail();
    let mdat = pos(&v, b"mdat");
    let foreign = testkit::jpeg(8, 8, 77, 0);
    let at = mdat + 4 + 4096;
    v[at..at + foreign.len()].copy_from_slice(&foreign);
    let found = scan_kinds(&place(&v));
    assert_eq!(
        found[0],
        (
            AT as u64,
            (mdat + 4 + 512) as u64,
            FileKind::Cr3,
            Health::Partial
        ),
        "{found:?}"
    );
    assert!(
        found.contains(&(
            (AT + at) as u64,
            foreign.len() as u64,
            FileKind::Jpeg,
            Health::Intact
        )),
        "{found:?}"
    );
}

fn ftyp(brand: &[u8; 4]) -> Vec<u8> {
    let mut p = brand.to_vec();
    p.extend_from_slice(&[0; 4]);
    p.extend_from_slice(brand);
    bx(b"ftyp", &p)
}

#[test]
fn confirms_agrees_with_measure() {
    let cat = |parts: &[Vec<u8>]| parts.concat();
    let layouts: Vec<(&str, Vec<u8>, bool)> = vec![
        (
            "ftyp+free",
            cat(&[ftyp(b"isom"), bx(b"free", &[0; 8])]),
            false,
        ),
        (
            "ftyp+free+mdat",
            cat(&[ftyp(b"isom"), bx(b"free", &[0; 8]), bx(b"mdat", &[7; 64])]),
            true,
        ),
        (
            "ftyp+moov",
            cat(&[ftyp(b"isom"), bx(b"moov", &[0; 32])]),
            false,
        ),
        ("mp4", testkit::mp4(&opts()), true),
        (
            "heic",
            testkit::heic(20_000, 5, &testkit::TestExif::default()),
            true,
        ),
        (
            "heic brand + free",
            cat(&[ftyp(b"heic"), bx(b"free", &[0; 8])]),
            false,
        ),
    ];
    for (name, bytes, expected) in layouts {
        let img = DiskImage::noise(AT + bytes.len() + 20_000, 9)
            .put(AT, &bytes)
            .save();
        let src = FileSource::open(img.path()).unwrap();
        let confirms = IsoBmff.confirms(&src, AT as u64);
        let measured = IsoBmff.measure(&src, AT as u64);
        assert_eq!(confirms, measured.is_some(), "{name}: confirms vs measure");
        assert_eq!(confirms, expected, "{name}");
    }
}
