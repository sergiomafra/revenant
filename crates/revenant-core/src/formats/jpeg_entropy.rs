//! Entropy decoder for sequential (baseline and extended) Huffman-coded JPEGs, written from ITU-T T.81: Annex B (marker
//! segments: SOF0/SOF1, DHT, DRI, SOS), Annex C (canonical Huffman codes from a DHT) and Annex F.2 (decoding the DC
//! difference and the AC run/size symbols of each block, MCU by MCU, with restart markers).
//!
//! It produces no pixels: it walks the entropy-coded data and counts complete MCUs exactly, so a caller can tell where a
//! JPEG's data stops being valid (an impossible code, a wrong restart marker, EOI too early or too late) and can resume the
//! walk from a saved [`Cursor`] over other bytes. Every loop is bounded by the MCU count of the frame (itself bounded by
//! [`MAX_DECODE_PIXELS`]), by 64 coefficients per block and 16 bits per code, and by the bytes below the caller's limit.

use std::collections::VecDeque;

use super::Reader;
use super::jpeg::{MAX_DECODE_PIXELS, is_sof};

/// Cursors a [`Run`] keeps (the last MCU boundaries before it ended).
pub(crate) const RESUME_POINTS: usize = 4096;

/// Bits looked up at once by [`Huffman::fast`]; longer codes take the canonical slow path.
const LOOKUP_BITS: u32 = 9;
/// Most blocks one interleaved MCU may hold (T.81 B.2.3).
const MAX_BLOCKS_PER_MCU: u32 = 10;
/// Bytes read from the source at a time.
const CHUNK: usize = 16 * 1024;

/// One decoding table built from a DHT (T.81 Annex C and F.2.2.3).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Huffman {
    /// By the next [`LOOKUP_BITS`] bits: `(code length, symbol)`; length 0 when the code is longer (or there is none).
    fast: Vec<(u8, u8)>,
    /// Largest code of each length 1..=16 (`-1`: no code of that length).
    maxcode: [i32; 17],
    /// For each length: index in `vals` of a code of that length is `code + valoff`.
    valoff: [i32; 17],
    vals: Vec<u8>,
}

impl Huffman {
    /// From the 16 counts of codes per length and the symbols. `None` when the counts cannot form a prefix code (more
    /// codes of some length than remain) or exceed 256 symbols.
    fn new(counts: &[u8; 16], vals: &[u8]) -> Option<Huffman> {
        let total: usize = counts.iter().map(|&c| usize::from(c)).sum();
        if total > 256 || vals.len() != total {
            return None;
        }
        let mut fast = vec![(0u8, 0u8); 1 << LOOKUP_BITS];
        let mut maxcode = [-1i32; 17];
        let mut valoff = [0i32; 17];
        let (mut code, mut k) = (0u32, 0usize);
        for len in 1..=16u32 {
            let n = u32::from(counts[len as usize - 1]);
            if n > 0 {
                valoff[len as usize] = k as i32 - code as i32;
                for _ in 0..n {
                    if code >= 1 << len {
                        return None;
                    }
                    if len <= LOOKUP_BITS {
                        let shift = LOOKUP_BITS - len;
                        let first = (code << shift) as usize;
                        for e in &mut fast[first..first + (1 << shift)] {
                            *e = (len as u8, vals[k]);
                        }
                    }
                    code += 1;
                    k += 1;
                }
                maxcode[len as usize] = code as i32 - 1;
            }
            code <<= 1;
        }
        Some(Huffman {
            fast,
            maxcode,
            valoff,
            vals: vals.to_vec(),
        })
    }
}

/// One component of the scan, in scan order.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Component {
    /// Blocks of this component in one MCU, horizontally and vertically.
    h: u32,
    v: u32,
    /// Indexes into [`Frame::tables`].
    dc: usize,
    ac: usize,
    /// Frame component 0, whose DC values a run can keep.
    luma: bool,
}

/// What [`decode`] needs from a JPEG's header: the MCU grid, the restart interval, the scan's components and tables.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Frame {
    pub mcus_x: u32,
    pub mcus_y: u32,
    /// MCUs between restart markers; 0 means no restart markers.
    pub restart_interval: u16,
    components: Vec<Component>,
    tables: Vec<Huffman>,
    luma_hv: (u32, u32),
}

impl Frame {
    /// A frame with this geometry and nothing to decode with (every code is invalid), for tests of what is computed
    /// from the geometry alone.
    #[cfg(test)]
    pub(crate) fn for_test(mcus_x: u32, mcus_y: u32, luma_hv: (u32, u32)) -> Frame {
        Frame {
            mcus_x,
            mcus_y,
            restart_interval: 0,
            components: vec![Component {
                h: luma_hv.0,
                v: luma_hv.1,
                dc: 0,
                ac: 0,
                luma: true,
            }],
            tables: vec![Huffman::new(&[0; 16], &[]).expect("an empty table is valid")],
            luma_hv,
        }
    }

    pub(crate) fn total_mcus(&self) -> u64 {
        u64::from(self.mcus_x) * u64::from(self.mcus_y)
    }

    /// Blocks of component 0 in one MCU, `(horizontally, vertically)`; `(1, 1)` for a single-component scan.
    pub(crate) fn luma_blocks_per_mcu(&self) -> (u32, u32) {
        self.luma_hv
    }

    /// The state before the first MCU, for entropy-coded data starting at `scan_start`.
    pub(crate) fn start(&self, scan_start: u64) -> Cursor {
        Cursor {
            pos: scan_start,
            mcu: 0,
            bits: 0,
            nbits: 0,
            byte_pos: [0; 8],
            loaded: 0,
            pred: [0; 4],
        }
    }
}

/// Frame component as declared in the SOF: `(id, h, v)`.
type SofComponent = (u8, u32, u32);

