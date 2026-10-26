use std::fs::File;
use std::io;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::Path;

pub fn open_read_only(path: &Path) -> io::Result<File> {
    File::open(path)
}

/// `O_NONBLOCK` where its value is known (it differs between systems; no libc dependency).
#[cfg(any(target_os = "linux", target_os = "android"))]
const O_NONBLOCK: i32 = 0o4000;
#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
const O_NONBLOCK: i32 = 0x0004;
#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd"
)))]
const O_NONBLOCK: i32 = 0;

/// Opens what should be a regular file read-only, without blocking: a FIFO put in its place opens at once (and is then
/// refused by the caller's check on the handle) instead of waiting for a writer. Reads of a regular file ignore the flag.
pub fn open_file_read_only(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(O_NONBLOCK)
        .open(path)
}

/// `O_NOFOLLOW` where its value is certain (macOS); elsewhere 0, and a link swapped in after the caller's look is
/// followed (the handle check still refuses anything but a regular file).
#[cfg(target_os = "macos")]
const O_NOFOLLOW: i32 = 0x0100;
#[cfg(not(target_os = "macos"))]
const O_NOFOLLOW: i32 = 0;

/// Opens what should be a regular file the engine wrote (a rescue map, a session file) read-only, without blocking on a
/// FIFO and (on macOS) without following a link: the caller checks the handle.
pub(crate) fn open_file_no_follow(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(O_NONBLOCK | O_NOFOLLOW)
        .open(path)
}

/// How many names (hard links) the open file has.
pub(crate) fn link_count(file: &File) -> io::Result<u64> {
    Ok(file.metadata()?.nlink())
}

pub fn read_at(file: &File, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
    file.read_at(buf, offset)
}

pub fn volume_of_file(file: &File) -> io::Result<u64> {
    Ok(file.metadata()?.dev())
}

/// Opens a device node read-only (`/dev/rdiskN`): plain `File::open`, never a write mode.
#[cfg(target_os = "macos")]
pub(crate) fn open_device(path: &Path) -> io::Result<File> {
    File::open(path)
}

/// Whether a read error number means the device vanished (ruling D2): `ENOENT` (2), `ENXIO` (6), `ENODEV` (19), the same
/// numbers on macOS and Linux.
#[cfg(target_os = "macos")]
pub(crate) fn is_gone_errno(raw: i32) -> bool {
    const ENOENT: i32 = 2;
    const ENXIO: i32 = 6;
    const ENODEV: i32 = 19;
    matches!(raw, ENOENT | ENXIO | ENODEV)
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn gone_errnos_match_the_system() {
        for (kind, raw) in [
            (io::ErrorKind::NotFound, 2),
            (io::ErrorKind::PermissionDenied, 13),
        ] {
            assert_eq!(io::Error::from_raw_os_error(raw).kind(), kind);
        }
        // ENXIO and ENODEV have no stable kind: their messages identify them.
        let msg = |raw| io::Error::from_raw_os_error(raw).to_string();
        assert!(msg(6).contains("Device not configured"), "{}", msg(6));
        assert!(
            msg(19).contains("Operation not supported by device"),
            "{}",
            msg(19)
        );
        assert!(is_gone_errno(2) && is_gone_errno(6) && is_gone_errno(19));
        assert!(!is_gone_errno(5)); // EIO: a bad sector unless the device is no longer present
        assert!(!is_gone_errno(13));
    }
}
