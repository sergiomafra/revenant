//! FAT12/16/32 and exFAT volumes of a source: where they are, their geometry, and the files they list.
//!
//! Formats from the public specifications only: the MBR/EBR conventions, the GPT chapter of the UEFI specification,
//! Microsoft's "FAT32 File System Specification" 1.03 (fatgen103) and Microsoft's "exFAT file system specification".
//! Nothing here writes, and a broken table or boot sector is never an error: it only yields fewer volumes.

mod alloc;
mod boot;
mod exfat;
mod fat;
mod fat_deleted;
mod listing;
mod orphans;
mod partitions;

use crate::EngineError;
use crate::cache::CachedSource;
use crate::source::Source;
use partitions::Candidate;

pub(crate) use listing::read_with;
pub use listing::{FsFile, FsOrigin, FsState, FsTime, Listing, Placement, read};
pub(crate) use orphans::{MAX_HEAD, OrphanDir, OrphanFinder, head_len, kind_class, orphan_entries};

/// Most volumes reported for one source (the lowest offsets are kept).
pub(crate) const MAX_VOLUMES: usize = 16;
/// Most EBRs read while following the extended partitions of one MBR (all chains together).
pub(crate) const MAX_EBR_LINKS: usize = 64;
/// Most entries a GPT may declare; a header declaring more is refused.
pub(crate) const MAX_GPT_ENTRIES: u32 = 128;
/// Most photo/video files one listing holds (later ones are not listed).
pub(crate) const MAX_FILES: usize = 262_144;
/// Most pieces one listing holds: the runs of every listed file (a photo/video's pieces, an other file's ranges in
/// `live`) and of every directory count; a file (or a directory's ranges) that would pass it is not listed.
pub const MAX_PIECES: usize = 1 << 20;
/// Most directories walked per volume.
pub(crate) const MAX_DIRS: usize = 65_536;
/// Most directory entries (32-byte slots) parsed per volume.
pub(crate) const MAX_ENTRIES: u64 = 4_000_000;
/// Deepest directory walked (the root is depth 0).
pub(crate) const MAX_DEPTH: u32 = 32;
/// Most bytes of one FAT directory read (65,536 entries).
pub(crate) const MAX_FAT_DIR_BYTES: u64 = 2 << 20;
/// Most bytes of one exFAT directory read (8,388,608 entries; the entry cap per volume comes first).
pub(crate) const MAX_EXFAT_DIR_BYTES: u64 = 256 << 20;
/// Most contested clusters recorded per volume; past it the volume's live files are all untrusted (ruling C11).
pub(crate) const MAX_CONTESTED: usize = 65_536;
/// Longest path (folder + '/' + name, in UTF-8 bytes) a listed file keeps; past it the file keeps its name and loses its
/// folder (ruling R6).
pub const MAX_PATH_BYTES: usize = 4096;
/// Bytes of folder strings one listing may hold (each folder is held once and shared by its files); past it, files are
/// listed without a folder (ruling R6).
pub(crate) const MAX_PATH_BUDGET: usize = 64 << 20;
/// Volumes with more clusters are reported but not walked.
pub(crate) const MAX_WALKED_CLUSTERS: u32 = 1 << 28;
/// Cluster examinations per volume shared by the deleted-file guesses (ruling C7); once spent, the volume's remaining
/// deleted files are not listed.
pub(crate) const MAX_GUESS_STEPS: u64 = 1 << 26;
/// Most old-directory candidates the scan keeps (ruling D11; 16 B each), the first ones in commit order.
pub(crate) const MAX_ORPHAN_DIRS: usize = 65_536;
/// Head checks of 512 bytes through the [`Probe`] per listing (ruling D10/C2): 32 MiB read outside the cache at most.
pub(crate) const MAX_PROBES: usize = 65_536;

/// The bounds of one listing: [`Limits::DEFAULT`] holds the constants above; unit tests pass lowered copies (the
/// `cfg(test)` twins), which integration tests cannot reach.
#[derive(Clone, Debug)]
pub(crate) struct Limits {
    pub files: usize,
    pub pieces: usize,
    pub dirs: usize,
    pub entries: u64,
    pub depth: u32,
    pub fat_dir_bytes: u64,
    pub exfat_dir_bytes: u64,
    pub contested: usize,
    pub path_bytes: usize,
    pub path_budget: usize,
    pub guess_steps: u64,
    pub probes: usize,
}

impl Limits {
    pub(crate) const DEFAULT: Limits = Limits {
        files: MAX_FILES,
        pieces: MAX_PIECES,
        dirs: MAX_DIRS,
        entries: MAX_ENTRIES,
        depth: MAX_DEPTH,
        fat_dir_bytes: MAX_FAT_DIR_BYTES,
        exfat_dir_bytes: MAX_EXFAT_DIR_BYTES,
        contested: MAX_CONTESTED,
        path_bytes: MAX_PATH_BYTES,
        path_budget: MAX_PATH_BUDGET,
        guess_steps: MAX_GUESS_STEPS,
        probes: MAX_PROBES,
    };
}

