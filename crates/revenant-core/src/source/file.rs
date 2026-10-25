use super::{Source, VolumeId, platform};
use crate::EngineError;
use std::fs::File;
use std::path::Path;

/// A [`Source`] backed by a regular file (a disk image file), opened read-only. Whole disks are read with
/// [`super::DeviceSource`].
pub struct FileSource {
    file: File,
    len: u64,
    volume: Option<VolumeId>,
}

impl FileSource {
    /// Opens `path` read-only. Only a regular file (links followed) is accepted, checked on the opened handle so a node
    /// swapped in after any earlier check is still refused: a device node, a FIFO (opened without waiting for a writer
    /// on Unix), a socket or a folder is `open_failed`, as is a file that cannot be opened.
    pub fn open(path: &Path) -> Result<Self, EngineError> {
        let open = |cause| EngineError::Open { cause };
        let file = platform::open_file_read_only(path).map_err(open)?;
        let meta = file.metadata().map_err(open)?;
        if !meta.is_file() {
            return Err(open(std::io::Error::other("not a regular file")));
        }
        let len = meta.len();
        let volume = platform::volume_of_file(&file).ok().map(VolumeId);
        Ok(Self { file, len, volume })
    }
}

impl Source for FileSource {
    fn len(&self) -> u64 {
        self.len
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, EngineError> {
        let mut filled = 0;
        while filled < buf.len() {
            let pos = offset + filled as u64;
            match platform::read_at(&self.file, pos, &mut buf[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(cause) => return Err(EngineError::Read { offset: pos, cause }),
            }
        }
        Ok(filled)
    }

    fn volume(&self) -> Option<VolumeId> {
        self.volume
    }
}
