//! A whole disk read directly, read-only: macOS `/dev/rdiskN`, Windows `\\.\PhysicalDriveN`.
//!
//! A device reads only whole logical sectors at sector offsets. [`DeviceSource`] takes any offset and length (ruling D4):
//! a request that is already aligned goes straight to the caller's buffer, the partial sectors at its ends go through a
//! sector-sized buffer aligned to 4096 bytes in memory. Scans and copies on a device use [`DeviceSource::scan_sector`],
//! the physical sector, so a bad physical sector is read once and becomes one unreadable run (P8).

use std::path::PathBuf;

use super::{Source, VolumeId};
use crate::EngineError;

/// Memory alignment of the intermediate sector buffers.
const ALIGN: usize = 4096;

/// A whole disk: macOS "disk4" (device path "/dev/rdisk4"), Windows "PhysicalDrive2" (path r"\\.\PhysicalDrive2").
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DiskId(String);

impl DiskId {
    /// Accepts "disk4", "/dev/disk4", "/dev/rdisk4", "PhysicalDrive2", r"\\.\PhysicalDrive2" on every platform (pure);
    /// a partition ("disk4s1") or anything else → Err(EngineError::NotWholeDisk), code "not_whole_disk".
    pub fn parse(s: &str) -> Result<DiskId, EngineError> {
        let unc = s.strip_prefix(r"\\.\").unwrap_or(s);
        if let Some(n) = strip_prefix_ignore_case(unc, "PhysicalDrive").and_then(number) {
            return Ok(DiskId(format!("PhysicalDrive{n}")));
        }
        let name = match s.strip_prefix("/dev/") {
            Some(node) => node.strip_prefix('r').unwrap_or(node),
            None => s,
        };
        match name.strip_prefix("disk").and_then(number) {
            Some(n) => Ok(DiskId(format!("disk{n}"))),
            None => Err(EngineError::NotWholeDisk),
        }
    }

    /// The canonical name: "disk4" or "PhysicalDrive2".
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The node read: "/dev/rdisk4" (the raw node, unbuffered) or r"\\.\PhysicalDrive2".
    pub fn device_path(&self) -> PathBuf {
        if self.is_windows() {
            PathBuf::from(format!(r"\\.\{}", self.0))
        } else {
            PathBuf::from(format!("/dev/r{}", self.0))
        }
    }

    fn is_windows(&self) -> bool {
        self.0.starts_with("PhysicalDrive")
    }
}

fn strip_prefix_ignore_case<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &s[prefix.len()..])
}

/// A canonical decimal unit number: digits only, no leading zero, fits a `u32`.
fn number(s: &str) -> Option<u32> {
    let canonical =
        !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) && (s == "0" || !s.starts_with('0'));
    canonical.then(|| s.parse().ok()).flatten()
}

/// Size and sector sizes of a device, in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    pub size: u64,
    pub logical_sector: u32,
    pub physical_sector: u32,
}

/// What a privileged helper serves (Module 2.1): aligned reads only. `offset` and `buf.len()` are multiples of
/// `geometry().logical_sector`; fills except at the end; `DeviceGone` when the device vanished (D2), `Read` otherwise.
pub trait RawDevice: Send + Sync {
    fn geometry(&self) -> Geometry;
    fn read_aligned(&self, offset: u64, buf: &mut [u8]) -> Result<usize, EngineError>;
}

/// A whole disk as a [`Source`]: reads any offset and length, aligned inside.
pub struct DeviceSource {
    raw: Box<dyn RawDevice>,
    disk: DiskId,
    geometry: Geometry,
}

impl DeviceSource {
    /// A source over `raw`, which reads the whole disk `disk`. A logical sector reported as 0 is taken as 1.
    pub fn new(raw: Box<dyn RawDevice>, disk: DiskId) -> DeviceSource {
        let mut geometry = raw.geometry();
        geometry.logical_sector = geometry.logical_sector.max(1);
        DeviceSource {
            raw,
            disk,
            geometry,
        }
    }

    pub fn geometry(&self) -> Geometry {
        self.geometry
    }

    pub fn disk_id(&self) -> &DiskId {
        &self.disk
    }

