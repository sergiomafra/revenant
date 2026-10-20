//! What a deleted FAT entry lost and how it is recovered: the first letter of its short name (ruling D9) and, on FAT32
//! when Windows zeroed it, the high word of its first cluster (ruling D10).
//!
//! From fatgen103: deleting writes `0xE5` over byte 0 of the short entry and of each long-name entry, so the short
//! name's first letter and the long-name ordinals are lost while the long-name checksum (byte 13) survives.

use std::collections::BTreeMap;

use super::Probe;
use super::alloc::ClusterMap;
use super::fat::{LFN_UNITS, checksum, sfn_char};
use super::u16_at;
use crate::formats::{FileKind, builtin};

/// Most deleted long-name parts read before one short entry (255 characters).
const MAX_PARTS: usize = 20;

/// The deleted long-name parts met since the last short entry, newest last (at most [`MAX_PARTS`], the oldest dropped).
pub(super) struct Gone {
    parts: [(u8, [u16; 13]); MAX_PARTS],
    len: usize,
    /// Where the next part goes.
    head: usize,
}

impl Default for Gone {
    fn default() -> Self {
        Gone {
            parts: [(0, [0; 13]); MAX_PARTS],
            len: 0,
            head: 0,
        }
    }
}

impl Gone {
    pub(super) fn push(&mut self, e: &[u8; 32]) {
        let mut units = [0u16; 13];
        for (u, &off) in units.iter_mut().zip(LFN_UNITS.iter()) {
            *u = u16_at(e, off);
        }
        self.parts[self.head] = (e[13], units);
        self.head = (self.head + 1) % MAX_PARTS;
        self.len = (self.len + 1).min(MAX_PARTS);
    }

    pub(super) fn clear(&mut self) {
        self.len = 0;
    }

    /// Ruling D9, first rule: the parts right before deleted short name `sfn` carrying the same checksum, nearest first
    /// (the nearest is part 1), give a name; when its first character, upper-cased (ASCII), as the short name's byte 0
    /// makes the checksum match, that byte and the name's text (up to the first `0x0000`, decoded lossily).
    pub(super) fn restore(&self, sfn: &[u8]) -> Option<(u8, String)> {
        if self.len == 0 {
            return None;
        }
        let at = |i: usize| &self.parts[(self.head + 2 * MAX_PARTS - 1 - i) % MAX_PARTS];
        let sum = at(0).0;
        let mut units: Vec<u16> = Vec::with_capacity(self.len * 13);
        for i in 0..self.len {
            let (s, part) = at(i);
            if *s != sum {
                break;
            }
            units.extend_from_slice(part);
        }
        let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
        let text = &units[..end];
        let first = *text.first()?;
        if first >= 0x80 {
            return None;
        }
        let byte = (first as u8).to_ascii_uppercase();
        let mut raw = [0u8; 11];
        raw.copy_from_slice(&sfn[..11]);
        raw[0] = byte;
        (checksum(&raw) == sum).then(|| (byte, String::from_utf16_lossy(text)))
    }
}

/// A short name's bytes 1, 2 and 3 with its byte 0: `b1 << 24 | b2 << 16 | b3 << 8 | b0`.
pub(super) fn pack(sfn: &[u8], first: u8) -> u32 {
    key(sfn) << 8 | first as u32
}

/// A short name's bytes 1, 2 and 3 (what D9 matches siblings by).
pub(super) fn key(sfn: &[u8]) -> u32 {
    (sfn[1] as u32) << 16 | (sfn[2] as u32) << 8 | sfn[3] as u32
}

/// How often each first byte goes with each key among known short names (live, or already restored): ruling D9's
/// sibling rule. Built once from packed values ([`pack`]); later additions are counted beside.
#[derive(Default)]
pub(super) struct FirstBytes {
    /// (packed value, count), sorted, each value once.
    base: Vec<(u32, u32)>,
    extra: BTreeMap<u32, u32>,
}

impl FirstBytes {
    pub(super) fn new(mut packed: Vec<u32>) -> FirstBytes {
        packed.sort_unstable();
        let distinct = packed.windows(2).filter(|w| w[0] != w[1]).count() + 1;
        let mut base: Vec<(u32, u32)> = Vec::with_capacity(distinct.min(packed.len()));
        for p in packed {
            match base.last_mut() {
                Some((q, n)) if *q == p => *n += 1,
                _ => base.push((p, 1)),
            }
        }
        FirstBytes {
            base,
            extra: BTreeMap::new(),
        }
    }

