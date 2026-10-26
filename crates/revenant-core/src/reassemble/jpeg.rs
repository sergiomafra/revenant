//! JPEG seams for the reassembly (D11): a baseline JPEG the 1.3 entropy decoder covers, walked across the pieces of a
//! fragmented photo.
//!
//! The state is the decoder's [`Cursor`] at an MCU-row boundary, with the luma DC values of the rows above it (one
//! buffer per search, truncated when the search goes back to an earlier state; rows shared between states, so a state
//! from another branch rebuilds it). A walk decodes from the state over the view; at each seam it learns the seam's MCU
//! (the first one the decoder cannot complete with the bytes before the seam; `prove` derives it the same way) and the
//! continuation has evidence once the MCU row below the seam's row is decoded. The early DC test (the 1.3 `early_base`,
//! generalised to `base_bound` over the trusted rows above and below the seams) drops only what the final test would.
//! The final proof is the multi-seam DC test (`seams_ok`) over the whole DC grid.

use std::collections::VecDeque;
use std::rc::Rc;

use crate::formats::jpeg::{JPEG_MAX_LEN, data_end, walk_header};
use crate::formats::jpeg_entropy::{self, Cursor, End, Frame, RESUME_POINTS};
use crate::formats::jpeg_join::{
    Dc, base_bound, blocks_per_mcu, boundary_mean, first_failing, on_seams, seam_mean, seams_ok,
    within,
};
use crate::formats::{Reader, SECTOR, cancelled};
use crate::source::Source;

use super::{Proof, Seams, WalkEnd, Walked};

/// Bytes a walk's readers load at a time while the walk is short (a candidate's foreign data usually fails within a
/// few bytes of the seam, and every byte asked of the source is charged to the file's read budget), and once it is long.
const SHORT_WINDOW: usize = 1 << 10;
const LONG_WINDOW: usize = 256 << 10;
/// Spans longer than this are read through the long readers.
const LONG_FROM: u64 = 16 << 10;
/// Bytes after the last seam the walk first decodes; each further stage reads 16 times more.
const FIRST_STAGE: u64 = 1 << 10;
/// Bytes the decoder may hold in its bit buffer ahead of the MCU it decodes (8 bytes, each possibly a stuffed `FF 00`).
const LOOKAHEAD: u64 = 16;

/// One complete MCU row of a path: its luma DC values, and the row above it.
struct Row {
    parent: Option<Rc<Row>>,
    dc: Box<[i32]>,
}

impl Drop for Row {
    /// Drops the rows above one by one (a tall photo's chain would otherwise drop recursively).
    fn drop(&mut self) {
        let mut next = self.parent.take();
        while let Some(rc) = next {
            match Rc::try_unwrap(rc) {
                Ok(mut row) => next = row.parent.take(),
                Err(_) => break,
            }
        }
    }
}

/// The decoder's state at an MCU-row boundary of one path through the pieces.
#[derive(Clone)]
pub(crate) struct JpegState {
    cur: Cursor,
    /// The last complete MCU row (with the rows above it); `None` before the first.
    row: Option<Rc<Row>>,
    /// The seams before `cur`: (logical position, MCU), the MCU being the first one decoded with bytes of the next piece.
    seams: Vec<(u64, u64)>,
}

/// A JPEG's content as the reassembly walks it.
pub(crate) struct JpegSeams {
    frame: Frame,
    /// Logical offset where the entropy-coded data starts.
    scan_start: u64,
    /// The luma DC values of the current path, in coding order (whole rows, then possibly part of one).
    dc: Vec<i32>,
    /// The complete rows of `dc`; `rows[k]` is row `k` of the same path (`rows[k].parent` is `rows[k - 1]`).
    rows: Vec<Rc<Row>>,
    /// Mean `|ΔDC|` of each block-row boundary `b ≥ 1` inside the complete rows (`means[b - 1]`).
    means: Vec<f64>,
    /// The re-check's hint for the last solution asked about: its length, and each MCU's (cut position, score).
    hint: Option<(u64, Vec<(u64, f64)>)>,
    /// The last phase from a state to a seam (see [`Lead`]).
    lead: Option<Lead>,
}

/// A whole view decoded: the decoder's position before each MCU decoded, the luma DC values, how it ended.
struct Decoded {
    pos: Vec<u64>,
    dc: Vec<i32>,
    end: End,
}

/// A seam a walk crossed: its logical position, its MCU, whether the MCU row below its row is decoded (evidence), and
/// whether the early test has run on it once its rows down to that one were trusted.
struct Crossed {
    pos: u64,
    mcu: u64,
    evident: bool,
    tested: bool,
}

/// Why a walk stopped before the decoder did.
#[derive(Clone, Copy)]
enum Halt {
    Cancelled,
    /// A sector of one repeated byte value, or an unreadable one, at this logical position.
    Erased(u64),
    /// The early DC test rejected the seam at this logical position.
    Seam(u64),
}

impl JpegSeams {
    /// `None` when the JPEG at `start` has no frame the 1.3 decoder covers.
    pub(crate) fn new(source: &dyn Source, start: u64) -> Option<JpegSeams> {
        let mut r = Reader::new(source);
        let header = walk_header(&mut r, start, JPEG_MAX_LEN)?;
        let frame = jpeg_entropy::frame(&mut r, start, header.scan_start)?;
        Some(JpegSeams {
            frame,
            scan_start: header.scan_start - start,
            dc: Vec::new(),
            rows: Vec::new(),
            means: Vec::new(),
            hint: None,
            lead: None,
        })
    }

    /// MCUs between restart markers; 0 without them (Q16 is the health rule's: a photo with them reassembled from
    /// several pieces is at most `Partial`).
    pub(crate) fn restart_interval(&self) -> u16 {
        self.frame.restart_interval
    }

    fn row_mcus(&self) -> u64 {
        u64::from(self.frame.mcus_x).max(1)
    }

    fn rows_v(&self) -> u64 {
        u64::from(self.frame.luma_blocks_per_mcu().1)
    }

    /// Makes `dc`, `rows` and `means` hold exactly the rows above `from` of its path: truncated when they are the same
    /// path, else rebuilt from the state's rows.
    fn align(&mut self, from: &JpegState) {
        let k = (from.cur.mcu / self.row_mcus()) as usize;
        let same = k <= self.rows.len()
            && match (&from.row, k) {
                (None, 0) => true,
                (Some(r), k) if k > 0 => Rc::ptr_eq(&self.rows[k - 1], r),
                _ => false,
            };
        if !same {
            let mut chain = Vec::with_capacity(k);
            let mut next = from.row.clone();
            while let Some(r) = next {
                next = r.parent.clone();
                chain.push(r);
            }
            chain.reverse();
            self.dc = chain.iter().flat_map(|r| r.dc.iter().copied()).collect();
            self.rows = chain;
            self.means = (1..(self.rows.len() as u64 * self.rows_v()).max(1))
                .map(|by| self.boundary(&[], by))
                .collect();
        }
        let per_row = (self.row_mcus() * blocks_per_mcu(&self.frame)) as usize;
        self.dc.truncate(k * per_row);
        self.rows.truncate(k);
        self.means
            .truncate((k as u64 * self.rows_v()).saturating_sub(1) as usize);
    }

    /// Mean `|ΔDC|` of the block-row boundary `by` over `dc` followed by `tail` (rows decoded whole).
    fn boundary(&self, tail: &[i32], by: u64) -> f64 {
        let dc = Dc {
            head: &self.dc,
            tail,
        };
        boundary_mean(&self.frame, dc, by).unwrap_or(f64::NAN)
    }

    /// At the boundary before MCU row `j` (`j ≥ 1`), reached with `tail` decoded after `dc`: records row `j − 1`.
    fn complete_row(&mut self, tail: &[i32], j: u64) -> Rc<Row> {
        let v = self.rows_v();
        for by in ((j - 1) * v).max(1)..j * v {
            let m = self.boundary(tail, by);
            self.means.push(m);
        }
        let per_row = (self.row_mcus() * blocks_per_mcu(&self.frame)) as usize;
        let (from, to) = ((j as usize - 1) * per_row, j as usize * per_row);
        let dc: Box<[i32]> = (from..to)
            .map(|i| match i.checked_sub(self.dc.len()) {
                None => self.dc[i],
                Some(t) => tail[t],
            })
            .collect();
        let row = Rc::new(Row {
            parent: self.rows.last().cloned(),
            dc,
        });
        self.rows.push(Rc::clone(&row));
        row
    }

