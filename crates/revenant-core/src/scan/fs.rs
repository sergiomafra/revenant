//! The file-system half of the scan: which listed files the scan reports, how each one is measured (through a view of
//! its own pieces) and how that merges with the carving at the same start, so a file found both ways comes out once
//! (rulings D3, D5, D6, D7). Pure functions of the source's bytes, so the result does not depend on who measures.

use super::pipeline::{self, Measured};
use crate::filesystem::{FsFile, FsOrigin, FsState, Listing, Placement};
use crate::formats::{FileKind, Format, Fragment, Health, Measurement};
use crate::reassemble::content_checks;
use crate::scan::{CancelToken, HEAD_LEN};
use crate::source::{Fragmented, Source};

/// Files measured one after another in the bytes between a file's structural end and its recorded size (ruling D6:
/// an MPF image, a motion photo's video).
const MAX_TAIL_FILES: usize = 8;
/// Bytes of a tail checked for padding per read.
const PAD_CHUNK: usize = 64 * 1024;

/// What the scan keeps of a file-system listing.
#[derive(Default)]
pub(crate) struct FsPlan {
    /// One photo/video per distinct first-piece start, sorted by it (see [`keep`]).
    pub kept: Vec<FsFile>,
    /// `kept[i].pieces[0].start`, ascending.
    pub starts: Vec<u64>,
    /// First-piece starts of the live files of other kinds, ascending: the head there is consumed without a finding.
    pub others: Vec<u64>,
    /// Merged, ascending ranges of every live file and trusted live directory (ruling D7).
    pub live: Vec<Fragment>,
    /// Merged, ascending ranges of every directory the listing walked (an old directory there is no orphan, D11).
    pub walked: Vec<Fragment>,
}

impl FsPlan {
    /// The plan of `listing` for a scan of `sector`-byte sectors over `total` bytes: files whose first piece is not
    /// sector-aligned (or starts past the end) are dropped, then one file is kept per start.
    pub(crate) fn new(listing: Listing, sector: u64, total: u64) -> Self {
        let files = listing
            .files
            .into_iter()
            .filter(|f| {
                f.pieces
                    .first()
                    .is_some_and(|p| p.start % sector == 0 && p.start < total)
            })
            .collect();
        let kept = keep(files);
        let starts = kept.iter().map(|f| f.pieces[0].start).collect();
        let mut others: Vec<u64> = listing.others.iter().map(|f| f.start).collect();
        others.dedup();
        FsPlan {
            kept,
            starts,
            others,
            live: listing.live,
            walked: listing.walked,
        }
    }

    /// Indexes in `kept` of the files starting in `[pos, end)`.
    pub(crate) fn kept_in(&self, pos: u64, end: u64) -> std::ops::Range<usize> {
        self.starts.partition_point(|&s| s < pos)..self.starts.partition_point(|&s| s < end)
    }

    /// Whether a live file of another kind starts at `at`.
    pub(crate) fn is_other(&self, at: u64) -> bool {
        self.others.binary_search(&at).is_ok()
    }

    /// Whether `[start, end)` overlaps a live range.
    pub(crate) fn overlaps_live(&self, start: u64, end: u64) -> bool {
        overlaps(&self.live, start, end)
    }

    /// Whether `[start, end)` overlaps a directory the listing walked.
    pub(crate) fn overlaps_walked(&self, start: u64, end: u64) -> bool {
        overlaps(&self.walked, start, end)
    }
}

/// Whether `[start, end)` overlaps one of `ranges` (sorted, disjoint).
fn overlaps(ranges: &[Fragment], start: u64, end: u64) -> bool {
    let i = ranges.partition_point(|r| r.start.saturating_add(r.len) <= start);
    ranges.get(i).is_some_and(|r| r.start < end)
}

/// The order of the files at one start: live before deleted, then the latest `modified` (a missing one loses to a
/// present one). "YYYY-MM-DDTHH:MM:SS" sorts as it reads; `None` sorts first.
fn rank_at_start(f: &FsFile) -> (bool, Option<String>) {
    (
        f.origin.state == FsState::Live,
        f.origin.modified.as_ref().map(|t| t.local.clone()),
    )
}

/// One file per distinct first-piece start, `files` sorted by it (the listing's order): the first of the highest
/// [`rank_at_start`]. When that is a deleted file and another deleted file ranks the same (deleted entries over each
/// other with equal times: an unset clock, the 2-second resolution), which one the clusters hold cannot be told, so
/// neither is kept: the head there is carved like any other, without a name (part 1.6, ruling Q52).
pub(crate) fn keep(files: Vec<FsFile>) -> Vec<FsFile> {
    let mut out: Vec<FsFile> = Vec::with_capacity(files.len());
    let mut group: Vec<FsFile> = Vec::new();
    let flush = |group: &mut Vec<FsFile>, out: &mut Vec<FsFile>| {
        let Some(best) = group.iter().map(rank_at_start).max() else {
            return;
        };
        let mut top = group.drain(..).filter(|f| rank_at_start(f) == best);
        let first = top.next().expect("the best is in the group");
        let tied = top.next().is_some();
        if !(tied && first.origin.state == FsState::Deleted) {
            out.push(first);
        }
    };
    for f in files {
        if group
            .first()
            .is_some_and(|g| g.pieces[0].start != f.pieces[0].start)
        {
            flush(&mut group, &mut out);
        }
        group.push(f);
    }
    flush(&mut group, &mut out);
    out
}

/// A file's measurement through the view of its own pieces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ViewMeasure {
    pub m: Option<Measurement>,
    /// Ruling D6: what lies between the structure's end and the recorded size is padding or whole files.
    pub tail_ok: bool,
    /// The structure ends before the recorded size and only padding (`00`/`FF`) follows (ruling Q53: for a deleted
    /// file, no proof the bytes are this entry's); a TIFF RAW's own padding to its grain is not ([`raw_padding`]).
    pub padded: bool,
    /// The structure ends exactly at the recorded size, or it is a TIFF RAW whose own padding (only `00`/`FF`) to its
    /// grain fills the rest up to it ([`raw_padding`], Q54 (b)): the content's own end is the entry's (ruling Q55).
    pub at_size: bool,
    /// The measured file's own files (ruling D12), absolute (mapped through the pieces).
    pub own: Vec<Fragment>,
    /// Part 1.6, D14: for a deleted file the listing placed `OldChain` whose view measured `Intact` in one run with a
    /// coherent tail, every content check of its format passes over the view ([`content_checks`]); `false` for every
    /// other file. No other file starts inside it (ruling Q6): the view's measure already ends a file at a sector start
    /// where some format recognizes and confirms another header (the file's own embedded files aside), so an `Intact`
    /// view has none. The scan's finding starts cannot tell: those after the file are not known yet when it is merged,
    /// and no head inside an `Intact` finding's pieces is examined.
    pub checks: bool,
}

