//! The files of a saved session (ruling D25) and how they are written and read back.
//!
//! Every file starts with `RVNTSESS`, a kind byte and the format version (`u16`, little-endian), then its payload:
//!
//! - `session.rvs` (the manifest), `listing.rvs` and `checkpoint-a.rvs`/`checkpoint-b.rvs`: the CRC-32 of the rest
//!   (`u32`), then the encoded value. Each is written whole into a new temporary file, synced and renamed over its name,
//!   so a crash leaves the previous file or the new one.
//! - `events.rvs`: records `[len u32][crc32 u32][data]`, only appended; what lies past the length the newest valid
//!   checkpoint names is dropped.
//!
//! Nothing here follows a link planted in the folder: new files are created exclusively (a planted temporary is removed
//! first, never what it points to), a rename replaces a link rather than its target, the event log is reopened only
//! after checking that the file opened is the regular, single-link file looked at, and every file read is checked to be
//! a regular file and read up to a cap.

use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::path::Path;

use crate::EngineError;
use crate::filesystem::crc32;

const MAGIC: &[u8; 8] = b"RVNTSESS";
/// The format version this engine writes and reads.
pub(super) const VERSION: u16 = 1;
/// Magic, kind and version.
pub(super) const HEADER_LEN: u64 = 11;

pub(super) const MANIFEST: &str = "session.rvs";
pub(super) const LISTING: &str = "listing.rvs";
pub(super) const EVENTS: &str = "events.rvs";
/// The two checkpoints, written alternately: generation `g` goes to `CHECKPOINTS[g % 2]`.
pub(super) const CHECKPOINTS: [&str; 2] = ["checkpoint-a.rvs", "checkpoint-b.rvs"];

/// Largest manifest read back (an identity and three numbers).
pub(super) const MAX_MANIFEST_FILE: u64 = 1 << 20;
/// Largest listing read back: the listing's caps (262,144 files, 2^20 pieces) stay far below it.
pub(super) const MAX_LISTING_FILE: u64 = 1 << 30;
/// Largest checkpoint read back: a scan state is bounded by the scan's caps (the bad map alone up to about 16 MiB), plus
/// one start per finding.
pub(super) const MAX_CHECKPOINT_FILE: u64 = 1 << 30;
/// Largest event record: a finding with every piece the listing can hold (2^20 × 16 bytes) fits.
pub(super) const MAX_RECORD: usize = 32 << 20;

/// What a file holds (its kind byte).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Manifest = 1,
    Listing = 2,
    Events = 3,
    Checkpoint = 4,
}

fn corrupt<T>() -> Result<T, EngineError> {
    Err(EngineError::SessionCorrupt)
}

fn write_err(cause: std::io::Error) -> EngineError {
    EngineError::Write { cause }
}

/// The header of a file of `kind`.
pub(super) fn header(kind: Kind) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN as usize);
    out.extend_from_slice(MAGIC);
    out.push(kind as u8);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out
}

/// A whole file of `kind`: header, CRC-32 of `payload`, `payload`.
pub(super) fn framed(kind: Kind, payload: &[u8]) -> Vec<u8> {
    let mut out = header(kind);
    out.extend_from_slice(&crc32(payload).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// Checks the header of a file of `kind`: `session_version` for another version of a session file of this kind,
/// `session_corrupt` for anything else wrong.
pub(super) fn check_header(kind: Kind, bytes: &[u8]) -> Result<(), EngineError> {
    if bytes.len() < HEADER_LEN as usize || &bytes[..8] != MAGIC || bytes[8] != kind as u8 {
        return corrupt();
    }
    if u16::from_le_bytes([bytes[9], bytes[10]]) != VERSION {
        return Err(EngineError::SessionVersion);
    }
    Ok(())
}

/// The payload of a file made by [`framed`], its header and checksum checked.
pub(super) fn unframe(kind: Kind, bytes: &[u8]) -> Result<&[u8], EngineError> {
    check_header(kind, bytes)?;
    let rest = &bytes[HEADER_LEN as usize..];
    if rest.len() < 4 {
        return corrupt();
    }
    let (crc, payload) = rest.split_at(4);
    if u32::from_le_bytes([crc[0], crc[1], crc[2], crc[3]]) != crc32(payload) {
        return corrupt();
    }
    Ok(payload)
}

/// One event record: length, CRC-32, data.
pub(super) fn record(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + data.len());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&crc32(data).to_le_bytes());
    out.extend_from_slice(data);
    out
}

