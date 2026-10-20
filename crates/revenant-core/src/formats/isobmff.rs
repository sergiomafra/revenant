//! ISO BMFF (MP4/MOV, Canon CR3, HEIC): walks the top-level boxes by their declared sizes to measure the file, then
//! judges its health from the chunk and sample tables inside `moov`.
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
//! Kind from the `ftyp` brands: major brand `crx ` is a CR3, `qt  ` a MOV; a HEIC brand (`heic heix heim heis mif1
//! msf1`, as the major or a compatible brand, but not with an `avif`/`avis` major brand) with no `moov` in the file is
//! a HEIC (an MP4 that merely lists `mif1` has a `moov` and stays an MP4); anything else is an MP4.
//!
//! CR3 follows the rules above (it has `trak`s), and in addition both its JPEGs must decode strictly for `Intact`: the
//! `PRVW` preview (in the top-level `uuid` `eaf42b5e-1c98-4b88-b9fb-b7dc406e4d16`) and the full-size JPEG (first chunk
//! of the first track, when it is a JPEG), each of a measurable size; else the file is at best `Partial` (a missing
//! `PRVW`, or one whose size does not fit its box, too). Not "the largest decides" as for a TIFF RAW: the CR3 walk checks
//! file heads only inside `mdat`, never inside the `PRVW` box, so a broken `PRVW` is the only sign that a file was
//! written over it, and `Partial` keeps the scan looking for heads inside it (only `Intact` suppresses that).
//! The `PRVW` layout (after the 16-byte uuid and 8 bytes: the box with 4+2 reserved bytes,
//! width, height, a 1, the JPEG size, then the JPEG) is from public format notes and the testkit; not yet checked on
//! real files.
//!
//! HEIC has no `moov` or `mdat` rule: it needs a `meta` (at most [`MOOV_MAX_LEN`]) whose `iloc` lists the items'
//! extents. Every extent (construction method 0 and data reference 0: offsets absolute in the file, plus the item's
//! base offset) must end inside the measured file, else `Partial`; no `meta` (with an `mdat`), a garbled `meta`, no
//! `iloc` or an `iloc` without extents is `Damaged`. Extents the engine cannot check (construction method 1 or 2, an
//! external data reference, a zero length, zero-size offset and length fields, an `iloc` version above 2, more than
//! 65,536 items or 262,144 extents) make the file at best `Partial`. The pixel data is HEVC (or similar) and is never
//! decoded: `Intact` means the structure is complete and every extent lies inside the file. `mdat` may be absent (data
//! in `idat`), but then the extents are method 1: `Partial`.
//!
//! An `mdat` head that is exactly a chunk start (CR3: of any track; Motion-JPEG video: of its track) or an extent start
//! (HEIC) is the file's own data, not another file: see `measure`. A foreign file starting exactly there is
//! indistinguishable from it. So is a CR3 head inside its own full-size JPEG (in `mdat`) while that JPEG still decodes
//! (ruling P13); the `PRVW` has no such exemption, as no head is checked inside its box at all (see the CR3 health rule).
//!
//! Unreadable sectors read as zeros: inside `moov` the index is unusable -> `Damaged` (the length still covers `moov`, so
//! extraction keeps it); anywhere else in the file an otherwise `Intact` file becomes `Partial`. A box header that cannot
//! be read ends the walk there, like a cut (`Partial`, or `Damaged` without `moov`).

use super::jpeg::{OwnJpegs, every_preview_health};
use super::{FileKind, Format, Health, Measurement, Reader, SECTOR, all_formats};
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

pub(crate) fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

