//! Windows glue (compiled only on Windows, verified by CI): the system queries of `source/windows.rs` assembled by
//! `win_parse`.

use std::io;
use std::path::Path;

use super::{DestInfo, DiskInfo, win_parse};
use crate::EngineError;
use crate::source::windows as platform;

/// The highest `\\.\PhysicalDriveN` looked at.
const MAX_DRIVE: u32 = 63;

/// The connected whole disks (ruling D9): `\\.\PhysicalDrive0..=63` queried without read access, every volume with its
/// extents, and the volume of `%SystemDrive%` for the system disk.
pub(crate) fn list() -> Result<Vec<DiskInfo>, EngineError> {
    let disks: Vec<_> = (0..=MAX_DRIVE).filter_map(platform::disk_raw).collect();
    let volumes = platform::volumes_raw();
    let system = std::env::var("SystemDrive")
        .ok()
        .and_then(|drive| platform::volume_name_of(Path::new(&format!(r"{drive}\"))));
    Ok(win_parse::disks(&disks, &volumes, system.as_deref()))
}

/// Where `dir` lives (ruling D12): its volume's extents, file system and free space. A folder without disk extents (a
/// network share) fails with `open_failed`.
pub(crate) fn destination(dir: &Path) -> Result<DestInfo, EngineError> {
    let raw = platform::dest_raw(dir).map_err(|cause| EngineError::Open { cause })?;
    win_parse::destination(&raw).ok_or_else(|| EngineError::Open {
        cause: io::Error::other("the folder's volume has no disk extents"),
    })
}
