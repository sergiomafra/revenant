//! The scan: walks a source sector by sector, lets each format recognize and measure files, and reports events.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::EngineError;
use crate::formats::{FileKind, Format, Fragment, Health};
use crate::source::Source;

/// Bytes of a sector start shown to `Format::recognizes`.
const HEAD_LEN: usize = 16;

/// A file found by the scan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    /// Sequential from 1, in discovery order.
    pub id: u64,
    /// Where the file starts (the first piece's start when it is in pieces).
    pub start: u64,
    /// Total length of the file (the sum of its pieces when it has any).
    pub len: u64,
    pub kind: FileKind,
    pub health: Health,
    /// Empty for a contiguous file. Otherwise at least 2 pieces in file order: the first starts at `start`, the lengths
    /// sum to `len`, no two overlap.
    pub fragments: Vec<Fragment>,
}

impl Finding {
    /// The file's byte ranges in file order: `[Fragment { start, len }]` for a contiguous file, else `fragments`.
    pub fn ranges(&self) -> Vec<Fragment> {
        ranges(self.start, self.len, &self.fragments)
    }
}

/// `[Fragment { start, len }]` when `fragments` is empty, else `fragments`.
pub(crate) fn ranges(start: u64, len: u64, fragments: &[Fragment]) -> Vec<Fragment> {
    if fragments.is_empty() {
        vec![Fragment { start, len }]
    } else {
        fragments.to_vec()
    }
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
/// The first format that recognizes a sector start and measures a file there wins. The scan reads every block. After an
/// `Intact` finding it examines no head until the first sector at or after the file's end; after a `Partial` or `Damaged`
/// one it keeps examining heads, so files inside it are still found (one broken file must not hide others). A finding is
/// emitted once the scan has read past its end, so that unreadable sectors inside it are known: they are reported as
/// `Unreadable` events (before the `Found`), and an `Intact` finding containing any of them is downgraded to `Partial`.
/// Findings are emitted in the order they were found (by start), so one found inside another is emitted right after it.
/// `measure` reads the source directly, so files may cross block boundaries.
///
/// `opts.sector` is raised to at least 16 (the head length) and `opts.block` rounded down to whole sectors, so a head
/// never straddles two blocks. On cancellation the scan returns `Err(Cancelled)` and findings still pending are dropped.
pub fn scan(
    source: &dyn Source,
    formats: &[Box<dyn Format>],
    opts: &ScanOptions,
    cancel: &CancelToken,
    on_event: &mut dyn FnMut(Event),
) -> Result<ScanSummary, EngineError> {
    let sector = opts.sector.max(HEAD_LEN as u64);
    let block = ((opts.block as u64 / sector).max(1) * sector) as usize;
    let total = source.len();
    let mut summary = ScanSummary::default();
    let mut run: Option<(u64, u64)> = None; // open unreadable run (start, len)
    // Found but not yet emitted, in start order, each with its end rounded up to a whole sector.
    let mut pending: VecDeque<(Finding, u64)> = VecDeque::new();
    let mut skip_until = 0u64; // no heads are examined before this offset
    let mut next_id = 1;
    let mut pos = 0u64;

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
            while let Some((_, end)) = pending.front()
                && at >= *end
            {
                // Everything inside the finding has been read: report its unreadable run first.
                flush(&mut run, &mut summary, on_event);
                let (f, _) = pending.pop_front().expect("front exists");
                summary.found += 1;
                on_event(Event::Found(f));
            }
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
                for (f, end) in &mut pending {
                    if at < *end && f.health == Health::Intact {
                        f.health = Health::Partial;
                    }
                }
                at = sector_end;
                continue;
            }
            flush(&mut run, &mut summary, on_event);
            if at < skip_until {
                at = sector_end;
                continue;
            }

            let rel = (at - pos) as usize;
            let head_len = HEAD_LEN.min((total - at) as usize);
            let found = data.get(rel..rel + head_len).and_then(|head| {
                formats
                    .iter()
                    .filter(|f| f.recognizes(head))
                    .find_map(|f| f.measure(source, at))
            });
            match found {
                Some(m) if m.len > 0 => {
                    let end = at.saturating_add(m.len).div_ceil(sector) * sector;
                    if m.health == Health::Intact {
                        skip_until = end;
                    }
                    let finding = Finding {
                        id: next_id,
                        start: at,
                        len: m.len,
                        kind: m.kind,
                        health: m.health,
                        fragments: m.fragments,
                    };
                    pending.push_back((finding, end));
                    next_id += 1;
                }
                _ => {}
            }
            at = sector_end;
        }
        pos = block_end;
        on_event(Event::Progress { read: pos, total });
        if cancel.is_cancelled() {
            return Err(EngineError::Cancelled);
        }
    }
    flush(&mut run, &mut summary, on_event);
    for (f, _) in pending {
        summary.found += 1;
        on_event(Event::Found(f));
    }
    if total == 0 {
        on_event(Event::Progress { read: 0, total });
    }
    Ok(summary)
}
