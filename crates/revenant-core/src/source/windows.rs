//! Windows backend. Compiled only on Windows (verified by CI).

use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::fs::{FileExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, GetFileInformationByHandle,
};

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