pub(crate) fn be64(b: &[u8]) -> u64 {
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

/// First 512-byte sector start strictly inside `(box_start, box_end)` where any format recognizes a file, except the
/// sorted absolute positions in `own` (data the file itself keeps there) and heads inside an own JPEG that still decodes
/// (`jpegs`, ruling P13).
fn next_file_inside(
    r: &mut Reader,
    formats: &[Box<dyn Format>],
    box_start: u64,
    box_end: u64,
    own: &[u64],
    jpegs: &mut OwnJpegs,
) -> Option<u64> {
    (box_start / SECTOR + 1..)
        .map(|i| i * SECTOR)
        .take_while(|&s| s < box_end)
        .find(|&s| {
            own.binary_search(&s).is_err() && r.file_starts_at(formats, s) && !jpegs.vouch_for(r, s)
        })
}

/// Children `(type, payload)` of a box payload, bounded by the payload itself. Sets `garbled` if the walk ends on an
/// invalid size (or leftover bytes) instead of consuming the payload exactly. Fewer than 8 trailing zero bytes (a
/// QuickTime terminator or padding) are tolerated.
pub(crate) fn children<'a>(mut data: &'a [u8], garbled: &mut bool) -> Vec<([u8; 4], &'a [u8])> {
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

pub(crate) fn child<'a>(data: &'a [u8], kind: &[u8; 4], garbled: &mut bool) -> Option<&'a [u8]> {
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

    /// Whether [`measure`](Format::measure) would accept the file, by a header-only walk of at most [`CONFIRM_BOXES`]
    /// top-level boxes (no payload reads, no `file_starts_at`): it meets an `mdat`, or the `ftyp` has a HEIC brand and a
    /// `meta` is met with no `moov`, or a box header falls in an unreadable sector.
    fn confirms(&self, source: &dyn Source, start: u64) -> bool {
        let mut r = Reader::new(source);
        let (head, n) = r.head(start);
        if !self.recognizes(&head[..n]) {
            return false;
        }
        let source_len = source.len();
        let (mut heic, mut meta, mut moov) = (false, false, false);
        let mut pos = start;
        for _ in 0..CONFIRM_BOXES {
            if pos >= source_len {
                break;
            }
            let (head, n) = r.head(pos);
            let Some(h) = parse_header(&head[..n]) else {
                // Like JPEG's `confirms`: a header in an unreadable sector (read as zeros) is not proof of a non-video.
                return r.bad_in(pos, pos + n as u64);
            };
            match &h.kind {
                b"ftyp" if pos != start => return false,
                b"ftyp" => {
                    let size = h.size.unwrap_or(0);
                    heic = size >= 12 && heic_brand(&mut r, pos, &h, size);
                }
                b"mdat" => return true,
                b"meta" => meta = true,
                b"moov" => moov = true,
                _ => {}
            }
            // Only `mdat` may have no size; a box that overruns the source ends the walk, as in `measure`.
            let Some(size) = h.size else {
                break;
            };
            pos = pos.saturating_add(size);
        }
        // A HEIC is a `meta` in a file without `moov` (an MP4 that merely lists `mif1` has one).
        heic && meta && !moov
    }

    fn measure(&self, source: &dyn Source, start: u64) -> Option<Measurement> {
        let mut r = Reader::new(source);
        let (head, n) = r.head(start);
        if !self.recognizes(&head[..n]) {
            return None;
        }
        let formats = all_formats();
        let walk = walk_boxes(&mut r, &formats, start, true, &[], &mut OwnJpegs::default());
        let first = judge(&mut r, start, &walk)?;
        if !walk.cut_in_mdat {
            return Some(first);
        }
        // Motion-JPEG stores one JPEG per frame in `mdat`, a CR3 keeps its full-size JPEG there and a HEIC may hold JPEG
        // items: those are the file's own, not another file. Walk the box headers again, without the checks inside
        // `mdat` (so this costs one read per box, not per sector), to learn the file's own data starts from its index.
        let again = walk_boxes(
            &mut r,
            &formats,
            start,
            false,
            &[],
            &mut OwnJpegs::default(),
        );
        let own: Vec<u64> = own_starts(&mut r, &again)
            .iter()
            .map(|o| start + o)
            .collect();
        let exempt = !own.is_empty()
            && match again.kind {
                // A damaged or partial index keeps its own health in the walk below.
                FileKind::Cr3 | FileKind::Heic => true,
                // Motion-JPEG (ruling P2): only when the uncut index is Intact.
                _ => judge(&mut r, start, &again).is_some_and(|m| m.health == Health::Intact),
            };
        if exempt {
            // Skip the file's own data starts: the first head that is not one ends the file (for Motion-JPEG, a video
            // whose every head is one of its frames is whole).
            // A CR3's own JPEGs carry EXIF thumbnails that may start on a sector (ruling P13).
            let mut jpegs = if again.kind == FileKind::Cr3 {
                OwnJpegs::new(cr3_jpegs(&mut r, start, &again))
            } else {
                OwnJpegs::default()
            };
            let walk = walk_boxes(&mut r, &formats, start, true, &own, &mut jpegs);
            return judge(&mut r, start, &walk);
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
    /// Payload offset and length of the first `moov`.
    moov: Option<(u64, u64)>,
    /// Payload offset and length of the first top-level `meta`.
    meta: Option<(u64, u64)>,
    /// Payload start and box end of the first top-level `uuid` boxes (bounded).
    uuids: Vec<(u64, u64)>,
    /// The `ftyp` lists a HEIC brand (and does not make it an AVIF).
    heic_brand: bool,
}

/// Top-level `uuid` boxes remembered while walking (the CR3 preview is the second or third one).
const MAX_UUIDS: usize = 16;
/// Compatible brands read from `ftyp`.
const MAX_BRANDS: usize = 64;
/// Top-level boxes that [`IsoBmff::confirms`] looks at.
const CONFIRM_BOXES: usize = 64;
const HEIC_BRANDS: [&[u8; 4]; 6] = [b"heic", b"heix", b"heim", b"heis", b"mif1", b"msf1"];

/// Walks the top-level boxes of the file at `start`. With `inner_checks`, every sector start inside `mdat` where a file
/// starts ends the file there, except the sorted absolute positions in `own` and heads that `jpegs` vouches for.
fn walk_boxes(
    r: &mut Reader,
    formats: &[Box<dyn Format>],
    start: u64,
    inner_checks: bool,
    own: &[u64],
    jpegs: &mut OwnJpegs,
) -> Walk {
    let source_len = r.source.len();
    let mut w = Walk {
        end: start,
        kind: FileKind::Mp4,
        has_mdat: false,
        has_moof: false,
        truncated: false,
        unreadable_header: false,
        cut_in_mdat: false,
        moov: None,
        meta: None,
        uuids: Vec::new(),
        heic_brand: false,
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
        if inner_checks
            && &h.kind == b"mdat"
            && let Some(next) = next_file_inside(r, formats, pos, pos + size, own, jpegs)
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
                } else if &brand == b"crx " {
                    w.kind = FileKind::Cr3;
                }
                w.heic_brand = heic_brand(r, pos, &h, size);
            }
            b"mdat" => w.has_mdat = true,
            b"moof" => w.has_moof = true,
            b"moov" if w.moov.is_none() => {
                w.moov = Some((pos + h.header_len, size.saturating_sub(h.header_len)))
            }
            b"meta" if w.meta.is_none() => {
                w.meta = Some((pos + h.header_len, size.saturating_sub(h.header_len)))
            }
            b"uuid" if w.uuids.len() < MAX_UUIDS => w.uuids.push((pos + h.header_len, pos + size)),
            _ => {}
        }
        pos += size;
        w.cut_in_mdat |= cut;
        if w.truncated || cut {
            break;
        }
    }
    w.end = pos;
    if w.heic_brand && w.moov.is_none() && w.kind == FileKind::Mp4 {
        w.kind = FileKind::Heic;
    }
    w
}

