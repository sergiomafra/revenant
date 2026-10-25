//! An image read through its rescue map (ruling D18): what was never rescued is unreadable, never zeros.

use std::path::Path;

use super::map::{RegionState, RescueMap, map_path};
use crate::EngineError;
use crate::source::{FileSource, Source, VolumeId};

/// The image of a rescue copy, read through its map (`<image>.rvmap`): a read touching any byte that is not `Rescued`
/// fails with `Read { offset }` of the first such byte, as a bad sector of the card would, so the scan's cache resolves
/// it sector by sector, reports the hole as `Unreadable` and calls nothing in it `Intact`. `volume()` is the image
/// file's. The map is read once, at open.
pub struct MappedSource {
    image: FileSource,
    map: RescueMap,
}

impl MappedSource {
    /// Opens `image` read-only and loads its map: `map_mismatch` when the map's size is not the image's length (and the
    /// errors of [`FileSource::open`] and [`RescueMap::load`]).
    pub fn open(image: &Path) -> Result<MappedSource, EngineError> {
        let file = FileSource::open(image)?;
        let map = RescueMap::load(&map_path(image))?;
        if map.size() != file.len() {
            return Err(EngineError::MapMismatch);
        }
        Ok(MappedSource { image: file, map })
    }

    pub fn map(&self) -> &RescueMap {
        &self.map
    }

    /// Like `read_at`, but a byte that is not `Rescued` reads as 0 instead of failing (only for
    /// `SourceIdentity::of_mapped`).
    pub(crate) fn read_zero_filled(
        &self,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<usize, EngineError> {
        let n = self.image.read_at(offset, buf)?;
        for (start, len, state) in self.map.runs(offset, offset + n as u64) {
            if state != RegionState::Rescued {
                let from = (start - offset) as usize;
                buf[from..from + len as usize].fill(0);
            }
        }
        Ok(n)
    }
}

impl Source for MappedSource {
    fn len(&self) -> u64 {
        self.image.len()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, EngineError> {
        let end = offset.saturating_add(buf.len() as u64).min(self.len());
        if let Some(at) = self.map.first_unrescued(offset, end) {
            return Err(EngineError::Read {
                offset: at,
                cause: std::io::Error::other("not rescued"),
            });
        }
        self.image.read_at(offset, buf)
    }

    fn volume(&self) -> Option<VolumeId> {
        self.image.volume()
    }
}
