//! ISO BMFF (MP4/MOV): walks the top-level boxes by their declared sizes to measure the file, then judges its health
//! from the chunk and sample tables inside `moov`.
//!
//! Health rules:
//! - no `mdat` -> not a video (`None`); no `moov` -> `Damaged` (the media data is there but the index is not);
//! - a box (`mdat` or `moov`) that is declared larger than what exists in the source -> `Partial`;
//! - otherwise, for each `trak`, from its `stco`/`co64` and `stsz` tables: any chunk offset at or past the measured end,
//!   or `first chunk offset + total sample size` past the measured end (a lower bound on where the track's data ends)
//!   -> `Partial`; else `Intact`.
//!
//! Only the first `moov` is parsed, and only if it is at most [`MOOV_MAX_LEN`] (larger -> `Damaged`).

use super::{FileKind, Format, Health, Measurement};
use crate::source::Source;

/// Largest `moov` box the detector will load into memory.
pub const MOOV_MAX_LEN: u64 = 16 * 1024 * 1024;

const ACCEPTED: [&[u8; 4]; 14] = [
    b"ftyp", b"moov", b"mdat", b"free", b"skip", b"wide", b"uuid", b"meta", b"pdin", b"moof",
    b"mfra", b"styp", b"sidx", b"udta",
];

pub struct IsoBmff;

struct BoxHeader {
    kind: [u8; 4],
    header_len: u64,
    /// Declared total size; `None` when the box runs to the end of the source (size 0).
    size: Option<u64>,
}

fn read_exact_at(source: &dyn Source, offset: u64, buf: &mut [u8]) -> Option<()> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = source
            .read_at(offset + filled as u64, &mut buf[filled..])
            .ok()?;
        if n == 0 {
            return None;
        }
        filled += n;
    }
    Some(())
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

fn be64(b: &[u8]) -> u64 {
    u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

/// Parses a header from up to 16 bytes. `None` if the box is not valid at top level.
fn parse_header(b: &[u8]) -> Option<BoxHeader> {
    if b.len() < 8 {
        return None;
    }
    let kind = [b[4], b[5], b[6], b[7]];
    if !ACCEPTED.contains(&&kind) {
        return None;
    }
    match be32(b) {
        0 if &kind == b"mdat" => Some(BoxHeader {
            kind,
            header_len: 8,
            size: None,
        }),
        0 => None,
        1 => {
            if b.len() < 16 {
                return None;
            }
            let size = be64(&b[8..]);
            (size >= 16).then_some(BoxHeader {
                kind,
                header_len: 16,
                size: Some(size),
            })
        }
        s if s >= 8 => Some(BoxHeader {
            kind,
            header_len: 8,
            size: Some(u64::from(s)),
        }),
        _ => None,
    }
}

fn read_header(source: &dyn Source, pos: u64) -> Option<BoxHeader> {
    let mut b = [0u8; 16];
    let n = source.read_at(pos, &mut b).ok()?;
    parse_header(&b[..n])
}

/// Children `(type, payload)` of a box payload, bounded by the payload itself. Stops at the first invalid size.
fn children(mut data: &[u8]) -> Vec<([u8; 4], &[u8])> {
    let mut out = Vec::new();
    while data.len() >= 8 {
        let size = be32(data) as usize;
        let (header, size) = match size {
            0 => (8, data.len()),
            1 if data.len() >= 16 => (16, usize::try_from(be64(&data[8..])).unwrap_or(usize::MAX)),
            _ => (8, size),
        };
        if size < header || size > data.len() {
            break;
        }
        out.push(([data[4], data[5], data[6], data[7]], &data[header..size]));
        data = &data[size..];
    }
    out
}

fn child<'a>(data: &'a [u8], kind: &[u8; 4]) -> Option<&'a [u8]> {
    children(data)
        .into_iter()
        .find(|(k, _)| k == kind)
        .map(|(_, p)| p)
}

