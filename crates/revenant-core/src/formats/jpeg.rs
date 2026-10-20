//! JPEG: walks the marker segments by their declared lengths (so a thumbnail inside APP1 is skipped), then scans the
//! entropy-coded data up to EOI.
//!
//! Unreadable sectors read as zeros. A file containing any is at best `Partial`; if the header walk breaks on one, the
//! search for EOI continues from there (the file is then judged by the decoders like any other).

use std::collections::HashMap;

use super::{FileKind, Format, Health, Measurement, Reader, SECTOR, builtin, jpeg_entropy};
use zune_core::bytestream::ZCursor;
use zune_core::colorspace::ColorSpace;
use zune_core::options::DecoderOptions;
use zune_jpeg::JpegDecoder;

use crate::budget::DECODE_BUDGET;
use crate::source::Source;

/// Longest JPEG the detector will follow (and decode).
pub const JPEG_MAX_LEN: u64 = 64 * 1024 * 1024;

const SOI: u8 = 0xD8;
const EOI: u8 = 0xD9;
const SOS: u8 = 0xDA;
const DQT: u8 = 0xDB;

pub struct Jpeg;

/// Markers allowed before SOS, all carrying a length: SOFn, JPG (0xC8), DHT, DAC (0xCC), DQT, DNL, DRI, APPn, COM.
fn is_segment_marker(m: u8) -> bool {
    matches!(m, 0xC0..=0xCF | DQT | 0xDC | 0xDD | 0xE0..=0xEF | 0xFE)
}

/// SOF markers (T.81 Table B.1).
pub(super) fn is_sof(m: u8) -> bool {
    matches!(m, 0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF)
}

/// Result of walking the header segments.
pub(super) struct Header {
    /// Offset just past SOS's segment: where the entropy-coded data starts.
    pub(super) scan_start: u64,
    /// A DAC segment (arithmetic coding conditioning) was seen: the content is beyond our decoder.
    has_dac: bool,
}

/// Longest header [`Jpeg::confirms`] walks: it only has to tell a real header from a byte pattern.
const CONFIRM_MAX_HEADER: u64 = 1 << 20;

/// Walks the header segments of the JPEG at `start`, up to and including SOS. A walk that breaks on an unreadable
/// sector is not proof that this is not a JPEG: the walk then ends where it broke (the search for EOI continues from
/// there). `None` means "not a JPEG": a bad marker or length, no DQT/SOF, or the end of the source.
pub(super) fn walk_header(r: &mut Reader, start: u64, limit: u64) -> Option<Header> {
    let (mut pos, mut has_dqt, mut has_sof, mut has_dac) = (start + 2, false, false, false);
    // Whether the 4 bytes (marker and length) at `pos` are within `limit` of the start.
    let within = |pos: u64| pos - start + 4 <= limit;
    loop {
        // Fill bytes (extra FF) may precede any marker.
        while within(pos) && r.byte(pos)? == 0xFF && r.byte(pos + 1)? == 0xFF {
            pos += 1;
        }
        if !within(pos) {
            return None;
        }
        let marker = match (r.byte(pos)?, r.byte(pos + 1)?) {
            (0xFF, m) if m == SOS || is_segment_marker(m) => Some(m),
            _ => None,
        };
        let seg_len = u64::from(r.u16(pos + 2)?);
        let Some(marker) = marker.filter(|_| seg_len >= 2) else {
            if r.bad_in(start, pos + 4) {
                return Some(Header {
                    scan_start: pos,
                    has_dac,
                });
            }
            return None;
        };
        has_dqt |= marker == DQT;
        has_sof |= is_sof(marker);
        has_dac |= marker == 0xCC;
        pos += 2 + seg_len;
        if pos - start > limit {
            return None;
        }
        if marker == SOS {
            if !(has_dqt && has_sof) && !r.bad_in(start, pos) {
                return None;
            }
            return Some(Header {
                scan_start: pos,
                has_dac,
            });
        }
    }
}

/// Largest image (in pixels) the validation decode will attempt: 1 byte per pixel keeps it near 200 MB.
pub(super) const MAX_DECODE_PIXELS: u64 = 210_000_000;

