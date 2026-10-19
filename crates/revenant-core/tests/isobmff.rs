use revenant_core::formats::{FileKind, Format, Health, IsoBmff, Measurement};
use revenant_core::source::FileSource;
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
fn child_larger_than_parent_is_ignored_without_panic() {
    let mut v = testkit::mp4(&opts());
    let trak = find(&v, b"trak");
    v[trak - 4..trak].copy_from_slice(&0x00FF_FFFFu32.to_be_bytes());
    let img = DiskImage::noise(AT + v.len() + 4096, 12).put(AT, &v).save();
    // The unreadable trak is skipped: no tables to contradict the file, so the structure alone decides.
    let m = measure(&img).unwrap();
    assert_eq!((m.len, m.health), (v.len() as u64, Health::Intact));
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
