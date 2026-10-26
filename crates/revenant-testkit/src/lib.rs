//! Synthetic test data for Revenant: deterministic JPEGs and disk images. Nothing here touches real data.

pub mod fs;
pub mod os_fs;

use std::io::Write;

use image::ExtendedColorType;
use image::codecs::jpeg::JpegEncoder;

/// Largest payload of one JPEG segment (the 16-bit length counts its own two bytes).
const MAX_SEGMENT_PAYLOAD: usize = 65_533;

/// Small deterministic PRNG (SplitMix64); enough for reproducible noise.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn fill(&mut self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            let bytes = self.next().to_le_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
        }
    }
}

fn encode_noise(width: u32, height: u32, seed: u64) -> Vec<u8> {
    let mut pixels = vec![0u8; width as usize * height as usize * 3];
    SplitMix64(seed).fill(&mut pixels);
    encode_rgb(&pixels, width, height)
}

/// Baseline JPEG (quality 90) of `pixels` (RGB, row by row), without the JFIF APP0.
fn encode_rgb(pixels: &[u8], width: u32, height: u32) -> Vec<u8> {
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, 90)
        .encode(pixels, width, height, ExtendedColorType::Rgb8)
        .expect("encoding a JPEG in memory cannot fail");
    // Drop the JFIF APP0 (18 bytes) so tiny images stay as small as a baseline JPEG can be.
    if out[2..4] == [0xFF, 0xE0] {
        let len = usize::from(u16::from_be_bytes([out[4], out[5]]));
        out.drain(2..4 + len);
    }
    out
}

/// Valid baseline JPEG (quality 90, no APP0/APP1) of smooth content, like a photo's sky or skin: each channel is the sum
/// of two low-frequency sinusoids whose orientation and phase come from `seed`. Neighbouring 8×8 blocks have close DC
/// values, so a seam between two different images shows in the DC grid.
pub fn jpeg_smooth(width: u32, height: u32, seed: u64) -> Vec<u8> {
    encode_rgb(&smooth_pixels(width, height, seed, 0), width, height)
}

/// `n` photos of one scene and one frame (same size, tables and sampling): photo k is [`jpeg_smooth`]'s scene shifted by
/// `k` pixels right, so neighbouring photos have near-equal DC grids (the "burst" a seam test must still tell apart).
/// Photo 0 is `jpeg_smooth(width, height, seed)`.
pub fn jpeg_burst(width: u32, height: u32, seed: u64, n: usize) -> Vec<Vec<u8>> {
    (0..n)
        .map(|k| encode_rgb(&smooth_pixels(width, height, seed, k as u32), width, height))
        .collect()
}

/// The RGB pixels of [`jpeg_smooth`]'s scene, shifted `shift` pixels right (pixel `x` shows the scene at `x - shift`).
fn smooth_pixels(width: u32, height: u32, seed: u64, shift: u32) -> Vec<u8> {
    let mut rng = SplitMix64(seed);
    let mut unit = || (rng.next() >> 11) as f64 / (1u64 << 53) as f64;
    let tau = std::f64::consts::TAU;
    // (cos θ, sin θ, cycles across the image, phase) of each wave.
    let waves: [(f64, f64, f64, f64); 2] = std::array::from_fn(|_| {
        let angle = unit() * tau;
        (angle.cos(), angle.sin(), 1.0 + 2.0 * unit(), unit() * tau)
    });
    let size = f64::from(width.max(height));
    let mut pixels = Vec::with_capacity(width as usize * height as usize * 3);
    for y in 0..height {
        for x in 0..width {
            let (fx, fy) = (
                (f64::from(x) - f64::from(shift)) / size,
                f64::from(y) / size,
            );
            for channel in 0..3 {
                let shift = f64::from(channel) * 0.7;
                let v: f64 = waves
                    .iter()
                    .zip([60.0, 40.0])
                    .map(|(&(c, s, cycles, phase), amp)| {
                        amp * (tau * cycles * (c * fx + s * fy) + phase + shift).sin()
                    })
                    .sum();
                pixels.push((128.0 + v).round().clamp(0.0, 255.0) as u8);
            }
        }
    }
    pixels
}

fn app1_segment(payload: &[u8]) -> Vec<u8> {
    let mut seg = vec![0xFF, 0xE1];
    seg.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
    seg.extend_from_slice(payload);
    seg
}

/// Valid baseline JPEG, deterministic pseudo-random content per `seed`, with APP1 data of about `app1_extra` bytes
/// (simulates camera EXIF with an embedded thumbnail; the first APP1 payload contains a complete 16×16 JPEG).
///
/// `app1_extra == 0` emits no APP1 segment. Larger values are padded to exactly `app1_extra` payload bytes in total,
/// split over several APP1 segments when they exceed the 65533-byte segment limit.
pub fn jpeg(width: u32, height: u32, seed: u64, app1_extra: usize) -> Vec<u8> {
    let base = encode_noise(width, height, seed);
    if app1_extra == 0 {
        return base;
    }
    // First payload: Exif identifier (as in camera files), then the thumbnail, then padding.
    let mut first = b"Exif\0\0".to_vec();
    first.extend(encode_noise(16, 16, seed ^ 0x5DEE_CE66));
    let mut remaining = app1_extra.max(first.len());
    let mut rng = SplitMix64(seed.wrapping_add(1));
    let first_len = remaining.min(MAX_SEGMENT_PAYLOAD);
    let thumb_len = first.len();
    first.resize(first_len, 0);
    rng.fill(&mut first[thumb_len..]);
    remaining -= first_len;

    let mut segments = app1_segment(&first);
    while remaining > 0 {
        let n = remaining.min(MAX_SEGMENT_PAYLOAD);
        let mut pad = vec![0u8; n];
        rng.fill(&mut pad);
        segments.extend(app1_segment(&pad));
        remaining -= n;
    }

    let mut out = Vec::with_capacity(base.len() + segments.len());
    out.extend_from_slice(&base[..2]); // SOI
    out.extend(segments);
    out.extend_from_slice(&base[2..]);
    out
}

/// In-memory disk image under construction; [`DiskImage::save`] writes it to a temporary file.
pub struct DiskImage {
    bytes: Vec<u8>,
}

impl DiskImage {
    /// Deterministic random fill.
    pub fn noise(len: usize, seed: u64) -> Self {
        let mut bytes = vec![0u8; len];
        SplitMix64(seed).fill(&mut bytes);
        Self { bytes }
    }

    pub fn zeros(len: usize) -> Self {
        Self {
            bytes: vec![0; len],
        }
    }

    /// Copies `data` over the image at `offset`. Panics if it does not fit.
    pub fn put(&mut self, offset: usize, data: &[u8]) -> &mut Self {
        self.bytes[offset..offset + data.len()].copy_from_slice(data);
        self
    }

    pub fn save(&self) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().expect("create temp file");
        file.write_all(&self.bytes).expect("write temp file");
        file.flush().expect("flush temp file");
        file
    }
}

/// Shape of a synthetic MP4/MOV built by [`mp4`].
pub struct Mp4Options {
    /// Payload bytes inside `mdat`.
    pub mdat_len: usize,
    /// `ftyp, mdat, moov` (the camera case) instead of `ftyp, moov, mdat`.
    pub moov_at_end: bool,
    /// Major brand `qt  ` instead of `isom`.
    pub mov: bool,
    /// `mdat` with a 64-bit `largesize` header, and `co64` instead of `stco`.
    pub mdat_64bit: bool,
}

fn bmff_box(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
    out.extend_from_slice(kind);
    out.extend_from_slice(payload);
    out
}

/// `stsd` with one video sample entry of the given codec type (`avc1`, `mjpa`, ...).
fn stsd_box(codec: &[u8; 4]) -> Vec<u8> {
    let mut payload = vec![0; 4]; // version and flags
    payload.extend_from_slice(&1u32.to_be_bytes()); // entry count
    let mut entry = vec![0; 6]; // reserved
    entry.extend_from_slice(&1u16.to_be_bytes()); // data reference index
    payload.extend(bmff_box(codec, &entry));
    bmff_box(b"stsd", &payload)
}

/// Like [`moov_box`], with `extra` (already-built boxes) placed after `mvhd` and before the `trak`.
fn moov_box_with(
    chunk_offsets: &[u64],
    sample_len: u32,
    wide: bool,
    codec: &[u8; 4],
    extra: &[u8],
) -> Vec<u8> {
    moov_box_full(chunk_offsets, sample_len, wide, codec, extra, None)
}

/// Creation time (seconds since 1904-01-01 UTC) and `tkhd` size of a video built by [`mp4_dated`].
#[derive(Clone, Copy)]
struct VideoMeta {
    /// `mvhd` and `tkhd` version 1 (64-bit times) instead of version 0.
    v1: bool,
    creation: u64,
    width: u32,
    height: u32,
}

fn moov_box_full(
    chunk_offsets: &[u64],
    sample_len: u32,
    wide: bool,
    codec: &[u8; 4],
    extra: &[u8],
    meta: Option<VideoMeta>,
) -> Vec<u8> {
    let mut mvhd = vec![0u8; 100];
    match meta {
        Some(m) if m.v1 => {
            // Version 1: 64-bit creation and modification times, 32-bit timescale, 64-bit duration.
            mvhd = vec![0u8; 112];
            mvhd[0] = 1;
            mvhd[4..12].copy_from_slice(&m.creation.to_be_bytes());
        }
        Some(m) => mvhd[4..8].copy_from_slice(&(m.creation as u32).to_be_bytes()),
        None => {}
    }
    let mvhd = bmff_box(b"mvhd", &mvhd);
    let mut stsz = vec![0; 4];
    stsz.extend_from_slice(&0u32.to_be_bytes()); // sample_size = 0: per-sample table
    stsz.extend_from_slice(&1u32.to_be_bytes());
    stsz.extend_from_slice(&sample_len.to_be_bytes());
    let mut chunks = vec![0; 4];
    chunks.extend_from_slice(&(chunk_offsets.len() as u32).to_be_bytes());
    for &offset in chunk_offsets {
        if wide {
            chunks.extend_from_slice(&offset.to_be_bytes());
        } else {
            chunks.extend_from_slice(&(offset as u32).to_be_bytes());
        }
    }
    let chunk_table = bmff_box(if wide { b"co64" } else { b"stco" }, &chunks);
    // hdlr: version/flags, pre_defined, handler type, reserved, empty name.
    let mut hdlr = vec![0; 8];
    hdlr.extend_from_slice(b"vide");
    hdlr.extend_from_slice(&[0; 13]);
    let hdlr = bmff_box(b"hdlr", &hdlr);
    let stbl = bmff_box(
        b"stbl",
        &[stsd_box(codec), bmff_box(b"stsz", &stsz), chunk_table].concat(),
    );
    let mdia = bmff_box(b"mdia", &[hdlr, bmff_box(b"minf", &stbl)].concat());
    let trak = match meta {
        Some(m) => {
            // Width and height are 16.16 fixed point after the matrix: at 76 in version 0 (84 bytes of payload), at 88 in
            // version 1 (96 bytes; the times and the duration are 64-bit).
            let at = if m.v1 { 88 } else { 76 };
            let mut tkhd = vec![0u8; at + 8];
            tkhd[0] = u8::from(m.v1);
            tkhd[at..at + 4].copy_from_slice(&(m.width << 16).to_be_bytes());
            tkhd[at + 4..at + 8].copy_from_slice(&(m.height << 16).to_be_bytes());
            bmff_box(b"trak", &[bmff_box(b"tkhd", &tkhd), mdia].concat())
        }
        None => bmff_box(b"trak", &mdia),
    };
    bmff_box(b"moov", &[mvhd, extra.to_vec(), trak].concat())
}

