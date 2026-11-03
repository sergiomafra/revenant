//! Receiving an open file descriptor over a Unix socket (`SCM_RIGHTS`), macOS only: how `/usr/libexec/authopen
//! -stdoutpipe` hands back the disk it opened after the system's permission prompt. With `source/windows.rs`, the only
//! file allowed `unsafe` (CLAUDE.md, Sérgio, 2.1); every block says why it is sound.

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;

/// Most descriptors one message may carry that are received (and closed, past the first) rather than left in flight.
const MAX_FDS: usize = 8;

/// Bytes of control data for `MAX_FDS` descriptors.
// SAFETY: `CMSG_SPACE` is arithmetic on its argument only (it is `unsafe` in libc, not in what it does).
const CONTROL_LEN: usize =
    unsafe { libc::CMSG_SPACE((MAX_FDS * size_of::<libc::c_int>()) as u32) } as usize;

/// Receives exactly one descriptor sent over `stream` with `SCM_RIGHTS` in one message (read once, whatever bytes come
/// with it). No descriptor (only bytes, or the peer closed the stream) or more than one is an error; every descriptor
/// received beyond the one returned is closed, and so is every one received when the call fails.
pub(crate) fn recv_fd(stream: &UnixStream) -> io::Result<OwnedFd> {
    let mut data = [0u8; 64];
    // `u64` elements keep the control buffer aligned for `cmsghdr`.
    let mut control = [0u64; CONTROL_LEN.div_ceil(8)];
    let mut iov = libc::iovec {
        iov_base: data.as_mut_ptr().cast(),
        iov_len: data.len(),
    };
    // SAFETY: `msghdr` is plain data for which all-zero is valid (no name, no buffers, no flags).
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = (control.len() * 8) as libc::socklen_t;
    let n = loop {
        // SAFETY: the socket is open for the lifetime of `stream`; `msg` points at `iov` (which describes `data`) and at
        // `control`, all alive and writable for the call, with the lengths given.
        let n = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut msg, 0) };
        if n >= 0 {
            break n;
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    };
    let mut fds = Vec::new();
    // SAFETY: `msg` was filled by `recvmsg`: `msg_control`/`msg_controllen` describe the control data it wrote, inside
    // `control`.
    let mut cmsg = unsafe { libc::CMSG_FIRSTHDR(&msg) };
    while !cmsg.is_null() {
        // SAFETY: `cmsg` is non-null and points at a control header inside `control` (as `CMSG_FIRSTHDR`/`CMSG_NXTHDR`
        // return it).
        let header = unsafe { &*cmsg };
        if header.cmsg_level == libc::SOL_SOCKET && header.cmsg_type == libc::SCM_RIGHTS {
            // SAFETY: as above; `CMSG_DATA` is the start of this header's data and `CMSG_LEN(0)` its header length.
            let (start, head) = unsafe { (libc::CMSG_DATA(cmsg), libc::CMSG_LEN(0) as usize) };
            let count = (header.cmsg_len as usize).saturating_sub(head) / size_of::<libc::c_int>();
            for i in 0..count {
                // SAFETY: `count` whole `c_int`s follow `start` inside this header's `cmsg_len`, within `control`; read
                // unaligned since the data need not be aligned for `c_int`.
                let raw = unsafe { start.cast::<libc::c_int>().add(i).read_unaligned() };
                if raw >= 0 {
                    // SAFETY: the kernel installed `raw` in this process for this message: it is open and owned by
                    // nothing else, so it is ours to close (exactly once, through `OwnedFd`).
                    fds.push(unsafe { OwnedFd::from_raw_fd(raw) });
                }
            }
        }
        // SAFETY: `msg` and `cmsg` are as above; `CMSG_NXTHDR` returns null past the control data.
        cmsg = unsafe { libc::CMSG_NXTHDR(&msg, cmsg) };
    }
    if msg.msg_flags & libc::MSG_CTRUNC != 0 {
        return Err(io::Error::other("descriptor message truncated"));
    }
    if n == 0 && fds.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "stream closed without a descriptor",
        ));
    }
    let mut fds = fds.into_iter();
    match (fds.next(), fds.next()) {
        (Some(fd), None) => Ok(fd),
        (None, _) => Err(io::Error::other("no descriptor received")),
        (Some(_), Some(_)) => Err(io::Error::other("more than one descriptor received")),
    }
}

