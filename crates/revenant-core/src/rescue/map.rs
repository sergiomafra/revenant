//! The rescue map: the state of every region of the copy, the pass the copy is in, and crash-safe saving (ruling D16).

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use super::text;
use crate::{EngineError, SourceIdentity};

/// Most regions a map holds. A `set` that would need more is refused (the copy then marks a failed block bad whole
/// instead of splitting it, ruling D16).
pub const MAX_MAP_REGIONS: usize = 1 << 20;
/// Largest map file `load` reads: 2^20 region lines of at most 52 bytes fit, with room for the header.
const MAX_MAP_FILE: u64 = 64 << 20;

/// What a region of the copy holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegionState {
    /// Not read yet.
    Untried,
    /// Read: the image holds the card's bytes.
    Rescued,
    /// A failed block whose edges are still to be read sector by sector.
    ToTrim,
    /// A failed block's middle, still to be read sector by sector.
    ToScrape,
    /// Sectors that failed when read one by one.
    Bad,
}

impl RegionState {
    /// Every state, in the order above.
    pub const ALL: [RegionState; 5] = [
        RegionState::Untried,
        RegionState::Rescued,
        RegionState::ToTrim,
        RegionState::ToScrape,
        RegionState::Bad,
    ];

    /// Stable name, as written in the map file.
    pub fn as_str(self) -> &'static str {
        match self {
            RegionState::Untried => "untried",
            RegionState::Rescued => "rescued",
            RegionState::ToTrim => "to_trim",
            RegionState::ToScrape => "to_scrape",
            RegionState::Bad => "bad",
        }
    }
}

/// The pass of the copy (design §5.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pass {
    /// First pass: large blocks, skipping ahead after an error.
    Fast,
    /// Re-reading what the fast pass skipped, without skipping.
    Skipped,
    /// The edges of failed blocks, sector by sector.
    Trim,
    /// The middle of failed blocks, sector by sector.
    Scrape,
    /// Re-reading bad sectors; the retry number, from 1.
    Retry(u32),
    /// Finished.
    Done,
}

impl Pass {
    /// Stable name (the map file writes `retry:N` for `Retry(N)`).
    pub fn as_str(self) -> &'static str {
        match self {
            Pass::Fast => "fast",
            Pass::Skipped => "skipped",
            Pass::Trim => "trim",
            Pass::Scrape => "scrape",
            Pass::Retry(_) => "retry",
            Pass::Done => "done",
        }
    }
}

/// Bytes of the map in each state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MapTotals {
    pub untried: u64,
    pub rescued: u64,
    pub to_trim: u64,
    pub to_scrape: u64,
    pub bad: u64,
}

/// The map of a rescue copy: merged regions covering `[0, size)` (neighbors always differ in state), the source's
/// identity, and the pass with its position. Two maps are equal when all of that is (the region cap is not compared).
#[derive(Clone, Debug)]
pub struct RescueMap {
    size: u64,
    sector: u32,
    identity: SourceIdentity,
    /// Region start → state; a region ends where the next starts (the last at `size`). Empty only when `size` is 0.
    regions: BTreeMap<u64, RegionState>,
    pass: Pass,
    pos: u64,
    cap: usize,
}

impl PartialEq for RescueMap {
    fn eq(&self, other: &RescueMap) -> bool {
        // `cap` is a limit of this copy in memory, not part of what the map says (a loaded map has the default one).
        let RescueMap {
            size,
            sector,
            identity,
            regions,
            pass,
            pos,
            cap: _,
        } = self;
        *size == other.size
            && *sector == other.sector
            && *identity == other.identity
            && *regions == other.regions
            && *pass == other.pass
            && *pos == other.pos
    }
}

impl Eq for RescueMap {}

/// The map file of an image: `<image>.rvmap`.
pub fn map_path(image: &Path) -> PathBuf {
    let mut name = OsString::from(image.as_os_str());
    name.push(".rvmap");
    PathBuf::from(name)
}

/// The temporary file a save writes first: `<map>.tmp`.
fn tmp_path(map: &Path) -> PathBuf {
    let mut name = OsString::from(map.as_os_str());
    name.push(".tmp");
    PathBuf::from(name)
}

impl RescueMap {
    /// A map of `size` bytes (sector of `sector` bytes, at least 1), all `Untried`, in `Pass::Fast` at 0.
    pub fn new(size: u64, sector: u32, identity: SourceIdentity) -> RescueMap {
        RescueMap::with_cap(size, sector, identity, MAX_MAP_REGIONS)
    }

    /// [`RescueMap::new`] holding at most `cap` regions (ruling P1).
    pub(crate) fn with_cap(
        size: u64,
        sector: u32,
        identity: SourceIdentity,
        cap: usize,
    ) -> RescueMap {
        let mut regions = BTreeMap::new();
        if size > 0 {
            regions.insert(0, RegionState::Untried);
        }
        RescueMap {
            size,
            sector: sector.max(1),
            identity,
            regions,
            pass: Pass::Fast,
            pos: 0,
            cap: cap.max(1),
        }
    }

