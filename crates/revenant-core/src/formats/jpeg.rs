//! JPEG: walks the marker segments by their declared lengths (so a thumbnail inside APP1 is skipped), then scans the
//! entropy-coded data up to EOI.
//!
//! Unreadable sectors read as zeros. A file containing any is at best `Partial`; if the header walk breaks on one, the
//! search for EOI continues from there (the file is then judged by the decoders like any other).

use std::collections::HashMap;

use super::{
    FileKind, Format, Health, Measurement, Reader, SECTOR, builtin, cancelled, jpeg_entropy,
    jpeg_join,
};
use zune_core::bytestream::ZCursor;
use zune_core::colorspace::ColorSpace;
use zune_core::options::DecoderOptions;
use zune_jpeg::JpegDecoder;

use crate::budget::DECODE_BUDGET;
use crate::source::{Fragmented, Source};

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
pub(crate) struct Header {
    /// Offset just past SOS's segment: where the entropy-coded data starts.
    pub(crate) scan_start: u64,
    /// A DAC segment (arithmetic coding conditioning) was seen: the content is beyond our decoder.
    has_dac: bool,
}

/// Longest header [`Jpeg::confirms`] walks: it only has to tell a real header from a byte pattern.
const CONFIRM_MAX_HEADER: u64 = 1 << 20;