/// MP4/MOV with `ftyp`, an `mdat` of noise, and a `moov` holding one `trak` whose `stsz`/`stco` (or `co64`) describe the
/// whole `mdat` payload as a single sample in a single chunk. The file starts at offset 0 of the returned bytes.
pub fn mp4(opts: &Mp4Options) -> Vec<u8> {
    let mut payload = vec![0u8; opts.mdat_len];
    SplitMix64(opts.mdat_len as u64).fill(&mut payload);
    mp4_with(opts, b"avc1", &payload, &[0])
}

/// Like [`mp4`], but the video track's sample entry type is `codec` (`avc1`, `mjpa`, ...) and the `mdat` payload is
/// `payload` (`opts.mdat_len` is ignored). The payload sits at offset 28 of the file (20-byte `ftyp`, 8-byte `mdat`
/// header; 36 with `mdat_64bit`) when `moov` is at the end. `chunks` are the chunk offsets listed in the index, relative to
/// the start of the payload.
pub fn mp4_with(opts: &Mp4Options, codec: &[u8; 4], payload: &[u8], chunks: &[u64]) -> Vec<u8> {
    mp4_build(opts, codec, payload, chunks, None)
}

/// Like [`mp4`], with the `mvhd` creation time set to `creation` (seconds since 1904-01-01 UTC) and a `tkhd` of
/// `width` x `height` pixels in the video track.
pub fn mp4_dated(opts: &Mp4Options, creation: u32, width: u32, height: u32) -> Vec<u8> {
    dated(opts, false, u64::from(creation), width, height)
}

/// Like [`mp4_dated`] with version 1 `mvhd` and `tkhd` boxes (64-bit times).
pub fn mp4_dated_v1(opts: &Mp4Options, creation: u64, width: u32, height: u32) -> Vec<u8> {
    dated(opts, true, creation, width, height)
}

fn dated(opts: &Mp4Options, v1: bool, creation: u64, width: u32, height: u32) -> Vec<u8> {
    let mut payload = vec![0u8; opts.mdat_len];
    SplitMix64(opts.mdat_len as u64).fill(&mut payload);
    let meta = VideoMeta {
        v1,
        creation,
        width,
        height,
    };
    mp4_build(opts, b"avc1", &payload, &[0], Some(meta))
}

fn mp4_build(
    opts: &Mp4Options,
    codec: &[u8; 4],
    payload: &[u8],
    chunks: &[u64],
    meta: Option<VideoMeta>,
) -> Vec<u8> {
    let brand: &[u8; 4] = if opts.mov { b"qt  " } else { b"isom" };
    let mut ftyp = brand.to_vec();
    ftyp.extend_from_slice(&0u32.to_be_bytes());
    ftyp.extend_from_slice(brand);
    let ftyp = bmff_box(b"ftyp", &ftyp);

    let mdat = if opts.mdat_64bit {
        let mut b = 1u32.to_be_bytes().to_vec();
        b.extend_from_slice(b"mdat");
        b.extend_from_slice(&((payload.len() + 16) as u64).to_be_bytes());
        b.extend_from_slice(payload);
        b
    } else {
        bmff_box(b"mdat", payload)
    };
    let header = if opts.mdat_64bit { 16 } else { 8 };
    let moov_len =
        moov_box_full(&vec![0; chunks.len()], 0, opts.mdat_64bit, codec, &[], meta).len();
    let mdat_at = if opts.moov_at_end {
        ftyp.len()
    } else {
        ftyp.len() + moov_len
    };
    let absolute: Vec<u64> = chunks
        .iter()
        .map(|c| (mdat_at + header) as u64 + c)
        .collect();
    let moov = moov_box_full(
        &absolute,
        payload.len() as u32,
        opts.mdat_64bit,
        codec,
        &[],
        meta,
    );

    if opts.moov_at_end {
        [ftyp, mdat, moov].concat()
    } else {
        [ftyp, moov, mdat].concat()
    }
}

/// Shape of a synthetic camera video built by [`avc_video`] or [`camera_video`].
pub struct AvcOptions {
    /// Video samples (frames).
    pub samples: usize,
    pub seed: u64,
    /// Major brand `qt  ` instead of `isom`.
    pub mov: bool,
    /// Bytes of padding at the end of `mdat`, after the last sample.
    pub mdat_pad: usize,
    /// `true`: NAL payloads of 64–448 bytes (1–3 per sample, sample sizes follow), so every 512-byte step of a wrong
    /// break point crosses a NAL header and the correct one is unique; `false`: samples of 2–24 KiB (large NAL units, so a
    /// break point a sector off may land inside one NAL unit on both sides and still pass a NAL walk).
    pub small_nals: bool,
}

/// A synthetic camera video and where its parts are.
pub struct AvcVideo {
    pub bytes: Vec<u8>,
    /// The `mdat` payload (samples, then the padding).
    pub mdat_payload: std::ops::Range<usize>,
    /// The whole `moov` box.
    pub moov: std::ops::Range<usize>,
}

/// One NAL unit with a 4-byte big-endian length: `header` (1 byte for H.264, 2 for HEVC), then `payload` seeded random
/// bytes.
fn push_nal(out: &mut Vec<u8>, header: &[u8], payload: usize, rng: &mut SplitMix64) {
    out.extend_from_slice(&((payload + header.len()) as u32).to_be_bytes());
    out.extend_from_slice(header);
    let at = out.len();
    out.resize(at + payload, 0);
    rng.fill(&mut out[at..]);
}

/// A camera-style H.264 video: `ftyp`, `mdat` (32-bit size), `moov` at the end. One video `trak`: an `avc1` sample entry
/// with an `avcC` (lengthSizeMinusOne = 3), `stsz` per sample, `stsc` 4 samples per chunk, `stco`. Each sample holds 1–3
/// NAL units (4-byte big-endian length, header byte `0x65` for the first sample of a chunk, else `0x41`, seeded random
/// payload); sample sizes are in [2 KiB, 24 KiB] from `seed` (with `small_nals`, NAL payloads in [64, 448] bytes instead).
/// Chunks follow each other in `mdat`, then `mdat_pad` random bytes. Byte for byte
/// `camera_video(opts, VideoCodec::Avc, VideoLayout::MoovAtEnd)`.
pub fn avc_video(opts: &AvcOptions) -> AvcVideo {
    camera_video(opts, VideoCodec::Avc, VideoLayout::MoovAtEnd)
}

/// The video codec of a [`camera_video`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoCodec {
    /// H.264: `avc1` with an `avcC`, 1-byte NAL headers.
    Avc,
    /// H.265: `hvc1` with an `hvcC`, 2-byte NAL headers.
    Hevc,
}

/// Where a [`camera_video`] keeps its index and its media data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoLayout {
    /// `ftyp, mdat, moov` (the camera case).
    MoovAtEnd,
    /// `ftyp, moov, mdat`.
    MoovFirst,
    /// `ftyp, moov, mdat` with the mdat size field 0 (to the end of the file).
    MoovFirstMdatSizeZero,
    /// `ftyp, moov (with mvex/trex, empty sample tables), then (moof, mdat)` per fragment of `samples_per_fragment`
    /// samples (the last one may be shorter).
    Fragmented { samples_per_fragment: usize },
}

/// A full box (version 0, `flags`) holding `body`.
fn full_box(kind: &[u8; 4], flags: u32, body: &[u8]) -> Vec<u8> {
    let mut p = flags.to_be_bytes().to_vec();
    p.extend_from_slice(body);
    bmff_box(kind, &p)
}

/// The `stsd` payload (after version and flags) of a 1920×1080 video with lengthSizeMinusOne = 3.
fn video_sample_entry(codec: VideoCodec) -> Vec<u8> {
    // Visual sample entry: reserved (6), data reference index (2), then 70 bytes of visual fields, then the config box.
    let mut entry = vec![0u8; 78];
    entry[7] = 1;
    entry[24..28].copy_from_slice(&(1920u32 << 16 | 1080).to_be_bytes());
    entry[74..76].copy_from_slice(&0x18u16.to_be_bytes());
    entry[76..78].copy_from_slice(&0xFFFFu16.to_be_bytes());
    let kind = match codec {
        VideoCodec::Avc => {
            // avcC: version 1, profile, compatibility, level, lengthSizeMinusOne = 3, no SPS, no PPS.
            entry.extend(bmff_box(b"avcC", &[1, 0x64, 0, 0x28, 0xFF, 0xE0, 0]));
            b"avc1"
        }
        VideoCodec::Hevc => {
            // hvcC (ISO/IEC 14496-15 §8.3.3.1), the 23-byte fixed part and no arrays: version 1, Main profile (general
            // profile space 0, tier 0, idc 1), compatibility flags, progressive/frame-only constraint flags, level 4.0,
            // min_spatial_segmentation 0, parallelism 0, 4:2:0, 8-bit luma and chroma, average frame rate 0, then
            // constantFrameRate 0, numTemporalLayers 1, temporalIdNested 1, lengthSizeMinusOne 3; numOfArrays 0.
            let hvcc = [
                1, 0x01, 0x60, 0, 0, 0, 0x90, 0, 0, 0, 0, 0, 120, 0xF0, 0, 0xFC, 0xFD, 0xF8, 0xF8,
                0, 0, 0x0F, 0,
            ];
            entry.extend(bmff_box(b"hvcC", &hvcc));
            b"hvc1"
        }
    };
    let mut stsd = 1u32.to_be_bytes().to_vec();
    stsd.extend(bmff_box(kind, &entry));
    stsd
}

/// The `moov` of a [`camera_video`]: `mvhd`, one video `trak` (`tkhd` with `track_id`, `hdlr`, `minf/stbl` with `stsd`,
/// `stsz`, `stsc`, `stco`), then `mvex` when given.
fn camera_moov(
    codec: VideoCodec,
    sizes: &[u32],
    chunks: &[u32],
    track_id: u32,
    mvex: Option<Vec<u8>>,
) -> Vec<u8> {
    let mut stsz = 0u32.to_be_bytes().to_vec(); // sample_size 0: a table follows
    stsz.extend_from_slice(&(sizes.len() as u32).to_be_bytes());
    sizes
        .iter()
        .for_each(|s| stsz.extend_from_slice(&s.to_be_bytes()));
    // 4 samples per chunk; a shorter last chunk gets its own entry. No samples: no entry.
    let mut runs = Vec::new();
    if !sizes.is_empty() {
        runs.push([1u32, 4, 1]);
    }
    if !sizes.len().is_multiple_of(4) {
        runs.push([chunks.len() as u32, (sizes.len() % 4) as u32, 1]);
    }
    let mut stsc = (runs.len() as u32).to_be_bytes().to_vec();
    runs.iter()
        .flatten()
        .for_each(|v| stsc.extend_from_slice(&v.to_be_bytes()));
    let mut stco = (chunks.len() as u32).to_be_bytes().to_vec();
    chunks
        .iter()
        .for_each(|c| stco.extend_from_slice(&c.to_be_bytes()));
    let stbl = bmff_box(
        b"stbl",
        &[
            full_box(b"stsd", 0, &video_sample_entry(codec)),
            full_box(b"stsz", 0, &stsz),
            full_box(b"stsc", 0, &stsc),
            full_box(b"stco", 0, &stco),
        ]
        .concat(),
    );
    let mut hdlr = vec![0; 8];
    hdlr.extend_from_slice(b"vide");
    hdlr.extend_from_slice(&[0; 13]);
    let mdia = bmff_box(
        b"mdia",
        &[bmff_box(b"hdlr", &hdlr), bmff_box(b"minf", &stbl)].concat(),
    );
    // tkhd version 0: version/flags (4), creation (4), modification (4), track_ID (4), ...
    let mut tkhd = [0u8; 84];
    tkhd[12..16].copy_from_slice(&track_id.to_be_bytes());
    let trak = bmff_box(b"trak", &[bmff_box(b"tkhd", &tkhd), mdia].concat());
    let mut body = [bmff_box(b"mvhd", &[0; 100]), trak].concat();
    body.extend(mvex.unwrap_or_default());
    bmff_box(b"moov", &body)
}

