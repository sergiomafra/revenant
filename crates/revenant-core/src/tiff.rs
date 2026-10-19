//! Shared TIFF/IFD reader for the TIFF-based RAW formats (CR2, NEF, ARW, DNG, ORF, RW2).
//!
//! [`walk`] follows the IFD chain, SubIFDs, Exif, GPS and Interop IFDs of a TIFF whose header sits at some absolute
//! position, collecting the extent, the EXIF fields the app shows, and the ranges of embedded JPEGs and raw data. It
//! parses hostile on-disk data: every offset is checked against a limit, IFDs are visited once, and every loop and
//! allocation driven by an on-disk number is capped.

use std::collections::HashSet;

use crate::formats::Reader;

/// Largest TIFF extent accepted (offsets past it are structural errors).
#[allow(dead_code)] // used by Task 3/5
pub(crate) const TIFF_MAX_LEN: u64 = 1 << 30;
/// IFDs visited per TIFF.
const MAX_IFDS: usize = 64;
/// Entries read per IFD.
const MAX_ENTRIES: u16 = 1024;
/// Strip/tile offsets read per tag.
const MAX_STRIPS: u64 = 4096;
/// SubIFD pointers followed per tag.
const MAX_SUBIFDS: u64 = 16;
/// Bytes read of a string tag.
const MAX_STRING: u64 = 256;

#[allow(dead_code)] // used by Task 3/5
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Endian {
    Little,
    Big,
}

#[allow(dead_code)] // used by Task 3/5
#[derive(Clone, Debug, Default)]
pub(crate) struct TiffInfo {
    /// Furthest byte referenced, relative to the TIFF header.
    pub end: u64,
    /// A structural error was hit (cycle, offset past the limit, bad type or count, too many IFDs or entries).
    pub garbled: bool,
    /// DNGVersion (0xC612) present.
    pub dng: bool,
    pub make: Option<String>,
    pub model: Option<String>,
    pub lens: Option<String>,
    pub serial: Option<String>,
    pub iso: Option<u32>,
    /// Raw EXIF strings, trimmed of NULs and spaces; ASCII only, else `None`.
    pub date_original: Option<String>,
    /// Largest ImageWidth/ImageLength seen.
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// `(offset, len)` relative to the header: JPEGInterchangeFormat ranges and strips/tiles of Compression 6/7 that
    /// start with FF D8.
    pub jpegs: Vec<(u64, u64)>,
    /// Other strip/tile ranges (raw data).
    pub data: Vec<(u64, u64)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// IFD0, its chain and SubIFDs: image data and size tags count.
    Image,
    /// Exif and Interop IFDs: descriptive tags count.
    Exif,
    /// GPS: only the extent counts.
    Gps,
}

fn type_size(typ: u16) -> Option<u64> {
    match typ {
        1 | 2 | 6 | 7 => Some(1),
        3 | 8 => Some(2),
        4 | 9 | 11 | 13 => Some(4),
        5 | 10 | 12 | 16 | 17 | 18 => Some(8),
        _ => None,
    }
}

struct Walker<'a, 'b> {
    r: &'a mut Reader<'b>,
    base: u64,
    limit: u64,
    endian: Endian,
    info: TiffInfo,
}

/// What one IFD says about its image data, resolved once the whole IFD has been read.
#[derive(Default)]
struct ImageTags {
    compression: Option<u32>,
    strip_offsets: Vec<u64>,
    strip_counts: Vec<u64>,
    tile_offsets: Vec<u64>,
    tile_counts: Vec<u64>,
    jpeg_offset: Option<u64>,
    jpeg_len: Option<u64>,
}

