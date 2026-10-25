//! Finding a card again after it was disconnected (a loose cable, the computer sleeping) and unmounting it before reading
//! (ruling D11, P17).
//!
//! The system mounts a card again by itself when it comes back, and may write to it (`.fseventsd`, a dirty flag) before
//! it is unmounted again: that window, which includes reading the identity sectors, is not closed here (refusing the
//! mount for the session's length is the Module 2.1 helper's job).

use std::time::{Duration, Instant};

use super::{DeviceOps, DiskInfo, UnmountGuard};
use crate::preflight::SAMPLE_TIMEOUT;
use crate::scan::CancelToken;
use crate::source::{DeviceSource, DiskId};
use crate::timed::{Timed, timed};
use crate::{EngineError, SourceIdentity};

/// How often a wait looks at its cancel token.
const TICK: Duration = Duration::from_millis(10);

/// A card found again.
pub struct Reconnected {
    /// The card, opened again (read-only) after the unmount.
    pub source: DeviceSource,
    /// The unmount, done before anything but the identity sectors was read; when it failed, the card is read mounted
    /// (the system may write to it).
    pub unmount: Result<UnmountGuard, EngineError>,
}

/// Waits for the card `identity` (a device's) to come back (D11, P17). Every `poll`: lists the disks with the same size
/// and model (and serial when both have one), skipping the system disk; opens each candidate READ-ONLY just to read its
/// identity sectors (on a thread, waited for at most [`SAMPLE_TIMEOUT`]) and compares with
/// [`SourceIdentity::same_source`]; no match → closes it, unmounts nothing, keeps waiting; match → closes it, unmounts it
/// BEFORE any other read, THEN opens it again. Nothing but identity sectors is read before the unmount.
///
/// Errors: `cancelled` (checked while waiting, and while an identity read is pending), `device_not_found` for an identity
/// that is not a device's, `unsupported_platform` from the listing, and any error of opening the matched card again
/// (a listing that fails otherwise, or a candidate that cannot be opened or read, only means waiting longer).
pub fn reconnect(
    ops: &dyn DeviceOps,
    identity: &SourceIdentity,
    cancel: &CancelToken,
    poll: Duration,
) -> Result<Reconnected, EngineError> {
    if !matches!(identity, SourceIdentity::Device { .. }) {
        return Err(EngineError::DeviceNotFound);
    }
    loop {
        if cancel.is_cancelled() {
            return Err(EngineError::Cancelled);
        }
        match ops.list() {
            Ok(disks) => {
                for info in disks.into_iter().filter(|d| candidate(d, identity)) {
                    if is_same(ops, info.clone(), identity, cancel)? {
                        return finish(ops, info.id);
                    }
                }
            }
            Err(EngineError::Unsupported) => return Err(EngineError::Unsupported),
            Err(_) => {}
        }
        wait(poll, cancel)?;
    }
}

/// Whether the listed disk `info` could be the card `identity`: not the system disk, same size and model, and the same
/// serial when both have one.
fn candidate(info: &DiskInfo, identity: &SourceIdentity) -> bool {
    let SourceIdentity::Device {
        size,
        model,
        serial,
        ..
    } = identity
    else {
        return false;
    };
    let serials_agree = match (serial, &info.serial) {
        (Some(a), Some(b)) => a == b,
        _ => true,
    };
    !info.system && info.geometry.size == *size && info.model == *model && serials_agree
}

/// Opens the candidate read-only, reads its identity sectors on a thread and closes it. A candidate that cannot be
/// opened, read or that does not answer in time is not the card.
fn is_same(
    ops: &dyn DeviceOps,
    info: DiskInfo,
    identity: &SourceIdentity,
    cancel: &CancelToken,
) -> Result<bool, EngineError> {
    let Ok(raw) = ops.open(&info.id) else {
        return Ok(false);
    };
    let source = DeviceSource::new(raw, info.id.clone());
    let read = timed(SAMPLE_TIMEOUT, Some(cancel), move || {
        let found = SourceIdentity::of_device(&info, &source);
        // Closed before the unmount.
        drop(source);
        found
    });
    match read {
        Timed::Done(Ok(found)) => Ok(identity.same_source(&found)),
        Timed::Done(Err(_)) | Timed::Late => Ok(false),
        Timed::Cancelled => Err(EngineError::Cancelled),
    }
}

/// The card is back as `disk`: unmounts it, then opens it again.
fn finish(ops: &dyn DeviceOps, disk: DiskId) -> Result<Reconnected, EngineError> {
    let unmount = ops.unmount(&disk);
    let raw = ops.open(&disk)?;
    Ok(Reconnected {
        source: DeviceSource::new(raw, disk),
        unmount,
    })
}

/// Sleeps `poll`, looking at `cancel` every [`TICK`].
fn wait(poll: Duration, cancel: &CancelToken) -> Result<(), EngineError> {
    let until = Instant::now() + poll;
    loop {
        if cancel.is_cancelled() {
            return Err(EngineError::Cancelled);
        }
        let now = Instant::now();
        if now >= until {
            return Ok(());
        }
        std::thread::sleep((until - now).min(TICK));
    }
}