/// One movie fragment: `moof(mfhd(seq), traf(tfhd(track 1, default-base-is-moof), trun(data offset, sample sizes)))`
/// for samples whose data follows in the next `mdat`'s payload.
fn movie_fragment(seq: u32, sizes: &[u32]) -> Vec<u8> {
    let build = |data_offset: u32| {
        let mfhd = full_box(b"mfhd", 0, &seq.to_be_bytes());
        let tfhd = full_box(b"tfhd", 0x02_0000, &1u32.to_be_bytes());
        let mut trun = (sizes.len() as u32).to_be_bytes().to_vec();
        trun.extend_from_slice(&data_offset.to_be_bytes());
        sizes
            .iter()
            .for_each(|s| trun.extend_from_slice(&s.to_be_bytes()));
        // Flags: data-offset-present (0x1), sample-size-present (0x200).
        let trun = full_box(b"trun", 0x201, &trun);
        let traf = bmff_box(b"traf", &[tfhd, trun].concat());
        bmff_box(b"moof", &[mfhd, traf].concat())
    };
    // The data offset is from the start of the moof to the first sample, the payload of the mdat right after it.
    let len = build(0).len() as u32;
    build(len + 8)
}

/// A camera video. Each sample holds 1–3 length-prefixed NAL units (4-byte big-endian length, seeded random payload);
/// sample sizes are in [2 KiB, 24 KiB] from `seed` (with `small_nals`, NAL payloads in [64, 448] bytes instead), then
/// `mdat_pad` random bytes end the last `mdat`. Avc: `avc1` sample entry with an `avcC` (lengthSizeMinusOne = 3), NAL
/// header `0x65` for the first sample of a chunk, else `0x41`. Hevc: `hvc1` sample entry with an `hvcC`
/// (lengthSizeMinusOne = 3), NAL headers of 2 bytes (type 19 for the first sample of a chunk, else 1;
/// `nuh_layer_id = 0`, `nuh_temporal_id_plus1 = 1`). A chunk is 4 samples (`stsc`), or a fragment's samples when
/// fragmented. The NAL units are structurally valid and never decode as real video.
///
/// Non-fragmented layouts list the samples in `stsz`/`stsc`/`stco` (chunks back to back in one `mdat`) with a `tkhd` of
/// track ID 0; the fragmented one has empty sample tables, a `tkhd` of track ID 1, an `mvex/trex` for track 1 and one
/// `moof` (sequence numbers from 1) and `mdat` per fragment. `AvcVideo::moov` is the `moov` box wherever it is;
/// `mdat_payload` the first `mdat`'s payload.
pub fn camera_video(opts: &AvcOptions, codec: VideoCodec, layout: VideoLayout) -> AvcVideo {
    let key_every = match layout {
        VideoLayout::Fragmented {
            samples_per_fragment,
        } => {
            assert!(samples_per_fragment > 0, "fragments hold samples");
            samples_per_fragment
        }
        _ => 4,
    };
    let (key, other, header_len): (&[u8], &[u8], usize) = match codec {
        VideoCodec::Avc => (&[0x65], &[0x41], 1),
        // forbidden_zero_bit, nal_unit_type (6), nuh_layer_id (6), nuh_temporal_id_plus1 (3).
        VideoCodec::Hevc => (&[19 << 1, 1], &[1 << 1, 1], 2),
    };
    let mut rng = SplitMix64(opts.seed);
    let mut samples = Vec::new();
    let mut sizes = Vec::with_capacity(opts.samples);
    for i in 0..opts.samples {
        let header = if i % key_every == 0 { key } else { other };
        let nals = 1 + (rng.next() % 3) as usize;
        let start = samples.len();
        if opts.small_nals {
            for _ in 0..nals {
                let len = 64 + (rng.next() % 385) as usize;
                push_nal(&mut samples, header, len, &mut rng);
            }
        } else {
            let size = 2048 + (rng.next() % (22 * 1024 + 1)) as usize;
            // Split the sample into `nals` units of about equal size (each unit: 4-byte length, header, payload).
            let body = size - (4 + header_len) * nals;
            for j in 0..nals {
                let len = if j + 1 == nals {
                    body - (nals - 1) * (body / nals)
                } else {
                    body / nals
                };
                push_nal(&mut samples, header, len, &mut rng);
            }
        }
        sizes.push((samples.len() - start) as u32);
    }
    let mut pad = vec![0u8; opts.mdat_pad];
    rng.fill(&mut pad);

    let brand: &[u8; 4] = if opts.mov { b"qt  " } else { b"isom" };
    let mut ftyp = brand.to_vec();
    ftyp.extend_from_slice(&0u32.to_be_bytes());
    ftyp.extend_from_slice(brand);
    let ftyp = bmff_box(b"ftyp", &ftyp);

    // Chunk offsets (absolute in the file) of samples back to back from `data_at`: every 4 samples.
    let chunks_at = |data_at: usize| {
        let mut chunks = Vec::new();
        let mut off = data_at as u32;
        for (i, &s) in sizes.iter().enumerate() {
            if i % 4 == 0 {
                chunks.push(off);
            }
            off += s;
        }
        chunks
    };
    let payload = [samples, pad.clone()].concat();
    match layout {
        VideoLayout::MoovAtEnd => {
            let data_at = ftyp.len() + 8;
            let moov = camera_moov(codec, &sizes, &chunks_at(data_at), 0, None);
            let moov_at = data_at + payload.len();
            let bytes = [ftyp, bmff_box(b"mdat", &payload), moov].concat();
            AvcVideo {
                mdat_payload: data_at..moov_at,
                moov: moov_at..bytes.len(),
                bytes,
            }
        }
        VideoLayout::MoovFirst | VideoLayout::MoovFirstMdatSizeZero => {
            // The chunk offsets are 32-bit whatever their value: the moov's length does not depend on them.
            let moov_len = camera_moov(codec, &sizes, &chunks_at(0), 0, None).len();
            let data_at = ftyp.len() + moov_len + 8;
            let moov = camera_moov(codec, &sizes, &chunks_at(data_at), 0, None);
            assert_eq!(moov.len(), moov_len);
            let mut mdat = bmff_box(b"mdat", &payload);
            if layout == VideoLayout::MoovFirstMdatSizeZero {
                mdat[..4].copy_from_slice(&0u32.to_be_bytes());
            }
            let moov_at = ftyp.len();
            let bytes = [ftyp, moov, mdat].concat();
            AvcVideo {
                mdat_payload: data_at..bytes.len(),
                moov: moov_at..data_at - 8,
                bytes,
            }
        }
        VideoLayout::Fragmented {
            samples_per_fragment,
        } => {
            // trex: track 1, sample description 1, default duration, size and flags 0.
            let mut trex = 1u32.to_be_bytes().to_vec();
            trex.extend_from_slice(&1u32.to_be_bytes());
            trex.extend_from_slice(&[0; 12]);
            let mvex = bmff_box(b"mvex", &full_box(b"trex", 0, &trex));
            let moov = camera_moov(codec, &[], &[], 1, Some(mvex));
            let moov_at = ftyp.len();
            let mut bytes = [ftyp, moov].concat();
            let moov_range = moov_at..bytes.len();
            let mut first_payload = None;
            let mut at = 0;
            let fragments = sizes.chunks(samples_per_fragment).count();
            for (f, frag) in sizes.chunks(samples_per_fragment).enumerate() {
                let len = frag.iter().map(|&s| s as usize).sum::<usize>();
                let mut data = payload[at..at + len].to_vec();
                at += len;
                if f + 1 == fragments {
                    data.extend_from_slice(&pad);
                }
                bytes.extend(movie_fragment(f as u32 + 1, frag));
                let data_at = bytes.len() + 8;
                first_payload.get_or_insert(data_at..data_at + data.len());
                bytes.extend(bmff_box(b"mdat", &data));
            }
            AvcVideo {
                mdat_payload: first_payload.expect("a fragmented video holds samples"),
                moov: moov_range,
                bytes,
            }
        }
    }
}

/// A camera-style Motion-JPEG video (`mjpa`, `isom` brand): `ftyp`, `mdat` (32-bit size), `moov` at the end. `frames`
/// samples, each a seeded noise JPEG padded (APP1) to a whole number of 512-byte sectors and starting on a sector
/// boundary of the file (the first at 512), one sample per chunk: every frame start is a file head a scan finds.
pub fn mjpeg_video(frames: usize, seed: u64) -> AvcVideo {
    let mut payload = vec![0u8; 512 - 28];
    SplitMix64(seed).fill(&mut payload);
    let mut sizes = Vec::with_capacity(frames);
    let mut chunks = Vec::with_capacity(frames);
    for i in 0..frames {
        let s = seed.wrapping_add(i as u64);
        // Past the embedded thumbnail, each APP1 byte adds one byte to the file.
        let base = jpeg(32, 32, s, 4000).len();
        let frame = jpeg(32, 32, s, 4000 + (512 - base % 512) % 512);
        assert_eq!(frame.len() % 512, 0);
        chunks.push((28 + payload.len()) as u32);
        sizes.push(frame.len() as u32);
        payload.extend_from_slice(&frame);
    }
    let mut ftyp = b"isom".to_vec();
    ftyp.extend_from_slice(&0u32.to_be_bytes());
    ftyp.extend_from_slice(b"isom");
    let ftyp = bmff_box(b"ftyp", &ftyp);
    let mdat = bmff_box(b"mdat", &payload);
    assert_eq!(ftyp.len() + 8, 28);
    let full_box = |kind: &[u8; 4], body: &[u8]| {
        let mut p = vec![0u8; 4]; // version and flags
        p.extend_from_slice(body);
        bmff_box(kind, &p)
    };
    let mut stsz = 0u32.to_be_bytes().to_vec();
    stsz.extend_from_slice(&(sizes.len() as u32).to_be_bytes());
    sizes
        .iter()
        .for_each(|s| stsz.extend_from_slice(&s.to_be_bytes()));
    let mut stsc = 1u32.to_be_bytes().to_vec();
    [1u32, 1, 1]
        .iter()
        .for_each(|v| stsc.extend_from_slice(&v.to_be_bytes()));
    let mut stco = (chunks.len() as u32).to_be_bytes().to_vec();
    chunks
        .iter()
        .for_each(|c| stco.extend_from_slice(&c.to_be_bytes()));
    let stbl = bmff_box(
        b"stbl",
        &[
            stsd_box(b"mjpa"),
            full_box(b"stsz", &stsz),
            full_box(b"stsc", &stsc),
            full_box(b"stco", &stco),
        ]
        .concat(),
    );
    let mut hdlr = vec![0; 8];
    hdlr.extend_from_slice(b"vide");
    hdlr.extend_from_slice(&[0; 13]);
    let mdia = bmff_box(
        b"mdia",
        &[bmff_box(b"hdlr", &hdlr), bmff_box(b"minf", &stbl)].concat(),
    );
    let trak = bmff_box(b"trak", &[bmff_box(b"tkhd", &[0; 84]), mdia].concat());
    let moov = bmff_box(b"moov", &[bmff_box(b"mvhd", &[0; 100]), trak].concat());
    let bytes = [ftyp, mdat, moov].concat();
    let moov_at = 28 + payload.len();
    AvcVideo {
        mdat_payload: 28..moov_at,
        moov: moov_at..bytes.len(),
        bytes,
    }
}

