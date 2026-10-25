//! Extraction: copies a finding's bytes from the source into a destination folder.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::EngineError;
use crate::device::{DestInfo, DeviceOps, SystemDevices};
use crate::naming::{fallback_name, file_name, is_unsafe_char, sanitize, split_name};
use crate::scan::Finding;
use crate::source::{Source, VolumeId, read_full, volume_of};

/// Copy granularity.
const CHUNK: usize = 1024 * 1024;
/// Granularity of the sector-by-sector retry after a failed chunk read.
const SECTOR: u64 = 512;
/// How many `_N` suffixes are tried before giving up.
const MAX_NAME_ATTEMPTS: u32 = 10_000;

/// Copies the finding's bytes into `dest_dir` under [`file_name`]`(finding, None)`: the original name of a finding with
/// a file-system origin, else `revenant_<start as 12 hex digits>.<ext>` (`_2`, `_3`… if it exists; never overwrites).
/// Same as [`extract_as`].
pub fn extract(
    source: &dyn Source,
    finding: &Finding,
    dest_dir: &Path,
) -> Result<PathBuf, EngineError> {
    extract_as(source, finding, dest_dir, &file_name(finding, None))
}

/// A destination folder checked once against a source (ruling D13): extraction, copies and sessions write only into a
/// `Destination`.
///
/// Refused (`destination_on_source`) when the folder is on the source's volume (an image file's) or when the folder's
/// disks (an APFS container counts as its physical stores) include the disk [`Source::disk`] names: a card is never
/// written to while it is being recovered, through any of its volumes. For a device source whose destination disks cannot
/// be determined (a network folder, a failed query) the folder is refused too (`destination_unknown`): it could be the
/// card. An image source (`disk()` is `None`) only gets the volume rule, and `ops` is not called.
///
/// The folder is resolved first (`std::fs::canonicalize`: symbolic links and, on Windows, junctions followed), and both
/// rules and every later write use the resolved folder: a link pointing into the card is judged, and written, as the
/// card.
#[derive(Clone, Debug)]
pub struct Destination {
    dir: PathBuf,
    volume: VolumeId,
    info: Option<DestInfo>,
}

impl Destination {
    /// Checks `dir` (an existing folder) against `source`. Errors: `open_failed` (the folder cannot be resolved or
    /// opened), `destination_on_source`, `destination_unknown`.
    pub fn check(
        ops: &dyn DeviceOps,
        source: &dyn Source,
        dir: &Path,
    ) -> Result<Destination, EngineError> {
        let dir = &std::fs::canonicalize(dir).map_err(|cause| EngineError::Open { cause })?;
        let dest_volume = volume_of(dir)?;
        if source.volume() == Some(dest_volume) {
            return Err(EngineError::DestinationOnSource);
        }
        let info = match source.disk() {
            None => None,
            Some(disk) => {
                let info = ops
                    .destination(dir)
                    .map_err(|_| EngineError::DestinationUnknown)?;
                if info.disks.is_empty() {
                    return Err(EngineError::DestinationUnknown);
                }
                if info.disks.contains(&disk) {
                    return Err(EngineError::DestinationOnSource);
                }
                Some(info)
            }
        };
        Ok(Destination {
            dir: dir.to_path_buf(),
            volume: dest_volume,
            info,
        })
    }

    /// The resolved folder (links followed).
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// What the folder lives on (disks, file system, free space, file-size limit); `None` for an image source.
    pub fn info(&self) -> Option<&DestInfo> {
        self.info.as_ref()
    }

    /// Refuses (`destination_on_source`) a folder no longer on the volume it was checked on (it could have been
    /// replaced since).
    pub(crate) fn still_checked(&self) -> Result<(), EngineError> {
        if volume_of(&self.dir)? != self.volume {
            return Err(EngineError::DestinationOnSource);
        }
        Ok(())
    }
}

/// Copies the finding's bytes into the checked destination under [`sanitize`]`(name)` (`revenant_<start as 12 hex
/// digits>.<ext>` when nothing is left of it), then `<stem>_2.<ext>`, `<stem>_3.<ext>`… when the name is taken (10,000
/// tries; never overwrites). Refuses (`destination_on_source`) when the folder is no longer on the volume it was checked
/// on.
///
/// The file is created with `create_new`, so a taken name is detected by the volume itself: it is right on case-sensitive
/// and case-insensitive volumes alike and for different Unicode forms of one name. The result is always a single
/// component inside the destination folder.
///
/// A file found in pieces is written as its pieces in file order. Stretches that cannot be read are written as zeros, so a
/// partial file comes out instead of none. Each piece stops at the source's end even if it runs past it.
/// A write error returns [`EngineError::Write`] and removes the incomplete file. A source that vanishes
/// ([`EngineError::DeviceGone`], or a read coming back short before the source's length) is never written as zeros: the
/// copy stops, the incomplete file is removed and `DeviceGone` is returned.
pub fn extract_to(
    source: &dyn Source,
    finding: &Finding,
    dest: &Destination,
    name: &str,
) -> Result<PathBuf, EngineError> {
    dest.still_checked()?;
    let (path, mut file) = create_named(dest.dir(), finding, name, None)?;
    if let Err(e) = copy(source, finding, &mut file) {
        drop(file);
        let _ = std::fs::remove_file(&path);
        return Err(e);
    }
    Ok(path)
}