/// Walks the TIFF whose header is at absolute `base` (offsets are relative to `base`). `limit` caps every offset
/// (min([`TIFF_MAX_LEN`], what the caller allows, e.g. an APP1 payload length)). `None` if the header is not TIFF.
#[allow(dead_code)] // used by Task 3/5
pub(crate) fn walk(r: &mut Reader, base: u64, limit: u64) -> Option<TiffInfo> {
    let limit = limit.min(TIFF_MAX_LEN);
    let mut head = [0u8; 8];
    if r.read(base, &mut head) < 8 {
        return None;
    }
    let endian = match &head[..2] {
        b"II" => Endian::Little,
        b"MM" => Endian::Big,
        _ => return None,
    };
    let magic_ok = matches!(
        (endian, &head[2..4]),
        (Endian::Little, [0x2A, 0] | b"RO" | b"RS" | b"U\0") | (Endian::Big, [0, 0x2A] | b"OR")
    );
    if !magic_ok {
        return None;
    }
    let mut w = Walker {
        r,
        base,
        limit,
        endian,
        info: TiffInfo {
            end: 8,
            ..TiffInfo::default()
        },
    };
    let first = w.u32_of([head[4], head[5], head[6], head[7]]);
    w.run(u64::from(first));
    Some(w.info)
}

impl Walker<'_, '_> {
    fn u16_of(&self, b: [u8; 2]) -> u16 {
        match self.endian {
            Endian::Little => u16::from_le_bytes(b),
            Endian::Big => u16::from_be_bytes(b),
        }
    }

    fn u32_of(&self, b: [u8; 4]) -> u32 {
        match self.endian {
            Endian::Little => u32::from_le_bytes(b),
            Endian::Big => u32::from_be_bytes(b),
        }
    }

    fn abs(&self, off: u64) -> u64 {
        self.base.saturating_add(off)
    }

    fn read_u16(&mut self, off: u64) -> Option<u16> {
        let mut b = [0u8; 2];
        (self.r.read(self.abs(off), &mut b) == 2).then(|| self.u16_of(b))
    }

    fn read_u32(&mut self, off: u64) -> Option<u32> {
        let mut b = [0u8; 4];
        (self.r.read(self.abs(off), &mut b) == 4).then(|| self.u32_of(b))
    }

    /// Whether `[off, off+len)` lies inside the limit; extends `end` when it does, flags the TIFF when it does not.
    fn claim(&mut self, off: u64, len: u64) -> bool {
        match off.checked_add(len) {
            Some(e) if e <= self.limit => {
                self.info.end = self.info.end.max(e);
                true
            }
            _ => {
                self.info.garbled = true;
                false
            }
        }
    }

    fn run(&mut self, first: u64) {
        let mut visited: HashSet<u64> = HashSet::new();
        let mut pending: Vec<(u64, Kind)> = vec![(first, Kind::Image)];
        while let Some((off, kind)) = pending.pop() {
            if visited.len() >= MAX_IFDS {
                self.info.garbled = true;
                break;
            }
            if !visited.insert(off) {
                self.info.garbled = true; // cycle
                continue;
            }
            self.ifd(off, kind, &mut pending);
        }
    }

    fn ifd(&mut self, off: u64, kind: Kind, pending: &mut Vec<(u64, Kind)>) {
        if off < 8 || off.saturating_add(2) > self.limit {
            self.info.garbled = true;
            return;
        }
        let Some(declared) = self.read_u16(off) else {
            self.info.garbled = true;
            return;
        };
        let n = declared.min(MAX_ENTRIES);
        if declared > MAX_ENTRIES {
            self.info.garbled = true;
        }
        let table = 2 + 12 * u64::from(n);
        if !self.claim(off, table) {
            return;
        }
        let mut tags = ImageTags::default();
        // Pushed in reverse of the order we want to visit them.
        let mut found: Vec<(u64, Kind)> = Vec::new();
        for i in 0..u64::from(n) {
            self.entry(off + 2 + 12 * i, kind, &mut tags, &mut found);
        }
        self.finish_image(&tags);
        if declared <= MAX_ENTRIES
            && self.claim(off + table, 4)
            && let Some(next) = self.read_u32(off + table)
            && next != 0
        {
            found.push((u64::from(next), Kind::Image));
        }
        found.reverse();
        pending.extend(found);
    }

