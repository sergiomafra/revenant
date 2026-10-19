//! ISO BMFF (MP4/MOV): walks the top-level boxes by their declared sizes to measure the file, then judges its health
//! from the chunk and sample tables inside `moov`.
//!
//! Health rules:
//! - no `mdat` -> not a video (`None`); no `moov` -> `Damaged` (the media data is there but the index is not);
//! - a box (`mdat` or `moov`) that is declared larger than what exists in the source -> `Partial`;
//! - a garbled index (a child walk in `moov/trak/mdia/minf/stbl` ending on an invalid size, or no `trak` with both a chunk
//!   table and `stsz` in a non-fragmented file) -> `Damaged`;
//! - otherwise, for each `trak`, from its `stco`/`co64` and `stsz` tables: any chunk offset at or past the measured end,
//!   or `first chunk offset + total sample size` past the measured end (a lower bound on where the track's data ends)
//!   -> `Partial`; else `Intact`.
//!
//! Chunk offsets are relative to the start of the file (the `ftyp`), not to the source. The walk accepts `ftyp` only as the
//! first box; once `moov` and `mdat` are both seen it continues only through trailing `free skip wide uuid meta udta mfra`
//! (and `moof sidx styp mdat` for fragmented files), never counts a trailing box that overruns the source, and stops at a
//! box-aligned 512-byte sector where a recognized file starts.
//!
//! Inside `mdat` every 512-byte sector start is checked too: where a recognized file (of any format) starts, the video ends.
//! A declared `mdat` size cut this way counts as a box larger than what exists (`Partial`, or `Damaged` without `moov`); a
//! size-0 `mdat` runs to the first such sector, or to the end of the source.
//!
//! Only the first `moov` is parsed, and only if it is at most [`MOOV_MAX_LEN`] (larger -> `Damaged`).
//!
//! Unreadable sectors read as zeros: inside `moov` the index is unusable -> `Damaged` (the length still covers `moov`, so
//! extraction keeps it); anywhere else in the file an otherwise `Intact` file becomes `Partial`. A box header that cannot
//! be read ends the walk there, like a cut (`Partial`, or `Damaged` without `moov`).

use super::{FileKind, Format, HEAD_LEN, Health, Measurement, Reader, SECTOR, all_formats};
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

/// First 512-byte sector start strictly inside `(box_start, box_end)` where any format recognizes a file.
fn next_file_inside(
    r: &mut Reader,
    formats: &[Box<dyn Format>],
    box_start: u64,
    box_end: u64,
) -> Option<u64> {
    (box_start / SECTOR + 1..)
        .map(|i| i * SECTOR)
        .take_while(|&s| s < box_end)
        .find(|&s| r.file_starts_at(formats, s))
}

/// Children `(type, payload)` of a box payload, bounded by the payload itself. Sets `garbled` if the walk ends on an
/// invalid size (or leftover bytes) instead of consuming the payload exactly. Fewer than 8 trailing zero bytes (a
/// QuickTime terminator or padding) are tolerated.
fn children<'a>(mut data: &'a [u8], garbled: &mut bool) -> Vec<([u8; 4], &'a [u8])> {
    let mut out = Vec::new();
    while !data.is_empty() {
        if data.len() < 8 && data.iter().all(|&b| b == 0) {
            break; // QuickTime-style zero terminator or padding
        }
        let (header, size) = match data.get(..8).map(be32) {
            Some(0) => (8, data.len()),
            Some(1) if data.len() >= 16 => {
                (16, usize::try_from(be64(&data[8..])).unwrap_or(usize::MAX))
            }
            Some(size) => (8, size as usize),
            None => (8, 0),
        };
        if size < header || size > data.len() {
            *garbled = true;
            break;
        }
        out.push(([data[4], data[5], data[6], data[7]], &data[header..size]));
        data = &data[size..];
    }
    out
}

fn child<'a>(data: &'a [u8], kind: &[u8; 4], garbled: &mut bool) -> Option<&'a [u8]> {
    children(data, garbled)
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

/// Judges the index in `moov` against the measured file end. A garbled box walk anywhere on the way to the sample
/// tables is `Damaged`; so is a non-fragmented file with no `trak` carrying both a chunk table and `stsz`. Otherwise
/// `Partial` if some track's chunks point at or past `end` or cannot fit before it, else `Intact`.
fn index_health(moov: &[u8], end: u64, fragmented: bool) -> Health {
    let mut garbled = false;
    let (mut has_tables, mut exceeds) = (false, false);
    for (_, trak) in children(moov, &mut garbled)
        .into_iter()
        .filter(|(k, _)| k == b"trak")
    {
        let stbl = child(trak, b"mdia", &mut garbled)
            .and_then(|m| child(m, b"minf", &mut garbled))
            .and_then(|m| child(m, b"stbl", &mut garbled));
        let Some(stbl) = stbl else { continue };
        let range = child(stbl, b"stco", &mut garbled)
            .and_then(|p| chunk_range(p, false))
            .or_else(|| child(stbl, b"co64", &mut garbled).and_then(|p| chunk_range(p, true)));
        let stsz = child(stbl, b"stsz", &mut garbled).and_then(total_sample_size);
        let (Some((first, last)), Some(total)) = (range, stsz) else {
            continue;
        };
        has_tables = true;
        exceeds |= last >= end || first.saturating_add(total) > end;
    }
    if garbled || !(has_tables || fragmented) {
        Health::Damaged
    } else if exceeds {
        Health::Partial
    } else {
        Health::Intact
    }
}

