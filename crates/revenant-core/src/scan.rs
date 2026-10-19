//! The scan: walks a source sector by sector, lets each format recognize and measure files, and reports events.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::EngineError;
use crate::formats::{FileKind, Format, Health};
use crate::source::Source;

/// Bytes of a sector start shown to `Format::recognizes`.
const HEAD_LEN: usize = 16;

/// A file found by the scan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    /// Sequential from 1, in discovery order.
    pub id: u64,
    pub start: u64,
    pub len: u64,
    pub kind: FileKind,
    pub health: Health,
}

pub struct ScanOptions {
    /// Granularity of candidate starts and of unreadable-region handling, in bytes.
    pub sector: u64,
    /// Bytes read per block (rounded down to a whole number of sectors, at least one).
    pub block: usize,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            sector: 512,
            block: 4 * 1024 * 1024,
        }
    }
}

/// Cooperative cancellation flag shared between clones.
#[derive(Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Found(Finding),
    /// `read` bytes of `total` have been examined.
    Progress {
        read: u64,
        total: u64,
    },
    /// A run of consecutive sectors that could not be read.
    Unreadable {
        start: u64,
        len: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct ScanSummary {
    pub found: u64,
    pub unreadable_bytes: u64,
}

/// Reads one block; on error re-reads sector by sector. Returns the data and, if any sector failed, a per-sector mask.
fn read_block(
    source: &dyn Source,
    offset: u64,
    want: usize,
    sector: usize,
) -> (Vec<u8>, Option<Vec<bool>>) {
    let mut buf = vec![0u8; want];
    match source.read_at(offset, &mut buf) {
        Ok(n) => {
            buf.truncate(n);
            (buf, None)
        }
        Err(_) => {
            let mut bad = vec![false; want.div_ceil(sector)];
            let mut filled = 0;
            for (i, chunk) in buf.chunks_mut(sector).enumerate() {
                match source.read_at(offset + (i * sector) as u64, chunk) {
                    Ok(n) => filled = i * sector + n,
                    Err(_) => bad[i] = true,
                }
            }
            // Bad sectors stay addressable as zeros; they are masked, never inspected.
            buf.truncate(filled);
            (buf, Some(bad))
        }
    }
}

/// Scans `source`, calling `on_event` for each finding, progress step and unreadable region.
///
/// The first format that recognizes a sector start and measures a file there wins; the scan resumes at the first sector
/// at or after the end of that file. `measure` reads the source directly, so files may cross block boundaries.
pub fn scan(
    source: &dyn Source,
    formats: &[Box<dyn Format>],
    opts: &ScanOptions,
    cancel: &CancelToken,
    on_event: &mut dyn FnMut(Event),
) -> Result<ScanSummary, EngineError> {
    let sector = opts.sector.max(1);
    let block = ((opts.block as u64 / sector).max(1) * sector) as usize;
    let total = source.len();
    let mut summary = ScanSummary::default();
    let mut run: Option<(u64, u64)> = None; // open unreadable run (start, len)
    let mut next_id = 1;
    let mut pos = 0u64;
    let mut last_progress = None;

    let flush = |run: &mut Option<(u64, u64)>,
                 summary: &mut ScanSummary,
                 on_event: &mut dyn FnMut(Event)| {
        if let Some((start, len)) = run.take() {
            summary.unreadable_bytes += len;
            on_event(Event::Unreadable { start, len });
        }
    };

    while pos < total {
        if cancel.is_cancelled() {
            return Err(EngineError::Cancelled);
        }
        let want = (total - pos).min(block as u64) as usize;
        let (data, bad) = read_block(source, pos, want, sector as usize);
        let block_end = pos + want as u64;
        let mut at = pos;
        while at < block_end {
            let idx = ((at - pos) / sector) as usize;
            let sector_end = (at + sector).min(block_end);
            if bad.as_ref().is_some_and(|b| b[idx]) {
                match &mut run {
                    Some((s, l)) if *s + *l == at => *l += sector_end - at,
                    _ => {
                        flush(&mut run, &mut summary, on_event);
                        run = Some((at, sector_end - at));
                    }
                }
                at = sector_end;
                continue;
            }
            flush(&mut run, &mut summary, on_event);

            let rel = (at - pos) as usize;
            let head_len = HEAD_LEN.min((total - at) as usize);
            let mut far = [0u8; HEAD_LEN];
            let head: Option<&[u8]> = if rel + head_len <= data.len() {
                Some(&data[rel..rel + head_len])
            } else {
                // Near the end of the block: read the head from the source instead of cutting it.
                source
                    .read_at(at, &mut far[..head_len])
                    .ok()
                    .map(|n| &far[..n])
            };
            let found = head.and_then(|head| {
                formats
                    .iter()
                    .filter(|f| f.recognizes(head))
                    .find_map(|f| f.measure(source, at))
            });
            match found {
                Some(m) if m.len > 0 => {
                    on_event(Event::Found(Finding {
                        id: next_id,
                        start: at,
                        len: m.len,
                        kind: m.kind,
                        health: m.health,
                    }));
                    next_id += 1;
                    summary.found += 1;
                    at = (at + m.len).div_ceil(sector) * sector;
                }
                _ => at += sector,
            }
        }
        pos = at.max(block_end);
        let read = pos.min(total);
        on_event(Event::Progress { read, total });
        last_progress = Some(read);
        if cancel.is_cancelled() {
            return Err(EngineError::Cancelled);
        }
    }
    flush(&mut run, &mut summary, on_event);
    if last_progress != Some(total) {
        on_event(Event::Progress { read: total, total });
    }
    Ok(summary)
}
