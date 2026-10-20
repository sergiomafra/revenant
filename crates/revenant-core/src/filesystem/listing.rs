//! The files the volumes of a source list: where each one's pieces are, its original path and times, and whether its
//! placement can be trusted.

use std::fmt;

use super::alloc::{Chain, Claims};
use super::{FsKind, Limits, MAX_WALKED_CLUSTERS, Volume, fat, volumes};
use crate::formats::{FileKind, Fragment};
use crate::scan::CancelToken;
use crate::source::Source;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsState {
    Live,
    Deleted,
    Orphaned,
}

impl FsState {
    pub fn as_str(self) -> &'static str {
        match self {
            FsState::Live => "live",
            FsState::Deleted => "deleted",
            FsState::Orphaned => "orphaned",
        }
    }
}

/// How the pieces were placed: Chain = FAT chain or exFAT NoFatChain of a live file; Recorded = exFAT NoFatChain run
/// of a deleted file; Guessed = deleted-file heuristic (D4); Carved = the scan's own measurement (D5, Task 10).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    Chain,
    Recorded,
    Guessed,
    Carved,
}

impl Placement {
    pub fn as_str(self) -> &'static str {
        match self {
            Placement::Chain => "chain",
            Placement::Recorded => "recorded",
            Placement::Guessed => "guessed",
            Placement::Carved => "carved",
        }
    }
}

/// "YYYY-MM-DDTHH:MM:SS" as recorded (local time); Display appends "+HH:MM"/"-HH:MM" when the offset is known.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FsTime {
    pub local: String,
    pub utc_offset_minutes: Option<i16>,
}