/// What a kept file's job computes: its view measurement and, for a deleted file, the carving at its start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FsJobResult {
    pub view: ViewMeasure,
    pub carved: Option<Measurement>,
    /// The carving's own files (ruling D12).
    pub carved_own: Vec<Fragment>,
}

impl FsJobResult {
    /// The own files of the measurement `merged` took: the carving's when it replaced the view (placement `Carved`,
    /// which only [`merge`] sets), else the view's.
    pub(crate) fn own_of(self, merged: &Merged) -> Vec<Fragment> {
        if merged.origin.placement == Placement::Carved {
            self.carved_own
        } else {
            self.view.own
        }
    }
}

/// `own`, offsets in the view of `pieces`, as absolute ranges: each start is mapped through the pieces (its length is
/// kept: the scan looks only at where an own file starts); one starting past the pieces is dropped.
fn absolute(pieces: &[Fragment], own: Vec<Fragment>) -> Vec<Fragment> {
    own.into_iter()
        .filter_map(|o| {
            let mut off = 0u64;
            pieces.iter().find_map(|p| {
                let inside = (off..off.saturating_add(p.len)).contains(&o.start);
                let at = inside.then(|| p.start + (o.start - off));
                off = off.saturating_add(p.len);
                at.map(|start| Fragment { start, len: o.len })
            })
        })
        .collect()
}

/// The first measurement of the formats that recognize the head at `at` of `source`.
fn measure_at(
    formats: &[Box<dyn Format>],
    source: &dyn Source,
    at: u64,
    cancel: &CancelToken,
) -> Option<Measured> {
    let want = (HEAD_LEN as u64).min(source.len().checked_sub(at)?) as usize;
    let mut head = [0u8; HEAD_LEN];
    let n = source.read_at(at, &mut head[..want]).ok()?;
    if n == 0 {
        return None;
    }
    pipeline::measure(formats, source, at, &head[..n], cancel)
}

/// Whether every byte of `view` in `[from, to)` is `00` or `FF` (an unreadable one is not).
fn padding(view: &dyn Source, from: u64, to: u64) -> bool {
    let mut buf = vec![0u8; PAD_CHUNK.min(to.saturating_sub(from) as usize)];
    let mut pos = from;
    while pos < to {
        let n = (PAD_CHUNK as u64).min(to - pos) as usize;
        match view.read_at(pos, &mut buf[..n]) {
            Ok(got) if got == n => {}
            _ => return false,
        }
        if buf[..n].iter().any(|&b| b != 0 && b != 0xFF) {
            return false;
        }
        pos += n as u64;
    }
    true
}

/// Where the content of a file of `kind` whose structure measures `l` bytes of `view` really ends (ruling Q56): `l`,
/// but for a video whose `mdat` has size 0 (it runs to the end of the view, whatever the clip), the highest sample end
/// its index lists.
fn content_end(view: &dyn Source, kind: FileKind, l: u64) -> u64 {
    match kind {
        FileKind::Mp4 | FileKind::Mov => {
            crate::formats::isobmff::size_zero_mdat_end(view, 0).map_or(l, |e| e.min(l))
        }
        _ => l,
    }
}

/// How a file of `kind` measured `l` bytes into `view` ends against the recorded `size` (rulings D6, Q53–Q56):
/// `(tail_ok, padded, at_size)` of [`ViewMeasure`]. The content's end is the format's full extent: its structure's
/// ([`content_end`]), then the whole files D6 allows after it (an MPF image, a motion photo's video), up to where only
/// fill follows. `at_size`: that end is the size, or a TIFF RAW's own grain padding fills the rest ([`raw_padding`]);
/// `padded`: fill follows it before the view's end, and it is no such RAW padding.
fn ends(
    formats: &[Box<dyn Format>],
    view: &dyn Source,
    kind: FileKind,
    l: u64,
    size: u64,
    cancel: &CancelToken,
) -> (bool, bool, bool) {
    let l = content_end(view, kind, l);
    let Some(end) = tail_end(formats, view, l, cancel) else {
        return (false, false, false);
    };
    let own = end == l && raw_padding(kind, l, size);
    (
        true,
        end < view.len() && !own,
        end == size || (own && view.len() == size),
    )
}

/// Ruling Q54 (b): the padding a camera writes into its own TIFF-based RAWs (Sony pads ARWs with zeros to a 4 KiB or
/// 64 KiB multiple): a recorded `size` that is the structure's end `l` rounded up to one of those grains.
fn raw_padding(kind: FileKind, l: u64, size: u64) -> bool {
    matches!(
        kind,
        FileKind::Arw
            | FileKind::Cr2
            | FileKind::Nef
            | FileKind::Dng
            | FileKind::Orf
            | FileKind::Rw2
            | FileKind::Tiff
    ) && [4096, 65536].iter().any(|&g| size == l.next_multiple_of(g))
}

/// Ruling D6 over a view of `S` bytes whose structure ends at `l`: `l == S`, or the rest is padding, or up to
/// [`MAX_TAIL_FILES`] files measured one after another (contiguous, not `Damaged`, inside the view), then padding.
fn tail_ok(formats: &[Box<dyn Format>], view: &dyn Source, l: u64, cancel: &CancelToken) -> bool {
    tail_end(formats, view, l, cancel).is_some()
}

/// [`tail_ok`]'s walk: where the last whole file after `l` ends and only padding follows; `None` when the tail is not
/// coherent.
fn tail_end(
    formats: &[Box<dyn Format>],
    view: &dyn Source,
    mut l: u64,
    cancel: &CancelToken,
) -> Option<u64> {
    let s = view.len();
    for i in 0..=MAX_TAIL_FILES {
        if l > s {
            return None;
        }
        if padding(view, l, s) {
            return Some(l);
        }
        if i == MAX_TAIL_FILES {
            break;
        }
        match measure_at(formats, view, l, cancel).map(|x| x.m) {
            Some(m2)
                if m2.health != Health::Damaged
                    && m2.fragments.is_empty()
                    && m2.len > 0
                    && l.saturating_add(m2.len) <= s =>
            {
                l += m2.len
            }
            _ => return None,
        }
    }
    None
}

/// Ruling D6 for a name from an old directory (D11): the `len` bytes at `start` (contiguous, inside the source) are
/// padding or up to [`MAX_TAIL_FILES`] whole files then padding.
pub(crate) fn tail_fits(
    formats: &[Box<dyn Format>],
    source: &dyn Source,
    start: u64,
    len: u64,
    cancel: &CancelToken,
) -> bool {
    if start.checked_add(len).is_none_or(|end| end > source.len()) {
        return false;
    }
    let view = Fragmented::new(source, vec![Fragment { start, len }]);
    tail_ok(formats, &view, 0, cancel)
}

