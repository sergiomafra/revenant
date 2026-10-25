//! The rescue copy (design §5.4, rulings D15–D17): a whole source copied into a sparse image file next to its map, in
//! passes that get the easy bytes first and touch bad sectors as little as possible, resumable from the map at any point.
//!
//! The passes, written from the design (no code or text taken from other tools):
//!
//! - **Fast**: the untried bytes in block-aligned chunks; a chunk that fails becomes `ToTrim` and the next `skip` bytes
//!   are jumped over (left `Untried`), the jump doubling on each consecutive failure up to `max_skip`, back to `min_skip`
//!   on a good chunk. A failing area is left early instead of being read block by block.
//! - **Skipped**: what the jumps left, in the same chunks, without jumping (a failure → `ToTrim`).
//! - **Trim**: each `ToTrim` run sector by sector from its start until a sector fails (→ `Bad`), then from its end
//!   backward until one fails (→ `Bad`); the middle becomes `ToScrape`.
//! - **Scrape**: each `ToScrape` sector once: read → `Rescued`, failed → `Bad`.
//! - **Retry(n)**, only when asked (`retries`, default 0, so a dying card is not hammered): each `Bad` sector read once
//!   more.
//!
//! A vanished device (`DeviceGone`, or a read coming back short) stops the copy with nothing marked; so does a cancel.
//! The map is saved (after the image's `sync_data`) every `save_every` or `save_bytes` rescued, at each pass change and
//! when the copy stops.

use std::fs::{File, OpenOptions};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::map::{MAX_MAP_REGIONS, MapTotals, Pass, RegionState, RescueMap, map_path};
use crate::naming::sanitize;
use crate::rate::RateMeter;
use crate::scan::CancelToken;
use crate::source::Source;
use crate::{Destination, EngineError, SourceIdentity};

/// Largest jump of the fast pass (D15), before the `size / 64` limit.
const MAX_SKIP: u64 = 1 << 30;
/// Largest block read at once (bounds the copy's buffer whatever the options say).
const MAX_BLOCK: u64 = 64 << 20;
/// Largest sector accepted.
const MAX_SECTOR: u32 = 1 << 20;

/// How the copy reads and when it saves its map.
#[derive(Clone, Debug)]
pub struct CopyOptions {
    /// The granularity of the map and of the sector-by-sector passes: against a device, its
    /// [`DeviceSource::scan_sector`](crate::source::DeviceSource::scan_sector), so a bad physical sector is read once (P8).
    pub sector: u32,
    /// The chunk of the fast and skipped passes (rounded down to the sector, at most 64 MiB).
    pub block: u64,
    /// The first jump after a failed chunk (rounded up to the sector).
    pub min_skip: u64,
    /// Extra reads of each bad sector at the end (0: none).
    pub retries: u32,
    /// Longest time between two map saves.
    pub save_every: Duration,
    /// Most bytes rescued between two map saves.
    pub save_bytes: u64,
}

impl Default for CopyOptions {
    /// Sector 512, block 1 MiB, min_skip 64 KiB, no retries, saved every 30 s or 256 MiB.
    fn default() -> CopyOptions {
        CopyOptions {
            sector: 512,
            block: 1 << 20,
            min_skip: 64 << 10,
            retries: 0,
            save_every: Duration::from_secs(30),
            save_bytes: 256 << 20,
        }
    }
}

/// What the copy reports while it runs.
#[derive(Clone, Debug, PartialEq)]
pub enum CopyEvent {
    /// After each read: the pass, the position reached, the bytes in each state, the speed (bytes read per second,
    /// smoothed) and the seconds left at that speed, when known. The last one of a finished copy has `Pass::Done`.
    Progress {
        pass: Pass,
        pos: u64,
        totals: MapTotals,
        bytes_per_sec: u64,
        eta_secs: Option<u64>,
    },
    /// A region just marked `Bad`.
    Bad { start: u64, len: u64 },
    /// The map was written.
    Saved,
}

/// How a copy ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopySummary {
    pub totals: MapTotals,
    /// Nothing is left `Untried`, `ToTrim` or `ToScrape`.
    pub complete: bool,
}

