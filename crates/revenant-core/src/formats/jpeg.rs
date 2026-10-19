//! JPEG: walks the marker segments by their declared lengths (so a thumbnail inside APP1 is skipped), then scans the
//! entropy-coded data up to EOI.
//!
//! Unreadable sectors read as zeros. A file containing any is at best `Partial`; if the header walk breaks on one, the
//! search for EOI continues from there (the file is then judged by the decoders like any other).

use super::{FileKind, Format, Health, Measurement, Reader, SECTOR, all_formats};
use zune_core::bytestream::ZCursor;
use zune_core::colorspace::ColorSpace;
use zune_core::options::DecoderOptions;
use zune_jpeg::JpegDecoder;

use crate::source::Source;

/// Longest JPEG the detector will follow (and decode).
pub const JPEG_MAX_LEN: u64 = 64 * 1024 * 1024;

const SOI: u8 = 0xD8;
const EOI: u8 = 0xD9;
const SOS: u8 = 0xDA;
const DQT: u8 = 0xDB;

pub struct Jpeg;

/// Markers allowed before SOS, all carrying a length: SOFn, DHT, DQT, DNL, DRI, APPn, COM.
fn is_segment_marker(m: u8) -> bool {
    matches!(m, 0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF | 0xC4 | DQT | 0xDC | 0xDD | 0xE0..=0xEF | 0xFE)
}

fn is_sof(m: u8) -> bool {
    matches!(m, 0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF)
}

/// Largest image (in pixels) the validation decode will attempt: 1 byte per pixel keeps it near 200 MB.
const MAX_DECODE_PIXELS: u64 = 210_000_000;

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
/// Partial (salvageable); neither -> Damaged. Oversized images are Damaged without decoding. With unreadable sectors
/// (decoded as zeros) a successful decode, strict or lenient, gives at most Partial.
fn decode_health(r: &mut Reader, start: u64, len: u64) -> Health {
    let mut data = vec![0u8; len as usize];
    r.read(start, &mut data);
    let unreadable = r.bad_in(start, start + len);
    match try_decode(&data, true) {
        Some(true) if !unreadable => Health::Intact,
        Some(true) => Health::Partial,
        Some(false) if try_decode(&data, false) == Some(true) => Health::Partial,
        _ => Health::Damaged,
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

    fn measure(&self, source: &dyn Source, start: u64) -> Option<Measurement> {
        let mut r = Reader::new(source);
        let (head, n) = r.head(start);
        if !self.recognizes(&head[..n]) {
            return None;
        }

        // Header segments, up to and including SOS. A walk that breaks on an unreadable sector is not proof that this
        // is not a JPEG: the search for EOI continues from where it broke.
        let (mut pos, mut has_dqt, mut has_sof) = (start + 2, false, false);
        loop {
            let marker = match (r.byte(pos)?, r.byte(pos + 1)?) {
                (0xFF, m) if m == SOS || is_segment_marker(m) => Some(m),
                _ => None,
            };
            let seg_len = u64::from(r.u16(pos + 2)?);
            let Some(marker) = marker.filter(|_| seg_len >= 2) else {
                if r.bad_in(start, pos + 4) {
                    break;
                }
                return None;
            };
            has_dqt |= marker == DQT;
            has_sof |= is_sof(marker);
            pos += 2 + seg_len;
            if pos - start > JPEG_MAX_LEN {
                return None;
            }
            if marker == SOS {
                if !(has_dqt && has_sof) && !r.bad_in(start, pos) {
                    return None;
                }
                break;
            }
        }

        // Entropy-coded data. FF00 and RSTn are data; EOI ends the file.
        let formats = all_formats();
        let partial = |end: u64| Measurement {
            len: end - start,
            kind: FileKind::Jpeg,
            health: Health::Partial,
        };
        loop {
            if pos - start >= JPEG_MAX_LEN {
                return Some(partial(start + JPEG_MAX_LEN));
            }
            let Some(b) = r.byte(pos) else {
                return Some(partial(pos));
            };
            if pos % SECTOR == 0 && r.file_starts_at(&formats, pos) {
                return Some(partial(pos));
            }
            if b == 0xFF && r.byte(pos + 1) == Some(EOI) {
                let len = pos + 2 - start;
                if len > JPEG_MAX_LEN {
                    return Some(partial(start + JPEG_MAX_LEN));
                }
                return Some(Measurement {
                    len,
                    kind: FileKind::Jpeg,
                    health: decode_health(&mut r, start, len),
                });
            }
            pos += 1;
        }
    }
}