/// From the JPEG's header segments (DHT, SOF0/SOF1, DRI, SOS) between `start` (its SOI) and `scan_start` (just past its
/// first SOS). `None` unless: sequential DCT with Huffman coding (SOF0/1), 8-bit, 1–4 components all in the single scan,
/// sampling factors ≤ 2, every referenced table present, width × height ≤ `MAX_DECODE_PIXELS`.
pub(crate) fn frame(r: &mut Reader, start: u64, scan_start: u64) -> Option<Frame> {
    let mut pos = start.checked_add(2)?;
    // Tables by class (0 = DC, 1 = AC) and id; a later DHT replaces an earlier one.
    let mut defined: [[Option<Huffman>; 4]; 2] = Default::default();
    let mut sof: Option<(u32, u32, Vec<SofComponent>)> = None;
    // Quantization tables each frame component uses, and those a DQT defined.
    let mut sof_tq: Vec<u8> = Vec::new();
    let mut quant = [false; 4];
    let mut restart_interval = 0u16;
    // Each step moves `pos` forward by at least one byte, and it stays below `scan_start`.
    while pos + 4 <= scan_start {
        if r.byte(pos)? != 0xFF {
            return None;
        }
        let marker = r.byte(pos + 1)?;
        if marker == 0xFF {
            pos += 1; // fill byte
            continue;
        }
        let len = u64::from(r.u16(pos + 2)?);
        let end = pos + 2 + len;
        if len < 2 || end > scan_start {
            return None;
        }
        let payload = |r: &mut Reader| {
            let mut p = vec![0u8; len as usize - 2];
            (r.read(pos + 4, &mut p) == p.len()).then_some(p)
        };
        match marker {
            0xC0 | 0xC1 if sof.is_none() => {
                let p = payload(r)?;
                sof = Some(parse_sof(&p)?);
                sof_tq = p[6..].as_chunks::<3>().0.iter().map(|c| c[2]).collect();
            }
            0xDB => parse_dqt(&payload(r)?, &mut quant)?,
            // Only SOF0/SOF1 (sequential, Huffman) are decoded; a second SOF is not a valid frame either.
            m if is_sof(m) => return None,
            0xC4 => parse_dht(&payload(r)?, &mut defined)?,
            0xCC => return None, // DAC: arithmetic coding
            0xDD => {
                let p = payload(r)?;
                if p.len() != 2 {
                    return None;
                }
                restart_interval = u16::from_be_bytes([p[0], p[1]]);
            }
            0xDA => {
                if end != scan_start {
                    return None;
                }
                let (width, height, comps) = sof?;
                // Every quantization table a component uses must be defined (the pixels could not be computed).
                if !sof_tq
                    .iter()
                    .all(|&t| quant.get(usize::from(t)) == Some(&true))
                {
                    return None;
                }
                return build(
                    width,
                    height,
                    &comps,
                    &payload(r)?,
                    &defined,
                    restart_interval,
                );
            }
            _ => {}
        }
        pos = end;
    }
    None
}

/// SOF0/SOF1 payload: precision, height, width, components `(id, h, v)`.
fn parse_sof(p: &[u8]) -> Option<(u32, u32, Vec<SofComponent>)> {
    let (&precision, rest) = p.split_first()?;
    if precision != 8 || rest.len() < 5 {
        return None;
    }
    let height = u32::from(u16::from_be_bytes([rest[0], rest[1]]));
    let width = u32::from(u16::from_be_bytes([rest[2], rest[3]]));
    let n = usize::from(rest[4]);
    if !(1..=4).contains(&n) || rest.len() != 5 + 3 * n || width == 0 || height == 0 {
        return None;
    }
    if u64::from(width) * u64::from(height) > MAX_DECODE_PIXELS {
        return None;
    }
    let mut comps: Vec<SofComponent> = Vec::with_capacity(n);
    for c in rest[5..].as_chunks::<3>().0 {
        let (h, v) = (u32::from(c[1] >> 4), u32::from(c[1] & 0x0F));
        if !(1..=2).contains(&h)
            || !(1..=2).contains(&v)
            || comps.iter().any(|&(id, ..)| id == c[0])
        {
            return None;
        }
        comps.push((c[0], h, v));
    }
    Some((width, height, comps))
}

/// One DQT payload (one or more tables): marks each table id as defined. `None` for an id over 3, a precision other
/// than 8 or 16 bits, or a payload that does not hold whole tables.
fn parse_dqt(mut p: &[u8], defined: &mut [bool; 4]) -> Option<()> {
    while let Some((&pq_tq, rest)) = p.split_first() {
        let (precision, id) = (pq_tq >> 4, usize::from(pq_tq & 0x0F));
        if precision > 1 || id > 3 {
            return None;
        }
        let size = 64 * (1 + usize::from(precision));
        p = rest.get(size..)?;
        defined[id] = true;
    }
    Some(())
}

/// One DHT payload (one or more tables) into `defined`.
fn parse_dht(mut p: &[u8], defined: &mut [[Option<Huffman>; 4]; 2]) -> Option<()> {
    while let Some((&tc_th, rest)) = p.split_first() {
        let (class, id) = (usize::from(tc_th >> 4), usize::from(tc_th & 0x0F));
        if class > 1 || id > 3 || rest.len() < 16 {
            return None;
        }
        let counts: &[u8; 16] = rest[..16].try_into().ok()?;
        let n: usize = counts.iter().map(|&c| usize::from(c)).sum();
        let vals = rest.get(16..16 + n)?;
        defined[class][id] = Some(Huffman::new(counts, vals)?);
        p = &rest[16 + n..];
    }
    Some(())
}

/// The frame from the SOF, the SOS payload and the tables defined before the SOS.
fn build(
    width: u32,
    height: u32,
    comps: &[SofComponent],
    sos: &[u8],
    defined: &[[Option<Huffman>; 4]; 2],
    restart_interval: u16,
) -> Option<Frame> {
    let (&ns, rest) = sos.split_first()?;
    let ns = usize::from(ns);
    // Every frame component in this one scan; spectral selection 0..=63 and no successive approximation (T.81 B.2.3).
    if ns != comps.len() || rest.len() != 2 * ns + 3 || rest[2 * ns..] != [0, 63, 0] {
        return None;
    }
    let h_max = comps.iter().map(|c| c.1).max()?;
    let v_max = comps.iter().map(|c| c.2).max()?;
    let interleaved = ns > 1;
    let mut tables: Vec<Huffman> = Vec::new();
    let mut table_index = |class: usize, id: usize| -> Option<usize> {
        tables.push(defined[class].get(id)?.clone()?);
        Some(tables.len() - 1)
    };
    let mut components = Vec::with_capacity(ns);
    for sel in rest[..2 * ns].as_chunks::<2>().0 {
        let i = comps.iter().position(|c| c.0 == sel[0])?;
        if components.iter().any(|c: &(usize, Component)| c.0 == i) {
            return None;
        }
        let (_, h, v) = comps[i];
        let (h, v) = if interleaved { (h, v) } else { (1, 1) };
        components.push((
            i,
            Component {
                h,
                v,
                dc: table_index(0, usize::from(sel[1] >> 4))?,
                ac: table_index(1, usize::from(sel[1] & 0x0F))?,
                luma: i == 0,
            },
        ));
    }
    let blocks: u32 = components.iter().map(|(_, c)| c.h * c.v).sum();
    if interleaved && blocks > MAX_BLOCKS_PER_MCU {
        return None;
    }
    // Interleaved: MCUs of 8·Hmax × 8·Vmax pixels. One component: one block per MCU over the component's own size,
    // which for a lone frame component is the image size.
    let (mcus_x, mcus_y) = if interleaved {
        (width.div_ceil(8 * h_max), height.div_ceil(8 * v_max))
    } else {
        (width.div_ceil(8), height.div_ceil(8))
    };
    let luma = components
        .iter()
        .find(|(_, c)| c.luma)
        .map(|(_, c)| (c.h, c.v))?;
    Some(Frame {
        mcus_x,
        mcus_y,
        restart_interval,
        components: components.into_iter().map(|(_, c)| c).collect(),
        tables,
        luma_hv: luma,
    })
}

