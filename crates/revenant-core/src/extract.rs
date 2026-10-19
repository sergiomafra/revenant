//! Extraction: copies a finding's bytes from the source into a destination folder.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::EngineError;
use crate::scan::Finding;
use crate::source::{Source, volume_of};

/// Copy granularity.
const CHUNK: usize = 1024 * 1024;
/// Granularity of the sector-by-sector retry after a failed chunk read.
const SECTOR: u64 = 512;
/// How many `_N` suffixes are tried before giving up.
const MAX_NAME_ATTEMPTS: u32 = 10_000;

/// Copies the finding's bytes into `dest_dir`, named `revenant_<start as 12 hex digits>.<ext>`
/// (`_2`, `_3`… if it exists; never overwrites). Refuses if the directory is on the source's volume.
///
/// Stretches that cannot be read are written as zeros, so a partial file comes out instead of none. The copy stops at the
/// source's end even if the finding's length runs past it.
/// A write error returns [`EngineError::Write`] and removes the incomplete file.
pub fn extract(
    source: &dyn Source,
    finding: &Finding,
    dest_dir: &Path,
) -> Result<PathBuf, EngineError> {
    let dest_volume = volume_of(dest_dir)?;
    if source.volume() == Some(dest_volume) {
        return Err(EngineError::DestinationOnSource);
    }

    let (path, mut file) = create_unique(finding, dest_dir)?;
    if let Err(cause) = copy(source, finding, &mut file) {
        drop(file);
        let _ = std::fs::remove_file(&path);
        return Err(EngineError::Write { cause });
    }
    Ok(path)
}

fn create_unique(finding: &Finding, dir: &Path) -> Result<(PathBuf, File), EngineError> {
    let ext = finding.kind.extension();
    for n in 1..=MAX_NAME_ATTEMPTS {
        let name = if n == 1 {
            format!("revenant_{:012x}.{ext}", finding.start)
        } else {
            format!("revenant_{:012x}_{n}.{ext}", finding.start)
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

fn copy(source: &dyn Source, finding: &Finding, out: &mut File) -> std::io::Result<()> {
    // Never past the source's end, whatever the finding claims.
    let end = finding.start.saturating_add(finding.len).min(source.len());
    let mut buf = vec![0u8; CHUNK];
    let mut pos = finding.start;
    while pos < end {
        let n = (end - pos).min(CHUNK as u64) as usize;
        read_lenient(source, pos, &mut buf[..n]);
        out.write_all(&buf[..n])?;
        pos += n as u64;
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
