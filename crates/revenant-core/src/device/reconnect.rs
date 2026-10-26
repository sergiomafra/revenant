//! Finding a card again after it was disconnected (a loose cable, the computer sleeping) and unmounting it before reading
//! (ruling D11, P17).
//!
//! The system mounts a card again by itself when it comes back, and may write to it (`.fseventsd`, a dirty flag) before
//! it is unmounted again: that window, which includes reading the identity sectors, is not closed here (refusing the
//! mount for the session's length is the Module 2.1 helper's job).

use std::collections::HashSet;
use std::panic::AssertUnwindSafe;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use super::{DeviceOps, DiskInfo, UnmountGuard};
use crate::preflight::SAMPLE_TIMEOUT;
use crate::scan::CancelToken;
use crate::source::{DeviceSource, DiskId};
use crate::timed::{Timed, timed};
use crate::{EngineError, SourceIdentity};

/// How often a wait looks at its cancel token.
const TICK: Duration = Duration::from_millis(10);
/// Most identity probes left running at once (each one a thread holding a candidate open): a disk whose reads hang
/// costs one probe, never one per poll.
const MAX_PROBES: usize = 16;

/// A card found again.
pub struct Reconnected {
    /// The card, opened again (read-only) after the unmount, and confirmed to be the card by its identity sectors.
    pub source: DeviceSource,
    /// The unmount, done before anything but the identity sectors was read; when it failed, the card is read mounted
    /// (the system may write to it).
    pub unmount: Result<UnmountGuard, EngineError>,
}

/// Waits for the card `identity` (a device's) to come back (D11, P17). Every `poll`: lists the disks with the same size
/// and model (and serial when both have one), skipping the system disk; opens each candidate READ-ONLY just to read its
/// identity sectors, on a thread of its own (a probe), and compares with [`SourceIdentity::same_source`] when the probe
/// answers. No match → the probe has closed it, nothing is unmounted, waiting goes on. Match → unmounts it BEFORE any
/// other read, THEN opens it again and reads its identity once more (after the unmount, P17): still the card → returned;
/// not (another card took its name meanwhile) → closed, waiting goes on. Nothing but identity sectors is read before the
/// unmount.
///
/// A probe that does not answer (a disk whose reads hang) is never waited for: the other candidates are probed meanwhile,
/// and that disk is not opened again until its probe returns. At most [`MAX_PROBES`] probes run at once; a probe left
/// blocked keeps its thread and the candidate open until the system gives up on the read.
///
/// Errors: `cancelled` (checked while waiting), `device_not_found` for an identity that is not a device's,
/// `unsupported_platform` from the listing, and any error of opening the matched card again (a listing that fails
/// otherwise, or a candidate that cannot be opened or read, only means waiting longer).
pub fn reconnect(
    ops: &dyn DeviceOps,
    identity: &SourceIdentity,
    cancel: &CancelToken,
    poll: Duration,
) -> Result<Reconnected, EngineError> {
    if !matches!(identity, SourceIdentity::Device { .. }) {
        return Err(EngineError::DeviceNotFound);
    }
    let (tx, rx) = mpsc::channel();
    let mut probing: HashSet<DiskId> = HashSet::new();
    loop {
        if cancel.is_cancelled() {
            return Err(EngineError::Cancelled);
        }
        match ops.list() {
            Ok(disks) => {
                for info in disks.into_iter().filter(|d| candidate(d, identity)) {
                    if probing.len() < MAX_PROBES
                        && !probing.contains(&info.id)
                        && probe(ops, info.clone(), &tx)
                    {
                        probing.insert(info.id);
                    }
                }
            }
            Err(EngineError::Unsupported) => return Err(EngineError::Unsupported),
            Err(_) => {}
        }
        if let Some(found) = answers(ops, identity, cancel, poll, &rx, &mut probing)? {
            return Ok(found);
        }
    }
}

/// What a probe answers: the disk and the identity read from it.
type Answer = (DiskId, Result<SourceIdentity, EngineError>);

/// Opens the candidate read-only and starts a thread that reads its identity sectors, closes it and sends the answer.
/// Whether the probe started (a candidate that cannot be opened is skipped this poll).
fn probe(ops: &dyn DeviceOps, info: DiskInfo, tx: &Sender<Answer>) -> bool {
    let Ok(raw) = ops.open(&info.id) else {
        return false;
    };
    let id = info.id.clone();
    let source = DeviceSource::new(raw, id.clone());
    let tx = tx.clone();
    std::thread::Builder::new()
        .name("revenant-reconnect".into())
        .spawn(move || {
            let read = std::panic::catch_unwind(AssertUnwindSafe(|| {
                SourceIdentity::of_device(&info, &source)
            }));
            // Closed before the answer is sent, so before any unmount.
            drop(source);
            let found = read.unwrap_or(Err(EngineError::DeviceNotFound));
            let _ = tx.send((id, found));
        })
        .is_ok()
}

/// Collects the probes' answers for `poll`; a match is unmounted, reopened and confirmed ([`finish`]).
fn answers(
    ops: &dyn DeviceOps,
    identity: &SourceIdentity,
    cancel: &CancelToken,
    poll: Duration,
    rx: &Receiver<Answer>,
    probing: &mut HashSet<DiskId>,
) -> Result<Option<Reconnected>, EngineError> {
    let until = Instant::now() + poll;
    loop {
        if cancel.is_cancelled() {
            return Err(EngineError::Cancelled);
        }
        let now = Instant::now();
        if now >= until {
            return Ok(None);
        }
        match rx.recv_timeout((until - now).min(TICK)) {
            Ok((disk, found)) => {
                probing.remove(&disk);
                if found.is_ok_and(|f| identity.same_source(&f))
                    && let Some(back) = finish(ops, disk, identity, cancel)?
                {
                    return Ok(Some(back));
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            // Never: `reconnect` keeps a sender.
            Err(RecvTimeoutError::Disconnected) => return Ok(None),
        }
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

/// The card is back as `disk`: unmounts it, then opens it again and reads its identity once more (on a thread, waited
/// for at most [`SAMPLE_TIMEOUT`]). `None` when it is no longer the card (or does not answer): it is closed and waiting
/// goes on (its volumes stay unmounted; nothing is remounted).
fn finish(
    ops: &dyn DeviceOps,
    disk: DiskId,
    identity: &SourceIdentity,
    cancel: &CancelToken,
) -> Result<Option<Reconnected>, EngineError> {
    let unmount = ops.unmount(&disk);
    let info = match ops.list() {
        Ok(listed) => listed.into_iter().find(|d| d.id == disk),
        Err(_) => None,
    };
    let Some(info) = info else {
        return Ok(None);
    };
    let source = DeviceSource::new(ops.open(&disk)?, disk);
    let checked = timed(SAMPLE_TIMEOUT, Some(cancel), move || {
        let found = SourceIdentity::of_device(&info, &source);
        (source, found)
    });
    match checked {
        Timed::Done((source, Ok(found))) if identity.same_source(&found) => {
            Ok(Some(Reconnected { source, unmount }))
        }
        Timed::Cancelled => Err(EngineError::Cancelled),
        _ => Ok(None),
    }
}