/// Whether the `ftyp` at `pos` (declared `size`) lists a HEIC brand, as the major or a compatible brand, and is not an
/// AVIF or CR3.
fn heic_brand(r: &mut Reader, pos: u64, h: &BoxHeader, size: u64) -> bool {
    let mut brand = [0u8; 4];
    r.read(pos + h.header_len, &mut brand);
    // Compatible brands follow the 4-byte minor version.
    let mut compat = vec![0u8; (size.saturating_sub(16) as usize / 4).min(MAX_BRANDS) * 4];
    r.read(pos + 16, &mut compat);
    let listed = |b: &[u8]| HEIC_BRANDS.iter().any(|h| h.as_slice() == b);
    (listed(&brand) || compat.chunks(4).any(listed))
        && &brand != b"avif"
        && &brand != b"avis"
        && &brand != b"crx "
}

/// Loads a box payload for parsing; `None` when it is over [`MOOV_MAX_LEN`].
pub(crate) fn load(r: &mut Reader, at: u64, len: u64) -> Option<Vec<u8>> {
    (len <= MOOV_MAX_LEN).then(|| {
        let mut data = vec![0u8; len as usize];
        r.read(at, &mut data);
        data
    })
}

/// Judges a walked file: `None` if it is not a video, CR3 or HEIC at all.
fn judge(r: &mut Reader, start: u64, w: &Walk) -> Option<Measurement> {
    if w.kind == FileKind::Heic {
        return judge_heic(r, start, w);
    }
    if !w.has_mdat && !w.unreadable_header {
        return None;
    }
    let (pos, kind) = (w.end, w.kind);
    let len = pos - start;
    let measured = |health| {
        Some(Measurement {
            len,
            kind,
            health,
            fragments: Vec::new(),
        })
    };

    let Some((moov_at, moov_len)) = w.moov else {
        return measured(Health::Damaged);
    };
    let data = load(r, moov_at, moov_len);
    if data.is_some() && r.bad_in(moov_at, moov_at + moov_len) {
        return measured(Health::Damaged);
    }
    if w.truncated {
        return measured(Health::Partial);
    }
    let Some(data) = data else {
        return measured(Health::Damaged);
    };
    match index_health(&data, len, w.has_moof) {
        Health::Intact if r.bad_in(start, pos) => measured(Health::Partial),
        Health::Intact if kind == FileKind::Cr3 => measured(cr3_preview_health(r, start, w)),
        health => measured(health),
    }
}

