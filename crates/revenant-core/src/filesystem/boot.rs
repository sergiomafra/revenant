//! FAT12/16/32 and exFAT boot sectors: validation, the backup boot sectors, and the volume label.
//!
//! FAT rules from fatgen103 (BPB fields, cluster-count typing, the FAT32 backup boot sector at sector 6); exFAT rules
//! from the exFAT specification §3.1 (main and backup boot regions, boot checksum §3.4, volume label entry §7.3).

use super::{FsKind, Scheme, Volume, read_exact, u16_at, u32_at, u64_at};
use crate::source::Source;

/// Sector sizes the backup boot sectors are looked for with.
const BACKUP_SECTOR_SIZES: [u64; 2] = [512, 4096];
/// The FAT32 backup boot sector (fatgen103 recommends 6, and every formatter writes it there).
const FAT_BACKUP_SECTOR: u64 = 6;
/// The exFAT backup boot region starts at sector 12.
const EXFAT_BACKUP_SECTOR: u64 = 12;
/// Bytes of an exFAT root's first cluster scanned for the label entry.
const EXFAT_LABEL_SCAN: u64 = 4096;
/// FAT root entries scanned for the volume-label entry.
const FAT_LABEL_ENTRIES: u64 = 16;

/// Where the root directory starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Root {
    /// FAT12/16: the fixed root region, at this sector of the volume, holding this many entries.
    Fixed { sector: u64, entries: u64 },
    /// FAT32/exFAT: the root's first cluster.
    Cluster(u32),
}

/// The geometry a valid boot sector gives (sectors are of `bps` bytes, relative to the volume start).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Boot {
    fs: FsKind,
    bps: u64,
    spc: u64,
    total: u64,
    /// First sector of the first FAT.
    fat_start: u64,
    /// Sectors of one FAT copy.
    fat_len: u64,
    nfats: u64,
    /// The media byte FAT entry 0 starts with (exFAT: always `0xF8`).
    media: u8,
    heap: u64,
    count: u32,
    root: Root,
    serial: u32,
    /// FAT: the BPB label (when the extended boot signature is present).
    bpb_label: Option<[u8; 11]>,
}

/// Whether `sector` (at least 512 bytes) is a valid FAT or exFAT boot sector.
pub(super) fn valid(sector: &[u8]) -> bool {
    fields(sector).is_some()
}

/// The bytes of a FAT or exFAT boot sector the system rewrites while the volume is mounted (ruling D7): FAT12/16 byte
/// 37 and FAT32 byte 65 (the dirty flags of `BS_Reserved1`), exFAT bytes 106, 107 (`VolumeFlags`) and 112
/// (`PercentInUse`). Empty for anything else.
pub(super) fn volatile_bytes(sector: &[u8]) -> &'static [usize] {
    match fields(sector).map(|b| b.fs) {
        Some(FsKind::Fat12 | FsKind::Fat16) => &[37],
        Some(FsKind::Fat32) => &[65],
        Some(FsKind::Exfat) => &[106, 107, 112],
        None => &[],
    }
}

fn fields(s: &[u8]) -> Option<Boot> {
    if s.len() < 512 || s[510..512] != [0x55, 0xAA] {
        return None;
    }
    if &s[3..11] == b"EXFAT   " {
        exfat_fields(s)
    } else {
        fat_fields(s)
    }
}

