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
//!   -> `Partial`; else `Intact`;
//! - a video whose index comes before its media data (`moov` before the first `mdat`, or fragmented) is `Intact` only
//!   when every checkable sample passes [`check_sample`] in place (each `moof`'s samples, from [`fragment_samples`], in
//!   a fragmented one); else `Partial` (part 1.6: nothing else tells another file's data in a later piece's place).
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
//! a HEIC when the major brand is a HEIC one or a top-level `meta` was walked (ruling D27); anything else is an MP4. So an
//! MP4 that merely lists `mif1` has a `moov` and stays an MP4, and one that lists `heic` but lost its `moov` (and has no
//! `meta`) is a `Damaged` MP4.
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
//! A HEIC is complete once every extent its `iloc` lists (all checkable, see below) ends inside what was walked (ruling
//! D27): from then on the walk continues only through trailing `free skip wide uuid meta udta mfra`, so a stray `moov`
//! after it neither joins it nor changes its kind, and an unreadable box header after it is what follows the file, not
//! a cut.
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

use std::rc::Rc;

use super::jpeg::{OwnJpegs, every_preview_health};
use super::{
    FileKind, Format, Fragment, Health, MAX_OWN_FILES, Measurement, Reader, SECTOR, builtin,
};
use crate::source::Source;

/// Largest `moov` box the detector will load into memory.
pub const MOOV_MAX_LEN: u64 = 16 * 1024 * 1024;

const ACCEPTED: [&[u8; 4]; 14] = [
    b"ftyp", b"moov", b"mdat", b"free", b"skip", b"wide", b"uuid", b"meta", b"pdin", b"moof",
    b"mfra", b"styp", b"sidx", b"udta",
];

pub struct IsoBmff;

pub(crate) struct BoxHeader {
    pub kind: [u8; 4],
    pub header_len: u64,
    /// Declared total size; `None` when the box runs to the end of the source (size 0).
    pub size: Option<u64>,
}

pub(crate) fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

pub(crate) fn be64(b: &[u8]) -> u64 {
    u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

/// Parses a header from up to 16 bytes. `None` if the box is not valid at top level.
pub(crate) fn parse_header(b: &[u8]) -> Option<BoxHeader> {
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
pub(crate) fn index_health(moov: &[u8], end: u64, fragmented: bool) -> Health {
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
                    heic = size >= 12 && !matches!(heic_brand(&mut r, pos, &h, size), Brand::None);
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
        let formats = builtin();
        let walk = walk_boxes(&mut r, formats, start, true, &[], &mut OwnJpegs::default());
        let first = judge(&mut r, start, &walk)?;
        if !walk.cut_in_mdat {
            return Some(first);
        }
        // Motion-JPEG stores one JPEG per frame in `mdat`, a CR3 keeps its full-size JPEG there and a HEIC may hold JPEG
        // items: those are the file's own, not another file. Walk the box headers again, without the checks inside
        // `mdat` (so this costs one read per box, not per sector), to learn the file's own data starts from its index.
        let again = walk_boxes(&mut r, formats, start, false, &[], &mut OwnJpegs::default());
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
            let walk = walk_boxes(&mut r, formats, start, true, &own, &mut jpegs);
            return judge(&mut r, start, &walk);
        }
        Some(first)
    }

    /// A CR3's `PRVW` JPEG and full-size JPEG (from a walk of the box headers); nothing for other kinds.
    fn own_files(&self, source: &dyn Source, start: u64) -> Vec<Fragment> {
        let mut r = Reader::new(source);
        let (head, n) = r.head(start);
        if !self.recognizes(&head[..n]) {
            return Vec::new();
        }
        let walk = walk_boxes(
            &mut r,
            builtin(),
            start,
            false,
            &[],
            &mut OwnJpegs::default(),
        );
        if walk.kind != FileKind::Cr3 {
            return Vec::new();
        }
        cr3_jpegs(&mut r, start, &walk)
            .into_iter()
            .take(MAX_OWN_FILES)
            .map(|(start, len)| Fragment { start, len })
            .collect()
    }
}

/// Result of walking the top-level boxes.
struct Walk {
    end: u64,
    kind: FileKind,
    has_mdat: bool,
    /// Absolute offset of the first `mdat` box.
    mdat_at: Option<u64>,
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
    /// That HEIC brand is the major one.
    heic_major: bool,
    /// Where (relative to the file) the last extent of the first `meta`'s `iloc` ends, when every extent is checkable.
    heic_end: Option<u64>,
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
        mdat_at: None,
        has_moof: false,
        truncated: false,
        unreadable_header: false,
        cut_in_mdat: false,
        moov: None,
        meta: None,
        uuids: Vec::new(),
        heic_brand: false,
        heic_major: false,
        heic_end: None,
    };
    let mut pos = start;
    while pos < source_len {
        if pos != start && pos.is_multiple_of(SECTOR) && r.file_starts_at(formats, pos) {
            break;
        }
        let (head, n) = r.head(pos);
        // A video with its index and its data, or a HEIC whose every extent ends inside what was walked (ruling D27).
        let heic_complete =
            w.heic_brand && w.moov.is_none() && w.heic_end.is_some_and(|e| e <= pos - start);
        let complete = (w.has_mdat && w.moov.is_some()) || heic_complete;
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
                (w.heic_brand, w.heic_major) = match heic_brand(r, pos, &h, size) {
                    Brand::Major => (true, true),
                    Brand::Compatible => (true, false),
                    Brand::None => (false, false),
                };
            }
            b"mdat" => {
                w.has_mdat = true;
                w.mdat_at.get_or_insert(pos);
            }
            b"moof" => w.has_moof = true,
            b"moov" if w.moov.is_none() => {
                w.moov = Some((pos + h.header_len, size.saturating_sub(h.header_len)))
            }
            b"meta" if w.meta.is_none() => {
                let (at, len) = (pos + h.header_len, size.saturating_sub(h.header_len));
                w.meta = Some((at, len));
                if w.heic_brand && w.moov.is_none() {
                    w.heic_end = load(r, at, len)
                        .and_then(|m| meta_iloc(&m))
                        .and_then(|iloc| iloc.end());
                }
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
    if heic_kind(
        w.heic_brand,
        w.heic_major,
        w.meta.is_some(),
        w.moov.is_some(),
    ) && w.kind == FileKind::Mp4
    {
        w.kind = FileKind::Heic;
    }
    w
}

/// Ruling D27: a file with a HEIC brand is a HEIC only without `moov`, and only when that brand is the major one or a
/// top-level `meta` is there.
fn heic_kind(brand: bool, major: bool, meta: bool, moov: bool) -> bool {
    brand && !moov && (major || meta)
}

/// Whether an `ftyp` lists a HEIC brand.
enum Brand {
    None,
    /// Among the compatible brands only.
    Compatible,
    /// As the major brand.
    Major,
}

/// Whether the `ftyp` at `pos` (declared `size`) lists a HEIC brand, as the major or a compatible brand, and is not an
/// AVIF or CR3.
fn heic_brand(r: &mut Reader, pos: u64, h: &BoxHeader, size: u64) -> Brand {
    let mut brand = [0u8; 4];
    r.read(pos + h.header_len, &mut brand);
    // Compatible brands follow the 4-byte minor version.
    let mut compat = vec![0u8; (size.saturating_sub(16) as usize / 4).min(MAX_BRANDS) * 4];
    r.read(pos + 16, &mut compat);
    let listed = |b: &[u8]| HEIC_BRANDS.iter().any(|h| h.as_slice() == b);
    if &brand == b"avif" || &brand == b"avis" || &brand == b"crx " {
        Brand::None
    } else if listed(&brand) {
        Brand::Major
    } else if compat.chunks(4).any(listed) {
        Brand::Compatible
    } else {
        Brand::None
    }
}

/// Loads a box payload for parsing; `None` when it is over [`MOOV_MAX_LEN`]. The reader keeps it: asking for the same
/// `(at, len)` again (in a measurement, the `moov` is wanted by the judge, the own-start list and the CR3 JPEG lookup)
/// does not read it again.
pub(crate) fn load(r: &mut Reader, at: u64, len: u64) -> Option<Rc<Vec<u8>>> {
    (len <= MOOV_MAX_LEN).then(|| r.load_box(at, len))
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
        Health::Intact if !index_first_samples_fit(r, start, len, &data, w) => {
            measured(Health::Partial)
        }
        health => measured(health),
    }
}

