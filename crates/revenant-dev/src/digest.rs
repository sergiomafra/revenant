//! `digest`: the FNV-1a 64 of a whole source, to compare a card with its copy.

use std::path::Path;

use revenant_core::EngineError;
use revenant_core::device::DeviceOps;
use revenant_core::source::{DeviceSource, FileSource, Source};
use serde::Serialize;

use crate::Exit;
use crate::devices::device_name;
use crate::output::emit_json;

/// Bytes read at once.
const CHUNK: usize = 1 << 20;
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// The FNV-1a 64 of `bytes`, continuing from `state`.
fn fnv1a64(state: u64, bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .fold(state, |h, &b| (h ^ u64::from(b)).wrapping_mul(FNV_PRIME))
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Digest {
    pub(crate) bytes: u64,
    pub(crate) fnv64: u64,
    pub(crate) unreadable_bytes: u64,
}

/// Reads `source` whole, in 1 MiB chunks; a chunk that fails is read again `sector` by `sector`, and each sector that
/// cannot be read is hashed as zeros and counted. A vanished source (`device_gone`, or a read coming back short) stops
/// it.
pub(crate) fn digest(source: &dyn Source, sector: u64) -> Result<Digest, EngineError> {
    let len = source.len();
    let mut d = Digest {
        bytes: len,
        fnv64: FNV_OFFSET,
        unreadable_bytes: 0,
    };
    let mut buf = vec![0u8; CHUNK];
    let mut pos = 0u64;
    while pos < len {
        let want = (len - pos).min(CHUNK as u64) as usize;
        match read_exact(source, pos, &mut buf[..want]) {
            Ok(()) => d.fnv64 = fnv1a64(d.fnv64, &buf[..want]),
            Err(e @ EngineError::DeviceGone { .. }) => return Err(e),
            Err(_) => {
                let mut at = 0usize;
                while at < want {
                    let n = (want - at).min(sector as usize);
                    let piece = &mut buf[at..at + n];
                    match read_exact(source, pos + at as u64, piece) {
                        Ok(()) => {}
                        Err(e @ EngineError::DeviceGone { .. }) => return Err(e),
                        Err(_) => {
                            piece.fill(0);
                            d.unreadable_bytes += n as u64;
                        }
                    }
                    d.fnv64 = fnv1a64(d.fnv64, piece);
                    at += n;
                }
            }
        }
        pos += want as u64;
    }
    Ok(d)
}

fn read_exact(source: &dyn Source, offset: u64, buf: &mut [u8]) -> Result<(), EngineError> {
    let n = source.read_at(offset, buf)?;
    if n < buf.len() {
        return Err(EngineError::DeviceGone {
            offset: offset + n as u64,
        });
    }
    Ok(())
}

#[derive(Serialize)]
struct DigestLine {
    event: &'static str,
    bytes: u64,
    fnv64: String,
    unreadable_bytes: u64,
}

/// `digest`: a disk (opened read-only, not unmounted) or a file read plainly (a map beside it is not used).
pub(crate) fn run_digest(ops: &dyn DeviceOps, source: &Path) -> Result<Exit, EngineError> {
    let d = match device_name(source)? {
        Some(disk) => {
            let device = DeviceSource::new(ops.open(&disk)?, disk);
            let sector = u64::from(device.scan_sector());
            digest(&device, sector)?
        }
        None => digest(&FileSource::open(source)?, 512)?,
    };
    emit_json(&DigestLine {
        event: "digest",
        bytes: d.bytes,
        fnv64: format!("{:016x}", d.fnv64),
        unreadable_bytes: d.unreadable_bytes,
    });
    Ok(Exit::Done)
}
