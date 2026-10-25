//! The files the volumes of a source list: where each one's pieces are, its original path and times, and whether its
//! placement can be trusted.

use std::fmt;
use std::sync::Arc;

use super::alloc::{Chain, Claims};
use super::fat_deleted::Probes;
use super::{
    FsKind, Limits, MAX_WALKED_CLUSTERS, Probe, Scheme, SourceProbe, Volume, exfat, fat, volumes,
};
use crate::EngineError;
use crate::codec::{Codec, Dec, Enc};
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
/// of a deleted file; Guessed = deleted-file heuristic (D4); Carved = the scan's own measurement at the file's start
/// replaced the guess (D5; set by the scan only).
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
    /// The folder holding the file: components joined by '/', root-relative, no leading '/', "" for the root; "?" for a
    /// directory whose name is unknown. `None` when it was dropped: the path would pass [`super::MAX_PATH_BYTES`] or the
    /// listing's path budget ran out ([`Listing::truncated`]). Files of one folder share the string.
    pub folder: Option<Arc<str>>,
    /// The file's name (one component, never holding '/').
    pub name: String,
    pub state: FsState,
    pub placement: Placement,
    /// As recorded in the entry.
    pub size: u64,
    pub created: Option<FsTime>,
    pub modified: Option<FsTime>,
}

impl FsOrigin {
    /// The file's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The folder holding the file ("" for the root), `None` when it was dropped.
    pub fn folder(&self) -> Option<&str> {
        self.folder.as_deref()
    }