/// Which camera family's TIFF flavor [`tiff_raw`] imitates (header magic and default `Make`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TiffVariant {
    Cr2,
    Nef,
    Arw,
    Dng,
    Orf,
    Rw2,
    Plain,
}

/// EXIF fields written by [`tiff_raw`]; `None` omits the tag.
#[derive(Clone, Debug, Default)]
pub struct TestExif {
    pub make: Option<String>,
    pub model: Option<String>,
    pub lens: Option<String>,
    pub serial: Option<String>,
    pub iso: Option<u16>,
    /// "YYYY:MM:DD HH:MM:SS", as stored in EXIF.
    pub date: Option<String>,
}

/// Shape of a synthetic TIFF-based RAW built by [`tiff_raw`].
pub struct TiffRawOptions {
    pub variant: TiffVariant,
    /// Big-endian byte order (ignored for [`TiffVariant::Rw2`], which is always little-endian).
    pub big_endian: bool,
    /// Bytes of raw image data (noise), placed last in the file. Must be at least 1.
    pub raw_len: usize,
    pub seed: u64,
    /// Embedded JPEG preview (width, height); `None` = no preview.
    pub preview: Option<(u32, u32)>,
    /// Place the preview at a multiple of 512 from the start of the file.
    pub preview_sector_aligned: bool,
    pub exif: TestExif,
}

/// A synthetic RAW file and where its parts are.
pub struct TiffRaw {
    pub bytes: Vec<u8>,
    pub preview: Option<std::ops::Range<usize>>,
    pub raw: std::ops::Range<usize>,
}

struct TiffEntry {
    tag: u16,
    typ: u16,
    count: u32,
    data: Vec<u8>,
}

#[derive(Clone, Copy)]
struct Enc(bool);

impl Enc {
    fn u16(self, v: u16) -> [u8; 2] {
        if self.0 {
            v.to_be_bytes()
        } else {
            v.to_le_bytes()
        }
    }

    fn u32(self, v: u32) -> [u8; 4] {
        if self.0 {
            v.to_be_bytes()
        } else {
            v.to_le_bytes()
        }
    }

    fn ascii(self, tag: u16, s: &str) -> TiffEntry {
        let mut data = s.as_bytes().to_vec();
        data.push(0);
        TiffEntry {
            tag,
            typ: 2,
            count: data.len() as u32,
            data,
        }
    }

    fn short(self, tag: u16, v: u16) -> TiffEntry {
        TiffEntry {
            tag,
            typ: 3,
            count: 1,
            data: self.u16(v).to_vec(),
        }
    }

    fn long(self, tag: u16, v: u32) -> TiffEntry {
        TiffEntry {
            tag,
            typ: 4,
            count: 1,
            data: self.u32(v).to_vec(),
        }
    }

    /// One IFD: entry table, then the values that do not fit in four bytes. `start` is where the IFD will sit.
    fn ifd(self, start: u32, mut entries: Vec<TiffEntry>) -> Vec<u8> {
        entries.sort_by_key(|e| e.tag);
        let mut table = Vec::new();
        table.extend_from_slice(&self.u16(entries.len() as u16));
        let mut cursor = start + 2 + 12 * entries.len() as u32 + 4;
        let mut external = Vec::new();
        for e in &entries {
            table.extend_from_slice(&self.u16(e.tag));
            table.extend_from_slice(&self.u16(e.typ));
            table.extend_from_slice(&self.u32(e.count));
            if e.data.len() <= 4 {
                let mut inline = e.data.clone();
                inline.resize(4, 0);
                table.extend_from_slice(&inline);
            } else {
                table.extend_from_slice(&self.u32(cursor));
                external.extend_from_slice(&e.data);
                if e.data.len() % 2 == 1 {
                    external.push(0);
                }
                cursor += (e.data.len() + e.data.len() % 2) as u32;
            }
        }
        table.extend_from_slice(&[0; 4]); // no next IFD
        table.extend(external);
        table
    }
}

/// Builds a TIFF-based RAW: header (with the variant's magic), IFD0 (Make, Model, size, preview JPEG pointers, Exif
/// pointer, SubIFDs, DNGVersion for DNG), one SubIFD holding the raw strip, and an Exif IFD. The raw data comes last, so
/// the last byte of the file is referenced.
pub fn tiff_raw(opts: &TiffRawOptions) -> TiffRaw {
    assert!(opts.raw_len > 0, "raw_len must be at least 1");
    let enc = Enc(opts.big_endian && opts.variant != TiffVariant::Rw2);
    let header_len: u32 = if opts.variant == TiffVariant::Cr2 {
        16
    } else {
        8
    };
    let preview_bytes = opts.preview.map(|(w, h)| jpeg(w, h, opts.seed, 0));
    let preview_len = preview_bytes.as_ref().map_or(0, Vec::len) as u32;
    let make = opts.exif.make.clone().unwrap_or_else(|| {
        match opts.variant {
            TiffVariant::Cr2 => "Canon",
            TiffVariant::Nef => "NIKON CORPORATION",
            TiffVariant::Arw => "SONY",
            TiffVariant::Orf => "OM Digital Solutions",
            TiffVariant::Rw2 => "Panasonic",
            TiffVariant::Dng => "Google",
            TiffVariant::Plain => "Generic",
        }
        .to_string()
    });
    let (ifd0_w, ifd0_h) = opts.preview.unwrap_or((6000, 4000));

    // Sizes do not depend on the offsets, so build once with placeholders to learn the layout, then for real.
    let build = |sub: u32, exif: u32, prev: u32, raw: u32| {
        let mut e0 = vec![
            enc.long(0x0100, ifd0_w),
            enc.long(0x0101, ifd0_h),
            enc.ascii(0x010F, &make),
        ];
        if let Some(m) = &opts.exif.model {
            e0.push(enc.ascii(0x0110, m));
        }
        e0.push(enc.long(0x014A, sub));
        if opts.preview.is_some() {
            e0.push(enc.long(0x0201, prev));
            e0.push(enc.long(0x0202, preview_len));
        }
        let ex = exif_entries(enc, &opts.exif);
        if !ex.is_empty() {
            e0.push(enc.long(0x8769, exif));
        }
        if opts.variant == TiffVariant::Dng {
            e0.push(TiffEntry {
                tag: 0xC612,
                typ: 1,
                count: 4,
                data: vec![1, 4, 0, 0],
            });
        }
        let es = vec![
            enc.long(0x0100, 6000),
            enc.long(0x0101, 4000),
            enc.short(0x0103, 1),
            enc.long(0x0111, raw),
            enc.long(0x0117, opts.raw_len as u32),
        ];
        (e0, es, ex)
    };
    let sizes = {
        let (e0, es, ex) = build(0, 0, 0, 0);
        let ex_len = if ex.is_empty() {
            0
        } else {
            enc.ifd(0, ex).len()
        };
        (
            enc.ifd(0, e0).len() as u32,
            enc.ifd(0, es).len() as u32,
            ex_len as u32,
        )
    };
    let ifd0_at = header_len;
    let sub_at = ifd0_at + sizes.0;
    let exif_at = sub_at + sizes.1;
    let mut prev_at = exif_at + sizes.2;
    if opts.preview_sector_aligned && preview_len > 0 {
        prev_at = prev_at.div_ceil(512) * 512;
    }
    let raw_at = prev_at + preview_len;
    let (e0, es, ex) = build(sub_at, exif_at, prev_at, raw_at);

    let mut bytes = Vec::new();
    match (opts.variant, enc.0) {
        (TiffVariant::Orf, false) => bytes.extend_from_slice(b"IIRO"),
        (TiffVariant::Orf, true) => bytes.extend_from_slice(b"MMOR"),
        (TiffVariant::Rw2, _) => bytes.extend_from_slice(b"IIU\0"),
        (_, false) => bytes.extend_from_slice(&[b'I', b'I', 0x2A, 0]),
        (_, true) => bytes.extend_from_slice(&[b'M', b'M', 0, 0x2A]),
    }
    bytes.extend_from_slice(&enc.u32(ifd0_at));
    if opts.variant == TiffVariant::Cr2 {
        bytes.extend_from_slice(b"CR\x02\0");
        bytes.extend_from_slice(&[0; 4]);
    }
    bytes.extend(enc.ifd(ifd0_at, e0));
    bytes.extend(enc.ifd(sub_at, es));
    if !ex.is_empty() {
        bytes.extend(enc.ifd(exif_at, ex));
    }
    bytes.resize(prev_at as usize, 0);
    if let Some(p) = &preview_bytes {
        bytes.extend_from_slice(p);
    }
    let raw_start = bytes.len();
    debug_assert_eq!(raw_start as u32, raw_at);
    bytes.extend_from_slice(&DiskImage::noise(opts.raw_len, opts.seed ^ 0x0052_4157).bytes);
    TiffRaw {
        preview: preview_bytes.map(|p| prev_at as usize..prev_at as usize + p.len()),
        raw: raw_start..bytes.len(),
        bytes,
    }
}

/// Shape of a synthetic Panasonic RW2 built by [`panasonic_rw2`].
pub struct Rw2Options {
    /// Sensor size (Panasonic tags 0x0002/0x0003).
    pub width: u16,
    pub height: u16,
    /// Panasonic RawFormat (tag 0x002D) and BitsPerSample (tag 0x000A).
    pub raw_format: u16,
    pub bits: u16,
    /// Bytes of raw data (noise) at the end of the file.
    pub raw_len: usize,
    /// StripByteCounts value next to StripOffsets 0xFFFFFFFF: 0 on most bodies, an unpacked size on some (DMC-FZ28).
    pub strip_len: u32,
    pub seed: u64,
    pub exif: TestExif,
}