fn fat_fields(s: &[u8]) -> Option<Boot> {
    if !((s[0] == 0xEB && s[2] == 0x90) || s[0] == 0xE9) {
        return None;
    }
    let bps = u16_at(s, 11) as u64;
    if !matches!(bps, 512 | 1024 | 2048 | 4096) {
        return None;
    }
    let spc = s[13] as u64;
    if spc == 0 || !spc.is_power_of_two() || spc > 128 {
        return None;
    }
    let reserved = u16_at(s, 14) as u64;
    let nfats = s[16] as u64;
    if reserved == 0 || !(1..=2).contains(&nfats) {
        return None;
    }
    let root_entries = u16_at(s, 17) as u64;
    let total = match u16_at(s, 19) {
        0 => u32_at(s, 32) as u64,
        n => n as u64,
    };
    let fat16_len = u16_at(s, 22) as u64;
    let fat32_len = u32_at(s, 36) as u64;
    let root_cluster = u32_at(s, 44);
    // Ruling D8: FAT32 is told by its BPB.
    let fat32 = fat16_len == 0 && root_entries == 0 && fat32_len > 0 && root_cluster >= 2;
    // fatgen103: FAT12/16 have a 16-bit FAT size and a fixed root; without either the BPB is neither FAT12/16 nor a
    // FAT32 one (a FAT32 with a corrupt RootClus, say), so its backup is tried instead.
    if !fat32 && (fat16_len == 0 || root_entries == 0) {
        return None;
    }
    let fat_len = if fat32 { fat32_len } else { fat16_len };
    if total == 0 {
        return None;
    }
    let root_sectors = (root_entries * 32).div_ceil(bps);
    let root_start = reserved + nfats * fat_len;
    let heap = root_start + root_sectors;
    if heap >= total {
        return None;
    }
    let count = (total - heap) / spc;
    if count == 0 {
        return None;
    }
    let count = u32::try_from(count).ok()?;
    let fs = if fat32 {
        FsKind::Fat32
    } else if count < 4085 {
        FsKind::Fat12
    } else if count < 65_525 {
        FsKind::Fat16
    } else {
        return None;
    };
    let root = if fat32 {
        if root_cluster as u64 > count as u64 + 1 {
            return None;
        }
        Root::Cluster(root_cluster)
    } else {
        Root::Fixed {
            sector: root_start,
            entries: root_entries,
        }
    };
    let ext = if fat32 { 64 } else { 36 };
    let (serial, bpb_label) = if s[ext + 2] == 0x29 {
        let mut label = [0u8; 11];
        label.copy_from_slice(&s[ext + 7..ext + 18]);
        (u32_at(s, ext + 3), Some(label))
    } else {
        (0, None)
    };
    Some(Boot {
        fs,
        bps,
        spc,
        total,
        fat_start: reserved,
        fat_len,
        nfats,
        media: s[21],
        heap,
        count,
        root,
        serial,
        bpb_label,
    })
}

fn exfat_fields(s: &[u8]) -> Option<Boot> {
    if s[11..64].iter().any(|&b| b != 0) {
        return None;
    }
    let bps_shift = s[108] as u32;
    let spc_shift = s[109] as u32;
    if !(9..=12).contains(&bps_shift) || spc_shift > 25 - bps_shift {
        return None;
    }
    let nfats = s[110] as u64;
    if !(1..=2).contains(&nfats) {
        return None;
    }
    let volume_len = u64_at(s, 72);
    let fat_offset = u32_at(s, 80) as u64;
    let fat_len = u32_at(s, 84) as u64;
    let heap = u32_at(s, 88) as u64;
    let count = u32_at(s, 92);
    let root = u32_at(s, 96);
    if fat_offset < 24 || heap < fat_offset + fat_len * nfats {
        return None;
    }
    if count == 0 || count > 0xFFFF_FFF5 {
        return None;
    }
    if heap + ((count as u64) << spc_shift) > volume_len {
        return None;
    }
    if root < 2 || root as u64 > count as u64 + 1 {
        return None;
    }
    // The byte length must be representable.
    volume_len.checked_mul(1 << bps_shift)?;
    Some(Boot {
        fs: FsKind::Exfat,
        bps: 1 << bps_shift,
        spc: 1 << spc_shift,
        total: volume_len,
        fat_start: fat_offset,
        fat_len,
        nfats,
        media: 0xF8,
        heap,
        count,
        root: Root::Cluster(root),
        serial: u32_at(s, 100),
        bpb_label: None,
    })
}

/// Whether the main or backup boot region starting at `at` (12 sectors of `bps` bytes) has a matching boot checksum
/// (exFAT spec §3.4: sectors 0..11 summed without bytes 106, 107 and 112, the sum repeated through sector 11).
fn exfat_checksum_ok(source: &dyn Source, at: u64, bps: u64) -> bool {
    let Some(region) = read_exact(source, at, 12 * bps as usize) else {
        return false;
    };
    let n = 11 * bps as usize;
    let sum = region[..n]
        .iter()
        .enumerate()
        .filter(|&(i, _)| i != 106 && i != 107 && i != 112)
        .fold(0u32, |s, (_, &b)| s.rotate_right(1).wrapping_add(b as u32));
    region[n..]
        .as_chunks::<4>()
        .0
        .iter()
        .all(|c| u32::from_le_bytes(*c) == sum)
}

