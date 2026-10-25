//! Windows backend. Compiled only on Windows (verified by CI).

use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::{FileExt, OpenOptionsExt};
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_DEV_NOT_EXIST, ERROR_DEVICE_NOT_CONNECTED, ERROR_FILE_NOT_FOUND,
    ERROR_MORE_DATA, ERROR_NO_SUCH_DEVICE, ERROR_NOT_READY, GENERIC_READ, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FindFirstVolumeW, FindNextVolumeW, FindVolumeClose, GetDiskFreeSpaceExW, GetDriveTypeW,
    GetFileInformationByHandle, GetVolumeInformationW, GetVolumeNameForVolumeMountPointW,
    GetVolumePathNameW, GetVolumePathNamesForVolumeNameW, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{
    DISK_GEOMETRY_EX, FSCTL_DISMOUNT_VOLUME, FSCTL_LOCK_VOLUME, GET_LENGTH_INFORMATION,
    IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, IOCTL_DISK_GET_LENGTH_INFO, IOCTL_STORAGE_CHECK_VERIFY2,
    IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery, STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR,
    STORAGE_PROPERTY_ID, STORAGE_PROPERTY_QUERY, StorageAccessAlignmentProperty,
    StorageDeviceProperty, StorageDeviceSeekPenaltyProperty,
};
use windows_sys::Win32::System::SystemInformation::GetSystemWindowsDirectoryW;

use super::Geometry;
use crate::device::win_parse::{
    self, ExtentsError, WinDestRaw, WinDiskRaw, WinExtent, WinVolumeRaw,
};

/// Opens read-only. `FILE_FLAG_BACKUP_SEMANTICS` is required to open directories.
pub fn open_read_only(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
}

/// Opens what should be a regular file read-only (the caller checks the handle: a directory opened through
/// `FILE_FLAG_BACKUP_SEMANTICS` is refused there).
pub fn open_file_read_only(path: &Path) -> io::Result<File> {
    open_read_only(path)
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

/// Sends `IOCTL_STORAGE_QUERY_PROPERTY` (a standard query of `property`) and fills `out`.
fn query_property<O>(file: &File, property: STORAGE_PROPERTY_ID, out: &mut O) -> io::Result<u32> {
    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: property,
        QueryType: PropertyStandardQuery,
        AdditionalParameters: [0],
    };
    ioctl(file, IOCTL_STORAGE_QUERY_PROPERTY, Some(&query), out)
}

/// Size (`IOCTL_DISK_GET_LENGTH_INFO`) and sector sizes (`StorageAccessAlignmentProperty`, else the bytes per sector of
/// `IOCTL_DISK_GET_DRIVE_GEOMETRY_EX` for both) of an open whole disk.
pub(crate) fn device_geometry(file: &File) -> io::Result<Geometry> {
    let mut length = GET_LENGTH_INFORMATION::default();
    ioctl::<(), _>(file, IOCTL_DISK_GET_LENGTH_INFO, None, &mut length)?;
    let size =
        u64::try_from(length.Length).map_err(|_| io::Error::other("negative disk length"))?;
    let (logical, physical) = sector_sizes(file)?;
    Ok(Geometry {
        size,
        logical_sector: logical,
        physical_sector: physical,
    })
}

/// The geometry of a disk opened without read access (a listing): the size from `IOCTL_DISK_GET_DRIVE_GEOMETRY_EX`
/// (`IOCTL_DISK_GET_LENGTH_INFO` needs read access), the sector sizes as [`device_geometry`].
fn query_geometry(file: &File) -> io::Result<Geometry> {
    let mut geo = DISK_GEOMETRY_EX::default();
    ioctl::<(), _>(file, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, None, &mut geo)?;
    let size = u64::try_from(geo.DiskSize).map_err(|_| io::Error::other("negative disk size"))?;
    let (logical, physical) = sector_sizes(file)?;
    Ok(Geometry {
        size,
        logical_sector: logical,
        physical_sector: physical,
    })
}