    pub(super) fn add(&mut self, key: u32, first: u8) {
        *self.extra.entry(key << 8 | first as u32).or_default() += 1;
    }

    /// The most common first byte with `key` (ties: the lowest byte), if any.
    pub(super) fn best(&self, key: u32) -> Option<u8> {
        let (lo, hi) = (key << 8, key << 8 | 0xFF);
        let mut counts = [0u32; 256];
        let from = self.base.partition_point(|&(p, _)| p < lo);
        for &(p, n) in self.base[from..].iter().take_while(|(p, _)| *p <= hi) {
            counts[(p & 0xFF) as usize] += n;
        }
        for (&p, &n) in self.extra.range(lo..=hi) {
            counts[(p & 0xFF) as usize] += n;
        }
        let mut best = None;
        let mut most = 0;
        for (b, &n) in counts.iter().enumerate() {
            if n > most {
                most = n;
                best = Some(b as u8);
            }
        }
        best
    }
}

/// Writes `first` as character 0 of a short name formatted with a placeholder there (the same rules as the rest of the
/// name: lower-cased when the NT base bit says so, `_` outside printable ASCII, and `/` as `_`).
pub(super) fn set_first(name: &mut String, first: u8, lower: bool) {
    let c = match sfn_char(first, lower) {
        '/' => '_',
        c => c,
    };
    if name.is_char_boundary(1) {
        name.replace_range(..1, c.encode_utf8(&mut [0; 4]));
    }
}

/// The listing-wide cap on head checks through the [`Probe`] (rulings D10, C2).
pub(super) struct Probes<'a> {
    probe: &'a dyn Probe,
    left: usize,
    ran_out: bool,
}

impl<'a> Probes<'a> {
    pub(super) fn new(probe: &'a dyn Probe, cap: usize) -> Self {
        Probes {
            probe,
            left: cap,
            ran_out: false,
        }
    }

    /// The cap stopped a check.
    pub(super) fn ran_out(&self) -> bool {
        self.ran_out
    }

    /// Whether the 512 bytes at `at` start a file of `kind` (its format recognizes the first 16 bytes); `None` when the
    /// cap is spent. A check the probe refuses or cannot read counts and recognizes nothing.
    fn recognizes(&mut self, at: u64, kind: FileKind) -> Option<bool> {
        if self.left == 0 {
            self.ran_out = true;
            return None;
        }
        self.left -= 1;
        let family = family(kind);
        Some(self.probe.read512(at).is_some_and(|head| {
            builtin()
                .iter()
                .any(|f| f.name() == family && f.recognizes(&head[..16]))
        }))
    }
}

/// The format that measures files of `kind`.
fn family(kind: FileKind) -> &'static str {
    match kind {
        FileKind::Jpeg => "jpeg",
        FileKind::Mp4 | FileKind::Mov | FileKind::Cr3 | FileKind::Heic => "isobmff",
        FileKind::Raf => "raf",
        FileKind::Cr2
        | FileKind::Nef
        | FileKind::Arw
        | FileKind::Dng
        | FileKind::Orf
        | FileKind::Rw2
        | FileKind::Tiff => "tiffraw",
    }
}

