//! The file-system half of the scan: which listed files the scan reports, how each one is measured (through a view of
//! its own pieces) and how that merges with the carving at the same start, so a file found both ways comes out once
//! (rulings D3, D5, D6, D7). Pure functions of the source's bytes, so the result does not depend on who measures.

use std::cmp::Ordering;

use super::pipeline;
use crate::filesystem::{FsFile, FsOrigin, FsState, Listing, Placement};
use crate::formats::{FileKind, Format, Fragment, Health, Measurement};
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
        let i = self
            .live
            .partition_point(|r| r.start.saturating_add(r.len) <= start);
        self.live.get(i).is_some_and(|r| r.start < end)
    }
}

/// Whether `a` is kept over `b` at the same start: live before deleted, then the latest `modified` (a missing one loses
/// to a present one), then listing order (`a` comes first, so a tie keeps it).
fn beats(a: &FsFile, b: &FsFile) -> bool {
    let live = |f: &FsFile| f.origin.state == FsState::Live;
    match live(a).cmp(&live(b)) {
        Ordering::Greater => return true,
        Ordering::Less => return false,
        Ordering::Equal => {}
    }
    // "YYYY-MM-DDTHH:MM:SS" sorts as it reads; `None` sorts first.
    let modified = |f: &FsFile| f.origin.modified.as_ref().map(|t| t.local.clone());
    modified(a) >= modified(b)
}

/// One file per distinct first-piece start, `files` sorted by it (the listing's order).
pub(crate) fn keep(files: Vec<FsFile>) -> Vec<FsFile> {
    let mut out: Vec<FsFile> = Vec::with_capacity(files.len());
    for f in files {
        match out.last_mut() {
            Some(last) if last.pieces[0].start == f.pieces[0].start => {
                if !beats(last, &f) {
                    *last = f;
                }
            }
            _ => out.push(f),
        }
    }
    out
}

/// A file's measurement through the view of its own pieces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ViewMeasure {
    pub m: Option<Measurement>,
    /// Ruling D6: what lies between the structure's end and the recorded size is padding or whole files.
    pub tail_ok: bool,
}

/// What a kept file's job computes: its view measurement and, for a deleted file, the carving at its start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FsJobResult {
    pub view: ViewMeasure,
    pub carved: Option<Measurement>,
}

/// The first measurement of the formats that recognize the head at `at` of `source`.
fn measure_at(
    formats: &[Box<dyn Format>],
    source: &dyn Source,
    at: u64,
    cancel: &CancelToken,
) -> Option<Measurement> {
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

/// Ruling D6 over a view of `S` bytes whose structure ends at `l`: `l == S`, or the rest is padding, or up to
/// [`MAX_TAIL_FILES`] files measured one after another (contiguous, not `Damaged`, inside the view), then padding.
fn tail_ok(
    formats: &[Box<dyn Format>],
    view: &dyn Source,
    mut l: u64,
    cancel: &CancelToken,
) -> bool {
    let s = view.len();
    for i in 0..=MAX_TAIL_FILES {
        if l > s {
            return false;
        }
        if padding(view, l, s) {
            return true;
        }
        if i == MAX_TAIL_FILES {
            break;
        }
        match measure_at(formats, view, l, cancel) {
            Some(m2)
                if m2.health != Health::Damaged
                    && m2.fragments.is_empty()
                    && m2.len > 0
                    && l.saturating_add(m2.len) <= s =>
            {
                l += m2.len
            }
            _ => return false,
        }
    }
    false
}

/// The detectors over `Fragmented(f.pieces)` at 0, then the D6 tail check.
pub(crate) fn measure_view(
    formats: &[Box<dyn Format>],
    source: &dyn Source,
    f: &FsFile,
    cancel: &CancelToken,
) -> ViewMeasure {
    let view = Fragmented::new(source, f.pieces.clone());
    let m = measure_at(formats, &view, 0, cancel);
    let tail_ok = m
        .as_ref()
        .is_some_and(|m| tail_ok(formats, &view, m.len, cancel));
    ViewMeasure { m, tail_ok }
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
        };
    }
    let carved = measure_at(formats, source, at, cancel);
    if reuse
        && let [piece] = f.pieces.as_slice()
        && let Some(c) = &carved
        && c.fragments.is_empty()
        && c.len <= piece.len
    {
        let view = Fragmented::new(source, f.pieces.clone());
        let tail_ok = tail_ok(formats, &view, c.len, cancel);
        return FsJobResult {
            view: ViewMeasure {
                m: Some(c.clone()),
                tail_ok,
            },
            carved,
        };
    }
    FsJobResult {
        view: measure_view(formats, source, f, cancel),
        carved,
    }
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
/// `Partial` when the placement is not trusted, the tail is not coherent or the view measurement joined pieces of its
/// own; the file system's pieces. A deleted file takes `carved` instead when it is of the same kind, strictly better
/// and exactly the recorded size (placement `Carved`).
pub(crate) fn merge(f: &FsFile, view: &ViewMeasure, carved: Option<&Measurement>) -> Merged {
    let kind = view.m.as_ref().map_or(f.kind, |m| m.kind);
    let mut health = view.m.as_ref().map_or(Health::Damaged, |m| m.health);
    let joined = view.m.as_ref().is_some_and(|m| !m.fragments.is_empty());
    if (!f.trusted || !view.tail_ok || joined) && health == Health::Intact {
        health = Health::Partial;
    }
    if f.origin.state == FsState::Deleted
        && let Some(c) = carved
        && c.kind == kind
        && rank(c.health) > rank(health)
        && c.len == f.origin.size
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
        len: f.pieces.iter().map(|p| p.len).sum(),
        kind,
        health,
        fragments,
        origin: f.origin.clone(),
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

    fn view(m: Option<Measurement>, tail_ok: bool) -> ViewMeasure {
        ViewMeasure { m, tail_ok }
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
        // Then listing order.
        let v = vec![
            at(FsState::Deleted, None, "FIRST"),
            at(FsState::Deleted, None, "SECOND"),
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
}
