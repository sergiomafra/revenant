//! RAW seams for the reassembly (D13, ruling Q45): a TIFF-based RAW (ARW, CR2, NEF, DNG, ORF, RW2, generic TIFF) walked
//! across the pieces of a fragmented file.
//!
//! The file is a list of units in file order, from the TIFF structure read over the view (the pieces so far and a
//! candidate run; bytes past the view read as zeros, so a structure not yet in view simply is not there):
//!
//! - the IFD entry tables (the structure must read without a structural error);
//! - the embedded JPEGs (previews, thumbnails, lossy JPEG strips), decoded by the 1.3 entropy decoder when it covers
//!   the frame (strictly otherwise), across any seam inside them;
//! - the lossless-JPEG tiles and strips (`Compression 7` with SOF3), whose difference codes are walked to their
//!   `FF D9`, which must end exactly at the tile's declared end with the exact sample count (Q15), when the first
//!   tile's frame is one [`lossless_frame`] covers (else every tile is blind).
//!
//! Everything else is blind: uncompressed or vendor-coded sensor data, data in neither list (CR2 slices are
//! `Compression 6`, ruling C6), padding, value blocks. A blind stretch is walked over without reading it and gives no
//! evidence either way; it never breaks.
//!
//! The state is the logical position of the end of the last unit that passed (0 before the first): a walk resumes only
//! there, so a unit is always checked from its start. A walk breaks right at a seam when a unit holding it fails within
//! [`EVIDENCE`] bytes after it (the candidate is dropped), and has evidence past a seam once a unit holding or following
//! it decodes [`EVIDENCE`] bytes past it or passes whole.
//!
//! Proof (ruling Q45): `Proven` only when every seam lies exactly between two units that passed whole and are anchored
//! at their declared places (an IFD table; a tile or JPEG whose `FF D9` ends exactly at its declared end). A seam inside
//! a tile is never proven (Q44: a same-frame foreign piece passes the lossless check a few percent of the time), nor
//! one inside a preview (Q37: a burst neighbour's preview is near-identical), nor one in blind data.

use crate::EngineError;
use crate::formats::jpeg::{decode_health, preview_health, walk_header};
use crate::formats::jpeg_entropy::{self, End};
use crate::formats::jpeg_lossless::{LlCursor, decode_lossless, lossless_frame};
use crate::formats::{Fragment, Health, Reader, SECTOR, cancelled};
use crate::source::{DiskId, Source, VolumeId};
use crate::tiff::{self, TIFF_MAX_LEN};

use super::{Proof, Refutes, Seams, WalkEnd, Walked};

/// Bytes the TIFF walk's reader loads at a time (the structure is a few small tables and arrays, read once per walk).
const TIFF_WINDOW: usize = SECTOR as usize;
/// Bytes a unit check's reader loads at a time.
const UNIT_WINDOW: usize = 4 << 10;
/// Bytes a unit holding (or following) a seam must decode past it, without error, for evidence about it: a failure
/// within them breaks the walk right at the seam. Foreign data hits a marker that cannot be there within a few hundred
/// bytes on average.
const EVIDENCE: u64 = 4096;
/// How far before the failing code's byte a seam may still be the cause (a code read across it).
const LOOKAHEAD: u64 = 16;
/// Longest lossless-JPEG header (SOI to the end of SOS) a view may cut: a shorter view cannot tell yet.
const HEADER_MAX: u64 = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Ifd,
    Jpeg,
    Tile,
}

/// A checked unit, `[start, end)` logical.
#[derive(Clone, Copy, Debug)]
struct Unit {
    kind: Kind,
    start: u64,
    end: u64,
}

/// The units of a view, from its TIFF structure.
struct Layout {
    units: Vec<Unit>,
    /// A structural error, or a range past the extent: the view's bytes are not this file's structure.
    broken: bool,
    /// A cap hit (or any other trouble the TIFF walk flags): the structure is not wholly known, nothing is proven.
    incomplete: bool,
    /// The embedded JPEGs, `(offset, len)` (the RAW's own check, as the scan's measure does it).
    jpegs: Vec<(u64, u64)>,
}

/// How a unit's check ended.
enum Check {
    /// Whole and passed; `anchored` when its end is exactly its declared end.
    Pass { anchored: bool },
    /// It goes on past the walk's limit; `evident` when what is in view decodes.
    Open { evident: bool },
    /// Its bytes stop being this unit's at `at`.
    Fail { at: u64 },
}

/// A view padded with zeros up to `len` (reads past the view: nothing there yet).
struct Padded<'a> {
    inner: &'a dyn Source,
    len: u64,
}

impl Source for Padded<'_> {
    fn len(&self) -> u64 {
        self.len
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, EngineError> {
        let inner = self.inner.len();
        let want = buf.len().min(self.len.saturating_sub(offset) as usize);
        let mut done = 0;
        if offset < inner {
            let n = want.min((inner - offset) as usize);
            done = self.inner.read_at(offset, &mut buf[..n])?;
            if done < n {
                return Ok(done);
            }
        }
        buf[done..want].fill(0);
        Ok(want)
    }

    fn volume(&self) -> Option<VolumeId> {
        self.inner.volume()
    }

    fn disk(&self) -> Option<DiskId> {
        self.inner.disk()
    }
}