/// The detectors over `Fragmented(f.pieces)` at 0, then the D6 tail check; for a deleted file placed `OldChain` whose
/// view is `Intact` with a coherent tail, the D14 checks too ([`ViewMeasure::checks`]).
pub(crate) fn measure_view(
    formats: &[Box<dyn Format>],
    source: &dyn Source,
    f: &FsFile,
    cancel: &CancelToken,
) -> ViewMeasure {
    let view = Fragmented::new(source, f.pieces.clone());
    let (m, own) =
        measure_at(formats, &view, 0, cancel).map_or((None, Vec::new()), |x| (Some(x.m), x.own));
    let (tail_ok, padded, at_size) = m.as_ref().map_or((false, false, false), |m| {
        ends(formats, &view, m.kind, m.len, f.origin.size, cancel)
    });
    let old_chain = f.origin.state == FsState::Deleted && f.origin.placement == Placement::OldChain;
    let checks = old_chain
        && tail_ok
        && m.as_ref().is_some_and(|m| {
            m.health == Health::Intact
                && m.fragments.is_empty()
                && content_checks(&view, m.kind, m.len, cancel)
        });
    ViewMeasure {
        m,
        tail_ok,
        padded,
        at_size,
        own: absolute(&f.pieces, own),
        checks,
    }
}

/// The one job of the kept file `f` starting at `at` (the worker's and every commit path's): the view measurement and,
/// for a deleted file, `carved`, the measurement at `at` when a format recognizes the head. Reuse (`reuse`, always on
/// outside tests): a deleted file in one piece whose carving is contiguous and no longer than the piece read the same
/// bytes as the view would, so the carving stands for the view measurement and only the tail check runs.
pub(crate) fn fs_job(
    formats: &[Box<dyn Format>],
    source: &dyn Source,
    f: &FsFile,
    at: u64,
    cancel: &CancelToken,
    reuse: bool,
) -> FsJobResult {
    if f.origin.state != FsState::Deleted {
        return FsJobResult {
            view: measure_view(formats, source, f, cancel),
            carved: None,
            carved_own: Vec::new(),
        };
    }
    let (carved, carved_own) =
        measure_at(formats, source, at, cancel).map_or((None, Vec::new()), |x| (Some(x.m), x.own));
    if reuse
        && let [piece] = f.pieces.as_slice()
        && let Some(c) = &carved
        && c.fragments.is_empty()
        && c.len <= piece.len
    {
        let view = Fragmented::new(source, f.pieces.clone());
        let (tail_ok, padded, at_size) = ends(formats, &view, c.kind, c.len, f.origin.size, cancel);
        return FsJobResult {
            view: ViewMeasure {
                m: Some(c.clone()),
                tail_ok,
                padded,
                at_size,
                own: carved_own.clone(),
                checks: false,
            },
            carved,
            carved_own,
        };
    }
    FsJobResult {
        view: measure_view(formats, source, f, cancel),
        carved,
        carved_own,
    }
}

/// What rulings Q53 to Q55 make of a kept file whose structure ends early over fill ([`decide`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Padded {
    /// The rule does not apply: the merge as usual.
    Keep,
    /// Reported as the carving at its start, without the entry's name.
    Carving,
    /// Its own pieces, without the entry's name, at most `Partial` to the end of the scan (the carving cannot stand for
    /// them; the reassembly may move its pieces, never make it `Intact`, Q55 (2)).
    Unnamed,
}

/// What the content proves of a kept file-system file ([`decide`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Decision {
    /// Whether the entry's name stays, and what replaces it when not.
    pub rule: Padded,
    /// A deleted file's old exFAT chain proves its placement (D14 and ruling Q55 (1)): it may be `Intact` though the
    /// placement is not trusted, and keeps the placement `OldChain`.
    pub proven: bool,
}

/// The one decision table for a kept file `f` (part 1.6, rulings D6, D14, Q53, Q54 (b), Q55), from how its structure,
/// measured over its pieces, ends against the recorded size:
///
/// - **at size** (`view.at_size`: the content ends exactly at the size, or a TIFF RAW's own padding to its 4 KiB or
///   64 KiB grain fills the rest): the name stays. An old chain is proof when its view is `Intact` in one run, every
///   content check passes and its pieces sum to the size; a recorded run or a guess keeps the D3/D5 rules.
/// - **early, then fill** (`view.padded`, only for a deleted file): no proof of which file the bytes are (a shorter one
///   may have been written over the clusters, the rest of its last cluster zero-filled), whatever placed them, an old
///   chain too (Q55: no exemption). The carving at the start replaces the entry, without a name, when it reads the same
///   bytes as the view (contiguous, within the entry's first piece: a carving that runs past it, as a size-0 `mdat` to
///   the end of the source, does not); otherwise the entry's pieces stay without the name ([`Padded::Unnamed`]).
/// - **early, then other bytes** (not `tail_ok`): the name stays, at most `Partial`; an old chain is no proof.
///
/// The content's end is the format's full extent, computed once before the fill check ([`ends`], ruling Q56): the
/// structure (for a video with a size-0 `mdat`, its highest sample end), then the whole files D6 allows after it (an
/// MPF image, a motion photo's video). So "whole files, then fill" is fill, and "whole files ending at the size" is at
/// size.
///
/// A live file always keeps its name (its chain says where it is): only D6 applies.
pub(crate) fn decide(f: &FsFile, view: &ViewMeasure, carved: Option<&Measurement>) -> Decision {
    let deleted = f.origin.state == FsState::Deleted;
    let joined = view.m.as_ref().is_some_and(|m| !m.fragments.is_empty());
    let len: u64 = f.pieces.iter().map(|p| p.len).sum();
    let proven = deleted
        && f.origin.placement == Placement::OldChain
        && view.checks
        && view.tail_ok
        && view.at_size
        && !joined
        && len == f.origin.size;
    let rule = if !deleted || !view.padded {
        Padded::Keep
    } else {
        let first = f.pieces.first().map_or(0, |p| p.len);
        match carved {
            Some(c) if c.fragments.is_empty() && c.len <= first => Padded::Carving,
            _ => Padded::Unnamed,
        }
    };
    Decision { rule, proven }
}

/// [`decide`]'s rule for `f`.
pub(crate) fn padded_rule(f: &FsFile, view: &ViewMeasure, carved: Option<&Measurement>) -> Padded {
    decide(f, view, carved).rule
}

/// A kept file's finding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Merged {
    pub len: u64,
    pub kind: FileKind,
    pub health: Health,
    pub fragments: Vec<Fragment>,
    pub origin: FsOrigin,
}

/// Health order: `Intact` 2, `Partial` 1, `Damaged` 0; "better" is strictly greater.
pub(crate) fn rank(h: Health) -> u8 {
    match h {
        Health::Intact => 2,
        Health::Partial => 1,
        Health::Damaged => 0,
    }
}