/// Ruling D10 (with C3): the first cluster of a deleted FAT32 file of `kind` whose high word reads 0, on a volume
/// whose clusters `2 ..= last` pass 0xFFFF. The candidates are `lo` and each `h << 16 | lo` up to `last` (`h ≥ 1`, at
/// most `last >> 16` of them); one counts when it is free today and its head is recognized as `kind`. Exactly one
/// candidate is the answer; none, several, or the probe cap running out before all were checked give `None`.
pub(super) fn high_word(
    map: &mut dyn ClusterMap,
    probes: &mut Probes<'_>,
    offset: impl Fn(u32) -> Option<u64>,
    lo: u32,
    last: u32,
    kind: FileKind,
) -> Option<u32> {
    let highs = (1..=last >> 16)
        .map(move |h| h << 16 | lo)
        .take_while(move |&c| c <= last);
    let mut found = None;
    for c in std::iter::once(lo).chain(highs) {
        if map.allocated(c) != Some(false) {
            continue;
        }
        let at = offset(c)?;
        if probes.recognizes(at, kind)? && found.replace(c).is_some() {
            return None;
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_most_common_first_byte_wins_and_ties_take_the_lowest() {
        let sfn = |s: &[u8; 11]| *s;
        let dsc = sfn(b"DSC01234JPG");
        let mut t = FirstBytes::new(vec![
            pack(&dsc, b'D'),
            pack(&dsc, b'D'),
            pack(&dsc, b'X'),
            pack(b"ASC00001JPG", b'A'),
            pack(b"IMG00001JPG", b'I'),
        ]);
        assert_eq!(t.best(key(&dsc)), Some(b'D'));
        assert_eq!(t.best(key(b"QXYZ    JPG")), None);
        t.add(key(&dsc), b'X');
        // Tie D/X (2 each): the lowest byte.
        assert_eq!(t.best(key(&dsc)), Some(b'D'));
        t.add(key(&dsc), b'X');
        assert_eq!(t.best(key(&dsc)), Some(b'X'));
        // Ties of two bytes never seen in the base.
        let mut t = FirstBytes::new(Vec::new());
        t.add(key(b"MIMG    JPG"), b'Z');
        t.add(key(b"MIMG    JPG"), b'B');
        assert_eq!(t.best(key(b"MIMG    JPG")), Some(b'B'));
    }

    fn lfn(sum: u8, text: &[u16; 13]) -> [u8; 32] {
        let mut e = [0u8; 32];
        e[0] = 0xE5;
        e[11] = 0x0F;
        e[13] = sum;
        for (k, &off) in LFN_UNITS.iter().enumerate() {
            e[off..off + 2].copy_from_slice(&text[k].to_le_bytes());
        }
        e
    }

    fn units(s: &str) -> [u16; 13] {
        let mut u: Vec<u16> = s.encode_utf16().collect();
        if u.len() < 13 {
            u.push(0);
            u.resize(13, 0xFFFF);
        }
        u.try_into().unwrap()
    }

    #[test]
    fn a_deleted_long_name_restores_the_first_letter_only_through_the_checksum() {
        let sfn = *b"LONGNA~1JPG";
        let sum = checksum(&sfn);
        let mut gone = Gone::default();
        // On disk the last part comes first; the nearest to the short entry is part 1.
        gone.push(&lfn(sum, &units("to.jpg")));
        gone.push(&lfn(sum, &units("Long name pho")));
        let mut deleted = sfn;
        deleted[0] = 0xE5;
        assert_eq!(
            gone.restore(&deleted),
            Some((b'L', "Long name photo.jpg".to_string()))
        );
        // A part with another checksum before them is not part of the name.
        let mut g = Gone::default();
        g.push(&lfn(sum.wrapping_add(1), &units("zzz")));
        g.push(&lfn(sum, &units("to.jpg")));
        g.push(&lfn(sum, &units("Long name pho")));
        assert_eq!(g.restore(&deleted).unwrap().1, "Long name photo.jpg");
        // The first letter does not give the checksum: nothing.
        let mut g = Gone::default();
        g.push(&lfn(sum, &units("Kong.jpg")));
        assert_eq!(g.restore(&deleted), None);
        // A lower-case first letter is upper-cased; a non-ASCII one gives nothing.
        let mut g = Gone::default();
        g.push(&lfn(sum, &units("to.jpg")));
        g.push(&lfn(sum, &units("long name pho")));
        assert_eq!(g.restore(&deleted).unwrap().0, b'L');
        let mut g = Gone::default();
        g.push(&lfn(sum, &units("élan.jpg")));
        assert_eq!(g.restore(&deleted), None);
        // Nothing met, or cleared.
        g.clear();
        assert_eq!(g.restore(&deleted), None);
        // More than 20 parts: the 20 nearest are kept (the name is cut, the checksum still decides).
        let mut g = Gone::default();
        for _ in 0..25 {
            g.push(&lfn(sum, &units("LLLLLLLLLLLLL")));
        }
        let (b, name) = g.restore(&deleted).unwrap();
        assert_eq!((b, name.len()), (b'L', 260));
    }

    #[test]
    fn the_first_character_is_written_as_a_short_name_character() {
        let mut n = "_SC01235.JPG".to_string();
        set_first(&mut n, b'D', false);
        assert_eq!(n, "DSC01235.JPG");
        let mut n = "_sc01235.JPG".to_string();
        set_first(&mut n, b'D', true);
        assert_eq!(n, "dsc01235.JPG");
        for (b, want) in [(b'/', "_X"), (0x05, "_X"), (0xE5, "_X"), (b' ', " X")] {
            let mut n = "_X".to_string();
            set_first(&mut n, b, false);
            assert_eq!(n, want, "{b:#x}");
        }
    }
}