impl Format for IsoBmff {
    fn name(&self) -> &'static str {
        "isobmff"
    }

    fn recognizes(&self, head: &[u8]) -> bool {
        head.len() >= 8 && &head[4..8] == b"ftyp" && (8..=4096).contains(&be32(head))
    }

    fn confirms(&self, source: &dyn Source, start: u64) -> bool {
        let mut r = Reader::new(source);
        let (head, n) = r.head(start);
        if !self.recognizes(&head[..n]) {
            return false;
        }
        let next_at = start + u64::from(be32(&head));
        let (next, n) = r.head(next_at);
        // Like JPEG's `confirms`: a header that falls in an unreadable sector (read as zeros) is not proof that this is
        // not a video, so it confirms.
        parse_header(&next[..n]).is_some() || r.bad_in(next_at, next_at + HEAD_LEN as u64)
    }

    fn measure(&self, source: &dyn Source, start: u64) -> Option<Measurement> {
        let mut r = Reader::new(source);
        let (head, n) = r.head(start);
        if !self.recognizes(&head[..n]) {
            return None;
        }
        let formats = all_formats();
        let walk = walk_boxes(&mut r, &formats, start, true);
        let first = judge(&mut r, start, &walk)?;
        if walk.cut_in_mdat {
            // Motion-JPEG stores one JPEG per frame in `mdat`: those are the video's own, not another file. Walk again
            // without the checks inside `mdat`; keep the cut unless the whole video then checks out as Motion-JPEG.
            let again = walk_boxes(&mut r, &formats, start, false);
            if let Some(full) = judge(&mut r, start, &again)
                && full.health == Health::Intact
                && again
                    .moov
                    .is_some_and(|m| frames_are_the_videos_own(&mut r, start, m, &again.heads))
            {
                return Some(full);
            }
        }
        Some(first)
    }
}

/// Result of walking the top-level boxes.
struct Walk {
    end: u64,
    kind: FileKind,
    has_mdat: bool,
    has_moof: bool,
    truncated: bool,
    unreadable_header: bool,
    /// The walk ended because another file starts inside an `mdat`.
    cut_in_mdat: bool,
    /// Every sector start inside an `mdat` where a file starts (only collected without the checks inside `mdat`).
    heads: Vec<u64>,
    /// Payload offset and length of the first `moov`.
    moov: Option<(u64, u64)>,
}

/// Walks the top-level boxes of the video at `start`. With `inner_checks`, every sector start inside `mdat` where a file
/// starts ends the video there.
fn walk_boxes(r: &mut Reader, formats: &[Box<dyn Format>], start: u64, inner_checks: bool) -> Walk {
    let source_len = r.source.len();
    let mut w = Walk {
        end: start,
        kind: FileKind::Mp4,
        has_mdat: false,
        has_moof: false,
        truncated: false,
        unreadable_header: false,
        cut_in_mdat: false,
        heads: Vec::new(),
        moov: None,
    };
    let mut pos = start;
    while pos < source_len {
        if pos != start && pos.is_multiple_of(SECTOR) && r.file_starts_at(formats, pos) {
            break;
        }
        let (head, n) = r.head(pos);
        let complete = w.has_mdat && w.moov.is_some();
        let Some(h) = parse_header(&head[..n]) else {
            // An unreadable box header: what follows is lost, as if the file were cut here, unless the video was
            // already complete (then it is only what comes after the file).
            w.unreadable_header = r.bad_in(pos, pos + n as u64);
            w.truncated |= w.unreadable_header && !complete;
            break;
        };
        let allowed = match &h.kind {
            b"ftyp" => pos == start,
            _ if !complete => true,
            b"free" | b"skip" | b"wide" | b"uuid" | b"meta" | b"udta" | b"mfra" => true,
            b"moof" | b"sidx" | b"styp" => w.has_moof,
            b"mdat" => w.has_moof,
            _ => false,
        };
        if !allowed {
            break;
        }
        let available = source_len - pos;
        let size = h.size.unwrap_or(available);
        if size > available {
            if complete {
                break;
            }
            w.truncated = true;
        }
        let mut size = size.min(available);
        let mut cut = false;
        if !inner_checks && &h.kind == b"mdat" {
            let end = pos + size;
            let mut from = pos;
            while let Some(next) = next_file_inside(r, formats, from, end) {
                w.heads.push(next);
                from = next;
            }
        }
        if inner_checks
            && &h.kind == b"mdat"
            && let Some(next) = next_file_inside(r, formats, pos, pos + size)
        {
            // Another file was written over the rest of this mdat (or a size-0 mdat ends where the next file
            // begins): the video ends there. A declared size the data no longer reaches is a cut.
            size = next - pos;
            cut = true;
            w.truncated |= h.size.is_some();
        }
        match &h.kind {
            b"ftyp" if size >= 12 => {
                let mut brand = [0u8; 4];
                r.read(pos + h.header_len, &mut brand);
                if &brand == b"qt  " {
                    w.kind = FileKind::Mov;
                }
            }
            b"mdat" => w.has_mdat = true,
            b"moof" => w.has_moof = true,
            b"moov" if w.moov.is_none() => {
                w.moov = Some((pos + h.header_len, size.saturating_sub(h.header_len)))
            }
            _ => {}
        }
        pos += size;
        w.cut_in_mdat |= cut;
        if w.truncated || cut {
            break;
        }
    }
    w.end = pos;
    w
}

