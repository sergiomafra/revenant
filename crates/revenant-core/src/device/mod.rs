//! The operating system's disks: listing the connected ones ([`DeviceOps::list`]), opening one read-only
//! ([`DeviceOps::open`]), finding which disks a destination folder lives on ([`DeviceOps::destination`]) and unmounting a
//! disk's volumes without ejecting it ([`DeviceOps::unmount`], [`DeviceOps::remount`]); finding a card again after it was
//! disconnected ([`reconnect`]).
//!
//! macOS asks its tools (`diskutil`, `ioreg`, `df`; parsed in `macos_parse`), Windows asks its devices directly (FFI in
//! `source/windows.rs`, assembled in `win_parse`); other systems answer `unsupported_platform`. The parsers are pure and
//! tested on every platform.

use std::path::{Path, PathBuf};

use crate::EngineError;
use crate::source::{DiskId, Geometry, RawDevice};

pub use reconnect::{Reconnected, reconnect};

#[cfg(target_os = "macos")]
pub(crate) mod macos;
#[cfg(any(target_os = "macos", test))]
pub(crate) mod macos_parse;
#[cfg(any(target_os = "macos", test))]
pub(crate) mod plist;
mod reconnect;
#[cfg(windows)]
pub(crate) mod win;
#[cfg(any(windows, test))]
pub(crate) mod win_parse;

/// The largest file FAT12/16/32 can hold: 4 GiB − 1.
pub const FAT_MAX_FILE: u64 = 4_294_967_295;

/// A volume (partition) of a listed disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VolumeInfo {
    /// macOS "disk4s1"; Windows the volume GUID path (`\\?\Volume{…}\`).
    pub id: String,
    /// The file system as the system names it (macOS `exfat`, `msdos`, `apfs`; Windows `exFAT`, `FAT32`, `NTFS`), when
    /// it knows it.
    pub fs: Option<String>,
    pub label: Option<String>,
    pub mount_point: Option<PathBuf>,
    pub size: u64,
}

/// A connected whole disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiskInfo {
    pub id: DiskId,
    pub geometry: Geometry,
    pub vendor: Option<String>,
    pub model: Option<String>,
    /// Often the card reader's, not the card's: identity never relies on it alone.
    pub serial: Option<String>,
    /// Removable media (an SD card, a USB stick).
    pub removable: bool,
    /// Built into the computer.
    pub internal: bool,
    /// Holds the running system (the startup volume, or a part of it).
    pub system: bool,
    /// A disk image attached by the system (macOS `hdiutil`, a Windows VHD).
    pub virtual_disk: bool,
    /// `Some(true)` for an SSD, `Some(false)` for a spinning disk, `None` when the system does not say.
    pub solid_state: Option<bool>,
    pub volumes: Vec<VolumeInfo>,
}

/// What a destination folder lives on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DestInfo {
    /// Every whole disk the folder's volume uses: on macOS an APFS container and the disks of its physical stores; on
    /// Windows each disk of the volume's extents.
    pub disks: Vec<DiskId>,
    pub fs: Option<String>,
    /// Free space available to this user, in bytes.
    pub free_bytes: u64,
    /// The largest file the file system holds ([`FAT_MAX_FILE`] on FAT), `None` when practically unlimited.
    pub max_file_bytes: Option<u64>,
}

/// A volume lock an unmount holds: a Windows volume handle; nothing elsewhere (the type has no value there).
#[cfg(windows)]
type VolumeLock = std::os::windows::io::OwnedHandle;
#[cfg(not(windows))]
type VolumeLock = std::convert::Infallible;

/// Keeps what an unmount must hold: nothing on macOS; on Windows the locked volume handles (released on drop, after
/// which Windows may mount them again on the next access). Never remounts by itself.
#[derive(Debug)]
pub struct UnmountGuard {
    locks: Vec<VolumeLock>,
}

impl UnmountGuard {
    /// How many volume locks it holds (always 0 on macOS).
    pub fn held(&self) -> usize {
        self.locks.len()
    }

    /// A guard holding nothing (macOS, a disk with nothing mounted, a test double).
    pub fn empty() -> UnmountGuard {
        UnmountGuard { locks: Vec::new() }
    }
}