/// Logical and physical sector sizes: `StorageAccessAlignmentProperty`, else the bytes per sector of
/// `IOCTL_DISK_GET_DRIVE_GEOMETRY_EX` for both.
fn sector_sizes(file: &File) -> io::Result<(u32, u32)> {
    let mut align = STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR::default();
    let full = size_of::<STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR>() as u32;
    let sizes = match query_property(file, StorageAccessAlignmentProperty, &mut align) {
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
    Ok(sizes)
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

/// Longest volume name or path read, in UTF-16 units.
const NAME_LEN: usize = 1024;
/// Most volumes looked at in a listing.
const MAX_VOLUMES: usize = 256;
/// Extents read per volume (a volume with more is described by its first ones).
const MAX_EXTENTS: usize = 64;

/// `s` as a NUL-terminated UTF-16 string.
fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(Some(0)).collect()
}

/// The UTF-16 text of `buf` up to its first NUL.
fn until_nul(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

fn non_empty(s: String) -> Option<String> {
    let s = s.trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Opens a device or volume for queries only (access 0: no read, no write), sharing read and write.
fn open_query(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .access_mode(0)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(path)
}

/// What `\\.\PhysicalDrive<number>` says of itself, opened for queries only; `None` when there is no such disk or it does
/// not answer.
pub(crate) fn disk_raw(number: u32) -> Option<WinDiskRaw> {
    let file = open_query(Path::new(&format!(r"\\.\PhysicalDrive{number}"))).ok()?;
    let mut desc = [0u8; 1024];
    let n = query_property(&file, StorageDeviceProperty, &mut desc).ok()?;
    let descriptor = win_parse::descriptor(&desc[..(n as usize).min(desc.len())])?;
    let geometry = query_geometry(&file).ok()?;
    // DEVICE_SEEK_PENALTY_DESCRIPTOR: Version, Size, then the BOOLEAN IncursSeekPenalty at byte 8 (read as a byte).
    let mut penalty = [0u8; 12];
    let seek_penalty = match query_property(&file, StorageDeviceSeekPenaltyProperty, &mut penalty) {
        Ok(n) if n >= 9 => Some(penalty[8] != 0),
        _ => None,
    };
    Some(WinDiskRaw {
        number,
        geometry,
        descriptor,
        seek_penalty,
    })
}

/// The volume GUID path of the volume `path` lies on (`GetVolumePathNameW` + `GetVolumeNameForVolumeMountPointW`).
pub(crate) fn volume_name_of(path: &Path) -> Option<String> {
    let root = volume_root(path).ok()?;
    let root = wide(root.as_os_str());
    let mut buf = [0u16; NAME_LEN];
    // SAFETY: `root` is a NUL-terminated UTF-16 string that outlives the call; `buf` is a writable buffer of exactly
    // `NAME_LEN` UTF-16 units, the length passed.
    let ok = unsafe {
        GetVolumeNameForVolumeMountPointW(root.as_ptr(), buf.as_mut_ptr(), NAME_LEN as u32)
    };
    (ok != 0).then(|| until_nul(&buf))
}

/// The root of the volume `path` lies on (`C:\`, or a mounted folder).
fn volume_root(path: &Path) -> io::Result<PathBuf> {
    let name = wide(path.as_os_str());
    let mut buf = [0u16; NAME_LEN];
    // SAFETY: `name` is a NUL-terminated UTF-16 string that outlives the call; `buf` is a writable buffer of exactly
    // `NAME_LEN` UTF-16 units, the length passed.
    let ok = unsafe { GetVolumePathNameW(name.as_ptr(), buf.as_mut_ptr(), NAME_LEN as u32) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(PathBuf::from(until_nul(&buf)))
}

/// The label and file-system name of the volume whose root (or GUID path, with its trailing backslash) is `root`.
fn volume_information(root: &OsStr) -> (Option<String>, Option<String>) {
    let root = wide(root);
    let mut label = [0u16; 261];
    let mut fs = [0u16; 261];
    // SAFETY: `root` is a NUL-terminated UTF-16 string that outlives the call; `label` and `fs` are writable buffers of
    // exactly the lengths passed; the serial, component-length and flags out-pointers are null, which the function
    // accepts.
    let ok = unsafe {
        GetVolumeInformationW(
            root.as_ptr(),
            label.as_mut_ptr(),
            label.len() as u32,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            fs.as_mut_ptr(),
            fs.len() as u32,
        )
    };
    if ok == 0 {
        return (None, None);
    }
    (non_empty(until_nul(&label)), non_empty(until_nul(&fs)))
}

/// `GetDriveTypeW` answers for a network volume (`DRIVE_REMOTE`; the constant lives in a `windows-sys` feature not
/// enabled).
const DRIVE_REMOTE: u32 = 4;
/// `GetDriveTypeW` answers for a CD or DVD (`DRIVE_CDROM`).
const DRIVE_CDROM: u32 = 5;

/// The disk extents of the volume `\\?\Volume{…}\`, asked once (no second look, no wait: a listing, a destination and
/// a reconnect poll take this answer as it is; only an unmount confirms "no media", see
/// [`win_parse::confirm_no_media`]): empty when it has none (a CD or a network volume, by `GetDriveTypeW`, or the IOCTL
/// answering zero extents), else what [`extents_attempt`] answers.
pub(crate) fn volume_extents(name: &str, partial: bool) -> Result<Vec<WinExtent>, ExtentsError> {
    let root = wide(OsStr::new(name));
    // SAFETY: `root` is a NUL-terminated UTF-16 string that outlives the call.
    let kind = unsafe { GetDriveTypeW(root.as_ptr()) };
    if kind == DRIVE_REMOTE || kind == DRIVE_CDROM {
        return Ok(Vec::new());
    }
    extents_attempt(name, partial)
}

/// One attempt at the extents of the volume `name`: the volume opened for queries only (without the trailing
/// backslash) and `IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS`; on `ERROR_NOT_READY`, `IOCTL_STORAGE_CHECK_VERIFY2` on the same
/// handle tells an empty drive from media not ready yet. Only the error codes are gathered; `win_parse` decides.
fn extents_attempt(name: &str, partial: bool) -> Result<Vec<WinExtent>, ExtentsError> {
    let file = open_query(Path::new(name.trim_end_matches('\\')))
        .map_err(|e| ExtentsError::Open(e.raw_os_error()))?;
    let mut buf = [0u8; 8 + 24 * MAX_EXTENTS];
    match ioctl::<(), _>(&file, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, None, &mut buf) {
        Ok(_) => Ok(win_parse::extents(&buf)),
        // More extents than the buffer holds: the first ones are there.
        Err(e) if partial && e.raw_os_error() == Some(ERROR_MORE_DATA as i32) => {
            Ok(win_parse::extents(&buf))
        }
        Err(e) => {
            let code = e.raw_os_error();
            let verify = if code == Some(win_parse::ERROR_NOT_READY) {
                ioctl::<(), ()>(&file, IOCTL_STORAGE_CHECK_VERIFY2, None, &mut ())
                    .err()
                    .and_then(|e| e.raw_os_error())
            } else {
                None
            };
            Err(ExtentsError::Query { code, verify })
        }
    }
}

/// The first path the volume is mounted at, if any (`GetVolumePathNamesForVolumeNameW`).
fn mount_point(name: &str) -> Option<PathBuf> {
    let name = wide(OsStr::new(name));
    let mut buf = [0u16; NAME_LEN];
    let mut returned = 0u32;
    // SAFETY: `name` is a NUL-terminated UTF-16 string that outlives the call; `buf` is a writable buffer of exactly
    // `NAME_LEN` UTF-16 units, the length passed; `returned` is a valid out-pointer.
    let ok = unsafe {
        GetVolumePathNamesForVolumeNameW(
            name.as_ptr(),
            buf.as_mut_ptr(),
            NAME_LEN as u32,
            &mut returned,
        )
    };
    if ok == 0 {
        return None;
    }
    non_empty(until_nul(&buf)).map(PathBuf::from)
}

/// Every volume of the system (at most 256): its GUID path, file system, label, first mount point and disk extents.
pub(crate) fn volumes_raw() -> Vec<WinVolumeRaw> {
    let mut out = Vec::new();
    let mut buf = [0u16; NAME_LEN];
    // SAFETY: `buf` is a writable buffer of exactly `NAME_LEN` UTF-16 units, the length passed.
    let find = unsafe { FindFirstVolumeW(buf.as_mut_ptr(), NAME_LEN as u32) };
    if find == INVALID_HANDLE_VALUE {
        return out;
    }
    loop {
        let name = until_nul(&buf);
        if !name.is_empty() {
            let (label, fs) = volume_information(OsStr::new(&name));
            let (extents, extents_failed, no_media) =
                win_parse::single_shot(volume_extents(&name, true));
            out.push(WinVolumeRaw {
                fs,
                label,
                mount_point: mount_point(&name),
                extents_failed,
                extents,
                no_media,
                name,
            });
        }
        if out.len() >= MAX_VOLUMES {
            break;
        }
        // SAFETY: `find` is the search handle FindFirstVolumeW returned, not yet closed; `buf` as above.
        if unsafe { FindNextVolumeW(find, buf.as_mut_ptr(), NAME_LEN as u32) } == 0 {
            break;
        }
    }
    // SAFETY: `find` is a valid search handle, closed exactly once here.
    unsafe { FindVolumeClose(find) };
    out
}

/// The volume a destination folder lies on: its file system, the free space available to this user and its extents.
pub(crate) fn dest_raw(dir: &Path) -> io::Result<WinDestRaw> {
    let root = volume_root(dir)?;
    let name = volume_name_of(&root).ok_or_else(io::Error::last_os_error)?;
    let (_, fs) = volume_information(root.as_os_str());
    let dir_w = wide(dir.as_os_str());
    let mut free = 0u64;
    // SAFETY: `dir_w` is a NUL-terminated UTF-16 string that outlives the call; `free` is a valid out-pointer; the
    // total and total-free out-pointers are null, which the function accepts.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            dir_w.as_ptr(),
            &mut free,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(WinDestRaw {
        fs,
        free_bytes: free,
        // No extents (a network volume), no media or a failed query: the destination check fails closed on an empty list.
        extents: volume_extents(&name, false).unwrap_or_default(),
    })
}

/// Locks and dismounts the volume `volume` (its GUID path, `\\?\Volume{…}\`) so the system neither writes to it nor
/// mounts it again while the returned handle stays open (ruling D10); closing the handle releases the lock, after which
/// Windows may mount the volume on its next access. The volume is opened read-only first; only when the system refuses
/// the lock or the dismount on that handle (`ERROR_ACCESS_DENIED`) is it opened again with write access, solely to
/// repeat the two requests. The handle is never used to read or write data: it goes to [`lock_and_dismount`] and is then
/// only held and closed.
// WRITE-ACCESS EXCEPTION (CLAUDE.md): volume lock/dismount only, never reads or writes data.
pub(crate) fn lock_volume_for_dismount(volume: &str) -> io::Result<OwnedHandle> {
    let path = Path::new(volume.trim_end_matches('\\'));
    let share = FILE_SHARE_READ | FILE_SHARE_WRITE;
    let read_only = OpenOptions::new().read(true).share_mode(share).open(path)?;
    let locked = match lock_and_dismount(&read_only) {
        Ok(()) => read_only,
        Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => {
            // FSCTL_LOCK_VOLUME also answers ERROR_ACCESS_DENIED for a volume in use: the reopen then happens too and fails
            // the same way. Closed before the second open: a lock taken on it (lock accepted, dismount refused) would
            // refuse that open.
            drop(read_only);
            // The exception (CLAUDE.md): write access on a VOLUME handle, only because the system demands it for the lock
            // and dismount requests. This handle is passed to nothing but `lock_and_dismount`: no ReadFile, no WriteFile.
            let for_lock = OpenOptions::new()
                .access_mode(GENERIC_READ | windows_sys::Win32::Foundation::GENERIC_WRITE)
                .share_mode(share)
                .open(path)?;
            lock_and_dismount(&for_lock)?;
            for_lock
        }
        Err(e) => return Err(e),
    };
    Ok(OwnedHandle::from(locked))
}

/// Sends `FSCTL_LOCK_VOLUME` then `FSCTL_DISMOUNT_VOLUME` on an open volume (neither takes nor returns data).
fn lock_and_dismount(volume: &File) -> io::Result<()> {
    ioctl::<(), ()>(volume, FSCTL_LOCK_VOLUME, None, &mut ())?;
    ioctl::<(), ()>(volume, FSCTL_DISMOUNT_VOLUME, None, &mut ())?;
    Ok(())
}

/// Whether the volume `volume` (its GUID path) answers a file-system query (`GetVolumeInformationW`), which makes Windows
/// mount it when it is not.
pub(crate) fn volume_answers(volume: &str) -> bool {
    volume_information(OsStr::new(volume)).1.is_some()
}

/// The Windows folder of the running system (`GetSystemWindowsDirectoryW`, `C:\Windows`): the system disk holds its
/// volume. Does not depend on environment variables (a privileged helper may run with a sanitized environment).
pub(crate) fn system_windows_directory() -> io::Result<PathBuf> {
    let mut buf = [0u16; NAME_LEN];
    // SAFETY: `buf` is a writable buffer of exactly `NAME_LEN` UTF-16 units, the length passed.
    let n = unsafe { GetSystemWindowsDirectoryW(buf.as_mut_ptr(), NAME_LEN as u32) };
    if n == 0 || n as usize >= NAME_LEN {
        return Err(io::Error::last_os_error());
    }
    Ok(PathBuf::from(until_nul(&buf)))
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