/// The finding of `f` (rulings D3, D5, D6): the view's kind and health (else the extension's kind, `Damaged`), at most
/// `Partial` when the placement is not trusted, the tail is not coherent, the view measurement joined pieces of its
/// own or the structure ends early over fill (a deleted file); the file system's pieces. A deleted file placed
/// `OldChain` (D14) keeps an `Intact` view when [`decide`] finds its old chain proven; otherwise its old chain was only
/// a hypothesis (placement `Guessed`). A deleted file takes `carved` instead when it is of the same kind, strictly
/// better and exactly the recorded size (placement `Carved`), unless the carving is one piece and the file system put
/// the file in two or more: two placements, neither proven better (the reassembly decides).
pub(crate) fn merge(f: &FsFile, view: &ViewMeasure, carved: Option<&Measurement>) -> Merged {
    let kind = view.m.as_ref().map_or(f.kind, |m| m.kind);
    let mut health = view.m.as_ref().map_or(Health::Damaged, |m| m.health);
    let joined = view.m.as_ref().is_some_and(|m| !m.fragments.is_empty());
    let len: u64 = f.pieces.iter().map(|p| p.len).sum();
    let old_chain = f.origin.state == FsState::Deleted && f.origin.placement == Placement::OldChain;
    let Decision { rule, proven } = decide(f, view, carved);
    // Q53/Q55: a deleted entry whose structure ends before its size over fill is never `Intact` under the name (the
    // scan reports the carving there, or the pieces without the name, instead).
    let padded = rule != Padded::Keep;
    if ((!f.trusted && !proven) || !view.tail_ok || joined || padded) && health == Health::Intact {
        health = Health::Partial;
    }
    let mut origin = f.origin.clone();
    if old_chain && !(proven && health == Health::Intact) {
        origin.placement = Placement::Guessed;
    }
    if f.origin.state == FsState::Deleted
        && let Some(c) = carved
        && c.kind == kind
        && rank(c.health) > rank(health)
        && c.len == f.origin.size
        && !(c.fragments.is_empty() && f.pieces.len() > 1)
    {
        let mut origin = f.origin.clone();
        origin.placement = Placement::Carved;
        return Merged {
            len: c.len,
            kind: c.kind,
            health: c.health,
            fragments: c.fragments.clone(),
            origin,
        };
    }
    let fragments = if f.pieces.len() > 1 {
        f.pieces.clone()
    } else {
        Vec::new()
    };
    Merged {
        len,
        kind,
        health,
        fragments,
        origin,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filesystem::FsTime;
    use crate::formats::all_formats;

    fn origin(state: FsState, size: u64, modified: Option<&str>) -> FsOrigin {
        FsOrigin {
            volume: 0,
            folder: Some("DCIM".into()),
            name: "A.JPG".into(),
            state,
            placement: if state == FsState::Live {
                Placement::Chain
            } else {
                Placement::Guessed
            },
            size,
            created: None,
            modified: modified.map(|m| FsTime {
                local: m.into(),
                utc_offset_minutes: None,
            }),
        }
    }

    fn file(state: FsState, trusted: bool, pieces: &[(u64, u64)]) -> FsFile {
        let pieces: Vec<Fragment> = pieces
            .iter()
            .map(|&(start, len)| Fragment { start, len })
            .collect();
        FsFile {
            origin: origin(state, pieces.iter().map(|p| p.len).sum(), None),
            kind: FileKind::Jpeg,
            pieces,
            trusted,
        }
    }

    fn m(len: u64, health: Health) -> Measurement {
        Measurement {
            len,
            kind: FileKind::Jpeg,
            health,
            fragments: Vec::new(),
        }
    }

    /// A view whose structure, when measured, ends at the recorded size.
    fn view(m: Option<Measurement>, tail_ok: bool) -> ViewMeasure {
        ViewMeasure {
            at_size: m.is_some(),
            m,
            tail_ok,
            padded: false,
            own: Vec::new(),
            checks: false,
        }
    }

    /// Q53: a deleted entry whose structure ends before its size with only padding after it is never `Intact` under
    /// its name (the scan reports the carving there, nameless, when there is one); a live one keeps the D6 rule.
    #[test]
    fn padded_tail_of_a_deleted_entry_is_no_proof() {
        let padded = ViewMeasure {
            padded: true,
            at_size: false,
            ..view(Some(m(900, Health::Intact)), true)
        };
        let live = file(FsState::Live, true, &[(4096, 1000)]);
        let deleted = file(FsState::Deleted, true, &[(4096, 1000)]);
        assert_eq!(merge(&live, &padded, None).health, Health::Intact);
        assert_eq!(merge(&deleted, &padded, None).health, Health::Partial);
        let carved = m(900, Health::Intact);
        assert_eq!(
            padded_rule(&deleted, &padded, Some(&carved)),
            Padded::Carving
        );
        assert_eq!(padded_rule(&deleted, &padded, None), Padded::Unnamed);
        assert_eq!(padded_rule(&live, &padded, Some(&carved)), Padded::Keep);
        let plain = view(Some(m(900, Health::Intact)), true);
        assert_eq!(padded_rule(&deleted, &plain, Some(&carved)), Padded::Keep);
        // Q54: in pieces, a carving longer than the first piece is not the view's bytes; Q55: an old chain whose view
        // passes its checks is no exemption.
        let mut chain = file(FsState::Deleted, false, &[(4096, 600), (65536, 400)]);
        assert_eq!(padded_rule(&chain, &padded, Some(&carved)), Padded::Unnamed);
        assert_eq!(
            padded_rule(&chain, &padded, Some(&m(500, Health::Partial))),
            Padded::Carving
        );
        chain.origin.placement = Placement::OldChain;
        let checked = ViewMeasure {
            checks: true,
            ..padded.clone()
        };
        assert_eq!(
            padded_rule(&chain, &checked, Some(&carved)),
            Padded::Unnamed
        );
        assert_eq!(padded_rule(&chain, &padded, Some(&carved)), Padded::Unnamed);
        assert_eq!(
            merge(&chain, &checked, Some(&carved)).health,
            Health::Partial
        );
        // Q54 (b): a RAW's own padding to 4 KiB or 64 KiB is not the rule's padding.
        assert!(raw_padding(FileKind::Arw, 900_000, 917_504));
        assert!(raw_padding(FileKind::Arw, 900_000, 901_120));
        assert!(!raw_padding(FileKind::Arw, 900_000, 905_000));
        assert!(!raw_padding(FileKind::Jpeg, 900_000, 917_504));
    }

    /// The decision table of [`decide`] (rulings Q55, Q56), cell by cell, for a deleted entry in 2 pieces of 1000 bytes:
    /// each placement (a guess trusted or not) × how the content ends ([`ends`]) → name kept, placement, best health.
    /// "short view": the content runs to the end of pieces that stop short of the size (cut at the source's end).
    #[test]
    fn decision_table_for_a_deleted_entry() {
        let full = |m_len: u64, tail_ok, padded, at_size| ViewMeasure {
            m: Some(m(m_len, Health::Intact)),
            tail_ok,
            padded,
            at_size,
            own: Vec::new(),
            checks: true,
        };
        let cells = [
            ("at size", full(1000, true, false, true)),
            ("RAW grain", full(900, true, false, true)),
            ("fill", full(900, true, true, false)),
            ("short view", full(900, true, false, false)),
            ("foreign", full(900, false, false, false)),
        ];
        let carved = m(500, Health::Intact);
        for (placement, trusted) in [
            (Placement::Recorded, true),
            (Placement::Guessed, true),
            (Placement::Guessed, false),
            (Placement::OldChain, false),
        ] {
            let mut f = file(FsState::Deleted, trusted, &[(4096, 600), (65536, 400)]);
            f.origin.placement = placement;
            for (cell, v) in &cells {
                let d = decide(&f, v, Some(&carved));
                let r = merge(&f, v, Some(&carved));
                let named = d.rule == Padded::Keep;
                let want_named = *cell != "fill";
                assert_eq!(named, want_named, "{placement:?} {cell}");
                // Carving within the first piece (500 ≤ 600): the carving stands for the entry, nameless.
                if *cell == "fill" {
                    assert_eq!(d.rule, Padded::Carving, "{placement:?} {cell}");
                    let none = decide(&f, v, None).rule;
                    assert_eq!(none, Padded::Unnamed, "{placement:?} {cell}");
                }
                let proof = matches!(*cell, "at size" | "RAW grain");
                let want = match placement {
                    Placement::Recorded | Placement::Guessed
                        if trusted && (proof || *cell == "short view") =>
                    {
                        (Health::Intact, placement)
                    }
                    Placement::Recorded | Placement::Guessed => (Health::Partial, placement),
                    _ if proof => (Health::Intact, Placement::OldChain),
                    _ => (Health::Partial, Placement::Guessed),
                };
                assert_eq!(
                    d.proven,
                    placement == Placement::OldChain && proof,
                    "{placement:?} {cell}"
                );
                assert_eq!((r.health, r.origin.placement), want, "{placement:?} {cell}");
            }
        }
        // A live entry keeps its name whatever follows its structure.
        let live = file(FsState::Live, true, &[(4096, 1000)]);
        for (cell, v) in &cells {
            assert_eq!(decide(&live, v, None).rule, Padded::Keep, "live {cell}");
        }
    }

    #[test]
    fn merge_rules() {
        let live = file(FsState::Live, true, &[(4096, 1000)]);
        let intact = view(Some(m(1000, Health::Intact)), true);
        // A trusted live file with a clean view: Intact, its own pieces.
        let r = merge(&live, &intact, None);
        assert_eq!(
            (r.len, r.health, r.fragments.len()),
            (1000, Health::Intact, 0)
        );
        assert_eq!(r.origin, live.origin);
        // Untrusted, an incoherent tail, a join inside the view: at most Partial.
        let untrusted = file(FsState::Live, false, &[(4096, 1000)]);
        assert_eq!(merge(&untrusted, &intact, None).health, Health::Partial);
        assert_eq!(
            merge(&live, &view(Some(m(1000, Health::Intact)), false), None).health,
            Health::Partial
        );
        let mut joined = m(1000, Health::Intact);
        joined.fragments = vec![
            Fragment { start: 0, len: 500 },
            Fragment {
                start: 700,
                len: 500,
            },
        ];
        assert_eq!(
            merge(&live, &view(Some(joined), true), None).health,
            Health::Partial
        );
        // Never raised: a Damaged view stays Damaged.
        assert_eq!(
            merge(&untrusted, &view(Some(m(10, Health::Damaged)), false), None).health,
            Health::Damaged
        );
        // No view measurement: the extension's kind, Damaged.
        let mut mov = file(FsState::Live, true, &[(4096, 1000)]);
        mov.kind = FileKind::Mov;
        let r = merge(&mov, &view(None, false), None);
        assert_eq!((r.kind, r.health), (FileKind::Mov, Health::Damaged));
        // Two pieces: the file system's pieces, their sum.
        let two = file(FsState::Live, true, &[(4096, 600), (65536, 400)]);
        let r = merge(&two, &intact, None);
        assert_eq!((r.len, r.fragments.clone()), (1000, two.pieces.clone()));

        // A deleted, untrusted guess; the carving is better and of the recorded size: the carving, placement Carved.
        let deleted = file(FsState::Deleted, false, &[(4096, 1000)]);
        let mut carved = m(1000, Health::Intact);
        carved.fragments = vec![
            Fragment {
                start: 4096,
                len: 512,
            },
            Fragment {
                start: 9000,
                len: 488,
            },
        ];
        let r = merge(&deleted, &intact, Some(&carved));
        assert_eq!(
            (r.len, r.health, r.fragments.clone()),
            (1000, Health::Intact, carved.fragments.clone())
        );
        assert_eq!(r.origin.placement, Placement::Carved);
        // Another size: the view.
        let other = m(1200, Health::Intact);
        let r = merge(&deleted, &intact, Some(&other));
        assert_eq!(
            (r.health, r.origin.placement),
            (Health::Partial, Placement::Guessed)
        );
        // Not strictly better, or of another kind: the view.
        let r = merge(&deleted, &intact, Some(&m(1000, Health::Partial)));
        assert_eq!(r.origin.placement, Placement::Guessed);
        let mut mp4 = carved.clone();
        mp4.kind = FileKind::Mp4;
        assert_eq!(
            merge(&deleted, &intact, Some(&mp4)).origin.placement,
            Placement::Guessed
        );
        // A live file never takes the carving.
        let r = merge(&untrusted, &intact, Some(&carved));
        assert_eq!(
            (r.health, r.origin.placement),
            (Health::Partial, Placement::Chain)
        );

        assert!(rank(Health::Intact) > rank(Health::Partial));
        assert!(rank(Health::Partial) > rank(Health::Damaged));
    }

    /// D14: a deleted file placed by its old exFAT chain keeps an `Intact` view only with its checks, a coherent tail
    /// and its pieces summing to the entry's size; otherwise the chain was a hypothesis (`Guessed`, `Partial`). A
    /// contiguous carving never replaces a placement in two or more pieces.
    #[test]
    fn merge_keeps_intact_for_a_checked_old_chain() {
        let mut chain = file(FsState::Deleted, false, &[(4096, 600), (65536, 400)]);
        chain.origin.placement = Placement::OldChain;
        let checked = |checks, tail_ok| ViewMeasure {
            checks,
            ..view(Some(m(1000, Health::Intact)), tail_ok)
        };
        let r = merge(&chain, &checked(true, true), None);
        assert_eq!(
            (r.health, r.origin.placement, r.fragments.clone()),
            (Health::Intact, Placement::OldChain, chain.pieces.clone())
        );
        for (what, v) in [
            ("no checks", checked(false, true)),
            ("tail", checked(true, false)),
        ] {
            let r = merge(&chain, &v, None);
            assert_eq!(
                (r.health, r.origin.placement),
                (Health::Partial, Placement::Guessed),
                "{what}"
            );
        }
        // The pieces short of the entry's size (cut at the source's end).
        let mut short = chain.clone();
        short.origin.size = 1500;
        assert_eq!(
            merge(&short, &checked(true, true), None).health,
            Health::Partial
        );
        // Another placement never keeps an Intact view through the checks.
        let mut guessed = chain.clone();
        guessed.origin.placement = Placement::Guessed;
        assert_eq!(
            merge(&guessed, &checked(true, true), None).health,
            Health::Partial
        );
        // A contiguous carving of the size, Intact, is another placement, not a better one; a carving in the same pieces
        // still is.
        let carved = m(1000, Health::Intact);
        let r = merge(&chain, &checked(false, true), Some(&carved));
        assert_eq!(
            (r.health, r.origin.placement),
            (Health::Partial, Placement::Guessed)
        );
        let mut same = carved.clone();
        same.fragments = chain.pieces.clone();
        let r = merge(&chain, &checked(false, true), Some(&same));
        assert_eq!(
            (r.health, r.origin.placement),
            (Health::Intact, Placement::Carved)
        );
    }

    #[test]
    fn one_file_per_start() {
        let at = |state, modified: Option<&str>, name: &str| {
            let mut f = file(state, true, &[(4096, 1000)]);
            f.origin.modified = modified.map(|m| FsTime {
                local: m.into(),
                utc_offset_minutes: None,
            });
            f.origin.name = name.into();
            f
        };
        let names = |files: Vec<FsFile>| -> Vec<String> {
            keep(files).into_iter().map(|f| f.origin.name).collect()
        };
        // Live before deleted, whatever the dates.
        let v = vec![
            at(FsState::Deleted, Some("2026-01-01T00:00:00"), "D"),
            at(FsState::Live, None, "L"),
        ];
        assert_eq!(names(v), ["L"]);
        // Then the latest modified; a missing one loses to a present one.
        let v = vec![
            at(FsState::Deleted, None, "N"),
            at(FsState::Deleted, Some("2025-01-01T00:00:00"), "OLD"),
            at(FsState::Deleted, Some("2025-06-01T00:00:00"), "NEW"),
        ];
        assert_eq!(names(v), ["NEW"]);
        let v = vec![
            at(FsState::Deleted, Some("2025-01-01T00:00:00"), "A"),
            at(FsState::Deleted, None, "B"),
        ];
        assert_eq!(names(v), ["A"]);
        // Deleted files tied (equal times, or none): neither is kept (Q52), unless one ranks above them.
        let v = vec![
            at(FsState::Deleted, None, "FIRST"),
            at(FsState::Deleted, None, "SECOND"),
        ];
        assert!(names(v).is_empty());
        let v = vec![
            at(FsState::Deleted, Some("2025-01-01T00:00:00"), "A"),
            at(FsState::Deleted, Some("2025-01-01T00:00:00"), "B"),
        ];
        assert!(names(v).is_empty());
        let v = vec![
            at(FsState::Deleted, Some("2025-01-01T00:00:00"), "A"),
            at(FsState::Deleted, Some("2025-01-01T00:00:00"), "B"),
            at(FsState::Deleted, Some("2025-02-01T00:00:00"), "NEWER"),
        ];
        assert_eq!(names(v), ["NEWER"]);
        // Live files tied: listing order, as before.
        let v = vec![
            at(FsState::Live, None, "FIRST"),
            at(FsState::Live, None, "SECOND"),
        ];
        assert_eq!(names(v), ["FIRST"]);
        // Other starts are all kept.
        let mut other = at(FsState::Deleted, None, "ELSEWHERE");
        other.pieces[0].start = 8192;
        let v = vec![at(FsState::Live, None, "L"), other];
        assert_eq!(names(v), ["L", "ELSEWHERE"]);
    }

    #[test]
    fn plan_lookups() {
        let mut odd = file(FsState::Live, true, &[(4096 + 512, 100)]);
        odd.origin.name = "ODD.JPG".into();
        let listing = Listing {
            files: vec![file(FsState::Live, true, &[(4096, 100)]), odd],
            live: vec![
                Fragment {
                    start: 4096,
                    len: 4096,
                },
                Fragment {
                    start: 65536,
                    len: 512,
                },
            ],
            others: vec![Fragment {
                start: 65536,
                len: 512,
            }],
            ..Listing::default()
        };
        // 4096-byte sectors drop the file not aligned to them.
        let plan = FsPlan::new(listing, 4096, 1 << 20);
        assert_eq!(plan.starts, [4096]);
        assert_eq!(plan.kept_in(0, 4096), 0..0);
        assert_eq!(plan.kept_in(4096, 8192), 0..1);
        assert!(plan.is_other(65536));
        assert!(!plan.is_other(4096));
        assert!(plan.overlaps_live(8191, 8192));
        assert!(!plan.overlaps_live(8192, 65536));
        assert!(plan.overlaps_live(0, 1 << 20));
        assert!(!plan.overlaps_live(66048, 70000));
    }

    struct Mem(Vec<u8>);

    impl Source for Mem {
        fn len(&self) -> u64 {
            self.0.len() as u64
        }

        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, crate::EngineError> {
            let start = (offset as usize).min(self.0.len());
            let n = buf.len().min(self.0.len() - start);
            buf[..n].copy_from_slice(&self.0[start..start + n]);
            Ok(n)
        }

        fn volume(&self) -> Option<crate::source::VolumeId> {
            None
        }
    }

    /// Recognizes `TAILFILE` and measures 128 bytes there, in two pieces when `.0`.
    struct Fake(bool);

    impl Format for Fake {
        fn name(&self) -> &'static str {
            "fake"
        }

        fn recognizes(&self, head: &[u8]) -> bool {
            head.starts_with(b"TAILFILE")
        }

        fn confirms(&self, _: &dyn Source, _: u64) -> bool {
            false
        }

        fn measure(&self, _: &dyn Source, start: u64) -> Option<Measurement> {
            let fragments = if self.0 {
                vec![
                    Fragment { start, len: 64 },
                    Fragment {
                        start: start + 64,
                        len: 64,
                    },
                ]
            } else {
                Vec::new()
            };
            Some(Measurement {
                len: 128,
                kind: FileKind::Jpeg,
                health: Health::Intact,
                fragments,
            })
        }
    }

    /// Ruling Q56: the content's end is the format's full extent, computed before the fill check: a JPEG's appended
    /// whole images count, and a video whose `mdat` has size 0 ends at its highest sample end, not at the view's end.
    #[test]
    fn content_end_is_the_full_extent() {
        use revenant_testkit as tk;
        let formats = all_formats();
        let ends_of = |bytes: Vec<u8>, size: u64| {
            let src = Mem(bytes);
            let at = measure_at(&formats, &src, 0, &CancelToken::default()).unwrap();
            ends(
                &formats,
                &src,
                at.m.kind,
                at.m.len,
                size,
                &CancelToken::default(),
            )
        };
        let mut photo = tk::jpeg_smooth(320, 240, 1);
        photo.extend_from_slice(&tk::jpeg(64, 64, 77, 0));
        let n = photo.len() as u64;
        // (tail_ok, padded, at_size)
        assert_eq!(
            ends_of(photo.clone(), n),
            (true, false, true),
            "appended, at size"
        );
        let mut zeros = photo.clone();
        zeros.resize(photo.len() + 3000, 0);
        assert_eq!(
            ends_of(zeros, n + 3000),
            (true, true, false),
            "appended, then fill"
        );
        let mut foreign = photo.clone();
        foreign.extend_from_slice(&[1, 2, 3]);
        assert_eq!(
            ends_of(foreign, n + 3),
            (false, false, false),
            "then other bytes"
        );
        let clip = tk::camera_video(
            &tk::AvcOptions {
                samples: 20,
                seed: 1,
                mov: false,
                mdat_pad: 0,
                small_nals: false,
            },
            tk::VideoCodec::Avc,
            tk::VideoLayout::MoovFirstMdatSizeZero,
        )
        .bytes;
        let n = clip.len() as u64;
        assert_eq!(
            ends_of(clip.clone(), n),
            (true, false, true),
            "size-0 mdat, at size"
        );
        let mut zeros = clip.clone();
        zeros.resize(clip.len() + 3000, 0);
        assert_eq!(
            ends_of(zeros, n + 3000),
            (true, true, false),
            "size-0 mdat, then fill"
        );
    }

    #[test]
    fn tail_rules() {
        use revenant_testkit as tk;
        let formats = all_formats();
        let j = tk::jpeg(64, 64, 1, 0);
        let check = |tail: &[u8]| {
            let mut bytes = j.clone();
            bytes.extend_from_slice(tail);
            let src = Mem(bytes);
            let f = file(FsState::Live, true, &[(0, src.len())]);
            measure_view(&formats, &src, &f, &CancelToken::default())
        };
        assert!(check(&[]).tail_ok);
        assert!(check(&[0, 0xFF, 0, 0xFF]).tail_ok);
        assert!(!check(&[0, 1, 0]).tail_ok);
        // Up to 8 files one after another, then padding.
        let eight: Vec<u8> = (0..8).flat_map(|_| j.clone()).collect();
        let mut nine = eight.clone();
        nine.extend_from_slice(&j);
        assert!(check(&eight).tail_ok);
        assert!(!check(&nine).tail_ok);
        // A file then other bytes.
        let mut then = j.clone();
        then.extend_from_slice(&[1, 2, 3]);
        assert!(!check(&then).tail_ok);
        // A file in the tail that is itself in pieces (a join) is not one of the files D6 allows.
        assert!(!tail_ok(
            &[Box::new(Fake(true))],
            &Mem(b"TAILFILE".repeat(16)),
            0,
            &CancelToken::default()
        ));
        assert!(tail_ok(
            &[Box::new(Fake(false))],
            &Mem(b"TAILFILE".repeat(16)),
            0,
            &CancelToken::default()
        ));
        // A view the detectors do not recognize.
        let src = Mem(vec![7; 4096]);
        let f = file(FsState::Live, true, &[(0, 4096)]);
        assert_eq!(
            measure_view(&formats, &src, &f, &CancelToken::default()),
            view(None, false)
        );
    }

    #[test]
    fn reuse_matches_view_measurement() {
        use revenant_testkit as tk;
        let formats = all_formats();
        let exif = tk::TestExif::default();
        let raw = |variant| {
            tk::tiff_raw(&tk::TiffRawOptions {
                variant,
                big_endian: false,
                raw_len: 50_000,
                seed: 3,
                preview: Some((160, 120)),
                preview_sector_aligned: false,
                exif: exif.clone(),
            })
            .bytes
        };
        let video = |mov| {
            tk::mp4(&tk::Mp4Options {
                mdat_len: 100_000,
                moov_at_end: true,
                mov,
                mdat_64bit: false,
            })
        };
        let files: Vec<(&str, Vec<u8>)> = vec![
            ("jpeg", tk::jpeg(64, 64, 1, 0)),
            ("smooth", tk::jpeg_smooth(320, 240, 1)),
            ("mp4", video(false)),
            ("mov", video(true)),
            (
                "avc",
                tk::avc_video(&tk::AvcOptions {
                    samples: 20,
                    seed: 1,
                    mov: false,
                    mdat_pad: 0,
                    small_nals: true,
                })
                .bytes,
            ),
            ("cr2", raw(tk::TiffVariant::Cr2)),
            ("nef", raw(tk::TiffVariant::Nef)),
            ("arw", raw(tk::TiffVariant::Arw)),
            ("dng", raw(tk::TiffVariant::Dng)),
            ("orf", raw(tk::TiffVariant::Orf)),
            ("rw2", raw(tk::TiffVariant::Rw2)),
            ("tif", raw(tk::TiffVariant::Plain)),
            ("raf", tk::raf(50_000, 2, &exif)),
            ("cr3", tk::cr3(2, &exif)),
            ("heic", tk::heic(30_000, 2, &exif)),
        ];
        let cancel = CancelToken::default();
        for (what, bytes) in files {
            // The file at 4096, its recorded size exact, then 4096 zeros, then noise; also with the size 2 KiB larger
            // (the tail check over the zeros) and 2 KiB smaller (cut).
            for extra in [0i64, 2048, -2048] {
                let mut img = vec![0u8; 4096];
                img.extend_from_slice(&bytes);
                img.extend(std::iter::repeat_n(0u8, 4096));
                img.extend((0..65536u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 9) as u8));
                let src = Mem(img);
                let size = (bytes.len() as i64 + extra) as u64;
                let f = file(FsState::Deleted, true, &[(4096, size)]);
                let with = fs_job(&formats, &src, &f, 4096, &cancel, true);
                let without = fs_job(&formats, &src, &f, 4096, &cancel, false);
                let (a, b) = (
                    merge(&f, &with.view, with.carved.as_ref()),
                    merge(&f, &without.view, without.carved.as_ref()),
                );
                assert_eq!(a, b, "{what} {extra}: {with:?} vs {without:?}");
                // The reuse ran: the carving stands for the view measurement.
                if extra >= 0 {
                    assert_eq!(with.view.m, with.carved, "{what} {extra}");
                }
                if extra == 0 {
                    assert_eq!(a.health, Health::Intact, "{what}");
                }
            }
        }
    }

    #[test]
    fn fs_job_is_independent_of_who_measures() {
        use crate::scan::{COMMIT_FS_JOBS, Event, ScanOptions, scan};
        use revenant_testkit as tk;
        use revenant_testkit::fs::{DeleteStyle, FsBuilder, FsType, Partitioning, disk};
        use std::io::Write;

        // FAT16 (no high-word check), 1 KiB clusters, at LBA 8192.
        let mut b = FsBuilder::new(FsType::Fat16, 32 << 20, 1024);
        b.add("DCIM/100MSDCF/DSC00001.JPG", &tk::jpeg(64, 64, 1, 0));
        let d = tk::jpeg(64, 64, 2, 0);
        let dc: Vec<u32> = (3000..3000 + d.len().div_ceil(1024) as u32).collect();
        b.add_at("DCIM/100MSDCF/DSC00002.JPG", &d, &dc);
        b.delete("DCIM/100MSDCF/DSC00002.JPG", DeleteStyle::Camera);
        let v = b.build();
        let mut img = disk(
            Partitioning::Mbr { first_lba: 8192 },
            &v,
            v.len() + (8 << 20),
        );
        // A photo with no entry over the deleted file's clusters, from 150 KiB before them: an `Intact` carving whose
        // skip range defers the 4 KiB blocks it covers, the deleted file's first one among them.
        let x = tk::jpeg_smooth(2048, 1536, 1);
        let xo = (8192 * 512 + b.cluster_offset(2850)) as usize;
        assert!(
            xo + x.len() > (8192 * 512 + b.cluster_offset(*dc.last().unwrap())) as usize + 1024
        );
        img[xo..xo + x.len()].copy_from_slice(&x);
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&img).unwrap();
        file.flush().unwrap();
        let src = crate::source::FileSource::open(file.path()).unwrap();
        let formats = all_formats();
        let run = |threads, drop_fs_jobs| {
            COMMIT_FS_JOBS.with(|c| c.set((0, 0)));
            let mut events = Vec::new();
            let opts = ScanOptions {
                threads,
                block: 4096,
                drop_fs_jobs,
                ..ScanOptions::default()
            };
            scan(&src, &formats, &opts, &CancelToken::default(), &mut |e| {
                events.push(e)
            })
            .unwrap();
            (events, COMMIT_FS_JOBS.with(|c| c.get()))
        };
        let (base, jobs) = run(8, false);
        let found: Vec<_> = base
            .iter()
            .filter_map(|e| match e {
                Event::Found(f) => Some(f),
                _ => None,
            })
            .collect();
        let deleted = found
            .iter()
            .find(|f| {
                f.origin
                    .as_ref()
                    .is_some_and(|o| o.name() == "DSC00002.JPG")
            })
            .expect("the deleted file");
        assert_eq!(deleted.origin.as_ref().unwrap().state, FsState::Deleted);
        assert!(
            found
                .iter()
                .any(|f| f.origin.is_none() && f.start == xo as u64 && f.health == Health::Intact)
        );
        // The workers measured the live file; the deleted one sits in a deferred block, so the commit loop ran it.
        assert_eq!(jobs, (1, 1));
        // Workers handing every job back, or no workers: the commit loop runs both, with the same events.
        assert_eq!(run(8, true), (base.clone(), (2, 1)));
        assert_eq!(run(1, false), (base, (2, 1)));
    }

    #[test]
    fn own_files_are_the_same_on_every_path() {
        use crate::scan::{COMMIT_FS_JOBS, Event, ScanOptions, scan};
        use revenant_testkit as tk;
        use revenant_testkit::fs::{DeleteStyle, FsBuilder, FsType, Partitioning, disk};
        use std::io::Write;

        // A Panasonic RW2 of an unchecked packing: Partial; its JpgFromRaw starts at 512, a thumbnail inside it at 1024.
        let rw2 = |seed| {
            tk::panasonic_rw2(&tk::Rw2Options {
                width: 280,
                height: 200,
                raw_format: 5,
                bits: 12,
                raw_len: 98_304,
                strip_len: 0,
                seed,
                exif: tk::TestExif::default(),
            })
        };
        // FAT16, 1 KiB clusters, at LBA 2048: a deleted RW2 at cluster 3000.
        let lba = 2048u64;
        let mut b = FsBuilder::new(FsType::Fat16, 8 << 20, 1024);
        let r = rw2(3);
        let dc: Vec<u32> = (3000..3000 + r.bytes.len().div_ceil(1024) as u32).collect();
        b.add_at("DCIM/100_PANA/P1000001.RW2", &r.bytes, &dc);
        b.delete("DCIM/100_PANA/P1000001.RW2", DeleteStyle::Camera);
        let v = b.build();
        let mut img = disk(
            Partitioning::Mbr {
                first_lba: lba as u32,
            },
            &v,
            v.len() + (2 << 20),
        );
        let rw2_at = (lba * 512 + b.cluster_offset(3000)) as usize;
        // An MP4 with no entry ending in a `free` box whose last 512 bytes are the RW2's first sector: an `Intact`
        // carving whose skip range defers the deleted RW2's first 512-byte block, so the commit loop runs its job.
        let x = tk::mp4(&tk::Mp4Options {
            mdat_len: 100_000,
            moov_at_end: true,
            mov: false,
            mdat_64bit: false,
        });
        let xo = (rw2_at - 8 - x.len()) / 512 * 512;
        img[xo..xo + x.len()].copy_from_slice(&x);
        let free = (rw2_at + 512 - (xo + x.len())) as u32;
        img[xo + x.len()..xo + x.len() + 4].copy_from_slice(&free.to_be_bytes());
        img[xo + x.len() + 4..xo + x.len() + 8].copy_from_slice(b"free");
        // Another RW2 with no entry, far after the first: carved, measured by the workers.
        let r2 = rw2(4);
        let carved_at = rw2_at + (1 << 20);
        img[carved_at..carved_at + r2.bytes.len()].copy_from_slice(&r2.bytes);
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&img).unwrap();
        file.flush().unwrap();
        let src = crate::source::FileSource::open(file.path()).unwrap();
        let formats = all_formats();
        let run = |threads, drop_fs_jobs| {
            COMMIT_FS_JOBS.with(|c| c.set((0, 0)));
            let mut events = Vec::new();
            let opts = ScanOptions {
                threads,
                block: 512,
                drop_fs_jobs,
                ..ScanOptions::default()
            };
            scan(&src, &formats, &opts, &CancelToken::default(), &mut |e| {
                events.push(e)
            })
            .unwrap();
            (events, COMMIT_FS_JOBS.with(|c| c.get()))
        };
        let (base, jobs) = run(1, false);
        // The deleted RW2's job ran in a deferred block.
        assert_eq!(jobs, (1, 1));
        let found: Vec<(u64, FileKind, Health, bool)> = base
            .iter()
            .filter_map(|e| match e {
                Event::Found(f) => Some((f.start, f.kind, f.health, f.origin.is_some())),
                _ => None,
            })
            .collect();
        assert_eq!(
            found,
            [
                (xo as u64, FileKind::Mp4, Health::Intact, false),
                (rw2_at as u64, FileKind::Rw2, Health::Partial, true),
                (carved_at as u64, FileKind::Rw2, Health::Partial, false),
            ]
        );
        assert_eq!(run(8, false).0, base);
        assert_eq!(run(8, true).0, base);
    }
}