/// Reads the head of a cluster for the FAT32 high-word check (ruling D10): 512 bytes from the raw source, outside the
/// scan's cache. `None` means "does not recognize": a read error, the end of the source, or (ruling C2) a range the
/// cache already knows to hold an unreadable sector, which is then not read at all. Beside the scan's cache, a source
/// that vanished (`DeviceGone`, or a short read before its length) is reported to the cache (ruling P3), so the scan
/// stops with `device_gone` instead of using the listing; and once the scan is cancelled or the source has vanished
/// nothing is read at all (each check would be one more slow read of a dying card).
pub(crate) trait Probe {
    fn read512(&self, at: u64) -> Option<[u8; 512]>;
}

/// A [`Probe`] over a source, refusing what `known_bad`'s bad-sector map holds.
pub(crate) struct SourceProbe<'a, 'c> {
    source: &'a dyn Source,
    known_bad: Option<&'a CachedSource<'c>>,
}

impl<'a, 'c> SourceProbe<'a, 'c> {
    /// Reads `source` itself (what [`read`] uses).
    pub(crate) fn new(source: &'a dyn Source) -> Self {
        SourceProbe {
            source,
            known_bad: None,
        }
    }

    /// Reads `raw`, the source under `cache`, never a range `cache` knows to be unreadable (the scan's probe).
    pub(crate) fn beside(raw: &'a dyn Source, cache: &'a CachedSource<'c>) -> Self {
        SourceProbe {
            source: raw,
            known_bad: Some(cache),
        }
    }
}

impl Probe for SourceProbe<'_, '_> {
    fn read512(&self, at: u64) -> Option<[u8; 512]> {
        let end = at.checked_add(512)?;
        if let Some(cache) = self.known_bad
            && (cache.stopped() || !cache.bad_runs(at, end).is_empty())
        {
            return None;
        }
        if end > self.source.len() {
            return None;
        }
        let mut buf = [0u8; 512];
        let gone = match self.source.read_at(at, &mut buf) {
            Ok(512) => return Some(buf),
            Ok(n) => at + n as u64,
            Err(EngineError::DeviceGone { offset }) => offset,
            Err(_) => return None,
        };
        if let Some(cache) = self.known_bad {
            cache.mark_gone(gone);
        }
        None
    }
}

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

    /// 1 MiB of zeros that has vanished past `gone` (`DeviceGone`), or reads short past it (`short`); counts its reads.
    struct Vanishing {
        gone: u64,
        short: bool,
        reads: std::sync::atomic::AtomicU64,
    }

    impl Vanishing {
        fn new(gone: u64, short: bool) -> Self {
            Vanishing {
                gone,
                short,
                reads: Default::default(),
            }
        }

        fn reads(&self) -> u64 {
            self.reads.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl Source for Vanishing {
        fn len(&self) -> u64 {
            1 << 20
        }

        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, EngineError> {
            self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let end = offset + buf.len() as u64;
            if end <= self.gone {
                buf.fill(0);
                Ok(buf.len())
            } else if self.short {
                let n = self.gone.saturating_sub(offset) as usize;
                buf[..n].fill(0);
                Ok(n)
            } else {
                Err(EngineError::DeviceGone { offset })
            }
        }

        fn volume(&self) -> Option<crate::source::VolumeId> {
            None
        }
    }

    #[test]
    fn probe_reports_a_vanished_source_to_the_cache() {
        for short in [false, true] {
            let raw = Vanishing::new(8192, short);
            let cache = CachedSource::new(&raw, 512, 1 << 20);
            let probe = SourceProbe::beside(&raw, &cache);
            assert!(probe.read512(4096).is_some());
            assert_eq!(cache.gone(), None);
            assert!(probe.read512(8192 - 256).is_none());
            let want = if short { 8192 } else { 8192 - 256 };
            assert_eq!(cache.gone(), Some(want), "{short}");
        }
    }

    #[test]
    fn probe_beside_a_stopped_cache_reads_nothing() {
        // Cancelled.
        let raw = Vanishing::new(u64::MAX, false);
        let cancel = crate::scan::CancelToken::default();
        let cache = CachedSource::new(&raw, 512, 1 << 20).with_cancel(cancel.clone());
        let probe = SourceProbe::beside(&raw, &cache);
        assert!(probe.read512(4096).is_some());
        let before = raw.reads();
        cancel.cancel();
        assert!(probe.read512(4096).is_none());
        assert_eq!(raw.reads(), before);
        // Gone (seen by the cache).
        let raw = Vanishing::new(u64::MAX, false);
        let cache = CachedSource::new(&raw, 512, 1 << 20);
        cache.mark_gone(1 << 19);
        let probe = SourceProbe::beside(&raw, &cache);
        assert!(probe.read512(4096).is_none());
        assert_eq!(raw.reads(), 0);
    }

    #[test]
    fn crc32_check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }
}