    fn blocks_y(&self) -> u64 {
        u64::from(self.frame.mcus_y) * self.rows_v()
    }

    /// The early DC test (D11, Q14) of the path's `seams` (logical position, MCU) with the rows above MCU `trusted`: the
    /// block-row boundaries decoded there, above and below the seams, bound the final base once they are more than
    /// half of them ([`base_bound`]; with only the rows above one seam this is the 1.3 `early_base`). The position of
    /// the first seam (whose rows down to the MCU row below it lie above `trusted`) rougher than 3 × the bound + 4,
    /// which the final test would reject too.
    ///
    /// `trusted` must be a row boundary no continuation of the path cuts before: the walk's boundary before the last
    /// one it reached (a piece is never cut before it: the search's break points start there), so the rows used belong
    /// to the final photo whatever is found after them; rows decoded past it may be foreign data that still decodes.
    fn dc_rejects(&self, tail: &[i32], seams: &[(u64, u64)], trusted: u64) -> Option<u64> {
        if seams.is_empty() {
            return None;
        }
        let (row, v, blocks_y) = (self.row_mcus(), self.rows_v(), self.blocks_y());
        let mcus: Vec<u64> = seams.iter().map(|s| s.1).collect();
        let mut on: Vec<u64> = mcus
            .iter()
            .flat_map(|&m| [m / row * v, (m / row + 1) * v])
            .filter(|&by| by >= 1 && by < blocks_y && on_seams(&self.frame, &mcus, by))
            .collect();
        on.sort_unstable();
        on.dedup();
        // Boundaries between two block rows above `trusted`.
        let last = (trusted / row * v).saturating_sub(1);
        let known: Vec<f64> = self
            .means
            .iter()
            .enumerate()
            .map(|(i, &m)| (i as u64 + 1, m))
            .filter(|&(by, m)| by <= last && !m.is_nan() && on.binary_search(&by).is_err())
            .map(|(_, m)| m)
            .collect();
        let bound = base_bound(&self.frame, &known, on.len() as u64)?;
        let dc = Dc {
            head: &self.dc,
            tail,
        };
        let mut seams = seams.to_vec();
        seams.sort_unstable();
        seams
            .into_iter()
            .filter(|&(_, m)| (m / row + 2) * row <= trusted)
            .find(|&(_, m)| seam_mean(&self.frame, dc, m).is_some_and(|at| !within(at, bound)))
            .map(|(p, _)| p)
    }

    /// The position of the first of the path's `seams` the final DC test rejects over the whole DC grid (`None`: it
    /// accepts them, or there are none, or fewer than 4 block rows make it unverifiable).
    fn failing_seam(&self, seams: &[(u64, u64)]) -> Option<u64> {
        if seams.is_empty() || self.blocks_y() < 4 {
            return None;
        }
        let mut seams = seams.to_vec();
        seams.sort_unstable();
        let mcus: Vec<u64> = seams.iter().map(|s| s.1).collect();
        first_failing(&self.frame, &self.dc, &mcus).map(|i| seams[i].0)
    }

    /// Decodes the whole `view` from the start (cancelled: `None`).
    fn decode_all(&self, view: &dyn Source) -> Option<Decoded> {
        let mut r = Reader::new(view);
        let mut cur = self.frame.start(self.scan_start);
        let bpm = blocks_per_mcu(&self.frame) as usize;
        let total = self.frame.total_mcus();
        let (mut pos, mut dc) = (Vec::new(), Vec::new());
        loop {
            // A segment keeps the state before each of its MCUs (at most RESUME_POINTS); the next one starts from the
            // last of them, decoding that MCU again.
            let stop = cur.mcu + RESUME_POINTS as u64 - 1;
            let run = jpeg_entropy::decode_rows(
                &mut r,
                &self.frame,
                cur.clone(),
                view.len(),
                true,
                true,
                stop,
                &mut |_, _| !cancelled(),
            );
            if cancelled() {
                return None;
            }
            if run.end == End::OutOfData && run.mcus == stop && stop < total {
                let (last, done) = run.resume.split_last()?;
                pos.extend(done.iter().map(|c| c.pos));
                dc.extend_from_slice(&run.dc[..done.len() * bpm]);
                cur = last.clone();
                continue;
            }
            pos.extend(
                run.resume
                    .iter()
                    .filter(|c| c.mcu < run.mcus)
                    .map(|c| c.pos),
            );
            dc.extend_from_slice(&run.dc);
            return Some(Decoded {
                pos,
                dc,
                end: run.end,
            });
        }
    }

    /// Decodes the whole `view` from the start, up to each of `seams` in turn and then to its end, as the walk does: each
    /// seam's MCU is the first one the decoder cannot complete below the seam (the state before it is where the next
    /// phase starts). The seams' MCUs, the luma DC values, how the decoder ended; `None` when cancelled.
    fn decode_at_seams(
        &self,
        view: &dyn Source,
        seams: &[u64],
    ) -> Option<(Vec<u64>, Vec<i32>, End)> {
        let mut r = Reader::new(view);
        let bpm = blocks_per_mcu(&self.frame) as usize;
        let mut cur = self.frame.start(self.scan_start);
        let (mut mcus, mut dc) = (Vec::new(), Vec::new());
        for to in seams.iter().copied().chain(std::iter::once(view.len())) {
            let at_seam = to < view.len();
            let run = jpeg_entropy::decode_rows(
                &mut r,
                &self.frame,
                cur.clone(),
                to,
                true,
                at_seam,
                u64::MAX,
                &mut |_, _| !cancelled(),
            );
            if cancelled() {
                return None;
            }
            dc.extend_from_slice(&run.dc);
            match (run.end, run.resume.last()) {
                (End::OutOfData, Some(last)) if at_seam => {
                    dc.truncate(last.mcu as usize * bpm);
                    mcus.push(last.mcu);
                    cur = last.clone();
                }
                (end, _) => return Some((mcus, dc, end)),
            }
        }
        None
    }