/// Where a walk resumes: right after a unit that passed (`cursor` empty), or inside the tile starting at `pos`, at a row
/// of its samples.
#[derive(Clone, Debug)]
pub(crate) struct RawState {
    pos: u64,
    cursor: Option<LlCursor>,
}

impl RawState {
    fn at(pos: u64) -> RawState {
        RawState { pos, cursor: None }
    }
}

/// A TIFF-based RAW's content as the reassembly walks it.
pub(crate) struct RawSeams {
    /// The TIFF extent (the furthest byte the structure of the first piece references): the known length.
    len: u64,
    /// The previews and JPEG-coded strips/tiles of the first piece's structure, logical (D10).
    own: Vec<Fragment>,
    /// The units of the last view walked: the prefilter reads the unit at the seam from them.
    last: Vec<Unit>,
    /// The re-check's hint for the last solution asked about: its length and the cut positions.
    hints: Option<(u64, Vec<u64>)>,
}

impl RawSeams {
    /// The RAW whose TIFF header is at `start`: its extent and own files from the structure read contiguously (the
    /// first piece). `None` when that structure is malformed or runs past the source.
    pub(crate) fn new(source: &dyn Source, start: u64) -> Option<RawSeams> {
        let mut r = Reader::with_window(source, TIFF_WINDOW);
        let available = source.len().saturating_sub(start);
        let info = tiff::walk(&mut r, start, available.min(TIFF_MAX_LEN))?;
        if info.structural || info.starts_past_cap || info.past_limit || info.end == 0 {
            return None;
        }
        let coded: std::collections::HashSet<u64> = info.jpeg_data_starts.iter().copied().collect();
        let own = info
            .jpegs
            .iter()
            .chain(info.data.iter().filter(|(o, _)| coded.contains(o)))
            .map(|&(start, len)| Fragment { start, len })
            .collect();
        Some(RawSeams {
            len: info.end,
            own,
            last: Vec::new(),
            hints: None,
        })
    }

    /// The units of `view`, from its TIFF structure (bytes past the view read as zeros).
    fn layout(&self, view: &dyn Source) -> Layout {
        let padded = Padded {
            inner: view,
            len: self.len.max(view.len()),
        };
        let mut r = Reader::with_window(&padded, TIFF_WINDOW);
        let Some(info) = tiff::walk(&mut r, 0, self.len.min(TIFF_MAX_LEN)) else {
            return Layout {
                units: Vec::new(),
                broken: true,
                incomplete: true,
                jpegs: Vec::new(),
            };
        };
        // A range past the first piece's extent: the view's structure is not the one the file started with.
        let broken = info.structural || info.starts_past_cap || info.past_limit;
        let incomplete = info.garbled || info.truncated;
        let mut units: Vec<Unit> = info
            .ifds
            .iter()
            .map(|&(o, l)| (Kind::Ifd, o, l))
            .chain(info.jpegs.iter().map(|&(o, l)| (Kind::Jpeg, o, l)))
            .chain(tiles_of(view, &info.lossless).map(|(o, l)| (Kind::Tile, o, l)))
            .filter(|&(_, _, l)| l > 0)
            .map(|(kind, start, len)| Unit {
                kind,
                start,
                end: start.saturating_add(len),
            })
            .filter(|u| u.end <= self.len)
            .collect();
        units.sort_by_key(|u| (u.start, u.end));
        // Overlapping ranges (a hostile structure): the first one in file order is checked, the others are not units.
        let mut end = 0;
        units.retain(|u| {
            let keep = u.start >= end;
            if keep {
                end = u.end;
            }
            keep
        });
        Layout {
            units,
            broken,
            incomplete,
            jpegs: info.jpegs,
        }
    }