/// Copies `source` into `dest.dir()/image` (D15–D17): created sparse at `source.len()` when absent; else resumed from
/// its map (`<image>.rvmap`), whose identity must be `same_source` as `identity` (`map_other_source`), and whose size and
/// sector must be the source's and `opts.sector` (`map_mismatch`, also when the image's length differs).
///
/// Before creating or touching anything (P12): when `dest.info()` is present, the destination's largest file must hold
/// `source.len()` and its free space what is still to copy (the whole length on a fresh copy, the bytes not yet
/// `Rescued` on a resume), else `write_failed` and no file is created. A `Destination` without info (an image source)
/// skips these two checks (P19). The image is never split. `image` must be a plain, safe file name (`write_failed`
/// otherwise); an existing `image` that is not a regular file is refused (`write_failed`), and the folder must still be
/// on the volume it was checked on (`destination_on_source`).
///
/// The source is read directly (no cache): a read coming back short before `source.len()` is `DeviceGone`. On
/// `DeviceGone`, a cancel (`cancelled`) or a write error the map is saved and the error returned; nothing is marked for
/// the read that stopped it. `retries > 0` on a finished map runs only the repetitions it has not counted yet: a copy
/// that ends after `n` repetitions stores `Pass::Retry(n)` at the end of the source, a copy that ends without any
/// stores `Pass::Done`.
pub fn copy(
    source: &dyn Source,
    identity: &SourceIdentity,
    dest: &Destination,
    image: &str,
    opts: &CopyOptions,
    cancel: &CancelToken,
    on_event: &mut dyn FnMut(CopyEvent),
) -> Result<CopySummary, EngineError> {
    copy_with_cap(
        source,
        identity,
        dest,
        image,
        opts,
        cancel,
        on_event,
        MAX_MAP_REGIONS,
    )
}

/// [`copy`] with a map of at most `map_cap` regions (ruling P1, for the unit test of the cap).
///
/// Once the map is full (D16), what is left is finished coarsely, never failing: each `ToTrim` and `ToScrape` region
/// becomes `Bad` whole without being read; the untried bytes are read in blocks, and a failed block is marked `Bad`
/// whole when the map has room for it, else the rest of that untried stretch is marked `Bad` unread. Good sectors near
/// bad ones are lost that way, never data marked rescued that was not read.
#[allow(clippy::too_many_arguments)]
pub(crate) fn copy_with_cap(
    source: &dyn Source,
    identity: &SourceIdentity,
    dest: &Destination,
    image: &str,
    opts: &CopyOptions,
    cancel: &CancelToken,
    on_event: &mut dyn FnMut(CopyEvent),
    map_cap: usize,
) -> Result<CopySummary, EngineError> {
    if opts.sector == 0 || opts.sector > MAX_SECTOR || sanitize(image).as_deref() != Some(image) {
        return Err(invalid());
    }
    dest.still_checked()?;
    let image_path = dest.dir().join(image);
    let map_file = map_path(&image_path);
    let (file, map, fresh) = open(
        source,
        identity,
        dest,
        opts,
        &image_path,
        &map_file,
        map_cap,
    )?;
    let mut copier = Copier::new(source, file, map, map_file, opts, cancel, on_event);
    if fresh {
        // The map exists from the start, so the image never lies there without one.
        if let Err(e) = copier.save() {
            drop(copier);
            let _ = std::fs::remove_file(&image_path);
            return Err(e);
        }
    }
    match copier.run() {
        Ok(()) => {
            copier.save()?;
            copier.progress(Pass::Done, source.len());
            let totals = copier.totals;
            Ok(CopySummary {
                totals,
                complete: totals.untried + totals.to_trim + totals.to_scrape == 0,
            })
        }
        Err(e) => {
            // The map keeps what was done; the error that stopped the copy is the one reported.
            let _ = copier.save();
            Err(e)
        }
    }
}

fn invalid() -> EngineError {
    EngineError::Write {
        cause: std::io::Error::from(ErrorKind::InvalidInput),
    }
}

