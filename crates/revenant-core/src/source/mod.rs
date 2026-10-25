//! Read-only positional access to the data being recovered.

mod device;
mod file;
mod fragmented;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
use unix as platform;
#[cfg(windows)]
use windows as platform;

use crate::EngineError;

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

/// Volume of an existing path (file or directory).
pub fn volume_of(path: &std::path::Path) -> Result<VolumeId, EngineError> {
    let file = platform::open_read_only(path).map_err(|cause| EngineError::Open { cause })?;
    platform::volume_of_file(&file)
        .map(VolumeId)
        .map_err(|cause| EngineError::Open { cause })
}