    /// Walks the units of `view` from `from` up to `limit`; the `bool` is whether a unit's content failed.
    fn run(
        &mut self,
        view: &dyn Source,
        seams: &[u64],
        from: &RawState,
        limit: u64,
    ) -> (Walked<RawState>, bool) {
        let limit = limit.min(view.len());
        let layout = self.layout(view);
        self.last = layout.units.clone();
        let mut checked = from.pos;
        let mut resume = Vec::new();
        if layout.broken {
            // The newest piece's bytes are not the structure: broken at its seam.
            let at = seams
                .iter()
                .rev()
                .find(|&&s| s >= from.pos)
                .copied()
                .unwrap_or(from.pos);
            let end = WalkEnd::Broken { at };
            return (
                Walked {
                    end,
                    checked,
                    resume,
                },
                true,
            );
        }
        let mut r = Reader::with_window(view, UNIT_WINDOW);
        let newest = |lo: u64, hi: u64| seams.iter().rev().find(|&&s| s >= lo && s <= hi).copied();
        for u in layout.units.iter().filter(|u| u.start >= from.pos) {
            if u.start >= limit {
                break;
            }
            if cancelled() {
                let end = WalkEnd::Stopped;
                return (
                    Walked {
                        end,
                        checked,
                        resume,
                    },
                    false,
                );
            }
            // A tile the walk resumes inside, at a row of its samples.
            let at_row = from.cursor.clone().filter(|_| u.start == from.pos);
            let mut rows = Vec::new();
            let got = match u.kind {
                Kind::Tile => check_tile(&mut r, u, limit, at_row, &mut rows),
                _ => check(&mut r, u, limit),
            };
            // Resume points inside a tile: each row of samples decoded (a later walk, the re-check's too, continues from
            // the row before its seam instead of the tile's start).
            let mut rows_of = |resume: &mut Vec<(u64, RawState)>| {
                for c in rows.drain(..).filter(|c| c.pos > u.start) {
                    let pos = c.pos;
                    let state = RawState {
                        pos: u.start,
                        cursor: Some(c),
                    };
                    resume.push((pos, state));
                }
            };
            // A JPEG holding a seam must end exactly at its declared end: a burst neighbour's tail decodes on, to its own.
            let seamed = u.kind == Kind::Jpeg && newest(u.start + 1, u.end - 1).is_some();
            match got {
                Check::Pass { anchored: false } if seamed => {
                    let end = WalkEnd::Broken {
                        at: newest(u.start + 1, u.end - 1).unwrap_or(u.start),
                    };
                    return (
                        Walked {
                            end,
                            checked,
                            resume,
                        },
                        true,
                    );
                }
                Check::Pass { .. } => {
                    rows_of(&mut resume);
                    checked = u.end;
                    resume.push((u.end, RawState::at(u.end)));
                }
                Check::Fail { at } => {
                    let at = at.clamp(u.start, limit);
                    let end = match newest(u.start, at.saturating_add(LOOKAHEAD)) {
                        // Right after a seam: the continuation is not this unit's.
                        Some(s) if at < s.saturating_add(EVIDENCE) => {
                            return (
                                Walked {
                                    end: WalkEnd::Broken { at: s },
                                    checked,
                                    resume,
                                },
                                true,
                            );
                        }
                        // Evidence past the seam (the next seam lies further on, before the failure); the break points
                        // start a little after it, never at the rows past it.
                        Some(s) => {
                            checked = checked.max(s + EVIDENCE);
                            WalkEnd::Broken {
                                at: (at + LOOKAHEAD).min(limit),
                            }
                        }
                        None => WalkEnd::Broken {
                            at: (at + LOOKAHEAD).min(limit),
                        },
                    };
                    rows_of(&mut resume);
                    return (
                        Walked {
                            end,
                            checked,
                            resume,
                        },
                        true,
                    );
                }
                Check::Open { evident } => {
                    let base = newest(u.start, limit).unwrap_or(u.start);
                    if evident && limit >= base.saturating_add(EVIDENCE) {
                        checked = checked.max(base + EVIDENCE);
                    }
                    rows_of(&mut resume);
                    let end = WalkEnd::Broken { at: limit };
                    return (
                        Walked {
                            end,
                            checked,
                            resume,
                        },
                        false,
                    );
                }
            }
        }
        let end = if limit >= self.len {
            WalkEnd::Complete { len: self.len }
        } else {
            WalkEnd::Broken { at: limit }
        };
        (
            Walked {
                end,
                checked,
                resume,
            },
            false,
        )
    }

    /// The RAW's own check of the whole file over `view` (D6), as the scan's measure: the structure whole and the
    /// largest preview decoding strictly ([`preview_health`]).
    pub(crate) fn own_ok(&mut self, view: &dyn Source) -> bool {
        let layout = self.layout(view);
        !layout.broken
            && !layout.incomplete
            && preview_health(&mut Reader::new(view), &layout.jpegs) == Health::Intact
    }
}

/// The lossless tiles of `view` worth checking: all of them when the first one's frame is one [`lossless_frame`]
/// covers (as far as the view holds it), else none (they are blind).
fn tiles_of<'a>(
    view: &dyn Source,
    lossless: &'a [(u64, u64)],
) -> impl Iterator<Item = (u64, u64)> + 'a {
    let covered = lossless.first().is_some_and(|&(o, l)| {
        let to = o.saturating_add(l).min(view.len());
        lossless_frame(&mut Reader::with_window(view, TIFF_WINDOW), o, to).is_some()
    });
    lossless.iter().copied().filter(move |_| covered)
}

/// The check of unit `u` over the bytes below `limit`.
fn check(r: &mut Reader, u: &Unit, limit: u64) -> Check {
    let whole = u.end <= limit;
    let to = u.end.min(limit);
    match u.kind {
        Kind::Ifd if whole => Check::Pass { anchored: true },
        Kind::Ifd => Check::Open { evident: false },
        Kind::Jpeg => check_jpeg(r, u, to, whole),
        Kind::Tile => check_tile(r, u, limit, None, &mut Vec::new()),
    }
}