/// Sample-check steps (one per NAL unit or JPEG) of one measurement: past it the samples are not all checked and the
/// video is at most `Partial`.
const MEASURE_STEPS: u64 = 1 << 24;
/// Most top-level boxes walked by the fragment check of one measurement (part 1.6, C9): a hostile chain of tiny boxes
/// is bounded by count, not only by bytes.
pub(crate) const MAX_BOXES: usize = 1 << 16;

/// For a video whose index comes before its media data (`moov` before the first `mdat`, or fragments), whether every
/// checkable sample passes [`check_sample`] in place within the measured `len` bytes: a video stored in pieces with
/// other data in the place of a later piece has no other sign (its `mdat` size, or its box chain, still fits). A
/// `moov` at the end needs no check: it is found only where the media data really ends. A garbled `moof`, a sample
/// outside the file, fragments that do not follow each other ([`Chain::follows`]), an unreadable byte or more than
/// [`MEASURE_STEPS`] steps (every sample a `moof` lists costs one, of any track): `false`. Sample tables that cannot be
/// listed ([`index_tracks`]) and tracks with no check: nothing to check, `true`.
fn index_first_samples_fit(r: &mut Reader, start: u64, len: u64, moov: &[u8], w: &Walk) -> bool {
    samples_fit(r, start, len, moov, w, false, &mut 0)
}

/// [`index_first_samples_fit`]; with `every`, whatever the layout (a `moov` at the end too), and `checked` counts the
/// samples checked.
fn samples_fit(
    r: &mut Reader,
    start: u64,
    len: u64,
    moov: &[u8],
    w: &Walk,
    every: bool,
    checked: &mut u64,
) -> bool {
    let mut steps = MEASURE_STEPS;
    let mut check = |r: &mut Reader, s: &CheckedSample, steps: &mut u64| {
        *checked += 1;
        s.offset.checked_add(s.size).is_some_and(|e| e <= len)
            && check_sample(
                &mut |off, buf| {
                    let at = start + off;
                    r.read(at, buf) == buf.len() && !r.bad_in(at, at + buf.len() as u64)
                },
                s,
                steps,
            )
    };
    if w.has_moof {
        let mut chain = None;
        let Some(track) = frag_track(moov) else {
            return true;
        };
        let end = start + len;
        let mut pos = start;
        for _ in 0..MAX_BOXES {
            if pos >= end {
                return true;
            }
            let (head, n) = r.head(pos);
            let Some(h) = parse_header(&head[..n]) else {
                return true;
            };
            let size = h.size.unwrap_or(end - pos).min(end - pos);
            if size < h.header_len {
                return false;
            }
            if &h.kind == b"moof" {
                let Some(payload) = load(r, pos + h.header_len, size - h.header_len) else {
                    return false;
                };
                // Every sample listed costs a step, and the fragments must follow each other (a fragment of another
                // clip in a later piece's place checks itself).
                let Some(f) = fragment_samples(pos - start, &payload, &track, &mut steps) else {
                    return false;
                };
                let Some(next) = Chain::follows(chain, &f, pos - start) else {
                    return false;
                };
                chain = Some(next);
                if !f.samples.iter().all(|s| check(r, s, &mut steps)) {
                    return false;
                }
            }
            pos += size;
        }
        return false;
    }
    if !every
        && w.moov
            .is_none_or(|(at, _)| w.mdat_at.is_none_or(|m| m < at))
    {
        return true;
    }
    // Tables the sample listing cannot use (no `stsc`, say) are judged by `index_health` alone, as before.
    let Some(tracks) = index_tracks(moov) else {
        return true;
    };
    tracks
        .iter()
        .filter_map(|t| t.check.map(|c| (t, c)))
        .all(|(t, c)| {
            t.samples().iter().all(|s| {
                check(
                    r,
                    &CheckedSample {
                        offset: s.offset,
                        size: s.size,
                        check: c,
                    },
                    &mut steps,
                )
            })
        })
}

/// Every content check a video's index allows over the `len` bytes at `start` of `source`, whatever its layout (part
/// 1.6, D14: the view of an exFAT old chain, whose `moov` lies where the chain puts it): its index `Intact` against
/// `len` ([`index_health`]) and every checkable sample (or every sample each `moof` lists, the fragments following each
/// other) passing [`check_sample`] in place, at least one of them checked; bounded by [`MEASURE_STEPS`]. `false` for a
/// file that is no video with an index (a CR3, a HEIC).
pub(crate) fn every_sample_fits(source: &dyn Source, start: u64, len: u64) -> bool {
    let mut r = Reader::new(source);
    let w = walk_boxes(&mut r, &[], start, false, &[], &mut OwnJpegs::default());
    if !matches!(w.kind, FileKind::Mp4 | FileKind::Mov) || w.truncated {
        return false;
    }
    let Some((at, moov_len)) = w.moov else {
        return false;
    };
    let Some(moov) = load(&mut r, at, moov_len) else {
        return false;
    };
    if r.bad_in(at, at + moov_len) || index_health(&moov, len, w.has_moof) != Health::Intact {
        return false;
    }
    let mut checked = 0;
    samples_fit(&mut r, start, len, &moov, &w, true, &mut checked) && checked > 0
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

/// Where a video without `moov` keeps its media data.
pub(crate) struct MdatLayout {
    /// From the `ftyp`, as the walk decides.
    pub kind: FileKind,
    /// Absolute offset of the `mdat` box.
    pub mdat_at: u64,
    /// 8, or 16 for a 64-bit size.
    pub header_len: u64,
    /// The `mdat` box's declared size (header included), non-zero.
    pub declared: u64,
}

/// Header-only walk from `start` (no payload reads): `Some` only for an `ftyp` followed, through at most
/// [`CONFIRM_BOXES`] other accepted boxes, by an `mdat` with a declared (non-zero) size, with no `moov` before it and none
/// right after it (a video whose own `moov` follows its `mdat` is not missing one).
pub(crate) fn moovless_layout(r: &mut Reader, start: u64) -> Option<MdatLayout> {
    let (head, n) = r.head(start);
    let h = parse_header(&head[..n])?;
    let size = h.size?;
    if &h.kind != b"ftyp" || !(12..=4096).contains(&size) {
        return None;
    }
    let mut brand = [0u8; 4];
    r.read(start + h.header_len, &mut brand);
    let mut kind = match &brand {
        b"qt  " => FileKind::Mov,
        b"crx " => FileKind::Cr3,
        _ => FileKind::Mp4,
    };
    let heic = heic_brand(r, start, &h, size);
    let mut meta = false;
    let mut pos = start + size;
    for _ in 0..CONFIRM_BOXES {
        let (head, n) = r.head(pos);
        let h = parse_header(&head[..n])?;
        let size = h.size?;
        match &h.kind {
            b"mdat" => {
                let end = pos.checked_add(size)?;
                let (next, n) = r.head(end);
                if parse_header(&next[..n]).is_some_and(|b| &b.kind == b"moov") {
                    return None;
                }
                let (brand, major) = match heic {
                    Brand::Major => (true, true),
                    Brand::Compatible => (true, false),
                    Brand::None => (false, false),
                };
                if kind == FileKind::Mp4 && heic_kind(brand, major, meta, false) {
                    kind = FileKind::Heic;
                }
                return Some(MdatLayout {
                    kind,
                    mdat_at: pos,
                    header_len: h.header_len,
                    declared: size,
                });
            }
            b"moov" | b"ftyp" | b"moof" => return None,
            b"meta" => {
                meta = true;
                pos = pos.checked_add(size)?;
            }
            _ => pos = pos.checked_add(size)?,
        }
    }
    None
}

/// Where a video whose index comes before its media data keeps them (part 1.6): `ftyp`, then a `moov`, then either an
/// `mdat` (moov first) or the movie fragments (`moof`, or `sidx`/`styp` before them).
pub(crate) struct IndexFirst {
    /// From the `ftyp` (`Mov` for `qt  `, else `Mp4`).
    pub kind: FileKind,
    /// The `moov` box: absolute offset, header length, size.
    pub moov: (u64, u64, u64),
    /// Absolute end of what comes before the media data: the first `mdat`'s payload start (moov first), or the first
    /// fragment box (fragmented).
    pub head_end: u64,
    /// Moov first: the `mdat` box (absolute offset, declared size or `None` for size 0); `None` when fragmented.
    pub mdat: Option<(u64, Option<u64>)>,
}

/// Header-only walk from `start` (no payload reads), through at most [`CONFIRM_BOXES`] boxes: `Some` for an `ftyp`
/// (not a CR3's) followed by a `moov` and, after it, an `mdat` or a fragment box (`moof sidx styp`). `None` with no
/// `moov` before the media data, two `moov`s, or a garbled header on the way.
pub(crate) fn index_first_layout(r: &mut Reader, start: u64) -> Option<IndexFirst> {
    let (head, n) = r.head(start);
    let h = parse_header(&head[..n])?;
    let size = h.size?;
    if &h.kind != b"ftyp" || !(12..=4096).contains(&size) {
        return None;
    }
    let mut brand = [0u8; 4];
    r.read(start + h.header_len, &mut brand);
    let kind = match &brand {
        b"qt  " => FileKind::Mov,
        b"crx " => return None,
        _ => FileKind::Mp4,
    };
    let mut moov = None;
    let mut pos = start.checked_add(size)?;
    for _ in 0..CONFIRM_BOXES {
        let (head, n) = r.head(pos);
        let h = parse_header(&head[..n])?;
        match &h.kind {
            b"moov" if moov.is_none() => {
                let size = h.size?;
                moov = Some((pos, h.header_len, size));
                pos = pos.checked_add(size)?;
            }
            b"mdat" => {
                return Some(IndexFirst {
                    kind,
                    moov: moov?,
                    head_end: pos + h.header_len,
                    mdat: Some((pos, h.size)),
                });
            }
            b"moof" | b"sidx" | b"styp" => {
                return Some(IndexFirst {
                    kind,
                    moov: moov?,
                    head_end: pos,
                    mdat: None,
                });
            }
            b"moov" | b"ftyp" => return None,
            _ => pos = pos.checked_add(h.size?)?,
        }
    }
    None
}

/// Where the media of a video whose index comes first and whose `mdat` has size 0 ("to the end of the file") really
/// ends: the highest sample end its `moov` lists, relative to `start` (the end `VideoSeams::with_own_moov` takes for
/// such a file). `None` for any other layout, a `moov` larger than [`MOOV_MAX_LEN`] or unreadable, or a track whose
/// samples cannot be bounded. A size-0 `mdat` runs to the end of what is read, so its measured length says nothing
/// about where the content ends (part 1.6, ruling Q56).
pub(crate) fn size_zero_mdat_end(source: &dyn Source, start: u64) -> Option<u64> {
    let mut r = Reader::new(source);
    let l = index_first_layout(&mut r, start)?;
    let (_, None) = l.mdat? else {
        return None;
    };
    let (moov_at, hl, size) = l.moov;
    let len = size.checked_sub(hl).filter(|&n| n <= MOOV_MAX_LEN)?;
    let payload = r.load_box(moov_at + hl, len);
    if r.bad_in(moov_at, moov_at + size) || (payload.len() as u64) < len {
        return None;
    }
    let mut end = 0u64;
    for t in index_tracks(&payload)?.iter().filter(|t| t.count > 0) {
        end = end.max(t.bounds()?.1);
    }
    Some(end)
}

/// One sample of a track.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Sample {
    /// Relative to the start of the file.
    pub offset: u64,
    pub size: u64,
}