    /// A map read from its file, with its regions as they are (the file was checked to be well formed).
    pub(super) fn from_parts(
        size: u64,
        sector: u32,
        identity: SourceIdentity,
        regions: BTreeMap<u64, RegionState>,
        (pass, pos): (Pass, u64),
    ) -> RescueMap {
        RescueMap {
            size,
            sector,
            identity,
            regions,
            pass,
            pos,
            cap: MAX_MAP_REGIONS,
        }
    }

    /// Changes the cap of regions (ruling P1: a copy resumed under a test cap). Regions already there are kept.
    pub(crate) fn set_cap(&mut self, cap: usize) {
        self.cap = cap.max(1);
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn sector(&self) -> u32 {
        self.sector
    }

    /// The source the copy reads.
    pub fn identity(&self) -> &SourceIdentity {
        &self.identity
    }

    /// Sets `[start, start + len)` to `state`; `start` and `len` are multiples of the sector, and the end clamps to the
    /// size. Returns false, changing nothing, when they are not, or when the map would need more than its cap of regions
    /// ([`MAX_MAP_REGIONS`]).
    pub fn set(&mut self, start: u64, len: u64, state: RegionState) -> bool {
        let sector = u64::from(self.sector);
        if !start.is_multiple_of(sector) || !len.is_multiple_of(sector) {
            return false;
        }
        let end = start.saturating_add(len).min(self.size);
        if start >= end {
            return true;
        }
        let before = (start > 0).then(|| self.state_at(start - 1));
        let after = (end < self.size).then(|| self.state_at(end));
        let inside: Vec<u64> = self
            .regions
            .range(start + 1..end)
            .map(|(&k, _)| k)
            .collect();
        let need_start = before != Some(state);
        let need_end = after.is_some_and(|a| a != state);
        let count = self.regions.len()
            - inside.len()
            - usize::from(self.regions.contains_key(&start))
            - usize::from(self.regions.contains_key(&end))
            + usize::from(need_start)
            + usize::from(need_end);
        if count > self.cap {
            return false;
        }
        for k in inside {
            self.regions.remove(&k);
        }
        if need_start {
            self.regions.insert(start, state);
        } else {
            self.regions.remove(&start);
        }
        if let Some(after) = after {
            if need_end {
                self.regions.insert(end, after);
            } else {
                self.regions.remove(&end);
            }
        }
        debug_assert_eq!(self.regions.len(), count);
        true
    }

    /// The state of the byte at `offset` (`Untried` past the end).
    pub fn state_at(&self, offset: u64) -> RegionState {
        if offset >= self.size {
            return RegionState::Untried;
        }
        self.regions
            .range(..=offset)
            .next_back()
            .map_or(RegionState::Untried, |(_, &s)| s)
    }

    /// The regions as `(start, len, state)`: merged, ascending, contiguous from 0, summing to the size.
    pub fn regions(&self) -> Vec<(u64, u64, RegionState)> {
        self.runs(0, self.size).collect()
    }

    /// The regions overlapping `[start, end)`, clipped to it, as `(start, len, state)`.
    pub(crate) fn runs(
        &self,
        start: u64,
        end: u64,
    ) -> impl Iterator<Item = (u64, u64, RegionState)> + '_ {
        let end = end.min(self.size);
        let first = self
            .regions
            .range(..=start)
            .next_back()
            .map_or(0, |(&k, _)| k);
        let mut iter = self.regions.range(first..end.max(first)).peekable();
        std::iter::from_fn(move || {
            let (&s, &state) = iter.next()?;
            let e = iter.peek().map_or(self.size, |&(&k, _)| k).min(end);
            let s = s.max(start);
            Some((s, e.saturating_sub(s), state))
        })
        .filter(|&(_, len, _)| len > 0)
    }

    /// The first byte of `[start, end)` that is not `Rescued`.
    pub(crate) fn first_unrescued(&self, start: u64, end: u64) -> Option<u64> {
        self.runs(start, end)
            .find(|&(_, _, s)| s != RegionState::Rescued)
            .map(|(s, _, _)| s)
    }

    /// Bytes in each state.
    pub fn totals(&self) -> MapTotals {
        let mut t = MapTotals::default();
        for (_, len, state) in self.runs(0, self.size) {
            *match state {
                RegionState::Untried => &mut t.untried,
                RegionState::Rescued => &mut t.rescued,
                RegionState::ToTrim => &mut t.to_trim,
                RegionState::ToScrape => &mut t.to_scrape,
                RegionState::Bad => &mut t.bad,
            } += len;
        }
        t
    }

    /// The pass the copy is in and its position in it.
    pub fn pass(&self) -> (Pass, u64) {
        (self.pass, self.pos)
    }

