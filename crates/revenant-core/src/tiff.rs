//! Shared TIFF/IFD reader for the TIFF-based RAW formats (CR2, NEF, ARW, DNG, ORF, RW2).
//!
//! [`walk`] follows the IFD chain, SubIFDs, Exif, GPS and Interop IFDs of a TIFF whose header sits at some absolute
//! position, collecting the extent, the EXIF fields the app shows, and the ranges of embedded JPEGs and raw data. It
//! parses hostile on-disk data: every offset is checked against a limit, IFDs are visited once, a tag counts once per
//! IFD, every value block is read with a single read, and every loop and allocation driven by an on-disk number is
//! capped (per tag and per walk).

use std::collections::HashSet;

use crate::formats::Reader;
use crate::formats::jpeg::SOF_SEGMENTS;

/// Largest TIFF extent accepted (offsets past it are structural errors).
pub(crate) const TIFF_MAX_LEN: u64 = 1 << 30;
/// IFDs visited per TIFF.
const MAX_IFDS: usize = 64;
/// Entries read per IFD.
const MAX_ENTRIES: u16 = 1024;
/// Strip/tile offsets or byte counts read per tag.
const MAX_STRIPS: u64 = 65_536;
/// Strip/tile values read per walk, over all tags and IFDs; also caps `data.len() + jpegs.len()`.
const STRIP_BUDGET: u64 = 262_144;
/// SubIFD pointers followed per tag.
const MAX_SUBIFDS: u64 = 16;
/// Bytes read of a string tag.
const MAX_STRING: u64 = 256;
/// Panasonic raw data is stored in blocks of this many bytes.
const PANASONIC_BLOCK: u64 = 0x4000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Endian {
    Little,
    Big,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct TiffInfo {
    /// Furthest byte referenced, relative to the TIFF header.
    pub end: u64,
    /// A structural error was hit: cycle, offset or value range past the limit, unknown type, truncated value block,
    /// (a tag repeated within one IFD is ignored, not an error: real cameras occasionally do it).
    pub garbled: bool,
    /// Part of `garbled`: some offset or range lay beyond the limit passed to `walk` (a range that starts inside is recorded
    /// clamped to the limit, one that starts beyond it is not counted). When the caller's limit is the end
    /// of the source, this means the structure continues past the end of the data, not that it is malformed.
    pub past_limit: bool,
    /// Part of `garbled`: an error other than `past_limit` (cycle, unknown type, short value block, offset below 8).
    pub structural: bool,
    /// A size cap was hit (too many IFDs, entries, SubIFDs, strips, per-walk strip budget, a string longer than 256
    /// bytes without NUL): the walk is incomplete but not malformed.
    pub truncated: bool,
    /// DNGVersion (0xC612) present.
    pub dng: bool,
    /// First value seen wins for the six fields below.
    pub make: Option<String>,
    pub model: Option<String>,
    pub lens: Option<String>,
    pub serial: Option<String>,
    pub iso: Option<u32>,
    /// Raw EXIF string, trimmed of NULs and spaces; ASCII only, else `None`.
    pub date_original: Option<String>,
    /// Largest ImageWidth/ImageLength seen.
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// `(offset, len)` relative to the header: JPEGInterchangeFormat ranges, strips/tiles of Compression 6/7 that
    /// start with FF D8 and whose first SOF is SOF0, SOF1 or SOF2 (baseline, extended, progressive), and the same kind
    /// of JPEG in vendor places: Panasonic's JpgFromRaw (RW2 tag 0x002E) and the thumbnail and preview of an Olympus
    /// MakerNote. Lossless JPEG (SOF3, as in CR2 raw slices and lossless DNG) lands in `data`, as does a JPEG whose SOF
    /// is not found in the first [`SOF_SEGMENTS`] (64) marker segments.
    pub jpegs: Vec<(u64, u64)>,
    /// Other strip/tile ranges (raw data), and Panasonic's raw data (RW2 tag 0x0118, length from the sensor size and
    /// packing, or an upper bound for an unknown packing: see `finish_panasonic`).
    pub data: Vec<(u64, u64)>,
    /// IFD entries processed (test observation of the entry cap).
    #[cfg(test)]
    pub entries_seen: usize,
    /// IFDs visited (test observation of the IFD cap).
    #[cfg(test)]
    pub ifds_seen: usize,
}

impl TiffInfo {
    fn hard_garbled(&mut self) {
        self.garbled = true;
        self.structural = true;
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// IFD0, its chain and SubIFDs: image data and size tags count.
    Image,
    /// Exif and Interop IFDs: descriptive tags count; their next pointer is not followed.
    Exif,
    /// GPS: only the extent counts; its next pointer is not followed.
    Gps,
}

pub(crate) fn type_size(typ: u16) -> Option<u64> {
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
    /// Panasonic RW2 ("IIU\0" header): IFD0 holds Panasonic's own tags.
    panasonic: bool,
    info: TiffInfo,
    /// Strip/tile values still allowed in this walk.
    budget: u64,
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
    panasonic: PanasonicTags,
}

/// Panasonic RW2 tags of IFD0: sensor size (0x0002/0x0003), bits per sample (0x000A), raw format (0x002D), the
/// JpgFromRaw block (0x002E, `(offset, len)`) and the raw data offset (0x0118).
#[derive(Default)]
struct PanasonicTags {
    width: Option<u32>,
    height: Option<u32>,
    bits: Option<u32>,
    format: Option<u32>,
    jpeg: Option<(u64, u64)>,
    raw_offset: Option<u64>,
}

/// Largest sensor accepted for a Panasonic raw length (far above any real camera).
const PANASONIC_MAX_PIXELS: u64 = 1_000_000_000;

/// Sensor pixel count from the Panasonic size tags; `None` for a missing or zero size or more than
/// [`PANASONIC_MAX_PIXELS`].
fn panasonic_pixels(t: &PanasonicTags) -> Option<u64> {
    let pixels = u64::from(t.width?).checked_mul(u64::from(t.height?))?;
    (pixels > 0 && pixels <= PANASONIC_MAX_PIXELS).then_some(pixels)
}

/// Length of Panasonic packed raw data: 16-byte packets of `n` pixels, the whole rounded up to [`PANASONIC_BLOCK`]. Only
/// packings checked against real files are known (ruling P14): raw format 4 at 12 bits, 14 pixels per packet (DMC-FZ28,
/// DMC-GF1, DMC-LX7 twice, DC-G100), and raw format 5 at 14 bits, 9 pixels per packet (DC-GH5S). `None` (length unknown)
/// for any other packing, a zero size or more than [`PANASONIC_MAX_PIXELS`].
fn panasonic_raw_len(t: &PanasonicTags) -> Option<u64> {
    let per_packet: u64 = match (t.format?, t.bits?) {
        (4, 12) => 14,
        (5, 14) => 9,
        _ => return None,
    };
    panasonic_pixels(t)?
        .div_ceil(per_packet)
        .checked_mul(16)?
        .checked_next_multiple_of(PANASONIC_BLOCK)
}

/// Upper bound on the length of Panasonic raw data of an unknown packing (ruling P16): 2 bytes per pixel (no packing
/// stores more than 16 bits a pixel), rounded up to [`PANASONIC_BLOCK`]. `None` when the sensor size is unusable.
fn panasonic_raw_bound(t: &PanasonicTags) -> Option<u64> {
    panasonic_pixels(t)?
        .checked_mul(2)?
        .checked_next_multiple_of(PANASONIC_BLOCK)
}

/// Walks the TIFF whose header is at absolute `base` (offsets are relative to `base`). `limit` caps every offset
/// (min([`TIFF_MAX_LEN`], what the caller allows, e.g. an APP1 payload length)). `None` if the header is not TIFF.
pub(crate) fn walk(r: &mut Reader, base: u64, limit: u64) -> Option<TiffInfo> {
    walk_from(r, base, limit, Kind::Image)
}

/// Like [`walk`] for a TIFF whose IFD0 is an Exif IFD (Canon CR3's `CMT2` box): ISO, capture date, lens and serial are
/// read from IFD0 itself, and its next pointer is not followed.
pub(crate) fn walk_exif_ifd(r: &mut Reader, base: u64, limit: u64) -> Option<TiffInfo> {
    walk_from(r, base, limit, Kind::Exif)
}

fn walk_from(r: &mut Reader, base: u64, limit: u64, root: Kind) -> Option<TiffInfo> {
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
        panasonic: endian == Endian::Little && &head[2..4] == b"U\0",
        info: TiffInfo {
            end: 8,
            ..TiffInfo::default()
        },
        budget: STRIP_BUDGET,
    };
    let first = w.u32_of([head[4], head[5], head[6], head[7]]);
    w.run(u64::from(first), root);
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
                self.info.past_limit = true;
                if off < self.limit {
                    // Starts inside, ends past the limit: what lies inside still counts.
                    self.info.end = self.info.end.max(self.limit);
                }
                false
            }
        }
    }

    fn run(&mut self, first: u64, root: Kind) {
        let mut visited: HashSet<u64> = HashSet::new();
        let mut pending: Vec<(u64, Kind)> = vec![(first, root)];
        while let Some((off, kind)) = pending.pop() {
            if visited.contains(&off) {
                self.info.hard_garbled(); // cycle
                continue;
            }
            if visited.len() >= MAX_IFDS {
                self.info.truncated = true;
                break;
            }
            visited.insert(off);
            self.ifd(off, kind, &mut pending);
        }
        #[cfg(test)]
        {
            self.info.ifds_seen = visited.len();
        }
    }

    fn ifd(&mut self, off: u64, kind: Kind, pending: &mut Vec<(u64, Kind)>) {
        if off < 8 {
            self.info.hard_garbled();
            return;
        }
        if off.saturating_add(2) > self.limit {
            self.info.garbled = true;
            self.info.past_limit = true;
            return;
        }
        let Some(declared) = self.read_u16(off) else {
            self.info.hard_garbled();
            return;
        };
        let n = declared.min(MAX_ENTRIES);
        if declared > MAX_ENTRIES {
            self.info.truncated = true;
        }
        let table = 2 + 12 * u64::from(n);
        if !self.claim(off, table) {
            return;
        }
        let mut tags = ImageTags::default();
        let mut seen: HashSet<u16> = HashSet::new();
        // Collected in visiting order.
        let mut found: Vec<(u64, Kind)> = Vec::new();
        for i in 0..u64::from(n) {
            self.entry(off + 2 + 12 * i, kind, &mut seen, &mut tags, &mut found);
        }
        self.finish_image(&tags);
        if self.claim(off + table, 4)
            && declared <= MAX_ENTRIES
            && kind == Kind::Image
            && let Some(next) = self.read_u32(off + table)
            && next != 0
        {
            found.push((u64::from(next), Kind::Image));
        }
        // `pending` is a stack: push in reverse so the first found is visited first.
        found.reverse();
        pending.extend(found);
    }

    fn entry(
        &mut self,
        at: u64,
        kind: Kind,
        seen: &mut HashSet<u16>,
        tags: &mut ImageTags,
        found: &mut Vec<(u64, Kind)>,
    ) {
        #[cfg(test)]
        {
            self.info.entries_seen += 1;
        }
        let mut raw = [0u8; 8];
        if self.r.read(self.abs(at), &mut raw) < 8 {
            self.info.hard_garbled();
            return;
        }
        let tag = self.u16_of([raw[0], raw[1]]);
        let typ = self.u16_of([raw[2], raw[3]]);
        let count = self.u32_of([raw[4], raw[5], raw[6], raw[7]]);
        if !seen.insert(tag) {
            return; // only the first occurrence counts
        }
        let Some(size) = type_size(typ) else {
            self.info.hard_garbled();
            return;
        };
        let total = size * u64::from(count);
        let value_at = if total <= 4 {
            at + 8
        } else {
            let Some(o) = self.read_u32(at + 8) else {
                self.info.hard_garbled();
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
        let image = kind == Kind::Image;
        match tag {
            0x0100 | 0x0101 if image => {
                if let Some(v) = self.uint(typ, value_at) {
                    let slot = if tag == 0x0100 {
                        &mut self.info.width
                    } else {
                        &mut self.info.height
                    };
                    *slot = (*slot).max(Some(v));
                }
            }
            0x0103 if image => tags.compression = self.uint(typ, value_at),
            0x0111 if image => tags.strip_offsets = self.uints(typ, value_at, count),
            0x0117 if image => tags.strip_counts = self.uints(typ, value_at, count),
            0x0144 if image => tags.tile_offsets = self.uints(typ, value_at, count),
            0x0145 if image => tags.tile_counts = self.uints(typ, value_at, count),
            0x0201 if image => tags.jpeg_offset = self.uint(typ, value_at).map(u64::from),
            0x0202 if image => tags.jpeg_len = self.uint(typ, value_at).map(u64::from),
            0x0002 if image && self.panasonic => tags.panasonic.width = self.uint(typ, value_at),
            0x0003 if image && self.panasonic => tags.panasonic.height = self.uint(typ, value_at),
            0x000A if image && self.panasonic => tags.panasonic.bits = self.uint(typ, value_at),
            0x002D if image && self.panasonic => tags.panasonic.format = self.uint(typ, value_at),
            0x002E if image && self.panasonic && typ == 7 && total > 4 => {
                tags.panasonic.jpeg = Some((value_at, total));
            }
            0x0118 if image && self.panasonic => {
                tags.panasonic.raw_offset = self.uint(typ, value_at).map(u64::from);
            }
            0x927C if !image && typ == 7 && total > 4 => self.olympus_makernote(value_at, total),
            0x014A if image && matches!(typ, 4 | 13) => {
                let n = count.min(MAX_SUBIFDS);
                if count > MAX_SUBIFDS {
                    self.info.truncated = true;
                }
                let mut buf = vec![0u8; 4 * n as usize];
                let got = self.r.read(self.abs(value_at), &mut buf);
                for &c in buf[..got].as_chunks::<4>().0 {
                    found.push((u64::from(self.u32_of(c)), Kind::Image));
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
            0x010F if image => {
                let v = self.string(typ, value_at, total);
                self.info.make = self.info.make.take().or(v);
            }
            0x0110 if image => {
                let v = self.string(typ, value_at, total);
                self.info.model = self.info.model.take().or(v);
            }
            0x9003 if !image => {
                let v = self.string(typ, value_at, total);
                self.info.date_original = self.info.date_original.take().or(v);
            }
            0xA434 if !image => {
                let v = self.string(typ, value_at, total);
                self.info.lens = self.info.lens.take().or(v);
            }
            0xA431 if !image => {
                let v = self.string(typ, value_at, total);
                self.info.serial = self.info.serial.take().or(v);
            }
            0x8827 if !image => {
                let v = self.uint(typ, value_at);
                self.info.iso = self.info.iso.or(v);
            }
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

    /// SHORT/LONG values read with one read, up to [`MAX_STRIPS`] and what is left of the walk's budget.
    fn uints(&mut self, typ: u16, at: u64, count: u64) -> Vec<u64> {
        let size: usize = match typ {
            3 => 2,
            4 | 13 => 4,
            _ => {
                self.info.hard_garbled();
                return Vec::new();
            }
        };
        let n = count.min(MAX_STRIPS).min(self.budget);
        if n < count {
            self.info.truncated = true;
        }
        self.budget -= n;
        let mut buf = vec![0u8; n as usize * size];
        let got = self.r.read(self.abs(at), &mut buf);
        if got < buf.len() {
            self.info.hard_garbled();
        }
        buf[..got]
            .chunks_exact(size)
            .map(|c| {
                if size == 2 {
                    u64::from(self.u16_of([c[0], c[1]]))
                } else {
                    u64::from(self.u32_of([c[0], c[1], c[2], c[3]]))
                }
            })
            .collect()
    }

    /// An ASCII value: up to [`MAX_STRING`] bytes, cut at the first NUL, trimmed; `None` unless plain ASCII.
    fn string(&mut self, typ: u16, at: u64, total: u64) -> Option<String> {
        if !matches!(typ, 1 | 2 | 7) {
            return None;
        }
        let mut buf = vec![0u8; total.min(MAX_STRING) as usize];
        let n = self.r.read(self.abs(at), &mut buf);
        buf.truncate(n);
        match buf.iter().position(|&b| b == 0) {
            Some(z) => buf.truncate(z),
            None if total > MAX_STRING => self.info.truncated = true,
            None => {}
        }
        if !buf.iter().all(|&b| b.is_ascii() && !b.is_ascii_control()) {
            return None;
        }
        let s = String::from_utf8(buf).ok()?;
        let s = s.trim();
        (!s.is_empty()).then(|| s.to_string())
    }

    /// Whether the JPEG at `o` (`len` bytes) starts with FF D8 and its first SOF is SOF0, SOF1 or SOF2.
    fn is_lossy_jpeg(&mut self, o: u64, len: u64) -> bool {
        let mut magic = [0u8; 2];
        if self.r.read(self.abs(o), &mut magic) != 2 || magic != [0xFF, 0xD8] {
            return false;
        }
        let mut pos = 2u64;
        for _ in 0..SOF_SEGMENTS {
            if pos.saturating_add(4) > len {
                return false;
            }
            let mut seg = [0u8; 4];
            if self.r.read(self.abs(o + pos), &mut seg) != 4 || seg[0] != 0xFF {
                return false;
            }
            match seg[1] {
                0xC0..=0xC2 => return true,
                0xDA | 0xD9 => return false,
                // Other SOFs (lossless, arithmetic, hierarchical) are not previews.
                0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF => return false,
                0x01 | 0xD0..=0xD8 => pos += 2,
                0xFF => pos += 1,
                _ => {
                    let seg_len = u64::from(u16::from_be_bytes([seg[2], seg[3]]));
                    if seg_len < 2 {
                        return false;
                    }
                    pos += 2 + seg_len;
                }
            }
        }
        false
    }

    /// Olympus/OM System MakerNote at `at` (`len` bytes, already claimed): header "OLYMPUS\0" + byte order + version with
    /// the IFD at +12, or "OM SYSTEM\0\0\0" + byte order + version with the IFD at +16; offsets are relative to the
    /// MakerNote's start. Its thumbnail (0x0100, UNDEFINED) and the preview named by its CameraSettings IFD (0x2020 →
    /// PreviewImageStart 0x0101, PreviewImageLength 0x0102) are recorded as JPEGs when they lie inside the MakerNote and
    /// are lossy JPEGs. A MakerNote that does not parse adds nothing: it is vendor data, not part of the TIFF structure.
    fn olympus_makernote(&mut self, at: u64, len: u64) {
        let mut head = [0u8; 16];
        if len < 16 || self.r.read(self.abs(at), &mut head) != 16 {
            return;
        }
        let ifd = if head.starts_with(b"OLYMPUS\0") {
            12
        } else if head.starts_with(b"OM SYSTEM\0\0\0") {
            16
        } else {
            return;
        };
        let endian = match &head[ifd - 4..ifd - 2] {
            b"II" => Endian::Little,
            b"MM" => Endian::Big,
            _ => return,
        };
        let main = self.makernote_ifd(at, len, ifd as u64, endian);
        let value = |entries: &[(u16, u16, u32, u32)], tag: u16| {
            entries.iter().find(|e| e.0 == tag).copied()
        };
        let mut found: Vec<(u64, u64)> = Vec::new();
        if let Some((_, 7, count, off)) = value(&main, 0x0100) {
            found.push((u64::from(off), u64::from(count)));
        }
        if let Some((_, 4 | 7 | 13, _, settings)) = value(&main, 0x2020) {
            let s = self.makernote_ifd(at, len, u64::from(settings), endian);
            if let (Some((_, 4, 1, start)), Some((_, 4, 1, size))) =
                (value(&s, 0x0101), value(&s, 0x0102))
            {
                found.push((u64::from(start), u64::from(size)));
            }
        }
        for (o, l) in found {
            let inside = l > 0 && o.checked_add(l).is_some_and(|e| e <= len);
            if inside && self.room() && self.is_lossy_jpeg(at + o, l) {
                self.info.jpegs.push((at + o, l));
            }
        }
    }

    /// Entries `(tag, type, count, raw value field)` of the MakerNote IFD at `ifd` (relative to the MakerNote at `at`,
    /// `len` bytes), read with one read, at most [`MAX_ENTRIES`]; empty if the table does not fit in the MakerNote.
    fn makernote_ifd(
        &mut self,
        at: u64,
        len: u64,
        ifd: u64,
        endian: Endian,
    ) -> Vec<(u16, u16, u32, u32)> {
        let u16_of = |b: [u8; 2]| match endian {
            Endian::Little => u16::from_le_bytes(b),
            Endian::Big => u16::from_be_bytes(b),
        };
        let u32_of = |b: [u8; 4]| match endian {
            Endian::Little => u32::from_le_bytes(b),
            Endian::Big => u32::from_be_bytes(b),
        };
        let mut count = [0u8; 2];
        if ifd.saturating_add(2) > len || self.r.read(self.abs(at + ifd), &mut count) != 2 {
            return Vec::new();
        }
        let n = u16_of(count).min(MAX_ENTRIES);
        let table = 12 * u64::from(n);
        if ifd + 2 + table > len {
            return Vec::new();
        }
        let mut buf = vec![0u8; table as usize];
        if self.r.read(self.abs(at + ifd + 2), &mut buf) != buf.len() {
            return Vec::new();
        }
        buf.as_chunks::<12>()
            .0
            .iter()
            .map(|e| {
                (
                    u16_of([e[0], e[1]]),
                    u16_of([e[2], e[3]]),
                    u32_of([e[4], e[5], e[6], e[7]]),
                    u32_of([e[8], e[9], e[10], e[11]]),
                )
            })
            .collect()
    }

    /// Panasonic's JpgFromRaw and raw data (RW2 IFD0): the raw data has no length tag, so its length comes from the
    /// sensor size and packing ([`panasonic_raw_len`]). An unknown packing leaves the walk `truncated` (ruling P14) but
    /// still claims the raw data up to [`panasonic_raw_bound`], clamped to the limit without flagging it (the bound is an
    /// estimate, not a declared extent), so a recovered RW2 keeps it (ruling P16); the caller's cut check and the end of
    /// the source trim it. With no usable sensor size nothing is claimed.
    fn finish_panasonic(&mut self, t: &PanasonicTags) {
        if let Some((o, l)) = t.jpeg
            && self.room()
            && self.is_lossy_jpeg(o, l)
        {
            self.info.jpegs.push((o, l));
        }
        let Some(o) = t.raw_offset else {
            return;
        };
        let Some(l) = panasonic_raw_len(t) else {
            self.info.truncated = true;
            if let Some(bound) = panasonic_raw_bound(t)
                && o < self.limit
                && self.room()
            {
                let end = o.saturating_add(bound).min(self.limit);
                self.info.end = self.info.end.max(end);
                self.info.data.push((o, end - o));
            }
            return;
        };
        if self.claim(o, l) {
            if self.room() {
                self.info.data.push((o, l));
            }
        } else if o < self.limit && self.room() {
            self.info.data.push((o, self.limit - o)); // clamped: truncated by the limit
        }
    }

    /// Turns the strip/tile/JPEG tags of one IFD into ranges.
    fn finish_image(&mut self, t: &ImageTags) {
        self.finish_panasonic(&t.panasonic);
        // Panasonic gives the raw data's place in 0x0118; its StripOffsets is a placeholder (0xFFFFFFFF), sometimes with
        // a nonzero StripByteCounts (DMC-FZ28), that points at nothing.
        let no_strips = Vec::new();
        let (strip_offsets, strip_counts) = if t.panasonic.raw_offset.is_some() {
            (&no_strips, &no_strips)
        } else {
            (&t.strip_offsets, &t.strip_counts)
        };
        if let (Some(o), Some(l)) = (t.jpeg_offset, t.jpeg_len)
            && l > 0
        {
            if self.claim(o, l) {
                if self.room() {
                    self.info.jpegs.push((o, l));
                }
            } else if o < self.limit && self.room() {
                self.info.data.push((o, self.limit - o)); // clamped: truncated by the limit
            }
        }
        let jpeg_compression = matches!(t.compression, Some(6 | 7));
        let mut kind: Option<bool> = None;
        for (offsets, counts) in [
            (strip_offsets, strip_counts),
            (&t.tile_offsets, &t.tile_counts),
        ] {
            for (&o, &l) in offsets.iter().zip(counts.iter()) {
                if l == 0 {
                    continue;
                }
                let l = if self.claim(o, l) {
                    l
                } else if o < self.limit {
                    // Clamped to the limit: the part inside is recorded as raw data (it cannot be classified).
                    if !self.room() {
                        return;
                    }
                    self.info.data.push((o, self.limit - o));
                    continue;
                } else {
                    continue;
                };
                if !self.room() {
                    return;
                }
                // One lookup per IFD: all its strips/tiles share one Compression.
                let lossy =
                    *kind.get_or_insert_with(|| jpeg_compression && self.is_lossy_jpeg(o, l));
                if lossy {
                    self.info.jpegs.push((o, l));
                } else {
                    self.info.data.push((o, l));
                }
            }
        }
    }

    /// Whether another range fits under the per-walk cap (flags `truncated` when not).
    fn room(&mut self) -> bool {
        let ok = ((self.info.data.len() + self.info.jpegs.len()) as u64) < STRIP_BUDGET;
        if !ok {
            self.info.truncated = true;
        }
        ok
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

    fn entry(tag: u16, typ: u16, count: u32, val: u32) -> Vec<u8> {
        let mut e = Vec::new();
        e.extend_from_slice(&tag.to_le_bytes());
        e.extend_from_slice(&typ.to_le_bytes());
        e.extend_from_slice(&count.to_le_bytes());
        e.extend_from_slice(&val.to_le_bytes());
        e
    }

    /// Little-endian IFD bytes: entry count, entries as given, next pointer.
    fn ifd_bytes(entries: &[Vec<u8>], next: u32) -> Vec<u8> {
        let mut b = (entries.len() as u16).to_le_bytes().to_vec();
        for e in entries {
            b.extend_from_slice(e);
        }
        b.extend_from_slice(&next.to_le_bytes());
        b
    }

    fn header() -> Vec<u8> {
        let mut b = b"II*\0".to_vec();
        b.extend_from_slice(&8u32.to_le_bytes());
        b
    }

    #[test]
    fn amplification_is_bounded() {
        // 64 chained IFDs of 1024 entries cycling the four strip/tile tags, all pointing at one 16 KiB value area.
        let area = 800_000u32;
        let ifd_len = 2 + 12 * 1024 + 4;
        let mut b = vec![0u8; area as usize + 4096 * 4];
        put(&mut b, 0, &header());
        for k in 0..64usize {
            let tags = [0x0111u16, 0x0117, 0x0144, 0x0145];
            let entries: Vec<_> = (0..1024)
                .map(|i| entry(tags[i % 4], 4, 4096, area))
                .collect();
            let next = if k < 63 {
                (8 + (k + 1) * ifd_len) as u32
            } else {
                0
            };
            put(&mut b, 8 + k * ifd_len, &ifd_bytes(&entries, next));
        }
        for i in 0..4096usize {
            put(&mut b, area as usize + 4 * i, &4u32.to_le_bytes());
        }
        let started = Instant::now();
        let i = walk_all(&b).unwrap();
        assert!(
            started.elapsed().as_secs_f32() < 1.0,
            "{:?}",
            started.elapsed()
        );
        assert!(i.data.len() + i.jpegs.len() <= 262_144);
        // The per-walk value budget is what stops it: 16 IFDs worth of values (2 ranges per offset/count pair).
        assert_eq!(i.data.len(), 131_072);
        assert!(i.truncated);
    }

    /// A file of `len` zero bytes with the header and `parts` (offset, bytes) put in.
    fn file(len: usize, parts: &[(usize, Vec<u8>)]) -> Vec<u8> {
        let mut b = vec![0u8; len];
        put(&mut b, 0, &header());
        for (at, bytes) in parts {
            put(&mut b, *at, bytes);
        }
        b
    }

    fn le(v: u32) -> Vec<u8> {
        v.to_le_bytes().to_vec()
    }

    #[test]
    fn structural_errors_are_garbled() {
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
            ("huge count", vec![entry(0x0111, 4, 0xFFFF_FFFF, 100)], 0),
            ("exif -> ifd0", vec![entry(0x8769, 4, 1, 8)], 0),
        ];
        for (name, entries, next) in cases {
            let started = Instant::now();
            let i = walk_all(&file(4096, &[(8, ifd_bytes(&entries, next))])).unwrap();
            assert!(i.garbled, "{name}");
            assert!(!i.truncated, "{name}");
            assert!(started.elapsed().as_secs_f32() < 1.0, "{name}");
        }
        // Past the limit is told apart from other structural errors (a cut file vs a malformed one).
        let far = ifd_bytes(
            &[entry(0x0111, 4, 1, 0xFFFF_0000), entry(0x0117, 4, 1, 100)],
            0,
        );
        let i = walk_all(&file(4096, &[(8, far)])).unwrap();
        assert!(i.garbled && i.past_limit && !i.structural);
        let cycle = walk_all(&file(4096, &[(8, ifd_bytes(&[], 8))])).unwrap();
        assert!(cycle.garbled && cycle.structural && !cycle.past_limit);
        // The first of two duplicates is the one used, and the repeat is not an error.
        let dup = ifd_bytes(&[entry(0x0100, 4, 1, 5), entry(0x0100, 4, 1, 6)], 0);
        let i = walk_all(&file(4096, &[(8, dup)])).unwrap();
        assert_eq!(i.width, Some(5));
        assert!(!i.garbled && !i.truncated);
        // A clean tiny TIFF is not flagged.
        let i = walk_all(&file(4096, &[(8, ifd_bytes(&[], 0))])).unwrap();
        assert!(!i.garbled && !i.truncated);
    }

    #[test]
    fn pointer_cycles_through_exif_are_garbled_and_gps_is_not_followed() {
        // IFD0 -> Exif at 200 -> Interop pointing back at the Exif IFD.
        let b = file(
            4096,
            &[
                (8, ifd_bytes(&[entry(0x8769, 4, 1, 200)], 0)),
                (200, ifd_bytes(&[entry(0xA005, 4, 1, 200)], 0)),
            ],
        );
        let i = walk_all(&b).unwrap();
        assert!(i.garbled);
        assert_eq!(i.ifds_seen, 2);
        // GPS contents are extent only: a pointer inside is not followed, so no cycle is seen.
        let b = file(
            4096,
            &[
                (8, ifd_bytes(&[entry(0x8825, 4, 1, 200)], 0)),
                (200, ifd_bytes(&[entry(0x8825, 4, 1, 200)], 0)),
            ],
        );
        let i = walk_all(&b).unwrap();
        assert!(!i.garbled);
        assert_eq!(i.ifds_seen, 2);
    }

    #[test]
    fn size_caps_set_truncated_not_garbled() {
        // Entries above MAX_ENTRIES (the buffer holds all of them): only 1024 are read.
        let entries: Vec<_> = (0..1500u16).map(|i| entry(0x2000 + i, 1, 1, 0)).collect();
        let i = walk_all(&file(20_000, &[(8, ifd_bytes(&entries, 0))])).unwrap();
        assert_eq!(i.entries_seen, 1024);
        assert!(i.truncated && !i.garbled);

        // Strip count above MAX_STRIPS that fits in the file.
        let n = 70_000usize;
        let (offs, cnts) = (1000usize, 1000 + 4 * n);
        let ifd = ifd_bytes(
            &[
                entry(0x0111, 4, n as u32, offs as u32),
                entry(0x0117, 4, n as u32, cnts as u32),
            ],
            0,
        );
        let mut b = file(cnts + 4 * n + 100, &[(8, ifd)]);
        for k in 0..n {
            put(&mut b, offs + 4 * k, &le(8));
            put(&mut b, cnts + 4 * k, &le(1));
        }
        let i = walk_all(&b).unwrap();
        assert_eq!(i.data.len(), 65_536);
        assert!(i.truncated && !i.garbled);

        // SubIFD count above its cap, each pointing at a distinct empty IFD.
        let mut parts = vec![(8, ifd_bytes(&[entry(0x014A, 4, 40, 300)], 0))];
        parts.push((300, (0..40u32).flat_map(|k| le(500 + 6 * k)).collect()));
        for k in 0..40usize {
            parts.push((500 + 6 * k, ifd_bytes(&[], 0)));
        }
        let i = walk_all(&file(4096, &parts)).unwrap();
        assert_eq!(i.ifds_seen, 1 + 16);
        assert!(i.truncated && !i.garbled);

        // A chain longer than MAX_IFDS.
        let parts: Vec<_> = (0..100usize)
            .map(|k| (8 + 6 * k, ifd_bytes(&[], (8 + 6 * (k + 1)) as u32)))
            .collect();
        let i = walk_all(&file(4096, &parts)).unwrap();
        assert_eq!(i.ifds_seen, 64);
        assert!(i.truncated && !i.garbled);

        // A string over 256 bytes without NUL is cut and flagged; with a NUL early it is not.
        let long = vec![b'A'; 300];
        let make = ifd_bytes(&[entry(0x010F, 2, 300, 200)], 0);
        let b = file(4096, &[(8, make.clone()), (200, long.clone())]);
        let i = walk_all(&b).unwrap();
        assert_eq!(i.make.as_deref(), Some("A".repeat(256).as_str()));
        assert!(i.truncated && !i.garbled);
        let mut nul = long;
        nul[5] = 0;
        let i = walk_all(&file(4096, &[(8, make), (200, nul)])).unwrap();
        assert_eq!(i.make.as_deref(), Some("AAAAA"));
        assert!(!i.truncated);
    }

    #[test]
    fn end_covers_the_last_referenced_byte() {
        // (a) the last bytes are the next-IFD pointer.
        let i = walk_all(&file(14, &[(8, ifd_bytes(&[], 0))])).unwrap();
        assert_eq!((i.end, i.garbled), (14, false));
        // (b) the last bytes are an out-of-line value.
        let at = 8 + 2 + 12 + 4;
        let ifd = ifd_bytes(&[entry(0x010F, 2, 10, at as u32)], 0);
        let i = walk_all(&file(at + 10, &[(8, ifd), (at, b"ABCDEFGHI\0".to_vec())])).unwrap();
        assert_eq!((i.end, i.garbled), ((at + 10) as u64, false));
        assert_eq!(i.make.as_deref(), Some("ABCDEFGHI"));
        // (c) the last bytes are a JPEG range.
        let at = 8 + 2 + 24 + 4;
        let ifd = ifd_bytes(
            &[entry(0x0201, 4, 1, at as u32), entry(0x0202, 4, 1, 50)],
            0,
        );
        let i = walk_all(&file(at + 50, &[(8, ifd)])).unwrap();
        assert_eq!((i.end, i.garbled), ((at + 50) as u64, false));
        assert_eq!(i.jpegs, vec![(at as u64, 50)]);
    }

    #[test]
    fn first_value_wins_and_exif_next_is_not_an_image_ifd() {
        let word = |b: [u8; 4]| u32::from_le_bytes(b);
        let iso = |v: u32| entry(0x8827, 3, 1, v);
        let ifd0 = ifd_bytes(
            &[
                entry(0x010F, 2, 4, word(*b"AAA\0")),
                entry(0x0110, 2, 4, word(*b"Mod\0")),
                entry(0x8769, 4, 1, 300),
            ],
            100,
        );
        let ifd1 = ifd_bytes(
            &[
                entry(0x010F, 2, 4, word(*b"BBB\0")),
                entry(0x0110, 2, 4, word([0xC3, 0xA9, 0, 0])), // non-ASCII must not erase "Mod"
                entry(0x8769, 4, 1, 400),
            ],
            0,
        );
        let b = file(
            4096,
            &[
                (8, ifd0),
                (100, ifd1),
                (300, ifd_bytes(&[iso(100)], 600)), // its next pointer must not be walked
                (400, ifd_bytes(&[iso(200)], 0)),
                (600, ifd_bytes(&[entry(0x0100, 4, 1, 9999)], 0)),
            ],
        );
        let i = walk_all(&b).unwrap();
        assert_eq!(i.make.as_deref(), Some("AAA"));
        assert_eq!(i.model.as_deref(), Some("Mod"));
        assert_eq!(i.iso, Some(100));
        assert_eq!(i.width, None);
        assert_eq!(i.ifds_seen, 4);
        assert!(!i.garbled);
    }

    /// One IFD with the given compression and a single strip holding `strip`.
    fn strip_tiff(compression: u16, strip: &[u8]) -> TiffInfo {
        let at = 200;
        let ifd = ifd_bytes(
            &[
                entry(0x0103, 3, 1, u32::from(compression)),
                entry(0x0111, 4, 1, at),
                entry(0x0117, 4, 1, strip.len() as u32),
            ],
            0,
        );
        let at = at as usize;
        walk_all(&file(at + strip.len(), &[(8, ifd), (at, strip.to_vec())])).unwrap()
    }

    fn jpeg_with_sof(sof: u8) -> Vec<u8> {
        // SOI, a DHT-like segment, the SOF segment, padding.
        let mut b = vec![0xFF, 0xD8, 0xFF, 0xC4, 0, 4, 0, 0, 0xFF, sof, 0, 11];
        b.extend_from_slice(&[0; 20]);
        b
    }

    #[test]
    fn only_lossy_jpeg_strips_are_previews() {
        for sof in [0xC0, 0xC1, 0xC2] {
            let i = strip_tiff(7, &jpeg_with_sof(sof));
            assert_eq!((i.jpegs.len(), i.data.len()), (1, 0), "SOF {sof:#x}");
        }
        let i = strip_tiff(7, &jpeg_with_sof(0xC3));
        assert_eq!((i.jpegs.len(), i.data.len()), (0, 1), "lossless");
        // Not compressed as JPEG: raw data whatever the bytes are.
        let i = strip_tiff(1, &jpeg_with_sof(0xC0));
        assert_eq!((i.jpegs.len(), i.data.len()), (0, 1));
        // Compression 6 but not a JPEG.
        let i = strip_tiff(6, &[0u8; 40]);
        assert_eq!((i.jpegs.len(), i.data.len()), (0, 1));
    }

    /// One IFD with compression 7, `n` tiles all at `tile` (`j.len()` bytes each) and a JPEG preview range.
    fn tiled(n: usize, j: &[u8]) -> TiffInfo {
        let (offs, cnts, tile, prev) = (300usize, 300 + 4 * n, 300 + 8 * n, 300 + 8 * n + j.len());
        let ifd = ifd_bytes(
            &[
                entry(0x0103, 3, 1, 7),
                entry(0x0144, 4, n as u32, offs as u32),
                entry(0x0145, 4, n as u32, cnts as u32),
                entry(0x0201, 4, 1, prev as u32),
                entry(0x0202, 4, 1, 30),
            ],
            0,
        );
        let mut b = file(prev + 30, &[(8, ifd), (tile, j.to_vec())]);
        for k in 0..n {
            put(&mut b, offs + 4 * k, &le(tile as u32));
            put(&mut b, cnts + 4 * k, &le(j.len() as u32));
        }
        let i = walk_all(&b).unwrap();
        assert_eq!(i.jpegs.first(), Some(&(prev as u64, 30)));
        i
    }

    #[test]
    fn many_lossless_tiles_are_not_truncated() {
        let i = tiled(1000, &jpeg_with_sof(0xC3));
        assert_eq!(i.data.len(), 1000);
        assert_eq!(i.jpegs.len(), 1); // only the real preview
        assert!(!i.truncated && !i.garbled);
    }

    #[test]
    fn a_whole_ifd_is_classified_by_its_first_range() {
        let i = tiled(100, &jpeg_with_sof(0xC0));
        assert_eq!(i.jpegs.len(), 101);
        assert!(i.data.is_empty() && !i.truncated);
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

    fn at(hay: &[u8], needle: &[u8]) -> usize {
        hay.windows(needle.len()).position(|w| w == needle).unwrap()
    }

    /// The `n` bytes after `tag` (a box type) in `bytes`, past the 4 bytes of the tag.
    fn after<'a>(bytes: &'a [u8], tag: &[u8], skip: usize) -> &'a [u8] {
        &bytes[at(bytes, tag) + 4 + skip..]
    }

    fn assert_fields(i: &TiffInfo, with_ifd0: bool) {
        if with_ifd0 {
            assert_eq!(i.make.as_deref(), Some("Acme"));
            assert_eq!(i.model.as_deref(), Some("Model X"));
        }
        assert_eq!(i.lens.as_deref(), Some("Lens 50mm f/1.8"));
        assert_eq!(i.serial.as_deref(), Some("SN12345"));
        assert_eq!(i.iso, Some(400));
        assert_eq!(i.date_original.as_deref(), Some("2024:05:06 07:08:09"));
        assert!(!i.garbled);
    }

    #[test]
    fn testkit_exif_in_a_jpeg_raf_cr3_and_heic_is_readable() {
        // jpeg_with_exif (and the RAF that embeds it): APP1 "Exif\0\0" + TIFF.
        let jpeg = revenant_testkit::jpeg_with_exif(32, 24, 1, &exif());
        assert_eq!(&jpeg[2..4], &[0xFF, 0xE1]);
        assert_eq!(&jpeg[6..12], b"Exif\0\0");
        assert_fields(&walk_all(&jpeg[12..]).unwrap(), true);
        let raf = revenant_testkit::raf(1000, 2, &exif());
        let jpeg_at = u32::from_be_bytes(raf[0x54..0x58].try_into().unwrap()) as usize;
        assert_eq!(&raf[jpeg_at + 6..jpeg_at + 12], b"Exif\0\0");
        assert_fields(&walk_all(&raf[jpeg_at + 12..]).unwrap(), true);

        // CR3: CMT1 holds IFD0 (Make, Model), CMT2 the Exif IFD as its IFD0.
        let cr3 = revenant_testkit::cr3(3, &exif());
        let cmt1 = walk_all(after(&cr3, b"CMT1", 0)).unwrap();
        assert_eq!(cmt1.make.as_deref(), Some("Acme"));
        assert_eq!(cmt1.model.as_deref(), Some("Model X"));
        // CMT2's IFD0 is the Exif IFD itself (as in real CR3): `walk` reads Exif tags only behind an Exif pointer, so
        // check the tags directly: ISO, DateTimeOriginal, BodySerialNumber, LensModel.
        let cmt2 = after(&cr3, b"CMT2", 0);
        assert_eq!(&cmt2[..8], b"II*\0\x08\0\0\0");
        let n = usize::from(u16::from_le_bytes([cmt2[8], cmt2[9]]));
        let tags: Vec<u16> = (0..n)
            .map(|i| u16::from_le_bytes([cmt2[10 + 12 * i], cmt2[11 + 12 * i]]))
            .collect();
        assert_eq!(tags, [0x8827, 0x9003, 0xA431, 0xA434]);

        // HEIC: the Exif item is a 4-byte offset prefix then the TIFF; find it through iloc's second extent.
        let heic = revenant_testkit::heic(500, 4, &exif());
        let iloc = at(&heic, b"iloc") + 4;
        let item2 = iloc + 4 + 2 + 2 + 16; // version/flags, sizes, count, first item
        let off = u32::from_be_bytes(heic[item2 + 8..item2 + 12].try_into().unwrap()) as usize;
        assert_fields(&walk_all(&heic[off + 4..]).unwrap(), true);
    }

    #[test]
    fn exif_ifd_as_ifd0_reads_exif_tags() {
        let cr3 = revenant_testkit::cr3(3, &exif());
        let cmt2 = after(&cr3, b"CMT2", 0);
        let src = Mem(cmt2.to_vec());
        // `walk` ignores Exif tags in IFD0 ...
        let plain = walk(&mut Reader::new(&src), 0, TIFF_MAX_LEN).unwrap();
        assert_eq!((plain.iso, plain.date_original.as_deref()), (None, None));
        // ... `walk_exif_ifd` reads them.
        let info = walk_exif_ifd(&mut Reader::new(&src), 0, TIFF_MAX_LEN).unwrap();
        assert_eq!(info.iso, Some(400));
        assert_eq!(info.date_original.as_deref(), Some("2024:05:06 07:08:09"));
        assert_eq!(info.serial.as_deref(), Some("SN12345"));
        assert_eq!(info.lens.as_deref(), Some("Lens 50mm f/1.8"));
        assert!(!info.garbled);
    }
}