/// How a track's samples can be checked without decoding them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Check {
    /// H.264 (`avc1`/`avc3`) or HEVC (`hvc1`/`hev1`): length-prefixed NAL units, `length_size` bytes per length.
    Nal { length_size: u8, hevc: bool },
    /// Motion-JPEG: one JPEG per sample.
    Mjpeg,
}

/// A sample to check: its offset in the file, size (from `stsz`) and track check.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CheckedSample {
    pub offset: u64,
    pub size: u64,
    pub check: Check,
}

/// Most NAL units walked per sample.
pub(crate) const MAX_NAL_UNITS: usize = 512;

/// How the check of one sample ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SampleEnd {
    Pass,
    /// The bytes read up to file offset `at` (exclusive) do not fit.
    Fail {
        at: u64,
    },
    /// A read of the bytes from file offset `at` was refused.
    Unread {
        at: u64,
    },
    /// The steps ran out.
    Spent,
}

/// The 1.3 structural check of one sample (NAL lengths and header bytes with `length_size` from avcC/hvcC, or SOI/EOI
/// for Motion-JPEG), reading through `read(file offset, buf) -> bool`; `steps` is charged per NAL unit, false when spent.
pub(crate) fn check_sample(
    read: &mut dyn FnMut(u64, &mut [u8]) -> bool,
    s: &CheckedSample,
    steps: &mut u64,
) -> bool {
    walk_sample(read, s, steps, &mut |_, _| {}) == SampleEnd::Pass
}

/// [`check_sample`], telling how it ended and handing `checked` each file range `[from, to)` whose bytes the check read
/// and accepted (a NAL unit's length and header bytes, a JPEG's SOI and EOI), in file order. A failing check hands
/// nothing for the read that failed; the bytes it read end at `SampleEnd::Fail::at`.
pub(crate) fn walk_sample(
    read: &mut dyn FnMut(u64, &mut [u8]) -> bool,
    s: &CheckedSample,
    steps: &mut u64,
    checked: &mut dyn FnMut(u64, u64),
) -> SampleEnd {
    let mut take = || {
        if *steps == 0 {
            false
        } else {
            *steps -= 1;
            true
        }
    };
    let end = s.offset.saturating_add(s.size);
    match s.check {
        Check::Mjpeg => {
            if !take() {
                return SampleEnd::Spent;
            }
            if s.size < 5 {
                return SampleEnd::Fail { at: end };
            }
            let (mut soi, mut eoi) = ([0u8; 3], [0u8; 2]);
            if !read(s.offset, &mut soi) {
                return SampleEnd::Unread { at: s.offset };
            }
            if soi != [0xFF, 0xD8, 0xFF] {
                return SampleEnd::Fail { at: s.offset + 3 };
            }
            if !read(end - 2, &mut eoi) {
                return SampleEnd::Unread { at: end - 2 };
            }
            if eoi != [0xFF, 0xD9] {
                return SampleEnd::Fail { at: end };
            }
            checked(s.offset, s.offset + 3);
            checked(end - 2, end);
            SampleEnd::Pass
        }
        Check::Nal { length_size, hevc } => {
            // An empty sample costs a step too (a hostile index may list millions of them).
            if s.size == 0 {
                return if take() {
                    SampleEnd::Pass
                } else {
                    SampleEnd::Spent
                };
            }
            let ls = u64::from(length_size);
            let need = ls + if hevc { 2 } else { 1 };
            let mut pos = 0u64;
            for _ in 0..MAX_NAL_UNITS {
                if pos == s.size {
                    return SampleEnd::Pass;
                }
                let at = s.offset + pos;
                if !take() {
                    return SampleEnd::Spent;
                }
                if pos + need > s.size {
                    // The last length read was wrong: nothing past it was read.
                    return SampleEnd::Fail { at };
                }
                let mut h = [0u8; 6];
                let h = &mut h[..need as usize];
                if !read(at, h) {
                    return SampleEnd::Unread { at };
                }
                let len = h[..ls as usize]
                    .iter()
                    .fold(0u64, |acc, &b| (acc << 8) | u64::from(b));
                let header = h[ls as usize];
                let ok = header & 0x80 == 0
                    && if hevc {
                        len >= 2 && (header >> 1) & 0x3F <= 40 && h[ls as usize + 1] & 7 != 0
                    } else {
                        len >= 1 && (1..=23).contains(&(header & 0x1F))
                    };
                if !ok {
                    return SampleEnd::Fail { at: at + need };
                }
                pos += ls + len;
                if pos > s.size {
                    return SampleEnd::Fail { at: at + need };
                }
                checked(at, at + need);
            }
            if pos == s.size {
                SampleEnd::Pass
            } else {
                SampleEnd::Fail { at: s.offset + pos }
            }
        }
    }
}

/// Most samples [`track_samples`] lists for one `moov` (all tracks together).
pub(crate) const MAX_SAMPLES: usize = 4_000_000;