    /// For every MCU of a decoded photo: where a piece would be cut to splice another photo in at it (before the bytes
    /// the decoder may hold ahead of it), and how much the DC grid jumps along the seam it would make there. The jump is
    /// measured on the vertical DC steps: at each block of the seam line, how far the step across it is from the mean
    /// of the steps above and below it (a smooth photo's steps change slowly; a spliced-in look-alike shifts them).
    fn scores(&self, d: &Decoded) -> Vec<(u64, f64)> {
        let (h, v) = self.frame.luma_blocks_per_mcu();
        let (h, v) = (u64::from(h), u64::from(v));
        let mcus_x = self.row_mcus();
        let (bx_n, by_n) = (mcus_x * h, u64::from(self.frame.mcus_y) * v);
        let at = |bx: u64, by: u64| -> f64 {
            let mcu = by / v * mcus_x + bx / h;
            let k = by % v * h + bx % h;
            f64::from(d.dc[(mcu * h * v + k) as usize])
        };
        let step = |bx: u64, by: u64| at(bx, by) - at(bx, by - 1);
        // For each MCU-row top `R × v` (R ≥ 1), the prefix sums of the anomaly along it.
        let mut prefix: Vec<Vec<f64>> = Vec::with_capacity(self.frame.mcus_y as usize);
        for r in 0..u64::from(self.frame.mcus_y) {
            let by = r * v;
            let mut p = Vec::with_capacity(bx_n as usize + 1);
            p.push(0.0);
            if by >= 1 {
                for bx in 0..bx_n {
                    let s = step(bx, by);
                    let around: Vec<f64> = [by - 1, by + 1]
                        .into_iter()
                        .filter(|&b| b >= 1 && b < by_n)
                        .map(|b| step(bx, b))
                        .collect();
                    let expected = if around.is_empty() {
                        0.0
                    } else {
                        around.iter().sum::<f64>() / around.len() as f64
                    };
                    p.push(p[bx as usize] + (s - expected).abs());
                }
            }
            prefix.push(p);
        }
        d.pos
            .iter()
            .enumerate()
            .map(|(m, &pos)| {
                let (r, c) = (m as u64 / mcus_x, m as u64 % mcus_x);
                let cx = (c * h) as usize;
                let (mut sum, mut n) = (0.0, 0usize);
                if r >= 1 {
                    let p = &prefix[r as usize];
                    sum += p[bx_n as usize] - p[cx];
                    n += bx_n as usize - cx;
                }
                if let Some(p) = prefix.get(r as usize + 1).filter(|p| p.len() > 1) {
                    sum += p[cx];
                    n += cx;
                }
                let score = if n > 0 { sum / n as f64 } else { 0.0 };
                (pos.saturating_sub(LOOKAHEAD), score)
            })
            .collect()
    }
    /// The phase from `from` to the walk's first seam `seam`, remembered for the next walk from the same state to the same
    /// seam (the other candidates of that seam).
    fn lead_to_seam(
        &mut self,
        w: &mut Walk,
        from: &JpegState,
        cur: &Cursor,
        seam: u64,
    ) -> Result<Cursor, Option<End>> {
        let (dc0, rows0, means0, resume0) = (
            self.dc.len(),
            self.rows.len(),
            self.means.len(),
            w.resume.len(),
        );
        let c = self.phase(w, cur, seam, true)?;
        if w.resume.len() - resume0 == w.bounds.len() - 1 {
            self.lead = Some(Lead {
                from_row: from.row.clone(),
                from_pos: from.cur.pos,
                from_mcu: from.cur.mcu,
                seam,
                cur: c.clone(),
                dc: self.dc[dc0..].to_vec(),
                rows: self.rows[rows0..].to_vec(),
                means: self.means[means0..].to_vec(),
                resume: w.resume.iter().skip(resume0).cloned().collect(),
                seam_mcu: w.crossed.last().map_or(c.mcu, |s| s.mcu),
            });
        }
        Ok(c)
    }

    /// Replays a remembered phase to the first seam into `w` (the buffer is aligned to its state); the state at the seam.
    fn replay(&mut self, lead: &Lead, w: &mut Walk) -> Cursor {
        self.dc.extend_from_slice(&lead.dc);
        self.rows.extend(lead.rows.iter().cloned());
        self.means.extend_from_slice(&lead.means);
        for r in &lead.resume {
            w.bounds.push((r.0, r.1.cur.mcu));
            w.resume.push_back(r.clone());
        }
        w.crossed.push(Crossed {
            pos: lead.seam,
            mcu: lead.seam_mcu,
            evident: false,
            tested: false,
        });
        w.known.push((lead.seam, lead.seam_mcu));
        lead.cur.clone()
    }

    /// Decodes from `cur` up to `to`. A seam phase (`at_seam`) ending below the seam records the seam and gives the state
    /// at its MCU; any other end (or a halt) is `Err` with how the decoder ended (`None` after a halt).
    fn phase(
        &mut self,
        w: &mut Walk,
        cur: &Cursor,
        to: u64,
        at_seam: bool,
    ) -> Result<Cursor, Option<End>> {
        let row = self.row_mcus();
        let bpm = blocks_per_mcu(&self.frame) as usize;
        let frame = self.frame.clone();
        let view = w.view;
        let reader = if to.saturating_sub(cur.pos) <= LONG_FROM {
            &mut w.short
        } else {
            w.long
                .get_or_insert_with(|| Reader::with_window(view, LONG_WINDOW))
        };
        let (crossed, known, bounds, resume, clean_to, halt, next_test) = (
            &mut w.crossed,
            &w.known,
            &mut w.bounds,
            &mut w.resume,
            &mut w.clean_to,
            &mut w.halt,
            &mut w.next_test,
        );
        let (check_short, check_long) = (&mut w.check_short, &mut w.check_long);
        let run = jpeg_entropy::decode_rows(
            reader,
            &frame,
            cur.clone(),
            to,
            true,
            at_seam,
            u64::MAX,
            &mut |c, tail| {
                if cancelled() {
                    *halt = Some(Halt::Cancelled);
                    return false;
                }
                let erased = check(check_short, check_long, view, *clean_to, c.pos);
                if erased < c.pos {
                    *halt = Some(Halt::Erased(erased));
                    return false;
                }
                *clean_to = c.pos;
                let r = self.complete_row(tail, c.mcu / row);
                // The boundary before this one: no continuation of the path is cut above it.
                let trusted = bounds.last().map_or(0, |b| b.1);
                bounds.push((c.pos, c.mcu));
                if resume.len() == RESUME_POINTS {
                    resume.pop_front();
                }
                let state = JpegState {
                    cur: c.clone(),
                    row: Some(r),
                    seams: known.clone(),
                };
                resume.push_back((c.pos, state));
                // Evidence: the MCU row below the seam's row is decoded. The early test once the rows down to that one
                // are trusted, and again as the decoded rows grow (by a quarter each time): every seam of the path faces
                // the bound from all the trusted rows.
                for s in crossed.iter_mut() {
                    s.evident |= c.mcu >= (s.mcu / row + 2) * row;
                }
                let reached = crossed
                    .iter_mut()
                    .filter(|s| !s.tested && trusted >= (s.mcu / row + 2) * row)
                    .fold(false, |_, s| {
                        s.tested = true;
                        true
                    });
                if reached || self.means.len() >= *next_test {
                    *next_test = self.means.len() + (self.means.len() / 4).max(1);
                    if let Some(p) = self.dc_rejects(tail, known, trusted) {
                        *halt = Some(Halt::Seam(p));
                        return false;
                    }
                }
                true
            },
        );
        self.dc.extend_from_slice(&run.dc);
        if w.halt.is_some() {
            return Err(None);
        }
        match run.end {
            End::OutOfData if at_seam => {
                // The seam's MCU: the state before the first MCU not completed below the seam (before the last MCU
                // when the seam lies in the photo's last bytes); the next phase decodes from it.
                let Some(last) = run.resume.last() else {
                    return Err(Some(End::OutOfData));
                };
                self.dc.truncate(last.mcu as usize * bpm);
                w.crossed.push(Crossed {
                    pos: to,
                    mcu: last.mcu,
                    evident: false,
                    tested: false,
                });
                w.known.push((to, last.mcu));
                Ok(last.clone())
            }
            end => Err(Some(end)),
        }
    }

    /// The walk's result once the decoder ended (`end`, `None` after a halt).
    fn finish(
        &mut self,
        mut w: Walk,
        from: &JpegState,
        limit: u64,
        end: Option<End>,
    ) -> Walked<JpegState> {
        let first_blind = w.crossed.iter().find(|s| !s.evident).map(|s| s.pos);
        let clean_to = w.clean_to;
        let (complete, at) = match (w.halt, end) {
            (Some(Halt::Cancelled), _) => (None, from.cur.pos),
            (Some(Halt::Erased(u)), _) => (None, u),
            (Some(Halt::Seam(s)), _) => (None, s),
            (None, Some(End::Eoi { at })) => {
                let len = at + 2;
                match w.check(clean_to, len) {
                    u if u < len => (None, u),
                    // Complete: the final DC test over the whole grid (`seams_ok`, as `prove`): a path it rejects is no
                    // solution (with fewer than 4 block rows no seam is verifiable, and `prove` says so).
                    _ => match self.failing_seam(&w.known) {
                        Some(p) => (None, p),
                        None => (Some(len), len),
                    },
                }
            }
            (None, Some(End::Error { at })) => {
                let x = (at + 1).min(limit);
                match w.check(clean_to, x) {
                    u if u < x => (None, u),
                    _ => (None, first_blind.unwrap_or(x)),
                }
            }
            (None, _) => (None, w.check(clean_to, limit)),
        };
        // The boundary before the last one reached, at or before the break: where the search's break points start.
        let evidence = &w.bounds[..w.bounds.len().saturating_sub(1).max(1)];
        let checked_at = |at: u64| {
            evidence
                .iter()
                .copied()
                .filter(|b| b.0 <= at)
                .max_by_key(|b| b.0)
        };
        // The early test once more, with the rows above that boundary (those every continuation keeps).
        let at = match (complete, w.halt) {
            (None, None) => {
                let trusted = checked_at(at).map_or(from.cur.mcu, |b| b.1);
                self.dc_rejects(&[], &w.known, trusted)
                    .map_or(at, |p| p.min(at))
            }
            _ => at,
        };
        let resume: Vec<(u64, JpegState)> = std::mem::take(&mut w.resume).into();
        if matches!(w.halt, Some(Halt::Cancelled)) {
            return Walked {
                end: WalkEnd::Stopped,
                checked: from.cur.pos,
                resume,
            };
        }
        if let Some(len) = complete {
            return Walked {
                end: WalkEnd::Complete { len },
                checked: len,
                resume,
            };
        }
        let checked = checked_at(at).map_or(from.cur.pos.min(at), |b| b.0);
        Walked {
            end: WalkEnd::Broken { at },
            checked,
            resume,
        }
    }
}

