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