/// Decodes `data` as grayscale (1 byte per pixel). A decoder panic on hostile input counts as failure.
///
/// Returns `None` when the declared size is above `MAX_DECODE_PIXELS` (not decoded) and `Some(ok)` otherwise.
fn try_decode(data: &[u8], strict: bool) -> Option<bool> {
    std::panic::catch_unwind(|| {
        let options = DecoderOptions::default()
            .jpeg_set_out_colorspace(ColorSpace::Luma)
            .set_strict_mode(strict);
        let mut decoder = JpegDecoder::new_with_options(ZCursor::new(data), options);
        if decoder.decode_headers().is_err() {
            return Some(false);
        }
        let info = decoder.info()?;
        if u64::from(info.width) * u64::from(info.height) > MAX_DECODE_PIXELS {
            return None;
        }
        Some(decoder.decode().is_ok())
    })
    .unwrap_or(Some(false))
}

/// Health of a complete (EOI-terminated) JPEG: strict decode succeeds -> Intact; only the lenient decode succeeds ->
/// Partial (salvageable); neither -> Damaged. Oversized images are not decoded: complete structure -> Partial (not fully verifiable). With unreadable sectors
/// (decoded as zeros) a successful decode, strict or lenient, gives at most Partial.
pub(super) fn decode_health(r: &mut Reader, start: u64, len: u64) -> Health {
    // The bytes plus the 1-byte-per-pixel output; a size the header does not give counts as the largest decoded.
    let pixels = sof_pixels(r, start, len)
        .unwrap_or(MAX_DECODE_PIXELS)
        .min(MAX_DECODE_PIXELS);
    let _budget = DECODE_BUDGET.acquire(len.saturating_add(pixels));
    let mut data = vec![0u8; len as usize];
    r.read(start, &mut data);
    let unreadable = r.bad_in(start, start + len);
    match try_decode(&data, true) {
        Some(true) if !unreadable => Health::Intact,
        Some(true) => Health::Partial,
        Some(false) if try_decode(&data, false) == Some(true) => Health::Partial,
        None => Health::Partial, // too large to decode: structure complete, content not verifiable
        Some(_) => Health::Damaged,
    }
}

/// Health of the JPEG at `start` whose bytes end at `end` without EOI (cut by another file, the end of the source or
/// [`JPEG_MAX_LEN`]): `Partial` if something of the photo would show, that is our entropy decoder completes at least one
/// full row of MCUs from `scan_start` before its first error or the cut; otherwise `Damaged`. A frame the decoder does not
/// cover (progressive, arithmetic, 12-bit, a header broken by an unreadable sector...) stays `Partial`.
fn cut_health(r: &mut Reader, start: u64, scan_start: u64, end: u64) -> Health {
    let Some(frame) = jpeg_entropy::frame(r, start, scan_start) else {
        return Health::Partial;
    };
    let row = u64::from(frame.mcus_x);
    // Stops as soon as the first row is complete: the rest of the file does not change the answer.
    let run = jpeg_entropy::decode_until(r, &frame, frame.start(scan_start), end, false, row);
    if run.mcus >= row {
        Health::Partial
    } else {
        Health::Damaged
    }
}

/// Marker segments looked through for a SOF (health and inspection use the same bound).
pub(crate) const SOF_SEGMENTS: usize = 64;

/// Pixel count (width x height) from the first SOF of the JPEG of `len` bytes at `start`, looking at the first [`SOF_SEGMENTS`]
/// marker segments. `None` if no SOF is found there.
pub(super) fn sof_pixels(r: &mut Reader, start: u64, len: u64) -> Option<u64> {
    sof_dims(r, start, len).map(|(w, h)| u64::from(w) * u64::from(h))
}

