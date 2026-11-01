//! Structure check for lossless JPEG (SOF3), written from ITU-T T.81 Annex H (and Annex B for the marker segments, Annex C
//! for the Huffman codes). It is how a RAW's tiles and strips (Canon CR2 slices, lossless DNG tiles) are
//! told apart: the walk reads the difference codes only (a Huffman category SSSS of 0–16 and SSSS extra bits; SSSS 16 is
//! the difference 32768 with none), counts the samples and checks where the data ends. Predictions are never computed,
//! because the reconstructed samples are not needed to know whether the bytes form this frame.
//!
//! Only the Huffman table builder and the byte source are shared with the baseline decoder in `jpeg_entropy`, which keeps
//! rejecting SSSS over 11. Every loop is bounded by the declared sample count and by the bytes below the caller's limit.

use std::collections::VecDeque;

use super::Reader;
use super::jpeg::is_sof;
use super::jpeg_entropy::{Bytes, End, Huffman, LOOKUP_BITS, RESUME_POINTS};

/// Marker segments read before the SOS; more than this is not a lossless JPEG header.
const MAX_SEGMENTS: usize = 64;
/// Samples (width × height × components) a frame may declare; beyond this nothing is checked.
const MAX_SAMPLES: u64 = 1 << 32;

/// What [`decode_lossless`] needs from a lossless JPEG's header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LosslessFrame {
    pub width: u32,
    pub height: u32,
    pub components: u8,
    pub precision: u8,
    /// Predictor selection (1–7) of the scan.
    pub predictor: u8,
    /// MCUs (pixels) between restart markers; 0 means none.
    pub restart_interval: u16,
    /// DC table of each scan component, in scan order.
    tables: Vec<Huffman>,
}

impl LosslessFrame {
    /// Difference codes in the scan: width × height × components.
    pub(crate) fn samples(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height) * u64::from(self.components)
    }

    /// The state before the first sample, for entropy-coded data starting at `scan_start`.
    pub(crate) fn start(&self, scan_start: u64) -> LlCursor {
        LlCursor {
            pos: scan_start,
            sample: 0,
            bits: 0,
            nbits: 0,
            byte_pos: [0; 8],
            loaded: 0,
        }
    }
}

/// From the JPEG's header, between `start` (its SOI) and the end of its first SOS, reading only bytes below `limit`.
/// `None` unless: one SOF3 with 2–16 bits of precision and 1–4 components of sampling 1×1, a Huffman DC table for every
/// component, a single scan holding all of them with predictor 1–7, no successive approximation and a point transform
/// below the precision, and no arithmetic coding. Returns the frame and the position where the entropy-coded data starts.
pub(crate) fn lossless_frame(
    r: &mut Reader,
    start: u64,
    limit: u64,
) -> Option<(LosslessFrame, u64)> {
    if r.byte(start)? != 0xFF || r.byte(start.checked_add(1)?)? != 0xD8 {
        return None;
    }
    let mut pos = start.checked_add(2)?;
    let mut tables: [Option<Huffman>; 4] = Default::default();
    let mut sof: Option<(u32, u32, u8, Vec<u8>)> = None;
    let mut restart_interval = 0u16;
    for _ in 0..MAX_SEGMENTS {
        if r.byte(pos)? != 0xFF {
            return None;
        }
        let marker = r.byte(pos.checked_add(1)?)?;
        if marker == 0xFF {
            pos = pos.checked_add(1)?; // fill byte
            continue;
        }
        let len = u64::from(r.u16(pos.checked_add(2)?)?);
        let end = pos.checked_add(2)?.checked_add(len)?;
        if len < 2 || end > limit {
            return None;
        }
        let mut payload = vec![0u8; len as usize - 2];
        if r.read(pos + 4, &mut payload) != payload.len() {
            return None;
        }
        match marker {
            0xC3 if sof.is_none() => sof = Some(parse_sof3(&payload)?),
            m if is_sof(m) => return None, // another coding process, or a second frame
            0xCC => return None,           // DAC: arithmetic coding
            0xC4 => parse_dc_tables(&payload, &mut tables)?,
            0xDD => {
                if payload.len() != 2 {
                    return None;
                }
                restart_interval = u16::from_be_bytes([payload[0], payload[1]]);
            }
            0xDA => {
                let (width, height, precision, ids) = sof?;
                let (predictor, scan_tables) = parse_sos(&payload, precision, &ids, &tables)?;
                let components = ids.len() as u8;
                let samples = u64::from(width) * u64::from(height) * u64::from(components);
                if samples > MAX_SAMPLES {
                    return None;
                }
                let frame = LosslessFrame {
                    width,
                    height,
                    components,
                    precision,
                    predictor,
                    restart_interval,
                    tables: scan_tables,
                };
                return Some((frame, end));
            }
            // SOI, EOI and the standalone markers have no place in a header; anything else is skipped.
            0xD8 | 0xD9 | 0x01 | 0xD0..=0xD7 => return None,
            _ => {}
        }
        pos = end;
    }
    None
}

