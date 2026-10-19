//! File-format detectors. Each recognizes a signature at a sector start and measures the file's exact extent by
//! walking its structure.

mod isobmff;
mod jpeg;

pub use isobmff::{IsoBmff, MOOV_MAX_LEN};
pub use jpeg::{JPEG_MAX_LEN, Jpeg};

use crate::source::Source;

/// Kind of file a detector found.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FileKind {
    Jpeg,
    Mp4,
    Mov,
}

impl FileKind {
    /// Conventional file extension, without the dot.
    pub fn extension(self) -> &'static str {
        match self {
            FileKind::Jpeg => "jpg",
            FileKind::Mp4 => "mp4",
            FileKind::Mov => "mov",
        }
    }
}

/// How complete a measured file is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Health {
    /// Structure complete and content decodes cleanly.
    Intact,
    /// Salvageable but not clean: the structure ends early (cut by another file or the end of the source), or the
    /// structure is complete and only a lenient decoder accepts the content.
    Partial,
    /// Structure complete but no decoder accepts the content.
    Damaged,
}

/// Result of measuring a file at some offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Measurement {
    pub len: u64,
    pub kind: FileKind,
    pub health: Health,
}

pub trait Format: Send + Sync {
    fn name(&self) -> &'static str;

    /// `head`: up to 16 bytes read at a sector start.
    fn recognizes(&self, head: &[u8]) -> bool;

    /// Measures the file starting at `start`. `None` = not a file of this format (or the source could not be read).
    fn measure(&self, source: &dyn Source, start: u64) -> Option<Measurement>;
}

/// Every supported format.
pub fn all_formats() -> Vec<Box<dyn Format>> {
    vec![Box::new(Jpeg), Box::new(IsoBmff)]
}