/// A Panasonic RW2 laid out like real ones (DMC-LX7, DC-G100, DC-GH5S): "IIU\0" header; IFD0 with the Panasonic sensor
/// tags (0x0002/0x0003 size, 0x000A bits, 0x000B compression 34316, 0x002D raw format), a 160x120 preview JPEG as tag
/// 0x002E (UNDEFINED, starting on a 512-byte boundary, with an EXIF APP1 holding a 16x16 thumbnail JPEG that also starts
/// on a 512-byte boundary, as in a real DMC-LX7), StripOffsets 0xFFFFFFFF with StripByteCounts 0, the raw data offset in
/// tag 0x0118 (no length anywhere), and an Exif IFD. The raw data comes last, on a 512-byte boundary.
pub fn panasonic_rw2(opts: &Rw2Options) -> TiffRaw {
    let enc = Enc(false);
    let preview = jpeg_with_thumbnail_at(160, 120, opts.seed, 512);
    let make = opts.exif.make.clone().unwrap_or_else(|| "Panasonic".into());
    let entries = |exif_at: u32, raw_at: u32| {
        let mut e = vec![
            enc.short(0x0002, opts.width),
            enc.short(0x0003, opts.height),
            enc.short(0x000A, opts.bits),
            enc.short(0x000B, 34316),
            enc.short(0x002D, opts.raw_format),
            TiffEntry {
                tag: 0x002E,
                typ: 7,
                count: preview.len() as u32,
                data: preview.clone(),
            },
            enc.ascii(0x010F, &make),
            enc.long(0x0111, u32::MAX),
            enc.long(0x0117, opts.strip_len),
            enc.long(0x0118, raw_at),
        ];
        e.extend(opts.exif.model.as_deref().map(|m| enc.ascii(0x0110, m)));
        let ex = exif_entries(enc, &opts.exif);
        if !ex.is_empty() {
            e.push(enc.long(0x8769, exif_at));
        }
        e
    };
    let n = entries(0, 0).len() as u32;
    // 0x002E is the first value stored outside the table: start the IFD so that it lands on 512.
    let ifd0_at = 512 - (2 + 12 * n + 4);
    let ifd0_len = enc.ifd(ifd0_at, entries(0, 0)).len() as u32;
    let exif_at = (ifd0_at + ifd0_len).next_multiple_of(2);
    let ex = exif_entries(enc, &opts.exif);
    let ex_bytes = if ex.is_empty() {
        Vec::new()
    } else {
        enc.ifd(exif_at, ex)
    };
    let raw_at = (exif_at + ex_bytes.len() as u32).next_multiple_of(512);
    let mut bytes = b"IIU\0".to_vec();
    bytes.extend_from_slice(&enc.u32(ifd0_at));
    bytes.resize(ifd0_at as usize, 0);
    bytes.extend(enc.ifd(ifd0_at, entries(exif_at, raw_at)));
    bytes.resize(exif_at as usize, 0);
    bytes.extend(ex_bytes);
    bytes.resize(raw_at as usize, 0);
    bytes.extend_from_slice(&DiskImage::noise(opts.raw_len, opts.seed ^ 0x0052_5732).bytes);
    TiffRaw {
        preview: Some(512..512 + preview.len()),
        raw: raw_at as usize..bytes.len(),
        bytes,
    }
}

/// Like [`jpeg`] without padding, with an APP1 `Exif\0\0` segment holding a 16x16 thumbnail JPEG that starts `at` bytes
/// from the start of the JPEG (zeros fill the payload before it). `at` must be at least 12 and leave the APP1 under the
/// segment limit.
pub fn jpeg_with_thumbnail_at(width: u32, height: u32, seed: u64, at: usize) -> Vec<u8> {
    let base = encode_noise(width, height, seed);
    // SOI (2) + APP1 marker and length (4) come before the payload.
    let mut payload = b"Exif\0\0".to_vec();
    payload.resize(at - 6, 0);
    payload.extend(encode_noise(16, 16, seed ^ 0x5DEE_CE66));
    assert!(payload.len() <= MAX_SEGMENT_PAYLOAD);
    let mut out = base[..2].to_vec();
    out.extend(app1_segment(&payload));
    out.extend_from_slice(&base[2..]);
    debug_assert_eq!(&out[at..at + 3], &[0xFF, 0xD8, 0xFF]);
    out
}

/// One IFD table with every value inline (`(tag, type, count, value)`), little-endian, no next IFD.
fn inline_ifd(entries: &[(u16, u16, u32, u32)]) -> Vec<u8> {
    let mut out = (entries.len() as u16).to_le_bytes().to_vec();
    for &(tag, typ, count, value) in entries {
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&typ.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(&value.to_le_bytes());
    }
    out.extend_from_slice(&[0; 4]);
    out
}

/// An Olympus/OM System ORF laid out like real ones (E-PL2, E-M10 Mark II, TG-7): "IIRO" header; IFD0 with size, Make,
/// Model, one raw strip and an Exif pointer; an Exif IFD whose MakerNote (0x927C) starts on a 512-byte boundary and holds
/// a little-endian Olympus IFD (header "OLYMPUS\0II\x03\0" with the IFD at +12, or with `om_system` "OM SYSTEM\0\0\0II\x04\0"
/// with the IFD at +16; offsets relative to the MakerNote start): a 64x48 thumbnail JPEG (tag 0x0100, UNDEFINED) and a
/// CameraSettings IFD (tag 0x2020) whose PreviewImageStart/Length (0x0101/0x0102) give a 320x240 preview. Both JPEGs
/// start on 512-byte boundaries inside the MakerNote; the raw strip (noise) follows it and ends the file.
///
/// `preview` is the larger JPEG's range, `raw` the strip.
pub fn olympus_orf(om_system: bool, raw_len: usize, seed: u64, exif: &TestExif) -> TiffRaw {
    let enc = Enc(false);
    let thumb = jpeg(64, 48, seed, 0);
    let big = jpeg(320, 240, seed ^ 1, 0);
    // MakerNote, offsets relative to its own start.
    let (header, ifd_at): (&[u8], u32) = if om_system {
        (b"OM SYSTEM\0\0\0II\x04\0", 16)
    } else {
        (b"OLYMPUS\0II\x03\0", 12)
    };
    let main_len = 2 + 12 * 2 + 4;
    let settings_at = ifd_at + main_len;
    let thumb_at = 512u32;
    let big_at = (thumb_at + thumb.len() as u32).next_multiple_of(512);
    let mut note = header.to_vec();
    note.extend(inline_ifd(&[
        (0x0100, 7, thumb.len() as u32, thumb_at),
        (0x2020, 13, 1, settings_at),
    ]));
    note.extend(inline_ifd(&[
        (0x0100, 3, 1, 1),
        (0x0101, 4, 1, big_at),
        (0x0102, 4, 1, big.len() as u32),
    ]));
    note.resize(thumb_at as usize, 0);
    note.extend_from_slice(&thumb);
    note.resize(big_at as usize, 0);
    note.extend_from_slice(&big);

    let make = exif
        .make
        .clone()
        .unwrap_or_else(|| "OM Digital Solutions".into());
    let ifd0 = |exif_at: u32, raw_at: u32| {
        let mut e = vec![
            enc.long(0x0100, 4000),
            enc.long(0x0101, 3000),
            enc.ascii(0x010F, &make),
            enc.long(0x0111, raw_at),
            enc.long(0x0117, raw_len as u32),
            enc.long(0x8769, exif_at),
        ];
        e.extend(exif.model.as_deref().map(|m| enc.ascii(0x0110, m)));
        e
    };
    let mut ex = exif_entries(enc, exif);
    ex.push(TiffEntry {
        tag: 0x927C,
        typ: 7,
        count: note.len() as u32,
        data: note.clone(),
    });
    // Bytes of the Exif IFD before the MakerNote's value: the table and the values of lower tags stored outside it.
    let before_note = 2
        + 12 * ex.len() as u32
        + 4
        + ex.iter()
            .filter(|e| e.tag < 0x927C && e.data.len() > 4)
            .map(|e| e.data.len().next_multiple_of(2) as u32)
            .sum::<u32>();
    let ifd0_len = enc.ifd(8, ifd0(0, 0)).len() as u32;
    let exif_at = (8 + ifd0_len + before_note).next_multiple_of(512) - before_note;
    let note_at = exif_at + before_note;
    let ex_bytes = enc.ifd(exif_at, ex);
    let raw_at = (exif_at + ex_bytes.len() as u32).next_multiple_of(2);
    let mut bytes = b"IIRO".to_vec();
    bytes.extend_from_slice(&enc.u32(8));
    bytes.extend(enc.ifd(8, ifd0(exif_at, raw_at)));
    bytes.resize(exif_at as usize, 0);
    bytes.extend(ex_bytes);
    bytes.resize(raw_at as usize, 0);
    debug_assert_eq!(&bytes[note_at as usize..][..header.len()], header);
    bytes.extend_from_slice(&DiskImage::noise(raw_len, seed ^ 0x004F_5246).bytes);
    let big_abs = (note_at + big_at) as usize;
    TiffRaw {
        preview: Some(big_abs..big_abs + big.len()),
        raw: raw_at as usize..bytes.len(),
        bytes,
    }
}

/// The Exif-IFD entries for the `TestExif` fields that are set (ISO, date, serial, lens).
fn exif_entries(enc: Enc, x: &TestExif) -> Vec<TiffEntry> {
    let mut ex = Vec::new();
    if let Some(v) = x.iso {
        ex.push(enc.short(0x8827, v));
    }
    if let Some(v) = &x.date {
        ex.push(enc.ascii(0x9003, v));
    }
    if let Some(v) = &x.serial {
        ex.push(enc.ascii(0xA431, v));
    }
    if let Some(v) = &x.lens {
        ex.push(enc.ascii(0xA434, v));
    }
    ex
}

/// IFD0 entries for Make and Model (those that are set).
fn make_model_entries(enc: Enc, x: &TestExif) -> Vec<TiffEntry> {
    let mut e = Vec::new();
    if let Some(v) = &x.make {
        e.push(enc.ascii(0x010F, v));
    }
    if let Some(v) = &x.model {
        e.push(enc.ascii(0x0110, v));
    }
    e
}

/// Little-endian TIFF header (offset of IFD0 = 8) followed by one IFD.
fn le_tiff(enc: Enc, entries: Vec<TiffEntry>) -> Vec<u8> {
    let mut out = b"II*\0".to_vec();
    out.extend_from_slice(&enc.u32(8));
    out.extend(enc.ifd(8, entries));
    out
}

/// Little-endian TIFF with IFD0 (Make, Model, Exif pointer) and an Exif IFD holding the other `TestExif` fields.
fn exif_tiff(exif: &TestExif) -> Vec<u8> {
    let enc = Enc(false);
    let ifd0 = |ptr: u32| {
        let mut e = make_model_entries(enc, exif);
        if !exif_entries(enc, exif).is_empty() {
            e.push(enc.long(0x8769, ptr));
        }
        e
    };
    let exif_at = 8 + enc.ifd(8, ifd0(0)).len() as u32;
    let mut out = le_tiff(enc, ifd0(exif_at));
    let ex = exif_entries(enc, exif);
    if !ex.is_empty() {
        out.extend(enc.ifd(exif_at, ex));
    }
    out
}

/// Like [`jpeg`] without padding, with an APP1 `Exif\0\0` segment holding a little-endian TIFF: IFD0 with Make and Model
/// (those set in `exif`) and an Exif IFD with the other fields.
pub fn jpeg_with_exif(width: u32, height: u32, seed: u64, exif: &TestExif) -> Vec<u8> {
    let base = encode_noise(width, height, seed);
    let mut payload = b"Exif\0\0".to_vec();
    payload.extend(exif_tiff(exif));
    let mut out = base[..2].to_vec(); // SOI
    out.extend(app1_segment(&payload));
    out.extend_from_slice(&base[2..]);
    out
}

/// Like [`jpeg`] without padding, with `count` extra 100-byte APP2 segments (as split ICC profiles) before the rest of
/// the headers, so the SOF comes after more than `count` marker segments.
pub fn jpeg_with_app_segments(width: u32, height: u32, seed: u64, count: usize) -> Vec<u8> {
    let base = encode_noise(width, height, seed);
    let mut out = base[..2].to_vec();
    for i in 0..count {
        let mut seg = vec![0xFF, 0xE2, 0, 102];
        seg.extend(std::iter::repeat_n(i as u8, 100));
        out.extend(seg);
    }
    out.extend_from_slice(&base[2..]);
    out
}