    fn entry(&mut self, at: u64, kind: Kind, tags: &mut ImageTags, found: &mut Vec<(u64, Kind)>) {
        let (Some(tag), Some(typ), Some(count)) = (
            self.read_u16(at),
            self.read_u16(at + 2),
            self.read_u32(at + 4),
        ) else {
            self.info.garbled = true;
            return;
        };
        let Some(size) = type_size(typ) else {
            self.info.garbled = true;
            return;
        };
        let total = size * u64::from(count);
        let value_at = if total <= 4 {
            at + 8
        } else {
            let Some(o) = self.read_u32(at + 8) else {
                self.info.garbled = true;
                return;
            };
            if !self.claim(u64::from(o), total) {
                return;
            }
            u64::from(o)
        };
        if kind == Kind::Gps {
            return;
        }
        let count = u64::from(count);
        match tag {
            0x0100 | 0x0101 if kind == Kind::Image => {
                if let Some(v) = self.uint(typ, value_at) {
                    let slot = if tag == 0x0100 {
                        &mut self.info.width
                    } else {
                        &mut self.info.height
                    };
                    *slot = (*slot).max(Some(v));
                }
            }
            0x0103 if kind == Kind::Image => tags.compression = self.uint(typ, value_at),
            0x0111 if kind == Kind::Image => tags.strip_offsets = self.uints(typ, value_at, count),
            0x0117 if kind == Kind::Image => tags.strip_counts = self.uints(typ, value_at, count),
            0x0144 if kind == Kind::Image => tags.tile_offsets = self.uints(typ, value_at, count),
            0x0145 if kind == Kind::Image => tags.tile_counts = self.uints(typ, value_at, count),
            0x0201 if kind == Kind::Image => {
                tags.jpeg_offset = self.uint(typ, value_at).map(u64::from)
            }
            0x0202 if kind == Kind::Image => {
                tags.jpeg_len = self.uint(typ, value_at).map(u64::from)
            }
            0x014A if kind == Kind::Image && matches!(typ, 4 | 13) => {
                let n = count.min(MAX_SUBIFDS);
                if count > MAX_SUBIFDS {
                    self.info.garbled = true;
                }
                for i in 0..n {
                    if let Some(o) = self.read_u32(value_at + 4 * i) {
                        found.push((u64::from(o), Kind::Image));
                    }
                }
            }
            0x8769 | 0xA005 if matches!(typ, 4 | 13) => {
                if let Some(o) = self.read_u32(value_at) {
                    found.push((u64::from(o), Kind::Exif));
                }
            }
            0x8825 if matches!(typ, 4 | 13) => {
                if let Some(o) = self.read_u32(value_at) {
                    found.push((u64::from(o), Kind::Gps));
                }
            }
            0xC612 => self.info.dng = true,
            0x010F if kind == Kind::Image => self.info.make = self.string(typ, value_at, total),
            0x0110 if kind == Kind::Image => self.info.model = self.string(typ, value_at, total),
            0x9003 if kind == Kind::Exif => {
                self.info.date_original = self.string(typ, value_at, total)
            }
            0xA434 if kind == Kind::Exif => self.info.lens = self.string(typ, value_at, total),
            0xA431 if kind == Kind::Exif => self.info.serial = self.string(typ, value_at, total),
            0x8827 if kind == Kind::Exif => self.info.iso = self.uint(typ, value_at),
            _ => {}
        }
    }

    /// First SHORT/LONG value at `at`.
    fn uint(&mut self, typ: u16, at: u64) -> Option<u32> {
        match typ {
            3 => self.read_u16(at).map(u32::from),
            4 | 13 => self.read_u32(at),
            _ => None,
        }
    }