/// SOF3 payload: precision, height, width, components. Returns `(width, height, precision, component ids)`.
fn parse_sof3(p: &[u8]) -> Option<(u32, u32, u8, Vec<u8>)> {
    let (&precision, rest) = p.split_first()?;
    if !(2..=16).contains(&precision) || rest.len() < 5 {
        return None;
    }
    let height = u32::from(u16::from_be_bytes([rest[0], rest[1]]));
    let width = u32::from(u16::from_be_bytes([rest[2], rest[3]]));
    let n = usize::from(rest[4]);
    // A height of 0 would be defined by a DNL segment later: not supported.
    if !(1..=4).contains(&n) || rest.len() != 5 + 3 * n || width == 0 || height == 0 {
        return None;
    }
    let mut ids: Vec<u8> = Vec::with_capacity(n);
    for c in rest[5..].as_chunks::<3>().0 {
        if c[1] != 0x11 || ids.contains(&c[0]) {
            return None;
        }
        ids.push(c[0]);
    }
    Some((width, height, precision, ids))
}

/// One DHT payload into `tables` (DC tables, class 0, ids 0–3); AC tables are checked for shape and ignored. A DC table
/// holding a symbol over 16 is not a lossless one.
fn parse_dc_tables(mut p: &[u8], tables: &mut [Option<Huffman>; 4]) -> Option<()> {
    while let Some((&tc_th, rest)) = p.split_first() {
        let (class, id) = (tc_th >> 4, usize::from(tc_th & 0x0F));
        if class > 1 || id > 3 || rest.len() < 16 {
            return None;
        }
        let counts: &[u8; 16] = rest[..16].try_into().ok()?;
        let n: usize = counts.iter().map(|&c| usize::from(c)).sum();
        let vals = rest.get(16..16usize.checked_add(n)?)?;
        let table = Huffman::new(counts, vals)?;
        if class == 0 {
            if vals.iter().any(|&v| v > 16) {
                return None;
            }
            tables[id] = Some(table);
        }
        p = &rest[16 + n..];
    }
    Some(())
}

/// The SOS payload: every frame component once, a defined DC table each, predictor 1–7, Se 0, Ah 0, Al below the
/// precision. Returns the predictor and the tables in scan order.
fn parse_sos(
    p: &[u8],
    precision: u8,
    ids: &[u8],
    tables: &[Option<Huffman>; 4],
) -> Option<(u8, Vec<Huffman>)> {
    let (&ns, rest) = p.split_first()?;
    let ns = usize::from(ns);
    if ns != ids.len() || rest.len() != 2 * ns + 3 {
        return None;
    }
    let (ss, se, a) = (rest[2 * ns], rest[2 * ns + 1], rest[2 * ns + 2]);
    if !(1..=7).contains(&ss) || se != 0 || a >> 4 != 0 || (a & 0x0F) >= precision {
        return None;
    }
    let mut seen: Vec<u8> = Vec::with_capacity(ns);
    let mut scan_tables = Vec::with_capacity(ns);
    for sel in rest[..2 * ns].as_chunks::<2>().0 {
        if !ids.contains(&sel[0]) || seen.contains(&sel[0]) {
            return None;
        }
        seen.push(sel[0]);
        scan_tables.push(tables.get(usize::from(sel[1] >> 4))?.clone()?);
    }
    Some((ss, scan_tables))
}

