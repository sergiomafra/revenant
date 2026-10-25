//! Extraction: copies a finding's bytes from the source into a destination folder.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::EngineError;
use crate::naming::{fallback_name, file_name, sanitize, split_name};
use crate::scan::Finding;
use crate::source::{Source, volume_of};

/// Copy granularity.
const CHUNK: usize = 1024 * 1024;
/// Granularity of the sector-by-sector retry after a failed chunk read.
const SECTOR: u64 = 512;
/// How many `_N` suffixes are tried before giving up.
const MAX_NAME_ATTEMPTS: u32 = 10_000;

/// Copies the finding's bytes into `dest_dir` under [`file_name`]`(finding, None)`: the original name of a finding with
/// a file-system origin, else `revenant_<start as 12 hex digits>.<ext>` (`_2`, `_3`… if it exists; never overwrites).
/// Same as [`extract_as`].
pub fn extract(
    source: &dyn Source,
    finding: &Finding,
    dest_dir: &Path,
) -> Result<PathBuf, EngineError> {
    extract_as(source, finding, dest_dir, &file_name(finding, None))
}

/// Copies the finding's bytes into `dest_dir` under [`sanitize`]`(name)` (`revenant_<start as 12 hex digits>.<ext>` when
/// nothing is left of it), then `<stem>_2.<ext>`, `<stem>_3.<ext>`… when the name is taken (10,000 tries; never
/// overwrites). Refuses if the directory is on the source's volume.
///
/// The file is created with `create_new`, so a taken name is detected by the volume itself: it is right on case-sensitive
/// and case-insensitive volumes alike and for different Unicode forms of one name. The result is always a single
/// component inside `dest_dir`.
///
/// A file found in pieces is written as its pieces in file order. Stretches that cannot be read are written as zeros, so a
/// partial file comes out instead of none. Each piece stops at the source's end even if it runs past it.
/// A write error returns [`EngineError::Write`] and removes the incomplete file.
pub fn extract_as(
    source: &dyn Source,
    finding: &Finding,
    dest_dir: &Path,
    name: &str,
) -> Result<PathBuf, EngineError> {
    let dest_volume = volume_of(dest_dir)?;
    if source.volume() == Some(dest_volume) {
        return Err(EngineError::DestinationOnSource);
    }

    let name = sanitize(name).unwrap_or_else(|| fallback_name(finding));
    let (stem, suffix) = split_name(&name);
    let (path, mut file) = create_unique(dest_dir, stem, suffix)?;
    if let Err(cause) = copy(source, finding, &mut file) {
        drop(file);
        let _ = std::fs::remove_file(&path);
        return Err(EngineError::Write { cause });
    }
    Ok(path)
}

/// Creates `<stem><suffix>` in `dir`, else `<stem>_2<suffix>`, `<stem>_3<suffix>`… (never overwriting; at most 10,000
/// names tried). `stem` and `suffix` must already be safe name parts.
pub fn create_unique(dir: &Path, stem: &str, suffix: &str) -> Result<(PathBuf, File), EngineError> {
    for n in 1..=MAX_NAME_ATTEMPTS {
        let name = if n == 1 {
            format!("{stem}{suffix}")
        } else {
            format!("{stem}_{n}{suffix}")
        };
        let path = dir.join(name);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(cause) => return Err(EngineError::Write { cause }),
        }
    }
    Err(EngineError::Write {
        cause: std::io::Error::other("no free file name"),
    })
}

/// Copies the finding's ranges in file order.
fn copy(source: &dyn Source, finding: &Finding, out: &mut File) -> std::io::Result<()> {
    let mut buf = vec![0u8; CHUNK];
    for range in finding.ranges() {
        // Never past the source's end, whatever the finding claims.
        let end = range.start.saturating_add(range.len).min(source.len());
        let mut pos = range.start;
        while pos < end {
            let n = (end - pos).min(CHUNK as u64) as usize;
            read_lenient(source, pos, &mut buf[..n]);
            out.write_all(&buf[..n])?;
            pos += n as u64;
        }
    }
    out.flush()
}

/// Fills `buf` from `offset`; anything unreadable (or missing past the source's end) becomes zeros.
fn read_lenient(source: &dyn Source, offset: u64, buf: &mut [u8]) {
    match source.read_at(offset, buf) {
        Ok(n) => buf[n..].fill(0),
        Err(_) => {
            // Retry sector by sector (aligned to absolute sector boundaries) so only bad sectors are lost.
            let mut done = 0;
            while done < buf.len() {
                let abs = offset + done as u64;
                let step = ((SECTOR - abs % SECTOR) as usize).min(buf.len() - done);
                let piece = &mut buf[done..done + step];
                match source.read_at(abs, piece) {
                    Ok(n) => piece[n..].fill(0),
                    Err(_) => piece.fill(0),
                }
                done += step;
            }
        }
    }
}