    pub fn set_pass(&mut self, pass: Pass, pos: u64) {
        self.pass = pass;
        self.pos = pos;
    }

    /// Reads a map file. A file that cannot be opened or read is `open_failed`; anything wrong in it (cut short, a
    /// checksum that does not match, regions that do not cover the size, more than [`MAX_MAP_REGIONS`]) is
    /// `map_corrupt`.
    pub fn load(path: &Path) -> Result<RescueMap, EngineError> {
        let open = |cause| EngineError::Open { cause };
        let mut bytes = Vec::new();
        File::open(path)
            .map_err(open)?
            .take(MAX_MAP_FILE + 1)
            .read_to_end(&mut bytes)
            .map_err(open)?;
        if bytes.len() as u64 > MAX_MAP_FILE {
            return Err(EngineError::MapCorrupt);
        }
        text::decode(&bytes).ok_or(EngineError::MapCorrupt)
    }

    /// Writes the map to `path` so that a crash at any moment leaves either the previous file or this one: into a new
    /// `<path>.tmp` (whatever was there is removed first, a link included, never what it points to), `sync_all`, renamed over `path`, then (on Unix) the folder synced. Failures are `write_failed`,
    /// except the folder sync, which is best effort (ruling P33): the new file is complete and synced by then, and some
    /// file systems refuse to sync a folder; at worst a crash right after brings the previous map back.
    ///
    /// A map whose identity nests `mapped` deeper than the file format reads back (4) is refused (`write_failed`,
    /// nothing written), so a map that saves always loads.
    pub fn save(&self, path: &Path) -> Result<(), EngineError> {
        let write = |cause| EngineError::Write { cause };
        if !text::identity_fits(&self.identity) {
            return Err(write(std::io::Error::from(
                std::io::ErrorKind::InvalidInput,
            )));
        }
        let tmp = tmp_path(path);
        // Never through something planted at `<path>.tmp`: a link there is removed (not its target), and the file is
        // created exclusively, which never follows a link (a link planted meanwhile makes the save fail instead).
        match std::fs::remove_file(&tmp) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(write(e)),
            _ => {}
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .map_err(write)?;
        file.write_all(&text::encode(self)).map_err(write)?;
        file.sync_all().map_err(write)?;
        drop(file);
        std::fs::rename(&tmp, path).map_err(write)?;
        #[cfg(unix)]
        {
            let dir = match path.parent() {
                Some(p) if !p.as_os_str().is_empty() => p,
                _ => Path::new("."),
            };
            let _ = File::open(dir).and_then(|d| d.sync_all());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_cap_refuses_new_regions() {
        let identity = SourceIdentity::Image {
            size: 1 << 20,
            digest: 1,
        };
        let mut map = RescueMap::with_cap(1 << 20, 512, identity, 8);
        // Bad sectors every other sector: each adds two regions until the cap.
        for i in 0..3u64 {
            assert!(map.set(1024 + i * 2048, 512, RegionState::Bad));
        }
        assert_eq!(map.regions().len(), 7);
        let before = map.clone();
        // Region 8 fits (a Bad run reaching the end splits off one region)...
        assert!(map.set((1 << 20) - 512, 512, RegionState::Bad));
        assert_eq!(map.regions().len(), 8);
        let at_cap = map.clone();
        // ...region 9 does not: refused, nothing changed.
        assert!(!map.set(0, 512, RegionState::Bad));
        assert_eq!(map, at_cap);
        assert!(!map.set(100 * 512, 512, RegionState::Bad));
        assert_eq!(map, at_cap);
        // A set that keeps or lowers the count still works at the cap.
        assert!(map.set(1024, 512, RegionState::Untried));
        assert_eq!(map.regions().len(), 6);
        assert!(map.set(0, 1 << 20, RegionState::Rescued));
        assert_eq!(map.regions(), [(0, 1 << 20, RegionState::Rescued)]);
        assert_ne!(map, before);
        // Misaligned ranges are refused too.
        assert!(!map.set(100, 512, RegionState::Bad));
        assert!(!map.set(0, 100, RegionState::Bad));
        assert_eq!(map.regions(), [(0, 1 << 20, RegionState::Rescued)]);
    }

    #[test]
    fn equality_ignores_the_cap() {
        let identity = SourceIdentity::Image {
            size: 1 << 20,
            digest: 1,
        };
        let small = RescueMap::with_cap(1 << 20, 512, identity.clone(), 8);
        let mut big = RescueMap::new(1 << 20, 512, identity);
        assert_eq!(small, big);
        big.set_cap(3);
        assert_eq!(small, big);
        assert!(big.set(0, 512, RegionState::Bad));
        assert_ne!(small, big);
        let mut moved = small.clone();
        moved.set_pass(Pass::Trim, 0);
        assert_ne!(small, moved);
    }
}
