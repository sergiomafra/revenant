//! FAT12/16/32 and exFAT volumes of a source: where they are and their geometry.
//!
//! Formats from the public specifications only: the MBR/EBR conventions, the GPT chapter of the UEFI specification,
//! Microsoft's "FAT32 File System Specification" 1.03 (fatgen103) and Microsoft's "exFAT file system specification".
//! Nothing here writes, and a broken table or boot sector is never an error: it only yields fewer volumes.

mod boot;
mod partitions;

use crate::source::Source;
use partitions::Candidate;

/// Most volumes reported for one source (the lowest offsets are kept).
pub(crate) const MAX_VOLUMES: usize = 16;
/// Most EBRs read while following the extended partitions of one MBR (all chains together).
pub(crate) const MAX_EBR_LINKS: usize = 64;
/// Most entries a GPT may declare; a header declaring more is refused.
pub(crate) const MAX_GPT_ENTRIES: u32 = 128;
/// Where a volume is looked for, in 512-byte sectors, when no table gave one.
const PROBE_LBAS: [u64; 7] = [0, 63, 128, 2048, 8192, 32_768, 65_536];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FsKind {
    Fat12,
    Fat16,
    Fat32,
    Exfat,
}

impl FsKind {
    pub fn as_str(self) -> &'static str {
        match self {
            FsKind::Fat12 => "fat12",
            FsKind::Fat16 => "fat16",
            FsKind::Fat32 => "fat32",
            FsKind::Exfat => "exfat",
        }
    }
}

/// How a volume was found: no partition table (a superfloppy, or a probe after the table was lost), MBR or GPT.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scheme {
    None,
    Mbr,
    Gpt,
}

impl Scheme {
    pub fn as_str(self) -> &'static str {
        match self {
            Scheme::None => "none",
            Scheme::Mbr => "mbr",
            Scheme::Gpt => "gpt",
        }
    }
}

/// One FAT or exFAT volume. Offsets are absolute in the source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Volume {
    /// Position in offset order, from 0.
    pub index: u32,
    pub fs: FsKind,
    /// Byte offset of the volume's first sector.
    pub offset: u64,
    /// Bytes, as the boot sector says (may run past the source's end).
    pub len: u64,
    pub sector_size: u32,
    pub cluster_size: u64,
    /// Byte offset of cluster 2.
    pub heap_offset: u64,
    /// Clusters `2 ..= cluster_count + 1`; the heap ends at `heap_offset + cluster_count × cluster_size`.
    pub cluster_count: u32,
    pub label: Option<String>,
    pub serial: u32,
    /// The primary boot sector was unusable; the backup was used.
    pub from_backup: bool,
    pub scheme: Scheme,
}

impl Volume {
    /// Absolute byte offset of cluster `c`; `None` outside `2 ..= cluster_count + 1`.
    pub fn cluster_offset(&self, c: u32) -> Option<u64> {
        if c < 2 || c as u64 > self.cluster_count as u64 + 1 {
            return None;
        }
        Some(self.heap_offset + (c as u64 - 2) * self.cluster_size)
    }
}

/// Every FAT/exFAT volume of the source, in offset order (at most [`MAX_VOLUMES`]). Reads a bounded amount: the
/// partition tables (at most [`MAX_EBR_LINKS`] EBRs, 6 GPT headers with at most [`MAX_GPT_ENTRIES`] entries of ≤ 1 KiB),
/// then a few sectors per candidate offset; at most 1,362 reads and 16,588,800 bytes in all (core README).
pub fn volumes(source: &dyn Source) -> Vec<Volume> {
    let mut found = parse_all(source, partitions::candidates(source));
    if found.is_empty() {
        found = probe(source);
    }
    for (i, v) in found.iter_mut().enumerate() {
        v.index = i as u32;
    }
    found
}

/// Parses each distinct candidate offset inside the source, lowest first, until [`MAX_VOLUMES`] volumes are found. At
/// an offset given both with and without a required sector size, the one without is kept.
fn parse_all(source: &dyn Source, mut cands: Vec<Candidate>) -> Vec<Volume> {
    let len = source.len();
    cands.retain(|c| c.offset < len);
    cands.sort_by_key(|c| (c.offset, c.sector_size.is_some()));
    cands.dedup_by_key(|c| c.offset);
    let mut out = Vec::new();
    for c in cands {
        if out.len() == MAX_VOLUMES {
            break;
        }
        if let Some(v) = boot::parse(source, c.offset, c.scheme, c.sector_size) {
            out.push(v);
        }
    }
    out
}

/// The volumes at the probe offsets (scheme `None`), one per offset, in offset order.
fn probe(source: &dyn Source) -> Vec<Volume> {
    let mut out: Vec<Volume> = Vec::new();
    for lba in PROBE_LBAS {
        let o = lba * 512;
        if o >= source.len() {
            continue;
        }
        if let Some(v) = boot::probe(source, o)
            && !out.iter().any(|w| w.offset == v.offset)
        {
            out.push(v);
        }
    }
    out.sort_by_key(|v| v.offset);
    out.truncate(MAX_VOLUMES);
    out
}

/// Exactly `len` bytes at `offset`, or `None` (read error, or the source ends first).
pub(crate) fn read_exact(source: &dyn Source, offset: u64, len: usize) -> Option<Vec<u8>> {
    if offset.checked_add(len as u64)? > source.len() {
        return None;
    }
    let mut buf = vec![0u8; len];
    match source.read_at(offset, &mut buf) {
        Ok(n) if n == len => Some(buf),
        _ => None,
    }
}

pub(crate) fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

pub(crate) fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

pub(crate) fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().expect("8 bytes"))
}

const CRC_TABLE: [u32; 256] = {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
};

/// CRC-32 as GPT uses it (IEEE 802.3, reflected, initial value and final XOR `0xFFFF_FFFF`).
pub(crate) fn crc32(data: &[u8]) -> u32 {
    !data.iter().fold(0xFFFF_FFFFu32, |c, &b| {
        CRC_TABLE[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }
}