/// Where a volume's allocation tables and root are, in absolute bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Layout {
    pub sector_size: u64,
    /// The first FAT copy.
    pub fat_offset: u64,
    /// Bytes of one FAT copy.
    pub fat_bytes: u64,
    pub fat_copies: u64,
    /// FAT12/16: `Fixed` with an absolute byte offset in `sector`; FAT32/exFAT: the root's first cluster.
    pub root: Root,
}

/// The layout of a volume [`super::volumes`] reported, from the same boot sector it was read from (`None` when that
/// boot sector no longer gives the same geometry).
pub(super) fn layout(source: &dyn Source, v: &Volume) -> Option<Layout> {
    let (b, _) = choose(source, v.offset)?;
    if b.fs != v.fs
        || b.count != v.cluster_count
        || offset_of(v.offset, b.heap, b.bps)? != v.heap_offset
    {
        return None;
    }
    let root = match b.root {
        Root::Fixed { sector, entries } => Root::Fixed {
            sector: offset_of(v.offset, sector, b.bps)?,
            entries,
        },
        r => r,
    };
    Some(Layout {
        sector_size: b.bps,
        fat_offset: offset_of(v.offset, b.fat_start, b.bps)?,
        fat_bytes: b.fat_len.checked_mul(b.bps)?,
        fat_copies: b.nfats,
        root,
    })
}

/// `base + sector × bps`, when representable.
fn offset_of(base: u64, sector: u64, bps: u64) -> Option<u64> {
    sector.checked_mul(bps)?.checked_add(base)
}

/// The boot sector at `at`, when valid.
fn boot_at(source: &dyn Source, at: u64) -> Option<Boot> {
    fields(&read_exact(source, at, 512)?)
}

/// A valid backup boot sector of the volume at `offset` (relative to it, the backup's own sector size), the exFAT ones
/// whose region checksum matches first, as `(boot, checksum matches)`.
fn backup(source: &dyn Source, offset: u64) -> Option<(Boot, bool)> {
    let mut fallback = None;
    for s in BACKUP_SECTOR_SIZES {
        let at = offset + EXFAT_BACKUP_SECTOR * s;
        if let Some(b) = boot_at(source, at).filter(|b| b.fs == FsKind::Exfat && b.bps == s) {
            if exfat_checksum_ok(source, at, s) {
                return Some((b, true));
            }
            fallback.get_or_insert((b, false));
        }
        let at = offset + FAT_BACKUP_SECTOR * s;
        if let Some(b) = boot_at(source, at).filter(|b| b.fs == FsKind::Fat32 && b.bps == s) {
            return Some((b, true));
        }
    }
    fallback
}

/// The boot sector of the volume whose first sector is at `offset`, as `(boot, from_backup)`: its primary boot sector,
/// else a backup. An exFAT primary whose fields are valid but whose boot checksum does not match gives way to an exFAT
/// backup whose checksum does.
fn choose(source: &dyn Source, offset: u64) -> Option<(Boot, bool)> {
    Some(match boot_at(source, offset) {
        Some(p) if p.fs != FsKind::Exfat || exfat_checksum_ok(source, offset, p.bps) => (p, false),
        Some(p) => match backup(source, offset) {
            Some((b, true)) if b.fs == p.fs => (b, true),
            _ => (p, false),
        },
        None => (backup(source, offset)?.0, true),
    })
}

/// The volume whose first sector is at `offset` (see [`choose`]), refused when `sector_size` is given and its boot
/// sector declares another.
pub(super) fn parse(
    source: &dyn Source,
    offset: u64,
    scheme: Scheme,
    sector_size: Option<u32>,
) -> Option<Volume> {
    let (boot, from_backup) = choose(source, offset)?;
    if sector_size.is_some_and(|s| boot.bps != s as u64) {
        return None;
    }
    volume(source, offset, boot, from_backup, scheme)
}

/// As [`parse`] at a probed offset (scheme `None`), guarding against a backup boot sector read as a primary: when the
/// primary at `offset` is a FAT32 or exFAT boot sector that would also be the backup (sector 6 or 12) of a volume
/// starting that many sectors earlier, with the same geometry, that earlier volume is the one reported when its primary
/// is there too, or when its FAT starts where the boot sector says and this one's does not.
pub(super) fn probe(source: &dyn Source, offset: u64) -> Option<Volume> {
    let (boot, from_backup) = choose(source, offset)?;
    let back = match boot.fs {
        FsKind::Fat32 => FAT_BACKUP_SECTOR,
        FsKind::Exfat => EXFAT_BACKUP_SECTOR,
        _ => 0,
    } * boot.bps;
    if !from_backup
        && back > 0
        && let Some(earlier) = offset.checked_sub(back)
        && let Some((other, other_from_backup)) = choose(source, earlier)
        && other == boot
        && (!other_from_backup
            || (fat_starts(source, earlier, &other) && !fat_starts(source, offset, &boot)))
    {
        return volume(source, earlier, other, other_from_backup, Scheme::None);
    }
    volume(source, offset, boot, from_backup, Scheme::None)
}