fn check_jpeg(r: &mut Reader, u: &Unit, to: u64, whole: bool) -> Check {
    let header = walk_header(r, u.start, to - u.start);
    let frame = header
        .as_ref()
        .and_then(|h| Some((jpeg_entropy::frame(r, u.start, h.scan_start)?, h.scan_start)));
    let Some((frame, scan)) = frame else {
        return match (whole, header.is_some()) {
            (false, _) => Check::Open { evident: false },
            // A frame the entropy decoder does not cover: the strict decode decides.
            (true, true) if decode_health(r, u.start, u.end - u.start) == Health::Intact => {
                Check::Pass { anchored: false }
            }
            (true, _) => Check::Fail { at: u.start },
        };
    };
    match jpeg_entropy::decode(r, &frame, frame.start(scan), to, false).end {
        End::Eoi { at } => Check::Pass {
            anchored: at + 2 == u.end,
        },
        End::Error { at } => Check::Fail { at },
        End::OutOfData if whole => Check::Fail { at: to },
        End::OutOfData => Check::Open { evident: true },
    }
}

/// The check of tile `u` over the bytes below `limit`, from its start or from `row` (a cursor at a row of its samples);
/// the cursors at the rows it decodes go to `rows`.
fn check_tile(
    r: &mut Reader,
    u: &Unit,
    limit: u64,
    row: Option<LlCursor>,
    rows: &mut Vec<LlCursor>,
) -> Check {
    let whole = u.end <= limit;
    let to = u.end.min(limit);
    let Some((frame, scan)) = lossless_frame(r, u.start, to) else {
        return if !whole && to - u.start < HEADER_MAX {
            Check::Open { evident: false }
        } else {
            Check::Fail { at: u.start }
        };
    };
    let from = row.unwrap_or_else(|| frame.start(scan));
    let run = decode_lossless(r, &frame, from, to);
    *rows = run.resume;
    match run.end {
        // Only at the declared end, with the exact count (Q15).
        End::Eoi { .. } => Check::Pass { anchored: true },
        End::Error { at } => Check::Fail { at },
        End::OutOfData if whole => Check::Fail { at: to },
        End::OutOfData => Check::Open { evident: true },
    }
}

/// Whether the `n` bytes at absolute `g` may be entropy-coded data (inside a tile): readable, not one repeated byte
/// value (erased), and no `FF` followed by anything but `00` (stuffing), `D0`–`D7` (restart), `D9` (end) or `FF` (fill).
fn may_be_entropy(r: &mut Reader, g: u64, n: u64) -> bool {
    let n = n.min(SECTOR) as usize;
    if n < 2 {
        return true;
    }
    let mut buf = [0u8; SECTOR as usize];
    let buf = &mut buf[..n];
    if r.read(g, buf) < n || r.bad_in(g, g + n as u64) {
        return false;
    }
    if buf[1..] == buf[..n - 1] {
        return false;
    }
    buf.windows(2)
        .all(|w| w[0] != 0xFF || matches!(w[1], 0x00 | 0xD0..=0xD7 | 0xD9 | 0xFF))
}

impl Seams for RawSeams {
    type State = RawState;

    fn begin(&mut self) -> RawState {
        RawState::at(0)
    }

    /// The units from `from` in file order, each one checked over the view: a resume point after each that passes
    /// whole; `Broken` right at a seam when a unit holding it fails within [`EVIDENCE`] bytes after it, else where it
    /// fails (or where the view ends: no evidence either way inside blind data); `Complete` when every unit passes and
    /// the view reaches the known length.
    fn walk(
        &mut self,
        view: &dyn Source,
        seams: &[u64],
        from: &RawState,
        limit: u64,
    ) -> Walked<RawState> {
        self.run(view, seams, from, limit).0
    }

    /// `Proven` iff the structure reads whole over the view, every unit passes, and every seam lies exactly between a
    /// unit that passed whole and ends there anchored and one that passed whole and starts there anchored (Q45);
    /// `Failed` when a unit fails or the structure is malformed; `Unproven` otherwise (a seam inside a unit or in blind
    /// data, a cap hit).
    fn prove(&mut self, view: &dyn Source, seams: &[u64]) -> Proof {
        let layout = self.layout(view);
        if layout.broken {
            return Proof::Failed;
        }
        let len = view.len();
        let mut r = Reader::with_window(view, UNIT_WINDOW);
        let mut anchored: Vec<Unit> = Vec::new();
        for u in &layout.units {
            if cancelled() {
                return Proof::Unproven;
            }
            match check(&mut r, u, len) {
                Check::Pass { anchored: true } => anchored.push(*u),
                Check::Pass { anchored: false } => {}
                Check::Open { .. } => return Proof::Unproven,
                Check::Fail { .. } => return Proof::Failed,
            }
        }
        if layout.incomplete || len != self.len {
            return Proof::Unproven;
        }
        let between =
            |s: u64| anchored.iter().any(|u| u.end == s) && anchored.iter().any(|u| u.start == s);
        if seams.iter().all(|&s| between(s)) {
            Proof::Proven
        } else {
            Proof::Unproven
        }
    }