/// One walk's readers and what it has seen.
struct Walk<'v> {
    view: &'v dyn Source,
    /// The decoder's reader for short spans, and for long ones (made when first needed).
    short: Reader<'v>,
    long: Option<Reader<'v>>,
    /// The same for the erased-sector check.
    check_short: Reader<'v>,
    check_long: Option<Reader<'v>>,
    /// The seams of the path so far: (logical position, MCU).
    known: Vec<(u64, u64)>,
    /// The count of known block-row boundaries at which the early test runs next.
    next_test: usize,
    /// The seams this walk crossed.
    crossed: Vec<Crossed>,
    /// The row boundaries reached (position, MCU), the walk's start first.
    bounds: Vec<(u64, u64)>,
    resume: VecDeque<(u64, JpegState)>,
    /// Bytes before this position are known not to be erased.
    clean_to: u64,
    halt: Option<Halt>,
}

impl Walk<'_> {
    /// [`data_end`] of `[from, to)`.
    fn check(&mut self, from: u64, to: u64) -> u64 {
        check(
            &mut self.check_short,
            &mut self.check_long,
            self.view,
            from,
            to,
        )
    }
}

/// [`data_end`] of `[from, to)` through the reader for the span's length.
fn check<'v>(
    short: &mut Reader<'v>,
    long: &mut Option<Reader<'v>>,
    view: &'v dyn Source,
    from: u64,
    to: u64,
) -> u64 {
    if to.saturating_sub(from) <= LONG_FROM {
        data_end(short, from, to)
    } else {
        data_end(
            long.get_or_insert_with(|| Reader::with_window(view, LONG_WINDOW)),
            from,
            to,
        )
    }
}

/// The phase from a state to the first seam of a walk, remembered: the next walk from the same state to the same seam
/// (another candidate of that seam) replays it instead of decoding (and reading) it again.
struct Lead {
    /// The state's identity: its last row (kept alive, so never confused with a later one), position and MCU.
    from_row: Option<Rc<Row>>,
    from_pos: u64,
    from_mcu: u64,
    seam: u64,
    seam_mcu: u64,
    /// The state at the seam's MCU, and what the phase recorded.
    cur: Cursor,
    dc: Vec<i32>,
    rows: Vec<Rc<Row>>,
    means: Vec<f64>,
    resume: Vec<(u64, JpegState)>,
}

impl Lead {
    fn is_for(&self, from: &JpegState, seam: u64) -> bool {
        let same_row = match (&self.from_row, &from.row) {
            (None, None) => true,
            (Some(a), Some(b)) => Rc::ptr_eq(a, b),
            _ => false,
        };
        same_row
            && self.from_pos == from.cur.pos
            && self.from_mcu == from.cur.mcu
            && self.seam == seam
    }
}

/// Whether a candidate's first sector (absolute `g`) may be entropy-coded data, by the 1.3 window rule: no marker but
/// `00` (stuffing), `D0`–`D7` (restart), `D9` (EOI) or `FF` (fill) after an `FF`, not one repeated byte value, readable.
fn may_be_data(r: &mut Reader, g: u64) -> bool {
    let mut sector = [0u8; SECTOR as usize];
    let n = r.read(g, &mut sector);
    if n < sector.len() || r.bad_in(g, g + SECTOR) || sector[1..] == sector[..sector.len() - 1] {
        return false;
    }
    let mut p = 0;
    while let Some(k) = memchr::memchr(0xFF, &sector[p..]) {
        let f = p + k;
        let following = match sector.get(f + 1) {
            Some(&b) => Some(b),
            None => r.byte(g + SECTOR),
        };
        match following {
            None => return true,
            Some(0x00 | 0xD0..=0xD7 | 0xD9 | 0xFF) => p = f + 1,
            Some(_) => return false,
        }
    }
    true
}

impl Seams for JpegSeams {
    type State = JpegState;

    fn begin(&mut self) -> JpegState {
        JpegState {
            cur: self.frame.start(self.scan_start),
            row: None,
            seams: Vec::new(),
        }
    }

    /// Decodes from `from` over `view`, phase by phase: up to each seam after `from` (where the seam's MCU is the first
    /// one the decoder could not complete below it), then to `limit`, in stages (1 KiB after the last seam, then 16
    /// times more each time: a candidate's foreign data usually fails within bytes, and every byte read is charged). At
    /// each MCU-row boundary: the bytes since the last one are checked for an erased or unreadable sector (`data_end`:
    /// zeros decode as valid blocks), the row is recorded (a resume point), and a seam whose MCU row below is now decoded
    /// faces the early DC test. The phase from a state to the first seam is the same for every candidate of that seam:
    /// it is decoded once and replayed.
    ///
    /// `checked` is the boundary before the last one reached (a row of foreign data can still decode), so a seam has
    /// evidence once the MCU row below its own is decoded. A decode error, or a rejected seam, before the evidence of a
    /// seam breaks the walk right at that seam; the end of the view or an erased sector breaks it where it is (no
    /// evidence either way: a blind candidate, Q2).
    fn walk(
        &mut self,
        view: &dyn Source,
        seams: &[u64],
        from: &JpegState,
        limit: u64,
    ) -> Walked<JpegState> {
        let limit = limit.min(view.len());
        self.align(from);
        let new: Vec<u64> = seams
            .iter()
            .copied()
            .filter(|&s| s >= from.cur.pos && s < limit)
            .collect();
        let mut w = Walk {
            view,
            short: Reader::with_window(view, SHORT_WINDOW),
            long: None,
            check_short: Reader::with_window(view, SHORT_WINDOW),
            check_long: None,
            known: from.seams.clone(),
            // The bound needs more than half the boundaries: no test before about half of them is known.
            next_test: (self.blocks_y() as usize / 2)
                .saturating_sub(2 * (from.seams.len() + seams.len()) + 2),
            crossed: Vec::new(),
            bounds: vec![(from.cur.pos, from.cur.mcu)],
            resume: VecDeque::new(),
            // The bytes before the first seam were checked by the walk `from` comes from.
            clean_to: new.first().copied().unwrap_or(from.cur.pos),
            halt: None,
        };
        let mut cur = from.cur.clone();
        let mut next = 0;
        if let Some(&first) = new.first() {
            match self.lead.take().filter(|l| l.is_for(from, first)) {
                Some(lead) => {
                    cur = self.replay(&lead, &mut w);
                    self.lead = Some(lead);
                    next = 1;
                }
                None => match self.lead_to_seam(&mut w, from, &cur, first) {
                    Ok(c) => {
                        cur = c;
                        next = 1;
                    }
                    Err(end) => return self.finish(w, from, limit, end),
                },
            }
        }
        for &s in &new[next..] {
            match self.phase(&mut w, &cur, s, true) {
                Ok(c) => cur = c,
                Err(end) => return self.finish(w, from, limit, end),
            }
        }
        // After the last seam, in stages.
        let base = cur.pos.max(new.last().copied().unwrap_or(0));
        let mut stage = FIRST_STAGE;
        loop {
            let to = limit.min(base.saturating_add(stage));
            match self.phase(&mut w, &cur, to, false) {
                Err(Some(End::OutOfData)) if to < limit && w.halt.is_none() => {
                    // More bytes: on from the last row boundary this stage reached (its rows are recorded), else again
                    // from where it started.
                    if let Some((_, st)) = w.resume.back().filter(|r| r.1.cur.mcu > cur.mcu) {
                        cur = st.cur.clone();
                    }
                    let bpm = blocks_per_mcu(&self.frame) as usize;
                    self.dc.truncate(cur.mcu as usize * bpm);
                    stage = stage.saturating_mul(16);
                }
                Err(end) => return self.finish(w, from, limit, end),
                Ok(_) => unreachable!("a phase without a seam never stops at one"),
            }
        }
    }