/// The checkable video track of a fragmented file, as its `moov` describes it: the `trak` whose samples the `moof`s
/// list (its `tkhd` track ID), its check, and the default sample size of its `mvex/trex` (0 when none).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FragTrack {
    pub id: u32,
    pub check: Check,
    pub default_size: u32,
    /// The `trex` default sample duration (0 when none).
    pub default_duration: u32,
}

/// The first video `trak` of a `moov` payload with a checkable sample entry (see [`Check`]), with its track ID and the
/// default sample size its `mvex/trex` gives; `None` when there is none or the boxes are garbled.
pub(crate) fn frag_track(moov: &[u8]) -> Option<FragTrack> {
    let mut garbled = false;
    let boxes = children(moov, &mut garbled);
    let track = boxes
        .iter()
        .filter(|(k, _)| k == b"trak")
        .find_map(|(_, trak)| {
            let mut g = false;
            let tkhd = child(trak, b"tkhd", &mut g)?;
            // Version 0: version/flags, creation and modification (4 each), then the track ID; version 1: 8-byte times.
            let id_at = if *tkhd.first()? == 1 { 20 } else { 12 };
            let id = be32(tkhd.get(id_at..id_at + 4)?);
            let mdia = child(trak, b"mdia", &mut g)?;
            let video = child(mdia, b"hdlr", &mut g).is_some_and(|p| p.get(8..12) == Some(b"vide"));
            let stbl = child(mdia, b"minf", &mut g).and_then(|m| child(m, b"stbl", &mut g))?;
            let check = child(stbl, b"stsd", &mut g).and_then(sample_check)?;
            (video && !g).then_some((id, check))
        });
    let (id, check) = track?;
    // trex: version/flags, track ID, default sample description index, duration, size, flags.
    let trex = boxes
        .iter()
        .filter(|(k, _)| k == b"mvex")
        .flat_map(|(_, mvex)| children(mvex, &mut garbled))
        .find(|(k, p)| k == b"trex" && p.get(4..8).map(be32) == Some(id))
        .map(|(_, p)| p);
    let field = |at: usize| trex.and_then(|p| p.get(at..at + 4)).map_or(0, be32);
    (!garbled).then_some(FragTrack {
        id,
        check,
        default_size: field(16),
        default_duration: field(12),
    })
}

/// What one `moof` says about the video track: its sequence number (`mfhd`), its samples and its time.
#[derive(Clone, Debug)]
pub(crate) struct MovieFragment {
    /// The `mfhd` sequence number (`None` without an `mfhd`).
    pub seq: Option<u32>,
    pub samples: Vec<CheckedSample>,
    /// The track's `tfdt` base media decode time, when there is one.
    pub start_time: Option<u64>,
    /// The sum of the track's sample durations (`trun`, else the `tfhd`, else the `trex` default).
    pub duration: u64,
    /// The `moof` holds a `traf` of the track.
    pub has_track: bool,
}

/// Where a chain of fragments stands after a fragment: its sequence number, the decode time the next fragment of the
/// track must start at (when known), and where its `moof` is (file offset).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Chain {
    pub seq: u32,
    pub time: Option<u64>,
    pub at: u64,
}

impl Chain {
    /// The chain after fragment `f` (its `moof` at file offset `at`) when `f` follows `prev` (the chain so far, `None`
    /// before the first fragment): its `mfhd` sequence number is the previous one plus one (1 for the first fragment, as
    /// a camera starts it), and, when it holds the track, its `tfdt` (when both are known) is where the previous
    /// fragment's samples ended; one without the track (a muxer writing one `moof` per track) leaves the time as it was.
    /// `None` when it does not follow: a fragment of another clip, or one further on.
    pub(crate) fn follows(prev: Option<Chain>, f: &MovieFragment, at: u64) -> Option<Chain> {
        let seq = f.seq?;
        if prev.map_or(1, |p| p.seq.wrapping_add(1)) != seq
            || prev.is_some_and(|p| p.seq == u32::MAX)
        {
            return None;
        }
        // A fragment of other tracks only (one `moof` per track) keeps the sequence going and the time where it was.
        if !f.has_track {
            return Some(Chain {
                seq,
                time: prev.and_then(|p| p.time),
                at,
            });
        }
        let expected = prev.and_then(|p| p.time);
        if let (Some(e), Some(t)) = (expected, f.start_time)
            && e != t
        {
            return None;
        }
        let time = match f.start_time.or(expected) {
            Some(t) => Some(t.checked_add(f.duration)?),
            None => None,
        };
        Some(Chain { seq, time, at })
    }
}

/// The fragment that the `moof` box at file offset `moof_at` (its `payload`, after the header) describes for `track`:
/// its samples absolute in the file, in `trun` order: each `traf/tfhd` gives the base data offset (explicit, or the
/// `moof` start when default-base-is-moof or for the first `traf`, else where the previous `traf`'s data ended) and the
/// default sample size and duration (else the `trex` ones), each `trun` its data offset from that base (else where the
/// previous `trun` ended), its sample sizes and durations. Every sample listed, of any track, costs one of `steps`.
/// `None` when a box is garbled or truncated, an offset or a time falls outside `u64`, the `moof` lists more than
/// [`MAX_SAMPLES`] samples (all tracks together), or the steps run out (then `*steps == 0`).
pub(crate) fn fragment_samples(
    moof_at: u64,
    payload: &[u8],
    track: &FragTrack,
    steps: &mut u64,
) -> Option<MovieFragment> {
    let mut garbled = false;
    let boxes = children(payload, &mut garbled);
    let seq = boxes
        .iter()
        .find(|(k, _)| k == b"mfhd")
        .and_then(|(_, p)| p.get(4..8))
        .map(be32);
    let mut out = MovieFragment {
        seq,
        samples: Vec::new(),
        start_time: None,
        duration: 0,
        has_track: false,
    };
    let mut listed = 0usize;
    let mut prev_end: Option<u64> = None;
    for (_, traf) in boxes.iter().filter(|(k, _)| k == b"traf") {
        let tfhd = child(traf, b"tfhd", &mut garbled)?;
        let flags = be32(tfhd.get(..4)?) & 0x00FF_FFFF;
        let id = be32(tfhd.get(4..8)?);
        let ours = id == track.id;
        out.has_track |= ours;
        let mut at = 8usize;
        let mut field = |n: usize| {
            let f = tfhd.get(at..at + n);
            at += n;
            f
        };
        let explicit = if flags & 0x1 != 0 {
            Some(be64(field(8)?))
        } else {
            None
        };
        if flags & 0x2 != 0 {
            field(4)?;
        }
        let default_duration = if flags & 0x8 != 0 {
            u64::from(be32(field(4)?))
        } else if ours {
            u64::from(track.default_duration)
        } else {
            0
        };
        let default_size = if flags & 0x10 != 0 {
            u64::from(be32(field(4)?))
        } else if ours {
            u64::from(track.default_size)
        } else {
            0
        };
        if flags & 0x20 != 0 {
            field(4)?;
        }
        if ours && let Some(t) = child(traf, b"tfdt", &mut garbled) {
            out.start_time = Some(match *t.first()? {
                1 => be64(t.get(4..12)?),
                _ => u64::from(be32(t.get(4..8)?)),
            });
        }
        let base = match explicit {
            Some(b) => b,
            None if flags & 0x02_0000 != 0 => moof_at,
            None => prev_end.unwrap_or(moof_at),
        };
        let mut data = base;
        for (_, trun) in children(traf, &mut garbled)
            .into_iter()
            .filter(|(k, _)| k == b"trun")
        {
            let flags = be32(trun.get(..4)?) & 0x00FF_FFFF;
            let count = be32(trun.get(4..8)?) as usize;
            let mut at = 8usize;
            if flags & 0x1 != 0 {
                let off = i32::from_be_bytes(trun.get(8..12)?.try_into().ok()?);
                data = base.checked_add_signed(i64::from(off))?;
                at += 4;
            }
            if flags & 0x4 != 0 {
                at += 4;
            }
            listed = listed.checked_add(count).filter(|&n| n <= MAX_SAMPLES)?;
            let fields = [0x100, 0x200, 0x400, 0x800].map(|b| flags & b != 0);
            let per = 4 * fields.iter().filter(|&&f| f).count();
            let size_at = if fields[0] { 4 } else { 0 };
            if at.checked_add(count.checked_mul(per)?)? > trun.len() {
                return None;
            }
            for i in 0..count {
                if *steps == 0 {
                    return None;
                }
                *steps -= 1;
                let e = at + i * per;
                let size = if fields[1] {
                    u64::from(be32(&trun[e + size_at..]))
                } else {
                    default_size
                };
                if ours {
                    let duration = if fields[0] {
                        u64::from(be32(&trun[e..]))
                    } else {
                        default_duration
                    };
                    out.duration = out.duration.checked_add(duration)?;
                    out.samples.push(CheckedSample {
                        offset: data,
                        size,
                        check: track.check,
                    });
                }
                data = data.checked_add(size)?;
            }
        }
        prev_end = Some(data);
    }
    (!garbled).then_some(out)
}

