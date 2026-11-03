//! Opening a whole disk through the operating system's own permission prompt, so the app never runs as root or
//! Administrator. macOS: `/usr/libexec/authopen` asks, opens the raw node read-only and hands the descriptor back over a
//! socket ([`super::macos_fd`]). Windows: the app relaunches itself elevated (`runas`, the UAC prompt); that process
//! opens the disk read-only, duplicates the handle into the app and reports the handle's value as its exit code. No
//! `unsafe` here: the system calls live in `source/macos_fd.rs` and `source/windows.rs`.

use std::ffi::OsString;

use super::{DiskId, RawDevice};
use crate::EngineError;

/// First argument of the elevated relaunch on Windows: `--revenant-elevated-open <PhysicalDriveN> <app pid>`.
pub(crate) const ELEVATED_FLAG: &str = "--revenant-elevated-open";

// Exit codes of the elevated process. A handle value is a positive multiple of 4, so errors are fixed odd numbers.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) const ELEVATED_DENIED: i32 = 1;
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) const ELEVATED_NOT_FOUND: i32 = 3;
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) const ELEVATED_OPEN_FAILED: i32 = 5;
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) const ELEVATED_BAD_ARGS: i32 = 7;

/// Opens the whole disk `disk` read-only after the system asks the user for permission (blocks until they answer).
/// Errors: `permission_denied` (the user cancelled or refused), `device_not_found` (the disk is gone, or another medium
/// is in its place by the time it opened), `system_tool_failed` (the prompt could not be shown: `authopen` or the
/// elevated relaunch did not run, or `diskutil` failed), `open_failed`, and `unsupported_platform` off macOS and Windows.
pub fn open_with_system_prompt(disk: &DiskId) -> Result<Box<dyn RawDevice>, EngineError> {
    imp::open(disk)
}

/// The elevated side of [`open_with_system_prompt`] on Windows: when `args[1]` is `--revenant-elevated-open`, does the
/// elevated process's work and returns its exit code (the app's `main` calls this first and exits with it); otherwise
/// `None`. Always `None` off Windows.
pub fn elevated_entry(args: &[OsString]) -> Option<i32> {
    if args.get(1).is_none_or(|a| a != ELEVATED_FLAG) {
        return None;
    }
    imp::elevated(args)
}

/// The arguments of `/usr/libexec/authopen` for `disk`: `-stdoutpipe` (hand the descriptor back on stdout) and the raw
/// node, read-only: never `-w`, `-c`, `-a`, `-x` nor `-o`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn authopen_args(disk: &DiskId) -> Vec<String> {
    vec![
        "-stdoutpipe".to_string(),
        disk.device_path().to_string_lossy().into_owned(),
    ]
}

/// Whether an exit code can carry `value` as a handle: positive, a multiple of 4, within `i32`.
fn carries_handle(value: u32) -> bool {
    value > 0 && value.is_multiple_of(4) && value <= i32::MAX as u32
}

fn helper_failed(what: String) -> EngineError {
    EngineError::Open {
        cause: std::io::Error::other(what),
    }
}

/// The elevated process's exit code: the handle value, or the odd code of the error.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn encode_exit(result: Result<u32, EngineError>) -> i32 {
    match result {
        Ok(handle) if carries_handle(handle) => handle as i32,
        Ok(_) => ELEVATED_OPEN_FAILED,
        Err(EngineError::PermissionDenied) => ELEVATED_DENIED,
        Err(EngineError::DeviceNotFound) => ELEVATED_NOT_FOUND,
        Err(_) => ELEVATED_OPEN_FAILED,
    }
}

/// What the elevated process's exit code says: a handle value (a positive multiple of 4) or the error.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn decode_exit(code: i32) -> Result<u32, EngineError> {
    match code {
        ELEVATED_DENIED => Err(EngineError::PermissionDenied),
        ELEVATED_NOT_FOUND => Err(EngineError::DeviceNotFound),
        ELEVATED_OPEN_FAILED => Err(helper_failed(
            "the elevated process could not open the disk".to_string(),
        )),
        ELEVATED_BAD_ARGS => Err(helper_failed(
            "the elevated process refused its arguments".to_string(),
        )),
        // A crash (an NTSTATUS such as 0xC0000005) is negative as an `i32`, never a handle.
        c if c > 0 && carries_handle(c as u32) => Ok(c as u32),
        c => Err(helper_failed(format!(
            "the elevated process ended with code {c}"
        ))),
    }
}

