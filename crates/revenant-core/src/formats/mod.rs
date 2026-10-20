//! File-format detectors. Each recognizes a signature at a sector start and measures the file's exact extent by
//! walking its structure.
//!
//! Detectors read through [`Reader`], which reads the source in 1 MiB windows aligned to 512-byte sectors. A window that
//! cannot be read is re-read sector by sector; unreadable sectors read as zeros and are remembered, so a read error never
//! ends a walk early (only the end of the source does) and the detector can lower the file's health instead.

pub(crate) mod isobmff;
pub(crate) mod jpeg;
pub(crate) mod raf;
mod tiffraw;

pub use isobmff::{IsoBmff, MOOV_MAX_LEN};
pub use jpeg::{JPEG_MAX_LEN, Jpeg};
pub use raf::Raf;
pub use tiffraw::TiffRaw;

use crate::source::Source;

/// Kind of file a detector found.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FileKind {
    Jpeg,
    Mp4,
    Mov,
    Cr2,
    Nef,
    Arw,
    Dng,
    Orf,
    Rw2,
    /// Fujifilm RAW.
    Raf,
    /// Canon CR3 (ISO BMFF with major brand `crx `).
    Cr3,
    /// HEIC/HEIF photo (ISO BMFF without a video `moov`).
    Heic,
    /// A TIFF that is not a recognizable camera RAW.
    Tiff,
}

impl FileKind {
    /// Stable lowercase identifier (`"jpeg"`, `"mp4"`, `"mov"`, `"cr2"`, ..., `"heic"`, `"tiff"`), for machine-readable
    /// output.
    pub fn as_str(self) -> &'static str {
        match self {
            FileKind::Jpeg => "jpeg",
            FileKind::Mp4 => "mp4",
            FileKind::Mov => "mov",
            FileKind::Cr2 => "cr2",
            FileKind::Nef => "nef",
            FileKind::Arw => "arw",
            FileKind::Dng => "dng",
            FileKind::Orf => "orf",
            FileKind::Rw2 => "rw2",
            FileKind::Raf => "raf",
            FileKind::Cr3 => "cr3",
            FileKind::Heic => "heic",
            FileKind::Tiff => "tiff",
        }
    }

    /// Conventional file extension, without the dot.
    pub fn extension(self) -> &'static str {
        match self {
            FileKind::Jpeg => "jpg",
            FileKind::Mp4 => "mp4",
            FileKind::Mov => "mov",
            FileKind::Cr2 => "cr2",
            FileKind::Nef => "nef",
            FileKind::Arw => "arw",
            FileKind::Dng => "dng",
            FileKind::Orf => "orf",
            FileKind::Rw2 => "rw2",
            FileKind::Raf => "raf",
            FileKind::Cr3 => "cr3",
            FileKind::Heic => "heic",
            FileKind::Tiff => "tif",
        }
    }
}

/// How complete a measured file is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Health {
    /// Structure complete and content decodes cleanly.
    Intact,
    /// Salvageable but not clean: the structure ends early (cut by another file or the end of the source), the
    /// structure is complete and only a lenient decoder accepts the content, or the file contains unreadable sectors.
    Partial,
    /// Structure complete but no decoder accepts the content, or the index the file needs to open is missing or
    /// unreadable.
    Damaged,
}

/// One piece of a file: the absolute byte range `[start, start + len)` of the source.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Fragment {
    pub start: u64,
    pub len: u64,
}

/// Result of measuring a file at some offset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Measurement {
    /// Total length of the file (the sum of its pieces when it has any).
    pub len: u64,
    pub kind: FileKind,
    pub health: Health,
    /// Empty for a contiguous file. Otherwise at least 2 pieces in file order: the first starts at the measured start,
    /// the lengths sum to `len`, no two overlap.
    pub fragments: Vec<Fragment>,
}

pub trait Format: Send + Sync {
    fn name(&self) -> &'static str;

    /// `head`: up to 16 bytes read at a sector start.
    fn recognizes(&self, head: &[u8]) -> bool;

    /// Header-level validation at `start` (cheap, never recursive, never measures the whole file): true when a real file
    /// of this format plausibly starts here. Used to decide that "another file starts here", so a stray byte pattern
    /// that merely looks like a signature cannot cut a file.
    fn confirms(&self, source: &dyn Source, start: u64) -> bool;

    /// Measures the file starting at `start`. `None` means "not a file of this format", never "could not read": an
    /// unreadable sector inside a file lowers its health instead (see [`Health`]).
    fn measure(&self, source: &dyn Source, start: u64) -> Option<Measurement>;
}