    /// `Proven` iff the whole view decodes (every MCU, EOI right at its end, no erased or unreadable sector) and the DC
    /// grid passes the multi-seam test (`seams_ok`) at the seams' MCUs; `Unproven` with seams in a photo of fewer than 4
    /// block rows (not verifiable) or when cancelled; else `Failed`.
    fn prove(&mut self, view: &dyn Source, seams: &[u64]) -> Proof {
        let Some((mcus, dc, end)) = self.decode_at_seams(view, seams) else {
            return Proof::Unproven;
        };
        let len = view.len();
        if end
            != (End::Eoi {
                at: len.wrapping_sub(2),
            })
        {
            return Proof::Failed;
        }
        if data_end(&mut Reader::new(view), self.scan_start, len) < len {
            return Proof::Failed;
        }
        if seams.is_empty() {
            return Proof::Proven;
        }
        if self.blocks_y() < 4 {
            return Proof::Unproven;
        }
        if seams_ok(&self.frame, &dc, &mcus) {
            Proof::Proven
        } else {
            Proof::Failed
        }
    }

    /// The 1.3 window rule on the candidate's first sector.
    fn prefilter(&mut self, r: &mut Reader, g: u64, seam: u64) -> bool {
        let _ = seam;
        may_be_data(r, g)
    }

    /// The cut positions inside `(start, end)` by how much the DC grid jumps at the MCU there (see [`JpegSeams::scores`]),
    /// largest first. The whole solution is decoded once (the re-check asks for its pieces in a row).
    fn suspicious_breaks(&mut self, view: &dyn Source, start: u64, end: u64) -> Vec<u64> {
        let len = view.len();
        if self.hint.as_ref().is_none_or(|h| h.0 != len) {
            let scores = match self.decode_all(view) {
                Some(d) if d.pos.len() as u64 == self.frame.total_mcus() => self.scores(&d),
                _ => Vec::new(),
            };
            self.hint = Some((len, scores));
        }
        let Some((_, scores)) = &self.hint else {
            return Vec::new();
        };
        let mut inside: Vec<(u64, f64)> = scores
            .iter()
            .copied()
            .filter(|&(p, _)| start < p && p < end)
            .collect();
        inside.sort_by(|a, b| b.1.total_cmp(&a.1).then(b.0.cmp(&a.0)));
        inside.into_iter().map(|(p, _)| p).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Budget, Grid, Limits, Search, Space, Taken, search};
    use super::*;
    use crate::cache::Runs;
    use crate::formats::Fragment;
    use crate::formats::jpeg_entropy::{End, RESUME_POINTS};
    use crate::formats::tests::Mem;
    use crate::scan::CancelToken;
    use revenant_testkit as testkit;

    const HEAP: u64 = 1 << 20;
    const MIB: u64 = 1 << 20;

    /// `len` bytes of seeded noise.
    fn noise(len: u64, seed: u64) -> Vec<u8> {
        let mut x = seed;
        (0..len)
            .map(|_| {
                x = x
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (x >> 56) as u8
            })
            .collect()
    }

    /// A card of `clusters` clusters of seeded noise after a 1 MiB heap start.
    fn noise_card(clusters: u64, cluster: u64) -> Vec<u8> {
        noise(HEAP + clusters * cluster, 0x5EED)
    }

    fn at(cluster: u64, k: u64) -> u64 {
        HEAP + k * cluster
    }

    fn put(card: &mut [u8], at: u64, bytes: &[u8]) -> Fragment {
        card[at as usize..at as usize + bytes.len()].copy_from_slice(bytes);
        Fragment {
            start: at,
            len: bytes.len() as u64,
        }
    }

    /// `jpeg` cut at multiples of `cluster` into `order.len()` runs (the same whole number of clusters each, the last one
    /// the rest) placed at the clusters `order` of `card`; the runs in file order.
    fn put_runs(card: &mut [u8], jpeg: &[u8], cluster: u64, order: &[u64]) -> Vec<Fragment> {
        let per = (jpeg.len() as u64).div_ceil(cluster) / order.len() as u64;
        assert!(per >= 1, "the photo is too small for {} runs", order.len());
        order
            .iter()
            .enumerate()
            .map(|(k, &c)| {
                let from = k as u64 * per * cluster;
                let to = if k + 1 == order.len() {
                    jpeg.len() as u64
                } else {
                    from + per * cluster
                };
                put(card, at(cluster, c), &jpeg[from as usize..to as usize])
            })
            .collect()
    }

    /// `jpeg` scattered over a card of seeded noise (see [`put_runs`]).
    fn scatter(jpeg: &[u8], cluster: u64, order: &[u64]) -> (Vec<u8>, Vec<Fragment>) {
        let clusters = order.iter().max().unwrap() + (jpeg.len() as u64).div_ceil(cluster) + 8;
        let mut card = noise_card(clusters, cluster);
        let runs = put_runs(&mut card, jpeg, cluster, order);
        (card, runs)
    }

    fn grid(card: &[u8], cluster: u64) -> Grid {
        (HEAP, cluster, card.len() as u64)
    }

    #[allow(clippy::too_many_arguments)]
    fn find_with<S: Seams>(
        seams: &mut S,
        src: &dyn Source,
        grids: &[Grid],
        start: u64,
        starts: &[u64],
        live: &[Fragment],
        limits: &Limits,
    ) -> Option<Search> {
        let taken = Taken::new(live, &Runs::default());
        let space = Space::new(grids, start, src.len(), &taken, starts, limits);
        let mut budget = Budget::new(limits);
        search(
            seams,
            src,
            &space,
            start,
            None,
            limits,
            &mut budget,
            &CancelToken::default(),
        )
    }

    fn find(
        src: &dyn Source,
        grids: &[Grid],
        start: u64,
        starts: &[u64],
        live: &[Fragment],
        limits: &Limits,
    ) -> Option<Search> {
        let mut seams = JpegSeams::new(src, start).expect("a frame the decoder covers");
        find_with(&mut seams, src, grids, start, starts, live, limits)
    }

    fn frame_of(jpeg: &[u8]) -> (Frame, u64) {
        let src = Mem(jpeg.to_vec());
        let mut r = Reader::new(&src);
        let scan_start = walk_header(&mut r, 0, JPEG_MAX_LEN).unwrap().scan_start;
        (
            jpeg_entropy::frame(&mut r, 0, scan_start).unwrap(),
            scan_start,
        )
    }

    /// The decoder's state before every MCU of `jpeg`, and how the decode ended.
    fn cursors(jpeg: &[u8]) -> (Vec<Cursor>, End) {
        let (frame, scan_start) = frame_of(jpeg);
        let src = Mem(jpeg.to_vec());
        let mut r = Reader::new(&src);
        let mut cur = frame.start(scan_start);
        let mut out = Vec::new();
        loop {
            let stop = cur.mcu + RESUME_POINTS as u64 - 1;
            let run = jpeg_entropy::decode_rows(
                &mut r,
                &frame,
                cur.clone(),
                src.len(),
                false,
                true,
                stop,
                &mut |_, _| true,
            );
            if run.end == End::OutOfData && run.mcus == stop {
                let (last, done) = run.resume.split_last().unwrap();
                out.extend_from_slice(done);
                cur = last.clone();
                continue;
            }
            out.extend(run.resume.into_iter().filter(|c| c.mcu < run.mcus));
            return (out, run.end);
        }
    }

