//! The pre-flight check (design §5.3, ruling D14): one call, before any long read, that reports every problem already
//! known about reading a disk and copying it, as codes.
//!
//! In this order: the disk is found in the listing, opened read-only, unmounted (when asked), three sectors are read (the
//! first, the middle and the last logical sector, each on its own thread and waited for at most `sample_timeout`), and
//! the copy destination is checked (on the source? known disks? room and file-size limit for what is still to copy).
//! Nothing is written anywhere.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::device::{DeviceOps, DiskInfo, UnmountGuard};
use crate::rescue::{RescueMap, map_path, room_problems, still_to_copy};
use crate::source::{DeviceSource, DiskId, Source};
use crate::timed::{Timed, timed};
use crate::{Destination, EngineError, SourceIdentity};

/// How long each sample read is waited for (ruling D14). No `cfg(test)` twin: callers pass it in
/// [`PreflightRequest::sample_timeout`], and tests pass less (ruling P1).
pub const SAMPLE_TIMEOUT: Duration = Duration::from_secs(10);

/// What to check.
#[derive(Clone, Debug)]
pub struct PreflightRequest<'a> {
    pub disk: &'a DiskId,
    /// Unmount the disk's volumes (they stay unmounted: the guard is in [`Preflight::guard`]).
    pub unmount: bool,
    /// The image FILE a copy would write (ruling P10): its map is [`map_path`]`(copy_to)` and the destination checked is
    /// its parent folder. `None`: no copy planned, no destination checked.
    pub copy_to: Option<&'a Path>,
    /// How long each sample read is waited for: [`SAMPLE_TIMEOUT`] (ruling P1).
    pub sample_timeout: Duration,
}

/// A problem found by the pre-flight check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Issue {
    /// The disk cannot be opened for reading: root (macOS) or Administrator (Windows) is needed.
    PermissionDenied,
    /// The disk is not connected (or vanished during the check).
    DeviceNotFound,
    /// The disk holds the running system: it is never unmounted nor checked (ruling P38).
    SystemDisk,
    /// The system could not list the disks, or could not open the listed disk for another reason than permission
    /// (ruling P39).
    OpenFailed,
    /// The sample read at `offset` did not return within the timeout.
    NotResponding { offset: u64 },
    /// The sample read at `offset` failed.
    SampleUnreadable { offset: u64 },
    /// The unmount failed; `volumes` are still mounted (the system may write to them).
    UnmountFailed { volumes: Vec<String> },
    /// The destination has `free` bytes and the copy still needs `need`.
    DestinationTooSmall { need: u64, free: u64 },
    /// The destination folder is on the disk being read.
    DestinationOnSource,
    /// The destination's disks cannot be told (a network folder, a failed query, a folder that cannot be resolved).
    DestinationUnknown,
    /// The destination's file system holds files of at most `limit` bytes, less than the image (FAT32).
    DestinationFileTooLarge { limit: u64 },
    /// The session to resume was made from another disk (reported from Task 10 on).
    SessionOtherSource,
    /// The session to resume cannot be read, or its version is not supported (reported from Task 10 on).
    SessionCorrupt,
}

impl Issue {
    /// Stable `snake_case` code identifying the issue.
    pub fn code(&self) -> &'static str {
        match self {
            Self::PermissionDenied => "permission_denied",
            Self::DeviceNotFound => "device_not_found",
            Self::SystemDisk => "system_disk",
            Self::OpenFailed => "open_failed",
            Self::NotResponding { .. } => "device_not_responding",
            Self::SampleUnreadable { .. } => "sample_unreadable",
            Self::UnmountFailed { .. } => "unmount_failed",
            Self::DestinationTooSmall { .. } => "destination_too_small",
            Self::DestinationOnSource => "destination_on_source",
            Self::DestinationUnknown => "destination_unknown",
            Self::DestinationFileTooLarge { .. } => "destination_file_too_large",
            Self::SessionOtherSource => "session_other_source",
            Self::SessionCorrupt => "session_corrupt",
        }
    }
}