/// Code lengths (counts for lengths 1..=16) and symbols of the typical luminance DC table (ITU-T T.81, Table K.3).
const STD_LUMA_DC_BITS: [u8; 16] = [0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0];
const STD_LUMA_DC_VALS: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
/// Code lengths and symbols of the typical luminance AC table (ITU-T T.81, Table K.5).
const STD_LUMA_AC_BITS: [u8; 16] = [0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 0x7D];
const STD_LUMA_AC_VALS: [u8; 162] = [
    0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61, 0x07,
    0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xA1, 0x08, 0x23, 0x42, 0xB1, 0xC1, 0x15, 0x52, 0xD1, 0xF0,
    0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0A, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x25, 0x26, 0x27, 0x28,
    0x29, 0x2A, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3A, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49,
    0x4A, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5A, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69,
    0x6A, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7A, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89,
    0x8A, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9A, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7,
    0xA8, 0xA9, 0xAA, 0xB2, 0xB3, 0xB4, 0xB5, 0xB6, 0xB7, 0xB8, 0xB9, 0xBA, 0xC2, 0xC3, 0xC4, 0xC5,
    0xC6, 0xC7, 0xC8, 0xC9, 0xCA, 0xD2, 0xD3, 0xD4, 0xD5, 0xD6, 0xD7, 0xD8, 0xD9, 0xDA, 0xE1, 0xE2,
    0xE3, 0xE4, 0xE5, 0xE6, 0xE7, 0xE8, 0xE9, 0xEA, 0xF1, 0xF2, 0xF3, 0xF4, 0xF5, 0xF6, 0xF7, 0xF8,
    0xF9, 0xFA,
];

/// MSB-first bit writer for entropy-coded data: an `FF` data byte is followed by a stuffed `00`.
#[derive(Default)]
struct BitWriter {
    out: Vec<u8>,
    acc: u8,
    used: u32,
}

impl BitWriter {
    fn bits(&mut self, value: u32, count: u32) {
        for i in (0..count).rev() {
            self.acc = (self.acc << 1) | ((value >> i) & 1) as u8;
            self.used += 1;
            if self.used == 8 {
                self.byte_out();
            }
        }
    }

    fn byte_out(&mut self) {
        self.out.push(self.acc);
        if self.acc == 0xFF {
            self.out.push(0x00);
        }
        self.acc = 0;
        self.used = 0;
    }

    /// Pads the last byte with 1-bits.
    fn align(&mut self) {
        while self.used != 0 {
            self.bits(1, 1);
        }
    }
}

/// Grayscale baseline JPEG of a flat gray image, standard luminance Huffman tables, DRI = `interval` MCUs, built
/// byte by byte (every block: DC diff 0, EOB; RSTn every `interval` MCUs, byte-aligned with 1-bits). Strict-decodable.
/// `interval == 0` means no DRI segment and no RST markers.
pub fn jpeg_flat_with_restarts(width: u32, height: u32, interval: u16) -> Vec<u8> {
    flat_jpeg(width, height, &[(1, 1)], interval, &|_, _| 0)
}

/// Like [`jpeg_flat_with_restarts`], but block `(bx, by)` has the quantized DC value `dc(bx, by)` (its pixels are
/// `128 + dc / 8`; keep `|dc| ≤ 1016`). The DC predictor resets at each restart marker, as T.81 requires.
pub fn jpeg_gray_dc(
    width: u32,
    height: u32,
    interval: u16,
    dc: &dyn Fn(u32, u32) -> i16,
) -> Vec<u8> {
    flat_jpeg(width, height, &[(1, 1)], interval, dc)
}

/// Like [`jpeg_flat_with_restarts`], with three interleaved components (Y, Cb, Cr): Y sampled `luma` = `(h, v)` (each 1
/// or 2), Cb and Cr 1×1, all coded with the standard luminance tables. Each MCU holds `h × v` Y blocks then one Cb and
/// one Cr block. Strict-decodable, flat mid gray.
pub fn jpeg_flat_ycbcr(width: u32, height: u32, luma: (u8, u8), interval: u16) -> Vec<u8> {
    flat_jpeg(width, height, &[luma, (1, 1), (1, 1)], interval, &|_, _| 0)
}

/// Progressive (SOF2) grayscale JPEG of a flat mid-gray image, built byte by byte with the standard luminance tables: a
/// DC scan (every block: DC difference 0), then an AC scan of bands 1–63 (every block: end of band). Decodable by any
/// progressive decoder; for testing what happens to a frame Revenant's own entropy decoder does not cover.
pub fn jpeg_progressive_flat(width: u32, height: u32) -> Vec<u8> {
    let (w, h) = (
        u16::try_from(width).expect("width fits a SOF"),
        u16::try_from(height).expect("height fits a SOF"),
    );
    assert!(w > 0 && h > 0);
    let mut out = vec![0xFF, 0xD8];
    out.extend_from_slice(&[0xFF, 0xDB, 0x00, 0x43, 0x00]);
    out.extend_from_slice(&[1; 64]);
    out.extend_from_slice(&[0xFF, 0xC2, 0x00, 0x0B, 0x08]);
    out.extend_from_slice(&h.to_be_bytes());
    out.extend_from_slice(&w.to_be_bytes());
    out.extend_from_slice(&[0x01, 0x01, 0x11, 0x00]);
    for (class, bits, vals) in [
        (0x00, &STD_LUMA_DC_BITS, &STD_LUMA_DC_VALS[..]),
        (0x10, &STD_LUMA_AC_BITS, &STD_LUMA_AC_VALS[..]),
    ] {
        out.extend_from_slice(&[0xFF, 0xC4]);
        out.extend_from_slice(&((2 + 1 + 16 + vals.len()) as u16).to_be_bytes());
        out.push(class);
        out.extend_from_slice(bits);
        out.extend_from_slice(vals);
    }
    let blocks = u32::from(w.div_ceil(8)) * u32::from(h.div_ceil(8));
    // (Ss, Se, the code of each block): DC difference 0 is `00`; end of band (EOB0, symbol 0x00) is `1010`.
    for (ss, se, code, len) in [(0u8, 0u8, 0b00, 2), (1, 63, 0b1010, 4)] {
        out.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, ss, se, 0x00]);
        let mut data = BitWriter::default();
        for _ in 0..blocks {
            data.bits(code, len);
        }
        data.align();
        out.extend(data.out);
    }
    out.extend_from_slice(&[0xFF, 0xD9]);
    out
}

/// Code of each DC category (0..=11) of the typical luminance DC table: `(code, length)`, canonical (T.81 Annex C).
fn std_dc_codes() -> [(u32, u32); 12] {
    let mut codes = [(0, 0); 12];
    let (mut code, mut k) = (0u32, 0usize);
    for len in 1..=16u32 {
        for _ in 0..STD_LUMA_DC_BITS[len as usize - 1] {
            codes[usize::from(STD_LUMA_DC_VALS[k])] = (code, len);
            code += 1;
            k += 1;
        }
        code <<= 1;
    }
    codes
}

/// Baseline JPEG with the components sampled `(h, v)` each (ids 1, 2, ...), every one interleaved in one scan. Every
/// block has no AC coefficient; a single-component image's block `(bx, by)` has the DC value `dc(bx, by)`, the blocks of
/// several components DC 0.
fn flat_jpeg(
    width: u32,
    height: u32,
    comps: &[(u8, u8)],
    interval: u16,
    dc: &dyn Fn(u32, u32) -> i16,
) -> Vec<u8> {
    let (w, h) = (
        u16::try_from(width).expect("width fits a SOF"),
        u16::try_from(height).expect("height fits a SOF"),
    );
    assert!(w > 0 && h > 0 && (1..=4).contains(&comps.len()));
    let mut out = vec![0xFF, 0xD8];
    // DQT: table 0, 8-bit, every step 1.
    out.extend_from_slice(&[0xFF, 0xDB, 0x00, 0x43, 0x00]);
    out.extend_from_slice(&[1; 64]);
    // SOF0: 8-bit, each component with its sampling and quantization table 0.
    out.extend_from_slice(&[0xFF, 0xC0]);
    out.extend_from_slice(&(8 + 3 * comps.len() as u16).to_be_bytes());
    out.push(0x08);
    out.extend_from_slice(&h.to_be_bytes());
    out.extend_from_slice(&w.to_be_bytes());
    out.push(comps.len() as u8);
    for (i, &(ch, cv)) in comps.iter().enumerate() {
        out.extend_from_slice(&[i as u8 + 1, (ch << 4) | cv, 0x00]);
    }
    for (class, bits, vals) in [
        (0x00, &STD_LUMA_DC_BITS, &STD_LUMA_DC_VALS[..]),
        (0x10, &STD_LUMA_AC_BITS, &STD_LUMA_AC_VALS[..]),
    ] {
        out.extend_from_slice(&[0xFF, 0xC4]);
        out.extend_from_slice(&((2 + 1 + 16 + vals.len()) as u16).to_be_bytes());
        out.push(class);
        out.extend_from_slice(bits);
        out.extend_from_slice(vals);
    }
    if interval > 0 {
        out.extend_from_slice(&[0xFF, 0xDD, 0x00, 0x04]);
        out.extend_from_slice(&interval.to_be_bytes());
    }
    // SOS: every component, DC table 0, AC table 0, Ss 0, Se 63, Ah/Al 0.
    out.extend_from_slice(&[0xFF, 0xDA]);
    out.extend_from_slice(&(6 + 2 * comps.len() as u16).to_be_bytes());
    out.push(comps.len() as u8);
    for i in 0..comps.len() {
        out.extend_from_slice(&[i as u8 + 1, 0x00]);
    }
    out.extend_from_slice(&[0x00, 0x3F, 0x00]);

    // One component: one block per MCU over the image. Several: MCUs of 8·Hmax × 8·Vmax pixels.
    let (mcus, blocks) = if comps.len() == 1 {
        (u64::from(w.div_ceil(8)) * u64::from(h.div_ceil(8)), 1)
    } else {
        let h_max = u16::from(comps.iter().map(|c| c.0).max().unwrap());
        let v_max = u16::from(comps.iter().map(|c| c.1).max().unwrap());
        let mcus = u64::from(w.div_ceil(8 * h_max)) * u64::from(h.div_ceil(8 * v_max));
        (
            mcus,
            comps
                .iter()
                .map(|&(ch, cv)| u32::from(ch) * u32::from(cv))
                .sum(),
        )
    };
    let dc_codes = std_dc_codes();
    let blocks_x = u64::from(w.div_ceil(8));
    let mut data = BitWriter::default();
    let mut restarts = 0u8;
    let mut pred = 0i32;
    for m in 0..mcus {
        if interval > 0 && m > 0 && m % u64::from(interval) == 0 {
            data.align();
            data.out.extend_from_slice(&[0xFF, 0xD0 + restarts % 8]);
            restarts = restarts.wrapping_add(1);
            pred = 0;
        }
        for _ in 0..blocks {
            let value = if comps.len() == 1 {
                i32::from(dc((m % blocks_x) as u32, (m / blocks_x) as u32))
            } else {
                0
            };
            let diff = value - pred;
            pred = value;
            let category = 32 - diff.unsigned_abs().leading_zeros();
            let (code, len) = dc_codes[category as usize];
            data.bits(code, len);
            // T.81 F.1.2.1: a negative difference is sent as `diff − 1` in `category` bits.
            let bits = if diff < 0 { diff - 1 } else { diff };
            data.bits(bits as u32 & ((1 << category) - 1), category);
            data.bits(0b1010, 4); // AC end of block
        }
    }
    data.align();
    out.extend(data.out);
    out.extend_from_slice(&[0xFF, 0xD9]);
    out
}

fn put_be32(buf: &mut [u8], at: usize, v: u32) {
    buf[at..at + 4].copy_from_slice(&v.to_be_bytes());
}

/// "FUJIFILMCCD-RAW " header; at 0x54/0x58 the JPEG offset/len (big-endian u32), at 0x5C/0x60 the CFA header offset/len,
/// at 0x64/0x68 the CFA offset/len. The JPEG is a testkit JPEG with an EXIF APP1 built from `exif`. The CFA data is
/// `raw_len` bytes of noise and comes last.
pub fn raf(raw_len: usize, seed: u64, exif: &TestExif) -> Vec<u8> {
    raf_with_jpeg(raw_len, seed, &jpeg_with_exif(160, 120, seed, exif))
}