/// `(first chunk offset, last chunk offset)` from an `stco` (32-bit) or `co64` (64-bit) payload.
fn chunk_range(payload: &[u8], wide: bool) -> Option<(u64, u64)> {
    let entry = if wide { 8 } else { 4 };
    let table = payload.get(8..)?;
    let count = (be32(payload.get(4..8)?) as usize).min(table.len() / entry);
    let at = |i: usize| {
        let e = &table[i * entry..];
        if wide { be64(e) } else { u64::from(be32(e)) }
    };
    (count > 0).then(|| {
        let offsets = (0..count).map(at);
        (
            offsets.clone().min().unwrap_or(0),
            offsets.max().unwrap_or(0),
        )
    })
}

/// Total size in bytes of all samples from an `stsz` payload.
fn total_sample_size(payload: &[u8]) -> Option<u64> {
    let fixed = u64::from(be32(payload.get(4..8)?));
    let count = u64::from(be32(payload.get(8..12)?));
    if fixed != 0 {
        return Some(fixed.saturating_mul(count));
    }
    let table = payload.get(12..)?;
    let n = (count as usize).min(table.len() / 4);
    Some((0..n).map(|i| u64::from(be32(&table[i * 4..]))).sum())
}

/// True when some track's chunk table points at or past `end`, or its data cannot fit before `end`.
fn index_exceeds(moov: &[u8], end: u64) -> bool {
    children(moov)
        .into_iter()
        .filter(|(k, _)| k == b"trak")
        .any(|(_, trak)| {
            let stbl = child(trak, b"mdia")
                .and_then(|m| child(m, b"minf"))
                .and_then(|m| child(m, b"stbl"));
            let Some(stbl) = stbl else { return false };
            let range = child(stbl, b"stco")
                .and_then(|p| chunk_range(p, false))
                .or_else(|| child(stbl, b"co64").and_then(|p| chunk_range(p, true)));
            let Some((first, last)) = range else {
                return false;
            };
            let total = child(stbl, b"stsz")
                .and_then(total_sample_size)
                .unwrap_or(0);
            last >= end || first.saturating_add(total) > end
        })
}

impl Format for IsoBmff {
    fn name(&self) -> &'static str {
        "isobmff"
    }

    fn recognizes(&self, head: &[u8]) -> bool {
        head.len() >= 8 && &head[4..8] == b"ftyp" && (8..=4096).contains(&be32(head))
    }

    fn measure(&self, source: &dyn Source, start: u64) -> Option<Measurement> {
        let mut head = [0u8; 16];
        let n = source.read_at(start, &mut head).ok()?;
        if !self.recognizes(&head[..n]) {
            return None;
        }

        let source_len = source.len();
        let mut pos = start;
        let (mut kind, mut has_mdat, mut truncated) = (FileKind::Mp4, false, false);
        let mut moov: Option<(u64, u64)> = None; // payload offset, payload length
        while pos < source_len {
            let Some(h) = read_header(source, pos) else {
                break;
            };
            let available = source_len - pos;
            let size = h.size.unwrap_or(available);
            if size > available {
                truncated = true;
            }
            let size = size.min(available);
            match &h.kind {
                b"ftyp" if pos == start && size >= 12 => {
                    let mut brand = [0u8; 4];
                    read_exact_at(source, pos + h.header_len, &mut brand)?;
                    if &brand == b"qt  " {
                        kind = FileKind::Mov;
                    }
                }
                b"mdat" => has_mdat = true,
                b"moov" if moov.is_none() => {
                    moov = Some((pos + h.header_len, size.saturating_sub(h.header_len)))
                }
                _ => {}
            }
            pos += size;
            if truncated {
                break;
            }
        }
        if !has_mdat {
            return None;
        }
        let len = pos - start;
        let measured = |health| Some(Measurement { len, kind, health });

        let Some((moov_at, moov_len)) = moov else {
            return measured(Health::Damaged);
        };
        if truncated {
            return measured(Health::Partial);
        }
        if moov_len > MOOV_MAX_LEN {
            return measured(Health::Damaged);
        }
        let mut data = vec![0u8; moov_len as usize];
        read_exact_at(source, moov_at, &mut data)?;
        measured(if index_exceeds(&data, len) {
            Health::Partial
        } else {
            Health::Intact
        })
    }
}