/// `[app, --revenant-elevated-open, PhysicalDriveN, pid]` → the disk and the app's process id.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn parse_elevated_args(args: &[OsString]) -> Option<(DiskId, u32)> {
    let [_, flag, disk, pid] = args else {
        return None;
    };
    if flag != ELEVATED_FLAG {
        return None;
    }
    let disk = DiskId::parse(disk.to_str()?)
        .ok()
        .filter(DiskId::is_windows)?;
    let pid = pid.to_str()?;
    if pid.is_empty() || !pid.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let pid: u32 = pid.parse().ok()?;
    (pid != 0).then_some((disk, pid))
}

#[cfg(target_os = "macos")]
mod imp {
    use std::fs::File;
    use std::os::fd::{AsFd, OwnedFd};
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    use std::os::unix::net::UnixStream;
    use std::process::{Command, Stdio};

    use super::*;
    use crate::source::{device, macos_fd};

    const AUTHOPEN: &str = "/usr/libexec/authopen";

    fn wrong_fd(what: &str) -> EngineError {
        EngineError::Open {
            cause: std::io::Error::other(format!("authopen handed back {what}")),
        }
    }

    /// The same checks as opening as root (the node exists, the geometry from `diskutil`, the medium confirmed after the
    /// open), with `authopen` doing the open.
    pub(super) fn open(disk: &DiskId) -> Result<Box<dyn RawDevice>, EngineError> {
        device::open_raw_with(disk, authopen)
    }

    /// Runs `authopen -stdoutpipe /dev/rdiskN` (it shows the system's prompt, opens the node read-only and sends the
    /// descriptor over its stdout, one end of a socket pair) and receives the descriptor on the other end. A non-zero
    /// exit or no descriptor is the user saying no (`permission_denied`); `authopen` not starting is `system_tool_failed`.
    /// What comes back must be open read-only and be the raw node asked for, else `open_failed`.
    fn authopen(disk: &DiskId) -> Result<File, EngineError> {
        let (ours, theirs) = UnixStream::pair().map_err(|cause| EngineError::Open { cause })?;
        let mut child = {
            // The `Command` keeps its copy of `theirs` until dropped: dropped here, so once `authopen` exits nothing
            // holds the other end open and receiving cannot wait forever.
            let mut cmd = Command::new(AUTHOPEN);
            cmd.args(authopen_args(disk))
                .stdin(Stdio::null())
                .stdout(Stdio::from(OwnedFd::from(theirs)))
                .stderr(Stdio::null());
            cmd.spawn()
                .map_err(|_| EngineError::SystemTool { tool: AUTHOPEN })?
        };
        // Blocks until the user answers. The descriptor waits in the socket meanwhile.
        let status = child
            .wait()
            .map_err(|_| EngineError::SystemTool { tool: AUTHOPEN })?;
        let received = macos_fd::recv_fd(&ours);
        if !status.success() {
            return Err(EngineError::PermissionDenied);
        }
        let fd = received.map_err(|_| EngineError::PermissionDenied)?;
        let read_only =
            macos_fd::is_read_only(fd.as_fd()).map_err(|cause| EngineError::Open { cause })?;
        if !read_only {
            return Err(wrong_fd("a descriptor that can write"));
        }
        let file = File::from(fd);
        let got = file
            .metadata()
            .map_err(|cause| EngineError::Open { cause })?;
        let node =
            std::fs::metadata(disk.device_path()).map_err(|_| EngineError::DeviceNotFound)?;
        if !got.file_type().is_char_device() || got.rdev() != node.rdev() {
            return Err(wrong_fd("another file"));
        }
        Ok(file)
    }