/// Canon's top-level `uuid` that holds the `PRVW` preview.
const CR3_PRVW_UUID: [u8; 16] = [
    0xea, 0xf4, 0x2b, 0x5e, 0x1c, 0x98, 0x4b, 0x88, 0xb9, 0xfb, 0xb7, 0xdc, 0x40, 0x6e, 0x4d, 0x16,
];

/// The `PRVW` JPEG of a CR3 as `(absolute offset, length)`, from the top-level `uuid` boxes `(payload start, box end)`.
/// `None` when there is no `PRVW` or its size field does not fit its box.
pub(crate) fn cr3_prvw(r: &mut Reader, uuids: &[(u64, u64)]) -> Option<(u64, u64)> {
    for &(payload, end) in uuids {
        // uuid (16), 8 bytes, then the PRVW box: size (4), "PRVW", 4+2 reserved, width, height, 1, JPEG size (4).
        let mut head = [0u8; 16 + 8 + 8 + 16];
        if r.read(payload, &mut head) != head.len() || head[..16] != CR3_PRVW_UUID {
            continue;
        }
        if &head[28..32] != b"PRVW" {
            return None;
        }
        let jpeg_len = u64::from(be32(&head[44..48]));
        let jpeg_at = payload + head.len() as u64;
        if jpeg_at + jpeg_len > end {
            return None;
        }
        return Some((jpeg_at, jpeg_len));
    }
    None
}

/// Content check of a CR3 whose structure is complete: the `PRVW` JPEG and the full-size JPEG (when present) must both
/// decode strictly, and have a measurable size, for `Intact` (ruling P18, revised). The CR3 walk checks file heads only
/// inside `mdat`, never inside the `PRVW` box, so a broken `PRVW` is the only sign that a file was written over it:
/// `Partial` keeps the scan looking inside it. No `PRVW`, or one whose size field does not fit its box, is `Partial` too.
fn cr3_preview_health(r: &mut Reader, start: u64, w: &Walk) -> Health {
    if cr3_prvw(r, &w.uuids).is_none() {
        return Health::Partial;
    }
    let candidates = cr3_jpegs(r, start, w);
    every_preview_health(r, &candidates)
}