    /// `a`'s bytes up to an MCU at or after `from_mcu` where `a` and `b` (photos of one frame) are both byte-aligned and
    /// `keep(byte in a)` holds, then `b`'s from that MCU on: a valid photo, `a` up there and `b` (DC shifted) after.
    fn spliced(a: &[u8], b: &[u8], from_mcu: u64, keep: impl Fn(u64) -> bool) -> (Vec<u8>, u64) {
        let (ca, _) = cursors(a);
        let (cb, _) = cursors(b);
        let (byte_a, byte_b) = (from_mcu as usize..ca.len())
            .find_map(|m| match (ca[m].byte_aligned(), cb[m].byte_aligned()) {
                (Some(x), Some(y)) if keep(x) => Some((x, y)),
                _ => None,
            })
            .expect("an MCU byte-aligned in both photos");
        let mut s = a[..byte_a as usize].to_vec();
        s.extend_from_slice(&b[byte_b as usize..]);
        let (cs, end) = cursors(&s);
        assert_eq!(cs.len(), ca.len(), "the spliced photo decodes every MCU");
        assert_eq!(
            end,
            End::Eoi {
                at: s.len() as u64 - 2
            }
        );
        (s, byte_a)
    }

    #[test]
    fn jpeg_in_four_pieces_on_a_grid_is_unique() {
        let j = testkit::jpeg_smooth(2048, 1536, 3);
        // The photo is about 200 KB: 16 KiB clusters give runs of at least 2 clusters (32 KiB ones would not).
        let cluster = 16 << 10;
        let (card, runs) = scatter(&j, cluster, &[40, 90, 61, 200]);
        assert!(runs.iter().all(|r| r.len >= 2 * cluster));
        let g = grid(&card, cluster);
        let src = Mem(card);
        let s = find(
            &src,
            &[g],
            runs[0].start,
            &[runs[0].start],
            &[],
            &Limits::DEFAULT,
        )
        .expect("found");
        assert_eq!(s.pieces, runs);
        assert!(s.complete && s.unique && s.exhaustive, "{s:?}");
        assert_eq!(s.proof, Proof::Proven);
    }

    #[test]
    fn same_frame_neighbour_tail_is_not_intact() {
        // P2. A photo in 3 runs of 8 KiB clusters. At each seam, a burst neighbour's tail that passes: the photo's own
        // data from the seam, turning into its neighbour's three MCU rows below it (where both are byte-aligned), so it
        // decodes to EOI from the photo's state. One copy lies before the true run in candidate order, one after. Both
        // seams have a look-alike: never unique. Were the early DC test to prune the true seam wrongly, the look-alikes
        // (the same data at the seam) would go too and nothing would be complete.
        let burst = testkit::jpeg_burst(1024, 768, 5, 2);
        let (a, b) = (&burst[0], &burst[1]);
        let cluster = 8 << 10;
        let mut card = noise_card(140, cluster);
        let runs = put_runs(&mut card, a, cluster, &[40, 70, 100]);
        let (cs, _) = cursors(a);
        let mcus_x = u64::from(frame_of(a).0.mcus_x);
        let tail_from = |c: u64| -> Vec<u8> {
            let mcu = cs.partition_point(|x| x.pos <= c) as u64;
            let (s, _) = spliced(a, b, (mcu / mcus_x + 4) * mcus_x, |_| true);
            s[c as usize..].to_vec()
        };
        let (c1, c2) = (runs[0].len, runs[0].len + runs[1].len);
        let (t1, t2) = (tail_from(c1), tail_from(c2));
        // Seam 1 (after cluster 43): a look-alike at 50, the true run at 70, a look-alike at 80. Seam 2 (after cluster
        // 73): a look-alike at 90, the true run at 100, a look-alike at 120.
        for (k, t) in [(50, &t1), (80, &t1), (90, &t2), (120, &t2)] {
            put(&mut card, at(cluster, k), t);
        }
        let g = grid(&card, cluster);
        let src = Mem(card);
        let s = find(
            &src,
            &[g],
            runs[0].start,
            &[runs[0].start],
            &[],
            &Limits::DEFAULT,
        )
        .expect("found");
        assert!(s.complete, "{s:?}");
        assert!(!s.unique, "{s:?}");
    }

    #[test]
    fn blind_one_cluster_continuation_next_to_a_burst_tail_is_not_intact() {
        // Q2. A wide photo: one MCU row is larger than one cluster, so a one-cluster piece between taken clusters holds
        // less than the MCU row below its seam (no evidence either way: blind).
        let burst = testkit::jpeg_burst(24576, 128, 5, 2);
        let (a, b) = (&burst[0], &burst[1]);
        let cluster = 4096;
        let (ca, _) = cursors(a);
        let mcus_x = frame_of(a).0.mcus_x as usize;
        let rows: Vec<u64> = ca.iter().step_by(mcus_x).map(|c| c.pos).collect();
        assert!(
            rows.windows(2).all(|w| w[1] - w[0] > cluster),
            "one MCU row is larger than one cluster"
        );
        let c1 = 6 * cluster;
        // The burst neighbour's tail that passes after the first piece: `a`'s data from the cut, turning into `b`'s two
        // MCU rows below it (where both are byte-aligned).
        let row_of_c1 = rows.partition_point(|&p| p <= c1) as u64;
        let (s, _) = spliced(a, b, (row_of_c1 + 2) * mcus_x as u64, |_| true);
        let mut card = noise_card(200, cluster);
        let a1 = put(&mut card, at(cluster, 10), &a[..c1 as usize]);
        let m = put(
            &mut card,
            at(cluster, 30),
            &a[c1 as usize..(c1 + cluster) as usize],
        );
        let a3 = put(&mut card, at(cluster, 50), &a[(c1 + cluster) as usize..]);
        let t = put(&mut card, at(cluster, 100), &s[c1 as usize..]);
        let live = [
            Fragment {
                start: at(cluster, 29),
                len: cluster,
            },
            Fragment {
                start: at(cluster, 31),
                len: cluster,
            },
        ];
        let g = grid(&card, cluster);
        let src = Mem(card);
        // The test discriminates only with at most 3 pieces (the true path has 3, the look-alike 2): with more, paths
        // through the photo's own data decoded out of step (pieces inside A3 and T) can complete, or take the budget,
        // and make the result non-unique whether or not the blind piece is followed.
        let limits = Limits {
            pieces: 3,
            ..Limits::DEFAULT
        };
        let found = find(&src, &[g], a1.start, &[a1.start], &live, &limits).expect("found");
        assert!(found.complete, "{found:?}");
        assert!(
            !(found.unique && found.proof == Proof::Proven),
            "the blind true path [{a1:?}, {m:?}, {a3:?}] next to the look-alike {t:?}: {found:?}"
        );
    }

    #[test]
    fn restart_interval_is_reported() {
        let j = testkit::jpeg_flat_with_restarts(512, 384, 4);
        let src = Mem(j);
        assert_eq!(JpegSeams::new(&src, 0).unwrap().restart_interval(), 4);
        let plain = Mem(testkit::jpeg_smooth(64, 64, 1));
        assert_eq!(JpegSeams::new(&plain, 0).unwrap().restart_interval(), 0);
    }

    #[test]
    fn jpeg_without_grid_in_three_pieces_within_the_window() {
        let j = testkit::jpeg_smooth(1024, 768, 1);
        let len = j.len() as u64;
        let (c1, c2) = (len / 3 / SECTOR * SECTOR, 2 * len / 3 / SECTOR * SECTOR);
        let p1 = 64 << 10;
        let (p2, p3) = (p1 + 3 * MIB, p1 + 7 * MIB);
        let mut card = noise(p3 + len + (64 << 10), 0xC0FFEE);
        let runs = [
            put(&mut card, p1, &j[..c1 as usize]),
            put(&mut card, p2, &j[c1 as usize..c2 as usize]),
            put(&mut card, p3, &j[c2 as usize..]),
        ];
        let src = Mem(card);
        // More than 4096 sectors lie between the second piece and the third: the cap counts prefilter survivors.
        assert!((p3 - (p2 + c2 - c1)) / SECTOR > 4096);
        let s = find(&src, &[], p1, &[p1], &[], &Limits::DEFAULT).expect("found");
        assert_eq!(s.pieces, runs);
        assert!(s.complete && s.unique, "{s:?}");
        assert_eq!(s.proof, Proof::Proven);
    }