    fn known_len(&self) -> Option<u64> {
        Some(self.len)
    }

    /// Most RAW seams cannot be proven (sensor data, inside a tile or a preview): once the first solution is found and
    /// is not `Proven`, nothing the rest of the search could find would change the result.
    fn settles_unproven(&self) -> bool {
        true
    }

    /// The previews and JPEG-coded strips/tiles (D10): found as photos of their own in a later piece, they neither stop
    /// a free run nor a candidate when they lie exactly where the structure puts them (a coverage test, Q7).
    fn own(&self) -> Vec<Fragment> {
        self.own.clone()
    }

    /// The unit at the seam (Q1), from the structure of the last walk (the piece before the seam): a tile starting
    /// there must start with a lossless-JPEG header, a JPEG with `FF D8 FF`; a seam inside a tile needs entropy-coded
    /// data up to the tile's end within the first sector ([`may_be_entropy`]). Anything else is kept (blind, an IFD, a
    /// preview's inside). The answer depends on the structure, which lies in the first piece of every RAW seen so far:
    /// it is remembered per (candidate, seam).
    fn prefilter(&mut self, r: &mut Reader, g: u64, seam: u64) -> bool {
        let Some(u) = self.last.iter().find(|u| u.start <= seam && seam < u.end) else {
            return true;
        };
        match u.kind {
            Kind::Tile if u.start == seam => {
                let to = g.saturating_add((u.end - u.start).min(HEADER_MAX));
                lossless_frame(r, g, to).is_some() && !r.bad_in(g, to)
            }
            Kind::Jpeg if u.start == seam => {
                let mut head = [0u8; 3];
                r.read(g, &mut head) == 3 && head == [0xFF, 0xD8, 0xFF]
            }
            Kind::Tile => may_be_entropy(r, g, u.end - seam),
            _ => true,
        }
    }

    /// The boundaries inside `(start, end)` where one unit ends and the next starts (a tile boundary): the only places
    /// where another cut could be proven too (Q28), nearest the piece's end first. The structure of the whole view is
    /// read once (the re-check asks for its pieces in a row).
    fn suspicious_breaks(&mut self, view: &dyn Source, start: u64, end: u64) -> Vec<u64> {
        let len = view.len();
        if self.hints.as_ref().is_none_or(|h| h.0 != len) {
            let units = self.layout(view).units;
            let cuts = units
                .windows(2)
                .filter(|w| w[0].end == w[1].start)
                .map(|w| w[0].end)
                .collect();
            self.hints = Some((len, cuts));
        }
        let Some((_, cuts)) = &self.hints else {
            return Vec::new();
        };
        cuts.iter()
            .rev()
            .copied()
            .filter(|&c| start < c && c < end)
            .collect()
    }
}

impl Refutes for RawSeams {
    /// A unit of the placement fails by content, or its structure is malformed (blind data refutes nothing).
    fn refutes(&mut self, view: &dyn Source) -> bool {
        let len = view.len();
        self.run(view, &[], &RawState::at(0), len).1
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Budget, Grid, Limits, Space, Taken, search};
    use super::*;
    use crate::cache::Runs;
    use crate::formats::tests::Mem;
    use crate::scan::CancelToken;
    use crate::source::Fragmented;
    use revenant_testkit::{ArwCompression, ArwOptions, TiffRaw, arw};

    const C: u64 = 32 << 10;
    const HEAP: u64 = 1 << 20;

    fn lossless(seed: u64) -> TiffRaw {
        arw(&ArwOptions {
            seed,
            ..ArwOptions::default()
        })
    }

    /// Tiles 2, 5 and 8 end on a 32 KiB grid.
    fn aligned(seed: u64) -> TiffRaw {
        arw(&ArwOptions {
            seed,
            align: C as u32,
            aligned_tiles: vec![2, 5, 8],
            ..ArwOptions::default()
        })
    }

    fn uncompressed(seed: u64) -> TiffRaw {
        arw(&ArwOptions {
            compression: ArwCompression::Uncompressed,
            seed,
            ..ArwOptions::default()
        })
    }

    fn seams_of(bytes: &[u8]) -> RawSeams {
        RawSeams::new(&Mem(bytes.to_vec()), 0).expect("a TIFF")
    }

    fn noise(len: usize, seed: u64) -> Vec<u8> {
        let mut x = seed;
        (0..len)
            .map(|_| {
                x = x
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (x >> 56) as u8
            })
            .collect()
    }

    /// The proof of `bytes` as one view with `seams` (where the pieces would meet).
    fn proof(bytes: &[u8], seams: &[u64]) -> Proof {
        seams_of(bytes).prove(&Mem(bytes.to_vec()), seams)
    }