/// Opens the image and its map: resumed when the image exists, else created (after the room checks).
fn open(
    source: &dyn Source,
    identity: &SourceIdentity,
    dest: &Destination,
    opts: &CopyOptions,
    image: &Path,
    map_file: &Path,
    map_cap: usize,
) -> Result<(File, RescueMap, bool), EngineError> {
    let size = source.len();
    let write = |cause| EngineError::Write { cause };
    match std::fs::symlink_metadata(image) {
        Ok(meta) => {
            if !meta.is_file() {
                return Err(invalid());
            }
            let mut map = RescueMap::load(map_file)?;
            if !map.identity().same_source(identity) {
                return Err(EngineError::MapOtherSource);
            }
            if map.size() != size || meta.len() != size || map.sector() != opts.sector {
                return Err(EngineError::MapMismatch);
            }
            check_room(dest, size, size - map.totals().rescued)?;
            map.set_cap(map_cap);
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(image)
                .map_err(write)?;
            Ok((file, map, false))
        }
        Err(e) if e.kind() == ErrorKind::NotFound => {
            check_room(dest, size, size)?;
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(image)
                .map_err(write)?;
            // Sparse: nothing is allocated until written (D17).
            if let Err(cause) = file.set_len(size) {
                drop(file);
                let _ = std::fs::remove_file(image);
                return Err(write(cause));
            }
            let map = RescueMap::with_cap(size, opts.sector, identity.clone(), map_cap);
            Ok((file, map, true))
        }
        Err(cause) => Err(EngineError::Open { cause }),
    }
}

/// P12: the destination's file-size limit must hold the whole image and its free space what is still to copy.
fn check_room(dest: &Destination, size: u64, missing: u64) -> Result<(), EngineError> {
    let Some(info) = dest.info() else {
        return Ok(());
    };
    if info.max_file_bytes.is_some_and(|max| max < size) {
        return Err(EngineError::Write {
            cause: std::io::Error::from(ErrorKind::FileTooLarge),
        });
    }
    if info.free_bytes < missing {
        return Err(EngineError::Write {
            cause: std::io::Error::from(ErrorKind::StorageFull),
        });
    }
    Ok(())
}

#[cfg(unix)]
fn write_at(file: &File, buf: &[u8], offset: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.write_all_at(buf, offset)
}

#[cfg(windows)]
fn write_at(file: &File, buf: &[u8], offset: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut done = 0;
    while done < buf.len() {
        let n = file.seek_write(&buf[done..], offset + done as u64)?;
        if n == 0 {
            return Err(std::io::Error::from(ErrorKind::WriteZero));
        }
        done += n;
    }
    Ok(())
}

/// The bytes of `totals` in `state`.
fn field(totals: &mut MapTotals, state: RegionState) -> &mut u64 {
    match state {
        RegionState::Untried => &mut totals.untried,
        RegionState::Rescued => &mut totals.rescued,
        RegionState::ToTrim => &mut totals.to_trim,
        RegionState::ToScrape => &mut totals.to_scrape,
        RegionState::Bad => &mut totals.bad,
    }
}

/// A pass stopped because the map is full (D16): the rest is finished coarsely.
struct Full;

struct Copier<'a> {
    source: &'a dyn Source,
    image: File,
    map: RescueMap,
    map_file: PathBuf,
    /// The map's totals, kept up to date by `mark`.
    totals: MapTotals,
    size: u64,
    sector: u64,
    block: u64,
    min_skip: u64,
    max_skip: u64,
    retries: u32,
    save_every: Duration,
    save_bytes: u64,
    cancel: &'a CancelToken,
    on_event: &'a mut dyn FnMut(CopyEvent),
    rate: RateMeter,
    /// Bytes read or tried by this run.
    done: u64,
    last_save: Instant,
    rescued_since_save: u64,
    buf: Vec<u8>,
}

