//! Fujifilm RAF: a fixed header (`FUJIFILMCCD-RAW `) that points at three blocks, an embedded JPEG, the CFA header and
//! the CFA (raw sensor) data, each as a big-endian `u32` offset and length at 0x54, 0x5C and 0x64.
//!
//! The extent is the furthest end of the three blocks. A block that starts past the end of the source is not counted (a
//! corrupted offset never claims the rest of the source), one that starts inside is kept up to the end of the source;
//! either makes the file `Partial`, like a file cut at the first 512-aligned sector inside it where another file starts
//! (except at the starts of its own blocks).
//!
//! Health. There is no RAW decoder here, so the sensor data is never verified; the only content check is the embedded
//! JPEG, judged like a TIFF RAW's previews: `Intact` needs a complete file, no unreadable sector and a strict decode of
//! the JPEG. No JPEG, or one that decodes leniently or not at all, is `Partial`.

use std::collections::HashSet;

use super::jpeg::preview_health;
use super::{FileKind, Format, Health, Measurement, Reader, SECTOR, all_formats};
use crate::source::Source;
use crate::tiff::TIFF_MAX_LEN;

pub struct Raf;

const MAGIC: &[u8; 16] = b"FUJIFILMCCD-RAW ";
/// Where the three `(offset, length)` pairs start, and the end of the last one (the header's minimum size).
const PAIRS_AT: u64 = 0x54;
const PAIRS_END: u64 = 0x6C;

/// The JPEG, CFA header and CFA `(offset, length)` pairs, if the header holds them and each lies within
/// [`TIFF_MAX_LEN`].
fn pairs(r: &mut Reader, start: u64) -> Option<[(u64, u64); 3]> {
    let mut raw = [0u8; (PAIRS_END - PAIRS_AT) as usize];
    if r.read(start + PAIRS_AT, &mut raw) != raw.len() {
        return None;
    }
    let mut out = [(0, 0); 3];
    for (pair, c) in out.iter_mut().zip(raw.as_chunks::<8>().0) {
        let off = u64::from(u32::from_be_bytes([c[0], c[1], c[2], c[3]]));
        let len = u64::from(u32::from_be_bytes([c[4], c[5], c[6], c[7]]));
        if off + len > TIFF_MAX_LEN {
            return None;
        }
        *pair = (off, len);
    }
    Some(out)
}

impl Format for Raf {
    fn name(&self) -> &'static str {
        "raf"
    }

    fn recognizes(&self, head: &[u8]) -> bool {
        head.get(..16) == Some(MAGIC)
    }

    fn confirms(&self, source: &dyn Source, start: u64) -> bool {
        let mut r = Reader::new(source);
        let (head, n) = r.head(start);
        self.recognizes(&head[..n]) && pairs(&mut r, start).is_some()
    }

    fn measure(&self, source: &dyn Source, start: u64) -> Option<Measurement> {
        let mut r = Reader::new(source);
        let (head, n) = r.head(start);
        if !self.recognizes(&head[..n]) {
            return None;
        }
        let pairs = pairs(&mut r, start)?;
        let available = source.len().saturating_sub(start);

        let (mut len, mut cut) = (PAIRS_END.min(available), available < PAIRS_END);
        let mut own = HashSet::new();
        for &(off, l) in pairs.iter().filter(|p| p.1 > 0) {
            if off >= available {
                cut = true; // starts past the end of the source: not counted
                continue;
            }
            own.insert(start + off);
            cut |= off + l > available;
            len = len.max((off + l).min(available));
        }

        let formats = all_formats();
        let mut pos = (start / SECTOR + 1) * SECTOR;
        while pos < start + len {
            if !own.contains(&pos) && r.file_starts_at(&formats, pos) {
                len = pos - start;
                cut = true;
                break;
            }
            pos += SECTOR;
        }

        let (jpeg_at, jpeg_len) = pairs[0];
        let health = if cut || r.bad_in(start, start + len) || jpeg_len == 0 {
            Health::Partial
        } else {
            preview_health(&mut r, &[(start + jpeg_at, jpeg_len)])
        };
        Some(Measurement {
            len,
            kind: FileKind::Raf,
            health,
        })
    }
}
