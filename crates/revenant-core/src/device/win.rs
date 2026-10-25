//! Windows glue (compiled only on Windows, verified by CI): the system queries of `source/windows.rs` assembled by
//! `win_parse`.

use std::io;
use std::os::windows::io::OwnedHandle;
use std::path::Path;

use super::{DestInfo, DiskInfo, win_parse};
use crate::EngineError;
use crate::source::DiskId;
use crate::source::windows as platform;

/// The highest `\\.\PhysicalDriveN` looked at.
const MAX_DRIVE: u32 = 63;

/// The connected whole disks (ruling D9): `\\.\PhysicalDrive0..=63` queried without read access, every volume with its
/// extents, and the volume of the Windows folder (`GetSystemWindowsDirectoryW`) for the system disk. Fails (`open_failed`)
/// when the system volume or its disks cannot be told: a listing never silently shows no system disk.
pub(crate) fn list() -> Result<Vec<DiskInfo>, EngineError> {
    let disks: Vec<_> = (0..=MAX_DRIVE).filter_map(platform::disk_raw).collect();
    let volumes = platform::volumes_raw();
    let windows =
        platform::system_windows_directory().map_err(|cause| EngineError::Open { cause })?;
    let system = platform::volume_name_of(&windows).ok_or_else(|| EngineError::Open {
        cause: io::Error::other("the system volume cannot be told"),
    })?;
    win_parse::disks(&disks, &volumes, &system).ok_or_else(|| EngineError::Open {
        cause: io::Error::other("the system volume has no disk extents"),
    })
}

/// Where `dir` lives (ruling D12): its volume's extents, file system and free space. A folder without disk extents (a
/// network share) fails with `open_failed`.
pub(crate) fn destination(dir: &Path) -> Result<DestInfo, EngineError> {
    let raw = platform::dest_raw(dir).map_err(|cause| EngineError::Open { cause })?;
    win_parse::destination(&raw).ok_or_else(|| EngineError::Open {
        cause: io::Error::other("the folder's volume has no disk extents"),
    })
}

/// The volume GUID paths of the volumes on `disk` (`PhysicalDriveN`); `device_not_found` for a disk Windows does not
/// list.
fn volumes_of(disk: &DiskId) -> Result<Vec<String>, EngineError> {
    let number = disk
        .as_str()
        .strip_prefix("PhysicalDrive")
        .and_then(|n| n.parse::<u32>().ok())
        .ok_or(EngineError::NotWholeDisk)?;
    platform::disk_raw(number).ok_or(EngineError::DeviceNotFound)?;
    Ok(win_parse::volumes_on(&platform::volumes_raw(), number))
}

/// Locks and dismounts every volume on `disk` (ruling D10) and returns the locks, to be held for the session. When one
/// fails, the others are released (dropped) and the error names those that stayed mounted: `unmount_failed`.
pub(crate) fn unmount(disk: &DiskId) -> Result<Vec<OwnedHandle>, EngineError> {
    let mut locks = Vec::new();
    let mut results = Vec::new();
    for volume in volumes_of(disk)? {
        let result = match platform::lock_volume_for_dismount(&volume) {
            Ok(lock) => {
                locks.push(lock);
                Ok(())
            }
            Err(e) => Err(e.raw_os_error().unwrap_or(-1)),
        };
        results.push((volume, result));
    }
    win_parse::unmount_outcome(&results)
        .map(|()| locks)
        .map_err(|volumes| EngineError::UnmountFailed { volumes })
}

/// After the locks are released: queries each volume on `disk`, which makes Windows mount it. A volume that does not
/// answer is `remount_failed`.
pub(crate) fn remount(disk: &DiskId) -> Result<(), EngineError> {
    let volumes = volumes_of(disk).map_err(|_| EngineError::RemountFailed)?;
    if volumes.iter().all(|v| platform::volume_answers(v)) {
        Ok(())
    } else {
        Err(EngineError::RemountFailed)
    }
}
