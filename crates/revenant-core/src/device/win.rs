//! Windows glue (compiled only on Windows, verified by CI): the system queries of `source/windows.rs` assembled by
//! `win_parse`.

use std::io;
use std::os::windows::io::OwnedHandle;
use std::path::Path;

use super::win_parse::{self, WinVolumeRaw};
use super::{DestInfo, DiskInfo};
use crate::EngineError;
use crate::source::DiskId;
use crate::source::windows as platform;

/// The highest `\\.\PhysicalDriveN` looked at.
const MAX_DRIVE: u32 = 63;
/// How long an unmount waits before asking the volumes that said "no media" again (ruling P31), once for all of them.
const NO_MEDIA_RECHECK: std::time::Duration = std::time::Duration::from_millis(300);

/// The connected whole disks (ruling D9): `\\.\PhysicalDrive0..=63` queried without read access, every volume with its
/// extents, and the volume of the Windows folder (`GetSystemWindowsDirectoryW`) for the system disk. Fails (`open_failed`)
/// when the system volume or its disks cannot be told: a listing never silently shows no system disk.
pub(crate) fn list() -> Result<Vec<DiskInfo>, EngineError> {
    snapshot().map(|(disks, _)| disks)
}

/// The listing and the raw volumes it was assembled from.
fn snapshot() -> Result<(Vec<DiskInfo>, Vec<WinVolumeRaw>), EngineError> {
    let disks: Vec<_> = (0..=MAX_DRIVE).filter_map(platform::disk_raw).collect();
    let volumes = platform::volumes_raw();
    let windows =
        platform::system_windows_directory().map_err(|cause| EngineError::Open { cause })?;
    let system = platform::volume_name_of(&windows).ok_or_else(|| EngineError::Open {
        cause: io::Error::other("the system volume cannot be told"),
    })?;
    let listed = win_parse::disks(&disks, &volumes, &system).ok_or_else(|| EngineError::Open {
        cause: io::Error::other("the system volume has no disk extents"),
    })?;
    Ok((listed, volumes))
}

/// Where `dir` lives (ruling D12): its volume's extents, file system and free space. A folder without disk extents (a
/// network share) fails with `open_failed`.
pub(crate) fn destination(dir: &Path) -> Result<DestInfo, EngineError> {
    let raw = platform::dest_raw(dir).map_err(|cause| EngineError::Open { cause })?;
    win_parse::destination(&raw).ok_or_else(|| EngineError::Open {
        cause: io::Error::other("the folder's volume has no disk extents"),
    })
}

/// The number N of `PhysicalDriveN`.
fn number_of(disk: &DiskId) -> Result<u32, EngineError> {
    disk.as_str()
        .strip_prefix("PhysicalDrive")
        .and_then(|n| n.parse::<u32>().ok())
        .ok_or(EngineError::NotWholeDisk)
}

/// Locks and dismounts every volume on `disk` (ruling D10) and returns the locks, to be held for the session. The system
/// disk is refused (`system_disk`, P29) and a disk not listed is `device_not_found`, before anything is locked. Volumes
/// whose single query said "no media" are asked once more first ([`confirm_no_media`]). While a
/// local volume's disks are unknown (its extents query failed) nothing is locked and the call fails closed with
/// `unmount_failed` naming it: it could lie on `disk`. When a lock fails, the others are released (dropped) and the
/// error names those that stayed mounted.
pub(crate) fn unmount(disk: &DiskId) -> Result<Vec<OwnedHandle>, EngineError> {
    let (listed, mut volumes) = snapshot()?;
    super::unmountable(&listed, disk)?;
    let number = number_of(disk)?;
    confirm_no_media(&mut volumes);
    let unknown = win_parse::unknown_volumes(&volumes);
    if !unknown.is_empty() {
        return Err(EngineError::UnmountFailed { volumes: unknown });
    }
    let mut locks = Vec::new();
    let mut results = Vec::new();
    for volume in win_parse::volumes_on(&volumes, number) {
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

/// Asks again, after one wait of [`NO_MEDIA_RECHECK`] for all of them, only the volumes whose single query said "no
/// media" (a 4-slot reader costs one wait, not four); `win_parse` settles each.
fn confirm_no_media(volumes: &mut [WinVolumeRaw]) {
    let pending = win_parse::unconfirmed(volumes);
    if pending.is_empty() {
        return;
    }
    std::thread::sleep(NO_MEDIA_RECHECK);
    for i in pending {
        let retry = platform::volume_extents(&volumes[i].name, true);
        win_parse::confirm_no_media(&mut volumes[i], retry);
    }
}

/// After the locks are released: queries each volume on `disk`, which makes Windows mount it. A volume that does not
/// answer is `remount_failed`.
pub(crate) fn remount(disk: &DiskId) -> Result<(), EngineError> {
    let number = number_of(disk).map_err(|_| EngineError::RemountFailed)?;
    // A single query per volume (no "no media" confirmation): a volume briefly not ready right now has no extents here
    // and is skipped; Windows mounts it on its next access anyway, since nothing holds a lock on it any more.
    let volumes = win_parse::volumes_on(&platform::volumes_raw(), number);
    if volumes.iter().all(|v| platform::volume_answers(v)) {
        Ok(())
    } else {
        Err(EngineError::RemountFailed)
    }
}