    /// Up to [`MAX_STRIPS`] SHORT/LONG values; flags `garbled` when truncated or of another type.
    fn uints(&mut self, typ: u16, at: u64, count: u64) -> Vec<u64> {
        let Some(size) = (match typ {
            3 => Some(2),
            4 | 13 => Some(4),
            _ => None,
        }) else {
            self.info.garbled = true;
            return Vec::new();
        };
        let n = count.min(MAX_STRIPS);
        if count > MAX_STRIPS {
            self.info.garbled = true;
        }
        let mut out = Vec::with_capacity(n as usize);
        for i in 0..n {
            match self.uint(typ, at + size * i) {
                Some(v) => out.push(u64::from(v)),
                None => {
                    self.info.garbled = true;
                    break;
                }
            }
        }
        out
    }

    /// An ASCII value: up to [`MAX_STRING`] bytes, cut at the first NUL, trimmed; `None` unless plain ASCII.
    fn string(&mut self, typ: u16, at: u64, total: u64) -> Option<String> {
        if !matches!(typ, 1 | 2 | 7) {
            return None;
        }
        let mut buf = vec![0u8; total.min(MAX_STRING) as usize];
        let n = self.r.read(self.abs(at), &mut buf);
        buf.truncate(n);
        if let Some(z) = buf.iter().position(|&b| b == 0) {
            buf.truncate(z);
        }
        if !buf.iter().all(|&b| b.is_ascii() && !b.is_ascii_control()) {
            return None;
        }
        let s = String::from_utf8(buf).ok()?;
        let s = s.trim();
        (!s.is_empty()).then(|| s.to_string())
    }

