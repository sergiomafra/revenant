//! The disks of the machine: `disks`, `preflight`, `unmount`, `remount`, and what `scan`, `copy` and `digest` share to
//! tell a disk name from a file and to open a disk.

use std::path::Path;

use revenant_core::EngineError;
use revenant_core::device::{DeviceOps, DiskInfo, UnmountGuard, VolumeInfo};
use revenant_core::preflight::{Issue, Preflight, PreflightRequest, SAMPLE_TIMEOUT, preflight};
use revenant_core::source::{DeviceSource, DiskId};
use serde::Serialize;

use crate::Exit;
use crate::output::emit_json;

/// What a SOURCE argument names: `Some(disk)` for a whole disk ("disk4", "/dev/rdisk4", "PhysicalDrive2",
/// r"\\.\PhysicalDrive2"), `None` for a file. A name that looks like a disk but is not a whole one ("disk4s1",
/// "/dev/disk4s1") and is not an existing file is `not_whole_disk`.
pub(crate) fn device_name(source: &Path) -> Result<Option<DiskId>, EngineError> {
    let Some(s) = source.to_str() else {
        return Ok(None);
    };
    match DiskId::parse(s) {
        Ok(disk) => Ok(Some(disk)),
        Err(e) => {
            let lower = s.to_ascii_lowercase();
            let looks_like_disk = lower.starts_with("disk")
                || lower.starts_with("/dev/")
                || lower.starts_with(r"\\.\")
                || lower.starts_with("physicaldrive");
            if looks_like_disk && !source.exists() {
                Err(e)
            } else {
                Ok(None)
            }
        }
    }
}

/// The listed disk `disk`: `device_not_found` when it is not connected, `system_disk` when it holds the running system
/// (never read, unmounted or copied by these commands).
pub(crate) fn find_disk(ops: &dyn DeviceOps, disk: &DiskId) -> Result<DiskInfo, EngineError> {
    let info = ops
        .list()?
        .into_iter()
        .find(|d| d.id == *disk)
        .ok_or(EngineError::DeviceNotFound)?;
    if info.system {
        return Err(EngineError::SystemDisk);
    }
    Ok(info)
}

/// `disk` found ([`find_disk`]) and opened read-only.
pub(crate) fn open_device(
    ops: &dyn DeviceOps,
    disk: &DiskId,
) -> Result<(DeviceSource, DiskInfo), EngineError> {
    let info = find_disk(ops, disk)?;
    let raw = ops.open(disk)?;
    Ok((DeviceSource::new(raw, disk.clone()), info))
}

#[derive(Serialize)]
struct VolumeInfoLine<'a> {
    id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    fs: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mount_point: Option<String>,
    size: u64,
}

/// `{"event":"disk",…}`: options the system does not know are left out, except `solid_state` (`null`: unknown).
#[derive(Serialize)]
struct DiskLine<'a> {
    event: &'static str,
    id: &'a str,
    size: u64,
    logical_sector: u32,
    physical_sector: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    vendor: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    serial: Option<&'a str>,
    removable: bool,
    internal: bool,
    system: bool,
    #[serde(rename = "virtual")]
    virtual_disk: bool,
    solid_state: Option<bool>,
    volumes: Vec<VolumeInfoLine<'a>>,
}

fn volume_info_line(v: &VolumeInfo) -> VolumeInfoLine<'_> {
    VolumeInfoLine {
        id: &v.id,
        fs: v.fs.as_deref(),
        label: v.label.as_deref(),
        mount_point: v.mount_point.as_ref().map(|p| p.display().to_string()),
        size: v.size,
    }
}

fn disk_line(d: &DiskInfo) -> DiskLine<'_> {
    DiskLine {
        event: "disk",
        id: d.id.as_str(),
        size: d.geometry.size,
        logical_sector: d.geometry.logical_sector,
        physical_sector: d.geometry.physical_sector,
        vendor: d.vendor.as_deref(),
        model: d.model.as_deref(),
        serial: d.serial.as_deref(),
        removable: d.removable,
        internal: d.internal,
        system: d.system,
        virtual_disk: d.virtual_disk,
        solid_state: d.solid_state,
        volumes: d.volumes.iter().map(volume_info_line).collect(),
    }
}

/// `disks`: one line per connected whole disk.
pub(crate) fn run_disks(ops: &dyn DeviceOps) -> Result<Exit, EngineError> {
    for d in ops.list()? {
        if !emit_json(&disk_line(&d)) {
            break;
        }
    }
    Ok(Exit::Done)
}

/// One issue of the pre-flight check: its code and its fields.
#[derive(Serialize)]
struct IssueLine {
    code: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    offset: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    volumes: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    need: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    free: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    limit: Option<u64>,
}