/// What the pre-flight check found.
#[derive(Debug)]
pub struct Preflight {
    /// The disk as listed, when found.
    pub disk: Option<DiskInfo>,
    /// Every issue found, in the order of the check; empty when nothing is known to be wrong.
    pub issues: Vec<Issue>,
    /// The unmount's guard, when one was asked and succeeded: keep it while the disk is read.
    pub guard: Option<UnmountGuard>,
}

/// Checks `req.disk` (ruling D14), in this order: find → open → unmount → samples → copy destination. Every issue found
/// is reported, in that order; `DeviceNotFound`, `SystemDisk`, `OpenFailed` and `PermissionDenied` end the check early
/// (nothing after them can be checked; nothing is unmounted when the disk cannot be opened).
///
/// - Find: a listing that fails is `OpenFailed` (P39); a disk not in it is `DeviceNotFound`; the system disk is
///   `SystemDisk` (P38), before anything is opened, unmounted or read.
/// - Open (read-only): `permission_denied` → `PermissionDenied`; `device_not_found` (gone meanwhile) →
///   `DeviceNotFound`; any other failure → `OpenFailed` (P39).
/// - Unmount: `unmount_failed` → `UnmountFailed` with the volumes still mounted (the check goes on: the disk can still be
///   read mounted); `device_not_found` → `DeviceNotFound`; any other failure (a failed system tool) → `UnmountFailed`
///   naming the listed volumes that have a mount point.
/// - Samples: the first, middle (`sectors / 2`) and last logical sector, one after another, each read on its own thread
///   and waited for at most `req.sample_timeout`. A read that does not return in time is `NotResponding` and ends the
///   sampling (its thread stays blocked, with the device open, until the system gives up on the read); the whole check
///   waits at most about 4 × `sample_timeout` (three samples and the identity read of a resumed copy's map). A failed
///   or short read is `SampleUnreadable`; a vanished device (`device_gone`) is `DeviceNotFound`.
/// - Copy destination (`req.copy_to`): [`Destination::check`] on its parent folder (`DestinationOnSource`, and
///   `DestinationUnknown` for any other failure); then the room the copy would check (P12): free space for what is still
///   to copy — the whole disk, or, when `map_path(copy_to)` holds a map of this same disk (`same_source`, read on a
///   thread under the same timeout), the bytes it does not call rescued — `DestinationTooSmall`; and the destination's
///   file-size limit against the disk's size, `DestinationFileTooLarge`. A map of another disk, of another size, or that
///   cannot be read counts as a fresh copy (the copy itself refuses it). The bad bytes of a map count as still to copy:
///   the check does not know whether retries will be asked.
pub fn preflight(ops: &dyn DeviceOps, req: &PreflightRequest) -> Preflight {
    let mut out = Preflight {
        disk: None,
        issues: Vec::new(),
        guard: None,
    };
    let info = match ops.list() {
        Ok(listed) => listed.into_iter().find(|d| d.id == *req.disk),
        Err(_) => {
            out.issues.push(Issue::OpenFailed);
            return out;
        }
    };
    let Some(info) = info else {
        out.issues.push(Issue::DeviceNotFound);
        return out;
    };
    out.disk = Some(info.clone());
    if info.system {
        out.issues.push(Issue::SystemDisk);
        return out;
    }
    let source = match ops.open(req.disk) {
        Ok(raw) => Arc::new(DeviceSource::new(raw, req.disk.clone())),
        Err(EngineError::PermissionDenied) => {
            out.issues.push(Issue::PermissionDenied);
            return out;
        }
        Err(EngineError::DeviceNotFound) => {
            out.issues.push(Issue::DeviceNotFound);
            return out;
        }
        Err(_) => {
            out.issues.push(Issue::OpenFailed);
            return out;
        }
    };
    if req.unmount {
        match ops.unmount(req.disk) {
            Ok(guard) => out.guard = Some(guard),
            Err(EngineError::DeviceNotFound) => {
                out.issues.push(Issue::DeviceNotFound);
                return out;
            }
            Err(EngineError::UnmountFailed { volumes }) => {
                out.issues.push(Issue::UnmountFailed { volumes })
            }
            Err(_) => out.issues.push(Issue::UnmountFailed {
                volumes: mounted_volumes(&info),
            }),
        }
    }
    let responding = match sample(&source, req.sample_timeout, &mut out.issues) {
        Sampled::Answered => true,
        Sampled::Hung => false,
        Sampled::Gone => {
            out.issues.push(Issue::DeviceNotFound);
            return out;
        }
    };
    if let Some(image) = req.copy_to {
        let room = CopyCheck {
            ops,
            source: &source,
            info: &info,
            image,
            timeout: req.sample_timeout,
            responding,
        };
        room.run(&mut out.issues);
    }
    out
}