/// Like [`raf`], with `jpeg` as the embedded JPEG. It sits at offset 0x94 of the file.
pub fn raf_with_jpeg(raw_len: usize, seed: u64, jpeg: &[u8]) -> Vec<u8> {
    const HEADER: usize = 0x94;
    const CFA_HEADER_LEN: usize = 64;
    let cfa_header_at = (HEADER + jpeg.len()).next_multiple_of(16);
    let cfa_at = cfa_header_at + CFA_HEADER_LEN;
    let mut out = vec![0u8; cfa_at + raw_len];
    out[..16].copy_from_slice(b"FUJIFILMCCD-RAW ");
    out[16..20].copy_from_slice(b"0201");
    out[20..28].copy_from_slice(b"FF129502");
    for (at, v) in [
        (0x54, HEADER),
        (0x58, jpeg.len()),
        (0x5C, cfa_header_at),
        (0x60, CFA_HEADER_LEN),
        (0x64, cfa_at),
        (0x68, raw_len),
    ] {
        put_be32(&mut out, at, v as u32);
    }
    out[HEADER..HEADER + jpeg.len()].copy_from_slice(jpeg);
    SplitMix64(seed ^ 0x0052_4146).fill(&mut out[cfa_header_at..]);
    out
}

/// Canon's `uuid` that holds CMT1/CMT2 inside `moov`.
const CR3_META_UUID: [u8; 16] = [
    0x85, 0xc0, 0xb6, 0x87, 0x82, 0x0f, 0x11, 0xe0, 0x81, 0x11, 0xf4, 0xce, 0x46, 0x2b, 0x6a, 0x48,
];
/// Canon's top-level `uuid` that holds the `PRVW` preview.
const CR3_PRVW_UUID: [u8; 16] = [
    0xea, 0xf4, 0x2b, 0x5e, 0x1c, 0x98, 0x4b, 0x88, 0xb9, 0xfb, 0xb7, 0xdc, 0x40, 0x6e, 0x4d, 0x16,
];

/// `ftyp` major brand "crx " ; `moov` with uuid 85c0b687-820f-11e0-8111-f4ce462b6a48 holding CMT1 (TIFF IFD0: Make/Model)
/// and CMT2 (Exif IFD) boxes; one trak whose single chunk (stco) is a full-size testkit JPEG in `mdat`; uuid
/// eaf42b5e-1c98-4b88-b9fb-b7dc406e4d16 at top level holding a `PRVW` box with a smaller JPEG. Box order: `ftyp`, `moov`,
/// preview `uuid`, a `free` pad, `mdat`; the `mdat` payload (the full-size JPEG) starts at a multiple of 512.
pub fn cr3(seed: u64, exif: &TestExif) -> Vec<u8> {
    cr3_with_full_jpeg(seed, exif, &jpeg(640, 480, seed, 0))
}

/// Like [`cr3`], with `full` as the full-size JPEG (the first chunk, at the start of the `mdat` payload).
pub fn cr3_with_full_jpeg(seed: u64, exif: &TestExif, full: &[u8]) -> Vec<u8> {
    let enc = Enc(false);
    let small = jpeg(160, 120, seed ^ 0x5052_5657, 0);

    let mut ftyp = b"crx ".to_vec();
    ftyp.extend_from_slice(&1u32.to_be_bytes());
    ftyp.extend_from_slice(b"crx isom");
    let ftyp = bmff_box(b"ftyp", &ftyp);

    let cmt1 = le_tiff(enc, make_model_entries(enc, exif));
    let cmt2 = le_tiff(enc, exif_entries(enc, exif));
    let mut meta = CR3_META_UUID.to_vec();
    meta.extend(bmff_box(b"CMT1", &cmt1));
    meta.extend(bmff_box(b"CMT2", &cmt2));
    let meta = bmff_box(b"uuid", &meta);

    // PRVW: reserved (4), reserved (2), width (2), height (2), 1 (2), JPEG size (4), JPEG.
    let mut prvw = vec![0u8; 6];
    prvw.extend_from_slice(&160u16.to_be_bytes());
    prvw.extend_from_slice(&120u16.to_be_bytes());
    prvw.extend_from_slice(&1u16.to_be_bytes());
    prvw.extend_from_slice(&(small.len() as u32).to_be_bytes());
    prvw.extend_from_slice(&small);
    let mut prvw_uuid = CR3_PRVW_UUID.to_vec();
    prvw_uuid.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 1]);
    prvw_uuid.extend(bmff_box(b"PRVW", &prvw));
    let prvw_uuid = bmff_box(b"uuid", &prvw_uuid);

    let moov_len = moov_box_with(&[0], full.len() as u32, false, b"CRAW", &meta).len();
    let before = ftyp.len() + moov_len + prvw_uuid.len();
    let pad = 8 + (512 - (before + 16) % 512) % 512;
    let payload_at = before + pad + 8;
    let moov = moov_box_with(
        &[payload_at as u64],
        full.len() as u32,
        false,
        b"CRAW",
        &meta,
    );
    let mut free = vec![0u8; pad - 8];
    SplitMix64(seed).fill(&mut free);
    [
        ftyp,
        moov,
        prvw_uuid,
        bmff_box(b"free", &free),
        bmff_box(b"mdat", full),
    ]
    .concat()
}

/// ftyp "heic" (compatible "mif1"); meta with hdlr "pict", pitm, iinf (item 1 "hvc1", item 2 "Exif"), iloc (version 1,
/// extents for both items into `mdat`); `mdat` with `image_len` random bytes for item 1 and the Exif item (4-byte offset
/// prefix + TIFF built from `exif`).
pub fn heic(image_len: usize, seed: u64, exif: &TestExif) -> Vec<u8> {
    heic_with_tiff_offset(image_len, seed, exif, 0)
}

/// Like [`heic`], with the Exif item's offset prefix set to `tiff_offset` and that many junk bytes between the prefix and
/// the TIFF header.
pub fn heic_with_tiff_offset(
    image_len: usize,
    seed: u64,
    exif: &TestExif,
    tiff_offset: u32,
) -> Vec<u8> {
    let mut ftyp = b"heic".to_vec();
    ftyp.extend_from_slice(&0u32.to_be_bytes());
    ftyp.extend_from_slice(b"mif1heic");
    let ftyp = bmff_box(b"ftyp", &ftyp);

    let mut image = vec![0u8; image_len];
    SplitMix64(seed).fill(&mut image);
    let mut exif_item = tiff_offset.to_be_bytes().to_vec();
    exif_item.extend(std::iter::repeat_n(0xEE, tiff_offset as usize));
    exif_item.extend(exif_tiff(exif));

    let full_box = |kind: &[u8; 4], version: u8, body: &[u8]| {
        let mut p = vec![version, 0, 0, 0];
        p.extend_from_slice(body);
        bmff_box(kind, &p)
    };
    let mut hdlr = vec![0u8; 4];
    hdlr.extend_from_slice(b"pict");
    hdlr.extend_from_slice(&[0; 13]);
    let infe = |id: u16, typ: &[u8; 4]| {
        let mut b = id.to_be_bytes().to_vec();
        b.extend_from_slice(&[0, 0]);
        b.extend_from_slice(typ);
        b.push(0);
        full_box(b"infe", 2, &b)
    };
    let mut iinf = 2u16.to_be_bytes().to_vec();
    iinf.extend(infe(1, b"hvc1"));
    iinf.extend(infe(2, b"Exif"));
    let meta_with = |image_at: u32, exif_at: u32| {
        // iloc v1: offset and length 4 bytes, no base offset, no extent index.
        let mut iloc = vec![0x44, 0x00];
        iloc.extend_from_slice(&2u16.to_be_bytes());
        for (id, at, len) in [
            (1u16, image_at, image.len() as u32),
            (2, exif_at, exif_item.len() as u32),
        ] {
            iloc.extend_from_slice(&id.to_be_bytes());
            iloc.extend_from_slice(&0u16.to_be_bytes()); // construction method 0
            iloc.extend_from_slice(&0u16.to_be_bytes()); // data reference index
            iloc.extend_from_slice(&1u16.to_be_bytes()); // extent count
            iloc.extend_from_slice(&at.to_be_bytes());
            iloc.extend_from_slice(&len.to_be_bytes());
        }
        let children = [
            full_box(b"hdlr", 0, &hdlr),
            full_box(b"pitm", 0, &1u16.to_be_bytes()),
            full_box(b"iinf", 0, &iinf),
            full_box(b"iloc", 1, &iloc),
        ]
        .concat();
        full_box(b"meta", 0, &children)
    };
    let meta_len = meta_with(0, 0).len();
    let payload_at = (ftyp.len() + meta_len + 8) as u32;
    let meta = meta_with(payload_at, payload_at + image.len() as u32);
    [ftyp, meta, bmff_box(b"mdat", &[image, exif_item].concat())].concat()
}

/// Kinds of synthetic disk image streamed by [`write_bench_image`] for the scan benchmark.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BenchImage {
    /// Seeded noise only.
    Noise,
    /// Camera-like JPEGs back to back.
    DenseJpeg,
    /// TIFF-based RAW files (with a JPEG preview) back to back.
    DenseRaw,
    /// MP4 files with `moov` at the end.
    Video,
    /// Like `DenseJpeg`, but the second half of every file is replaced by noise.
    CutJpeg,
    /// An exFAT volume in an MBR disk (first LBA 8192) filling the image, holding the `DenseJpeg` files as live files
    /// in `DCIM/100TEST`, every 4th one deleted.
    ExfatCard,
}

impl BenchImage {
    pub const ALL: [BenchImage; 6] = [
        BenchImage::Noise,
        BenchImage::DenseJpeg,
        BenchImage::DenseRaw,
        BenchImage::Video,
        BenchImage::CutJpeg,
        BenchImage::ExfatCard,
    ];

    pub fn name(self) -> &'static str {
        match self {
            BenchImage::Noise => "noise",
            BenchImage::DenseJpeg => "dense-jpeg",
            BenchImage::DenseRaw => "dense-raw",
            BenchImage::Video => "video",
            BenchImage::CutJpeg => "cut-jpeg",
            BenchImage::ExfatCard => "exfat-card",
        }
    }

    pub fn parse(s: &str) -> Option<BenchImage> {
        BenchImage::ALL.into_iter().find(|k| k.name() == s)
    }
}

/// Images of at least this size use the large file shapes.
const BENCH_BIG: u64 = 512 * 1024 * 1024;

fn write_noise(rng: &mut SplitMix64, mut n: u64, out: &mut dyn Write) -> std::io::Result<()> {
    let mut buf = vec![0u8; 1 << 20];
    while n > 0 {
        let k = n.min(buf.len() as u64) as usize;
        rng.fill(&mut buf[..k]);
        out.write_all(&buf[..k])?;
        n -= k as u64;
    }
    Ok(())
}