/// `(width, height)` from the first SOF of the JPEG of `len` bytes at `start`, looking at the first [`SOF_SEGMENTS`]
/// marker segments; the SOF must lie entirely inside `len`. `None` if no SOF is found there.
pub(crate) fn sof_dims(r: &mut Reader, start: u64, len: u64) -> Option<(u32, u32)> {
    let mut pos = 2u64;
    for _ in 0..SOF_SEGMENTS {
        if pos + 4 > len {
            return None;
        }
        let mut seg = [0u8; 4];
        if r.read(start + pos, &mut seg) != 4 || seg[0] != 0xFF {
            return None;
        }
        match seg[1] {
            m if is_sof(m) => {
                // Marker (2), length (2), precision (1), height (2), width (2).
                if pos + 9 > len {
                    return None;
                }
                let mut dims = [0u8; 4];
                // SOF payload: length (2), precision (1), height (2), width (2).
                if r.read(start + pos + 5, &mut dims) != 4 {
                    return None;
                }
                let h = u32::from(u16::from_be_bytes([dims[0], dims[1]]));
                let w = u32::from(u16::from_be_bytes([dims[2], dims[3]]));
                return Some((w, h));
            }
            SOS | EOI => return None,
            0xFF => pos += 1,
            _ => pos += 2 + u64::from(u16::from_be_bytes([seg[2], seg[3]])).max(2),
        }
    }
    None
}

/// Marker segments looked through for the EXIF APP1.
const EXIF_SEGMENTS: usize = 64;

/// Where the TIFF of the first `Exif\0\0` APP1 of the JPEG of `len` bytes at `start` lies: `(absolute offset of the
/// TIFF header, bytes of TIFF in the segment)`. Stops at SOS, EOI or after 64 segments.
pub(crate) fn exif_tiff(r: &mut Reader, start: u64, len: u64) -> Option<(u64, u64)> {
    let mut pos = 2u64;
    for _ in 0..EXIF_SEGMENTS {
        if pos + 4 > len {
            return None;
        }
        let mut seg = [0u8; 4];
        if r.read(start + pos, &mut seg) != 4 || seg[0] != 0xFF {
            return None;
        }
        let seg_len = u64::from(u16::from_be_bytes([seg[2], seg[3]]));
        match seg[1] {
            0xFF => pos += 1,
            SOS | EOI => return None,
            0xE1 if seg_len >= 8 && pos + 2 + seg_len <= len => {
                let mut id = [0u8; 6];
                if r.read(start + pos + 4, &mut id) == 6 && &id == b"Exif\0\0" {
                    return Some((start + pos + 10, seg_len - 8));
                }
                pos += 2 + seg_len;
            }
            m if is_segment_marker(m) && seg_len >= 2 => pos += 2 + seg_len,
            _ => return None,
        }
    }
    None
}

/// Content check of a structurally complete file whose only verifiable content is its embedded JPEG previews
/// (`(absolute offset, length)` each): the largest (by SOF pixel count) must decode strictly for `Intact`. If the size of
/// any of them cannot be measured (no SOF, zero size, over [`JPEG_MAX_LEN`]), a smaller one must not vouch for the file,
/// so it is `Partial`; so it is with no preview at all, or a largest one that decodes only leniently or not at all (the
/// raw data may still be fine).
pub(super) fn preview_health(r: &mut Reader, previews: &[(u64, u64)]) -> Health {
    let mut best: Option<(u64, u64, u64)> = None; // (pixels, offset, len)
    for &(o, l) in previews {
        let pixels = match sof_pixels(r, o, l) {
            Some(p) if p > 0 && l <= JPEG_MAX_LEN => p,
            _ => return Health::Partial,
        };
        if best.is_none_or(|(p, _, _)| pixels > p) {
            best = Some((pixels, o, l));
        }
    }
    match best {
        Some((_, o, l)) if decode_health(r, o, l) == Health::Intact => Health::Intact,
        _ => Health::Partial,
    }
}

/// Like [`preview_health`], but every JPEG (`(absolute offset, length)` each) must have a measurable size and decode
/// strictly for `Intact`, not just the largest. For a file whose walk cannot see file heads inside one of its JPEGs (a
/// CR3's `PRVW`), where a broken JPEG is the only sign that something was written over it. No JPEG at all is `Partial`.
pub(super) fn every_preview_health(r: &mut Reader, previews: &[(u64, u64)]) -> Health {
    if previews.is_empty() {
        return Health::Partial;
    }
    for &(o, l) in previews {
        if !matches!(sof_pixels(r, o, l), Some(p) if p > 0 && l <= JPEG_MAX_LEN)
            || decode_health(r, o, l) != Health::Intact
        {
            return Health::Partial;
        }
    }
    Health::Intact
}

