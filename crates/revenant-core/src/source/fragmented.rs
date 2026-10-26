//! A read-only logical view of pieces of a source, so a file found in pieces reads as one contiguous file.

use super::{DiskId, Source, VolumeId};
use crate::EngineError;
use crate::formats::Fragment;

/// Read-only logical view of pieces of a source, in the given order: offset 0 is the first byte of the first piece.
/// Pieces are clamped to the inner source's end.
pub struct Fragmented<'a> {
    source: &'a dyn Source,
    pieces: Vec<Fragment>,
    /// Logical offset where each piece starts (same length as `pieces`, non-decreasing).
    starts: Vec<u64>,
    len: u64,
}

impl<'a> Fragmented<'a> {
    /// Each piece is clamped to the inner source (a piece starting at or past its end becomes empty), so the view reads
    /// exactly the bytes [`crate::extract()`] copies for the same pieces.
    pub fn new(source: &'a dyn Source, pieces: Vec<Fragment>) -> Self {
        let inner_len = source.len();
        let pieces: Vec<Fragment> = pieces
            .into_iter()
            .map(|p| Fragment {
                start: p.start,
                len: p.len.min(inner_len.saturating_sub(p.start)),
            })
            .collect();
        let mut starts = Vec::with_capacity(pieces.len());
        let mut len = 0u64;
        for p in &pieces {
            starts.push(len);
            len = len.saturating_add(p.len);
        }
        Self {
            source,
            pieces,
            starts,
            len,
        }
    }

    /// Index of the piece holding logical `offset` (`offset` must be below `len`).
    fn piece_at(&self, offset: u64) -> usize {
        // The last piece starting at or before `offset`; a piece that starts there is never empty while `offset < len`.
        self.starts.partition_point(|&s| s <= offset) - 1
    }

    /// Absolute pieces of the logical range [start, start + len) (clamped to the view's end), adjacent pieces merged.
    pub fn absolute(&self, start: u64, len: u64) -> Vec<Fragment> {
        let end = start.saturating_add(len).min(self.len);
        let mut out: Vec<Fragment> = Vec::new();
        if start >= end {
            return out;
        }
        let mut pos = start;
        let mut i = self.piece_at(start);
        while pos < end {
            let p = self.pieces[i];
            let within = pos - self.starts[i];
            let n = (p.len - within).min(end - pos);
            let abs = p.start.saturating_add(within);
            match out.last_mut() {
                Some(last) if last.start.saturating_add(last.len) == abs => last.len += n,
                _ if n > 0 => out.push(Fragment { start: abs, len: n }),
                _ => {}
            }
            pos += n;
            i += 1;
        }
        out
    }
}

impl Source for Fragmented<'_> {
    /// Sum of the pieces' lengths.
    fn len(&self) -> u64 {
        self.len
    }

    /// Reads across pieces in order (fewer bytes only at the view's end, or if the inner source shrinks). A read error is
    /// the inner source's, so its offset is absolute in the inner source.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, EngineError> {
        if offset >= self.len {
            return Ok(0);
        }
        let want = (self.len - offset).min(buf.len() as u64) as usize;
        let mut done = 0;
        let mut i = self.piece_at(offset);
        while done < want {
            let pos = offset + done as u64;
            let p = self.pieces[i];
            let within = pos - self.starts[i];
            let n = (p.len - within).min((want - done) as u64) as usize;
            let Some(abs) = p.start.checked_add(within) else {
                break;
            };
            let got = self.source.read_at(abs, &mut buf[done..done + n])?;
            done += got;
            if got < n {
                break;
            }
            i += 1;
        }
        Ok(done)
    }

    fn volume(&self) -> Option<VolumeId> {
        self.source.volume()
    }

    fn disk(&self) -> Option<DiskId> {
        self.source.disk()
    }
}