    pub(super) fn elevated(_args: &[OsString]) -> Option<i32> {
        None
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use crate::source::{device, windows};

    /// Relaunches this program elevated (`runas`: the UAC prompt) with `--revenant-elevated-open <disk> <pid>`, waits for
    /// it, and adopts the handle it duplicated into this process (its exit code). The geometry comes from that handle.
    pub(super) fn open(disk: &DiskId) -> Result<Box<dyn RawDevice>, EngineError> {
        if !disk.is_windows() {
            return Err(EngineError::DeviceNotFound);
        }
        let exe = std::env::current_exe().map_err(|cause| EngineError::Open { cause })?;
        let params = format!("{ELEVATED_FLAG} {} {}", disk.as_str(), std::process::id());
        let code = windows::run_elevated(&exe, &params).map_err(|e| {
            if windows::was_cancelled(&e) {
                EngineError::PermissionDenied
            } else {
                EngineError::SystemTool { tool: "runas" }
            }
        })?;
        // The exit code is a `DWORD`; its bits as an `i32`, as `std::process::exit` set them.
        let handle = decode_exit(code as i32)?;
        device::raw_from_file(windows::file_from_handle_value(handle))
    }

    /// The elevated process: opens the disk read-only (as when run as Administrator) and duplicates the handle, with the
    /// same read-only access, into the app's process.
    pub(super) fn elevated(args: &[OsString]) -> Option<i32> {
        let Some((disk, pid)) = parse_elevated_args(args) else {
            return Some(ELEVATED_BAD_ARGS);
        };
        let result = device::open_device_file(&disk).and_then(|file| {
            windows::duplicate_into_process(&file, pid).map_err(|cause| EngineError::Open { cause })
        });
        Some(encode_exit(result))
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod imp {
    use super::*;

    pub(super) fn open(_disk: &DiskId) -> Result<Box<dyn RawDevice>, EngineError> {
        Err(EngineError::Unsupported)
    }

    pub(super) fn elevated(_args: &[OsString]) -> Option<i32> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn authopen_args_are_read_only() {
        let args = authopen_args(&DiskId::parse("disk4").unwrap());
        assert_eq!(args, ["-stdoutpipe", "/dev/rdisk4"]);
        for a in &args {
            assert!(!["-w", "-c", "-a", "-x"].contains(&a.as_str()), "{a}");
            assert!(!a.starts_with("-o"), "{a}");
        }
    }

    #[test]
    fn elevated_exit_codes_round_trip() {
        assert_eq!(decode_exit(encode_exit(Ok(0x1A4))).unwrap(), 0x1A4);
        let errors = [
            EngineError::PermissionDenied,
            EngineError::DeviceNotFound,
            EngineError::Open {
                cause: std::io::Error::other("x"),
            },
        ];
        for e in errors {
            let code = e.code();
            let exit = encode_exit(Err(e));
            assert_eq!(exit % 2, 1, "{code}: {exit}");
            assert_eq!(decode_exit(exit).unwrap_err().code(), code);
        }
        for odd in [9, 0, -1, 2, 6, ELEVATED_BAD_ARGS, i32::MIN] {
            assert_eq!(decode_exit(odd).unwrap_err().code(), "open_failed", "{odd}");
        }
        assert_eq!(
            [
                ELEVATED_DENIED,
                ELEVATED_NOT_FOUND,
                ELEVATED_OPEN_FAILED,
                ELEVATED_BAD_ARGS
            ],
            [1, 3, 5, 7]
        );
        // A value an exit code cannot carry as a handle is never encoded as one.
        for bad in [0, 2, 0x8000_0000] {
            assert_eq!(encode_exit(Ok(bad)), ELEVATED_OPEN_FAILED, "{bad}");
        }
    }

    #[test]
    fn elevated_args_parse() {
        let drive = DiskId::parse("PhysicalDrive2").unwrap();
        assert_eq!(
            parse_elevated_args(&os(&["app.exe", ELEVATED_FLAG, "PhysicalDrive2", "4242"])),
            Some((drive, 4242))
        );
        for bad in [
            os(&["app.exe", ELEVATED_FLAG, "disk4s1", "4242"]),
            os(&["app.exe", ELEVATED_FLAG, "disk4", "4242"]),
            os(&["app.exe", ELEVATED_FLAG, "PhysicalDrive2"]),
            os(&["app.exe", ELEVATED_FLAG, "PhysicalDrive2", "pid"]),
            os(&["app.exe", ELEVATED_FLAG, "PhysicalDrive2", "-1"]),
            os(&["app.exe", ELEVATED_FLAG, "PhysicalDrive2", "+42"]),
            os(&["app.exe", ELEVATED_FLAG, "PhysicalDrive2", "0"]),
            os(&["app.exe", ELEVATED_FLAG, "PhysicalDrive2", "4242", "x"]),
            os(&["app.exe", "PhysicalDrive2", "4242"]),
            os(&["app.exe", "--other", "PhysicalDrive2", "4242"]),
        ] {
            assert_eq!(parse_elevated_args(&bad), None, "{bad:?}");
        }
        for ordinary in [os(&[]), os(&["app.exe"]), os(&["app.exe", "--verbose"])] {
            assert_eq!(elevated_entry(&ordinary), None, "{ordinary:?}");
        }
    }
}