    #[test]
    fn known_length_and_own_files_come_from_the_structure() {
        let a = arw(&ArwOptions {
            thumbnail: true,
            ..ArwOptions::default()
        });
        let s = seams_of(&a.bytes);
        assert_eq!(s.known_len(), Some(a.bytes.len() as u64));
        let mut own = s.own();
        own.sort_by_key(|f| f.start);
        let (t, p) = (a.thumbnail.unwrap(), a.preview.unwrap());
        // The previews, then the JPEG-coded tiles (which begin with FF D8 too).
        assert_eq!(
            own[0],
            Fragment {
                start: t.start as u64,
                len: t.len() as u64
            }
        );
        assert_eq!(
            own[1],
            Fragment {
                start: p.start as u64,
                len: p.len() as u64
            }
        );
        assert_eq!(own.len(), 2 + a.tiles.len());
        assert!(RawSeams::new(&Mem(vec![0; 4096]), 0).is_none());
    }

    #[test]
    fn a_seam_is_proven_only_between_two_anchored_tiles() {
        let a = aligned(1);
        let t = &a.tiles;
        let at_tile = t[2].end as u64;
        assert_eq!(at_tile % C, 0);
        assert_eq!(proof(&a.bytes, &[]), Proof::Proven);
        assert_eq!(proof(&a.bytes, &[at_tile]), Proof::Proven);
        assert_eq!(
            proof(
                &a.bytes,
                &[t[2].end as u64, t[5].end as u64, t[8].end as u64]
            ),
            Proof::Proven
        );
        // Inside a tile (Q44), inside the preview (Q37), between the preview and the padding before the tiles: never.
        let p = a.preview.clone().unwrap();
        for s in [
            t[4].start as u64 + 100,
            (p.start + p.len() / 2) as u64,
            p.end as u64,
        ] {
            assert_eq!(
                proof(&a.bytes, &[at_tile, s]),
                Proof::Unproven,
                "seam at {s}"
            );
        }
        // Uncompressed sensor data is blind.
        let u = uncompressed(1);
        assert_eq!(proof(&u.bytes, &[u.raw.start as u64 + C]), Proof::Unproven);
        // A foreign tile at a boundary (another seed, the same frame): it does not end at the declared end.
        let b = aligned(2);
        let mut glued = a.bytes.clone();
        let k = 3;
        let n = t[k].len().min(b.tiles[k].len());
        glued[t[k].start..t[k].start + n].copy_from_slice(&b.bytes[b.tiles[k].start..][..n]);
        assert_eq!(proof(&glued, &[t[k].start as u64]), Proof::Failed);
    }

    /// The RAW `bytes` cut at `cut`, then `tail` in a second piece, as one view.
    fn spliced(bytes: &[u8], cut: usize, tail: &[u8]) -> (Mem, Vec<Fragment>) {
        let mut card = bytes[..cut].to_vec();
        card.extend_from_slice(tail);
        let pieces = vec![
            Fragment {
                start: 0,
                len: cut as u64,
            },
            Fragment {
                start: cut as u64,
                len: tail.len() as u64,
            },
        ];
        (Mem(card), pieces)
    }

    fn walk_spliced(bytes: &[u8], cut: usize, tail: &[u8]) -> Walked<RawState> {
        let mut s = seams_of(bytes);
        let (src, pieces) = spliced(bytes, cut, tail);
        let view = Fragmented::new(&src, pieces);
        let from = s.begin();
        let len = view.len();
        s.walk(&view, &[cut as u64], &from, len)
    }

    #[test]
    fn a_continuation_inside_a_tile_is_decoded_across_the_seam() {
        let a = lossless(1);
        let cut = 4 * C as usize;
        let t = a
            .tiles
            .iter()
            .position(|t| t.start < cut && cut < t.end)
            .unwrap();
        // The true continuation: the whole file, complete.
        let w = walk_spliced(&a.bytes, cut, &a.bytes[cut..]);
        assert!(matches!(w.end, WalkEnd::Complete { len } if len == a.bytes.len() as u64));
        assert!(w.checked > cut as u64);
        // Resume points after whole units, and at the rows inside tiles.
        assert!(
            w.resume
                .iter()
                .any(|r| r.0 == a.tiles[t].start as u64 && r.1.cursor.is_none())
        );
        assert!(
            w.resume
                .iter()
                .all(|r| r.1.cursor.is_some() || r.0 == r.1.pos)
        );
        assert!(w.resume.windows(2).all(|p| p[0].0 <= p[1].0));
        // The first piece alone stops inside the tile holding the cut: a resume point at each row it decoded.
        let mut s = seams_of(&a.bytes);
        let src = Mem(a.bytes.clone());
        let first = Fragmented::new(
            &src,
            vec![Fragment {
                start: 0,
                len: cut as u64,
            }],
        );
        let w = s.walk(&first, &[], &RawState::at(0), cut as u64);
        assert!(matches!(w.end, WalkEnd::Broken { at } if at == cut as u64));
        let tile = a.tiles[t].start as u64;
        let rows: Vec<u64> = w
            .resume
            .iter()
            .filter(|r| r.1.cursor.is_some() && r.1.pos == tile)
            .map(|r| r.0)
            .collect();
        assert!(rows.len() > 100 && rows.is_sorted(), "{}", rows.len());
        assert!(rows[0] > tile && *rows.last().unwrap() <= cut as u64);
        // A walk from the last row before the seam goes on as the one from the tile's start.
        let (src, pieces) = spliced(&a.bytes, cut, &a.bytes[cut..]);
        let view = Fragmented::new(&src, pieces);
        let st = w.resume.last().unwrap().1.clone();
        let again = s.walk(&view, &[cut as u64], &st, view.len());
        assert!(matches!(again.end, WalkEnd::Complete { .. }));
        // Noise right after the seam: broken right at it, no evidence.
        let w = walk_spliced(&a.bytes, cut, &noise(3 * C as usize, 7));
        assert!(
            matches!(w.end, WalkEnd::Broken { at } if at == cut as u64),
            "{:?}",
            w.checked
        );
        assert!(w.checked <= cut as u64);
        // A continuation cut short by the view (a whole cluster that decodes): evidence, broken at the view's end.
        let w = walk_spliced(&a.bytes, cut, &a.bytes[cut..cut + C as usize]);
        assert!(matches!(w.end, WalkEnd::Broken { at } if at == cut as u64 + C));
        assert!(w.checked > cut as u64);
    }

