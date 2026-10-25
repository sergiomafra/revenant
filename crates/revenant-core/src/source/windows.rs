//! Windows backend. Compiled only on Windows (verified by CI).

use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::fs::{FileExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use windows_sys::Win32::Foundation::{
    ERROR_DEV_NOT_EXIST, ERROR_DEVICE_NOT_CONNECTED, ERROR_FILE_NOT_FOUND, ERROR_NO_SUCH_DEVICE,
    ERROR_NOT_READY,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_READ, FILE_SHARE_WRITE,
    GetFileInformationByHandle,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{
    DISK_GEOMETRY_EX, GET_LENGTH_INFORMATION, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX,
    IOCTL_DISK_GET_LENGTH_INFO, IOCTL_STORAGE_CHECK_VERIFY2, IOCTL_STORAGE_QUERY_PROPERTY,
    PropertyStandardQuery, STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR, STORAGE_PROPERTY_QUERY,
    StorageAccessAlignmentProperty,
};

use super::Geometry;

/// Opens read-only. `FILE_FLAG_BACKUP_SEMANTICS` is required to open directories.
pub fn open_read_only(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
}

pub fn read_at(file: &File, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
    file.seek_read(buf, offset)
}

pub fn volume_of_file(file: &File) -> io::Result<u64> {
    // SAFETY: BY_HANDLE_FILE_INFORMATION is plain data for which all-zero is valid.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: the handle is valid for the lifetime of `file`, and `info` is a valid out-pointer.
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, &mut info) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(u64::from(info.dwVolumeSerialNumber))
}

/// Opens a whole disk (`\\.\PhysicalDriveN`) for reading only (`GENERIC_READ`), sharing read and write with the system.
pub(crate) fn open_device(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(path)
}

/// Sends `code` with input `input` and fills `out`; the number of bytes returned.
fn ioctl<I, O>(file: &File, code: u32, input: Option<&I>, out: &mut O) -> io::Result<u32> {
    let mut returned = 0u32;
    let (in_ptr, in_len) = match input {
        Some(i) => ((i as *const I).cast(), size_of::<I>() as u32),
        None => (std::ptr::null(), 0),
    };
    // SAFETY: the handle is valid for the lifetime of `file`; `in_ptr`/`in_len` describe `input` (or nothing), `out` is a
    // valid, writable `O` of `size_of::<O>()` bytes, `returned` a valid out-pointer, and the call is synchronous (no
    // OVERLAPPED), so no pointer outlives it.
    let ok = unsafe {
        DeviceIoControl(
            file.as_raw_handle() as _,
            code,
            in_ptr,
            in_len,
            (out as *mut O).cast(),
            size_of::<O>() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(returned)
}

/// Size (`IOCTL_DISK_GET_LENGTH_INFO`) and sector sizes (`StorageAccessAlignmentProperty`, else the bytes per sector of
/// `IOCTL_DISK_GET_DRIVE_GEOMETRY_EX` for both) of an open whole disk.
pub(crate) fn device_geometry(file: &File) -> io::Result<Geometry> {
    let mut length = GET_LENGTH_INFORMATION::default();
    ioctl::<(), _>(file, IOCTL_DISK_GET_LENGTH_INFO, None, &mut length)?;
    let size =
        u64::try_from(length.Length).map_err(|_| io::Error::other("negative disk length"))?;
    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageAccessAlignmentProperty,
        QueryType: PropertyStandardQuery,
        AdditionalParameters: [0],
    };
    let mut align = STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR::default();
    let full = size_of::<STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR>() as u32;
    let (logical, physical) =
        match ioctl(file, IOCTL_STORAGE_QUERY_PROPERTY, Some(&query), &mut align) {
            Ok(n) if n >= full && plausible(align.BytesPerLogicalSector) => {
                let physical = align.BytesPerPhysicalSector;
                let physical = if plausible(physical) && physical >= align.BytesPerLogicalSector {
                    physical
                } else {
                    align.BytesPerLogicalSector
                };
                (align.BytesPerLogicalSector, physical)
            }
            _ => {
                let mut geo = DISK_GEOMETRY_EX::default();
                ioctl::<(), _>(file, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, None, &mut geo)?;
                let bytes = geo.Geometry.BytesPerSector;
                if !plausible(bytes) {
                    return Err(io::Error::other("implausible sector size"));
                }
                (bytes, bytes)
            }
        };
    Ok(Geometry {
        size,
        logical_sector: logical,
        physical_sector: physical,
    })
}

/// A sector size from 512 to 64 KiB, a power of two.
fn plausible(sector: u32) -> bool {
    (512..=1 << 16).contains(&sector) && sector.is_power_of_two()
}

/// Whether the device behind an open handle is still there (`IOCTL_STORAGE_CHECK_VERIFY2`); a device that does not
/// answer is absent.
pub(crate) fn device_present(file: &File) -> bool {
    ioctl::<(), ()>(file, IOCTL_STORAGE_CHECK_VERIFY2, None, &mut ()).is_ok()
}

/// Whether a read error code means the device vanished (ruling D2): `ERROR_DEVICE_NOT_CONNECTED`, `ERROR_NO_SUCH_DEVICE`,
/// `ERROR_DEV_NOT_EXIST`, `ERROR_NOT_READY`, `ERROR_FILE_NOT_FOUND`.
pub(crate) fn is_gone_code(raw: i32) -> bool {
    [
        ERROR_DEVICE_NOT_CONNECTED,
        ERROR_NO_SUCH_DEVICE,
        ERROR_DEV_NOT_EXIST,
        ERROR_NOT_READY,
        ERROR_FILE_NOT_FOUND,
    ]
    .iter()
    .any(|&c| c as i32 == raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gone_codes_follow_d2() {
        for code in [1167, 433, 55, 21, 2] {
            assert!(is_gone_code(code), "{code}");
        }
        for code in [5, 23, 87, 1117] {
            assert!(!is_gone_code(code), "{code}");
        }
    }
}