    /// `folder/name` (the name alone in the root, or when the folder was dropped).
    pub fn path(&self) -> String {
        match self.folder() {
            Some(f) if !f.is_empty() => format!("{f}/{}", self.name),
            _ => self.name.clone(),
        }
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
    /// Ruling D3: the placement is trusted (a live file: a complete chain, no cycle nor cross-link; a deleted file: one
    /// run of clusters free today, as many as its size needs).
    pub trusted: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Listing {
    pub volumes: Vec<Volume>,
    /// Photo/video files only (extension known to `FileKind::from_extension`), sorted by (first piece start, volume,
    /// folder, name).
    pub files: Vec<FsFile>,
    /// Merged absolute ranges of every live file (any kind, trusted or not) and of every trusted live directory
    /// (D7 cap, `keep_moovs`, orphans, D18). Not in it: the fixed FAT12/16 root region, a directory that is not trusted
    /// (ruling R8), and a directory or file left out by the piece or photo cap.
    pub live: Vec<Fragment>,
    /// First piece of every live file that is not a photo/video (any other extension), sorted by start: the scan
    /// consumes the head there without a finding. There is no pre-skip list: nothing is skipped before the
    /// search (R10).
    pub others: Vec<Fragment>,
    /// A cap or budget dropped or degraded something (ruling R7): a file left out by the piece or photo cap, a folder
    /// dropped by the path cap or budget, a volume's trust lost to the contested-set overflow, a directory or entries
    /// not read because of the directory, depth, entry or directory-size caps, a volume too large to walk, deleted
    /// files not listed because a volume's guess budget or the listing's high-word probes ran out, or deleted folders
    /// not walked because too many were waiting.
    pub truncated: bool,
    /// Merged absolute ranges of every directory cluster the walks read, live or deleted, trusted or not, FAT and exFAT
    /// (at most [`super::MAX_PIECES`] runs per volume before merging): an old directory found there by the scan is no
    /// orphan (ruling D11).
    pub(crate) walked: Vec<Fragment>,
}

/// Lists the files of every volume (cancel checked per directory), live and deleted. Never fails: a broken file system
/// yields fewer files. Reads file-system metadata (boot sectors, FATs, directories), each once but for the FAT sectors
/// a small cache drops, plus at most 65,536 heads of 512 bytes for the FAT32 high-word check (ruling D10).
pub fn read(source: &dyn Source, cancel: &CancelToken) -> Listing {
    read_with(source, &SourceProbe::new(source), cancel)
}

/// [`read`] with metadata read through `meta` (the scan's cache) and the high-word heads through `probe` (the raw
/// source, ruling C2).
pub(crate) fn read_with(meta: &dyn Source, probe: &dyn Probe, cancel: &CancelToken) -> Listing {
    read_limited(meta, probe, cancel, &Limits::DEFAULT)
}

pub(crate) fn read_limited(
    source: &dyn Source,
    probe: &dyn Probe,
    cancel: &CancelToken,
    limits: &Limits,
) -> Listing {
    let mut probes = Probes::new(probe, limits.probes);
    let mut out = Listing {
        volumes: volumes(source),
        ..Listing::default()
    };
    let mut budget = Budget {
        files: limits.files,
        pieces: limits.pieces,
        path_bytes: limits.path_bytes,
        paths: limits.path_budget,
        truncated: false,
    };
    let vols = out.volumes.clone();
    for v in &vols {
        if cancel.is_cancelled() {
            break;
        }
        if v.cluster_count > MAX_WALKED_CLUSTERS {
            budget.truncated = true;
            continue;
        }
        match v.fs {
            FsKind::Fat12 | FsKind::Fat16 | FsKind::Fat32 => fat::walk(
                source,
                v,
                cancel,
                limits,
                &mut budget,
                &mut probes,
                &mut out,
            ),
            FsKind::Exfat => exfat::walk(source, v, cancel, limits, &mut budget, &mut out),
        }
    }
    out.truncated |= budget.truncated || probes.ran_out();
    out.files.sort_by(|a, b| {
        let key = |f: &FsFile| (f.pieces[0].start, f.origin.volume, f.origin.folder.clone());
        key(a)
            .cmp(&key(b))
            .then_with(|| a.origin.name.cmp(&b.origin.name))
    });
    out.others.sort_by_key(|f| (f.start, f.len));
    out.live = merged(std::mem::take(&mut out.live));
    out.walked = merged(std::mem::take(&mut out.walked));
    out
}

/// Records that the walk read `len` bytes of directory at absolute `start` into `walked` (a run continuing the last
/// one extends it); once `walked` holds `cap` runs, further ones are not recorded (they only keep a few more clusters
/// from being looked at as old directories).
pub(super) fn note_walked(walked: &mut Vec<Fragment>, start: u64, len: u64, cap: usize) {
    if let Some(last) = walked.last_mut()
        && last.start.saturating_add(last.len) == start
    {
        last.len += len;
    } else if walked.len() < cap {
        walked.push(Fragment { start, len });
    }
}

/// What is left of the listing's caps, shared by every volume, and whether one of them dropped something.
pub(super) struct Budget {
    files: usize,
    pieces: usize,
    /// Longest path (folder + '/' + name) kept.
    path_bytes: usize,
    /// Bytes of folder strings still allowed.
    paths: usize,
    pub(super) truncated: bool,
}

impl Budget {
    /// Photo/video slots left.
    pub(super) fn files_left(&self) -> usize {
        self.files
    }

    /// Takes `pieces` (and a photo/video slot when `photo`); false, taking nothing and marking the listing truncated,
    /// when either would run out.
    fn take(&mut self, pieces: usize, photo: bool) -> bool {
        if pieces > self.pieces || (photo && self.files == 0) {
            self.truncated = true;
            return false;
        }
        self.pieces -= pieces;
        self.files -= usize::from(photo);
        true
    }

    /// The folder of child directory `name` of `parent`: `None` when the parent's was dropped (already marked), and
    /// `None` marking the listing truncated when the path would pass the path cap or the path budget cannot pay for it.
    pub(super) fn subfolder(&mut self, parent: Option<&Arc<str>>, name: &str) -> Option<Arc<str>> {
        let parent = parent?;
        let len = if parent.is_empty() {
            name.len()
        } else {
            parent.len() + 1 + name.len()
        };
        if len > self.path_bytes || len > self.paths {
            self.truncated = true;
            return None;
        }
        self.paths -= len;
        Some(if parent.is_empty() {
            Arc::from(name)
        } else {
            Arc::from(format!("{parent}/{name}"))
        })
    }
}

/// A file waiting for the end of its volume's walk. Only a photo/video keeps its origin, boxed, so another file costs
/// only this struct and its runs.
struct Pending {
    origin: Option<Box<FsOrigin>>,
    kind: Option<FileKind>,
    size: u64,
    runs: Vec<(u32, u32)>,
    /// The chain (or the guess) alone allows trust: complete, no conflict (deleted: one run, not short).
    chain_ok: bool,
    /// A live file: its ranges go to `live` (a deleted one's never do).
    live: bool,
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

    /// A live file and its chain; not listed when it has no cluster or the budget runs out. A photo/video whose path
    /// would pass the path cap keeps its name and loses its folder.
    pub(super) fn file(&mut self, budget: &mut Budget, mut origin: FsOrigin, chain: Chain) {
        if chain.runs.is_empty() {
            return;
        }
        let kind = extension(&origin.name).and_then(FileKind::from_extension);
        if !budget.take(chain.runs.len(), kind.is_some()) {
            return;
        }
        let size = origin.size;
        let origin = kind.map(|_| {
            if let Some(f) = &origin.folder
                && f.len() + 1 + origin.name.len() > budget.path_bytes
            {
                origin.folder = None;
                budget.truncated = true;
            }
            Box::new(origin)
        });
        self.files.push(Pending {
            origin,
            kind,
            size,
            chain_ok: chain.complete && !chain.conflict,
            runs: chain.runs,
            live: true,
        });
    }

    /// A deleted photo/video of `kind` and where its clusters probably are (FAT: the D4 guess; exFAT: the recorded run,
    /// the old chain or the guess), `ok` when that placement alone allows trust; never in `live`, never claiming. Not
    /// listed when it has no cluster or the budget runs out. As for a live file, a path past the path cap drops the
    /// folder.
    pub(super) fn deleted(
        &mut self,
        budget: &mut Budget,
        mut origin: FsOrigin,
        kind: FileKind,
        runs: Vec<(u32, u32)>,
        ok: bool,
    ) {
        if runs.is_empty() || !budget.take(runs.len(), true) {
            return;
        }
        if let Some(f) = &origin.folder
            && f.len() + 1 + origin.name.len() > budget.path_bytes
        {
            origin.folder = None;
            budget.truncated = true;
        }
        self.files.push(Pending {
            size: origin.size,
            origin: Some(Box::new(origin)),
            kind: Some(kind),
            chain_ok: ok,
            runs,
            live: false,
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

    /// Settles trust (no contested cluster, the contested set not overflowed: ruling C11, which marks the listing
    /// truncated) and adds everything to `out`. A directory's ranges enter `live` only when it is trusted (ruling R8).
    /// Each pending file is consumed and its origin moved (never copied), so a name is held once.
    pub(super) fn finish(self, claims: &Claims, out: &mut Listing) {
        if claims.overflowed() {
            out.truncated = true;
        }
        let trusted = |ok: bool, runs: &[(u32, u32)]| {
            ok && !claims.overflowed() && !claims.touches_contested(runs)
        };
        let Collector {
            volume,
            source_len,
            files,
            dirs,
        } = self;
        for d in dirs {
            if trusted(d.chain_ok, &d.runs) {
                out.live
                    .extend(pieces(volume, source_len, &d.runs, u64::MAX));
            }
        }
        for f in files {
            let pieces = pieces(volume, source_len, &f.runs, f.size);
            let Some(&first) = pieces.first() else {
                continue;
            };
            if f.live {
                out.live.extend_from_slice(&pieces);
            }
            match (f.kind, f.origin) {
                (Some(kind), Some(origin)) => out.files.push(FsFile {
                    trusted: trusted(f.chain_ok, &f.runs),
                    origin: *origin,
                    kind,
                    pieces,
                }),
                _ if f.live => out.others.push(first),
                _ => {}
            }
        }
    }
}

/// Absolute pieces of `runs` of `volume`, the first `size` bytes, without what lies past `source_len`.
fn pieces(volume: &Volume, source_len: u64, runs: &[(u32, u32)], size: u64) -> Vec<Fragment> {
    let mut left = size;
    let mut out = Vec::with_capacity(runs.len());
    for &(first, n) in runs {
        if left == 0 {
            break;
        }
        let Some(start) = volume.cluster_offset(first) else {
            break;
        };
        let len = (n as u64).saturating_mul(volume.cluster_size).min(left);
        left -= len;
        let end = start.saturating_add(len).min(source_len);
        if end > start {
            out.push(Fragment {
                start,
                len: end - start,
            });
        }
    }
    out
}

/// The text after the last '.' of a name, if any.
pub(super) fn extension(name: &str) -> Option<&str> {
    name.rsplit_once('.').map(|(_, e)| e)
}

/// A path component as read from the disk: '/' and NUL become '_' (so `name()`/`folder()` never split it) and an
/// empty one becomes "_".
pub(super) fn component(raw: &str) -> String {
    if raw.is_empty() {
        return "_".to_string();
    }
    // Same length out as in ('/' and NUL are one byte, as is '_'), allocated exactly once.
    let mut out = String::with_capacity(raw.len());
    out.extend(
        raw.chars()
            .map(|c| if c == '/' || c == '\0' { '_' } else { c }),
    );
    out
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

// ---- Saved form (ruling D26): a scan's listing is stored once in its session and never read again (D23) ----

const FS_STATES: [FsState; 3] = [FsState::Live, FsState::Deleted, FsState::Orphaned];
const PLACEMENTS: [Placement; 4] = [
    Placement::Chain,
    Placement::Recorded,
    Placement::Guessed,
    Placement::Carved,
];
const FS_KINDS: [FsKind; 4] = [FsKind::Fat12, FsKind::Fat16, FsKind::Fat32, FsKind::Exfat];
const SCHEMES: [Scheme; 3] = [Scheme::None, Scheme::Mbr, Scheme::Gpt];

impl Codec for FsState {
    fn enc(&self, e: &mut Enc) {
        e.str(self.as_str());
    }
    fn dec(d: &mut Dec) -> Result<Self, EngineError> {
        d.named(&FS_STATES, FsState::as_str)
    }
}

impl Codec for Placement {
    fn enc(&self, e: &mut Enc) {
        e.str(self.as_str());
    }
    fn dec(d: &mut Dec) -> Result<Self, EngineError> {
        d.named(&PLACEMENTS, Placement::as_str)
    }
}

impl Codec for FsKind {
    fn enc(&self, e: &mut Enc) {
        e.str(self.as_str());
    }
    fn dec(d: &mut Dec) -> Result<Self, EngineError> {
        d.named(&FS_KINDS, FsKind::as_str)
    }
}

impl Codec for Scheme {
    fn enc(&self, e: &mut Enc) {
        e.str(self.as_str());
    }
    fn dec(d: &mut Dec) -> Result<Self, EngineError> {
        d.named(&SCHEMES, Scheme::as_str)
    }
}

impl Codec for FsTime {
    fn enc(&self, e: &mut Enc) {
        e.str(&self.local);
        e.opt(
            self.utc_offset_minutes
                .map(|m| u32::from(m as u16))
                .as_ref(),
        );
    }
    fn dec(d: &mut Dec) -> Result<Self, EngineError> {
        let local = d.str()?;
        let offset = match d.opt::<u32>()? {
            Some(m) => Some(u16::try_from(m).map_err(|_| EngineError::SessionCorrupt)? as i16),
            None => None,
        };
        Ok(FsTime {
            local,
            utc_offset_minutes: offset,
        })
    }
}

impl Codec for FsOrigin {
    fn enc(&self, e: &mut Enc) {
        let FsOrigin {
            volume,
            folder,
            name,
            state,
            placement,
            size,
            created,
            modified,
        } = self;
        e.u32(*volume);
        e.opt(folder.as_ref());
        e.str(name);
        e.put(state);
        e.put(placement);
        e.u64(*size);
        e.opt(created.as_ref());
        e.opt(modified.as_ref());
    }
    fn dec(d: &mut Dec) -> Result<Self, EngineError> {
        Ok(FsOrigin {
            volume: d.u32()?,
            folder: d.opt()?,
            name: d.str()?,
            state: d.get()?,
            placement: d.get()?,
            size: d.u64()?,
            created: d.opt()?,
            modified: d.opt()?,
        })
    }
}

impl Codec for Volume {
    fn enc(&self, e: &mut Enc) {
        let Volume {
            index,
            fs,
            offset,
            len,
            sector_size,
            cluster_size,
            heap_offset,
            cluster_count,
            label,
            serial,
            from_backup,
            scheme,
        } = self;
        e.u32(*index);
        e.put(fs);
        e.u64(*offset);
        e.u64(*len);
        e.u32(*sector_size);
        e.u64(*cluster_size);
        e.u64(*heap_offset);
        e.u32(*cluster_count);
        e.opt(label.as_ref());
        e.u32(*serial);
        e.bool(*from_backup);
        e.put(scheme);
    }
    fn dec(d: &mut Dec) -> Result<Self, EngineError> {
        Ok(Volume {
            index: d.u32()?,
            fs: d.get()?,
            offset: d.u64()?,
            len: d.u64()?,
            sector_size: d.u32()?,
            cluster_size: d.u64()?,
            heap_offset: d.u64()?,
            cluster_count: d.u32()?,
            label: d.opt()?,
            serial: d.u32()?,
            from_backup: d.bool()?,
            scheme: d.get()?,
        })
    }
}

impl Codec for FsFile {
    fn enc(&self, e: &mut Enc) {
        let FsFile {
            origin,
            kind,
            pieces,
            trusted,
        } = self;
        e.put(origin);
        e.put(kind);
        e.seq(pieces);
        e.bool(*trusted);
    }
    fn dec(d: &mut Dec) -> Result<Self, EngineError> {
        let f = FsFile {
            origin: d.get()?,
            kind: d.get()?,
            pieces: d.seq()?,
            trusted: d.bool()?,
        };
        // Every listed file has a first piece (the scan keys on it).
        if f.pieces.is_empty() {
            return Err(EngineError::SessionCorrupt);
        }
        Ok(f)
    }
}

impl Codec for Listing {
    fn enc(&self, e: &mut Enc) {
        let Listing {
            volumes,
            files,
            live,
            others,
            truncated,
            walked,
        } = self;
        e.seq(volumes);
        e.seq(files);
        e.seq(live);
        e.seq(others);
        e.bool(*truncated);
        e.seq(walked);
    }
    fn dec(d: &mut Dec) -> Result<Self, EngineError> {
        let volumes: Vec<Volume> = d.seq()?;
        let n = d.count()?;
        let mut files = Vec::with_capacity(crate::codec::prealloc(n));
        // Files of one folder share its string again, as when listed.
        let mut folders: std::collections::HashMap<Arc<str>, ()> = Default::default();
        for _ in 0..n {
            let mut f: FsFile = d.get()?;
            if let Some(folder) = f.origin.folder.take() {
                let shared = match folders.get_key_value(&folder) {
                    Some((k, _)) => Arc::clone(k),
                    None => {
                        folders.insert(Arc::clone(&folder), ());
                        folder
                    }
                };
                f.origin.folder = Some(shared);
            }
            files.push(f);
        }
        let listing = Listing {
            volumes,
            files,
            live: d.seq()?,
            others: d.seq()?,
            truncated: d.bool()?,
            walked: d.seq()?,
        };
        let sorted = listing
            .files
            .windows(2)
            .all(|w| w[0].pieces[0].start <= w[1].pieces[0].start);
        if listing.volumes.len() > super::MAX_VOLUMES || !sorted {
            return Err(EngineError::SessionCorrupt);
        }
        Ok(listing)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listing_round_trips_and_shares_folders() {
        use crate::codec::{from_bytes, to_bytes};
        let folder: Arc<str> = "DCIM/100MSDCF".into();
        let file = |start: u64, name: &str| FsFile {
            origin: FsOrigin {
                volume: 0,
                folder: Some(Arc::clone(&folder)),
                name: name.into(),
                state: FsState::Live,
                placement: Placement::Chain,
                size: 1000,
                created: None,
                modified: Some(FsTime {
                    local: "2026-10-25T11:00:00".into(),
                    utc_offset_minutes: Some(330),
                }),
            },
            kind: FileKind::Jpeg,
            pieces: vec![Fragment { start, len: 1000 }],
            trusted: true,
        };
        let listing = Listing {
            volumes: vec![volume()],
            files: vec![file(4096, "A.JPG"), file(8192, "B.JPG")],
            live: vec![Fragment {
                start: 4096,
                len: 8192,
            }],
            others: vec![Fragment {
                start: 65536,
                len: 512,
            }],
            truncated: true,
            walked: vec![Fragment {
                start: 2048,
                len: 512,
            }],
        };
        let bytes = to_bytes(&listing);
        let back: Listing = from_bytes(&bytes).unwrap();
        assert_eq!(back, listing);
        assert_eq!(to_bytes(&back), bytes);
        let (a, b) = (&back.files[0].origin.folder, &back.files[1].origin.folder);
        assert!(Arc::ptr_eq(a.as_ref().unwrap(), b.as_ref().unwrap()));
        for cut in 0..bytes.len() {
            assert!(from_bytes::<Listing>(&bytes[..cut]).is_err(), "cut {cut}");
        }
        // Files out of order are corruption.
        let mut swapped = listing.clone();
        swapped.files.swap(0, 1);
        assert!(matches!(
            from_bytes::<Listing>(&to_bytes(&swapped)),
            Err(EngineError::SessionCorrupt)
        ));
    }

    fn volume() -> Volume {
        Volume {
            index: 0,
            fs: FsKind::Fat32,
            offset: 0,
            len: 1 << 20,
            sector_size: 512,
            cluster_size: 512,
            heap_offset: 0,
            cluster_count: 1000,
            label: None,
            serial: 0,
            from_backup: false,
            scheme: super::super::Scheme::None,
        }
    }

    fn chain(first: u32) -> Chain {
        Chain {
            runs: vec![(first, 1)],
            complete: true,
            conflict: false,
            ended: true,
        }
    }

    /// The peak-memory rule (fix round 2): a name is held once. `finish` moves each photo's origin into the listing, so
    /// the name and folder in the output are the very buffers the walk allocated (a copy would be a new allocation),
    /// and a file of another kind keeps no origin at all.
    #[test]
    fn finish_moves_origins_instead_of_copying_them() {
        let v = volume();
        let mut budget = Budget {
            files: 10,
            pieces: 10,
            path_bytes: 4096,
            paths: 1 << 20,
            truncated: false,
        };
        let folder: Arc<str> = Arc::from("DCIM");
        let origin = |name: &str| FsOrigin {
            volume: 0,
            folder: Some(folder.clone()),
            name: component(&format!("{}{name}", "\u{FFFD}".repeat(250))),
            state: FsState::Live,
            placement: Placement::Chain,
            size: 512,
            created: None,
            modified: None,
        };
        let photo = origin("A.JPG");
        let other = origin("B.TXT");
        let name_at = photo.name.as_ptr();
        let mut col = Collector::new(&v, 1 << 20);
        col.file(&mut budget, photo, chain(2));
        col.file(&mut budget, other, chain(3));
        assert!(col.files[0].origin.is_some());
        assert!(
            col.files[1].origin.is_none(),
            "another kind keeps no origin"
        );
        let claims = Claims::new(1000, 16);
        let mut out = Listing::default();
        col.finish(&claims, &mut out);
        assert_eq!(out.files.len(), 1);
        assert_eq!(out.others.len(), 1);
        let o = &out.files[0].origin;
        assert_eq!(o.name.as_ptr(), name_at, "the name was copied");
        assert!(Arc::ptr_eq(o.folder.as_ref().unwrap(), &folder));
        // Retained strings are allocated exactly.
        assert_eq!(o.name.capacity(), o.name.len());
        assert_eq!(out.files[0].pieces.capacity(), out.files[0].pieces.len());
    }

    #[test]
    fn a_pending_file_of_another_kind_is_small() {
        // Its origin is boxed (one pointer) and only for photos/videos; the rest is the runs, size, kind and trust.
        assert!(
            std::mem::size_of::<Pending>() <= 48,
            "{}",
            std::mem::size_of::<Pending>()
        );
        assert!(std::mem::size_of::<FsOrigin>() > 48);
    }

    #[test]
    fn components_never_hold_a_separator() {
        assert_eq!(component("a/b\0c"), "a_b_c");
        assert_eq!(component(""), "_");
        assert_eq!(component("DSC01234.ARW"), "DSC01234.ARW");
        let long = component(&"\u{FFFD}".repeat(260));
        assert_eq!((long.len(), long.capacity()), (780, 780));
    }

    #[test]
    fn name_folder_and_path() {
        let mut o = FsOrigin {
            volume: 0,
            folder: Some(Arc::from("DCIM/100MSDCF")),
            name: "DSC01234.ARW".into(),
            state: FsState::Live,
            placement: Placement::Chain,
            size: 1,
            created: None,
            modified: None,
        };
        assert_eq!(
            (o.name(), o.folder()),
            ("DSC01234.ARW", Some("DCIM/100MSDCF"))
        );
        assert_eq!(o.path(), "DCIM/100MSDCF/DSC01234.ARW");
        o.folder = Some(Arc::from(""));
        assert_eq!((o.folder(), o.path().as_str()), (Some(""), "DSC01234.ARW"));
        o.folder = None;
        assert_eq!((o.folder(), o.path().as_str()), (None, "DSC01234.ARW"));
        assert_eq!(FsState::Orphaned.as_str(), "orphaned");
        assert_eq!(Placement::Guessed.as_str(), "guessed");
    }

    #[test]
    fn subfolders_respect_the_path_cap_and_budget() {
        let budget = |path_bytes, paths| Budget {
            files: 1,
            pieces: 1,
            path_bytes,
            paths,
            truncated: false,
        };
        let root: Arc<str> = Arc::from("");
        // The cap alone (an ample budget): 4 + 1 + 4 = 9 bytes is past 8.
        let mut b = budget(8, 1000);
        let a = b.subfolder(Some(&root), "AAAA").unwrap();
        assert_eq!(&*a, "AAAA");
        assert!(!b.truncated);
        assert_eq!(b.subfolder(Some(&a), "BBBB"), None);
        assert!(b.truncated);
        assert_eq!(b.subfolder(Some(&a), "BBB").as_deref(), Some("AAAA/BBB"));
        // Under a dropped folder (already counted when it was dropped).
        b.truncated = false;
        assert_eq!(b.subfolder(None, "C"), None);
        assert!(!b.truncated);
        // The budget alone (an ample cap): 4 bytes paid, 2 left, "AAAA/BB" (7) does not fit.
        let mut b = budget(1000, 6);
        let a = b.subfolder(Some(&root), "AAAA").unwrap();
        assert_eq!(b.subfolder(Some(&a), "BB"), None);
        assert!(b.truncated);
        assert_eq!(b.subfolder(Some(&root), "CC").as_deref(), Some("CC"));
        assert_eq!(b.subfolder(Some(&root), "D"), None);
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