    /// The granularity for scans and copies on this device (D4, P8): `max(physical_sector, logical_sector)`. Callers put it
    /// in `ScanOptions::sector` and `CopyOptions::sector`, so a bad physical sector is read once.
    pub fn scan_sector(&self) -> u32 {
        self.geometry
            .physical_sector
            .max(self.geometry.logical_sector)
    }

    /// One read of the device at `offset` (aligned) into `buf` (whole sectors). An error never points before `floor`,
    /// the first byte the caller asked for.
    fn raw_read(&self, offset: u64, buf: &mut [u8], floor: u64) -> Result<usize, EngineError> {
        let n = self.raw.read_aligned(offset, buf).map_err(|e| match e {
            EngineError::Read { offset, cause } => EngineError::Read {
                offset: offset.max(floor),
                cause,
            },
            EngineError::DeviceGone { offset } => EngineError::DeviceGone {
                offset: offset.max(floor),
            },
            e => e,
        })?;
        Ok(n.min(buf.len()))
    }
}

/// A zeroed buffer of `len` bytes whose start is aligned to [`ALIGN`] in memory, inside `storage`.
fn aligned(storage: &mut Vec<u8>, len: usize) -> &mut [u8] {
    storage.clear();
    storage.resize(len + ALIGN, 0);
    let skip = storage.as_ptr().align_offset(ALIGN).min(ALIGN);
    &mut storage[skip..skip + len]
}

impl Source for DeviceSource {
    fn len(&self) -> u64 {
        self.geometry.size
    }

    /// Reads whole sectors from the device: the aligned middle of the request directly into `buf`, each partial sector at
    /// its ends through an aligned sector buffer. Fewer bytes only at the end, or when the device returns fewer.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, EngineError> {
        let size = self.geometry.size;
        if offset >= size {
            return Ok(0);
        }
        let want = (size - offset).min(buf.len() as u64) as usize;
        let sector = self.geometry.logical_sector as usize;
        let mut storage = Vec::new();
        let mut done = 0;
        while done < want {
            let pos = offset + done as u64;
            let head = (pos % sector as u64) as usize;
            let left = want - done;
            if head == 0 && left >= sector {
                let len = left - left % sector;
                let got = self.raw_read(pos, &mut buf[done..done + len], pos)?;
                done += got;
                if got < len {
                    break;
                }
            } else {
                let bounce = aligned(&mut storage, sector);
                let got = self.raw_read(pos - head as u64, bounce, pos)?;
                let n = got.saturating_sub(head).min(left);
                buf[done..done + n].copy_from_slice(&bounce[head..head + n]);
                done += n;
                if n < (sector - head).min(left) {
                    break;
                }
            }
        }
        Ok(done)
    }

    fn volume(&self) -> Option<VolumeId> {
        None
    }

    fn disk(&self) -> Option<DiskId> {
        Some(self.disk.clone())
    }
}

/// Opens the whole disk `disk` read-only (root or Administrator needed); what `device::SystemDevices::open` serves.
/// Errors: `permission_denied`, `device_not_found` (also when, on macOS, the disk changed between its geometry query and
/// the open), `system_tool_failed` (macOS, the geometry query), `open_failed`, and `unsupported_platform` off macOS and
/// Windows.
pub(crate) fn open_raw(disk: &DiskId) -> Result<Box<dyn RawDevice>, EngineError> {
    Ok(Box::new(os::open(disk)?))
}

/// The operating system's devices: opening `/dev/rdiskN` or `\\.\PhysicalDriveN` read-only and classifying read errors
/// (ruling D2).
#[cfg(any(target_os = "macos", windows))]
mod os {
    use std::fs::File;
    use std::io;
    #[cfg(target_os = "macos")]
    use std::path::PathBuf;

    use super::{ALIGN, DiskId, Geometry, RawDevice, aligned};
    use crate::EngineError;
    use crate::source::platform;
    #[cfg(windows)]
    use platform::is_gone_code as is_gone;
    #[cfg(target_os = "macos")]
    use platform::is_gone_errno as is_gone;

    /// Largest read through the aligned buffer when the caller's buffer is not aligned in memory.
    const BOUNCE: usize = 1 << 20;

    pub(super) struct OsDevice {
        file: File,
        geometry: Geometry,
        /// The block node (`/dev/diskN`): a read error while it is gone is the device vanishing.
        #[cfg(target_os = "macos")]
        node: PathBuf,
    }