fn issue_line(issue: &Issue) -> IssueLine {
    let mut line = IssueLine {
        code: issue.code(),
        offset: None,
        volumes: None,
        need: None,
        free: None,
        limit: None,
    };
    match issue {
        Issue::NotResponding { offset } | Issue::SampleUnreadable { offset } => {
            line.offset = Some(*offset)
        }
        Issue::UnmountFailed { volumes } => line.volumes = Some(volumes.clone()),
        Issue::DestinationTooSmall { need, free } => {
            line.need = Some(*need);
            line.free = Some(*free);
        }
        Issue::DestinationFileTooLarge { limit } => line.limit = Some(*limit),
        _ => {}
    }
    line
}

#[derive(Serialize)]
struct PreflightLine<'a> {
    event: &'static str,
    disk: &'a str,
    issues: Vec<IssueLine>,
}

/// Prints the `preflight` line of `issues`.
pub(crate) fn emit_preflight(disk: &DiskId, issues: &[Issue]) {
    emit_json(&PreflightLine {
        event: "preflight",
        disk: disk.as_str(),
        issues: issues.iter().map(issue_line).collect(),
    });
}

/// The pre-flight check of `disk` for a copy into `copy_to` and/or a session in `session` (its `guard`, when it unmounted
/// the disk, is to be kept while the disk is read).
pub(crate) fn check_disk(
    ops: &dyn DeviceOps,
    disk: &DiskId,
    copy_to: Option<&Path>,
    session: Option<&Path>,
    unmount: bool,
) -> Preflight {
    let req = PreflightRequest {
        disk,
        unmount,
        copy_to,
        session,
        sample_timeout: SAMPLE_TIMEOUT,
    };
    preflight(ops, &req)
}

/// `preflight`: one line; exit 2 when anything is wrong.
pub(crate) fn run_preflight(
    ops: &dyn DeviceOps,
    disk: &str,
    copy_to: Option<&Path>,
    session: Option<&Path>,
    no_unmount: bool,
) -> Result<Exit, EngineError> {
    let disk = DiskId::parse(disk)?;
    let checked = check_disk(ops, &disk, copy_to, session, !no_unmount);
    emit_preflight(&disk, &checked.issues);
    if checked.guard.is_some() {
        remount_hint(&disk);
    }
    Ok(if checked.issues.is_empty() {
        Exit::Done
    } else {
        Exit::Issues
    })
}

#[derive(Serialize)]
struct UnmountLine<'a> {
    event: &'static str,
    disk: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    held: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    volumes: Option<Vec<String>>,
}

/// Prints the `unmount` line of an unmount's result and returns its guard: `held` on success; on failure the error's
/// code and the volumes still mounted (those `unmount_failed` names, else the listed volumes with a mount point).
pub(crate) fn report_unmount(
    disk: &DiskId,
    info: Option<&DiskInfo>,
    result: Result<UnmountGuard, EngineError>,
) -> Option<UnmountGuard> {
    let mut line = UnmountLine {
        event: "unmount",
        disk: disk.as_str(),
        held: None,
        error: None,
        volumes: None,
    };
    let guard = match result {
        Ok(guard) => {
            line.held = Some(guard.held());
            Some(guard)
        }
        Err(e) => {
            line.error = Some(e.code());
            line.volumes = Some(match e {
                EngineError::UnmountFailed { volumes } => volumes,
                _ => info
                    .map(|i| {
                        i.volumes
                            .iter()
                            .filter(|v| v.mount_point.is_some())
                            .map(|v| v.id.clone())
                            .collect()
                    })
                    .unwrap_or_default(),
            });
            eprintln!(
                "warning: {} is still mounted ({}): the system may write to it while it is read",
                disk.as_str(),
                line.error.unwrap_or_default()
            );
            None
        }
    };
    emit_json(&line);
    guard
}

/// `unmount`: the disk's volumes unmounted without ejecting; error codes exit 1.
pub(crate) fn run_unmount(ops: &dyn DeviceOps, disk: &str) -> Result<Exit, EngineError> {
    let disk = DiskId::parse(disk)?;
    let guard = ops.unmount(&disk)?;
    emit_json(&UnmountLine {
        event: "unmount",
        disk: disk.as_str(),
        held: Some(guard.held()),
        error: None,
        volumes: None,
    });
    remount_hint(&disk);
    Ok(Exit::Done)
}

#[derive(Serialize)]
struct RemountLine<'a> {
    event: &'static str,
    disk: &'a str,
}

/// `remount`: the disk's volumes mounted again.
pub(crate) fn run_remount(ops: &dyn DeviceOps, disk: &str) -> Result<Exit, EngineError> {
    let disk = DiskId::parse(disk)?;
    ops.remount(&disk, None)?;
    emit_json(&RemountLine {
        event: "remount",
        disk: disk.as_str(),
    });
    Ok(Exit::Done)
}

/// Tells (on stderr) how to mount the disk again: nothing remounts it by itself.
pub(crate) fn remount_hint(disk: &DiskId) {
    eprintln!(
        "{d} was unmounted and is never remounted automatically (on Windows the system may mount it again once this \
         command ends); to mount it again: revenant-dev remount {d}",
        d = disk.as_str()
    );
}