impl fmt::Display for FsTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.local)?;
        if let Some(m) = self.utc_offset_minutes {
            let sign = if m < 0 { '-' } else { '+' };
            let m = m.unsigned_abs();
            write!(f, "{sign}{:02}:{:02}", m / 60, m % 60)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FsOrigin {
    pub volume: u32,
    /// Components joined by '/', root-relative, no leading '/'; "?" for a directory whose name is unknown.
    pub path: String,
    pub state: FsState,
    pub placement: Placement,
    /// As recorded in the entry.
    pub size: u64,
    pub created: Option<FsTime>,
    pub modified: Option<FsTime>,
}

impl FsOrigin {
    /// The last component of the path.
    pub fn name(&self) -> &str {
        self.path.rsplit_once('/').map_or(&self.path, |(_, n)| n)
    }

    /// The path of the folder holding the file; "" for the root.
    pub fn folder(&self) -> &str {
        self.path.rsplit_once('/').map_or("", |(d, _)| d)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FsFile {
    pub origin: FsOrigin,
    /// From the extension.
    pub kind: FileKind,
    /// Absolute pieces in file order, sum = min(size, what the clusters hold and the source holds). The whole
    /// listing holds at most [`super::MAX_PIECES`] (1 << 20) pieces: a file that would pass it is not listed.
    pub pieces: Vec<Fragment>,
    /// Ruling D3: the placement is trusted (a live file: a complete chain, no cycle nor cross-link).
    pub trusted: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Listing {
    pub volumes: Vec<Volume>,
    /// Photo/video files only (extension known to `FileKind::from_extension`), sorted by (first piece start, volume,
    /// path).
    pub files: Vec<FsFile>,
    /// Merged absolute ranges of every live file and live directory of any kind (D7 cap, `keep_moovs`, orphans, D18);
    /// the fixed FAT12/16 root region is not in it.
    pub live: Vec<Fragment>,
    /// First piece of every live file that is not a photo/video (any other extension), sorted by start: the scan
    /// consumes the head there without a finding (Task 7). There is no pre-skip list: nothing is skipped before the
    /// search (R10).
    pub others: Vec<Fragment>,
}

/// Lists the files of every volume (cancel checked per directory). Never fails: a broken file system yields fewer
/// files. Reads only file-system metadata (boot sectors, FATs, directories), each once but for the FAT sectors a small
/// cache drops.
pub fn read(source: &dyn Source, cancel: &CancelToken) -> Listing {
    read_with(source, cancel, &Limits::DEFAULT)
}

pub(crate) fn read_with(source: &dyn Source, cancel: &CancelToken, limits: &Limits) -> Listing {
    let mut out = Listing {
        volumes: volumes(source),
        ..Listing::default()
    };
    let mut budget = Budget {
        files: limits.files,
        pieces: limits.pieces,
    };
    let vols = out.volumes.clone();
    for v in &vols {
        if cancel.is_cancelled() {
            break;
        }
        if v.cluster_count > MAX_WALKED_CLUSTERS {
            continue;
        }
        match v.fs {
            FsKind::Fat12 | FsKind::Fat16 | FsKind::Fat32 => {
                fat::walk(source, v, cancel, limits, &mut budget, &mut out)
            }
            FsKind::Exfat => {}
        }
    }
    out.files.sort_by(|a, b| {
        (a.pieces[0].start, a.origin.volume, &a.origin.path).cmp(&(
            b.pieces[0].start,
            b.origin.volume,
            &b.origin.path,
        ))
    });
    out.others.sort_by_key(|f| (f.start, f.len));
    out.live = merged(std::mem::take(&mut out.live));
    out
}

/// What is left of the listing's caps, shared by every volume.
pub(super) struct Budget {
    files: usize,
    pieces: usize,
}

impl Budget {
    /// Takes `pieces` (and a photo/video slot when `photo`); false, taking nothing, when either would run out.
    fn take(&mut self, pieces: usize, photo: bool) -> bool {
        if pieces > self.pieces || (photo && self.files == 0) {
            return false;
        }
        self.pieces -= pieces;
        self.files -= usize::from(photo);
        true
    }
}

struct Pending {
    origin: FsOrigin,
    kind: Option<FileKind>,
    runs: Vec<(u32, u32)>,
    /// The chain alone allows trust (complete, no conflict).
    chain_ok: bool,
}

struct PendingDir {
    runs: Vec<(u32, u32)>,
    chain_ok: bool,
}

/// The live files and directories of one volume as the walk finds them; trust is settled in [`Collector::finish`],
/// once every chain of the volume has claimed its clusters.
pub(super) struct Collector<'a> {
    volume: &'a Volume,
    source_len: u64,
    files: Vec<Pending>,
    dirs: Vec<PendingDir>,
}

impl<'a> Collector<'a> {
    pub(super) fn new(volume: &'a Volume, source_len: u64) -> Self {
        Collector {
            volume,
            source_len,
            files: Vec::new(),
            dirs: Vec::new(),
        }
    }

    /// A live file and its chain; not listed when it has no cluster or the budget runs out.
    pub(super) fn file(&mut self, budget: &mut Budget, origin: FsOrigin, chain: Chain) {
        if chain.runs.is_empty() {
            return;
        }
        let kind = extension(origin.name()).and_then(FileKind::from_extension);
        if !budget.take(chain.runs.len(), kind.is_some()) {
            return;
        }
        self.files.push(Pending {
            origin,
            kind,
            chain_ok: chain.complete && !chain.conflict,
            runs: chain.runs,
        });
    }

    /// A live directory's chain (its ranges are kept when the budget allows and it turns out trusted).
    pub(super) fn dir(&mut self, budget: &mut Budget, chain: &Chain) {
        if chain.runs.is_empty() || !budget.take(chain.runs.len(), false) {
            return;
        }
        self.dirs.push(PendingDir {
            runs: chain.runs.clone(),
            chain_ok: chain.ended && !chain.conflict,
        });
    }

    /// Settles trust (no contested cluster, the contested set not overflowed: ruling C11) and adds everything to `out`.
    /// A directory's ranges enter `live` only when it is trusted (ruling C10).
    pub(super) fn finish(self, claims: &Claims, out: &mut Listing) {
        let trusted = |ok: bool, runs: &[(u32, u32)]| {
            ok && !claims.overflowed() && !claims.touches_contested(runs)
        };
        for d in &self.dirs {
            if trusted(d.chain_ok, &d.runs) {
                out.live.extend(self.pieces(&d.runs, u64::MAX));
            }
        }
        for f in &self.files {
            let pieces = self.pieces(&f.runs, f.origin.size);
            let Some(&first) = pieces.first() else {
                continue;
            };
            out.live.extend_from_slice(&pieces);
            match f.kind {
                Some(kind) => out.files.push(FsFile {
                    trusted: trusted(f.chain_ok, &f.runs),
                    origin: f.origin.clone(),
                    kind,
                    pieces,
                }),
                None => out.others.push(first),
            }
        }
    }

    /// Absolute pieces of `runs`, the first `size` bytes, without what lies past the source's end.
    fn pieces(&self, runs: &[(u32, u32)], size: u64) -> Vec<Fragment> {
        let mut left = size;
        let mut out = Vec::new();
        for &(first, n) in runs {
            if left == 0 {
                break;
            }
            let Some(start) = self.volume.cluster_offset(first) else {
                break;
            };
            let len = (n as u64)
                .saturating_mul(self.volume.cluster_size)
                .min(left);
            left -= len;
            let end = start.saturating_add(len).min(self.source_len);
            if end > start {
                out.push(Fragment {
                    start,
                    len: end - start,
                });
            }
        }
        out
    }
}

/// The text after the last '.' of a name, if any.
fn extension(name: &str) -> Option<&str> {
    name.rsplit_once('.').map(|(_, e)| e)
}

/// A path component as read from the disk: '/' and NUL become '_' (so `name()`/`folder()` never split it) and an
/// empty one becomes "_".
pub(super) fn component(raw: &str) -> String {
    if raw.is_empty() {
        return "_".to_string();
    }
    raw.chars()
        .map(|c| if c == '/' || c == '\0' { '_' } else { c })
        .collect()
}

/// Sorted, with overlapping or touching ranges joined.
fn merged(mut ranges: Vec<Fragment>) -> Vec<Fragment> {
    ranges.sort_by_key(|r| r.start);
    let mut out: Vec<Fragment> = Vec::with_capacity(ranges.len());
    for r in ranges {
        match out.last_mut() {
            Some(last) if r.start <= last.start + last.len => {
                let end = (r.start + r.len).max(last.start + last.len);
                last.len = end - last.start;
            }
            _ => out.push(r),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn components_never_hold_a_separator() {
        assert_eq!(component("a/b\0c"), "a_b_c");
        assert_eq!(component(""), "_");
        assert_eq!(component("DSC01234.ARW"), "DSC01234.ARW");
    }

    #[test]
    fn name_and_folder() {
        let mut o = FsOrigin {
            volume: 0,
            path: "DCIM/100MSDCF/DSC01234.ARW".into(),
            state: FsState::Live,
            placement: Placement::Chain,
            size: 1,
            created: None,
            modified: None,
        };
        assert_eq!((o.name(), o.folder()), ("DSC01234.ARW", "DCIM/100MSDCF"));
        o.path = "ROOT.JPG".into();
        assert_eq!((o.name(), o.folder()), ("ROOT.JPG", ""));
        assert_eq!(FsState::Orphaned.as_str(), "orphaned");
        assert_eq!(Placement::Guessed.as_str(), "guessed");
    }

    #[test]
    fn time_display_appends_a_known_offset() {
        let t = |m| FsTime {
            local: "2025-12-14T07:12:04".into(),
            utc_offset_minutes: m,
        };
        assert_eq!(t(None).to_string(), "2025-12-14T07:12:04");
        assert_eq!(t(Some(-180)).to_string(), "2025-12-14T07:12:04-03:00");
        assert_eq!(t(Some(345)).to_string(), "2025-12-14T07:12:04+05:45");
        assert_eq!(t(Some(0)).to_string(), "2025-12-14T07:12:04+00:00");
    }

    #[test]
    fn ranges_merge_when_they_touch() {
        let f = |start, len| Fragment { start, len };
        assert_eq!(
            merged(vec![f(10, 5), f(0, 10), f(30, 1), f(12, 2)]),
            vec![f(0, 15), f(30, 1)]
        );
    }
}