impl<'a> Copier<'a> {
    fn new(
        source: &'a dyn Source,
        image: File,
        map: RescueMap,
        map_file: PathBuf,
        opts: &CopyOptions,
        cancel: &'a CancelToken,
        on_event: &'a mut dyn FnMut(CopyEvent),
    ) -> Copier<'a> {
        let size = source.len();
        let sector = u64::from(opts.sector);
        let block = (opts.block.min(MAX_BLOCK) / sector * sector).max(sector);
        let min_skip = opts.min_skip.div_ceil(sector).max(1) * sector;
        let max_skip = (MAX_SKIP.min(size / 64) / sector * sector).max(min_skip);
        Copier {
            source,
            image,
            totals: map.totals(),
            map,
            map_file,
            size,
            sector,
            block,
            min_skip,
            max_skip,
            retries: opts.retries,
            save_every: opts.save_every,
            save_bytes: opts.save_bytes,
            cancel,
            on_event,
            rate: RateMeter::new(),
            done: 0,
            last_save: Instant::now(),
            rescued_since_save: 0,
            buf: vec![0; block as usize],
        }
    }

    /// Runs the passes from where the map is.
    fn run(&mut self) -> Result<(), EngineError> {
        self.rate.sample(0, Instant::now());
        loop {
            let (pass, pos) = self.map.pass();
            let outcome = match pass {
                Pass::Fast => self.untried(pos, true)?,
                Pass::Skipped => self.untried(pos, false)?,
                Pass::Trim => self.trim(pos)?,
                Pass::Scrape => self.scrape(pos)?,
                Pass::Retry(n) if pos >= self.size => {
                    if n >= self.retries {
                        return Ok(());
                    }
                    self.next_pass(Pass::Retry(n + 1), 0)?;
                    continue;
                }
                // An unfinished repetition beyond what is asked now stays where it is, for a later copy asking for it.
                Pass::Retry(n) if n > self.retries => return Ok(()),
                Pass::Retry(n) => {
                    self.retry(n, pos)?;
                    self.next_pass(Pass::Retry(n), self.size)?;
                    continue;
                }
                Pass::Done if self.retries > 0 => {
                    self.next_pass(Pass::Retry(1), 0)?;
                    continue;
                }
                Pass::Done => return Ok(()),
            };
            let next = match outcome {
                Err(Full) => {
                    self.coarse()?;
                    None
                }
                Ok(()) => match pass {
                    Pass::Fast => Some(Pass::Skipped),
                    Pass::Skipped => Some(Pass::Trim),
                    Pass::Trim => Some(Pass::Scrape),
                    _ => None,
                },
            };
            match next {
                Some(p) => self.next_pass(p, 0)?,
                None if self.retries > 0 => self.next_pass(Pass::Retry(1), 0)?,
                None => self.next_pass(Pass::Done, self.size)?,
            }
        }
    }

    fn next_pass(&mut self, pass: Pass, pos: u64) -> Result<(), EngineError> {
        self.map.set_pass(pass, pos);
        self.save()
    }

    /// The first run in `state` at or after `from`, clipped to start there, as `[start, end)`.
    fn next_run(&self, from: u64, state: RegionState) -> Option<(u64, u64)> {
        self.map
            .runs(from, self.size)
            .find(|r| r.2 == state)
            .map(|(s, l, _)| (s, s + l))
    }

    /// The end of the block-aligned chunk starting at `p`, within `end`.
    fn chunk_end(&self, p: u64, end: u64) -> u64 {
        ((p / self.block + 1) * self.block).min(end)
    }

    /// The fast pass (`jump`) or the skipped pass over the untried bytes from `pos`.
    fn untried(&mut self, pos: u64, jump: bool) -> Result<Result<(), Full>, EngineError> {
        let pass = if jump { Pass::Fast } else { Pass::Skipped };
        let mut p = pos;
        let mut skip = self.min_skip;
        while let Some((start, run_end)) = self.next_run(p, RegionState::Untried) {
            let end = self.chunk_end(start, run_end);
            if self.read(start, end)? {
                if !self.mark(start, end, RegionState::Rescued) {
                    return Ok(Err(Full));
                }
                skip = self.min_skip;
                p = end;
            } else {
                if !self.mark(start, end, RegionState::ToTrim) {
                    return Ok(Err(Full));
                }
                p = end;
                if jump {
                    p = end.saturating_add(skip).min(self.size);
                    skip = (skip * 2).min(self.max_skip);
                }
            }
            self.map.set_pass(pass, p);
            self.step(p)?;
        }
        Ok(Ok(()))
    }

    /// The trim pass: the edges of each `ToTrim` run from `pos`, sector by sector.
    fn trim(&mut self, pos: u64) -> Result<Result<(), Full>, EngineError> {
        let mut p = pos;
        while let Some((start, end)) = self.next_run(p, RegionState::ToTrim) {
            self.map.set_pass(Pass::Trim, start);
            let mut a = start;
            let mut failed = false;
            while a < end && !failed {
                let b = (a + self.sector).min(end);
                failed = !self.read(a, b)?;
                let state = if failed {
                    RegionState::Bad
                } else {
                    RegionState::Rescued
                };
                if !self.mark(a, b, state) {
                    return Ok(Err(Full));
                }
                a = b;
                self.step(a)?;
            }
            let mut z = end;
            while z > a {
                let y = ((z - 1) / self.sector * self.sector).max(a);
                let ok = self.read(y, z)?;
                let state = if ok {
                    RegionState::Rescued
                } else {
                    RegionState::Bad
                };
                if !self.mark(y, z, state) {
                    return Ok(Err(Full));
                }
                z = y;
                self.step(y)?;
                if !ok {
                    break;
                }
            }
            if a < z && !self.mark(a, z, RegionState::ToScrape) {
                return Ok(Err(Full));
            }
            p = end;
        }
        Ok(Ok(()))
    }

    /// The scrape pass: each `ToScrape` sector from `pos`, once.
    fn scrape(&mut self, pos: u64) -> Result<Result<(), Full>, EngineError> {
        let mut p = pos;
        while let Some((start, end)) = self.next_run(p, RegionState::ToScrape) {
            let mut a = start;
            while a < end {
                let b = (a + self.sector).min(end);
                let state = if self.read(a, b)? {
                    RegionState::Rescued
                } else {
                    RegionState::Bad
                };
                if !self.mark(a, b, state) {
                    return Ok(Err(Full));
                }
                a = b;
                self.map.set_pass(Pass::Scrape, a);
                self.step(a)?;
            }
            p = end;
        }
        Ok(Ok(()))
    }

    /// Repetition `n`: each `Bad` sector from `pos` read once more. A sector that reads but does not fit in a full map
    /// stays `Bad`.
    fn retry(&mut self, n: u32, pos: u64) -> Result<(), EngineError> {
        let mut p = pos;
        while let Some((start, end)) = self.next_run(p, RegionState::Bad) {
            let mut a = start;
            while a < end {
                let b = (a + self.sector).min(end);
                if self.read(a, b)? {
                    self.mark(a, b, RegionState::Rescued);
                }
                a = b;
                self.map.set_pass(Pass::Retry(n), a);
                self.step(a)?;
            }
            p = end;
        }
        Ok(())
    }

    /// Finishes a full map (D16) without ever needing a new region: see [`copy_with_cap`].
    fn coarse(&mut self) -> Result<(), EngineError> {
        let mut p = 0;
        loop {
            let next = self.map.runs(p, self.size).find(|r| {
                matches!(
                    r.2,
                    RegionState::Untried | RegionState::ToTrim | RegionState::ToScrape
                )
            });
            let Some((start, len, state)) = next else {
                return Ok(());
            };
            let end = start + len;
            if state == RegionState::Untried {
                self.coarse_untried(start, end)?;
            } else {
                // A whole region replaced: never more regions.
                self.mark(start, end, RegionState::Bad);
                self.step(end)?;
            }
            p = end;
        }
    }

    /// The untried region `[start, end)` of a full map, in blocks. Everything before `start` is final.
    fn coarse_untried(&mut self, start: u64, end: u64) -> Result<(), EngineError> {
        let mut p = start;
        // The start of good blocks read that the map had no room to mark yet.
        let mut pending: Option<u64> = None;
        while p < end {
            let q = self.chunk_end(p, end);
            let ok = self.read(p, q)?;
            match (ok, pending) {
                (true, None) => {
                    if !self.mark(p, q, RegionState::Rescued) {
                        pending = Some(p);
                    }
                }
                (true, Some(_)) => {}
                (false, None) if self.mark(p, q, RegionState::Bad) => {}
                (false, from) => {
                    // No room: the rest of this stretch, a whole region, is bad (unread past here).
                    self.mark(from.unwrap_or(p), end, RegionState::Bad);
                    self.step(end)?;
                    return Ok(());
                }
            }
            p = q;
            self.step(p)?;
        }
        if let Some(from) = pending {
            self.mark(from, end, RegionState::Rescued);
            self.step(end)?;
        }
        Ok(())
    }

    /// Reads `[start, end)` and writes it into the image at the same offset: `Ok(true)` when read and written,
    /// `Ok(false)` on a read error (a bad sector). A cancel, a vanished device (also a short read) and a write error stop
    /// the copy.
    fn read(&mut self, start: u64, end: u64) -> Result<bool, EngineError> {
        if self.cancel.is_cancelled() {
            return Err(EngineError::Cancelled);
        }
        let len = (end - start) as usize;
        let buf = &mut self.buf[..len];
        match self.source.read_at(start, buf) {
            Ok(n) if n == len => {
                write_at(&self.image, buf, start).map_err(|cause| EngineError::Write { cause })?;
                self.done += len as u64;
                Ok(true)
            }
            Ok(n) => Err(EngineError::DeviceGone {
                offset: start + n as u64,
            }),
            Err(EngineError::Read { .. }) => {
                self.done += len as u64;
                Ok(false)
            }
            Err(e) => Err(e),
        }
    }

    /// Sets `[start, end)` (`end` a sector boundary or the size) to `state`, keeping the totals; false when the map is
    /// full (nothing changed).
    fn mark(&mut self, start: u64, end: u64, state: RegionState) -> bool {
        let len = (end - start).div_ceil(self.sector) * self.sector;
        let mut old = MapTotals::default();
        for (_, l, s) in self.map.runs(start, end) {
            *field(&mut old, s) += l;
        }
        if !self.map.set(start, len, state) {
            return false;
        }
        for s in RegionState::ALL {
            *field(&mut self.totals, s) -= *field(&mut old, s);
        }
        *field(&mut self.totals, state) += end - start;
        match state {
            RegionState::Rescued => self.rescued_since_save += end - start,
            RegionState::Bad => (self.on_event)(CopyEvent::Bad {
                start,
                len: end - start,
            }),
            _ => {}
        }
        true
    }

    /// After a read: progress, and a save when one is due.
    fn step(&mut self, pos: u64) -> Result<(), EngineError> {
        let now = Instant::now();
        self.rate.sample(self.done, now);
        self.progress(self.map.pass().0, pos);
        if self.rescued_since_save >= self.save_bytes
            || now.saturating_duration_since(self.last_save) >= self.save_every
        {
            self.save()?;
        }
        Ok(())
    }

    fn progress(&mut self, pass: Pass, pos: u64) {
        let t = self.totals;
        let mut remaining = t.untried + t.to_trim + t.to_scrape;
        if matches!(pass, Pass::Retry(_)) {
            remaining += t.bad;
        }
        (self.on_event)(CopyEvent::Progress {
            pass,
            pos,
            totals: t,
            bytes_per_sec: self.rate.bytes_per_sec(),
            eta_secs: self.rate.eta_secs(remaining),
        });
    }

    /// The image's data synced, then the map saved over the previous one (D16).
    fn save(&mut self) -> Result<(), EngineError> {
        self.image
            .sync_data()
            .map_err(|cause| EngineError::Write { cause })?;
        self.map.save(&self.map_file)?;
        self.last_save = Instant::now();
        self.rescued_since_save = 0;
        (self.on_event)(CopyEvent::Saved);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{FakeDevices, SimSource};

    const MIB: u64 = 1 << 20;

    #[test]
    fn map_cap_degrades_without_failing() {
        let size = 64 * MIB;
        let bytes: Vec<u8> = (0..size).map(|i| (i * 31 % 251) as u8).collect();
        // A bad sector every 2 MiB, 1 MiB into each pair.
        let bad: Vec<u64> = (0..32).map(|k| k * 2 * MIB + MIB + 4096).collect();
        let mut source = SimSource::new(bytes.clone());
        for &b in &bad {
            source = source.bad(b, 512);
        }
        let dir = tempfile::tempdir().unwrap();
        let dest = Destination::check(&FakeDevices::new(), &source, dir.path()).unwrap();
        let identity = SourceIdentity::Image { size, digest: 1 };
        let mut events = Vec::new();
        let summary = copy_with_cap(
            &source,
            &identity,
            &dest,
            "card.img",
            &CopyOptions::default(),
            &CancelToken::default(),
            &mut |e| events.push(e),
            16,
        )
        .unwrap();
        assert!(summary.complete);
        let image = dest.dir().join("card.img");
        let map = RescueMap::load(&map_path(&image)).unwrap();
        let regions = map.regions();
        assert!(regions.len() <= 16, "{}", regions.len());
        assert_eq!(map.totals(), summary.totals);
        // The degrade: whole blocks (or more) are bad, never a bad sector called rescued, and what is rescued is the
        // source's bytes.
        for &b in &bad {
            assert_eq!(map.state_at(b), RegionState::Bad, "{b}");
        }
        assert!(summary.totals.bad > 32 * 512);
        assert!(summary.totals.rescued > 0);
        let img = std::fs::read(&image).unwrap();
        for (start, len, state) in regions {
            if state == RegionState::Rescued {
                let r = start as usize..(start + len) as usize;
                assert!(img[r.clone()] == bytes[r], "{start}");
            }
        }
        // Each bad sector read at most twice (its failing block, then at most once sector by sector), never hammered.
        for &b in &bad {
            let n = source
                .reads()
                .iter()
                .filter(|&&(o, l)| o <= b && o + l as u64 > b)
                .count();
            assert!(n <= 2, "{b}: {n} reads");
        }
    }
}
