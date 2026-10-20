//! TIFF-based RAW photos (CR2, NEF, ARW, DNG, ORF, RW2) and generic TIFF.
//!
//! The extent comes from [`crate::tiff::walk`] (the furthest byte any IFD, value or strip references). The file ends
//! early where another file starts on a sector boundary inside that extent, or at the end of the source.
//!
//! Health. There is no RAW decoder here, so the raw image data itself can never be verified; the only content check is
//! the largest embedded JPEG preview. `Intact` therefore means: structure complete, no unreadable sector, no size cap
//! hit, and the largest preview decodes strictly. `Partial`: cut, unreadable sector, a size cap hit (`truncated`, e.g. a
//! huge legitimate DNG), no preview, a preview that only decodes leniently or not at all (the raw data may still be fine).
//! `Damaged`: the IFD structure is malformed.

use std::collections::HashSet;

use super::jpeg::{OwnJpegs, preview_health};
use super::{FileKind, Format, HEAD_LEN, Health, Measurement, Reader, SECTOR, all_formats};
use crate::source::Source;
use crate::tiff::{TIFF_MAX_LEN, TiffInfo, type_size, walk};

pub struct TiffRaw;

/// Largest IFD0 offset accepted.
const IFD0_MAX: u32 = 64 << 20;
/// Entries of IFD0 that [`TiffRaw::confirms`] checks.
const CONFIRM_MAX_ENTRIES: u16 = 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Magic {
    Tiff,
    Orf,
    Rw2,
}

/// Byte order, flavor and IFD0 offset from the first 8 bytes.
fn header(head: &[u8]) -> Option<(bool, Magic, u32)> {
    let head = head.get(..8)?;
    let (big, magic) = match &head[..4] {
        [b'I', b'I', 0x2A, 0] => (false, Magic::Tiff),
        [b'M', b'M', 0, 0x2A] => (true, Magic::Tiff),
        b"IIRO" | b"IIRS" => (false, Magic::Orf),
        b"MMOR" => (true, Magic::Orf),
        b"IIU\0" => (false, Magic::Rw2),
        _ => return None,
    };
    let b = [head[4], head[5], head[6], head[7]];
    let ifd0 = if big {
        u32::from_be_bytes(b)
    } else {
        u32::from_le_bytes(b)
    };
    (8..=IFD0_MAX).contains(&ifd0).then_some((big, magic, ifd0))
}

impl Format for TiffRaw {
    fn name(&self) -> &'static str {
        "tiffraw"
    }

    fn recognizes(&self, head: &[u8]) -> bool {
        header(head).is_some()
    }

    /// IFD0 holds at least 3 entries, all of a valid type, and fits in the source.
    fn confirms(&self, source: &dyn Source, start: u64) -> bool {
        let mut r = Reader::new(source);
        let (head, n) = r.head(start);
        let Some((big, _, ifd0)) = header(&head[..n]) else {
            return false;
        };
        let at = start + u64::from(ifd0);
        let mut count = [0u8; 2];
        if r.read(at, &mut count) != 2 {
            return false;
        }
        let count = if big {
            u16::from_be_bytes(count)
        } else {
            u16::from_le_bytes(count)
        };
        if !(3..=CONFIRM_MAX_ENTRIES).contains(&count) {
            return false;
        }
        let mut table = vec![0u8; 12 * usize::from(count)];
        if r.read(at + 2, &mut table) != table.len() {
            return false;
        }
        table.as_chunks::<12>().0.iter().all(|e| {
            let typ = [e[2], e[3]];
            type_size(if big {
                u16::from_be_bytes(typ)
            } else {
                u16::from_le_bytes(typ)
            })
            .is_some()
        })
    }

    fn measure(&self, source: &dyn Source, start: u64) -> Option<Measurement> {
        // Random bytes that merely look like a TIFF header are not a (damaged) file.
        if !self.confirms(source, start) {
            return None;
        }
        let mut r = Reader::new(source);
        let (head, n) = r.head(start);
        let (_, magic, _) = header(&head[..n])?;
        let available = source.len().saturating_sub(start);
        let info = walk(&mut r, start, available.min(TIFF_MAX_LEN))?;
        let kind = kind_of(&head[..n], magic, &info);

        // Structure that continues past the end of the source is a cut file; `walk` clamps a range that starts inside to the
        // limit, so `info.end` covers exactly what is there. Past the 1 GiB cap the source is larger than the limit, so
        // that is a real structural error.
        let source_ended = info.past_limit && available < TIFF_MAX_LEN;
        let damaged = info.structural || (info.past_limit && !source_ended);
        let mut len = info.end;
        let mut cut = source_ended;
        // The starts of the file's own previews and JPEG-coded strips/tiles (which begin with FF D8) are not other files.
        // An uncompressed or vendor-coded strip never begins with a JPEG, so a JPEG at its start is another file.
        let own: HashSet<u64> = info
            .jpegs
            .iter()
            .map(|&(o, _)| o)
            .chain(info.jpeg_data_starts.iter().copied())
            .map(|o| start + o)
            .collect();
        // Nor is a head inside one of its own JPEGs while that JPEG still decodes (ruling P13).
        let mut own_jpegs = OwnJpegs::new(info.jpegs.iter().map(|&(o, l)| (start + o, l)));
        let formats = all_formats();
        let mut pos = (start / SECTOR + 1) * SECTOR;
        while pos < start + len {
            if !own.contains(&pos)
                && r.file_starts_at(&formats, pos)
                && !own_jpegs.vouch_for(&mut r, pos)
            {
                len = pos - start;
                cut = true;
                break;
            }
            pos += SECTOR;
        }

        let health = if damaged {
            Health::Damaged
        } else if cut || info.truncated || r.bad_in(start, start + len) {
            Health::Partial
        } else {
            let previews: Vec<(u64, u64)> =
                info.jpegs.iter().map(|&(o, l)| (start + o, l)).collect();
            preview_health(&mut r, &previews)
        };
        Some(Measurement { len, kind, health })
    }
}

fn kind_of(head: &[u8], magic: Magic, info: &TiffInfo) -> FileKind {
    let make = info.make.as_deref().unwrap_or("");
    match magic {
        Magic::Orf => FileKind::Orf,
        Magic::Rw2 => FileKind::Rw2,
        Magic::Tiff if head.len() >= 10 && &head[8..10] == b"CR" => FileKind::Cr2,
        Magic::Tiff if info.dng => FileKind::Dng,
        Magic::Tiff if make.starts_with("NIKON") => FileKind::Nef,
        Magic::Tiff if make.starts_with("SONY") => FileKind::Arw,
        Magic::Tiff => FileKind::Tiff,
    }
}

const _: () = assert!(HEAD_LEN >= 10);