/// Judges a walked video: `None` if it is not a video at all.
fn judge(r: &mut Reader, start: u64, w: &Walk) -> Option<Measurement> {
    if !w.has_mdat && !w.unreadable_header {
        return None;
    }
    let (pos, kind) = (w.end, w.kind);
    let len = pos - start;
    let measured = |health| Some(Measurement { len, kind, health });

    let Some((moov_at, moov_len)) = w.moov else {
        return measured(Health::Damaged);
    };
    let mut data = Vec::new();
    if moov_len <= MOOV_MAX_LEN {
        data.resize(moov_len as usize, 0);
        r.read(moov_at, &mut data);
        if r.bad_in(moov_at, moov_at + moov_len) {
            return measured(Health::Damaged);
        }
    }
    if w.truncated {
        return measured(Health::Partial);
    }
    if moov_len > MOOV_MAX_LEN {
        return measured(Health::Damaged);
    }
    match index_health(&data, len, w.has_moof) {
        Health::Intact if r.bad_in(start, pos) => measured(Health::Partial),
        health => measured(health),
    }
}

/// Whether every file head found inside `mdat` is a frame of a Motion-JPEG video track: the track is a video track
/// (`hdlr` `vide`) whose first sample entry (`stsd`) is `mjpa`, `mjpb` or `jpeg`, and each head lies exactly at one of its
/// chunk offsets (`stco`/`co64`, relative to `start`). A photo written over the video lies elsewhere.
fn frames_are_the_videos_own(
    r: &mut Reader,
    start: u64,
    (at, len): (u64, u64),
    heads: &[u64],
) -> bool {
    if len > MOOV_MAX_LEN {
        return false;
    }
    let mut moov = vec![0u8; len as usize];
    r.read(at, &mut moov);
    let mut garbled = false;
    let mut chunks: Vec<u64> = Vec::new();
    for (_, trak) in children(&moov, &mut garbled)
        .into_iter()
        .filter(|(k, _)| k == b"trak")
    {
        let mdia = child(trak, b"mdia", &mut garbled);
        let video = mdia
            .and_then(|m| child(m, b"hdlr", &mut garbled))
            .is_some_and(|p| p.get(8..12) == Some(b"vide"));
        let stbl = mdia
            .and_then(|m| child(m, b"minf", &mut garbled))
            .and_then(|m| child(m, b"stbl", &mut garbled));
        let Some(stbl) = stbl.filter(|_| video) else {
            continue;
        };
        // version/flags (4), entry count (4), then the first entry: size (4), type (4).
        let mjpeg = child(stbl, b"stsd", &mut garbled)
            .and_then(|p| p.get(12..16))
            .is_some_and(|t| t == b"mjpa" || t == b"mjpb" || t == b"jpeg");
        if mjpeg {
            if let Some(p) = child(stbl, b"stco", &mut garbled) {
                chunks.extend(chunk_offsets(p, false));
            } else if let Some(p) = child(stbl, b"co64", &mut garbled) {
                chunks.extend(chunk_offsets(p, true));
            }
        }
    }
    chunks.sort_unstable();
    !chunks.is_empty()
        && heads
            .iter()
            .all(|h| chunks.binary_search(&(h - start)).is_ok())
}

/// All chunk offsets of an `stco` (32-bit) or `co64` (64-bit) payload, bounded by the table's actual size.
fn chunk_offsets(payload: &[u8], wide: bool) -> Vec<u64> {
    let entry = if wide { 8 } else { 4 };
    let (Some(table), Some(count)) = (payload.get(8..), payload.get(4..8)) else {
        return Vec::new();
    };
    let count = (be32(count) as usize).min(table.len() / entry);
    (0..count)
        .map(|i| {
            let e = &table[i * entry..];
            if wide { be64(e) } else { u64::from(be32(e)) }
        })
        .collect()
}