/// The `moof` offsets (in the file) that the `tfra` boxes of an `mfra` payload list, of every track; `None` when a box
/// is garbled or truncated.
pub(crate) fn tfra_offsets(mfra: &[u8]) -> Option<Vec<u64>> {
    let mut garbled = false;
    let mut out = Vec::new();
    for (_, tfra) in children(mfra, &mut garbled)
        .into_iter()
        .filter(|(k, _)| k == b"tfra")
    {
        let wide = *tfra.first()? == 1;
        let sizes = be32(tfra.get(8..12)?);
        let count = be32(tfra.get(12..16)?) as usize;
        let t = if wide { 8 } else { 4 };
        let lens: usize = [4, 2, 0]
            .iter()
            .map(|sh| ((sizes >> sh) & 3) as usize + 1)
            .sum();
        let entry = 2 * t + lens;
        if 16usize.checked_add(count.checked_mul(entry)?)? > tfra.len() {
            return None;
        }
        for i in 0..count {
            let e = &tfra[16 + i * entry + t..];
            out.push(if wide { be64(e) } else { u64::from(be32(e)) });
        }
    }
    (!garbled).then_some(out)
}

/// The sample tables of one track, validated (present, not truncated, the chunks holding exactly the samples `stsz`
/// lists) but not expanded: nothing here is allocated by a count read from the file.
pub(crate) struct IndexTrack<'a> {
    /// `None` = not checkable (audio, other codecs).
    pub check: Option<Check>,
    /// Samples `stsz` lists.
    pub count: usize,
    /// Fixed sample size, or 0 for a table.
    fixed: u64,
    /// The `stsz` payload.
    stsz: &'a [u8],
    /// The `stco`/`co64` payload, its entry count and width.
    chunks: &'a [u8],
    n_chunks: usize,
    wide: bool,
    /// The `stsc` payload and its entry count.
    stsc: &'a [u8],
    runs: usize,
}

impl IndexTrack<'_> {
    fn size(&self, i: usize) -> u64 {
        if self.fixed != 0 {
            self.fixed
        } else {
            u64::from(be32(&self.stsz[12 + 4 * i..]))
        }
    }

    fn chunk(&self, i: usize) -> u64 {
        let e = &self.chunks[8 + if self.wide { 8 } else { 4 } * i..];
        if self.wide {
            be64(e)
        } else {
            u64::from(be32(e))
        }
    }

    fn run(&self, i: usize) -> (u64, u64) {
        let e = &self.stsc[8 + 12 * i..];
        (u64::from(be32(e)), u64::from(be32(&e[4..])))
    }

    /// Calls `f(chunk offset, samples in it)` for every chunk, in table order: `O(chunks + runs)`.
    fn for_each_chunk(&self, mut f: impl FnMut(u64, u64)) {
        let mut r = 0usize;
        for c in 0..self.n_chunks {
            let n = c as u64 + 1;
            while r + 1 < self.runs && self.run(r + 1).0 <= n {
                r += 1;
            }
            let per_chunk = if self.runs > 0 && self.run(r).0 <= n {
                self.run(r).1
            } else {
                0
            };
            f(self.chunk(c), per_chunk);
        }
    }

    /// `(lowest sample offset, highest sample end)`, without listing the samples; `None` for an empty track or an end
    /// past `u64`. Costs `O(chunks + runs)` with a fixed sample size, else `O(samples)` over the `stsz` table itself.
    pub(crate) fn bounds(&self) -> Option<(u64, u64)> {
        let (mut lo, mut hi, mut next, mut ok) = (u64::MAX, 0u64, 0usize, true);
        self.for_each_chunk(|at, n| {
            if n == 0 || !ok {
                return;
            }
            let len = if self.fixed != 0 {
                self.fixed.checked_mul(n)
            } else {
                (next..next + n as usize).try_fold(0u64, |acc, i| acc.checked_add(self.size(i)))
            };
            next += n as usize;
            match len.and_then(|l| at.checked_add(l)) {
                Some(end) => {
                    lo = lo.min(at);
                    hi = hi.max(end);
                }
                None => ok = false,
            }
        });
        (ok && lo != u64::MAX).then_some((lo, hi))
    }

    /// The samples, sorted by offset (allocates `count` entries).
    pub(crate) fn samples(&self) -> Vec<Sample> {
        let mut samples = Vec::with_capacity(self.count);
        self.for_each_chunk(|mut at, n| {
            for _ in 0..n {
                let size = self.size(samples.len());
                samples.push(Sample { offset: at, size });
                at = at.saturating_add(size);
            }
        });
        samples.sort_unstable_by_key(|s| s.offset);
        samples
    }
}

/// The validated sample tables of every track of a `moov` payload (a `trak` without an `stbl` is skipped). `None` for
/// the whole `moov` when its boxes are garbled, a sample table is missing, truncated or inconsistent, or the tracks
/// list more than [`MAX_SAMPLES`] samples in total. Allocates one entry per track, never per sample.
pub(crate) fn index_tracks(moov: &[u8]) -> Option<Vec<IndexTrack<'_>>> {
    let mut garbled = false;
    let mut out = Vec::new();
    let mut total = 0usize;
    for (_, trak) in children(moov, &mut garbled)
        .into_iter()
        .filter(|(k, _)| k == b"trak")
    {
        let Some(mdia) = child(trak, b"mdia", &mut garbled) else {
            continue;
        };
        let video =
            child(mdia, b"hdlr", &mut garbled).is_some_and(|p| p.get(8..12) == Some(b"vide"));
        let stbl = child(mdia, b"minf", &mut garbled).and_then(|m| child(m, b"stbl", &mut garbled));
        let Some(stbl) = stbl else { continue };
        let check = if video {
            child(stbl, b"stsd", &mut garbled).and_then(sample_check)
        } else {
            None
        };
        let track = stbl_tables(stbl, &mut garbled, check)?;
        total = total
            .checked_add(track.count)
            .filter(|&t| t <= MAX_SAMPLES)?;
        out.push(track);
    }
    (!garbled).then_some(out)
}

/// Per track of a `moov` payload: its check (`None` = not checkable: audio, other codecs) and its samples sorted by
/// offset, from `stsz`/`stsc`/`stco`|`co64` (see [`index_tracks`] for when it is `None`). The pairing lists only the
/// checkable tracks, after its filters, through [`IndexTrack::samples`]; this is the whole listing, for tests.
#[cfg(test)]
pub(crate) fn track_samples(moov: &[u8]) -> Option<Vec<(Option<Check>, Vec<Sample>)>> {
    Some(
        index_tracks(moov)?
            .iter()
            .map(|t| (t.check, t.samples()))
            .collect(),
    )
}

/// The tables of one `stbl`; `None` if one is missing or truncated, or the chunks do not hold exactly the samples
/// `stsz` lists.
fn stbl_tables<'a>(
    stbl: &'a [u8],
    garbled: &mut bool,
    check: Option<Check>,
) -> Option<IndexTrack<'a>> {
    let stsz = child(stbl, b"stsz", garbled)?;
    let fixed = u64::from(be32(stsz.get(4..8)?));
    let count = be32(stsz.get(8..12)?) as usize;
    if count > MAX_SAMPLES || (fixed == 0 && stsz.len() < 12 + 4 * count) {
        return None;
    }
    let (chunks, wide) = match child(stbl, b"stco", garbled) {
        Some(p) => (p, false),
        None => (child(stbl, b"co64", garbled)?, true),
    };
    let n_chunks = be32(chunks.get(4..8)?) as usize;
    if chunks.len() < 8 + if wide { 8 } else { 4 } * n_chunks {
        return None;
    }
    let stsc = child(stbl, b"stsc", garbled)?;
    let runs = be32(stsc.get(4..8)?) as usize;
    if stsc.len() < 8 + 12 * runs {
        return None;
    }
    let t = IndexTrack {
        check,
        count,
        fixed,
        stsz,
        chunks,
        n_chunks,
        wide,
        stsc,
        runs,
    };
    // The chunks must hold exactly `count` samples (summed without listing them).
    let mut held = 0u64;
    t.for_each_chunk(|_, n| held = held.saturating_add(n));
    (held == count as u64).then_some(t)
}