/// The decoder's state at an MCU boundary (or anywhere in a walk): where the bytes are, what of them is buffered, and
/// the DC predictors.
#[derive(Clone, Debug)]
pub(crate) struct Cursor {
    /// First byte not yet consumed into the bit buffer.
    pub pos: u64,
    /// Index of the next MCU to decode.
    pub mcu: u64,
    /// Buffered bits, most significant first; the bits past `nbits` are zero.
    bits: u64,
    nbits: u32,
    /// Source positions of the last 8 bytes loaded into `bits` (a ring indexed by `loaded`), so an error can name the
    /// byte its code started in.
    byte_pos: [u64; 8],
    loaded: u8,
    /// DC predictor of each scan component.
    pred: [i32; 4],
}

impl Cursor {
    /// The same state, every position minus `by` (for decoding the same bytes through a view that starts `by` later).
    pub(crate) fn shifted_back(&self, by: u64) -> Cursor {
        let mut c = self.clone();
        c.pos -= by;
        for p in &mut c.byte_pos {
            *p = p.saturating_sub(by);
        }
        c
    }

    /// Everything the rest of a walk from this state depends on besides the bytes from `pos` on: `(mcu, bits, nbits,
    /// DC predictors)`. Two states with the same key at the same position over the same bytes end the same way.
    pub(crate) fn key(&self) -> (u64, u64, u32, [i32; 4]) {
        (self.mcu, self.bits, self.nbits, self.pred)
    }

    /// The position of the byte holding the next unconsumed bit when the bits consumed so far end on a byte boundary
    /// (so another stream byte-aligned at the same MCU could be spliced in there), else `None`.
    #[cfg(test)]
    pub(crate) fn byte_aligned(&self) -> Option<u64> {
        self.nbits.is_multiple_of(8).then(|| self.current_byte())
    }

    /// Position of the byte holding the next unconsumed bit.
    fn current_byte(&self) -> u64 {
        if self.nbits == 0 {
            return self.pos;
        }
        let back = self.nbits.div_ceil(8) as u8;
        self.byte_pos[usize::from(self.loaded.wrapping_sub(back) & 7)]
    }

    fn consume(&mut self, n: u32) {
        debug_assert!(n <= self.nbits && n <= 16);
        self.bits <<= n;
        self.nbits -= n;
    }
}

/// How a walk ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum End {
    /// Every MCU decoded, then padding, optional fill bytes and `FF D9`; `at` is the position of that `FF`.
    Eoi { at: u64 },
    /// The data cannot be this frame's: an invalid code or value, a wrong or missing restart marker, EOI before the last
    /// MCU or data after it. `at`: the byte where it was found.
    Error { at: u64 },
    /// The bytes below the limit (or the source) ended first.
    OutOfData,
}

/// Result of [`decode`].
pub(crate) struct Run {
    /// Complete MCUs: the index of the first MCU not decoded.
    pub mcus: u64,
    /// The final cursor's `pos`. Every byte the run read lies below `pos + 2` (it may look at a marker's two bytes
    /// without consuming them).
    pub pos: u64,
    pub end: End,
    /// Cursors at the last [`RESUME_POINTS`] MCU boundaries reached, oldest first; `resume[i]` is the state right before
    /// MCU `resume[i].mcu`.
    pub resume: Vec<Cursor>,
    /// With `keep_dc`: the DC value of every block of component 0 in the complete MCUs of this run, in coding order.
    pub dc: Vec<i32>,
}

/// Bytes `[.., limit)` of the source, read in chunks.
struct Bytes<'r, 'a> {
    r: &'r mut Reader<'a>,
    limit: u64,
    buf: Vec<u8>,
    start: u64,
    len: usize,
}

impl Bytes<'_, '_> {
    /// The byte at `pos`; `None` at or past the limit or the end of the source.
    #[inline]
    fn get(&mut self, pos: u64) -> Option<u8> {
        if pos >= self.limit {
            return None;
        }
        if pos < self.start || pos >= self.start + self.len as u64 {
            let want = (self.limit - pos).min(CHUNK as u64) as usize;
            self.len = self.r.read(pos, &mut self.buf[..want]);
            self.start = pos;
            if self.len == 0 {
                return None;
            }
        }
        Some(self.buf[(pos - self.start) as usize])
    }

    /// The 8 bytes at `pos` as a big-endian word, if they are all already buffered and below the limit.
    #[inline(always)]
    fn word(&self, pos: u64) -> Option<u64> {
        let end = pos.checked_add(8)?;
        if pos < self.start || end > self.start + self.len as u64 || end > self.limit {
            return None;
        }
        let at = (pos - self.start) as usize;
        let bytes: [u8; 8] = self.buf[at..at + 8].try_into().ok()?;
        Some(u64::from_be_bytes(bytes))
    }
}

/// Whether any byte of `w` is `FF`.
#[inline(always)]
fn has_ff(w: u64) -> bool {
    let x = !w; // an FF byte becomes 00
    x.wrapping_sub(0x0101_0101_0101_0101) & !x & 0x8080_8080_8080_8080 != 0
}