/// Opens `path` for reading when it is a regular file (a link, a folder or a device is `session_corrupt`); `None` when
/// there is nothing there.
pub(super) fn open_regular(path: &Path) -> Result<Option<File>, EngineError> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => return corrupt(),
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(cause) => return Err(EngineError::Open { cause }),
    }
    let file = File::open(path).map_err(|cause| EngineError::Open { cause })?;
    let meta = file
        .metadata()
        .map_err(|cause| EngineError::Open { cause })?;
    if !meta.is_file() {
        return corrupt();
    }
    Ok(Some(file))
}

/// The whole of the regular file `path`, at most `cap` bytes (a longer one is `session_corrupt`); `None` when there is
/// nothing there.
pub(super) fn read_file(path: &Path, cap: u64) -> Result<Option<Vec<u8>>, EngineError> {
    let Some(file) = open_regular(path)? else {
        return Ok(None);
    };
    let mut bytes = Vec::new();
    file.take(cap + 1)
        .read_to_end(&mut bytes)
        .map_err(|cause| EngineError::Open { cause })?;
    if bytes.len() as u64 > cap {
        return corrupt();
    }
    Ok(Some(bytes))
}

/// Creates `dir/name` (never there before: `create_new`) holding `bytes`, synced.
pub(super) fn create(dir: &Path, name: &str, bytes: &[u8]) -> Result<(), EngineError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dir.join(name))
        .map_err(write_err)?;
    file.write_all(bytes).map_err(write_err)?;
    file.sync_all().map_err(write_err)
}

/// Replaces `dir/name` with `bytes` so that a crash at any moment leaves the previous file or this one: into a new
/// `<name>.tmp` (whatever was there is removed first, a link included, never what it points to; then created
/// exclusively, which never follows a link), `sync_all`, renamed over `name` (a link there is replaced, not followed),
/// then the folder synced, best effort (ruling P33).
pub(super) fn replace(dir: &Path, name: &str, bytes: &[u8]) -> Result<(), EngineError> {
    let tmp_name = format!("{name}.tmp");
    let tmp = dir.join(&tmp_name);
    match std::fs::remove_file(&tmp) {
        Err(e) if e.kind() != ErrorKind::NotFound => return Err(write_err(e)),
        _ => {}
    }
    create(dir, &tmp_name, bytes)?;
    std::fs::rename(&tmp, dir.join(name)).map_err(write_err)?;
    sync_dir(dir);
    Ok(())
}

/// Syncs the folder itself, so a file created or renamed in it survives a power loss. Best effort (ruling P33): some
/// file systems refuse it, and the files are complete and synced already. Nothing to do on Windows.
pub(super) fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    {
        let _ = File::open(dir).and_then(|d| d.sync_all());
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
    }
}

/// Opens the existing event log `path` for writing, never creating nor truncating it. Refused (`write_failed`) unless
/// the file opened is the regular file looked at just before (same device and inode, a single link, on Unix); on
/// Windows a reparse point is opened itself, not followed, so the regular-file check refuses it.
pub(super) fn open_log(path: &Path) -> Result<File, EngineError> {
    let refused = || write_err(std::io::Error::from(ErrorKind::InvalidInput));
    let checked = std::fs::symlink_metadata(path).map_err(write_err)?;
    if !checked.is_file() {
        return Err(refused());
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path).map_err(write_err)?;
    let opened = file.metadata().map_err(write_err)?;
    if !opened.is_file() || !same_single_file(&checked, &opened) {
        return Err(refused());
    }
    Ok(file)
}

/// Whether `opened` is the file `checked` described and has a single link (a hard link to a file elsewhere would let
/// the log write into it).
#[cfg(unix)]
fn same_single_file(checked: &std::fs::Metadata, opened: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    (checked.dev(), checked.ino()) == (opened.dev(), opened.ino()) && opened.nlink() == 1
}

#[cfg(not(unix))]
fn same_single_file(_checked: &std::fs::Metadata, _opened: &std::fs::Metadata) -> bool {
    true
}

/// Reads event records from a log, up to a length.
pub(super) struct Records<R> {
    reader: R,
    /// Bytes of records left to read.
    left: u64,
}

impl<R: Read> Records<R> {
    /// The records of `reader` (positioned after the header), `len` bytes of them.
    pub(super) fn new(reader: R, len: u64) -> Self {
        Self { reader, left: len }
    }

    fn take(&mut self, buf: &mut [u8]) -> Result<(), EngineError> {
        if buf.len() as u64 > self.left {
            return corrupt();
        }
        self.reader.read_exact(buf).or_else(|_| corrupt())?;
        self.left -= buf.len() as u64;
        Ok(())
    }