/// The check for the first sample entry of an `stsd` payload: `avc1`/`avc3` with an `avcC`, `hvc1`/`hev1` with an `hvcC`
/// (length sizes 1, 2 or 4), or Motion-JPEG.
fn sample_check(stsd: &[u8]) -> Option<Check> {
    // version/flags (4), entry count (4), then the first entry: size (4), type (4), payload.
    let size = be32(stsd.get(8..12)?) as usize;
    let entry = stsd.get(16..8 + size)?;
    let kind = stsd.get(12..16)?;
    let hevc = match kind {
        b"mjpa" | b"mjpb" | b"jpeg" => return Some(Check::Mjpeg),
        b"avc1" | b"avc3" => false,
        b"hvc1" | b"hev1" => true,
        _ => return None,
    };
    // A visual sample entry has 78 bytes of fields before its child boxes.
    let mut ignored = false;
    let config = child(
        entry.get(78..)?,
        if hevc { b"hvcC" } else { b"avcC" },
        &mut ignored,
    )?;
    let length_size = (config.get(if hevc { 21 } else { 4 })? & 3) + 1;
    (length_size != 3).then_some(Check::Nal { length_size, hevc })
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

impl Iloc {
    /// Where the last extent ends (relative to the file), when every extent is checkable and there is at least one.
    fn end(&self) -> Option<u64> {
        if self.unverifiable {
            return None;
        }
        self.extents.iter().map(|&(o, l)| o.saturating_add(l)).max()
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::EngineError;
    use crate::formats::tests::Mem;
    use crate::source::VolumeId;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Counts the reads that overlap `[lo, hi)`.
    struct Counting {
        inner: Mem,
        lo: u64,
        hi: u64,
        hits: AtomicUsize,
    }

    impl Source for Counting {
        fn len(&self) -> u64 {
            self.inner.len()
        }
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, EngineError> {
            if offset < self.hi && offset + buf.len() as u64 > self.lo {
                self.hits.fetch_add(1, Ordering::SeqCst);
            }
            self.inner.read_at(offset, buf)
        }
        fn volume(&self) -> Option<VolumeId> {
            None
        }
    }

    fn avc(samples: usize, small_nals: bool) -> revenant_testkit::AvcVideo {
        revenant_testkit::avc_video(&revenant_testkit::AvcOptions {
            samples,
            seed: 3,
            mov: false,
            mdat_pad: 0,
            small_nals,
        })
    }

    #[test]
    fn track_samples_lists_the_index() {
        let v = avc(10, false);
        let t = track_samples(&v.bytes[v.moov.start + 8..v.moov.end]).unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!(
            t[0].0,
            Some(Check::Nal {
                length_size: 4,
                hevc: false
            })
        );
        let s = &t[0].1;
        assert_eq!(s.len(), 10);
        assert_eq!(s[0].offset, v.mdat_payload.start as u64);
        assert!(s.windows(2).all(|w| w[0].offset + w[0].size == w[1].offset));
        assert_eq!(s[9].offset + s[9].size, v.mdat_payload.end as u64);
        assert!(s.iter().all(|x| (2048..=24 * 1024).contains(&x.size)));
        // The bounds, from the tables alone, match the listing.
        let moov = &v.bytes[v.moov.start + 8..v.moov.end];
        let t = index_tracks(moov).unwrap();
        assert_eq!(t[0].count, 10);
        assert_eq!(
            t[0].bounds(),
            Some((v.mdat_payload.start as u64, v.mdat_payload.end as u64))
        );
    }

    /// The `moov` payload of `v` with 4 bytes at `delta` past the `kind` box type replaced by `value`.
    fn patched(
        v: &revenant_testkit::AvcVideo,
        kind: &[u8; 4],
        delta: usize,
        value: u32,
    ) -> Vec<u8> {
        let mut moov = v.bytes[v.moov.start + 8..v.moov.end].to_vec();
        let at = moov.windows(4).position(|w| w == kind).unwrap() + delta;
        moov[at..at + 4].copy_from_slice(&value.to_be_bytes());
        moov
    }

    /// Every sample of `v` (its only track) as a [`CheckedSample`].
    fn checked(v: &revenant_testkit::AvcVideo) -> Vec<CheckedSample> {
        let t = track_samples(&v.bytes[v.moov.start + 8..v.moov.end]).unwrap();
        let check = t[0].0.expect("a checkable track");
        t[0].1
            .iter()
            .map(|s| CheckedSample {
                offset: s.offset,
                size: s.size,
                check,
            })
            .collect()
    }

    #[test]
    fn check_sample_matches_the_pairing_check() {
        use revenant_testkit::{AvcOptions, VideoCodec, VideoLayout, camera_video};
        for (codec, small_nals) in [
            (VideoCodec::Avc, true),
            (VideoCodec::Avc, false),
            (VideoCodec::Hevc, true),
            (VideoCodec::Hevc, false),
        ] {
            let opts = AvcOptions {
                samples: 40,
                seed: 9,
                mov: false,
                mdat_pad: 0,
                small_nals,
            };
            let v = camera_video(&opts, codec, VideoLayout::MoovAtEnd);
            let bytes = &v.bytes;
            let mut read = |at: u64, buf: &mut [u8]| {
                let at = at as usize;
                match bytes.get(at..at + buf.len()) {
                    Some(b) => {
                        buf.copy_from_slice(b);
                        true
                    }
                    None => false,
                }
            };
            let samples = checked(&v);
            assert_eq!(
                samples[0].check,
                Check::Nal {
                    length_size: 4,
                    hevc: codec == VideoCodec::Hevc
                }
            );
            let mut steps = u64::MAX;
            for s in &samples {
                assert!(check_sample(&mut read, s, &mut steps), "{codec:?} {s:?}");
            }
            // Each sample read one sector off its place fails (its first NAL length is payload).
            for s in &samples {
                let off = CheckedSample {
                    offset: s.offset + 512,
                    ..*s
                };
                assert!(
                    !check_sample(&mut read, &off, &mut steps),
                    "{codec:?} {s:?}"
                );
            }
            // Steps: one per NAL unit; none left, no check passes.
            let mut none = 0;
            assert!(!check_sample(&mut read, &samples[0], &mut none));
            let mut one = 1;
            let first = samples[0];
            let units = {
                let mut n = u64::MAX;
                assert!(check_sample(&mut read, &first, &mut n));
                u64::MAX - n
            };
            assert!((1..=3).contains(&units), "{units}");
            assert_eq!(check_sample(&mut read, &first, &mut one), units == 1);
        }
        // Motion-JPEG: SOI at the start and EOI at the end, one step.
        let jpeg = revenant_testkit::jpeg(32, 32, 5, 0);
        let mut read = |at: u64, buf: &mut [u8]| {
            let at = at as usize;
            match jpeg.get(at..at + buf.len()) {
                Some(b) => {
                    buf.copy_from_slice(b);
                    true
                }
                None => false,
            }
        };
        let s = CheckedSample {
            offset: 0,
            size: jpeg.len() as u64,
            check: Check::Mjpeg,
        };
        let mut steps = 1;
        assert!(check_sample(&mut read, &s, &mut steps));
        assert_eq!(steps, 0);
        let short = CheckedSample {
            size: s.size - 1,
            ..s
        };
        assert!(!check_sample(&mut read, &short, &mut 8));
    }

    #[test]
    fn track_samples_rejects_truncated_or_inconsistent_tables() {
        let v = avc(10, true);
        // Counts far past the tables (stsz count, stco count, stsc entry count).
        assert!(track_samples(&patched(&v, b"stsz", 12, u32::MAX)).is_none());
        assert!(track_samples(&patched(&v, b"stco", 8, u32::MAX)).is_none());
        assert!(track_samples(&patched(&v, b"stsc", 8, u32::MAX)).is_none());
        // 5 samples per chunk: the 3 chunks would hold 15 samples, stsz lists 10.
        assert!(track_samples(&patched(&v, b"stsc", 16, 5)).is_none());
        // 3 per chunk: they hold 9.
        assert!(track_samples(&patched(&v, b"stsc", 16, 3)).is_none());
        // A fixed sample size above the 4,000,000-sample cap.
        let mut over = patched(&v, b"stsz", 8, 100);
        let at = over.windows(4).position(|w| w == b"stsz").unwrap() + 12;
        over[at..at + 4].copy_from_slice(&(MAX_SAMPLES as u32 + 1).to_be_bytes());
        assert!(track_samples(&over).is_none());
        // Not a known codec: listed, not checkable.
        let t = track_samples(&patched(&v, b"avc1", 0, u32::from_be_bytes(*b"mp4a"))).unwrap();
        assert_eq!(t[0].0, None);
        assert_eq!(t[0].1.len(), 10);
    }

    #[test]
    fn moovless_layout_needs_ftyp_then_mdat_without_moov() {
        let v = avc(20, true);
        let cut = v.mdat_payload.start + 4096;
        let mut image = vec![0x33u8; 8192];
        image.extend_from_slice(&v.bytes[..cut]);
        image.resize(image.len() + 65536, 0x33);
        let src = Mem(image);
        let l = moovless_layout(&mut Reader::new(&src), 8192).unwrap();
        assert_eq!(l.kind, FileKind::Mp4);
        assert_eq!(l.mdat_at, 8192 + 20);
        assert_eq!(l.header_len, 8);
        assert_eq!(l.declared, (v.moov.start - 20) as u64);
        // The whole video has its moov right after its mdat.
        let whole = Mem([vec![0; 512], v.bytes.clone()].concat());
        assert!(moovless_layout(&mut Reader::new(&whole), 512).is_none());
        // moov before mdat, or a size-0 mdat.
        let first = revenant_testkit::mp4(&revenant_testkit::Mp4Options {
            mdat_len: 4096,
            moov_at_end: false,
            mov: true,
            mdat_64bit: false,
        });
        assert!(moovless_layout(&mut Reader::new(&Mem(first)), 0).is_none());
        let mut zero = v.bytes[..cut].to_vec();
        zero[20..24].copy_from_slice(&[0; 4]);
        assert!(moovless_layout(&mut Reader::new(&Mem(zero)), 0).is_none());
    }

    #[test]
    fn moov_is_loaded_once_per_measure() {
        const AT: usize = 8192;
        const PAD: usize = 4 << 20;
        // Motion-JPEG MOV, moov at the end; a real JPEG sits at a sector start of the mdat that is not a chunk.
        let jpeg = revenant_testkit::jpeg(32, 32, 5, 0);
        // The mdat is 3 MiB and the JPEG sits in its middle, so no scan window reaches the moov.
        let jpeg_at = 484 + 512 * 3000; // file offset AT + 28 + jpeg_at: a sector start
        let mut payload = vec![0x11u8; 3 << 20];
        payload[jpeg_at..jpeg_at + jpeg.len()].copy_from_slice(&jpeg);
        let mut video = revenant_testkit::mp4_with(
            &revenant_testkit::Mp4Options {
                mdat_len: 0,
                moov_at_end: true,
                mov: true,
                mdat_64bit: false,
            },
            b"mjpa",
            &payload,
            &[0],
        );
        // Pad the moov with a 4 MiB `free` child and fix its size.
        let moov_at = 28 + payload.len();
        assert_eq!(&video[moov_at + 4..moov_at + 8], b"moov");
        let size = u32::from_be_bytes(video[moov_at..moov_at + 4].try_into().unwrap()) as usize;
        video[moov_at..moov_at + 4].copy_from_slice(&((size + PAD) as u32).to_be_bytes());
        video.extend_from_slice(&(PAD as u32).to_be_bytes());
        video.extend_from_slice(b"free");
        video.resize(video.len() + PAD - 8, 0);

        let mut image = vec![0u8; AT + video.len() + 4096];
        image[AT..AT + video.len()].copy_from_slice(&video);
        let src = Counting {
            inner: Mem(image),
            lo: (AT + moov_at) as u64,
            hi: (AT + video.len()) as u64,
            hits: AtomicUsize::new(0),
        };
        let m = IsoBmff.measure(&src, AT as u64).unwrap();
        // The JPEG cuts the video before its index: the walk that learns the file's own starts must have read the index.
        assert_eq!(m.len, 512 * 3001);
        // The three reads that overlap the index: (1) the window read for the `moov` box header, (2) the window read of the
        // header after the `moov` (its aligned window starts inside the index), (3) the single direct read of the payload.
        // Loading the payload twice would make 4.
        let hits = src.hits.load(Ordering::SeqCst);
        assert!(hits <= 3, "{hits} reads overlap the moov");
    }

    /// The top-level boxes of `bytes`: (kind, box start, box end).
    fn top_boxes(bytes: &[u8]) -> Vec<([u8; 4], usize, usize)> {
        let mut out = Vec::new();
        let mut pos = 0;
        while pos + 8 <= bytes.len() {
            let size = be32(&bytes[pos..]) as usize;
            out.push((
                [
                    bytes[pos + 4],
                    bytes[pos + 5],
                    bytes[pos + 6],
                    bytes[pos + 7],
                ],
                pos,
                pos + size,
            ));
            pos += size;
        }
        out
    }

    #[test]
    fn fragment_samples_lists_the_trun() {
        use revenant_testkit::{AvcOptions, VideoCodec, VideoLayout, camera_video};
        let opts = AvcOptions {
            samples: 30,
            seed: 9,
            mov: false,
            mdat_pad: 0,
            small_nals: true,
        };
        for codec in [VideoCodec::Avc, VideoCodec::Hevc] {
            let v = camera_video(
                &opts,
                codec,
                VideoLayout::Fragmented {
                    samples_per_fragment: 8,
                },
            );
            // The same samples, listed by the index of the non-fragmented layout.
            let flat = camera_video(&opts, codec, VideoLayout::MoovAtEnd);
            let sizes: Vec<u64> = checked(&flat).iter().map(|s| s.size).collect();
            let track = frag_track(&v.bytes[v.moov.start + 8..v.moov.end])
                .expect("a fragmented video track");
            assert_eq!(track.id, 1);
            let boxes = top_boxes(&v.bytes);
            let mut listed = Vec::new();
            let mut seqs = Vec::new();
            let mut chain = None;
            for (i, &(kind, at, end)) in boxes.iter().enumerate() {
                if &kind != b"moof" {
                    continue;
                }
                let f = fragment_samples(
                    at as u64,
                    &v.bytes[at + 8..end],
                    &track,
                    &mut u64::MAX.clone(),
                )
                .expect("a moof");
                assert!(f.has_track);
                // The testkit's decode times: 1000 per sample, fragments of 8.
                assert_eq!(f.start_time, Some(seqs.len() as u64 * 8 * 1000));
                chain = Some(Chain::follows(chain, &f, at as u64).expect("in sequence and time"));
                seqs.push(f.seq.unwrap());
                let got = f.samples;
                // Back to back from the payload of the `mdat` right after it.
                let (next, mdat_at, mdat_end) = boxes[i + 1];
                assert_eq!(&next, b"mdat");
                assert_eq!(got[0].offset, mdat_at as u64 + 8);
                assert!(
                    got.windows(2)
                        .all(|w| w[0].offset + w[0].size == w[1].offset)
                );
                let last = got.last().unwrap();
                assert_eq!(last.offset + last.size, mdat_end as u64);
                assert!(got.iter().all(|s| s.check == track.check));
                listed.extend(got.iter().map(|s| s.size));
            }
            assert_eq!(listed, sizes, "{codec:?}");
            assert_eq!(seqs, (1..=seqs.len() as u32).collect::<Vec<_>>());
            // Each one passes the sample check in place.
            let bytes = &v.bytes;
            let mut read = |at: u64, buf: &mut [u8]| {
                let at = at as usize;
                match bytes.get(at..at + buf.len()) {
                    Some(b) => {
                        buf.copy_from_slice(b);
                        true
                    }
                    None => false,
                }
            };
            for &(kind, at, end) in &boxes {
                if &kind == b"moof" {
                    let f = fragment_samples(
                        at as u64,
                        &bytes[at + 8..end],
                        &track,
                        &mut u64::MAX.clone(),
                    );
                    for s in f.unwrap().samples {
                        assert!(check_sample(&mut read, &s, &mut u64::MAX.clone()));
                    }
                }
            }
        }
    }

    /// A `moof` payload: one `traf` with a `tfhd` (`tf_flags`, track 1, then `tfhd_tail`) and a `trun` (`tr_flags`,
    /// `count`, then `trun_tail`).
    fn moof(
        tf_flags: u32,
        tfhd_tail: &[u8],
        tr_flags: u32,
        count: u32,
        trun_tail: &[u8],
    ) -> Vec<u8> {
        let bx = |kind: &[u8; 4], body: &[u8]| {
            let mut b = ((body.len() + 8) as u32).to_be_bytes().to_vec();
            b.extend_from_slice(kind);
            b.extend_from_slice(body);
            b
        };
        let mut tfhd = tf_flags.to_be_bytes().to_vec();
        tfhd.extend_from_slice(&1u32.to_be_bytes());
        tfhd.extend_from_slice(tfhd_tail);
        let mut trun = tr_flags.to_be_bytes().to_vec();
        trun.extend_from_slice(&count.to_be_bytes());
        trun.extend_from_slice(trun_tail);
        let traf = bx(b"traf", &[bx(b"tfhd", &tfhd), bx(b"trun", &trun)].concat());
        [bx(b"mfhd", &[0, 0, 0, 0, 0, 0, 0, 1]), traf].concat()
    }

    #[test]
    fn fragment_samples_rejects_garbled_moofs() {
        let track = FragTrack {
            id: 1,
            check: Check::Nal {
                length_size: 4,
                hevc: false,
            },
            default_size: 0,
            default_duration: 0,
        };
        let fragment_samples = |at: u64, moof: &[u8], track: &FragTrack| {
            fragment_samples(at, moof, track, &mut u64::MAX.clone()).map(|f| f.samples)
        };
        // Sizes present, data offset 100: two samples of 10 and 20 bytes from the moof start.
        let sizes = [
            100i32.to_be_bytes(),
            10u32.to_be_bytes(),
            20u32.to_be_bytes(),
        ]
        .concat();
        let good = moof(0x02_0000, &[], 0x201, 2, &sizes);
        let got = fragment_samples(1000, &good, &track).unwrap();
        assert_eq!(
            got.iter().map(|s| (s.offset, s.size)).collect::<Vec<_>>(),
            [(1100, 10), (1110, 20)]
        );
        // The trun lists more samples than it holds.
        assert!(fragment_samples(1000, &moof(0x02_0000, &[], 0x201, 3, &sizes), &track).is_none());
        assert!(
            fragment_samples(1000, &moof(0x02_0000, &[], 0x201, u32::MAX, &sizes), &track)
                .is_none()
        );
        // A count past MAX_SAMPLES with no per-sample field (the default size).
        let flat = moof(
            0x02_0010,
            &16u32.to_be_bytes(),
            0x001,
            MAX_SAMPLES as u32 + 1,
            &0i32.to_be_bytes(),
        );
        assert!(fragment_samples(1000, &flat, &track).is_none());
        // A base data offset near the end of u64: the data offset or the sample sizes run past it.
        let base = [(u64::MAX - 15).to_be_bytes().as_slice()].concat();
        assert!(fragment_samples(0, &moof(0x01, &base, 0x201, 2, &sizes), &track).is_none());
        // A negative data offset before the file start.
        let back = [
            (-2000i32).to_be_bytes(),
            10u32.to_be_bytes(),
            20u32.to_be_bytes(),
        ]
        .concat();
        assert!(fragment_samples(1000, &moof(0x02_0000, &[], 0x201, 2, &back), &track).is_none());
        // A truncated tfhd (base data offset flag without the field), a box size past the payload, no trun count.
        assert!(fragment_samples(1000, &moof(0x01, &[1, 2], 0x201, 2, &sizes), &track).is_none());
        let mut bad = good.clone();
        bad[16..20].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(fragment_samples(1000, &bad, &track).is_none());
        assert!(fragment_samples(1000, &good[..good.len() - 12], &track).is_none());
        // Every truncation of a valid moof: no panic.
        for n in 0..good.len() {
            let _ = fragment_samples(1000, &good[..n], &track);
        }
    }

    #[test]
    fn fragment_samples_charges_every_listed_sample() {
        // IMPORTANT 1 of the Task 6 review: samples of another track and empty samples cost work too.
        let track = FragTrack {
            id: 1,
            check: Check::Nal {
                length_size: 4,
                hevc: false,
            },
            default_size: 0,
            default_duration: 0,
        };
        let bx = |kind: &[u8; 4], body: &[u8]| {
            let mut b = ((body.len() + 8) as u32).to_be_bytes().to_vec();
            b.extend_from_slice(kind);
            b.extend_from_slice(body);
            b
        };
        let traf = |id: u32, count: u32| {
            let mut tfhd = 0x02_0000u32.to_be_bytes().to_vec();
            tfhd.extend_from_slice(&id.to_be_bytes());
            let mut trun = 0u32.to_be_bytes().to_vec();
            trun.extend_from_slice(&count.to_be_bytes());
            bx(b"traf", &[bx(b"tfhd", &tfhd), bx(b"trun", &trun)].concat())
        };
        let moof = [
            bx(b"mfhd", &[0, 0, 0, 0, 0, 0, 0, 7]),
            traf(2, 1000),
            traf(1, 10),
        ]
        .concat();
        let mut steps = 5000;
        let f = fragment_samples(0, &moof, &track, &mut steps).unwrap();
        assert_eq!((f.samples.len(), f.seq, steps), (10, Some(7), 5000 - 1010));
        let mut steps = 500;
        assert!(fragment_samples(0, &moof, &track, &mut steps).is_none());
        assert_eq!(steps, 0);
        // An empty NAL sample passes, for one step.
        let empty = CheckedSample {
            offset: 0,
            size: 0,
            check: track.check,
        };
        let mut read = |_: u64, _: &mut [u8]| true;
        let mut steps = 1;
        assert!(check_sample(&mut read, &empty, &mut steps));
        assert_eq!(steps, 0);
        assert!(!check_sample(&mut read, &empty, &mut steps));
    }

    #[test]
    fn fragments_follow_by_sequence_and_time() {
        let f = |seq, start_time, duration| MovieFragment {
            seq: Some(seq),
            samples: Vec::new(),
            start_time,
            duration,
            has_track: true,
        };
        assert_eq!(
            Chain::follows(None, &f(5, Some(100), 40), 9),
            None,
            "a later fragment first"
        );
        let a = Chain::follows(None, &f(1, Some(100), 40), 9).unwrap();
        assert_eq!(
            a,
            Chain {
                seq: 1,
                time: Some(140),
                at: 9
            }
        );
        assert_eq!(
            Chain::follows(Some(a), &f(2, Some(140), 40), 99),
            Some(Chain {
                seq: 2,
                time: Some(180),
                at: 99
            })
        );
        // Without a tfdt the time goes on from the durations.
        assert_eq!(
            Chain::follows(Some(a), &f(2, None, 10), 99),
            Some(Chain {
                seq: 2,
                time: Some(150),
                at: 99
            })
        );
        assert_eq!(
            Chain::follows(Some(a), &f(1, Some(140), 40), 99),
            None,
            "another clip's start"
        );
        assert_eq!(
            Chain::follows(Some(a), &f(3, Some(140), 40), 99),
            None,
            "a fragment skipped"
        );
        assert_eq!(
            Chain::follows(Some(a), &f(2, Some(120), 40), 99),
            None,
            "out of time"
        );
        let mut other = f(2, Some(140), 40);
        other.has_track = false;
        // A `moof` of another track only: the sequence goes on, the time stays.
        assert_eq!(
            Chain::follows(Some(a), &other, 99),
            Some(Chain {
                seq: 2,
                time: Some(140),
                at: 99
            }),
            "a moof per track"
        );
        other = f(1, Some(140), 40);
        other.seq = None;
        assert_eq!(Chain::follows(None, &other, 9), None, "no mfhd");
        let last = Chain {
            seq: u32::MAX,
            time: None,
            at: 9,
        };
        assert_eq!(
            Chain::follows(Some(last), &f(0, None, 0), 99),
            None,
            "no wrap"
        );
    }
}