/// Device operations a privileged helper could serve over a channel (Module 2.1). Errors are codes.
///
/// `destination` needs no privilege; it stays here for convenience (the helper may not serve it).
pub trait DeviceOps: Send + Sync {
    /// The connected whole disks, sorted by id (`disk2` before `disk10`).
    fn list(&self) -> Result<Vec<DiskInfo>, EngineError>;
    /// Opens a whole disk read-only (root or Administrator needed).
    fn open(&self, disk: &DiskId) -> Result<Box<dyn RawDevice>, EngineError>;
    /// The disks, file system and free space of the folder `dir`.
    fn destination(&self, dir: &Path) -> Result<DestInfo, EngineError>;
    /// Unmounts every volume of `disk` without ejecting (D10). Nothing mounted → Ok(empty guard).
    /// Failure → Err(EngineError::UnmountFailed { volumes }) with the volumes still mounted, code "unmount_failed".
    /// The system disk is never unmounted: Err(EngineError::SystemDisk), code "system_disk", before anything runs.
    fn unmount(&self, disk: &DiskId) -> Result<UnmountGuard, EngineError>;
    /// macOS `diskutil mountDisk`; Windows: drops `guard` and queries each volume. Err code "remount_failed".
    fn remount(&self, disk: &DiskId, guard: Option<UnmountGuard>) -> Result<(), EngineError>;
}

/// The running OS (macOS, Windows); elsewhere every call is `Err(Unsupported)`.
pub struct SystemDevices;

impl DeviceOps for SystemDevices {
    /// macOS: `diskutil list -plist`, `diskutil info -plist` per whole disk and partition and for `/`, one `ioreg`.
    /// Windows: `\\.\PhysicalDrive0..=63` queried without read access, and every volume's extents.
    fn list(&self) -> Result<Vec<DiskInfo>, EngineError> {
        let mut disks = os_list()?;
        sort_disks(&mut disks);
        Ok(disks)
    }

    /// Errors: `permission_denied`, `device_not_found` (also when the disk changed while it was being opened),
    /// `system_tool_failed` (macOS), `open_failed`, `unsupported_platform`.
    fn open(&self, disk: &DiskId) -> Result<Box<dyn RawDevice>, EngineError> {
        crate::source::open_raw(disk)
    }

    /// macOS: `df -P -k` and `diskutil info -plist` of the volume's node; Windows: the volume's extents, file system and
    /// free space. A folder whose disks cannot be told (a network share) is an error.
    fn destination(&self, dir: &Path) -> Result<DestInfo, EngineError> {
        os_destination(dir)
    }

    /// The system disk is refused first (`system_disk`). macOS: `diskutil unmountDisk` (never `force`, never eject),
    /// always run (an APFS container's volumes are not in the listing), then a listing to confirm; Windows:
    /// `FSCTL_LOCK_VOLUME` + `FSCTL_DISMOUNT_VOLUME` on every volume with an extent on the disk, the locks kept in the
    /// guard. Errors: `system_disk`, `unmount_failed`, `device_not_found`, `system_tool_failed` (macOS), `open_failed`
    /// (Windows listing), `unsupported_platform`.
    fn unmount(&self, disk: &DiskId) -> Result<UnmountGuard, EngineError> {
        os_unmount(disk)
    }

    /// Mounts the disk's volumes again, only when asked: nothing in the engine calls it by itself.
    fn remount(&self, disk: &DiskId, guard: Option<UnmountGuard>) -> Result<(), EngineError> {
        os_remount(disk, guard)
    }
}

#[cfg(target_os = "macos")]
fn os_list() -> Result<Vec<DiskInfo>, EngineError> {
    macos::list()
}

#[cfg(target_os = "macos")]
fn os_destination(dir: &Path) -> Result<DestInfo, EngineError> {
    macos::destination(dir)
}

#[cfg(target_os = "macos")]
fn os_unmount(disk: &DiskId) -> Result<UnmountGuard, EngineError> {
    macos::unmount(disk).map(|()| UnmountGuard::empty())
}