/// Whether FAT entry 0 at the volume's first FAT holds the media byte followed by set bits (fatgen103 §4; exFAT §4.1:
/// `0xFFFFFFF8`).
fn fat_starts(source: &dyn Source, offset: u64, b: &Boot) -> bool {
    let Some(at) = b
        .fat_start
        .checked_mul(b.bps)
        .and_then(|x| x.checked_add(offset))
    else {
        return false;
    };
    read_exact(source, at, 512).is_some_and(|f| f[0] == b.media && f[1] == 0xFF && f[2] == 0xFF)
}

fn volume(
    source: &dyn Source,
    offset: u64,
    b: Boot,
    from_backup: bool,
    scheme: Scheme,
) -> Option<Volume> {
    let cluster_size = b.spc * b.bps;
    let heap_offset = offset.checked_add(b.heap * b.bps)?;
    let mut v = Volume {
        index: 0,
        fs: b.fs,
        offset,
        len: b.total.checked_mul(b.bps)?,
        sector_size: b.bps as u32,
        cluster_size,
        heap_offset,
        cluster_count: b.count,
        label: None,
        serial: b.serial,
        from_backup,
        scheme,
    };
    v.label = match b.fs {
        FsKind::Exfat => exfat_label(source, &v, &b),
        _ => fat_label(source, &v, &b),
    };
    Some(v)
}

/// The volume-label entry among the root's first entries, else the BPB label.
fn fat_label(source: &dyn Source, v: &Volume, b: &Boot) -> Option<String> {
    let (at, entries) = match b.root {
        Root::Fixed { sector, entries } => {
            (v.offset + sector * b.bps, entries.min(FAT_LABEL_ENTRIES))
        }
        Root::Cluster(c) => (v.cluster_offset(c)?, FAT_LABEL_ENTRIES),
    };
    let from_root = read_exact(source, at, entries as usize * 32).and_then(|root| {
        for e in root.as_chunks::<32>().0 {
            match (e[0], e[11]) {
                (0x00, _) => break,
                (0xE5, _) => {}
                (_, attr) if attr & 0x0F == 0x0F => {}
                (_, attr) if attr & 0x18 == 0x08 => return Some(fat_text(&e[..11])),
                _ => {}
            }
        }
        None
    });
    named(from_root.or_else(|| b.bpb_label.map(|l| fat_text(&l))))
}

/// An 11-byte FAT name as text: printable ASCII kept, any other byte (an OEM code page character) read as `_`,
/// trailing spaces dropped.
fn fat_text(bytes: &[u8]) -> String {
    let s: String = bytes
        .iter()
        .map(|&c| {
            if (0x20..0x7F).contains(&c) {
                c as char
            } else {
                '_'
            }
        })
        .collect();
    s.trim_end_matches(' ').to_string()
}

/// The `0x83` volume-label entry among the root's first entries (up to the end-of-directory marker).
fn exfat_label(source: &dyn Source, v: &Volume, b: &Boot) -> Option<String> {
    let Root::Cluster(c) = b.root else {
        return None;
    };
    let len = v.cluster_size.min(EXFAT_LABEL_SCAN) as usize;
    let root = read_exact(source, v.cluster_offset(c)?, len)?;
    for e in root.as_chunks::<32>().0 {
        match e[0] {
            0x00 => break,
            0x83 => {
                let n = (e[1] as usize).min(11);
                let units: Vec<u16> = (0..n).map(|i| u16_at(e, 2 + 2 * i)).collect();
                let text: String = String::from_utf16_lossy(&units)
                    .chars()
                    .map(|c| if c.is_control() { '_' } else { c })
                    .collect();
                return named(Some(text));
            }
            _ => {}
        }
    }
    None
}

/// `None` for an empty label or the formatters' "no label" placeholder.
fn named(label: Option<String>) -> Option<String> {
    label.filter(|l| !l.is_empty() && l != "NO NAME")
}