/// First chunk of the first `trak` of a CR3 `moov` payload as `(offset from the start of the file, length of its first
/// sample)`: Canon keeps the full-size JPEG there.
fn cr3_first_chunk(moov: &[u8]) -> Option<(u64, u64)> {
    let mut garbled = false;
    let trak = child(moov, b"trak", &mut garbled)?;
    let stbl = child(trak, b"mdia", &mut garbled)
        .and_then(|m| child(m, b"minf", &mut garbled))
        .and_then(|m| child(m, b"stbl", &mut garbled))?;
    // version/flags (4), entry count (4), then the first entry.
    let first = |p: &[u8], wide: bool| {
        (be32(p.get(4..8)?) > 0)
            .then(|| {
                if wide {
                    p.get(8..16).map(be64)
                } else {
                    p.get(8..12).map(|b| u64::from(be32(b)))
                }
            })
            .flatten()
    };
    let at = match child(stbl, b"stco", &mut garbled) {
        Some(p) => first(p, false)?,
        None => first(child(stbl, b"co64", &mut garbled)?, true)?,
    };
    let stsz = child(stbl, b"stsz", &mut garbled)?;
    let fixed = u64::from(be32(stsz.get(4..8)?));
    let len = if fixed != 0 {
        fixed
    } else {
        u64::from(be32(stsz.get(12..16)?))
    };
    Some((at, len))
}

/// The JPEGs a walked CR3 keeps, as `(absolute offset, length)`: the `PRVW` preview and the full-size JPEG (see
/// [`cr3_full_jpeg`]).
fn cr3_jpegs(r: &mut Reader, start: u64, w: &Walk) -> Vec<(u64, u64)> {
    let moov = w.moov.and_then(|(at, len)| load(r, at, len));
    let full = moov.and_then(|m| cr3_full_jpeg(r, start, &m));
    cr3_prvw(r, &w.uuids).into_iter().chain(full).collect()
}

/// The first chunk of the first track of a CR3 (`moov` payload, file at `start`) as `(absolute offset, length of its
/// first sample)`, when it starts with a JPEG SOI: Canon keeps the full-size JPEG there.
pub(crate) fn cr3_full_jpeg(r: &mut Reader, start: u64, moov: &[u8]) -> Option<(u64, u64)> {
    let (o, l) = cr3_first_chunk(moov)?;
    let at = start.checked_add(o)?;
    let mut soi = [0u8; 3];
    (r.read(at, &mut soi) == 3 && soi == [0xFF, 0xD8, 0xFF]).then_some((at, l))
}

/// Top-level boxes read by [`top_level`].
const MAX_TOP_LEVEL_BOXES: usize = 4096;

/// Where the boxes an inspection needs lie in the file at `start` of `len` bytes. Offsets are absolute.
#[derive(Default)]
pub(crate) struct TopLevel {
    /// Payload offset and length of the first `moov`.
    pub moov: Option<(u64, u64)>,
    /// Payload offset and length of the first `meta`.
    pub meta: Option<(u64, u64)>,
    /// Payload start and box end of the first top-level `uuid` boxes.
    pub uuids: Vec<(u64, u64)>,
}

/// Walks the top-level boxes of the file at `start` (`len` bytes) by their declared sizes, clamped to the file, without
/// reading any payload: the cheap walk for `inspect`. Ends at the first header that is not valid.
pub(crate) fn top_level(r: &mut Reader, start: u64, len: u64) -> TopLevel {
    let mut out = TopLevel::default();
    let end = start.saturating_add(len);
    let mut pos = start;
    for _ in 0..MAX_TOP_LEVEL_BOXES {
        if pos.saturating_add(8) > end {
            break;
        }
        let (head, n) = r.head(pos);
        let Some(h) = parse_header(&head[..n]) else {
            break;
        };
        let size = h.size.unwrap_or(end - pos).min(end - pos);
        if size < h.header_len {
            break;
        }
        let payload = (pos + h.header_len, size - h.header_len);
        match &h.kind {
            b"moov" if out.moov.is_none() => out.moov = Some(payload),
            b"meta" if out.meta.is_none() => out.meta = Some(payload),
            b"uuid" if out.uuids.len() < MAX_UUIDS => out.uuids.push((payload.0, pos + size)),
            _ => {}
        }
        pos += size;
    }
    out
}

