use super::{Source, VolumeId, platform};
use crate::EngineError;
use std::fs::File;
use std::path::Path;

/// A [`Source`] backed by a regular file or a raw device node, opened read-only.
pub struct FileSource {
    file: File,
    len: u64,
    volume: Option<VolumeId>,
}

impl FileSource {
    pub fn open(path: &Path) -> Result<Self, EngineError> {
        let open = |cause| EngineError::Open { cause };
        let file = platform::open_read_only(path).map_err(open)?;
        let len = file.metadata().map_err(open)?.len();
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
