//! Synthetic test data for Revenant: deterministic JPEGs and disk images. Nothing here touches real data.

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
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, 90)
        .encode(&pixels, width, height, ExtendedColorType::Rgb8)
        .expect("encoding a JPEG in memory cannot fail");
    // Drop the JFIF APP0 (18 bytes) so tiny images stay as small as a baseline JPEG can be.
    if out[2..4] == [0xFF, 0xE0] {
        let len = usize::from(u16::from_be_bytes([out[4], out[5]]));
        out.drain(2..4 + len);
    }
    out
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

fn moov_box(chunk_offset: u64, sample_len: u32, wide: bool) -> Vec<u8> {
    let mvhd = bmff_box(b"mvhd", &[0; 100]);
    let mut stsz = vec![0; 4];
    stsz.extend_from_slice(&0u32.to_be_bytes()); // sample_size = 0: per-sample table
    stsz.extend_from_slice(&1u32.to_be_bytes());
    stsz.extend_from_slice(&sample_len.to_be_bytes());
    let mut chunks = vec![0; 4];
    chunks.extend_from_slice(&1u32.to_be_bytes());
    let chunk_table = if wide {
        chunks.extend_from_slice(&chunk_offset.to_be_bytes());
        bmff_box(b"co64", &chunks)
    } else {
        chunks.extend_from_slice(&(chunk_offset as u32).to_be_bytes());
        bmff_box(b"stco", &chunks)
    };
    let stbl = bmff_box(b"stbl", &[bmff_box(b"stsz", &stsz), chunk_table].concat());
    let trak = bmff_box(b"trak", &bmff_box(b"mdia", &bmff_box(b"minf", &stbl)));
    bmff_box(b"moov", &[mvhd, trak].concat())
}

/// MP4/MOV with `ftyp`, an `mdat` of noise, and a `moov` holding one `trak` whose `stsz`/`stco` (or `co64`) describe the
/// whole `mdat` payload as a single sample in a single chunk. The file starts at offset 0 of the returned bytes.
pub fn mp4(opts: &Mp4Options) -> Vec<u8> {
    let brand: &[u8; 4] = if opts.mov { b"qt  " } else { b"isom" };
    let mut ftyp = brand.to_vec();
    ftyp.extend_from_slice(&0u32.to_be_bytes());
    ftyp.extend_from_slice(brand);
    let ftyp = bmff_box(b"ftyp", &ftyp);

    let mut payload = vec![0u8; opts.mdat_len];
    SplitMix64(opts.mdat_len as u64).fill(&mut payload);
    let mdat = if opts.mdat_64bit {
        let mut b = 1u32.to_be_bytes().to_vec();
        b.extend_from_slice(b"mdat");
        b.extend_from_slice(&((payload.len() + 16) as u64).to_be_bytes());
        b.extend(payload);
        b
    } else {
        bmff_box(b"mdat", &payload)
    };
    let header = if opts.mdat_64bit { 16 } else { 8 };
    let moov_len = moov_box(0, 0, opts.mdat_64bit).len();
    let mdat_at = if opts.moov_at_end {
        ftyp.len()
    } else {
        ftyp.len() + moov_len
    };
    let moov = moov_box(
        (mdat_at + header) as u64,
        opts.mdat_len as u32,
        opts.mdat_64bit,
    );

    if opts.moov_at_end {
        [ftyp, mdat, moov].concat()
    } else {
        [ftyp, moov, mdat].concat()
    }
}
