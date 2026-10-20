//! File-format detectors. Each recognizes a signature at a sector start and measures the file's exact extent by
//! walking its structure.
//!
//! Detectors read through [`Reader`], which reads the source in 1 MiB windows aligned to 512-byte sectors. A window that
//! cannot be read is re-read sector by sector; unreadable sectors read as zeros and are remembered, so a read error never
//! ends a walk early (only the end of the source does) and the detector can lower the file's health instead.

pub(crate) mod isobmff;
pub(crate) mod jpeg;
pub(crate) mod jpeg_entropy;
mod jpeg_join;
pub(crate) mod raf;
mod tiffraw;

pub use isobmff::{IsoBmff, MOOV_MAX_LEN};
pub use jpeg::{JPEG_MAX_LEN, Jpeg};
pub use raf::Raf;
pub use tiffraw::TiffRaw;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::OnceLock;

use crate::scan::CancelToken;
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

/// Every supported format, built once. The detectors use this instead of calling [`all_formats`] on every measurement.
pub(crate) fn builtin() -> &'static [Box<dyn Format>] {
    static FORMATS: OnceLock<Vec<Box<dyn Format>>> = OnceLock::new();
    FORMATS.get_or_init(all_formats)
}

thread_local! {
    /// The cancel token of the scan whose measurement runs on this thread, if any.
    static CANCEL: RefCell<Option<CancelToken>> = const { RefCell::new(None) };
}

/// While alive, [`cancelled`] on this thread reports `token`: a measurement that can run long (the JPEG gap search)
/// looks at it and gives up when the scan is cancelled. Restores the previous token when dropped.
pub(crate) struct CancelScope(Option<CancelToken>);

impl CancelScope {
    pub(crate) fn enter(token: &CancelToken) -> Self {
        Self(CANCEL.with(|c| c.replace(Some(token.clone()))))
    }
}

impl Drop for CancelScope {
    fn drop(&mut self) {
        let prev = self.0.take();
        let _ = CANCEL.try_with(|c| *c.borrow_mut() = prev);
    }
}

/// Whether the scan this thread measures for has been cancelled (`false` outside a scan).
pub(crate) fn cancelled() -> bool {
    CANCEL
        .try_with(|c| c.borrow().as_ref().is_some_and(CancelToken::is_cancelled))
        .unwrap_or(false)
}

/// Bytes of a sector start shown to [`Format::recognizes`].
pub(crate) const HEAD_LEN: usize = 16;
/// Sector size used by the detectors: unreadable-sector granularity and "another file starts here" checks.
pub(crate) const SECTOR: u64 = 512;
/// Bytes read per window (a whole number of sectors).
const WINDOW: usize = 1 << 20;
/// Windows kept for reuse per thread (a measurement nests at most a few readers: `measure` and the `confirms` it calls).
const POOL_MAX: usize = 4;
/// Box payloads a [`Reader`] remembers (a measurement loads a `moov` and a `meta` at most).
const LOADED_MAX: usize = 4;

thread_local! {
    static POOL: RefCell<Vec<Vec<u8>>> = const { RefCell::new(Vec::new()) };
    /// Windows allocated fresh (not taken from the pool) on this thread, to test the reuse.
    #[cfg(test)]
    pub(crate) static FRESH_WINDOWS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// A 1 MiB window from this thread's pool, or a fresh one.
fn take_window() -> Vec<u8> {
    POOL.with(|p| p.borrow_mut().pop()).unwrap_or_else(|| {
        #[cfg(test)]
        FRESH_WINDOWS.with(|c| c.set(c.get() + 1));
        vec![0; WINDOW]
    })
}

fn give_back_window(buf: Vec<u8>) {
    // `try_with`: during thread teardown the pool may be gone; the buffer is then simply freed.
    let _ = POOL.try_with(|p| {
        let mut pool = p.borrow_mut();
        if pool.len() < POOL_MAX {
            pool.push(buf);
        }
    });
}

/// Windowed, sector-aligned reader shared by the detectors. Never reads a file whole.
pub(crate) struct Reader<'a> {
    source: &'a dyn Source,
    len: u64,
    buf: Vec<u8>,
    buf_start: u64,
    buf_len: usize,
    /// Starts of the unreadable sectors seen so far, sorted.
    bad: Vec<u64>,
    /// Box payloads already loaded by [`Reader::load_box`], by `(offset, length)`.
    loaded: HashMap<(u64, u64), Rc<Vec<u8>>>,
    /// Bytes read per load (at most [`WINDOW`]).
    window: usize,
}

impl Drop for Reader<'_> {
    fn drop(&mut self) {
        give_back_window(std::mem::take(&mut self.buf));
    }
}

impl<'a> Reader<'a> {
    pub(crate) fn new(source: &'a dyn Source) -> Self {
        Self {
            source,
            len: source.len(),
            buf: take_window(),
            buf_start: 0,
            buf_len: 0,
            bad: Vec::new(),
            loaded: HashMap::new(),
            window: WINDOW,
        }
    }

    /// A reader that reads at most `window` bytes (rounded to whole sectors, at least one) per load, for walks that
    /// often stop early (the JPEG gap search's tries).
    pub(crate) fn with_window(source: &'a dyn Source, window: usize) -> Self {
        let mut r = Self::new(source);
        r.window = (window / SECTOR as usize).clamp(1, WINDOW / SECTOR as usize) * SECTOR as usize;
        r
    }

