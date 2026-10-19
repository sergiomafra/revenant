//! JPEG: walks the marker segments by their declared lengths (so a thumbnail inside APP1 is skipped), then scans the
//! entropy-coded data up to EOI.

use super::{FileKind, Format, Health, Measurement, all_formats};
use crate::source::Source;

/// Longest JPEG the detector will follow (and decode).
pub const JPEG_MAX_LEN: u64 = 64 * 1024 * 1024;

const WINDOW: usize = 1 << 20;
const SECTOR: u64 = 512;

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

/// Byte-at-a-time access over 1 MiB windows, so the file is never read whole while searching for its end.
struct Reader<'a> {
    source: &'a dyn Source,
    buf: Vec<u8>,
    buf_start: u64,
    buf_len: usize,
    failed: bool,
}

impl<'a> Reader<'a> {
    fn new(source: &'a dyn Source) -> Self {
        Self {
            source,
            buf: vec![0; WINDOW],
            buf_start: 0,
            buf_len: 0,
            failed: false,
        }
    }

    /// `None` at the end of the source or on a read error (`failed` tells which).
    fn byte(&mut self, pos: u64) -> Option<u8> {
        if pos < self.buf_start || pos >= self.buf_start + self.buf_len as u64 {
            if pos >= self.source.len() {
                return None;
            }
            match self.source.read_at(pos, &mut self.buf) {
                Ok(n) if n > 0 => {
                    self.buf_start = pos;
                    self.buf_len = n;
                }
                Ok(_) => return None,
                Err(_) => {
                    self.failed = true;
                    return None;
                }
            }
        }
        Some(self.buf[(pos - self.buf_start) as usize])
    }

    fn u16(&mut self, pos: u64) -> Option<u16> {
        Some(u16::from_be_bytes([self.byte(pos)?, self.byte(pos + 1)?]))
    }
}

fn head_at(source: &dyn Source, offset: u64) -> Option<Vec<u8>> {
    let mut head = [0u8; 16];
    let n = source.read_at(offset, &mut head).ok()?;
    Some(head[..n].to_vec())
}

fn decodes(source: &dyn Source, start: u64, len: u64) -> Option<bool> {
    let mut data = vec![0u8; len as usize];
    let mut filled = 0;
    while filled < data.len() {
        let n = source
            .read_at(start + filled as u64, &mut data[filled..])
            .ok()?;
        if n == 0 {
            return None;
        }
        filled += n;
    }
    // A decoder panic on hostile input counts as "does not decode".
    let ok = std::panic::catch_unwind(|| {
        zune_jpeg::JpegDecoder::new(zune_core::bytestream::ZCursor::new(&data[..]))
            .decode()
            .is_ok()
    });
    Some(ok.unwrap_or(false))
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
        if !self.recognizes(&head_at(source, start)?) {
            return None;
        }
        let mut r = Reader::new(source);

        // Header segments, up to and including SOS.
        let (mut pos, mut has_dqt, mut has_sof) = (start + 2, false, false);
        loop {
            if r.byte(pos)? != 0xFF {
                return None;
            }
            let marker = r.byte(pos + 1)?;
            if marker != SOS && !is_segment_marker(marker) {
                return None;
            }
            let seg_len = u64::from(r.u16(pos + 2)?);
            if seg_len < 2 {
                return None;
            }
            has_dqt |= marker == DQT;
            has_sof |= is_sof(marker);
            pos += 2 + seg_len;
            if pos - start > JPEG_MAX_LEN {
                return None;
            }
            if marker == SOS {
                break;
            }
        }
        if !(has_dqt && has_sof) {
            return None;
        }

        // Entropy-coded data. FF00 and RSTn are data; EOI ends the file.
        let others: Vec<_> = all_formats()
            .into_iter()
            .filter(|f| f.name() != self.name())
            .collect();
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
                return if r.failed { None } else { Some(partial(pos)) };
            };
            if pos % SECTOR == 0 {
                let head = head_at(source, pos)?;
                if others.iter().any(|f| f.recognizes(&head)) || self.recognizes(&head) {
                    return Some(partial(pos));
                }
            }
            if b == 0xFF && r.byte(pos + 1) == Some(EOI) {
                let len = pos + 2 - start;
                let health = if decodes(source, start, len)? {
                    Health::Intact
                } else {
                    Health::Damaged
                };
                return Some(Measurement {
                    len,
                    kind: FileKind::Jpeg,
                    health,
                });
            }
            pos += 1;
        }
    }
}