/// Why the bit buffer could not be filled further.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stop {
    Full,
    /// A marker (`FF` followed by anything but `00`) at the cursor's `pos`.
    Marker,
    OutOfData,
}

/// Why decoding stopped inside an MCU.
enum Fail {
    Error(u64),
    OutOfData,
}

/// Loads whole bytes into the bit buffer until it holds more than 56 bits, a marker or the end of the data.
#[inline(always)]
fn fill(c: &mut Cursor, src: &mut Bytes) -> Stop {
    // Fast path: the next 8 bytes are buffered and none is `FF` (no stuffing, no marker), so the whole bytes that fit
    // are loaded at once.
    if c.nbits <= 56
        && let Some(w) = src.word(c.pos)
        && !has_ff(w)
    {
        let n = (64 - c.nbits) / 8;
        c.bits |= (w >> (64 - 8 * n)) << (64 - 8 * n - c.nbits);
        for i in 0..n {
            c.byte_pos[usize::from(c.loaded & 7)] = c.pos + u64::from(i);
            c.loaded = c.loaded.wrapping_add(1);
        }
        c.nbits += 8 * n;
        c.pos += u64::from(n);
        return Stop::Full;
    }
    while c.nbits <= 56 {
        let Some(b) = src.get(c.pos) else {
            return Stop::OutOfData;
        };
        let mut step = 1;
        if b == 0xFF {
            match src.get(c.pos + 1) {
                None => return Stop::OutOfData,
                Some(0x00) => step = 2, // stuffed: a data FF
                Some(_) => return Stop::Marker,
            }
        }
        c.bits |= u64::from(b) << (56 - c.nbits);
        c.byte_pos[usize::from(c.loaded & 7)] = c.pos;
        c.loaded = c.loaded.wrapping_add(1);
        c.nbits += 8;
        c.pos += step;
    }
    Stop::Full
}

/// More bits were needed than the data holds before `stop`.
fn short(c: &Cursor, stop: Stop) -> Fail {
    match stop {
        Stop::OutOfData => Fail::OutOfData,
        // A marker inside an MCU (an early EOI, a stray RST or any other marker), or a full buffer (cannot happen: a
        // code is at most 16 bits).
        Stop::Marker | Stop::Full => Fail::Error(c.pos),
    }
}

/// Makes at least `n` (≤ 16) bits available.
#[inline(always)]
fn need(c: &mut Cursor, src: &mut Bytes, n: u32) -> Result<(), Fail> {
    if c.nbits < n {
        let stop = fill(c, src);
        if c.nbits < n {
            return Err(short(c, stop));
        }
    }
    Ok(())
}

/// Decodes one Huffman symbol (T.81 F.2.2.3).
#[inline(always)]
fn symbol(c: &mut Cursor, src: &mut Bytes, t: &Huffman) -> Result<u8, Fail> {
    let stop = if c.nbits < 16 {
        fill(c, src)
    } else {
        Stop::Full
    };
    let (len, sym) = t.fast[(c.bits >> (64 - LOOKUP_BITS)) as usize];
    if len != 0 {
        if u32::from(len) > c.nbits {
            return Err(short(c, stop));
        }
        c.consume(u32::from(len));
        return Ok(sym);
    }
    for len in LOOKUP_BITS + 1..=16 {
        let code = (c.bits >> (64 - len)) as i32;
        if code <= t.maxcode[len as usize] {
            if len > c.nbits {
                return Err(short(c, stop));
            }
            c.consume(len);
            return Ok(t.vals[(code + t.valoff[len as usize]) as usize]);
        }
    }
    if c.nbits < 16 {
        // The bits that exist may still begin a valid code.
        return Err(short(c, stop));
    }
    Err(Fail::Error(c.current_byte()))
}

/// The next `n` (≤ 16) bits as a signed value of category `n` (T.81 F.2.2.1, EXTEND).
#[inline]
fn extended(c: &mut Cursor, src: &mut Bytes, n: u32) -> Result<i32, Fail> {
    if n == 0 {
        return Ok(0);
    }
    need(c, src, n)?;
    let v = (c.bits >> (64 - n)) as i32;
    c.consume(n);
    Ok(if v < 1 << (n - 1) {
        v - (1 << n) + 1
    } else {
        v
    })
}

/// One 8×8 block: the DC difference and the AC coefficients (T.81 F.2.2.1–F.2.2.2). Returns the new DC predictor.
fn block(
    c: &mut Cursor,
    src: &mut Bytes,
    dc: &Huffman,
    ac: &Huffman,
    slot: usize,
) -> Result<i32, Fail> {
    let size = symbol(c, src, dc)?;
    if size > 11 {
        return Err(Fail::Error(c.current_byte()));
    }
    let diff = extended(c, src, u32::from(size))?;
    c.pred[slot] = c.pred[slot].wrapping_add(diff);
    let mut k = 1u32;
    while k < 64 {
        let rs = symbol(c, src, ac)?;
        let (run, size) = (u32::from(rs >> 4), u32::from(rs & 0x0F));
        if size == 0 {
            match run {
                0 => break,                                     // EOB
                15 => {}                                        // ZRL: sixteen zero coefficients
                _ => return Err(Fail::Error(c.current_byte())), // no other run/0 symbol exists (T.81 F.1.2.2.1)
            }
            if k + 15 > 63 {
                return Err(Fail::Error(c.current_byte()));
            }
            k += 16;
            continue;
        }
        if size > 10 || k + run > 63 {
            return Err(Fail::Error(c.current_byte()));
        }
        need(c, src, size)?;
        c.consume(size);
        k += run + 1;
    }
    Ok(c.pred[slot])
}

/// At an MCU boundary that must be followed by a marker: drops the padding bits of the current byte (whole bytes left
/// in the buffer are data where there must be none), skips fill bytes and returns the position of the marker's `FF`
/// and its code.
fn marker_after_padding(c: &mut Cursor, src: &mut Bytes) -> Result<(u64, u8), Fail> {
    c.consume(c.nbits % 8);
    if c.nbits > 0 {
        return Err(Fail::Error(c.current_byte()));
    }
    c.bits = 0;
    loop {
        let b = src.get(c.pos).ok_or(Fail::OutOfData)?;
        if b != 0xFF {
            return Err(Fail::Error(c.pos));
        }
        match src.get(c.pos + 1).ok_or(Fail::OutOfData)? {
            0xFF => c.pos += 1, // fill byte
            m => return Ok((c.pos, m)),
        }
    }
}