/// Every supported format.
pub fn all_formats() -> Vec<Box<dyn Format>> {
    vec![
        Box::new(Jpeg),
        Box::new(IsoBmff),
        Box::new(TiffRaw),
        Box::new(Raf),
    ]
}

/// Bytes of a sector start shown to [`Format::recognizes`].
pub(crate) const HEAD_LEN: usize = 16;
/// Sector size used by the detectors: unreadable-sector granularity and "another file starts here" checks.
pub(crate) const SECTOR: u64 = 512;
/// Bytes read per window (a whole number of sectors).
const WINDOW: usize = 1 << 20;

/// Windowed, sector-aligned reader shared by the detectors. Never reads a file whole.
pub(crate) struct Reader<'a> {
    source: &'a dyn Source,
    len: u64,
    buf: Vec<u8>,
    buf_start: u64,
    buf_len: usize,
    /// Starts of the unreadable sectors seen so far, sorted.
    bad: Vec<u64>,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(source: &'a dyn Source) -> Self {
        Self {
            source,
            len: source.len(),
            buf: vec![0; WINDOW],
            buf_start: 0,
            buf_len: 0,
            bad: Vec::new(),
        }
    }

    /// Makes `pos` part of the window. `false` only at (or past) the end of the source.
    fn load(&mut self, pos: u64) -> bool {
        if pos >= self.buf_start && pos < self.buf_start + self.buf_len as u64 {
            return true;
        }
        if pos >= self.len {
            return false;
        }
        let start = pos - pos % SECTOR;
        let want = (self.len - start).min(WINDOW as u64) as usize;
        self.buf_start = start;
        self.buf_len = match self.source.read_at(start, &mut self.buf[..want]) {
            Ok(n) => n,
            Err(_) => self.load_by_sector(start, want),
        };
        pos < self.buf_start + self.buf_len as u64
    }

    /// Re-reads a failed window sector by sector; unreadable sectors become zeros and are remembered.
    fn load_by_sector(&mut self, start: u64, want: usize) -> usize {
        let mut filled = 0;
        for (i, chunk) in self.buf[..want].chunks_mut(SECTOR as usize).enumerate() {
            let at = start + (i as u64) * SECTOR;
            match self.source.read_at(at, chunk) {
                Ok(n) if n < chunk.len() => return filled + n, // end of the source
                Ok(n) => filled += n,
                Err(_) => {
                    chunk.fill(0);
                    filled += chunk.len();
                    if let Err(k) = self.bad.binary_search(&at) {
                        self.bad.insert(k, at);
                    }
                }
            }
        }
        filled
    }

    /// The byte at `pos`; `None` only at the end of the source.
    pub(crate) fn byte(&mut self, pos: u64) -> Option<u8> {
        self.load(pos)
            .then(|| self.buf[(pos - self.buf_start) as usize])
    }

    pub(crate) fn u16(&mut self, pos: u64) -> Option<u16> {
        Some(u16::from_be_bytes([self.byte(pos)?, self.byte(pos + 1)?]))
    }

    /// Copies bytes from `pos` into `out`; returns how many, fewer only at the end of the source.
    pub(crate) fn read(&mut self, pos: u64, out: &mut [u8]) -> usize {
        let mut done = 0;
        while done < out.len() && self.load(pos + done as u64) {
            let from = (pos + done as u64 - self.buf_start) as usize;
            let n = (self.buf_len - from).min(out.len() - done);
            out[done..done + n].copy_from_slice(&self.buf[from..from + n]);
            done += n;
        }
        done
    }

    /// Up to [`HEAD_LEN`] bytes at `pos` (fewer only at the end of the source).
    pub(crate) fn head(&mut self, pos: u64) -> ([u8; HEAD_LEN], usize) {
        let mut head = [0u8; HEAD_LEN];
        let n = self.read(pos, &mut head);
        (head, n)
    }

    /// Whether a file of any format starts at `pos`: some format recognizes the head and confirms its header.
    pub(crate) fn file_starts_at(&mut self, formats: &[Box<dyn Format>], pos: u64) -> bool {
        let (head, n) = self.head(pos);
        let source = self.source;
        formats
            .iter()
            .any(|f| f.recognizes(&head[..n]) && f.confirms(source, pos))
    }

    /// Whether an unreadable sector seen so far overlaps `[start, end)`.
    pub(crate) fn bad_in(&self, start: u64, end: u64) -> bool {
        let first = self.bad.partition_point(|&s| s + SECTOR <= start);
        self.bad.get(first).is_some_and(|&s| s < end)
    }
}