/// Extents the `iloc` of a HEIC `meta` lists, and whether some of them could not be checked.
struct Iloc {
    /// `(offset, length)` relative to the start of the file, for construction method 0 and data reference 0 with a
    /// non-zero length.
    extents: Vec<(u64, u64)>,
    /// The same extents with their item ID: `(item ID, offset, length)`.
    items: Vec<(u32, u64, u64)>,
    /// Some extent (or the whole table) cannot be checked against the file.
    unverifiable: bool,
}

const ILOC_MAX_ITEMS: u32 = 65_536;
const ILOC_MAX_EXTENTS: usize = 262_144;

/// Big-endian reader over a byte slice, for `iloc`.
struct Cursor<'a>(&'a [u8]);

impl Cursor<'_> {
    /// An unsigned integer of `n` (0 to 8) bytes; `None` past the end of the data.
    fn uint(&mut self, n: usize) -> Option<u64> {
        let (head, rest) = self.0.split_at_checked(n)?;
        self.0 = rest;
        Some(head.iter().fold(0, |acc, &b| (acc << 8) | u64::from(b)))
    }
}

/// Parses the `iloc` payload. `None` if it is garbled (a bad field size or data that ends early).
fn parse_iloc(payload: &[u8]) -> Option<Iloc> {
    let version = *payload.first()?;
    let mut out = Iloc {
        extents: Vec::new(),
        items: Vec::new(),
        unverifiable: false,
    };
    if version > 2 {
        out.unverifiable = true;
        return Some(out);
    }
    let mut c = Cursor(payload.get(4..)?);
    let sizes = c.uint(2)?;
    let (offset_size, length_size) = ((sizes >> 12) as usize & 15, (sizes >> 8) as usize & 15);
    let (base_size, index_size) = (
        (sizes >> 4) as usize & 15,
        if version >= 1 { sizes as usize & 15 } else { 0 },
    );
    if ![offset_size, length_size, base_size, index_size]
        .iter()
        .all(|s| [0, 4, 8].contains(s))
    {
        return None;
    }
    if offset_size == 0 && length_size == 0 {
        // Every extent is "the whole resource" and takes no bytes: nothing to check, and nothing bounds a loop.
        out.unverifiable = true;
        return Some(out);
    }
    let mut iterations = 0usize;
    let count = if version < 2 { c.uint(2)? } else { c.uint(4)? } as u32;
    if count > ILOC_MAX_ITEMS {
        out.unverifiable = true;
    }
    for _ in 0..count.min(ILOC_MAX_ITEMS) {
        let item_id = c.uint(if version < 2 { 2 } else { 4 })? as u32;
        let method = if version >= 1 { c.uint(2)? & 15 } else { 0 };
        let data_reference = c.uint(2)?;
        let base = c.uint(base_size)?;
        let extent_count = c.uint(2)?;
        for _ in 0..extent_count {
            iterations += 1;
            if iterations > ILOC_MAX_EXTENTS {
                out.unverifiable = true;
                return Some(out);
            }
            c.uint(index_size)?;
            let offset = c.uint(offset_size)?;
            let length = c.uint(length_size)?;
            if method != 0 || data_reference != 0 || length == 0 {
                out.unverifiable = true;
            } else if out.extents.len() < ILOC_MAX_EXTENTS {
                let offset = base.saturating_add(offset);
                out.extents.push((offset, length));
                out.items.push((item_id, offset, length));
            } else {
                out.unverifiable = true;
            }
        }
    }
    Some(out)
}

/// The parsed `iloc` of a `meta` payload: `None` if there is no `iloc` or the boxes (or the table) are garbled.
fn meta_iloc(meta: &[u8]) -> Option<Iloc> {
    let mut garbled = false;
    let iloc = child(meta.get(4..)?, b"iloc", &mut garbled)?; // meta is a full box: version and flags first
    if garbled { None } else { parse_iloc(iloc) }
}