/// Decodes MCUs from `from`, reading only bytes in `[from.pos, limit)`.
pub(crate) fn decode(
    r: &mut Reader,
    frame: &Frame,
    from: Cursor,
    limit: u64,
    keep_dc: bool,
) -> Run {
    decode_until(r, frame, from, limit, keep_dc, u64::MAX)
}

/// [`decode`], stopping (as [`End::OutOfData`]) once MCU `stop_at` is reached.
pub(crate) fn decode_until(
    r: &mut Reader,
    frame: &Frame,
    from: Cursor,
    limit: u64,
    keep_dc: bool,
    stop_at: u64,
) -> Run {
    decode_rows(
        r,
        frame,
        from,
        limit,
        keep_dc,
        true,
        stop_at,
        &mut |_, _| true,
    )
}

/// [`decode_until`], calling `on_row` at every MCU-row boundary after `from` (before decoding the row's first MCU) with
/// the state there and the DC values kept so far; `false` stops the walk there (as [`End::OutOfData`]). Without
/// `keep_resume`, [`Run::resume`] stays empty.
#[allow(clippy::too_many_arguments)]
pub(crate) fn decode_rows(
    r: &mut Reader,
    frame: &Frame,
    from: Cursor,
    limit: u64,
    keep_dc: bool,
    keep_resume: bool,
    stop_at: u64,
    on_row: &mut dyn FnMut(&Cursor, &[i32]) -> bool,
) -> Run {
    let row = u64::from(frame.mcus_x).max(1);
    let total = frame.total_mcus();
    let interval = u64::from(frame.restart_interval);
    let mut src = Bytes {
        r,
        limit,
        buf: vec![0; CHUNK],
        start: 0,
        len: 0,
    };
    let from_mcu = from.mcu;
    let mut c = from;
    let points = if keep_resume {
        total
            .min(stop_at)
            .saturating_sub(from_mcu)
            .min(RESUME_POINTS as u64)
    } else {
        0
    };
    let mut resume: VecDeque<Cursor> = VecDeque::with_capacity(points as usize);
    let mut dc = Vec::new();
    let end = loop {
        if c.mcu >= total {
            break match marker_after_padding(&mut c, &mut src) {
                Ok((at, 0xD9)) => End::Eoi { at },
                Ok((at, _)) => End::Error { at },
                Err(Fail::Error(at)) => End::Error { at },
                Err(Fail::OutOfData) => End::OutOfData,
            };
        }
        if c.mcu >= stop_at {
            break End::OutOfData;
        }
        if c.mcu > from_mcu && c.mcu.is_multiple_of(row) && !on_row(&c, &dc) {
            break End::OutOfData;
        }
        if keep_resume {
            if resume.len() == RESUME_POINTS {
                resume.pop_front();
            }
            resume.push_back(c.clone());
        }
        let dc_len = dc.len();
        match mcu(
            &mut c,
            &mut src,
            frame,
            interval,
            keep_dc.then_some(&mut dc),
        ) {
            Ok(()) => c.mcu += 1,
            Err(fail) => {
                dc.truncate(dc_len);
                break match fail {
                    Fail::Error(at) => End::Error { at },
                    Fail::OutOfData => End::OutOfData,
                };
            }
        }
    };
    Run {
        mcus: c.mcu,
        pos: c.pos,
        end,
        resume: resume.into(),
        dc,
    }
}

