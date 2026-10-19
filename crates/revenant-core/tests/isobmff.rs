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
    // The same bytes, readable and valid, confirm too; readable zeros do not.
    assert!(IsoBmff.confirms(&FileSource::open(img.path()).unwrap(), AT as u64));
    let img = DiskImage::zeros(AT + 4096).put(AT, &v[..512]).save();
    assert!(!IsoBmff.confirms(&FileSource::open(img.path()).unwrap(), AT as u64));
}