/// ID of the first `Exif` item listed in the `iinf` of a `meta` payload.
fn exif_item_id(meta: &[u8]) -> Option<u32> {
    let mut garbled = false;
    let iinf = child(meta.get(4..)?, b"iinf", &mut garbled)?;
    // version/flags (4), entry count (2 or 4), then the `infe` boxes.
    let skip = if *iinf.first()? == 0 { 6 } else { 8 };
    children(iinf.get(skip..)?, &mut garbled)
        .into_iter()
        .filter(|(k, _)| k == b"infe")
        .find_map(|(_, p)| match *p.first()? {
            // version/flags (4), item_ID (2), protection (2), item_type (4)
            2 if p.get(8..12)? == b"Exif" => Some(u32::from(u16::from_be_bytes([p[4], p[5]]))),
            // version/flags (4), item_ID (4), protection (2), item_type (4)
            3 if p.get(10..14)? == b"Exif" => Some(be32(p.get(4..8)?)),
            _ => None,
        })
}

/// The Exif item of a HEIC `meta` payload as `(offset from the start of the file, length)`.
pub(crate) fn heic_exif_item(meta: &[u8]) -> Option<(u64, u64)> {
    let id = exif_item_id(meta)?;
    let iloc = meta_iloc(meta)?;
    iloc.items
        .iter()
        .find(|i| i.0 == id)
        .map(|&(_, off, len)| (off, len))
}

/// Judges a HEIC: `meta` with an `iloc` whose extents lie inside the measured file.
fn judge_heic(r: &mut Reader, start: u64, w: &Walk) -> Option<Measurement> {
    if w.meta.is_none() && !w.has_mdat && !w.unreadable_header {
        return None;
    }
    let (pos, kind) = (w.end, w.kind);
    let len = pos - start;
    let measured = |health| {
        Some(Measurement {
            len,
            kind,
            health,
            fragments: Vec::new(),
        })
    };

    let Some((at, meta_len)) = w.meta else {
        return measured(Health::Damaged);
    };
    let data = load(r, at, meta_len);
    if data.is_some() && r.bad_in(at, at + meta_len) {
        return measured(Health::Damaged);
    }
    if w.truncated {
        return measured(Health::Partial);
    }
    let Some(data) = data else {
        return measured(Health::Damaged);
    };
    let Some(iloc) = meta_iloc(&data) else {
        return measured(Health::Damaged);
    };
    if iloc.extents.is_empty() && !iloc.unverifiable {
        return measured(Health::Damaged); // nothing to open
    }
    let inside = iloc
        .extents
        .iter()
        .all(|&(o, l)| o.saturating_add(l) <= len);
    measured(if inside && !iloc.unverifiable && !r.bad_in(start, pos) {
        Health::Intact
    } else {
        Health::Partial
    })
}

/// Starts (relative to the file) of the data the file itself keeps in `mdat`: for a HEIC its extents, for a CR3 the
/// chunks
/// of every track, for an MP4/MOV only the chunks of Motion-JPEG video tracks (`hdlr` `vide` whose first sample entry
/// is `mjpa`, `mjpb` or `jpeg`).
fn own_starts(r: &mut Reader, w: &Walk) -> Vec<u64> {
    let mut starts: Vec<u64> = Vec::new();
    if w.kind == FileKind::Heic {
        if let Some((at, len)) = w.meta
            && let Some(data) = load(r, at, len)
            && let Some(iloc) = meta_iloc(&data)
        {
            starts.extend(iloc.extents.iter().map(|e| e.0));
        }
    } else if let Some((at, len)) = w.moov
        && let Some(moov) = load(r, at, len)
    {
        let all_tracks = w.kind == FileKind::Cr3;
        let mut garbled = false;
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
            let Some(stbl) = stbl.filter(|_| all_tracks || video) else {
                continue;
            };
            // version/flags (4), entry count (4), then the first entry: size (4), type (4).
            let mjpeg = child(stbl, b"stsd", &mut garbled)
                .and_then(|p| p.get(12..16))
                .is_some_and(|t| t == b"mjpa" || t == b"mjpb" || t == b"jpeg");
            if all_tracks || mjpeg {
                if let Some(p) = child(stbl, b"stco", &mut garbled) {
                    starts.extend(chunk_offsets(p, false));
                } else if let Some(p) = child(stbl, b"co64", &mut garbled) {
                    starts.extend(chunk_offsets(p, true));
                }
            }
        }
    }
    starts.sort_unstable();
    starts
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