/// The listed volumes of `info` that have a mount point.
fn mounted_volumes(info: &DiskInfo) -> Vec<String> {
    info.volumes
        .iter()
        .filter(|v| v.mount_point.is_some())
        .map(|v| v.id.clone())
        .collect()
}

/// How the sample reads went.
enum Sampled {
    /// Every sample returned (read or failed).
    Answered,
    /// One did not return in time.
    Hung,
    /// The device vanished.
    Gone,
}

/// Reads the first, middle and last logical sector of `source`, each on its own thread under `timeout`.
fn sample(source: &Arc<DeviceSource>, timeout: Duration, issues: &mut Vec<Issue>) -> Sampled {
    let geometry = source.geometry();
    let sector = u64::from(geometry.logical_sector);
    let sectors = geometry.size / sector;
    if sectors == 0 {
        issues.push(Issue::SampleUnreadable { offset: 0 });
        return Sampled::Answered;
    }
    let mut offsets = vec![0, sectors / 2 * sector, (sectors - 1) * sector];
    offsets.dedup();
    for offset in offsets {
        let device = source.clone();
        let read = timed(timeout, None, move || {
            let mut buf = vec![0u8; sector as usize];
            device.read_at(offset, &mut buf).map(|n| n == buf.len())
        });
        match read {
            Timed::Done(Ok(true)) => {}
            Timed::Done(Err(EngineError::DeviceGone { .. })) => return Sampled::Gone,
            Timed::Done(_) => issues.push(Issue::SampleUnreadable { offset }),
            Timed::Late | Timed::Cancelled => {
                issues.push(Issue::NotResponding { offset });
                return Sampled::Hung;
            }
        }
    }
    Sampled::Answered
}

/// The copy destination part of the check.
struct CopyCheck<'a> {
    ops: &'a dyn DeviceOps,
    source: &'a Arc<DeviceSource>,
    info: &'a DiskInfo,
    image: &'a Path,
    timeout: Duration,
    /// Every sample answered: the identity sectors may be read.
    responding: bool,
}

impl CopyCheck<'_> {
    fn run(&self, issues: &mut Vec<Issue>) {
        let dir = match self.image.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => Path::new("."),
        };
        let dest = match Destination::check(self.ops, &**self.source, dir) {
            Ok(dest) => dest,
            Err(EngineError::DestinationOnSource) => {
                issues.push(Issue::DestinationOnSource);
                return;
            }
            Err(_) => {
                issues.push(Issue::DestinationUnknown);
                return;
            }
        };
        let Some(dest_info) = dest.info() else {
            issues.push(Issue::DestinationUnknown);
            return;
        };
        let size = self.source.len();
        let problems = room_problems(dest_info, size, self.still_to_copy(size));
        if let Some((need, free)) = problems.short {
            issues.push(Issue::DestinationTooSmall { need, free });
        }
        if let Some(limit) = problems.file_limit {
            issues.push(Issue::DestinationFileTooLarge { limit });
        }
    }

    /// The bytes the copy still has to write: the whole disk, or what a map of this disk does not call rescued.
    fn still_to_copy(&self, size: u64) -> u64 {
        if !self.responding {
            return size;
        }
        let Ok(map) = RescueMap::load(&map_path(self.image)) else {
            return size;
        };
        if map.size() != size {
            return size;
        }
        let (info, source) = (self.info.clone(), self.source.clone());
        match timed(self.timeout, None, move || {
            SourceIdentity::of_device(&info, &*source)
        }) {
            Timed::Done(Ok(identity)) if map.identity().same_source(&identity) => {
                still_to_copy(&map.totals(), true)
            }
            _ => size,
        }
    }
}