/// The decoder's state at a row boundary (or anywhere in a walk): where the bytes are, what of them is buffered.
#[derive(Clone, Debug)]
pub(crate) struct LlCursor {
    /// First byte not yet consumed into the bit buffer.
    pub pos: u64,
    /// Index of the next difference code (sample) to decode.
    pub sample: u64,
    /// Buffered bits, most significant first; the bits past `nbits` are zero.
    bits: u64,
    nbits: u32,
    /// Source positions of the last 8 bytes loaded into `bits` (a ring indexed by `loaded`), so an error can name the
    /// byte its code started in.
    byte_pos: [u64; 8],
    loaded: u8,
}

impl LlCursor {
    /// The same state, every position minus `by` (for decoding the same bytes through a view that starts `by` later);
    /// `None` when the view would start after the cursor.
    #[allow(dead_code)] // No walk starts a view inside a tile yet: tested, kept for one that does.
    pub(crate) fn shifted_back(&self, by: u64) -> Option<LlCursor> {
        let mut c = self.clone();
        c.pos = c.pos.checked_sub(by)?;
        for p in &mut c.byte_pos {
            *p = p.saturating_sub(by);
        }
        Some(c)
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

/// Result of [`decode_lossless`].
pub(crate) struct LlRun {
    /// How the walk ended; `Eoi { at }` only for a complete tile whose `FF D9` ends exactly at the limit.
    pub end: End,
    /// Difference codes decoded (the index of the first one not decoded).
    #[allow(dead_code)] // Read by the unit tests; callers go by `end`.
    pub samples: u64,
    /// Cursors at the last [`RESUME_POINTS`] row boundaries reached, oldest first.
    pub resume: Vec<LlCursor>,
}

/// Why the bit buffer could not be filled further.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stop {
    Full,
    /// A marker (`FF` followed by anything but `00`) at the cursor's `pos`.
    Marker,
    OutOfData,
}

enum Fail {
    Error(u64),
    OutOfData,
}

/// Loads whole bytes into the bit buffer until it holds more than 56 bits, a marker or the end of the data.
fn fill(c: &mut LlCursor, src: &mut Bytes) -> Stop {
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

fn short(c: &LlCursor, stop: Stop) -> Fail {
    match stop {
        Stop::OutOfData => Fail::OutOfData,
        // A marker inside the data (an early EOI, a stray RST or any other marker).
        Stop::Marker | Stop::Full => Fail::Error(c.pos),
    }
}

/// One Huffman symbol (T.81 F.2.2.3).
fn symbol(c: &mut LlCursor, src: &mut Bytes, t: &Huffman) -> Result<u8, Fail> {
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
            let index = code + t.valoff[len as usize];
            c.consume(len);
            return t
                .vals
                .get(index as usize)
                .copied()
                .ok_or(Fail::Error(c.current_byte()));
        }
    }
    if c.nbits < 16 {
        // The bits that exist may still begin a valid code.
        return Err(short(c, stop));
    }
    Err(Fail::Error(c.current_byte()))
}

/// One difference: the category and its extra bits (none for category 16).
fn difference(c: &mut LlCursor, src: &mut Bytes, t: &Huffman) -> Result<(), Fail> {
    let at = c.current_byte();
    let ssss = u32::from(symbol(c, src, t)?);
    if ssss > 16 {
        return Err(Fail::Error(at));
    }
    if ssss > 0 && ssss < 16 {
        if c.nbits < ssss {
            let stop = fill(c, src);
            if c.nbits < ssss {
                return Err(short(c, stop));
            }
        }
        c.consume(ssss);
    }
    Ok(())
}