/// Whether `fd` is open for reading only (`fcntl(F_GETFL)`, access mode `O_RDONLY`): what is received is checked, so a
/// descriptor that could write never becomes a source.
pub(crate) fn is_read_only(fd: BorrowedFd<'_>) -> io::Result<bool> {
    // SAFETY: `fd` is open for the lifetime of the borrow; `F_GETFL` takes no third argument and changes nothing.
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(flags & libc::O_ACCMODE == libc::O_RDONLY)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::fd::{AsFd, RawFd};

    /// Sends `fds` over `stream` with `SCM_RIGHTS`, with one byte of data (what `authopen` does).
    fn send_fds(stream: &UnixStream, fds: &[RawFd]) {
        let mut byte = [0u8; 1];
        let mut iov = libc::iovec {
            iov_base: byte.as_mut_ptr().cast(),
            iov_len: 1,
        };
        let mut control = [0u64; CONTROL_LEN.div_ceil(8)];
        // SAFETY: `msghdr` is plain data for which all-zero is valid.
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        // SAFETY: arithmetic on the argument only.
        let space = unsafe { libc::CMSG_SPACE(size_of_val(fds) as u32) };
        msg.msg_controllen = space as libc::socklen_t;
        // SAFETY: `msg_control` is `control`, at least `space` bytes (fds.len() <= MAX_FDS), so the first header fits;
        // its data area holds `fds.len()` `c_int`s by `CMSG_LEN`.
        unsafe {
            let cmsg = libc::CMSG_FIRSTHDR(&msg);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(size_of_val(fds) as u32) as libc::socklen_t;
            let data = libc::CMSG_DATA(cmsg).cast::<libc::c_int>();
            for (i, fd) in fds.iter().enumerate() {
                data.add(i).write_unaligned(*fd);
            }
        }
        // SAFETY: the socket is open; `msg` describes `byte` and `control`, alive for the call.
        let sent = unsafe { libc::sendmsg(stream.as_raw_fd(), &msg, 0) };
        assert_eq!(sent, 1, "{}", io::Error::last_os_error());
    }

    fn sample_file(dir: &std::path::Path) -> std::fs::File {
        let path = dir.join("sample.bin");
        std::fs::write(&path, b"revenant").unwrap();
        std::fs::File::open(&path).unwrap()
    }

    #[test]
    fn recv_fd_receives_a_sent_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = sample_file(dir.path());
        let (a, b) = UnixStream::pair().unwrap();
        send_fds(&a, &[file.as_raw_fd()]);
        drop(file);
        let fd = recv_fd(&b).unwrap();
        assert!(is_read_only(fd.as_fd()).unwrap());
        let mut got = Vec::new();
        std::fs::File::from(fd).read_to_end(&mut got).unwrap();
        assert_eq!(got, b"revenant");
    }

    #[test]
    fn recv_fd_without_descriptor_fails() {
        let (a, b) = UnixStream::pair().unwrap();
        std::io::Write::write_all(&mut &a, b"only bytes").unwrap();
        assert!(recv_fd(&b).is_err());
    }

    #[test]
    fn recv_fd_on_closed_stream_fails() {
        let (a, b) = UnixStream::pair().unwrap();
        drop(a);
        assert!(recv_fd(&b).is_err());
    }

    #[test]
    fn recv_fd_refuses_two_descriptors() {
        let dir = tempfile::tempdir().unwrap();
        let file = sample_file(dir.path());
        let (a, b) = UnixStream::pair().unwrap();
        send_fds(&a, &[file.as_raw_fd(), file.as_raw_fd()]);
        assert!(recv_fd(&b).is_err());
    }

    #[test]
    fn a_read_write_descriptor_is_not_read_only() {
        let file = tempfile::tempfile().unwrap();
        assert!(!is_read_only(file.as_fd()).unwrap());
    }
}