    #[test]
    fn erased_gap_is_not_a_continuation() {
        let j = testkit::jpeg_smooth(1024, 768, 1);
        let cluster = 8 << 10;
        let mut card = noise_card(80, cluster);
        // Three runs of 3 clusters, each followed by 3 zeroed clusters.
        let runs = put_runs(&mut card, &j, cluster, &[40, 46, 52]);
        let zeros: Vec<Fragment> = [43, 49, 55]
            .iter()
            .map(|&k| put(&mut card, at(cluster, k), &vec![0; 3 * cluster as usize]))
            .collect();
        // The last run ends inside its third cluster: the zeros start right after it.
        assert_eq!(
            runs[2].start + runs[2].len.next_multiple_of(cluster),
            zeros[2].start
        );
        let g = grid(&card, cluster);
        let src = Mem(card);
        let s = find(
            &src,
            &[g],
            runs[0].start,
            &[runs[0].start],
            &[],
            &Limits::DEFAULT,
        )
        .expect("found");
        for p in &s.pieces {
            for z in &zeros {
                assert!(
                    p.start + p.len <= z.start || z.start + z.len <= p.start,
                    "{p:?} in {z:?}"
                );
            }
        }
        assert_eq!(s.pieces, runs);
        assert!(s.complete && s.unique, "{s:?}");
    }

    /// `bytes` split into two pieces at `seam`.
    fn two_pieces(bytes: Vec<u8>, seam: u64) -> (Mem, Vec<Fragment>) {
        let len = bytes.len() as u64;
        let pieces = vec![
            Fragment {
                start: 0,
                len: seam,
            },
            Fragment {
                start: seam,
                len: len - seam,
            },
        ];
        (Mem(bytes), pieces)
    }

    #[test]
    fn foreign_data_right_after_a_seam_breaks_there() {
        let a = testkit::jpeg_smooth(1024, 768, 1);
        let c1 = 20 * SECTOR;
        let mut bytes = a[..c1 as usize].to_vec();
        bytes.extend(noise(64 << 10, 3));
        let (src, pieces) = two_pieces(bytes, c1);
        let view = crate::source::Fragmented::new(&src, pieces);
        let mut js = JpegSeams::new(&view, 0).unwrap();
        let begin = js.begin();
        let w = js.walk(&view, &[c1], &begin, view.len());
        assert!(
            matches!(w.end, WalkEnd::Broken { at } if at == c1),
            "broken right at the seam"
        );
        assert!(w.checked <= c1);
        // The true continuation, shorter than the MCU row below the seam: no evidence either way (blind), broken where
        // the view ends.
        let short = c1 + 300;
        let (src, pieces) = two_pieces(a[..short as usize].to_vec(), c1);
        let view = crate::source::Fragmented::new(&src, pieces);
        let w = js.walk(&view, &[c1], &begin, view.len());
        assert!(
            matches!(w.end, WalkEnd::Broken { at } if at == short),
            "blind"
        );
        assert!(w.checked <= c1);
    }

    #[test]
    fn a_seam_glued_to_another_scene_in_the_last_rows_is_refuted() {
        // Two photos of one frame and different scenes, spliced at an MCU of the second-to-last MCU row (where both are
        // byte-aligned): a valid JPEG whose only seam no early test can reach (no MCU row below it to wait for).
        let a = testkit::jpeg_smooth(1024, 768, 1);
        let other = testkit::jpeg_smooth(1024, 768, 9);
        let (frame, _) = frame_of(&a);
        let row = u64::from(frame.mcus_x);
        let last_rows = frame.total_mcus() - 2 * row;
        let (glued, seam) = spliced(&a, &other, last_rows, |_| true);
        let (src, pieces) = two_pieces(glued, seam);
        let view = crate::source::Fragmented::new(&src, pieces);
        let mut js = JpegSeams::new(&view, 0).unwrap();
        let begin = js.begin();
        let w = js.walk(&view, &[seam], &begin, view.len());
        assert!(
            matches!(w.end, WalkEnd::Broken { at } if at == seam),
            "refuted at its seam"
        );
        assert_eq!(js.prove(&view, &[seam]), Proof::Failed);
        // The photo itself, split at the same place: complete and proven.
        let (src, pieces) = two_pieces(a.clone(), seam);
        let view = crate::source::Fragmented::new(&src, pieces);
        let w = js.walk(&view, &[seam], &begin, view.len());
        assert!(matches!(w.end, WalkEnd::Complete { len } if len == a.len() as u64));
        assert_eq!(js.prove(&view, &[seam]), Proof::Proven);
    }

    /// P1: a photo that turns into a burst neighbour's data at `pct` % of its MCUs (where both are byte-aligned), stored
    /// as one run of 4 KiB clusters; the photo's own rest, from the cluster where the two part, lies elsewhere, free.
    /// The search with the default limits.
    fn swapped_in_one_run(burst: &[Vec<u8>], pct: u64) -> Search {
        swapped_with(burst, pct, &Limits::DEFAULT)
    }

    fn swapped_with(burst: &[Vec<u8>], pct: u64, limits: &Limits) -> Search {
        let (a, b) = (&burst[0], &burst[1]);
        let cluster = 4096;
        let mcus = cursors(a).0.len() as u64;
        let (s, byte_a) = spliced(a, b, mcus * pct / 100, |x| x % cluster >= 64);
        let cut = byte_a / cluster * cluster;
        let mut card = noise_card(260, cluster);
        let whole = put(&mut card, at(cluster, 10), &s);
        put(&mut card, at(cluster, 150), &a[cut as usize..]);
        let g = grid(&card, cluster);
        let src = Mem(card);
        let found = find(&src, &[g], whole.start, &[whole.start], &[], limits).expect("found");
        assert_eq!(found.pieces, [whole], "{pct} %");
        assert!(found.complete, "{pct} %: {found:?}");
        found
    }

