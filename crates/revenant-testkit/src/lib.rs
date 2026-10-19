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

/// `stsd` with one video sample entry of the given codec type (`avc1`, `mjpa`, ...).
fn stsd_box(codec: &[u8; 4]) -> Vec<u8> {
    let mut payload = vec![0; 4]; // version and flags
    payload.extend_from_slice(&1u32.to_be_bytes()); // entry count
    let mut entry = vec![0; 6]; // reserved
    entry.extend_from_slice(&1u16.to_be_bytes()); // data reference index
    payload.extend(bmff_box(codec, &entry));
    bmff_box(b"stsd", &payload)
}

fn moov_box(chunk_offsets: &[u64], sample_len: u32, wide: bool, codec: &[u8; 4]) -> Vec<u8> {
    let mvhd = bmff_box(b"mvhd", &[0; 100]);
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
    let trak = bmff_box(
        b"trak",
        &bmff_box(b"mdia", &[hdlr, bmff_box(b"minf", &stbl)].concat()),
    );
    bmff_box(b"moov", &[mvhd, trak].concat())
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
    let moov_len = moov_box(&vec![0; chunks.len()], 0, opts.mdat_64bit, codec).len();
    let mdat_at = if opts.moov_at_end {
        ftyp.len()
    } else {
        ftyp.len() + moov_len
    };
    let absolute: Vec<u64> = chunks
        .iter()
        .map(|c| (mdat_at + header) as u64 + c)
        .collect();
    let moov = moov_box(&absolute, payload.len() as u32, opts.mdat_64bit, codec);

    if opts.moov_at_end {
        [ftyp, mdat, moov].concat()
    } else {
        [ftyp, moov, mdat].concat()
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
        let x = &opts.exif;
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
