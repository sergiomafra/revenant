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