#[cfg(target_os = "macos")]
fn os_remount(disk: &DiskId, guard: Option<UnmountGuard>) -> Result<(), EngineError> {
    drop(guard);
    macos::remount(disk)
}

#[cfg(windows)]
fn os_list() -> Result<Vec<DiskInfo>, EngineError> {
    win::list()
}

#[cfg(windows)]
fn os_destination(dir: &Path) -> Result<DestInfo, EngineError> {
    win::destination(dir)
}

#[cfg(windows)]
fn os_unmount(disk: &DiskId) -> Result<UnmountGuard, EngineError> {
    win::unmount(disk).map(|locks| UnmountGuard { locks })
}

#[cfg(windows)]
fn os_remount(disk: &DiskId, guard: Option<UnmountGuard>) -> Result<(), EngineError> {
    // The locks go first: Windows mounts a volume again on its next access once nothing holds it.
    drop(guard);
    win::remount(disk)
}

#[cfg(not(any(target_os = "macos", windows)))]
fn os_list() -> Result<Vec<DiskInfo>, EngineError> {
    Err(EngineError::Unsupported)
}

#[cfg(not(any(target_os = "macos", windows)))]
fn os_destination(_dir: &Path) -> Result<DestInfo, EngineError> {
    Err(EngineError::Unsupported)
}

#[cfg(not(any(target_os = "macos", windows)))]
fn os_unmount(_disk: &DiskId) -> Result<UnmountGuard, EngineError> {
    Err(EngineError::Unsupported)
}

#[cfg(not(any(target_os = "macos", windows)))]
fn os_remount(_disk: &DiskId, _guard: Option<UnmountGuard>) -> Result<(), EngineError> {
    Err(EngineError::Unsupported)
}

/// Sorts disks by id in number order (`disk2` before `disk10`): names of one kind differ only in their number, so a
/// shorter name is a smaller number.
pub(crate) fn sort_disks(disks: &mut [DiskInfo]) {
    disks.sort_by(|a, b| {
        let (a, b) = (a.id.as_str(), b.id.as_str());
        (a.len(), a).cmp(&(b.len(), b))
    });
}

/// Appends `id` to `disks` unless already there (keeps the first order).
#[cfg(any(target_os = "macos", windows, test))]
fn push_unique(disks: &mut Vec<DiskId>, id: DiskId) {
    if !disks.contains(&id) {
        disks.push(id);
    }
}

/// The listed disk `disk`, when it may be unmounted (ruling P29): `device_not_found` when it is not listed, `system_disk`
/// when it holds the running system (never unmounted, whatever the caller asks).
#[cfg(any(target_os = "macos", windows, test))]
pub(crate) fn unmountable<'a>(
    listed: &'a [DiskInfo],
    disk: &DiskId,
) -> Result<&'a DiskInfo, EngineError> {
    let info = listed
        .iter()
        .find(|d| d.id == *disk)
        .ok_or(EngineError::DeviceNotFound)?;
    if info.system {
        return Err(EngineError::SystemDisk);
    }
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(id: &str, system: bool) -> DiskInfo {
        DiskInfo {
            id: DiskId::parse(id).unwrap(),
            geometry: Geometry {
                size: 1 << 30,
                logical_sector: 512,
                physical_sector: 512,
            },
            vendor: None,
            model: None,
            serial: None,
            removable: !system,
            internal: system,
            system,
            virtual_disk: false,
            solid_state: None,
            volumes: Vec::new(),
        }
    }

    #[test]
    fn system_disk_is_never_unmounted() {
        let listed = [
            info("disk0", true),
            info("disk4", false),
            info("PhysicalDrive0", true),
        ];
        let code = |d: &str| {
            unmountable(&listed, &DiskId::parse(d).unwrap())
                .map(|i| i.id.as_str().to_string())
                .map_err(|e| e.code())
        };
        assert_eq!(code("disk0"), Err("system_disk"));
        assert_eq!(code("PhysicalDrive0"), Err("system_disk"));
        assert_eq!(code("disk4"), Ok("disk4".to_string()));
        assert_eq!(code("disk9"), Err("device_not_found"));
    }
}