    /// The bytes `[at, at + len)` as one buffer, read at most once per reader (the same range asked again is shared).
    /// Short only at the end of the source.
    pub(crate) fn load_box(&mut self, at: u64, len: u64) -> Rc<Vec<u8>> {
        if let Some(data) = self.loaded.get(&(at, len)) {
            return Rc::clone(data);
        }
        let mut data = vec![0u8; len as usize];
        // One read straight from the source (not through the window, which a walk keeps moving); on an error the window
        // path re-reads it sector by sector and remembers the unreadable ones.
        if self.source.read_at(at, &mut data).is_err() {
            data.fill(0);
            self.read(at, &mut data);
        }
        let data = Rc::new(data);
        if self.loaded.len() < LOADED_MAX {
            self.loaded.insert((at, len), Rc::clone(&data));
        }
        data
    }

    /// The first `0xFF` byte in `[from, end)`, or `None` if there is none before `end` or the source ends first.
    pub(crate) fn find_ff(&mut self, from: u64, end: u64) -> Option<u64> {
        let mut pos = from;
        while pos < end && self.load(pos) {
            let at = (pos - self.buf_start) as usize;
            let stop = self
                .buf_len
                .min((end - self.buf_start).min(usize::MAX as u64) as usize);
            if let Some(i) = memchr::memchr(0xFF, &self.buf[at..stop]) {
                return Some(pos + i as u64);
            }
            pos = self.buf_start + stop as u64;
        }
        None
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
        let want = (self.len - start).min(self.window as u64) as usize;
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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::error::EngineError;
    use crate::source::VolumeId;

    pub(crate) struct Mem(pub Vec<u8>);

    impl Source for Mem {
        fn len(&self) -> u64 {
            self.0.len() as u64
        }
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, EngineError> {
            let start = (offset as usize).min(self.0.len());
            let n = buf.len().min(self.0.len() - start);
            buf[..n].copy_from_slice(&self.0[start..start + n]);
            Ok(n)
        }
        fn volume(&self) -> Option<VolumeId> {
            None
        }
    }

    #[test]
    fn reader_reuses_its_window() {
        let src = Mem(vec![7u8; 4096]);
        let before = FRESH_WINDOWS.with(|c| c.get());
        for _ in 0..1000 {
            let mut r = Reader::new(&src);
            assert_eq!(r.byte(100), Some(7));
            let mut out = [0u8; 8];
            assert_eq!(r.read(0, &mut out), 8);
        }
        // Counter is per thread (unit tests run in parallel threads): at most the first reader allocates.
        assert!(FRESH_WINDOWS.with(|c| c.get()) - before <= 1);
        // Nested readers (as `confirms` inside `measure`) get their own windows and all go back, up to the cap.
        {
            let _a = Reader::new(&src);
            let _b = Reader::new(&src);
        }
        let mid = FRESH_WINDOWS.with(|c| c.get());
        for _ in 0..100 {
            let _a = Reader::new(&src);
            let _b = Reader::new(&src);
        }
        assert_eq!(FRESH_WINDOWS.with(|c| c.get()), mid);
    }

    #[test]
    fn builtin_formats_are_built_once() {
        assert!(std::ptr::eq(builtin(), builtin()));
        assert_eq!(builtin().len(), all_formats().len());
    }

    #[test]
    fn find_ff_crosses_windows_and_respects_bounds() {
        let mut data = vec![0u8; 3 * WINDOW];
        data[WINDOW + 5] = 0xFF;
        data[2 * WINDOW + 900] = 0xFF;
        let src = Mem(data);
        let mut r = Reader::new(&src);
        assert_eq!(r.find_ff(0, u64::MAX), Some(WINDOW as u64 + 5));
        assert_eq!(
            r.find_ff(WINDOW as u64 + 5, u64::MAX),
            Some(WINDOW as u64 + 5)
        );
        assert_eq!(
            r.find_ff(WINDOW as u64 + 6, u64::MAX),
            Some(2 * WINDOW as u64 + 900)
        );
        // `end` is exclusive.
        assert_eq!(r.find_ff(0, WINDOW as u64 + 5), None);
        assert_eq!(r.find_ff(0, WINDOW as u64 + 6), Some(WINDOW as u64 + 5));
        // Nothing before the end of the source.
        assert_eq!(r.find_ff(2 * WINDOW as u64 + 901, u64::MAX), None);
        assert_eq!(r.find_ff(5 * WINDOW as u64, u64::MAX), None);
    }

    /// Fails every read that touches the sector at `bad`.
    pub(crate) struct BadSector {
        pub(crate) inner: Mem,
        pub(crate) bad: u64,
    }

    impl Source for BadSector {
        fn len(&self) -> u64 {
            self.inner.len()
        }
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, EngineError> {
            if offset < self.bad + SECTOR && offset + buf.len() as u64 > self.bad {
                return Err(EngineError::Read {
                    offset,
                    cause: std::io::Error::other("injected"),
                });
            }
            self.inner.read_at(offset, buf)
        }
        fn volume(&self) -> Option<VolumeId> {
            None
        }
    }

    #[test]
    fn load_box_falls_back_to_sectors_on_a_read_error() {
        let src = BadSector {
            inner: Mem(vec![9u8; 8192]),
            bad: 1536,
        };
        let mut r = Reader::new(&src);
        let data = r.load_box(1024, 2048);
        assert_eq!(data.len(), 2048);
        assert!(data[..512].iter().all(|&b| b == 9));
        assert!(
            data[512..1024].iter().all(|&b| b == 0),
            "the bad sector reads as zeros"
        );
        assert!(data[1024..].iter().all(|&b| b == 9));
        assert!(r.bad_in(1024, 3072));
        assert!(!r.bad_in(2048, 3072));
        // Asked again, it is the same shared buffer.
        assert!(Rc::ptr_eq(&data, &r.load_box(1024, 2048)));
    }
}
