//! The operating system's disks: listing the connected ones ([`DeviceOps::list`]), opening one read-only
//! ([`DeviceOps::open`]) and finding which disks a destination folder lives on ([`DeviceOps::destination`]).
//!
//! macOS asks its tools (`diskutil`, `ioreg`, `df`; parsed in `macos_parse`), Windows asks its devices directly (FFI in
//! `source/windows.rs`, assembled in `win_parse`); other systems answer `unsupported_platform`. The parsers are pure and
//! tested on every platform.

use std::path::{Path, PathBuf};

use crate::EngineError;
use crate::source::{DiskId, Geometry, RawDevice};

#[cfg(target_os = "macos")]
pub(crate) mod macos;
#[cfg(any(target_os = "macos", test))]
pub(crate) mod macos_parse;
#[cfg(any(target_os = "macos", test))]
pub(crate) mod plist;
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
}

#[cfg(target_os = "macos")]
fn os_list() -> Result<Vec<DiskInfo>, EngineError> {
    macos::list()
}

#[cfg(target_os = "macos")]
fn os_destination(dir: &Path) -> Result<DestInfo, EngineError> {
    macos::destination(dir)
}

#[cfg(windows)]
fn os_list() -> Result<Vec<DiskInfo>, EngineError> {
    win::list()
}

#[cfg(windows)]
fn os_destination(dir: &Path) -> Result<DestInfo, EngineError> {
    win::destination(dir)
}

#[cfg(not(any(target_os = "macos", windows)))]
fn os_list() -> Result<Vec<DiskInfo>, EngineError> {
    Err(EngineError::Unsupported)
}

#[cfg(not(any(target_os = "macos", windows)))]
fn os_destination(_dir: &Path) -> Result<DestInfo, EngineError> {
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