/// A file's own embedded JPEGs, for deciding whether a file head inside one of them is the file's own data (ruling P13):
/// the EXIF thumbnail in a preview's APP1 is a complete JPEG, and it may start on a sector. Such a head is the file's own
/// while every own JPEG containing it is at most [`JPEG_MAX_LEN`] long and still decodes strictly; a file written over
/// part of an own JPEG breaks that decode, so it is another file. One decode per containing JPEG, cached, and only done
/// when a head is found inside one.
#[derive(Default)]
pub(crate) struct OwnJpegs {
    /// `[start, end)` absolute, sorted.
    jpegs: Vec<(u64, u64)>,
    /// Runs of overlapping ranges merged into disjoint ones: `(start, end, first member, one past the last member)`, the
    /// members being indexes into `jpegs`. Ranges that merely touch stay apart, so a head costs one search plus the
    /// members of its own run, however many tiles a file has.
    merged: Vec<(u64, u64, usize, usize)>,
    decodes: HashMap<(u64, u64), bool>,
}

#[cfg(test)]
thread_local! {
    /// Members of a merged range examined by `vouch_for` on this thread.
    static MEMBERS_EXAMINED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl OwnJpegs {
    /// From `(absolute offset, length)` ranges; empty ranges are ignored.
    pub(crate) fn new(ranges: impl IntoIterator<Item = (u64, u64)>) -> Self {
        let mut jpegs: Vec<(u64, u64)> = ranges
            .into_iter()
            .filter(|&(_, l)| l > 0)
            .map(|(o, l)| (o, o.saturating_add(l)))
            .collect();
        jpegs.sort_unstable();
        let mut merged: Vec<(u64, u64, usize, usize)> = Vec::with_capacity(jpegs.len());
        for (i, &(s, e)) in jpegs.iter().enumerate() {
            match merged.last_mut() {
                Some(last) if s < last.1 => {
                    last.1 = last.1.max(e);
                    last.3 = i + 1;
                }
                _ => merged.push((s, e, i, i + 1)),
            }
        }
        Self {
            jpegs,
            merged,
            decodes: HashMap::new(),
        }
    }

    /// Whether the file head at `pos` lies inside an own JPEG and every own JPEG containing it vouches for it (see the
    /// type).
    pub(crate) fn vouch_for(&mut self, r: &mut Reader, pos: u64) -> bool {
        let i = self.merged.partition_point(|&(s, ..)| s < pos);
        if i == 0 || pos >= self.merged[i - 1].1 {
            return false;
        }
        let (_, _, first, last) = self.merged[i - 1];
        let decodes = &mut self.decodes;
        self.jpegs[first..last]
            .iter()
            .take_while(|&&(s, _)| s < pos)
            .inspect(|_| {
                #[cfg(test)]
                MEMBERS_EXAMINED.with(|c| c.set(c.get() + 1));
            })
            .filter(|&&(_, e)| pos < e)
            .all(|&(s, e)| {
                // Over the JPEG size cap it is not decoded, so it cannot vouch for the head.
                *decodes.entry((s, e)).or_insert_with(|| {
                    e - s <= JPEG_MAX_LEN && decode_health(r, s, e - s) == Health::Intact
                })
            })
    }
}

impl Format for Jpeg {
    fn name(&self) -> &'static str {
        "jpeg"
    }

    fn recognizes(&self, head: &[u8]) -> bool {
        head.len() >= 4
            && head[..3] == [0xFF, SOI, 0xFF]
            && matches!(head[3], 0xE0..=0xEF | DQT | 0xC0..=0xC4 | 0xFE)
    }

    fn confirms(&self, source: &dyn Source, start: u64) -> bool {
        walk_header(&mut Reader::new(source), start, CONFIRM_MAX_HEADER).is_some()
    }

    fn measure(&self, source: &dyn Source, start: u64) -> Option<Measurement> {
        let mut r = Reader::new(source);
        let (head, n) = r.head(start);
        if !self.recognizes(&head[..n]) {
            return None;
        }

        let header = walk_header(&mut r, start, JPEG_MAX_LEN)?;
        let mut pos = header.scan_start;

        // Entropy-coded data. FF00 and RSTn are data; EOI ends the file.
        let formats = builtin();
        let cut = |r: &mut Reader, end: u64| Measurement {
            len: end - start,
            kind: FileKind::Jpeg,
            health: cut_health(r, start, header.scan_start, end),
            fragments: Vec::new(),
        };
        loop {
            if pos - start >= JPEG_MAX_LEN {
                return Some(cut(&mut r, start + JPEG_MAX_LEN));
            }
            let Some(b) = r.byte(pos) else {
                return Some(cut(&mut r, pos));
            };
            if pos.is_multiple_of(SECTOR) && r.file_starts_at(formats, pos) {
                return Some(cut(&mut r, pos));
            }
            if b == 0xFF && r.byte(pos + 1) == Some(EOI) {
                let len = pos + 2 - start;
                if len > JPEG_MAX_LEN {
                    return Some(cut(&mut r, start + JPEG_MAX_LEN));
                }
                return Some(Measurement {
                    len,
                    kind: FileKind::Jpeg,
                    health: match decode_health(&mut r, start, len) {
                        // No decoder here handles arithmetic coding: complete but not verifiable.
                        Health::Damaged if header.has_dac => Health::Partial,
                        health => health,
                    },
                    fragments: Vec::new(),
                });
            }
            // Nothing happens between here and the next `FF`, the next sector start (where another file may begin), the size
            // cap or the end of the source.
            let stop = (pos / SECTOR + 1)
                .saturating_mul(SECTOR)
                .min(start + JPEG_MAX_LEN)
                .min(r.len);
            pos = r.find_ff(pos + 1, stop).unwrap_or(stop);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::tests::Mem;
    use std::time::Instant;

    #[test]
    fn vouch_for_scales_with_touching_ranges() {
        const N: u64 = 65_536;
        let src = Mem(vec![0u8; (N * 512 + 1024) as usize]);
        let mut r = Reader::new(&src);
        let mut own = OwnJpegs::new((0..N).map(|i| (i * 512, 512)));
        let before = MEMBERS_EXAMINED.with(|c| c.get());
        let t = Instant::now();
        for i in 0..N {
            // Strictly inside tile i. Zeros do not decode, so none vouches.
            assert!(!own.vouch_for(&mut r, i * 512 + 100));
        }
        let examined = MEMBERS_EXAMINED.with(|c| c.get()) - before;
        assert!(examined as u64 <= 2 * N, "examined {examined}");
        assert!(t.elapsed().as_secs_f64() < 2.0, "{:?}", t.elapsed());
    }

    #[test]
    fn overlapping_ranges_all_have_to_vouch() {
        let good = revenant_testkit::jpeg(32, 32, 3, 0);
        let l = good.len() as u64;
        let other = revenant_testkit::jpeg(32, 32, 4, 0);
        let lo = other.len() as u64;
        let mut img = vec![0u8; 16384];
        img[..good.len()].copy_from_slice(&good);
        img[4096..4096 + other.len()].copy_from_slice(&other);
        img[8192..8192 + good.len()].copy_from_slice(&good);
        img[8192 + good.len()..8192 + 2 * good.len()].copy_from_slice(&good);
        let src = Mem(img);
        let mut r = Reader::new(&src);

        // A good JPEG [0, l) overlapped by a corrupted range (not a JPEG) starting halfway.
        let mut own = OwnJpegs::new([(0, l), (l / 2, l)]);
        assert!(own.vouch_for(&mut r, 100), "only the good JPEG contains it");
        assert!(
            !own.vouch_for(&mut r, l - 10),
            "the corrupted range contains it too"
        );

        // Two good ranges over the same JPEG (the second runs past its EOI into zeros).
        let mut own = OwnJpegs::new([(4096, lo), (4096, lo + 4)]);
        assert!(
            own.vouch_for(&mut r, 4096 + 10),
            "both contain it and both decode"
        );

        // Two good JPEGs that merely touch: a head exactly on the boundary is inside neither.
        let mut own = OwnJpegs::new([(8192, l), (8192 + l, l)]);
        assert!(!own.vouch_for(&mut r, 8192 + l));
        assert!(own.vouch_for(&mut r, 8192 + l + 10));
        assert!(own.vouch_for(&mut r, 8192 + 10));
    }
}