    /// The next record's data; `None` at the end. A record cut short, too long or whose checksum does not match is
    /// `session_corrupt`.
    pub(super) fn next_record(&mut self) -> Result<Option<Vec<u8>>, EngineError> {
        if self.left == 0 {
            return Ok(None);
        }
        let mut head = [0u8; 8];
        self.take(&mut head)?;
        let len = u32::from_le_bytes([head[0], head[1], head[2], head[3]]) as usize;
        let crc = u32::from_le_bytes([head[4], head[5], head[6], head[7]]);
        if len > MAX_RECORD || len as u64 > self.left {
            return corrupt();
        }
        let mut data = vec![0u8; len];
        self.take(&mut data)?;
        if crc32(&data) != crc {
            return corrupt();
        }
        Ok(Some(data))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_and_damage_is_caught() {
        let bytes = framed(Kind::Checkpoint, b"payload");
        assert_eq!(unframe(Kind::Checkpoint, &bytes).unwrap(), b"payload");
        // Another kind, a flipped byte anywhere, a cut: corrupt.
        assert!(matches!(
            unframe(Kind::Manifest, &bytes),
            Err(EngineError::SessionCorrupt)
        ));
        for i in (0..bytes.len()).filter(|&i| !(9..11).contains(&i)) {
            let mut b = bytes.clone();
            b[i] ^= 0x10;
            assert!(
                matches!(
                    unframe(Kind::Checkpoint, &b),
                    Err(EngineError::SessionCorrupt)
                ),
                "byte {i}"
            );
        }
        for cut in 0..bytes.len() {
            assert!(unframe(Kind::Checkpoint, &bytes[..cut]).is_err());
        }
        // Another version: `session_version`.
        let mut b = bytes.clone();
        b[9] = 2;
        assert!(matches!(
            unframe(Kind::Checkpoint, &b),
            Err(EngineError::SessionVersion)
        ));
    }

    #[test]
    fn records_are_bounded_and_checked() {
        let mut log = record(b"one");
        log.extend(record(b"two"));
        let mut r = Records::new(&log[..], log.len() as u64);
        assert_eq!(r.next_record().unwrap().unwrap(), b"one");
        assert_eq!(r.next_record().unwrap().unwrap(), b"two");
        assert!(r.next_record().unwrap().is_none());
        // Cut inside the second record.
        let mut r = Records::new(&log[..], log.len() as u64 - 1);
        r.next_record().unwrap();
        assert!(r.next_record().is_err());
        // A length past the cap or past what remains, a wrong checksum.
        let mut huge = ((MAX_RECORD + 1) as u32).to_le_bytes().to_vec();
        huge.extend([0; 4]);
        assert!(Records::new(&huge[..], 1 << 40).next_record().is_err());
        let mut long = 100u32.to_le_bytes().to_vec();
        long.extend([0; 8]);
        assert!(Records::new(&long[..], 12).next_record().is_err());
        let mut bad = record(b"one");
        bad[9] ^= 1;
        assert!(
            Records::new(&bad[..], bad.len() as u64)
                .next_record()
                .is_err()
        );
    }

    #[test]
    fn reads_only_regular_files_up_to_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        assert!(read_file(&path, 10).unwrap().is_none());
        std::fs::write(&path, [1u8; 11]).unwrap();
        assert!(matches!(
            read_file(&path, 10),
            Err(EngineError::SessionCorrupt)
        ));
        assert_eq!(read_file(&path, 11).unwrap().unwrap().len(), 11);
        assert!(matches!(
            read_file(dir.path(), 10),
            Err(EngineError::SessionCorrupt)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn links_are_never_followed() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("victim");
        std::fs::write(&target, b"keep").unwrap();
        // A link planted at the temporary name and at the final name: replaced, the target untouched.
        std::os::unix::fs::symlink(&target, dir.path().join("x.rvs.tmp")).unwrap();
        std::os::unix::fs::symlink(&target, dir.path().join("x.rvs")).unwrap();
        replace(dir.path(), "x.rvs", b"new").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"keep");
        assert_eq!(std::fs::read(dir.path().join("x.rvs")).unwrap(), b"new");
        assert!(
            !std::fs::symlink_metadata(dir.path().join("x.rvs"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        // The log: a link, or a second hard link to a file elsewhere, is refused; a link is not read either.
        let log = dir.path().join(EVENTS);
        std::os::unix::fs::symlink(&target, &log).unwrap();
        assert_eq!(open_log(&log).unwrap_err().code(), "write_failed");
        assert!(matches!(
            read_file(&log, 100),
            Err(EngineError::SessionCorrupt)
        ));
        std::fs::remove_file(&log).unwrap();
        std::fs::hard_link(&target, &log).unwrap();
        assert_eq!(open_log(&log).unwrap_err().code(), "write_failed");
        assert_eq!(std::fs::read(&target).unwrap(), b"keep");
        std::fs::remove_file(&log).unwrap();
        std::fs::write(&log, b"own").unwrap();
        assert!(open_log(&log).is_ok());
    }
}