/// Walks the header segments of the JPEG at `start`, up to and including SOS. A walk that breaks on an unreadable
/// sector is not proof that this is not a JPEG: the walk then ends where it broke (the search for EOI continues from
/// there). `None` means "not a JPEG": a bad marker or length, no DQT/SOF, or the end of the source.
pub(crate) fn walk_header(r: &mut Reader, start: u64, limit: u64) -> Option<Header> {
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

#[cfg(test)]
thread_local! {
    /// Strict `zune-jpeg` decodes attempted on this thread.
    pub(super) static ZUNE_STRICT_DECODES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Lenient `zune-jpeg` decodes attempted on this thread.
    pub(super) static ZUNE_LENIENT_DECODES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Health of a complete JPEG (the `len` bytes at `start`, holding its EOI), the one rule every caller uses (a plain
/// JPEG, a RAW/RAF/CR3 preview, a reassembled photo; ruling Q6):
///
/// - A frame Revenant's entropy decoder covers ([`jpeg_entropy::frame`]: sequential Huffman, 8-bit, one interleaved
///   scan): `Intact` iff that decoder walks every MCU (the exact count, restart markers in sequence) with EOI right after
///   the last one. `zune-jpeg`'s strict mode is not used for these: it zero-fills past an early EOI and does not count
///   MCUs. If the walk fails before completing one row of MCUs, `Damaged` (ruling Q9: nothing of the photo would show);
///   if it fails later, `Partial` (ruling Q10: at least one row shows). Both as for a cut file; `zune-jpeg` is not run
///   for these frames at all.
/// - Any other frame (progressive, arithmetic, 12-bit, a header that does not parse): strict `zune-jpeg` decode ->
///   `Intact`; only the lenient one -> `Partial`; neither -> `Damaged`; too large to decode -> `Partial`.
///
/// With unreadable sectors (read as zeros) inside, a success of either kind gives at most `Partial`.
pub(crate) fn decode_health(r: &mut Reader, start: u64, len: u64) -> Health {
    verify(r, start, len).0
}

/// How our entropy decoder's walk of a covered frame ended.
enum Walk {
    /// Every MCU, then EOI; the end of the JPEG's data (just past `FF D9`).
    ToEoi(u64),
    /// It failed before completing one row of MCUs.
    UnderOneRow,
    /// It failed after completing at least one row.
    Failed,
}

/// [`decode_health`], with where the JPEG's data ends when our decoder walked it to its EOI (just past `FF D9`, which
/// may lie before `start + len` for a range declared longer than the JPEG); `None` when that is not known.
fn verify(r: &mut Reader, start: u64, len: u64) -> (Health, Option<u64>) {
    let end = start.saturating_add(len);
    let walked = walk_header(r, start, len).and_then(|header| {
        let frame = jpeg_entropy::frame(r, start, header.scan_start)?;
        let run = jpeg_entropy::decode(r, &frame, frame.start(header.scan_start), end, false);
        Some(match run.end {
            jpeg_entropy::End::Eoi { at } => Walk::ToEoi(at + 2),
            // Q9: less than one full row of MCUs before the failure: nothing of the photo would show (as for a cut
            // file), and the lenient decode of a frame declared huge would be the costliest step.
            _ if run.mcus < u64::from(frame.mcus_x) => Walk::UnderOneRow,
            _ => Walk::Failed,
        })
    });
    match walked {
        Some(Walk::ToEoi(eoi)) if r.bad_in(start, end) => (Health::Partial, Some(eoi)),
        Some(Walk::ToEoi(eoi)) => (Health::Intact, Some(eoi)),
        Some(Walk::UnderOneRow) => (Health::Damaged, None),
        // Q10: a row or more shows, whatever comes after (as for a cut file).
        Some(Walk::Failed) => (Health::Partial, None),
        None => (zune_health(r, start, len, true), None),
    }
}

/// Health from `zune-jpeg`: with `strict`, a strict decode gives `Intact`; a lenient-only decode (or an image too large
/// to decode) gives `Partial`; neither, `Damaged`. Unreadable sectors inside cap a success at `Partial`. A cancelled scan
/// (looked at before waiting for the decode budget and again once it is granted) decodes nothing: `Partial`, which the
/// scan drops anyway.
pub(super) fn zune_health(r: &mut Reader, start: u64, len: u64, strict: bool) -> Health {
    if cancelled() {
        return Health::Partial;
    }
    // The bytes plus the 1-byte-per-pixel output; a size the header does not give counts as the largest decoded.
    let pixels = sof_pixels(r, start, len)
        .unwrap_or(MAX_DECODE_PIXELS)
        .min(MAX_DECODE_PIXELS);
    let _budget = DECODE_BUDGET.acquire(len.saturating_add(pixels));
    if cancelled() {
        return Health::Partial;
    }
    let mut data = vec![0u8; len as usize];
    r.read(start, &mut data);
    let unreadable = r.bad_in(start, start + len);
    #[cfg(test)]
    if strict {
        ZUNE_STRICT_DECODES.with(|c| c.set(c.get() + 1));
    }
    if strict && try_decode(&data, true) == Some(true) {
        return if unreadable {
            Health::Partial
        } else {
            Health::Intact
        };
    }
    #[cfg(test)]
    ZUNE_LENIENT_DECODES.with(|c| c.set(c.get() + 1));
    match try_decode(&data, false) {
        Some(true) | None => Health::Partial, // None: too large to decode, content not verifiable
        Some(false) => Health::Damaged,
    }
}

/// Health of the JPEG at `start` whose bytes end at `end` without EOI (cut by another file, the end of the source or
/// [`JPEG_MAX_LEN`]): `Partial` if something of the photo would show, that is our entropy decoder completes at least one
/// full row of MCUs from `scan_start` before its first error, the cut, or the [`data_end`] of the bytes it used (zeros of
/// erased space decode as valid blocks, but they are not the photo); otherwise `Damaged`. A frame the decoder does not
/// cover (progressive, arithmetic, 12-bit, a header broken by an unreadable sector...) stays `Partial`.
fn cut_health(r: &mut Reader, start: u64, scan_start: u64, end: u64) -> Health {
    let Some(frame) = jpeg_entropy::frame(r, start, scan_start) else {
        return Health::Partial;
    };
    let row = u64::from(frame.mcus_x);
    // Stops as soon as the first row is complete: the rest of the file does not change the answer.
    let run = jpeg_entropy::decode_until(r, &frame, frame.start(scan_start), end, false, row);
    if run.mcus < row {
        return Health::Damaged;
    }
    // Only the bytes that row used are checked (the file is not read again).
    let used = run.pos.saturating_add(2).min(end);
    let data_end = data_end(r, scan_start, used);
    if data_end < used {
        let run =
            jpeg_entropy::decode_until(r, &frame, frame.start(scan_start), data_end, false, row);
        if run.mcus < row {
            return Health::Damaged;
        }
    }
    Health::Partial
}

/// Where entropy-coded data found in `[from, end)` stops being a photo's: the start of the first 512-byte sector
/// starting in that range that is made of one repeated byte value (erased or TRIMmed space, zero-filled slack; camera
/// entropy-coded data very rarely repeats one value over a whole sector, though a large flat area under optimized Huffman
/// tables could, and the photo would then be cut there), or of the first unreadable sector overlapping it
/// (clamped to `from`); `end` if there is neither. Reads each sector of the range once (a sector starting before `end` is
/// read whole). The cut-file health rule and the gap search share it.
pub(crate) fn data_end(r: &mut Reader, from: u64, end: u64) -> u64 {
    let mut sector = [0u8; SECTOR as usize];
    let mut s = from - from % SECTOR;
    while s < end {
        let n = r.read(s, &mut sector);
        // Reading marks the unreadable sectors (they read as zeros).
        if r.bad_in(s, s + SECTOR) {
            return s.max(from);
        }
        // One repeated value: each byte equals the next (one memcmp, vectorized).
        if s >= from && n == sector.len() && sector[1..] == sector[..sector.len() - 1] {
            return s;
        }
        if n < sector.len() {
            break; // end of the source
        }
        s += SECTOR;
    }
    end
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
pub(crate) fn preview_health(r: &mut Reader, previews: &[(u64, u64)]) -> Health {
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
    /// Per own JPEG: `None` if it does not vouch, else the end of the data it vouches for (its EOI when known).
    decodes: HashMap<(u64, u64), Option<u64>>,
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
                // Over the JPEG size cap it is not decoded, so it cannot vouch for the head; nor for one past its EOI,
                // in the rest of a range declared longer than the JPEG.
                let vouched = *decodes.entry((s, e)).or_insert_with(|| {
                    if e - s > JPEG_MAX_LEN {
                        return None;
                    }
                    match verify(r, s, e - s) {
                        (Health::Intact, eoi) => Some(eoi.unwrap_or(e)),
                        _ => None,
                    }
                });
                vouched.is_some_and(|end| pos < end)
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
        let contiguous = contiguous(&mut r, start, &header);
        if contiguous.health == Health::Intact {
            return Some(contiguous);
        }
        // The photo may continue elsewhere: put it back together from two pieces if it does.
        let end = start + contiguous.len;
        let Some(joined) = jpeg_join::join(&mut r, start, header.scan_start, end) else {
            return Some(contiguous);
        };
        let view = Fragmented::new(source, joined.fragments.to_vec());
        let health = match decode_health(&mut Reader::new(&view), 0, joined.len) {
            Health::Damaged => return Some(contiguous),
            // Q1: `Intact` only if exactly one continuation in the whole window passed.
            Health::Intact if joined.unique => Health::Intact,
            _ => Health::Partial,
        };
        Some(Measurement {
            len: joined.len,
            kind: FileKind::Jpeg,
            health,
            fragments: joined.fragments.to_vec(),
        })
    }
}

/// The JPEG at `start` as one contiguous run of bytes: up to its `EOI`, or cut by another file's head, the end of the
/// source or [`JPEG_MAX_LEN`].
pub(super) fn contiguous(r: &mut Reader, start: u64, header: &Header) -> Measurement {
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
            return cut(r, start + JPEG_MAX_LEN);
        }
        let Some(b) = r.byte(pos) else {
            return cut(r, pos);
        };
        if pos.is_multiple_of(SECTOR) && r.file_starts_at(formats, pos) {
            return cut(r, pos);
        }
        if b == 0xFF && r.byte(pos + 1) == Some(EOI) {
            let len = pos + 2 - start;
            if len > JPEG_MAX_LEN {
                return cut(r, start + JPEG_MAX_LEN);
            }
            return Measurement {
                len,
                kind: FileKind::Jpeg,
                health: match decode_health(r, start, len) {
                    // No decoder here handles arithmetic coding: complete but not verifiable.
                    Health::Damaged if header.has_dac => Health::Partial,
                    health => health,
                },
                fragments: Vec::new(),
            };
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::tests::Mem;

    #[test]
    fn a_cancelled_scan_does_not_decode() {
        use crate::formats::CancelScope;
        use crate::scan::CancelToken;
        let j = revenant_testkit::jpeg(64, 64, 1, 0);
        let decodes = || {
            ZUNE_STRICT_DECODES.with(std::cell::Cell::get)
                + ZUNE_LENIENT_DECODES.with(std::cell::Cell::get)
        };
        // Cancelled before: no wait for the budget, no decode.
        let token = CancelToken::default();
        token.cancel();
        let before = decodes();
        {
            let _scope = CancelScope::enter(&token);
            let h = zune_health(&mut Reader::new(&Mem(j.clone())), 0, j.len() as u64, true);
            assert_ne!(h, Health::Intact);
        }
        assert_eq!(decodes(), before);
        // Cancelled while waiting for the budget (all of it held here): no decode once it gets it.
        let token = CancelToken::default();
        let held = DECODE_BUDGET.acquire(u64::MAX);
        let waiter = {
            let token = token.clone();
            std::thread::spawn(move || {
                let _scope = CancelScope::enter(&token);
                let h = zune_health(&mut Reader::new(&Mem(j.clone())), 0, j.len() as u64, true);
                (h, decodes())
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(50));
        token.cancel();
        drop(held);
        let (h, n) = waiter.join().unwrap();
        assert_ne!(h, Health::Intact);
        assert_eq!(n, 0, "decoded after the scan was cancelled");
    }

    fn noise(len: usize) -> Vec<u8> {
        let mut x = 7u64;
        (0..len)
            .map(|_| {
                x = x
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (x >> 56) as u8
            })
            .collect()
    }

    /// `a = jpeg_smooth(1024, 768, 1)` and its half `c`, a good photo and the two early-EOI forgeries zune-jpeg's strict
    /// mode accepts: half the photo then zeros then EOI, and half the photo then another photo's tail.
    fn early_eoi_cases() -> (Vec<u8>, Vec<(&'static str, Vec<u8>)>) {
        let a = revenant_testkit::jpeg_smooth(1024, 768, 1);
        let b = revenant_testkit::jpeg_smooth(1024, 768, 2);
        let c = (a.len() / 2).next_multiple_of(512);
        let mut zeros = a[..c].to_vec();
        zeros.extend_from_slice(&[0; 510]);
        zeros.extend_from_slice(&[0xFF, 0xD9]);
        let mut other = a[..c].to_vec();
        other.extend_from_slice(&b[c..]);
        (a, vec![("zeros, EOI", zeros), ("other tail", other)])
    }

    fn strict_decodes() -> usize {
        ZUNE_STRICT_DECODES.with(std::cell::Cell::get)
    }

    #[test]
    fn early_eoi_is_never_intact_through_any_caller() {
        let (good, forged) = early_eoi_cases();
        for (name, bytes) in std::iter::once(("good", good)).chain(forged) {
            // Both zune-jpeg modes accept every one of them.
            assert_eq!(try_decode(&bytes, true), Some(true), "{name}");
            let len = bytes.len() as u64;
            let src = Mem(bytes);
            let mut r = Reader::new(&src);
            let intact = name == "good";
            let before = strict_decodes();
            let health = decode_health(&mut r, 0, len);
            assert_eq!(health == Health::Intact, intact, "{name}: {health:?}");
            assert_ne!(
                health,
                Health::Damaged,
                "{name}: the lenient decode still works"
            );
            let expected = if intact {
                Health::Intact
            } else {
                Health::Partial
            };
            assert_eq!(preview_health(&mut r, &[(0, len)]), expected, "{name}");
            assert_eq!(
                every_preview_health(&mut r, &[(0, len)]),
                expected,
                "{name}"
            );
            assert_eq!(
                OwnJpegs::new([(0, len)]).vouch_for(&mut r, 4096),
                intact,
                "{name}"
            );
            // A frame our decoder covers never goes through zune-jpeg's strict mode.
            assert_eq!(strict_decodes(), before, "{name}");
        }
    }

    #[test]
    fn a_walk_under_one_row_is_damaged_without_the_lenient_decode() {
        // A flat 16384×12800 frame (rows of 2048 MCUs, 1536 bytes) with 1 KiB of its data, then zeros and an EOI: the
        // lenient decode would fill 210 megapixels from that; our walk fails before the first row ends (Q9).
        let flat = revenant_testkit::jpeg_flat_with_restarts(16384, 12800, 0);
        let scan_start = {
            let src = Mem(flat.clone());
            walk_header(&mut Reader::new(&src), 0, JPEG_MAX_LEN)
                .unwrap()
                .scan_start as usize
        };
        let lenient = || ZUNE_LENIENT_DECODES.with(std::cell::Cell::get);
        let mut short = flat[..scan_start + 1024].to_vec();
        short.extend_from_slice(&[0; 510]);
        short.extend_from_slice(&[0xFF, 0xD9]);
        let src = Mem(short.clone());
        let before = lenient();
        assert_eq!(
            decode_health(&mut Reader::new(&src), 0, short.len() as u64),
            Health::Damaged
        );
        assert_eq!(lenient(), before, "no lenient decode");
        // Past the first row it is Partial, still without the lenient decode (Q10).
        let (_, forged) = early_eoi_cases();
        let (_, zeros) = &forged[0];
        let src = Mem(zeros.clone());
        let before = lenient();
        assert_eq!(
            decode_health(&mut Reader::new(&src), 0, zeros.len() as u64),
            Health::Partial
        );
        assert_eq!(lenient(), before);
    }

    #[test]
    fn a_crafted_huge_frame_is_partial_without_the_lenient_decode() {
        // P1 of the Q8 probes: a flat 16384×12800 frame (210 megapixels) with 32 KiB of its data (about 21 rows), then
        // 64 MiB of sectors `[00; 510] FF D9`. The lenient decode would fill 210 megapixels (about 1.1 s in release).
        let flat = revenant_testkit::jpeg_flat_with_restarts(16384, 12800, 0);
        let scan_start = {
            let src = Mem(flat.clone());
            walk_header(&mut Reader::new(&src), 0, JPEG_MAX_LEN)
                .unwrap()
                .scan_start as usize
        };
        let c = (scan_start + 32 * 1024).next_multiple_of(512);
        let mut img = flat[..c].to_vec();
        let mut sector = [0u8; 512];
        sector[510..].copy_from_slice(&[0xFF, 0xD9]);
        while img.len() < c + (64 << 20) {
            img.extend_from_slice(&sector);
        }
        let src = Mem(img);
        let (lenient, strict) = (
            || ZUNE_LENIENT_DECODES.with(std::cell::Cell::get),
            strict_decodes,
        );
        let before = (lenient(), strict());
        let m = Jpeg.measure(&src, 0).expect("a JPEG");
        assert_eq!(m.health, Health::Partial, "{m:?}");
        assert_eq!((lenient(), strict()), before, "no zune-jpeg decode");
        // Its time is pinned by `tests/jpeg_timing.rs`, a test binary of its own (nothing else runs beside it).
    }

    #[test]
    fn frames_our_decoder_does_not_cover_keep_the_strict_decode() {
        let p = revenant_testkit::jpeg_progressive_flat(64, 48);
        let src = Mem(p.clone());
        let mut r = Reader::new(&src);
        let scan_start = walk_header(&mut r, 0, JPEG_MAX_LEN).unwrap().scan_start;
        assert!(jpeg_entropy::frame(&mut r, 0, scan_start).is_none());
        let before = strict_decodes();
        assert_eq!(decode_health(&mut r, 0, p.len() as u64), Health::Intact);
        assert_eq!(strict_decodes(), before + 1);
        // Broken in the middle of its AC scan: the strict decode fails, the lenient one still shows it.
        let mut broken = p.clone();
        let mid = p.len() - 20;
        broken[mid..mid + 4].copy_from_slice(&[0xFF, 0x01, 0xFF, 0x01]);
        let src = Mem(broken.clone());
        let health = decode_health(&mut Reader::new(&src), 0, broken.len() as u64);
        assert_ne!(health, Health::Intact);
    }

    #[test]
    fn a_frame_without_its_quantization_table_is_not_ours() {
        // DQT table 0 relabelled 1: the component's table 0 is undefined, so the pixels cannot be computed.
        let mut j = revenant_testkit::jpeg_flat_with_restarts(64, 48, 0);
        assert_eq!(j[2..5], [0xFF, 0xDB, 0x00]);
        j[6] = 0x01;
        let src = Mem(j.clone());
        let mut r = Reader::new(&src);
        let scan_start = walk_header(&mut r, 0, JPEG_MAX_LEN).unwrap().scan_start;
        assert!(jpeg_entropy::frame(&mut r, 0, scan_start).is_none());
        assert_ne!(decode_health(&mut r, 0, j.len() as u64), Health::Intact);
    }

    #[test]
    fn a_head_between_a_previews_eoi_and_its_declared_end_is_not_vouched_for() {
        // A preview whose declared range runs 4 KiB past its EOI, and another file's head in that slack: the head is not
        // the preview's data, so it must not be hidden.
        let j = revenant_testkit::jpeg_smooth(256, 192, 3);
        let foreign_at = (j.len() + 2).next_multiple_of(512);
        let declared = foreign_at + 4096;
        let mut img = vec![0u8; declared + 4096];
        img[..j.len()].copy_from_slice(&j);
        let other = revenant_testkit::jpeg(32, 32, 4, 0);
        img[foreign_at..foreign_at + other.len()].copy_from_slice(&other);
        let src = Mem(img);
        let mut r = Reader::new(&src);
        let mut own = OwnJpegs::new([(0, declared as u64)]);
        assert!(own.vouch_for(&mut r, 512), "inside the preview's data");
        assert!(!own.vouch_for(&mut r, foreign_at as u64), "past its EOI");
    }

    #[test]
    fn data_end_stops_at_uniform_or_unreadable_sectors() {
        let mut data = noise(8192);
        data[2048..2560].fill(0x00);
        data[4096..4608].fill(0x55);
        data[6000..6600].fill(0x00); // no whole sector of one value
        let src = Mem(data.clone());
        let mut r = Reader::new(&src);
        assert_eq!(data_end(&mut r, 100, 8192), 2048);
        // A uniform sector starting before `from` is not counted; one starting before `end` is read whole.
        assert_eq!(data_end(&mut r, 2100, 8192), 4096);
        assert_eq!(data_end(&mut r, 100, 2048), 2048);
        assert_eq!(data_end(&mut r, 100, 2049), 2048);
        assert_eq!(data_end(&mut r, 4700, 8192), 8192);
        // An unreadable sector overlapping the range ends it, clamped to `from` (it reads as zeros, but it holds data
        // from before `from` too).
        let src = crate::formats::tests::BadSector {
            inner: Mem(noise(8192)),
            bad: 1536,
        };
        let mut r = Reader::new(&src);
        assert_eq!(data_end(&mut r, 1600, 8192), 1600);
        assert_eq!(data_end(&mut r, 100, 8192), 1536);
        assert_eq!(data_end(&mut r, 2048, 8192), 8192);
    }

    #[test]
    fn vouch_for_scales_with_touching_ranges() {
        const N: u64 = 65_536;
        let src = Mem(vec![0u8; (N * 512 + 1024) as usize]);
        let mut r = Reader::new(&src);
        let mut own = OwnJpegs::new((0..N).map(|i| (i * 512, 512)));
        let before = MEMBERS_EXAMINED.with(|c| c.get());
        for i in 0..N {
            // Strictly inside tile i. Zeros do not decode, so none vouches.
            assert!(!own.vouch_for(&mut r, i * 512 + 100));
        }
        // Ruling R2: the deterministic count is the bound (no wall-clock check, which flakes under load in debug).
        let examined = MEMBERS_EXAMINED.with(|c| c.get()) - before;
        assert!(examined as u64 <= 2 * N, "examined {examined}");
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