    fn open_error(cause: io::Error) -> EngineError {
        match cause.kind() {
            io::ErrorKind::PermissionDenied => EngineError::PermissionDenied,
            io::ErrorKind::NotFound => EngineError::DeviceNotFound,
            _ => EngineError::Open { cause },
        }
    }

    #[cfg(target_os = "macos")]
    pub(super) fn open(disk: &DiskId) -> Result<OsDevice, EngineError> {
        if !disk.as_str().starts_with("disk") {
            return Err(EngineError::DeviceNotFound);
        }
        let node = PathBuf::from(format!("/dev/{}", disk.as_str()));
        if !node.exists() {
            return Err(EngineError::DeviceNotFound);
        }
        let (geometry, info) = crate::device::macos::geometry(disk.as_str())?;
        let file = platform::open_device(&disk.device_path()).map_err(open_error)?;
        // The node may name another medium by now (a card swapped meanwhile): the geometry must be the opened one's.
        crate::device::macos::confirm(disk.as_str(), &info)?;
        Ok(OsDevice {
            file,
            geometry,
            node,
        })
    }

    #[cfg(windows)]
    pub(super) fn open(disk: &DiskId) -> Result<OsDevice, EngineError> {
        if !disk.is_windows() {
            return Err(EngineError::DeviceNotFound);
        }
        let file = platform::open_device(&disk.device_path()).map_err(open_error)?;
        let geometry =
            platform::device_geometry(&file).map_err(|cause| EngineError::Open { cause })?;
        Ok(OsDevice { file, geometry })
    }

    impl OsDevice {
        #[cfg(target_os = "macos")]
        fn present(&self) -> bool {
            self.node.exists()
        }

        #[cfg(windows)]
        fn present(&self) -> bool {
            platform::device_present(&self.file)
        }

        /// A read error at `at`: the device vanished (D2: a "gone" error code, or the device no longer present), else a
        /// bad sector.
        fn classify(&self, at: u64, cause: io::Error) -> EngineError {
            let gone_code = cause.raw_os_error().is_some_and(is_gone);
            if gone_code || !self.present() {
                EngineError::DeviceGone { offset: at }
            } else {
                EngineError::Read { offset: at, cause }
            }
        }

        fn read_into(&self, offset: u64, buf: &mut [u8]) -> Result<usize, EngineError> {
            let mut filled = 0;
            while filled < buf.len() {
                let pos = offset + filled as u64;
                match platform::read_at(&self.file, pos, &mut buf[filled..]) {
                    Ok(0) => break,
                    Ok(n) => filled += n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(self.classify(pos, e)),
                }
            }
            Ok(filled)
        }
    }

    impl RawDevice for OsDevice {
        fn geometry(&self) -> Geometry {
            self.geometry
        }

        /// Reads into `buf` directly when it is aligned in memory, else through an aligned buffer (some devices refuse
        /// unaligned memory).
        fn read_aligned(&self, offset: u64, buf: &mut [u8]) -> Result<usize, EngineError> {
            if (buf.as_ptr() as usize).is_multiple_of(ALIGN) {
                return self.read_into(offset, buf);
            }
            let mut storage = Vec::new();
            let mut done = 0;
            for chunk in buf.chunks_mut(BOUNCE) {
                let bounce = aligned(&mut storage, chunk.len());
                let got = self.read_into(offset + done as u64, bounce)?;
                chunk[..got].copy_from_slice(&bounce[..got]);
                done += got;
                if got < chunk.len() {
                    break;
                }
            }
            Ok(done)
        }
    }
}

/// No device access on this system.
#[cfg(not(any(target_os = "macos", windows)))]
mod os {
    use super::{DiskId, Geometry, RawDevice};
    use crate::EngineError;

    pub(super) enum OsDevice {}

    pub(super) fn open(_disk: &DiskId) -> Result<OsDevice, EngineError> {
        Err(EngineError::Unsupported)
    }

    impl RawDevice for OsDevice {
        fn geometry(&self) -> Geometry {
            match *self {}
        }
        fn read_aligned(&self, _offset: u64, _buf: &mut [u8]) -> Result<usize, EngineError> {
            match *self {}
        }
    }
}