    /// Turns the strip/tile/JPEG tags of one IFD into ranges.
    fn finish_image(&mut self, t: &ImageTags) {
        if let (Some(o), Some(l)) = (t.jpeg_offset, t.jpeg_len)
            && l > 0
            && self.claim(o, l)
        {
            self.info.jpegs.push((o, l));
        }
        let jpeg_compression = matches!(t.compression, Some(6 | 7));
        for (offsets, counts) in [
            (&t.strip_offsets, &t.strip_counts),
            (&t.tile_offsets, &t.tile_counts),
        ] {
            for (&o, &l) in offsets.iter().zip(counts.iter()) {
                if l == 0 || !self.claim(o, l) {
                    continue;
                }
                let mut magic = [0u8; 2];
                let is_jpeg = jpeg_compression
                    && self.r.read(self.abs(o), &mut magic) == 2
                    && magic == [0xFF, 0xD8];
                if is_jpeg {
                    self.info.jpegs.push((o, l));
                } else {
                    self.info.data.push((o, l));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EngineError;
    use crate::source::{Source, VolumeId};
    use revenant_testkit::{TestExif, TiffRaw, TiffRawOptions, TiffVariant, tiff_raw};
    use std::time::Instant;

    struct Mem(Vec<u8>);

    impl Source for Mem {
        fn len(&self) -> u64 {
            self.0.len() as u64
        }
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, EngineError> {
            let start = (offset as usize).min(self.0.len());
            let n = buf.len().min(self.0.len() - start);
            buf[..n].copy_from_slice(&self.0[start..start + n]);
            Ok(n)
        }
        fn volume(&self) -> Option<VolumeId> {
            None
        }
    }

    const VARIANTS: [TiffVariant; 7] = [
        TiffVariant::Cr2,
        TiffVariant::Nef,
        TiffVariant::Arw,
        TiffVariant::Dng,
        TiffVariant::Orf,
        TiffVariant::Rw2,
        TiffVariant::Plain,
    ];

    fn exif() -> TestExif {
        TestExif {
            make: Some("Acme".into()),
            model: Some("Model X".into()),
            lens: Some("Lens 50mm f/1.8".into()),
            serial: Some("SN12345".into()),
            iso: Some(400),
            date: Some("2024:05:06 07:08:09".into()),
        }
    }

    fn build(variant: TiffVariant, big_endian: bool, exif: TestExif) -> TiffRaw {
        tiff_raw(&TiffRawOptions {
            variant,
            big_endian,
            raw_len: 3000,
            seed: 7,
            preview: Some((64, 48)),
            preview_sector_aligned: false,
            exif,
        })
    }

    fn run(bytes: &[u8], base: u64, limit: u64) -> Option<TiffInfo> {
        let src = Mem(bytes.to_vec());
        let mut r = Reader::new(&src);
        walk(&mut r, base, limit)
    }

    fn walk_all(bytes: &[u8]) -> Option<TiffInfo> {
        run(bytes, 0, TIFF_MAX_LEN)
    }

    #[test]
    fn walks_every_variant_and_finds_the_extent() {
        for be in [false, true] {
            for v in VARIANTS {
                let f = build(v, be, exif());
                let info = walk_all(&f.bytes).unwrap_or_else(|| panic!("{v:?} be={be}"));
                assert_eq!(info.end, f.bytes.len() as u64, "{v:?} be={be}");
                assert!(!info.garbled, "{v:?} be={be}");
            }
        }
    }

    #[test]
    fn reads_exif_fields() {
        for be in [false, true] {
            let f = build(TiffVariant::Nef, be, exif());
            let i = walk_all(&f.bytes).unwrap();
            assert_eq!(i.make.as_deref(), Some("Acme"));
            assert_eq!(i.model.as_deref(), Some("Model X"));
            assert_eq!(i.lens.as_deref(), Some("Lens 50mm f/1.8"));
            assert_eq!(i.serial.as_deref(), Some("SN12345"));
            assert_eq!(i.iso, Some(400));
            assert_eq!(i.date_original.as_deref(), Some("2024:05:06 07:08:09"));
            assert_eq!((i.width, i.height), (Some(6000), Some(4000)));
        }
    }

    #[test]
    fn default_make_comes_from_the_variant() {
        let f = build(TiffVariant::Cr2, false, TestExif::default());
        assert_eq!(walk_all(&f.bytes).unwrap().make.as_deref(), Some("Canon"));
    }

    #[test]
    fn lists_the_preview_and_the_raw_data() {
        for be in [false, true] {
            for aligned in [false, true] {
                let f = tiff_raw(&TiffRawOptions {
                    variant: TiffVariant::Arw,
                    big_endian: be,
                    raw_len: 3000,
                    seed: 7,
                    preview: Some((64, 48)),
                    preview_sector_aligned: aligned,
                    exif: exif(),
                });
                let p = f.preview.clone().unwrap();
                if aligned {
                    assert_eq!(p.start % 512, 0);
                }
                let i = walk_all(&f.bytes).unwrap();
                assert_eq!(i.jpegs, vec![(p.start as u64, p.len() as u64)]);
                assert_eq!(i.data, vec![(f.raw.start as u64, f.raw.len() as u64)]);
            }
        }
    }

    #[test]
    fn dng_flag() {
        for be in [false, true] {
            for v in VARIANTS {
                let i = walk_all(&build(v, be, exif()).bytes).unwrap();
                assert_eq!(i.dng, v == TiffVariant::Dng, "{v:?}");
            }
        }
    }

    #[test]
    fn works_at_a_base_offset_and_respects_the_limit() {
        let f = build(TiffVariant::Nef, false, exif());
        let mut shifted = vec![0xAA; 1000];
        shifted.extend_from_slice(&f.bytes);
        let i = run(&shifted, 1000, TIFF_MAX_LEN).unwrap();
        assert_eq!(i.end, f.bytes.len() as u64);
        assert!(!i.garbled);
        // A limit that cuts the raw data off marks the walk as garbled and does not extend past it.
        let i = run(&f.bytes, 0, f.raw.start as u64 + 10).unwrap();
        assert!(i.garbled);
        assert!(i.end <= f.raw.start as u64 + 10);
    }

    fn put(b: &mut [u8], at: usize, v: &[u8]) {
        b[at..at + v.len()].copy_from_slice(v);
    }

    #[test]
    fn hostile_ifds_are_bounded() {
        let le = |v: u32| v.to_le_bytes();
        let entry = |tag: u16, typ: u16, count: u32, val: u32| {
            let mut e = Vec::new();
            e.extend_from_slice(&tag.to_le_bytes());
            e.extend_from_slice(&typ.to_le_bytes());
            e.extend_from_slice(&le(count));
            e.extend_from_slice(&le(val));
            e
        };
        // Each hostile thing alone must be flagged (so removing one guard breaks the test).
        let one = |entries: &[Vec<u8>], next: u32| {
            let mut b = vec![0u8; 4096];
            put(&mut b, 0, b"II*\0");
            put(&mut b, 4, &le(8));
            put(&mut b, 8, &(entries.len() as u16).to_le_bytes());
            for (i, e) in entries.iter().enumerate() {
                put(&mut b, 10 + 12 * i, e);
            }
            put(&mut b, 10 + 12 * entries.len(), &le(next));
            b
        };
        let cases = [
            ("self next", vec![], 8),
            (
                "far strip",
                vec![entry(0x0111, 4, 1, 0xFFFF_0000), entry(0x0117, 4, 1, 100)],
                0,
            ),
            ("subifd cycle", vec![entry(0x014A, 4, 1, 8)], 0),
            ("bad type", vec![entry(0x0100, 0, 1 << 31, 0)], 0),
            ("far value", vec![entry(0x010F, 2, 100, 0xFFFF_0000)], 0),
        ];
        for (name, entries, next) in cases {
            let started = Instant::now();
            let i = walk_all(&one(&entries, next)).unwrap();
            assert!(i.garbled, "{name}");
            assert!(started.elapsed().as_secs_f32() < 1.0, "{name}");
        }
        // A clean tiny TIFF is not flagged.
        assert!(!walk_all(&one(&[], 0)).unwrap().garbled);

        // 60_000 entries.
        let mut b = vec![0u8; 4096];
        put(&mut b, 0, b"II*\0");
        put(&mut b, 4, &le(8));
        put(&mut b, 8, &60_000u16.to_le_bytes());
        let i = walk_all(&b).unwrap();
        assert!(i.garbled);

        // Huge strip counts.
        let mut b = vec![0u8; 4096];
        put(&mut b, 0, b"II*\0");
        put(&mut b, 4, &le(8));
        put(&mut b, 8, &2u16.to_le_bytes());
        put(&mut b, 10, &entry(0x0111, 4, 0xFFFF_FFFF, 100));
        put(&mut b, 22, &entry(0x0117, 4, 0xFFFF_FFFF, 100));
        let started = Instant::now();
        let i = walk_all(&b).unwrap();
        assert!(i.garbled);
        assert!(i.data.len() <= 4096);
        assert!(started.elapsed().as_secs_f32() < 1.0);

        // A long chain of distinct IFDs stops at the IFD cap.
        let mut b = vec![0u8; 4096];
        put(&mut b, 0, b"II*\0");
        put(&mut b, 4, &le(8));
        for k in 0..200u32 {
            let at = 8 + k as usize * 6;
            put(&mut b, at, &0u16.to_le_bytes());
            put(&mut b, at + 2, &le(8 + (k + 1) * 6));
        }
        assert!(walk_all(&b).unwrap().garbled);
    }

    #[test]
    fn not_tiff_is_none() {
        let noise = revenant_testkit::DiskImage::noise(4096, 3);
        let src = noise.save();
        let bytes = std::fs::read(src.path()).unwrap();
        assert!(walk_all(&bytes).is_none());
        assert!(walk_all(&[]).is_none());
        // BigTIFF is not supported.
        assert!(walk_all(b"II+\0\x08\0\0\0\0\0\0\0\0\0\0\0").is_none());
    }
}
