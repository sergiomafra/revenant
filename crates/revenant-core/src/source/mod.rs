//! Read-only positional access to the data being recovered.

mod device;
mod file;
mod fragmented;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
pub(crate) mod windows;

#[cfg(unix)]
use unix as platform;
#[cfg(windows)]
use windows as platform;

use crate::EngineError;

pub(crate) use device::open_raw;
pub use device::{DeviceSource, DiskId, Geometry, RawDevice};
pub use file::FileSource;
pub use fragmented::Fragmented;

/// Opaque identifier of a volume (device on Unix, volume serial number on Windows).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct VolumeId(pub u64);

/// A read-only byte source. There is deliberately no write method.
pub trait Source: Send + Sync {
    fn len(&self) -> u64;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Reads from `offset`. Fills the whole buffer except at the end of the source (returns less; 0 past the end).
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, EngineError>;

    /// Volume the source lives on (to refuse extracting onto it). `None` if unknown.
    fn volume(&self) -> Option<VolumeId>;

    /// The whole disk the source reads, when it is a device (extraction and copies refuse every volume of it).
    fn disk(&self) -> Option<DiskId> {
        None
    }
}

/// Fills `buf` from `offset` of a source whose `len()` covers it. A short read means the source vanished there: it is
/// `DeviceGone` at the first byte missing (ruling D1).
pub(crate) fn read_full(
    source: &dyn Source,
    offset: u64,
    buf: &mut [u8],
) -> Result<(), EngineError> {
    let n = source.read_at(offset, buf)?;
    if n < buf.len() {
        return Err(EngineError::DeviceGone {
            offset: offset + n as u64,
        });
    }
    Ok(())
}

/// What sits where only a regular file the engine wrote belongs (a rescue map, a session file).
pub(crate) enum Regular {
    /// Nothing there.
    Missing,
    /// A link, a folder, a device, a FIFO or a socket.
    Other,
    File(std::fs::File),
}

/// Opens `path` read-only when it is a regular file: looked at without following a link, then opened without blocking
/// on a FIFO (and, where the system allows, without following a link swapped in meanwhile) and checked again on the
/// handle, so nothing but a regular file is ever read and nothing hangs.
pub(crate) fn open_regular(path: &std::path::Path) -> std::io::Result<Regular> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => return Ok(Regular::Other),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Regular::Missing),
        Err(e) => return Err(e),
    }
    let file = platform::open_file_no_follow(path)?;
    if !file.metadata()?.is_file() {
        return Ok(Regular::Other);
    }
    Ok(Regular::File(file))
}

/// Whether the open file has a single name: a file the engine writes into must not be a hard link to a file elsewhere.
pub(crate) fn single_link(file: &std::fs::File) -> std::io::Result<bool> {
    Ok(platform::link_count(file)? == 1)
}

/// Volume of an existing path (file or directory).
pub fn volume_of(path: &std::path::Path) -> Result<VolumeId, EngineError> {
    let file = platform::open_read_only(path).map_err(|cause| EngineError::Open { cause })?;
    platform::volume_of_file(&file)
        .map(VolumeId)
        .map_err(|cause| EngineError::Open { cause })
}
