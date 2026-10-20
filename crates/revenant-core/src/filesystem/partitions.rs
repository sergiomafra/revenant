//! Partition tables: the MBR with its EBR chains, and GPT (UEFI specification, chapter 5) with its backup header.
//! Gives the candidate offsets where a volume may start; the boot sector decides.

use super::{MAX_EBR_LINKS, MAX_GPT_ENTRIES, Scheme, boot, crc32, read_exact, u32_at, u64_at};
use crate::source::Source;

/// The logical sector sizes MBR/EBR LBAs are read with: 512, then 4096 (a 4Kn disk). A partition found with 4096 is
/// kept only when its boot sector declares 4096-byte sectors.
const MBR_SECTOR_SIZES: [u64; 2] = [512, 4096];
/// Logical block sizes a GPT is looked for with (its header sits at LBA 1).
const GPT_BLOCK_SIZES: [u64; 2] = [512, 4096];
const GPT_PROTECTIVE: u8 = 0xEE;

/// One MBR/EBR partition entry: type, first LBA, sectors.
#[derive(Clone, Copy)]
struct Entry {
    kind: u8,
    start: u64,
    size: u64,
}

fn entry(sector: &[u8], slot: usize) -> Entry {
    let e = &sector[446 + 16 * slot..462 + 16 * slot];
    Entry {
        kind: e[4],
        start: u32_at(e, 8) as u64,
        size: u32_at(e, 12) as u64,
    }
}

impl Entry {
    fn used(self) -> bool {
        self.kind != 0 && self.start != 0 && self.size != 0
    }

    fn extended(self) -> bool {
        matches!(self.kind, 0x05 | 0x0F | 0x85)
    }
}

fn signed(sector: &[u8]) -> bool {
    sector[510..512] == [0x55, 0xAA]
}

/// Where a volume may start.
#[derive(Clone, Copy, Debug)]
pub(super) struct Candidate {
    pub(super) offset: u64,
    pub(super) scheme: Scheme,
    /// The sector size the volume's boot sector must declare (an MBR read with 4096-byte sectors).
    pub(super) sector_size: Option<u32>,
}

fn at(offset: u64, scheme: Scheme) -> Candidate {
    Candidate {
        offset,
        scheme,
        sector_size: None,
    }
}

/// Where a volume may start: sector 0 itself when it is a boot sector, else the GPT's partitions, else (no GPT, or a
/// hybrid MBR whose GPT headers both fail) the MBR's. Empty when sector 0 is neither.
pub(super) fn candidates(source: &dyn Source) -> Vec<Candidate> {
    let Some(s0) = read_exact(source, 0, 512) else {
        return Vec::new();
    };
    if boot::valid(&s0) {
        return vec![at(0, Scheme::None)];
    }
    if !signed(&s0) {
        return Vec::new();
    }
    let entries: Vec<Entry> = (0..4).map(|i| entry(&s0, i)).collect();
    if entries.iter().any(|e| e.kind == GPT_PROTECTIVE) {
        if let Some(found) = gpt(source) {
            return found.into_iter().map(|o| at(o, Scheme::Gpt)).collect();
        }
        let rest: Vec<Entry> = entries
            .into_iter()
            .filter(|e| e.kind != GPT_PROTECTIVE)
            .collect();
        return mbr(source, &rest);
    }
    mbr(source, &entries)
}

/// The primary partitions, and the logical ones of every extended partition, read with 512- then 4096-byte sectors (at
/// most [`MAX_EBR_LINKS`] EBRs read in all).
fn mbr(source: &dyn Source, entries: &[Entry]) -> Vec<Candidate> {
    let mut out = Vec::new();
    let mut links = 0;
    for unit in MBR_SECTOR_SIZES {
        let sector_size = (unit != 512).then_some(unit as u32);
        let mut seen = Vec::new();
        for &e in entries.iter().filter(|e| e.used()) {
            if e.extended() {
                let chain = Chain {
                    unit,
                    sector_size,
                    ext: e.start,
                };
                chain.follow(source, &mut links, &mut seen, &mut out);
            } else {
                out.push(Candidate {
                    offset: e.start * unit,
                    scheme: Scheme::Mbr,
                    sector_size,
                });
            }
        }
    }
    out
}