    #[test]
    #[cfg_attr(
        debug_assertions,
        ignore = "about 30-50 s in debug: runs in the release suite"
    )]
    fn a_look_alike_swapped_into_the_upper_half_is_not_intact() {
        // Q33. The re-check never tries the solution's own later clusters (decoded out of step they resync and look
        // evident, and would take its whole budget), and must walk the candidates of the top-hinted cut to the end.
        let burst = testkit::jpeg_burst(2048, 1536, 5, 2);
        let intact: Vec<u64> = [5, 10, 25, 40]
            .into_iter()
            .filter(|&pct| {
                let found = swapped_in_one_run(&burst, pct);
                found.unique && found.exhaustive
            })
            .collect();
        assert!(
            intact.is_empty(),
            "unique and exhaustive with the swap at {intact:?} %"
        );
    }

    #[test]
    #[cfg_attr(
        debug_assertions,
        ignore = "about 30-50 s in debug: runs in the release suite"
    )]
    fn a_recheck_cut_short_at_the_top_hinted_cut_is_not_exhaustive() {
        // Q33 (b): with a re-check budget too small to walk every candidate of the top-hinted cut, the look-alike may
        // go unseen: the result is then not exhaustive.
        let burst = testkit::jpeg_burst(2048, 1536, 5, 2);
        let limits = Limits {
            recheck_tries: 8,
            ..Limits::DEFAULT
        };
        let found = swapped_with(&burst, 25, &limits);
        assert!(!(found.unique && found.exhaustive), "{found:?}");
    }

    #[test]
    #[cfg_attr(
        debug_assertions,
        ignore = "about 30-50 s in debug: runs in the release suite"
    )]
    fn a_look_alike_swapped_into_the_lower_half_is_not_unique() {
        let burst = testkit::jpeg_burst(2048, 1536, 5, 2);
        for pct in [55, 70, 85, 95] {
            let found = swapped_in_one_run(&burst, pct);
            assert!(!found.unique, "{pct} %: {found:?}");
        }
    }

    #[test]
    #[cfg_attr(
        debug_assertions,
        ignore = "about 30-50 s in debug: runs in the release suite"
    )]
    fn a_later_piece_of_the_solution_reused_by_the_true_path_is_not_unique() {
        // Q34. The solution is [P, A3]: P is one run holding a's data with MCUs x..y from its burst neighbour b (spliced
        // where both photos are byte-aligned, x and y chosen so that P's data after y is a's shifted by whole clusters),
        // and A3 is a's rest. The true path is [a[..c1], A2, A3]: it cuts P before x, goes on in A2 = a[c1..c2] (free,
        // elsewhere) and reuses the solution's own last piece. The re-check must still list A3 as a candidate.
        let burst = testkit::jpeg_burst(2048, 1536, 5, 2);
        let (a, b) = (&burst[0], &burst[1]);
        let cluster = 4096u64;
        let (ca, _) = cursors(a);
        let (cb, _) = cursors(b);
        let mcus_x = u64::from(frame_of(a).0.mcus_x) as usize;
        let total = ca.len();
        // MCUs byte-aligned in both photos, from the middle of the photo on: (MCU, byte in a, byte in b).
        let aligned: Vec<(usize, u64, u64)> = (total / 2..total * 9 / 10)
            .filter_map(|m| Some((m, ca[m].byte_aligned()?, cb[m].byte_aligned()?)))
            .collect();
        let residue = |x: u64, y: u64| (y.wrapping_sub(x)) % cluster;
        let mut first: std::collections::HashMap<u64, (usize, u64, u64)> = Default::default();
        let mut pair = None;
        for &(m, xa, xb) in &aligned {
            if let Some(&(m0, x0a, x0b)) = first.get(&residue(xa, xb))
                && m >= m0 + 8 * mcus_x
            {
                pair = Some(((m0, x0a, x0b), (m, xa, xb)));
                break;
            }
            if xa % cluster >= 64 {
                first.entry(residue(xa, xb)).or_insert((m, xa, xb));
            }
        }
        let ((_, xa, xb), (_, ya, yb)) =
            pair.expect("two MCUs where a and b part by whole clusters");
        let mut s = a[..xa as usize].to_vec();
        s.extend_from_slice(&b[xb as usize..yb as usize]);
        s.extend_from_slice(&a[ya as usize..]);
        let (cs, end) = cursors(&s);
        assert_eq!(
            (cs.len(), end),
            (
                total,
                End::Eoi {
                    at: s.len() as u64 - 2
                }
            )
        );
        // Past y, `s` is `a` shifted by `d` (a multiple of the cluster).
        let d = (xa + (yb - xb)) as i64 - ya as i64;
        assert_eq!(d.rem_euclid(cluster as i64), 0);
        let c1 = xa / cluster * cluster;
        let c2 = (ya / cluster + 2) * cluster;
        let e = (c2 as i64 + d) as u64;
        assert_eq!(s[e as usize..], a[c2 as usize..]);
        let mut card = noise_card(450, cluster);
        let p = put(&mut card, at(cluster, 10), &s[..e as usize]);
        let a2 = put(&mut card, at(cluster, 200), &a[c1 as usize..c2 as usize]);
        let a3 = put(&mut card, at(cluster, 350), &a[c2 as usize..]);
        let g = grid(&card, cluster);
        let src = Mem(card);
        let found = find(&src, &[g], p.start, &[p.start], &[], &Limits::DEFAULT).expect("found");
        assert!(found.complete, "{found:?}");
        assert!(
            !found.unique,
            "the true path [a[..c1], {a2:?}, {a3:?}] was not seen: {found:?}"
        );
    }

    #[test]
    #[cfg_attr(
        debug_assertions,
        ignore = "about 30-50 s in debug: runs in the release suite"
    )]
    fn a_solution_at_the_piece_cap_can_be_exhaustive() {
        // Q34. With at most 2 pieces, the last piece of a 2-piece solution cannot be cut (no third piece): its top-hinted
        // cut has nothing to walk and does not make the result non-exhaustive.
        // The second run is the photo's last two clusters, the last holding 51 bytes (seed 8): it has a cut to hint at,
        // and its one later cluster holds too little (under an MCU row) to decode out of step into evidence, break, and
        // hit the piece cap in the main search (which would rightly make the result non-exhaustive).
        let j = testkit::jpeg_smooth(2048, 1536, 8);
        assert_eq!(j.len() % (16 << 10), 51);
        let cluster = 16 << 10;
        let c1 = ((j.len() - 1) / cluster as usize - 1) * cluster as usize;
        assert!(j.len() - c1 > cluster as usize);
        let mut card = noise_card(110, cluster);
        let runs = [
            put(&mut card, at(cluster, 40), &j[..c1]),
            put(&mut card, at(cluster, 90), &j[c1..]),
        ];
        let g = grid(&card, cluster);
        let src = Mem(card);
        let limits = Limits {
            pieces: 2,
            ..Limits::DEFAULT
        };
        let s = find(&src, &[g], runs[0].start, &[runs[0].start], &[], &limits).expect("found");
        assert_eq!(s.pieces, runs);
        assert!(s.complete && s.unique && s.exhaustive, "{s:?}");
        assert_eq!(s.proof, Proof::Proven);
    }

    /// [`JpegSeams`] without its hint to the uniqueness re-check.
    struct NoHint(JpegSeams);

    impl Seams for NoHint {
        type State = JpegState;
        fn begin(&mut self) -> JpegState {
            self.0.begin()
        }
        fn walk(&mut self, v: &dyn Source, s: &[u64], f: &JpegState, l: u64) -> Walked<JpegState> {
            self.0.walk(v, s, f, l)
        }
        fn prove(&mut self, view: &dyn Source, seams: &[u64]) -> Proof {
            self.0.prove(view, seams)
        }
        fn prefilter(&mut self, r: &mut Reader, g: u64, seam: u64) -> bool {
            self.0.prefilter(r, g, seam)
        }
    }

    #[test]
    fn recheck_cuts_first_where_the_dc_grid_jumps() {
        // A photo whose data turns, deep inside it, into a near-identical burst neighbour's (DC grids 0.5–1.4 apart):
        // valid as one piece. The photo's own rest, from the cluster where the two part, lies elsewhere and is free.
        // The swap is in the lower half of the photo, where the early DC test drops the piece's own later clusters at
        // once (decoded out of step, they jump in DC): in the upper half each of them is evident until more than half
        // the photo is decoded, and the re-check's read budget goes to them first (in candidate order). The re-check
        // tries 8 break points per piece here (64 by default), so the photo stays small.
        let burst = testkit::jpeg_burst(2048, 1536, 5, 2);
        let (a, b) = (&burst[0], &burst[1]);
        let cluster = 4096;
        let limits = Limits {
            break_points: 8,
            ..Limits::DEFAULT
        };
        let mcus = cursors(a).0.len() as u64;
        let (s, byte_a) = spliced(a, b, mcus * 3 / 5, |x| x % cluster >= 64);
        let cut = byte_a / cluster * cluster;
        assert!(
            s.len() as u64 - cut > (limits.break_points as u64 + 4) * cluster,
            "the swap is beyond the nearest break points"
        );
        let mut card = noise_card(260, cluster);
        let whole = put(&mut card, at(cluster, 10), &s);
        put(&mut card, at(cluster, 150), &a[cut as usize..]);
        let g = grid(&card, cluster);
        let src = Mem(card);
        let found = find(&src, &[g], whole.start, &[whole.start], &[], &limits).expect("found");
        assert_eq!(found.pieces, [whole]);
        assert!(found.complete, "{found:?}");
        assert!(
            !found.unique,
            "the hint puts the cut where the DC grid jumps first: {found:?}"
        );
        // Without the hint the re-check tries only the nearest break points and misses it.
        let mut plain = NoHint(JpegSeams::new(&src, whole.start).unwrap());
        let found = find_with(
            &mut plain,
            &src,
            &[g],
            whole.start,
            &[whole.start],
            &[],
            &limits,
        )
        .expect("found");
        assert!(found.unique, "{found:?}");
    }
}