/// Streams a synthetic disk image of exactly `len` bytes into `out`; returns how many files it placed. Files go back to
/// back, each at the next 512-byte boundary, the rest is seeded noise. Big images (`len >= 512 MiB`) use larger files:
/// `DenseJpeg`/`CutJpeg` cycle 8 distinct JPEGs (3000×2000, else 320×240); `DenseRaw` cycles 4 RAWs (Arw, Nef, Cr2, Dng;
/// 24 MiB / 1 MiB of raw data, preview 1616×1080 / 160×120); `Video` writes MP4s with `mdat` of 256 MiB / 8 MiB.
pub fn write_bench_image(kind: BenchImage, len: u64, out: &mut dyn Write) -> std::io::Result<u64> {
    if kind == BenchImage::ExfatCard {
        return write_exfat_card(len, out);
    }
    let big = len >= BENCH_BIG;
    let files: Vec<Vec<u8>> = match kind {
        BenchImage::Noise => Vec::new(),
        BenchImage::ExfatCard => unreachable!("handled above"),
        BenchImage::DenseJpeg | BenchImage::CutJpeg => {
            let (w, h) = if big { (3000, 2000) } else { (320, 240) };
            (0..8).map(|s| jpeg(w, h, 100 + s, 30_000)).collect()
        }
        BenchImage::DenseRaw => [
            TiffVariant::Arw,
            TiffVariant::Nef,
            TiffVariant::Cr2,
            TiffVariant::Dng,
        ]
        .into_iter()
        .zip(200u64..)
        .map(|(variant, seed)| {
            tiff_raw(&TiffRawOptions {
                variant,
                big_endian: false,
                raw_len: if big { 24 << 20 } else { 1 << 20 },
                seed,
                preview: Some(if big { (1616, 1080) } else { (160, 120) }),
                preview_sector_aligned: false,
                exif: TestExif::default(),
            })
            .bytes
        })
        .collect(),
        BenchImage::Video => vec![mp4(&Mp4Options {
            mdat_len: if big { 256 << 20 } else { 8 << 20 },
            moov_at_end: true,
            mov: false,
            mdat_64bit: false,
        })],
    };
    let mut rng = SplitMix64(0xBE7C_0001);
    let mut pos = 0u64;
    let mut placed = 0u64;
    let mut cycle = 0usize;
    while !files.is_empty() {
        let file = &files[cycle % files.len()];
        if pos + file.len() as u64 > len {
            break;
        }
        if kind == BenchImage::CutJpeg {
            let keep = file.len() / 2;
            out.write_all(&file[..keep])?;
            write_noise(&mut rng, (file.len() - keep) as u64, out)?;
        } else {
            out.write_all(file)?;
        }
        pos += file.len() as u64;
        placed += 1;
        cycle += 1;
        let next = pos.next_multiple_of(512);
        if next - pos > 0 {
            write_noise(&mut rng, (next - pos).min(len - pos), out)?;
            pos = next.min(len);
        }
    }
    write_noise(&mut rng, len - pos, out)?;
    Ok(placed)
}

/// First LBA of the `ExfatCard` volume (4 MiB, where cameras and macOS put it).
const CARD_FIRST_LBA: u32 = 8192;
/// Cluster size of the `ExfatCard` volume.
const CARD_CLUSTER: u32 = 4096;

/// The `ExfatCard` image: the volume is built in memory (peak memory about twice `len`: the builder keeps a copy of every file and `build()` allocates the image), then written after its MBR head.
fn write_exfat_card(len: u64, out: &mut dyn Write) -> std::io::Result<u64> {
    use fs::{DeleteStyle, FsBuilder, FsType, mbr_head};
    let head_len = u64::from(CARD_FIRST_LBA) * 512;
    assert!(
        len > head_len && len.is_multiple_of(512),
        "the exfat-card image needs a multiple of 512 bytes past 4 MiB"
    );
    let big = len >= BENCH_BIG;
    let (w, h) = if big { (3000, 2000) } else { (320, 240) };
    let photos: Vec<Vec<u8>> = (0..8).map(|s| jpeg(w, h, 100 + s, 30_000)).collect();
    let mut b = FsBuilder::new(FsType::Exfat, len - head_len, CARD_CLUSTER);
    b.mkdir("DCIM/100TEST");
    // Leave 2% of the clusters (and at least 64) for the bitmap, the folder entries and the rounding of each file.
    let cluster = u64::from(CARD_CLUSTER);
    let mut room = u64::from(b.cluster_count());
    room -= (room / 50).max(64);
    let mut names = Vec::new();
    while let Some(photo) = photos.get(names.len() % photos.len()) {
        let need = (photo.len() as u64).div_ceil(cluster);
        if need > room {
            break;
        }
        room -= need;
        let name = format!("DCIM/100TEST/IMG_{:05}.JPG", names.len());
        b.add(&name, photo);
        names.push(name);
    }
    for name in names.iter().skip(3).step_by(4) {
        b.delete(name, DeleteStyle::Camera);
    }
    let mut volume = b.build();
    drop(b);
    out.write_all(&mbr_head(&mut volume, CARD_FIRST_LBA))?;
    out.write_all(&volume)?;
    Ok(names.len() as u64)
}

#[cfg(test)]
mod bench_tests {
    use super::*;

    #[test]
    fn bench_images_have_exact_length_and_are_named() {
        for kind in BenchImage::ALL {
            let mut v = Vec::new();
            let placed = write_bench_image(kind, 16 << 20, &mut v).unwrap();
            assert_eq!(v.len(), 16 << 20, "{}", kind.name());
            assert_eq!(placed == 0, kind == BenchImage::Noise, "{}", kind.name());
            assert_eq!(BenchImage::parse(kind.name()), Some(kind));
        }
        assert_eq!(BenchImage::parse("nope"), None);
    }

    #[test]
    fn bench_images_are_deterministic() {
        let (mut a, mut b) = (Vec::new(), Vec::new());
        write_bench_image(BenchImage::DenseJpeg, 16 << 20, &mut a).unwrap();
        write_bench_image(BenchImage::DenseJpeg, 16 << 20, &mut b).unwrap();
        assert!(a == b);
    }
}

#[cfg(test)]
mod flat_jpeg_tests {
    use super::*;

    /// Payload of the first DHT segment of `jpeg` whose first byte (table class and id) is `class`.
    fn dht_payload(jpeg: &[u8], class: u8) -> Vec<u8> {
        let mut pos = 2;
        while jpeg[pos] == 0xFF && jpeg[pos + 1] != 0xDA {
            let len = usize::from(u16::from_be_bytes([jpeg[pos + 2], jpeg[pos + 3]]));
            if jpeg[pos + 1] == 0xC4 && jpeg[pos + 4] == class {
                return jpeg[pos + 4..pos + 2 + len].to_vec();
            }
            pos += 2 + len;
        }
        panic!("no DHT of class {class:#x}");
    }

    #[test]
    fn standard_tables_match_the_encoder() {
        // The `image` encoder writes the typical tables of Annex K; ours must be the same bytes.
        let reference = encode_noise(16, 16, 1);
        let flat = jpeg_flat_with_restarts(16, 16, 0);
        for class in [0x00, 0x10] {
            assert_eq!(dht_payload(&flat, class), dht_payload(&reference, class));
        }
    }

    #[test]
    fn flat_jpeg_decodes_to_mid_gray() {
        for (w, h, interval) in [(256, 128, 4), (33, 17, 0), (8, 8, 1), (512, 24, 7)] {
            let bytes = jpeg_flat_with_restarts(w, h, interval);
            let img = image::load_from_memory_with_format(&bytes, image::ImageFormat::Jpeg)
                .expect("decodes")
                .to_luma8();
            assert_eq!(img.dimensions(), (w, h));
            assert!(img.pixels().all(|p| p.0[0] == 128), "{w}x{h}/{interval}");
            let rst = bytes
                .windows(2)
                .filter(|p| p[0] == 0xFF && (0xD0..=0xD7).contains(&p[1]))
                .count();
            let mcus = w.div_ceil(8) * h.div_ceil(8);
            let expected = if interval == 0 {
                0
            } else {
                (mcus - 1) / u32::from(interval)
            };
            assert_eq!(rst as u32, expected, "{w}x{h}/{interval}");
        }
    }

    #[test]
    fn subsampled_flat_jpeg_decodes_to_mid_gray() {
        for (luma, interval) in [((2, 2), 0), ((2, 1), 0), ((2, 2), 2), ((1, 2), 3)] {
            let bytes = jpeg_flat_ycbcr(33, 17, luma, interval);
            let img = image::load_from_memory_with_format(&bytes, image::ImageFormat::Jpeg)
                .expect("decodes")
                .to_rgb8();
            assert_eq!(img.dimensions(), (33, 17));
            assert!(img.pixels().all(|p| p.0 == [128; 3]), "{luma:?}/{interval}");
        }
    }
}

#[cfg(test)]
mod gray_dc_tests {
    use super::*;

    #[test]
    fn gray_dc_jpeg_decodes_to_its_dc_values() {
        let dc = |bx: u32, by: u32| (bx as i16 - 8) * 40 + by as i16 * 24 - 200;
        for interval in [0, 3, 16] {
            let bytes = jpeg_gray_dc(128, 64, interval, &dc);
            let img = image::load_from_memory_with_format(&bytes, image::ImageFormat::Jpeg)
                .expect("decodes")
                .to_luma8();
            for by in 0..8 {
                for bx in 0..16 {
                    let want = (128 + i32::from(dc(bx, by)) / 8) as u8;
                    let got = img.get_pixel(bx * 8 + 3, by * 8 + 3).0[0];
                    assert!(
                        got.abs_diff(want) <= 1,
                        "{interval}: ({bx},{by}) {got} vs {want}"
                    );
                }
            }
        }
        assert!(jpeg_gray_dc(64, 16, 4, &|_, _| 0) == jpeg_flat_with_restarts(64, 16, 4));
    }
}

#[cfg(test)]
mod progressive_jpeg_tests {
    use super::*;

    #[test]
    fn progressive_flat_jpeg_decodes_to_mid_gray() {
        for (w, h) in [(64, 48), (33, 17)] {
            let bytes = jpeg_progressive_flat(w, h);
            assert_eq!(bytes[2..4], [0xFF, 0xDB]);
            assert!(bytes.windows(2).any(|m| m == [0xFF, 0xC2]));
            let img = image::load_from_memory_with_format(&bytes, image::ImageFormat::Jpeg)
                .expect("decodes")
                .to_luma8();
            assert_eq!(img.dimensions(), (w, h));
            assert!(img.pixels().all(|p| p.0[0] == 128), "{w}x{h}");
        }
    }
}

#[cfg(test)]
mod smooth_jpeg_tests {
    use super::*;

    #[test]
    fn smooth_jpeg_is_smooth_deterministic_and_bare() {
        let a = jpeg_smooth(256, 128, 1);
        assert!(a == jpeg_smooth(256, 128, 1));
        assert!(a != jpeg_smooth(256, 128, 2));
        // SOI then straight to the tables: no APP0/APP1.
        assert_eq!(a[..2], [0xFF, 0xD8]);
        assert!(!matches!(a[3], 0xE0..=0xEF));
        let img = image::load_from_memory_with_format(&a, image::ImageFormat::Jpeg)
            .expect("decodes")
            .to_luma8();
        assert_eq!(img.dimensions(), (256, 128));
        // Neighbouring pixels differ little, unlike noise.
        let mean_step = img
            .rows()
            .flat_map(|row| {
                let row: Vec<u8> = row.map(|p| p.0[0]).collect();
                (1..row.len())
                    .map(|i| u32::from(row[i].abs_diff(row[i - 1])))
                    .collect::<Vec<_>>()
            })
            .sum::<u32>() as f64
            / f64::from(256 * 127);
        assert!(mean_step < 4.0, "{mean_step}");
        let (min, max) = img
            .pixels()
            .fold((255, 0), |(lo, hi), p| (p.0[0].min(lo), p.0[0].max(hi)));
        assert!(max - min > 60, "{min}..{max}");
    }
}