/// An extended partition's EBR chain, read with `unit`-byte sectors.
struct Chain {
    unit: u64,
    sector_size: Option<u32>,
    ext: u64,
}

impl Chain {
    /// Follows the EBRs from the extended partition's start: each EBR's first entry is a logical partition (relative to
    /// that EBR), its second the next EBR (relative to the extended start). A link already seen, outside the source,
    /// unreadable or unsigned ends the chain; `links` counts the EBRs read, all chains together.
    fn follow(
        &self,
        source: &dyn Source,
        links: &mut usize,
        seen: &mut Vec<u64>,
        out: &mut Vec<Candidate>,
    ) {
        let mut lba = self.ext;
        while *links < MAX_EBR_LINKS && !seen.contains(&lba) && lba * self.unit < source.len() {
            *links += 1;
            seen.push(lba);
            let Some(s) = read_exact(source, lba * self.unit, 512) else {
                return;
            };
            if !signed(&s) {
                return;
            }
            let logical = entry(&s, 0);
            if logical.used() && !logical.extended() {
                out.push(Candidate {
                    offset: (lba + logical.start) * self.unit,
                    scheme: Scheme::Mbr,
                    sector_size: self.sector_size,
                });
            }
            let next = entry(&s, 1);
            if !next.extended() || next.start == 0 {
                return;
            }
            lba = self.ext + next.start;
        }
    }
}

/// The GPT's partitions: the primary header (LBA 1) and entries when both CRCs hold, else the backup header at the
/// primary's AlternateLBA or at the last LBA. 512-byte blocks first, then 4096. `None` when no header checks out.
fn gpt(source: &dyn Source) -> Option<Vec<u64>> {
    for bs in GPT_BLOCK_SIZES {
        let blocks = source.len() / bs;
        if blocks < 3 {
            continue;
        }
        let mut tried = vec![1];
        let alternate = match table(source, bs, 1) {
            Ok(found) => return Some(found),
            Err(alternate) => alternate,
        };
        for lba in alternate.into_iter().chain([blocks - 1]) {
            if lba == 0 || lba >= blocks || tried.contains(&lba) {
                continue;
            }
            tried.push(lba);
            if let Ok(found) = table(source, bs, lba) {
                return Some(found);
            }
        }
    }
    None
}

/// The partitions of the GPT header at `lba` (blocks of `bs` bytes). `Err(AlternateLBA)` when the header or its
/// entries do not check out (`Err(None)` without the signature).
fn table(source: &dyn Source, bs: u64, lba: u64) -> Result<Vec<u64>, Option<u64>> {
    let Some(mut h) = read_exact(source, lba * bs, bs as usize) else {
        return Err(None);
    };
    if &h[..8] != b"EFI PART" {
        return Err(None);
    }
    let alternate = Some(u64_at(&h, 32));
    let size = u32_at(&h, 12) as usize;
    if !(92..=bs as usize).contains(&size) || u64_at(&h, 24) != lba {
        return Err(alternate);
    }
    let crc = u32_at(&h, 16);
    h[16..20].fill(0);
    if crc32(&h[..size]) != crc {
        return Err(alternate);
    }
    let count = u32_at(&h, 80);
    let esize = u32_at(&h, 84);
    if count > MAX_GPT_ENTRIES || !(128..=1024).contains(&esize) || !esize.is_multiple_of(128) {
        return Err(alternate);
    }
    let esize = esize as usize;
    let Some(at) = u64_at(&h, 72).checked_mul(bs) else {
        return Err(alternate);
    };
    let Some(entries) = read_exact(source, at, count as usize * esize) else {
        return Err(alternate);
    };
    if crc32(&entries) != u32_at(&h, 88) {
        return Err(alternate);
    }
    let mut out = Vec::new();
    for e in entries.chunks_exact(esize) {
        let (first, last) = (u64_at(e, 32), u64_at(e, 40));
        if e[..16].iter().any(|&b| b != 0)
            && first <= last
            && let Some(o) = first.checked_mul(bs)
        {
            out.push(o);
        }
    }
    Ok(out)
}