/// At a boundary that must be followed by a marker: checks and drops the 1-bit padding of the current byte (whole bytes left in the
/// buffer are data where there must be none), skips fill bytes and returns the position of the marker's `FF` and its code.
fn marker_after_padding(c: &mut LlCursor, src: &mut Bytes) -> Result<(u64, u8), Fail> {
    let pad = c.nbits % 8;
    // The padding of the last byte is 1-bits (T.81 F.1.2.3).
    if pad > 0 && c.bits >> (64 - pad) != (1 << pad) - 1 {
        return Err(Fail::Error(c.current_byte()));
    }
    c.consume(pad);
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

/// Walks difference codes from `from`, reading only bytes in `[from.pos, limit)`, `limit` being the tile's declared end
/// (TileOffsets + TileByteCounts). The result is `End::Eoi` only when exactly width × height × components samples are
/// decoded, then padding, optional fill bytes and an `FF D9` that ends exactly at `limit`: Huffman streams
/// resynchronize, so a sample count alone can be reached by chance on a neighbour's tail. Restart markers are checked
/// when the frame has a restart interval, and padding bits must be 1-bits. A cursor is kept at every row of samples (at
/// most [`RESUME_POINTS`]).
///
/// `Eoi` is evidence of structure, not proof of identity: the walk reads no pixels, and Huffman codes resynchronize, so
/// a piece of another tile of the same frame (same header, same table) passes a few percent of the time (under 1% in the
/// fixed measurement of the unit tests, more for shorter or smoother tiles). A caller must not treat it as a proof.
pub(crate) fn decode_lossless(
    r: &mut Reader,
    f: &LosslessFrame,
    from: LlCursor,
    limit: u64,
) -> LlRun {
    let mut src = Bytes::new(r, limit);
    let total = f.samples();
    let ns = u64::from(f.components);
    let row = (u64::from(f.width) * ns).max(1);
    let interval = u64::from(f.restart_interval) * ns;
    let mut resume: VecDeque<LlCursor> = VecDeque::new();
    let mut c = from;
    let end = loop {
        if c.sample >= total {
            break match marker_after_padding(&mut c, &mut src) {
                Ok((at, 0xD9)) if at.checked_add(2) == Some(limit) => End::Eoi { at },
                Ok((at, _)) => End::Error { at },
                Err(Fail::Error(at)) => End::Error { at },
                Err(Fail::OutOfData) => End::OutOfData,
            };
        }
        if c.sample.is_multiple_of(row) {
            if resume.len() == RESUME_POINTS {
                resume.pop_front();
            }
            resume.push_back(c.clone());
        }
        if interval > 0 && c.sample > 0 && c.sample.is_multiple_of(interval) {
            let expected = 0xD0 + ((c.sample / interval - 1) % 8) as u8;
            match marker_after_padding(&mut c, &mut src) {
                Ok((at, m)) if m == expected => c.pos = at + 2,
                Ok((at, _)) => break End::Error { at },
                Err(Fail::Error(at)) => break End::Error { at },
                Err(Fail::OutOfData) => break End::OutOfData,
            }
        }
        let table = &f.tables[(c.sample % ns) as usize];
        match difference(&mut c, &mut src, table) {
            Ok(()) => c.sample += 1,
            Err(Fail::Error(at)) => break End::Error { at },
            Err(Fail::OutOfData) => break End::OutOfData,
        }
    };
    LlRun {
        end,
        samples: c.sample,
        resume: resume.into(),
    }
}

/// Whether the lossless-JPEG tiles `tiles` (`(absolute offset, length)`, in file order) of a RAW all decode to their
/// declared ends ([`decode_lossless`]: the exact sample count, `FF D9` ending exactly there). Tiles whose first frame is
/// not one [`lossless_frame`] covers cannot be checked: `true`. A tile of another file of the same frame can pass (its
/// `Eoi` is evidence, not proof), so `true` is no proof of the data; `false` means some tile is not what the structure
/// declares.
pub(crate) fn tiles_decode(r: &mut Reader, tiles: &[(u64, u64)]) -> bool {
    let Some(&(o, l)) = tiles.first() else {
        return true;
    };
    if lossless_frame(r, o, o.saturating_add(l)).is_none() {
        return true;
    }
    tiles.iter().all(|&(o, l)| {
        let end = o.saturating_add(l);
        !super::cancelled()
            && lossless_frame(r, o, end).is_some_and(|(f, scan)| {
                matches!(
                    decode_lossless(r, &f, f.start(scan), end).end,
                    End::Eoi { .. }
                )
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::tests::Mem;
    use revenant_testkit as testkit;

    fn parse(bytes: &[u8]) -> Option<(LosslessFrame, u64)> {
        let src = Mem(bytes.to_vec());
        lossless_frame(&mut Reader::new(&src), 0, bytes.len() as u64)
    }

    /// Parses `bytes` (a JPEG at offset 0) and decodes it from its first sample, reading below `limit`.
    fn decode(bytes: &[u8], limit: u64) -> (LosslessFrame, LlRun) {
        let (f, scan) = parse(bytes).expect("a lossless frame");
        let src = Mem(bytes.to_vec());
        let run = decode_lossless(&mut Reader::new(&src), &f, f.start(scan), limit);
        (f, run)
    }

    fn eoi(len: usize) -> End {
        End::Eoi { at: len as u64 - 2 }
    }

    fn sof_at(j: &[u8]) -> usize {
        j.windows(2).position(|w| w == [0xFF, 0xC3]).unwrap()
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

    #[test]
    fn lossless_tile_decodes_whole() {
        for (w, h, n, p) in [
            (256, 256, 1u8, 14u8),
            (33, 17, 2, 12),
            (20, 20, 4, 16),
            (9, 9, 3, 8),
        ] {
            let j = testkit::lossless_jpeg(w, h, n, p, 3);
            let (f, run) = decode(&j, j.len() as u64);
            assert_eq!(
                (f.width, f.height, f.components, f.precision, f.predictor),
                (w, h, n, p, 1)
            );
            assert_eq!(run.end, eoi(j.len()), "{w}x{h}x{n}");
            assert_eq!(run.samples, u64::from(w * h) * u64::from(n));
            assert_eq!(run.samples, f.samples());
        }
        let j = testkit::lossless_jpeg(256, 256, 1, 14, 3);
        assert_eq!(decode(&j, j.len() as u64).1.samples, 65_536);
    }

    #[test]
    fn lossless_count_must_be_exact() {
        let j = testkit::lossless_jpeg(256, 256, 1, 14, 3);
        let at = sof_at(&j) + 5; // the height field
        for height in [257u16, 255] {
            let mut v = j.clone();
            v[at..at + 2].copy_from_slice(&height.to_be_bytes());
            let (_, run) = decode(&v, v.len() as u64);
            assert!(
                !matches!(run.end, End::Eoi { .. }),
                "height {height}: {:?}",
                run.end
            );
            if height == 255 {
                // Data after the last sample.
                assert!(matches!(run.end, End::Error { .. }), "{:?}", run.end);
            }
        }
    }

    #[test]
    fn lossless_eoi_must_sit_at_the_declared_tile_end() {
        let j = testkit::lossless_jpeg(64, 64, 1, 14, 3);
        let len = j.len();
        let (_, run) = decode(&j, len as u64);
        assert_eq!(run.end, eoi(len));
        // The marker is early: the declared end lies 64 bytes past it.
        let mut longer = j.clone();
        longer.extend_from_slice(&[0u8; 64]);
        let (_, run) = decode(&longer, len as u64 + 64);
        assert!(!matches!(run.end, End::Eoi { .. }), "{:?}", run.end);
        // The declared end cuts the marker.
        let (_, run) = decode(&j, len as u64 - 2);
        assert!(!matches!(run.end, End::Eoi { .. }), "{:?}", run.end);
        let (_, run) = decode(&j, len as u64 - 1);
        assert!(!matches!(run.end, End::Eoi { .. }), "{:?}", run.end);
        // The sample count is exact but the FF D9 sits 8 bytes before the declared end (fill bytes would not move it).
        let mut early = j[..len - 2].to_vec();
        early.extend_from_slice(&[0xFF, 0xD9]);
        early.extend_from_slice(&[0u8; 8]);
        let (_, run) = decode(&early, early.len() as u64);
        assert_eq!(run.samples, 64 * 64);
        assert_eq!(run.end, End::Error { at: len as u64 - 2 });
        // Fill bytes before the marker are fine as long as it ends at the declared end.
        let mut filled = j[..len - 2].to_vec();
        filled.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xD9]);
        let (_, run) = decode(&filled, filled.len() as u64);
        assert_eq!(run.end, eoi(filled.len()));
    }

    #[test]
    fn lossless_cut_tile_breaks_near_the_cut() {
        let j = testkit::lossless_jpeg(256, 256, 1, 14, 3);
        let (_, scan) = parse(&j).unwrap();
        let cut = scan as usize + (j.len() - scan as usize) * 6 / 10;
        let mut v = j[..cut].to_vec();
        v.extend(noise(j.len() - cut, 9));
        let (f, run) = decode(&v, v.len() as u64);
        match run.end {
            End::Error { at } => assert!(at + 64 >= cut as u64, "at {at}, cut {cut}"),
            end => panic!("{end:?}"),
        }
        assert!(run.samples < f.samples());
    }

    /// A 1x1 tile of `precision` bits with the header of `testkit::lossless_jpeg` and these exact scan bytes (no EOI).
    fn tiny_tile(precision: u8, scan: &[u8]) -> Vec<u8> {
        let j = testkit::lossless_jpeg(1, 1, 1, precision, 0);
        let (_, start) = parse(&j).unwrap();
        let mut v = j[..start as usize].to_vec();
        v.extend_from_slice(scan);
        v.extend_from_slice(&[0xFF, 0xD9]);
        v
    }

    #[test]
    fn padding_bits_must_be_ones() {
        // One sample of category 0: the code 000, then five padding bits (T.81 F.1.2.3: 1-bits).
        let good = tiny_tile(8, &[0b0001_1111]);
        let (_, run) = decode(&good, good.len() as u64);
        assert_eq!(run.end, eoi(good.len()));
        for pad in [0b0001_1110u8, 0b0001_0111, 0b0000_0000] {
            let bad = tiny_tile(8, &[pad]);
            let (_, run) = decode(&bad, bad.len() as u64);
            assert_eq!(run.samples, 1, "{pad:#b}");
            assert_eq!(
                run.end,
                End::Error {
                    at: parse(&bad).unwrap().1
                },
                "{pad:#b}"
            );
        }
    }

    #[test]
    fn category_16_has_no_extra_bits() {
        // Category 16 is the code 1111111110 (10 bits) and the difference 32768, with no extra bits: six 1-bits pad the
        // second byte. FF is stuffed with 00.
        let t = tiny_tile(16, &[0xFF, 0x00, 0b1011_1111]);
        let (f, run) = decode(&t, t.len() as u64);
        assert_eq!(f.precision, 16);
        assert_eq!((run.samples, run.end), (1, eoi(t.len())));
        // Category 15 (111111110) does take its 15 extra bits: 24 bits, no padding.
        let t = tiny_tile(16, &[0xFF, 0x00, 0x00, 0x01]);
        let (_, run) = decode(&t, t.len() as u64);
        assert_eq!((run.samples, run.end), (1, eoi(t.len())));
    }

    #[test]
    fn foreign_same_frame_pieces_rarely_pass() {
        // A tile whose tail (or middle) belongs to another tile of the same frame is decoded as a stream of valid
        // codes more often than one would hope: Huffman codes resynchronize. The guarantee is statistical, so it is
        // measured over fixed pairs and bounded; an `Eoi` is evidence of structure, never proof of identity.
        let (mut cases, mut passed) = (0u32, 0u32);
        for seed in 0..40u64 {
            let a = testkit::lossless_jpeg(128, 128, 1, 14, seed);
            let b = testkit::lossless_jpeg(128, 128, 1, 14, seed + 100);
            let (_, scan) = parse(&a).unwrap();
            let shortest = a.len().min(b.len());
            for k in 1..8 {
                let c = scan as usize + (shortest - scan as usize) * k / 8;
                for (x, y) in [(&a, &b), (&b, &a)] {
                    let mut v = x[..c].to_vec();
                    v.extend_from_slice(&y[c..]);
                    let (_, run) = decode(&v, v.len() as u64);
                    cases += 1;
                    passed += u32::from(matches!(run.end, End::Eoi { .. }));
                }
            }
        }
        assert_eq!(cases, 560);
        assert!(
            passed * 100 < cases * 5,
            "{passed} of {cases} spliced tiles passed"
        );
    }

    #[test]
    fn spliced_tiles_of_the_wrong_length_never_pass() {
        // Deterministic rejections: a tail from a tile of another size cannot end where the declared end is.
        let a = testkit::lossless_jpeg(64, 64, 1, 14, 3);
        let b = testkit::lossless_jpeg(64, 65, 1, 14, 3);
        let (_, scan) = parse(&a).unwrap();
        let c = scan as usize + (a.len() - scan as usize) / 2;
        let mut v = a[..c].to_vec();
        v.extend_from_slice(&b[c..]);
        let (_, run) = decode(&v, v.len() as u64);
        assert!(!matches!(run.end, End::Eoi { .. }), "{:?}", run.end);
    }

    #[test]
    fn a_cursor_cannot_be_shifted_before_the_start() {
        let j = testkit::lossless_jpeg(16, 16, 1, 12, 3);
        let (f, scan) = parse(&j).unwrap();
        let c = f.start(scan);
        assert!(c.shifted_back(scan).is_some());
        assert!(c.shifted_back(scan + 1).is_none());
    }

    #[test]
    fn lossless_resumes_across_a_view() {
        let j = testkit::lossless_jpeg(128, 96, 2, 14, 3);
        let len = j.len() as u64;
        let (f, whole) = decode(&j, len);
        assert_eq!(whole.end, eoi(j.len()));
        let (_, scan) = parse(&j).unwrap();
        let c = scan + (len - scan) / 2;
        // First piece: the source ends at c, so the walk stops there with its row cursors.
        let (_, first) = decode(&j, c);
        assert_eq!(first.end, End::OutOfData);
        let mid = first.resume.last().expect("row cursors").clone();
        assert!(mid.pos <= c && mid.sample > 0);
        // Second piece placed elsewhere: bytes [mid.pos, ..) as a view starting `by` later.
        let by = mid.pos;
        let tail = Mem(j[by as usize..].to_vec());
        let resumed = decode_lossless(
            &mut Reader::new(&tail),
            &f,
            mid.shifted_back(by).unwrap(),
            len - by,
        );
        assert_eq!(resumed.end, End::Eoi { at: len - 2 - by });
        assert_eq!(resumed.samples, whole.samples);
        // Unshifted resume gives the contiguous result exactly.
        let src = Mem(j.clone());
        let again = decode_lossless(&mut Reader::new(&src), &f, mid, len);
        assert_eq!((again.end, again.samples), (whole.end, whole.samples));
        // One cursor per row of samples.
        assert_eq!(whole.resume.len(), 96);
        assert!(whole.resume.iter().all(|c| c.sample % (128 * 2) == 0));
    }

    #[test]
    fn restart_markers_are_checked() {
        let j = testkit::lossless_jpeg_with_restarts(64, 32, 2, 14, 3, 100);
        let (f, run) = decode(&j, j.len() as u64);
        assert_eq!(f.restart_interval, 100);
        assert_eq!(run.end, eoi(j.len()));
        let rst: Vec<usize> = (0..j.len() - 1)
            .filter(|&i| j[i] == 0xFF && (0xD0..=0xD7).contains(&j[i + 1]))
            .collect();
        assert_eq!(rst.len(), 20);
        let mut swapped = j.clone();
        swapped.swap(rst[1] + 1, rst[2] + 1);
        let (_, run) = decode(&swapped, j.len() as u64);
        assert_eq!(run.end, End::Error { at: rst[1] as u64 });
        let mut missing = j.clone();
        missing.drain(rst[0]..rst[0] + 2);
        let (_, run) = decode(&missing, missing.len() as u64);
        assert!(matches!(run.end, End::Error { .. }), "{:?}", run.end);
    }

    #[test]
    fn baseline_jpeg_is_not_lossless() {
        assert!(parse(&testkit::jpeg(64, 64, 1, 0)).is_none());
        assert!(parse(&testkit::jpeg_flat_with_restarts(64, 64, 0)).is_none());
        assert!(parse(&testkit::lossless_jpeg(64, 64, 1, 14, 1)).is_some());
        assert!(parse(&noise(300, 1)).is_none());
    }

    #[test]
    fn unsupported_headers_are_none() {
        let j = testkit::lossless_jpeg(32, 32, 2, 12, 1);
        let sof = sof_at(&j);
        let patched = |at: usize, v: u8| {
            let mut x = j.clone();
            x[at] = v;
            x
        };
        assert!(parse(&patched(sof + 4, 1)).is_none(), "precision 1");
        assert!(parse(&patched(sof + 4, 17)).is_none(), "precision 17");
        assert!(parse(&patched(sof + 1, 0xC1)).is_none(), "SOF1");
        assert!(parse(&patched(sof + 1, 0xCB)).is_none(), "SOF11");
        assert!(parse(&patched(sof + 10, 0x22)).is_none(), "sampling 2x2");
        let mut zero_h = j.clone();
        zero_h[sof + 5..sof + 7].copy_from_slice(&[0, 0]);
        assert!(parse(&zero_h).is_none(), "height 0 (DNL)");
        // The scan's predictor selection and point transform.
        let sos = j.windows(2).position(|w| w == [0xFF, 0xDA]).unwrap();
        let ss = sos + 2 + 2 + 1 + 4;
        assert_eq!(j[ss], 1);
        assert!(parse(&patched(ss, 0)).is_none(), "predictor 0");
        assert!(parse(&patched(ss, 8)).is_none(), "predictor 8");
        assert!(parse(&patched(ss, 7)).is_some(), "predictor 7");
        assert!(parse(&patched(ss + 2, 0x0C)).is_none(), "Al = precision");
        assert!(parse(&patched(ss + 2, 0x10)).is_none(), "Ah");
        assert!(parse(&patched(ss + 1, 63)).is_none(), "Se");
        // A table that was never defined; a DHT symbol over 16.
        assert!(parse(&patched(sos + 6, 0x10)).is_none(), "table 1 missing");
        let dht = j.windows(2).position(|w| w == [0xFF, 0xC4]).unwrap();
        assert!(parse(&patched(dht + 4 + 17, 17)).is_none(), "symbol 17");
        // The header must lie below the limit.
        let src = Mem(j.clone());
        assert!(lossless_frame(&mut Reader::new(&src), 0, sos as u64).is_none());
    }

    #[test]
    fn garbled_headers_and_data_never_panic() {
        let j = testkit::lossless_jpeg(16, 12, 3, 12, 7);
        let (_, scan) = parse(&j).unwrap();
        let noise = noise(4096, 5);
        // Every header byte replaced by a few values, and truncations of the whole file.
        for at in 0..scan as usize {
            for v in [0u8, 1, 0x7F, 0x80, 0xFF, noise[at]] {
                let mut x = j.clone();
                x[at] = v;
                if let Some((f, s)) = parse(&x) {
                    let src = Mem(x.clone());
                    let run =
                        decode_lossless(&mut Reader::new(&src), &f, f.start(s), x.len() as u64);
                    assert!(run.samples <= f.samples());
                }
            }
        }
        for cut in 0..j.len() {
            if let Some((f, s)) = parse(&j[..cut]) {
                let src = Mem(j[..cut].to_vec());
                let run = decode_lossless(&mut Reader::new(&src), &f, f.start(s), cut as u64);
                assert!(!matches!(run.end, End::Eoi { .. }));
            }
        }
        // Noise after a valid header.
        let mut x = j[..scan as usize].to_vec();
        x.extend_from_slice(&noise);
        let (f, s) = parse(&x).unwrap();
        let src = Mem(x.clone());
        let run = decode_lossless(&mut Reader::new(&src), &f, f.start(s), x.len() as u64);
        assert!(!matches!(run.end, End::Eoi { .. }));
        // An extreme frame over little data is bounded by the data.
        let mut huge = j[..scan as usize].to_vec();
        let sof = sof_at(&huge);
        huge[sof + 5..sof + 9].copy_from_slice(&[0x0F, 0xFF, 0xFF, 0xFF]);
        huge.extend_from_slice(&noise);
        let (f, s) = parse(&huge).unwrap();
        let src = Mem(huge.clone());
        let run = decode_lossless(&mut Reader::new(&src), &f, f.start(s), huge.len() as u64);
        assert!(!matches!(run.end, End::Eoi { .. }));
    }
}