    #[test]
    fn blind_sensor_data_gives_no_evidence() {
        let u = uncompressed(1);
        let cut = u.raw.start + 2 * C as usize;
        let w = walk_spliced(&u.bytes, cut, &noise(C as usize, 3));
        // Not a failure (nothing can be checked there), no evidence: a blind candidate.
        assert!(matches!(w.end, WalkEnd::Broken { at } if at == (cut as u64 + C)));
        assert!(w.checked <= cut as u64);
        // Its end reached with the known length: complete.
        let w = walk_spliced(&u.bytes, cut, &noise(u.bytes.len() - cut, 3));
        assert!(matches!(w.end, WalkEnd::Complete { .. }));
    }

    #[test]
    fn a_preview_split_by_a_seam_is_decoded_across_it() {
        let u = uncompressed(1);
        let p = u.preview.clone().unwrap();
        // The first 32 KiB cluster ends inside the preview.
        let cut = C as usize;
        assert!(p.start < cut && cut < p.end);
        let w = walk_spliced(&u.bytes, cut, &u.bytes[cut..]);
        assert!(matches!(w.end, WalkEnd::Complete { .. }));
        assert!(w.checked >= p.end as u64);
        // A burst neighbour's preview tail (the same scene shifted by a pixel) at the same place: refuted.
        let b = uncompressed(2);
        let w = walk_spliced(&u.bytes, cut, &b.bytes[cut..]);
        assert!(
            matches!(w.end, WalkEnd::Broken { at } if at == cut as u64),
            "{:?}",
            w.checked
        );
    }

    #[test]
    fn prefilter_reads_the_unit_at_the_seam() {
        let a = aligned(1);
        let mut s = seams_of(&a.bytes);
        // Walk the first piece (up to a tile boundary) so the layout is known.
        let cut = a.tiles[2].end;
        let src = Mem(a.bytes.clone());
        let view = Fragmented::new(
            &src,
            vec![Fragment {
                start: 0,
                len: cut as u64,
            }],
        );
        let from = s.begin();
        let _ = s.walk(&view, &[], &from, cut as u64);
        let mut card = noise(HEAP as usize, 9);
        card.extend_from_slice(&a.bytes);
        let src = Mem(card);
        let mut r = Reader::new(&src);
        let next = HEAP + a.tiles[3].start as u64;
        assert!(s.prefilter(&mut r, next, cut as u64), "a lossless header");
        assert!(!s.prefilter(&mut r, 4096, cut as u64), "noise");
        // Inside a tile: entropy-coded data passes, a marker that cannot be there does not, nor an erased sector.
        let inside = a.tiles[4].start as u64 + 1000;
        assert!(s.prefilter(&mut r, HEAP + inside, inside));
        let mut erased = vec![0u8; 4096];
        erased.extend_from_slice(&[0xFF, 0xC4, 0, 0]);
        let src = Mem(erased);
        let mut r = Reader::new(&src);
        assert!(!s.prefilter(&mut r, 0, inside));
        assert!(!s.prefilter(&mut r, 4096 - 256, inside));
    }

    #[test]
    fn a_placement_with_foreign_tile_data_is_refuted() {
        let a = lossless(1);
        let mut s = seams_of(&a.bytes);
        assert!(!s.refutes(&Mem(a.bytes.clone())));
        let mut bad = a.bytes.clone();
        let t = a.tiles[5].clone();
        bad[t.start + 500..t.start + 500 + 4096].copy_from_slice(&noise(4096, 5));
        assert!(s.refutes(&Mem(bad)));
        // Blind sensor data refutes nothing.
        let u = uncompressed(1);
        let mut s = seams_of(&u.bytes);
        let mut other = u.bytes.clone();
        let r = u.raw.clone();
        other[r.start + 100..r.start + 4196].copy_from_slice(&noise(4096, 5));
        assert!(!s.refutes(&Mem(other)));
    }