/// [`Destination::check`] against the running system ([`SystemDevices`]), then [`extract_to`]: refuses a folder on the
/// source's volume and, for a device source, any volume of that device (or a folder whose disks cannot be told). Checks
/// on every call (cheap for an image; with a device, `revenant-dev` checks once and uses [`extract_to`]).
pub fn extract_as(
    source: &dyn Source,
    finding: &Finding,
    dest_dir: &Path,
    name: &str,
) -> Result<PathBuf, EngineError> {
    let dest = Destination::check(&SystemDevices, source, dest_dir)?;
    extract_to(source, finding, &dest, name)
}

/// Creates a new file in `dir` named after `name` and returns its path and handle: `sanitize(name)` (the finding's
/// `revenant_<start>.<ext>` when nothing is left), then `<stem>_2<ext>`, `<stem>_3<ext>`… when the name is taken (10,000
/// tries; never overwrites). `suffix` replaces the name's extension when given (`Some(".preview.jpg")`); it must be empty
/// or start with '.' and hold no separator, control or other unsafe char, else `write_failed` and nothing is created.
/// This is the only way out of the crate to create a file under a derived name, and the result always lies directly
/// inside `dir`.
pub fn create_named(
    dir: &Path,
    finding: &Finding,
    name: &str,
    suffix: Option<&str>,
) -> Result<(PathBuf, File), EngineError> {
    let name = sanitize(name).unwrap_or_else(|| fallback_name(finding));
    let (stem, ext) = split_name(&name);
    create_unique(dir, stem, suffix.unwrap_or(ext))
}

/// Whether `stem` and `suffix` join into one safe component.
fn safe_parts(stem: &str, suffix: &str) -> bool {
    let joined_ok = !stem.is_empty()
        && !stem.chars().any(is_unsafe_char)
        && !suffix.chars().any(is_unsafe_char)
        && (suffix.is_empty() || suffix.starts_with('.'));
    joined_ok && !matches!(format!("{stem}{suffix}").as_str(), "." | "..")
}

/// Creates `<stem><suffix>` in `dir`, else `<stem>_2<suffix>`… (never overwriting; at most 10,000 names tried). Refuses
/// (`write_failed`, nothing created) parts that are not a safe name, so nothing can leave `dir`.
pub(crate) fn create_unique(
    dir: &Path,
    stem: &str,
    suffix: &str,
) -> Result<(PathBuf, File), EngineError> {
    if !safe_parts(stem, suffix) {
        return Err(EngineError::Write {
            cause: std::io::Error::from(std::io::ErrorKind::InvalidInput),
        });
    }
    for n in 1..=MAX_NAME_ATTEMPTS {
        let name = if n == 1 {
            format!("{stem}{suffix}")
        } else {
            format!("{stem}_{n}{suffix}")
        };
        let path = dir.join(name);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(cause) => return Err(EngineError::Write { cause }),
        }
    }
    Err(EngineError::Write {
        cause: std::io::Error::other("no free file name"),
    })
}

/// Copies the finding's ranges in file order.
fn copy(source: &dyn Source, finding: &Finding, out: &mut File) -> Result<(), EngineError> {
    let write = |cause| EngineError::Write { cause };
    let mut buf = vec![0u8; CHUNK];
    for range in finding.ranges() {
        // Never past the source's end, whatever the finding claims.
        let end = range.start.saturating_add(range.len).min(source.len());
        let mut pos = range.start;
        while pos < end {
            let n = (end - pos).min(CHUNK as u64) as usize;
            read_lenient(source, pos, &mut buf[..n])?;
            out.write_all(&buf[..n]).map_err(write)?;
            pos += n as u64;
        }
    }
    out.flush().map_err(write)
}

/// Fills `buf` from `offset` (inside the source); anything unreadable becomes zeros. A vanished source (`DeviceGone`,
/// or a short read: `buf` lies inside the source's length) is an error, never zeros.
fn read_lenient(source: &dyn Source, offset: u64, buf: &mut [u8]) -> Result<(), EngineError> {
    match read_full(source, offset, buf) {
        Ok(()) => Ok(()),
        Err(e @ EngineError::DeviceGone { .. }) => Err(e),
        Err(_) => {
            // Retry sector by sector (aligned to absolute sector boundaries) so only bad sectors are lost.
            let mut done = 0;
            while done < buf.len() {
                let abs = offset + done as u64;
                let step = ((SECTOR - abs % SECTOR) as usize).min(buf.len() - done);
                let piece = &mut buf[done..done + step];
                match read_full(source, abs, piece) {
                    Ok(()) => {}
                    Err(e @ EngineError::DeviceGone { .. }) => return Err(e),
                    Err(_) => piece.fill(0),
                }
                done += step;
            }
            Ok(())
        }
    }
}