/// MCU `c.mcu`, preceded by its restart marker when one is due.
fn mcu(
    c: &mut Cursor,
    src: &mut Bytes,
    frame: &Frame,
    interval: u64,
    mut dc: Option<&mut Vec<i32>>,
) -> Result<(), Fail> {
    if interval > 0 && c.mcu > 0 && c.mcu.is_multiple_of(interval) {
        let expected = 0xD0 + ((c.mcu / interval - 1) % 8) as u8;
        let (at, m) = marker_after_padding(c, src)?;
        if m != expected {
            return Err(Fail::Error(at));
        }
        c.pos = at + 2;
        c.pred = [0; 4];
    }
    for (slot, comp) in frame.components.iter().enumerate() {
        let (dc_t, ac_t) = (&frame.tables[comp.dc], &frame.tables[comp.ac]);
        for _ in 0..comp.h * comp.v {
            let value = block(c, src, dc_t, ac_t, slot)?;
            if comp.luma
                && let Some(dc) = dc.as_deref_mut()
            {
                dc.push(value);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::Health;
    use crate::formats::jpeg::{JPEG_MAX_LEN, walk_header, zune_health};
    use crate::formats::tests::Mem;
    use revenant_testkit as testkit;

    /// Where the entropy-coded data of `bytes` (a JPEG at offset 0) starts.
    fn scan_start(bytes: &[u8]) -> u64 {
        let src = Mem(bytes.to_vec());
        walk_header(&mut Reader::new(&src), 0, JPEG_MAX_LEN)
            .expect("a JPEG header")
            .scan_start
    }

    fn frame_of(bytes: &[u8]) -> Option<Frame> {
        let src = Mem(bytes.to_vec());
        let mut r = Reader::new(&src);
        let scan_start = walk_header(&mut r, 0, JPEG_MAX_LEN)?.scan_start;
        frame(&mut r, 0, scan_start)
    }

    /// Decodes the JPEG `bytes` (at offset 0) from its first MCU, reading below `limit`.
    fn decode_from_start(bytes: &[u8], limit: u64, keep_dc: bool) -> (Frame, Run) {
        let src = Mem(bytes.to_vec());
        let mut r = Reader::new(&src);
        let scan_start = scan_start(bytes);
        let f = frame(&mut r, 0, scan_start).expect("a supported frame");
        let run = decode(&mut r, &f, f.start(scan_start), limit, keep_dc);
        (f, run)
    }

    fn noise(len: usize, seed: u64) -> Vec<u8> {
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

    /// Offset of the first `FF marker` in the header of `j` (before SOS).
    fn segment(j: &[u8], marker: u8) -> usize {
        j.windows(2)
            .position(|w| w == [0xFF, marker])
            .expect("segment present")
    }

    fn without_segments(j: &[u8], marker: u8) -> Vec<u8> {
        let mut v = j.to_vec();
        while let Some(at) = v[..scan_start(&v) as usize]
            .windows(2)
            .position(|w| w == [0xFF, marker])
        {
            let len = usize::from(u16::from_be_bytes([v[at + 2], v[at + 3]]));
            v.drain(at..at + 2 + len);
        }
        v
    }

    #[test]
    fn counts_every_mcu_of_testkit_jpegs() {
        let cases = [
            testkit::jpeg(64, 64, 1, 0),
            testkit::jpeg(640, 480, 2, 0),
            testkit::jpeg(33, 17, 3, 0),
            testkit::jpeg_with_exif(320, 200, 4, &testkit::TestExif::default()),
            testkit::jpeg_flat_with_restarts(256, 128, 4),
            testkit::jpeg_flat_with_restarts(33, 17, 0),
        ];
        for (i, j) in cases.iter().enumerate() {
            let (f, run) = decode_from_start(j, j.len() as u64, false);
            assert_eq!(
                run.end,
                End::Eoi {
                    at: j.len() as u64 - 2
                },
                "case {i}"
            );
            assert_eq!(run.mcus, f.total_mcus(), "case {i}");
            assert!(f.total_mcus() > 0, "case {i}");
        }
        let flat = frame_of(&testkit::jpeg_flat_with_restarts(256, 128, 4)).unwrap();
        assert_eq!(
            (flat.mcus_x, flat.mcus_y, flat.restart_interval),
            (32, 16, 4)
        );
        assert_eq!(flat.luma_blocks_per_mcu(), (1, 1));
        let odd = frame_of(&testkit::jpeg_flat_with_restarts(33, 17, 0)).unwrap();
        assert_eq!((odd.mcus_x, odd.mcus_y, odd.total_mcus()), (5, 3, 15));
        // The flat JPEG is what zune-jpeg accepts strictly, as documented.
        let j = testkit::jpeg_flat_with_restarts(256, 128, 4);
        let src = Mem(j.clone());
        assert_eq!(
            zune_health(&mut Reader::new(&src), 0, j.len() as u64, true),
            Health::Intact
        );
    }

    #[test]
    fn truncated_data_runs_out() {
        let j = testkit::jpeg(640, 480, 1, 0);
        let s = scan_start(&j);
        let limit = s + (j.len() as u64 - s) / 2;
        let (f, run) = decode_from_start(&j, limit, false);
        assert_eq!(run.end, End::OutOfData);
        assert!(run.mcus > 0 && run.mcus < f.total_mcus(), "{}", run.mcus);
        // Every cursor handed back read only bytes below the limit.
        assert!(run.resume.iter().all(|c| c.pos <= limit));
    }

    #[test]
    fn corrupt_entropy_data_is_an_error() {
        let mut j = testkit::jpeg(640, 480, 2, 0);
        let off = j.len() / 2;
        j[off..off + 64].copy_from_slice(&noise(64, 11));
        let (f, run) = decode_from_start(&j, j.len() as u64, false);
        match run.end {
            End::Error { at } => assert!(at >= off as u64 && at < j.len() as u64, "at {at}"),
            end => panic!("{end:?}"),
        }
        assert!(run.mcus < f.total_mcus());
    }

    #[test]
    fn restart_markers_are_checked() {
        let j = testkit::jpeg_flat_with_restarts(256, 128, 4);
        let (f, run) = decode_from_start(&j, j.len() as u64, false);
        assert_eq!(
            run.end,
            End::Eoi {
                at: j.len() as u64 - 2
            }
        );
        assert_eq!(run.mcus, f.total_mcus());

        let s = scan_start(&j) as usize;
        let rst: Vec<usize> = (s..j.len() - 1)
            .filter(|&i| j[i] == 0xFF && (0xD0..=0xD7).contains(&j[i + 1]))
            .collect();
        assert_eq!(rst.len(), 127);
        let mut swapped = j.clone();
        swapped.swap(rst[1] + 1, rst[2] + 1);
        let (_, run) = decode_from_start(&swapped, j.len() as u64, false);
        assert_eq!(run.end, End::Error { at: rst[1] as u64 });
        assert_eq!(run.mcus, 8, "two intervals of 4 MCUs decode");

        // A marker missing altogether: the data of the next interval follows the padding.
        let mut missing = j.clone();
        missing.drain(rst[0]..rst[0] + 2);
        let (_, run) = decode_from_start(&missing, missing.len() as u64, false);
        assert_eq!(run.end, End::Error { at: rst[0] as u64 });
        assert_eq!(run.mcus, 4);
    }

    #[test]
    fn eoi_must_follow_the_last_mcu() {
        let j = testkit::jpeg_flat_with_restarts(64, 64, 0);
        let end = j.len() - 2;
        // EOI before the last MCU.
        let mut early = j[..end - 2].to_vec();
        early.extend_from_slice(&[0xFF, 0xD9]);
        let (_, run) = decode_from_start(&early, early.len() as u64, false);
        assert_eq!(
            run.end,
            End::Error {
                at: early.len() as u64 - 2
            }
        );
        // Data after the last MCU.
        let mut late = j[..end].to_vec();
        late.extend_from_slice(&[0x12, 0xFF, 0xD9]);
        let (_, run) = decode_from_start(&late, late.len() as u64, false);
        assert_eq!(run.end, End::Error { at: end as u64 });
        assert_eq!(run.mcus, 64);
        // Fill bytes before EOI are fine; EOI is the FF right before D9.
        let mut fill = j[..end].to_vec();
        fill.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xD9]);
        let (_, run) = decode_from_start(&fill, fill.len() as u64, false);
        assert_eq!(
            run.end,
            End::Eoi {
                at: fill.len() as u64 - 2
            }
        );
        // Every MCU but no EOI before the limit.
        let (f, run) = decode_from_start(&j, end as u64, false);
        assert_eq!((run.end, run.mcus), (End::OutOfData, f.total_mcus()));
    }

    #[test]
    fn resuming_from_a_cursor_gives_the_same_result() {
        for j in [
            testkit::jpeg(640, 480, 3, 0),
            testkit::jpeg_flat_with_restarts(256, 128, 4),
            testkit::jpeg_flat_with_restarts(256, 128, 3),
        ] {
            let len = j.len() as u64;
            let (f, full) = decode_from_start(&j, len, true);
            let (h, v) = f.luma_blocks_per_mcu();
            let per_mcu = (h * v) as usize;
            assert_eq!(full.dc.len() as u64, f.total_mcus() * per_mcu as u64);
            let points = (f.total_mcus() as usize).min(RESUME_POINTS);
            assert_eq!(full.resume.len(), points);
            for (i, c) in full.resume.iter().enumerate() {
                assert_eq!(c.mcu, f.total_mcus() - points as u64 + i as u64);
            }

            let src = Mem(j.clone());
            let mut r = Reader::new(&src);
            for k in [0, full.resume.len() / 2, full.resume.len() - 1] {
                let mid = full.resume[k].clone();
                let resumed = decode(&mut r, &f, mid.clone(), len, true);
                assert_eq!((resumed.end, resumed.mcus), (full.end, full.mcus));
                assert_eq!(resumed.dc, full.dc[mid.mcu as usize * per_mcu..]);
            }

            // The same state at other positions: the bytes before `by` removed, positions shifted.
            let mid = full.resume[full.resume.len() / 2].clone();
            let by = mid.pos - 4;
            let src = Mem(j[by as usize..].to_vec());
            let mut r = Reader::new(&src);
            let shifted = decode(&mut r, &f, mid.shifted_back(by), len - by, true);
            assert_eq!(shifted.end, End::Eoi { at: len - 2 - by });
            assert_eq!(shifted.mcus, full.mcus);
        }
    }

    #[test]
    fn unsupported_frames_are_none() {
        let j = testkit::jpeg(64, 64, 5, 0);
        assert!(frame_of(&j).is_some());
        let sof = segment(&j, 0xC0);
        for (marker, precision, supported) in [
            (0xC1, 8, true),
            (0xC2, 8, false),
            (0xC9, 8, false),
            (0xC1, 12, false),
        ] {
            let mut v = j.clone();
            v[sof + 1] = marker;
            v[sof + 4] = precision;
            assert_eq!(frame_of(&v).is_some(), supported, "{marker:#x}/{precision}");
        }
        let no_dht = without_segments(&j, 0xC4);
        assert!(walk_header(&mut Reader::new(&Mem(no_dht.clone())), 0, JPEG_MAX_LEN).is_some());
        assert!(frame_of(&no_dht).is_none());
    }

    #[test]
    fn hostile_headers_are_rejected_or_bounded() {
        let j = testkit::jpeg_flat_with_restarts(64, 64, 0);
        let sof = segment(&j, 0xC0);
        let dht = segment(&j, 0xC4);
        // Three codes of length 1 cannot exist.
        let mut overflow = j.clone();
        overflow[dht + 5..dht + 14].copy_from_slice(&[3, 0, 3, 1, 1, 1, 1, 1, 1]);
        assert!(frame_of(&overflow).is_none());
        // Table class or id out of range.
        let mut class = j.clone();
        class[dht + 4] = 0x24;
        assert!(frame_of(&class).is_none());
        // Sampling factors over 2, zero, absurd component counts, zero size.
        for (at, value) in [
            (sof + 11, 0x44),
            (sof + 11, 0x10),
            (sof + 9, 200),
            (sof + 9, 0),
            (sof + 5, 0),
        ] {
            let mut v = j.clone();
            v[at] = value;
            if at == sof + 5 {
                v[at + 1] = 0;
            }
            assert!(frame_of(&v).is_none(), "byte {} = {value}", at - sof);
        }
        // Too many pixels: 65535 x 65535.
        let mut huge = j.clone();
        huge[sof + 5..sof + 9].copy_from_slice(&[0xFF; 4]);
        assert!(frame_of(&huge).is_none());
        // DRI 0 means no restart markers.
        let dri0 = [&j[..sof], &[0xFF, 0xDD, 0x00, 0x04, 0x00, 0x00], &j[sof..]].concat();
        let (f, run) = decode_from_start(&dri0, dri0.len() as u64, false);
        assert_eq!(f.restart_interval, 0);
        assert_eq!(
            run.end,
            End::Eoi {
                at: dri0.len() as u64 - 2
            }
        );
        // Noise after a valid header ends in bounded time with an error.
        let big = testkit::jpeg(1024, 1024, 6, 0);
        let s = scan_start(&big) as usize;
        let mut v = big[..s].to_vec();
        v.extend(noise(4 << 20, 3));
        let t = std::time::Instant::now();
        let (_, run) = decode_from_start(&v, v.len() as u64, true);
        assert!(matches!(run.end, End::Error { .. }), "{:?}", run.end);
        assert!(t.elapsed().as_secs() < 5);
    }

    #[test]
    fn test_frame_has_the_geometry_and_decodes_nothing() {
        let f = Frame::for_test(64, 48, (2, 1));
        assert_eq!((f.mcus_x, f.mcus_y, f.total_mcus()), (64, 48, 3072));
        assert_eq!(f.luma_blocks_per_mcu(), (2, 1));
        // No codes: the first block is an error, never a panic or a loop.
        let src = Mem(vec![0x55; 4096]);
        let run = decode(&mut Reader::new(&src), &f, f.start(0), 4096, true);
        assert_eq!((run.mcus, run.end), (0, End::Error { at: 0 }));
        assert!(run.dc.is_empty());
    }

    #[test]
    fn agrees_with_zune_on_testkit_sizes() {
        // Every size class of edge MCUs (partial last column/row) and a few seeds: zune-jpeg decodes each strictly, and
        // we must count every MCU and stop exactly at its EOI.
        for (i, (w, h)) in [
            (1, 1),
            (7, 9),
            (8, 8),
            (15, 16),
            (17, 31),
            (100, 3),
            (3, 100),
            (257, 129),
        ]
        .into_iter()
        .enumerate()
        {
            let j = testkit::jpeg(w, h, 40 + i as u64, 0);
            let src = Mem(j.clone());
            assert_eq!(
                zune_health(&mut Reader::new(&src), 0, j.len() as u64, true),
                Health::Intact
            );
            let (f, run) = decode_from_start(&j, j.len() as u64, true);
            assert_eq!(
                run.end,
                End::Eoi {
                    at: j.len() as u64 - 2
                },
                "{w}x{h}"
            );
            assert_eq!(run.mcus, f.total_mcus(), "{w}x{h}");
            let (bh, bv) = f.luma_blocks_per_mcu();
            assert_eq!(run.dc.len() as u64, f.total_mcus() * u64::from(bh * bv));
        }
    }

    /// Opt-in, like `tests/real_samples.rs`: every JPEG embedded in the real camera files (previews, thumbnails) that
    /// zune-jpeg decodes strictly and whose frame we cover must decode to its EOI with every MCU.
    /// `REVENANT_SAMPLES=samples/cache cargo test -p revenant-core --lib real_sample_jpegs -- --ignored --nocapture`
    #[test]
    #[ignore = "needs REVENANT_SAMPLES (real camera files)"]
    fn real_sample_jpegs_decode_to_their_eoi() {
        use crate::formats::{Format, Jpeg};
        let dir = std::env::var("REVENANT_SAMPLES").expect("REVENANT_SAMPLES");
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(dir);
        let (mut checked, mut uncovered) = (0, 0);
        let mut entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        entries.sort();
        for path in entries {
            let bytes = std::fs::read(&path).unwrap();
            let src = Mem(bytes.clone());
            for at in (0..bytes.len().saturating_sub(3))
                .filter(|&i| bytes[i..i + 3] == [0xFF, 0xD8, 0xFF])
            {
                let Some(m) = Jpeg.measure(&src, at as u64) else {
                    continue;
                };
                if m.health != Health::Intact {
                    continue;
                }
                let mut r = Reader::new(&src);
                let scan_start = walk_header(&mut r, at as u64, JPEG_MAX_LEN)
                    .unwrap()
                    .scan_start;
                let Some(f) = frame(&mut r, at as u64, scan_start) else {
                    uncovered += 1;
                    println!(
                        "{}: JPEG at {at} ({} bytes) not covered",
                        path.display(),
                        m.len
                    );
                    continue;
                };
                let run = decode(&mut r, &f, f.start(scan_start), at as u64 + m.len, false);
                println!(
                    "{}: JPEG at {at}, {} bytes, {}x{} MCUs of {:?} blocks, DRI {}: {:?}, {} MCUs",
                    path.display(),
                    m.len,
                    f.mcus_x,
                    f.mcus_y,
                    f.luma_blocks_per_mcu(),
                    f.restart_interval,
                    run.end,
                    run.mcus
                );
                assert_eq!(
                    run.end,
                    End::Eoi {
                        at: at as u64 + m.len - 2
                    },
                    "{}",
                    path.display()
                );
                assert_eq!(run.mcus, f.total_mcus(), "{}", path.display());
                checked += 1;
            }
        }
        println!("checked {checked}, not covered {uncovered}");
        assert!(checked > 0);
    }

    #[test]
    fn subsampled_geometry_is_counted_exactly() {
        // 33×17 with 3 interleaved components: MCUs of 8·Hmax × 8·Vmax pixels, h·v luma blocks then Cb and Cr.
        for (luma, interval, mcus) in [
            ((2, 2), 0, (3, 2)),
            ((2, 1), 0, (3, 3)),
            ((1, 2), 0, (5, 2)),
            ((2, 2), 2, (3, 2)),
            ((2, 1), 4, (3, 3)),
        ] {
            let j = testkit::jpeg_flat_ycbcr(33, 17, luma, interval);
            let src = Mem(j.clone());
            assert_eq!(
                zune_health(&mut Reader::new(&src), 0, j.len() as u64, true),
                Health::Intact,
                "{luma:?}/{interval}"
            );
            let (f, run) = decode_from_start(&j, j.len() as u64, true);
            let hv = (u32::from(luma.0), u32::from(luma.1));
            assert_eq!((f.mcus_x, f.mcus_y), mcus, "{luma:?}/{interval}");
            assert_eq!(f.luma_blocks_per_mcu(), hv, "{luma:?}/{interval}");
            assert_eq!(
                run.end,
                End::Eoi {
                    at: j.len() as u64 - 2
                },
                "{luma:?}/{interval}"
            );
            assert_eq!(run.mcus, f.total_mcus());
            assert_eq!(run.dc.len() as u64, f.total_mcus() * u64::from(hv.0 * hv.1));
        }
    }

    /// Baseline 8×8 grayscale JPEG with the given DC and AC tables (`(counts, symbols)`) and entropy-coded `data`.
    fn with_tables(dc: (&[u8; 16], &[u8]), ac: (&[u8; 16], &[u8]), data: &[u8]) -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8, 0xFF, 0xDB, 0x00, 0x43, 0x00];
        v.extend_from_slice(&[1; 64]);
        v.extend_from_slice(&[
            0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x00, 0x08, 0x00, 0x08, 0x01, 0x01, 0x11, 0x00,
        ]);
        for (class, (counts, vals)) in [(0x00, dc), (0x10, ac)] {
            v.extend_from_slice(&[0xFF, 0xC4]);
            v.extend_from_slice(&((19 + vals.len()) as u16).to_be_bytes());
            v.push(class);
            v.extend_from_slice(counts);
            v.extend_from_slice(vals);
        }
        v.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3F, 0x00]);
        v.extend_from_slice(data);
        v.extend_from_slice(&[0xFF, 0xD9]);
        v
    }

    #[test]
    fn impossible_symbols_are_errors() {
        // Two codes of length 1 in each table: `0` and `1`.
        let two = {
            let mut c = [0u8; 16];
            c[0] = 2;
            c
        };
        // Control: DC `0` (category 0), AC `0` (EOB), 1-bit padding.
        let ok = with_tables((&two, &[0, 12]), (&two, &[0x00, 0x0B]), &[0b0011_1111]);
        let (_, run) = decode_from_start(&ok, ok.len() as u64, false);
        assert_eq!(
            run.end,
            End::Eoi {
                at: ok.len() as u64 - 2
            }
        );
        for (what, dc_vals, ac_vals, byte) in [
            ("DC category 12", [0, 12], [0x00, 0x0B], 0b1000_0000),
            ("AC size 11", [0, 12], [0x00, 0x0B], 0b0100_0000),
            (
                "run/0 other than EOB and ZRL",
                [0, 12],
                [0x00, 0x10],
                0b0100_0000,
            ),
        ] {
            let j = with_tables((&two, &dc_vals), (&two, &ac_vals), &[byte]);
            let (_, run) = decode_from_start(&j, j.len() as u64, false);
            assert_eq!(run.end, End::Error { at: scan_start(&j) }, "{what}");
            assert_eq!(run.mcus, 0, "{what}");
        }
    }
}