    fn grid(card: &[u8]) -> Grid {
        (HEAP, C, card.len() as u64)
    }

    /// `bytes` cut at the cluster cuts `cuts` (in clusters of the file) into runs placed at clusters `at` of a card of
    /// noise; the runs in file order.
    fn scatter(bytes: &[u8], cuts: &[u64], at: &[u64]) -> (Vec<u8>, Vec<Fragment>) {
        let clusters = at.iter().max().unwrap() + (bytes.len() as u64).div_ceil(C) + 4;
        let mut card = noise((HEAP + clusters * C) as usize, 0x5EED);
        let mut runs = Vec::new();
        let mut from = 0u64;
        for (k, &c) in at.iter().enumerate() {
            let to = cuts.get(k).map_or(bytes.len() as u64, |&x| x * C);
            let pos = (HEAP + c * C) as usize;
            card[pos..pos + (to - from) as usize]
                .copy_from_slice(&bytes[from as usize..to as usize]);
            runs.push(Fragment {
                start: pos as u64,
                len: to - from,
            });
            from = to;
        }
        (card, runs)
    }

    /// Searches the RAW starting at `start` of `card`, the clusters right after each run but the last taken (a live
    /// file), so each first free run ends where the piece does.
    fn find(card: &[u8], runs: &[Fragment], bytes: &[u8]) -> super::super::Search {
        find_counting(card, runs, bytes).0
    }

    /// [`find`], with the walks it took.
    fn find_counting(card: &[u8], runs: &[Fragment], bytes: &[u8]) -> (super::super::Search, u64) {
        let live: Vec<Fragment> = runs[..runs.len() - 1]
            .iter()
            .map(|r| Fragment {
                start: r.start + r.len,
                len: C,
            })
            .collect();
        let src = Mem(card.to_vec());
        let mut seams = seams_of(bytes);
        let limits = Limits::DEFAULT;
        let taken = Taken::new(&live, &Runs::default());
        let grids = [grid(card)];
        let space = Space::new(&grids, runs[0].start, src.len(), &taken, &[], &limits);
        let mut budget = Budget::new(&limits);
        let found = search(
            &mut seams,
            &src,
            &space,
            runs[0].start,
            None,
            &limits,
            &mut budget,
            &CancelToken::default(),
        )
        .expect("a result");
        (found, limits.scan_tries - budget.tries_left())
    }

    #[test]
    fn tile_aligned_pieces_are_proven_and_unique() {
        let a = aligned(1);
        let cuts: Vec<u64> = [2, 5, 8]
            .iter()
            .map(|&k| a.tiles[k].end as u64 / C)
            .collect();
        let (card, runs) = scatter(&a.bytes, &cuts, &[10, 60, 30, 90]);
        let s = find(&card, &runs, &a.bytes);
        assert_eq!(s.pieces, runs);
        assert!(s.complete && s.unique && s.exhaustive, "{s:?}");
        assert_eq!(s.proof, Proof::Proven);
    }

    #[test]
    fn in_tile_pieces_are_placed_but_unproven() {
        let a = lossless(1);
        let (card, runs) = scatter(&a.bytes, &[4, 8, 12], &[10, 60, 30, 90]);
        let s = find(&card, &runs, &a.bytes);
        assert_eq!(s.pieces, runs);
        assert!(s.complete, "{s:?}");
        assert_eq!(s.proof, Proof::Unproven);
        // Blind sensor data: placed by the order of the candidates, unproven.
        let u = uncompressed(1);
        let (card, runs) = scatter(&u.bytes, &[17, 34], &[10, 28, 46]);
        let s = find(&card, &runs, &u.bytes);
        assert_eq!(s.pieces, runs);
        assert_eq!(s.proof, Proof::Unproven);
    }

    #[test]
    fn an_unproven_first_solution_settles_the_search() {
        // In-tile seams can never be proven: the first complete solution is the result, and the search stops there,
        // reporting it neither unique nor exhaustive (nothing after it could change a Partial result).
        let a = lossless(1);
        let (card, runs) = scatter(&a.bytes, &[4, 8, 12], &[10, 60, 30, 90]);
        let (s, walks) = find_counting(&card, &runs, &a.bytes);
        assert_eq!(s.pieces, runs);
        assert!(s.complete && !s.unique && !s.exhaustive, "{s:?}");
        assert!(walks < 2000, "{walks} walks");
        // A proven one is searched on and re-checked as before.
        let b = aligned(1);
        let cuts: Vec<u64> = [2, 5, 8]
            .iter()
            .map(|&k| b.tiles[k].end as u64 / C)
            .collect();
        let (card, runs) = scatter(&b.bytes, &cuts, &[10, 60, 30, 90]);
        let (s, _) = find_counting(&card, &runs, &b.bytes);
        assert!(
            s.unique && s.exhaustive && s.proof == Proof::Proven,
            "{s:?}"
        );
    }
}
