//! `copy`: a card (or an image file) copied into an image file next to its rescue map, resumable.

use std::path::{Path, PathBuf};

use revenant_core::device::DeviceOps;
use revenant_core::rescue::{CopyEvent, CopyOptions, MapTotals, Pass, copy};
use revenant_core::source::{DeviceSource, DiskId, FileSource, Source, VolumeId};
use revenant_core::{CancelToken, Destination, EngineError, SourceIdentity};
use serde::Serialize;

use crate::Exit;
use crate::devices::{check_disk, check_regular_file, device_name, emit_preflight, remount_hint};
use crate::output::emit_json;
use crate::scan_cmd::UnknownVolume;

#[derive(clap::Args, Default)]
pub(crate) struct CopyArgs {
    /// A whole disk ("disk4" on macOS as root, "PhysicalDrive2" on Windows as Administrator) or an image file.
    #[arg(value_name = "SOURCE")]
    pub(crate) source: PathBuf,
    /// The image file to write (its map is `<IMAGE>.rvmap`; when it exists the copy resumes).
    #[arg(value_name = "IMAGE")]
    pub(crate) image: PathBuf,
    /// Extra reads of each bad sector at the end (default 0: a dying card is not hammered).
    #[arg(long, value_name = "N", default_value_t = 0)]
    pub(crate) retries: u32,
    /// Copy a disk with its volumes mounted (by default they are unmounted first; the system may write to them).
    #[arg(long)]
    pub(crate) no_unmount: bool,
    /// Developer only: use when copying an image FILE, never a card (refused for a disk). Treats the source's volume
    /// as unknown so the image may go onto the same volume.
    #[arg(long)]
    pub(crate) allow_same_volume: bool,
}

/// The totals of a map, flattened into a line.
#[derive(Serialize)]
struct TotalsLine {
    untried: u64,
    rescued: u64,
    to_trim: u64,
    to_scrape: u64,
    bad: u64,
}

impl From<MapTotals> for TotalsLine {
    fn from(t: MapTotals) -> TotalsLine {
        TotalsLine {
            untried: t.untried,
            rescued: t.rescued,
            to_trim: t.to_trim,
            to_scrape: t.to_scrape,
            bad: t.bad,
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum CopyLine {
    #[serde(rename = "copy_progress")]
    Progress {
        pass: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        retry: Option<u32>,
        pos: u64,
        #[serde(flatten)]
        totals: TotalsLine,
        bytes_per_sec: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        eta_secs: Option<u64>,
    },
    #[serde(rename = "copy_bad")]
    Bad { start: u64, len: u64 },
    #[serde(rename = "copy_summary")]
    Summary {
        #[serde(flatten)]
        totals: TotalsLine,
        complete: bool,
    },
}

/// Prints a `copy_progress` line when the pass or the percent of the position changes (the engine reports after every
/// read), and every `copy_bad` line.
struct Lines {
    size: u64,
    last: Option<(Pass, u64)>,
}

impl Lines {
    fn on_event(&mut self, event: CopyEvent) {
        match event {
            CopyEvent::Progress {
                pass,
                pos,
                totals,
                bytes_per_sec,
                eta_secs,
            } => {
                let pct = (pos * 100).checked_div(self.size).unwrap_or(100);
                if self.last == Some((pass, pct)) {
                    return;
                }
                self.last = Some((pass, pct));
                emit_json(&CopyLine::Progress {
                    pass: pass.as_str(),
                    retry: match pass {
                        Pass::Retry(n) => Some(n),
                        _ => None,
                    },
                    pos,
                    totals: totals.into(),
                    bytes_per_sec,
                    eta_secs,
                });
            }
            CopyEvent::Bad { start, len } => {
                emit_json(&CopyLine::Bad { start, len });
            }
            CopyEvent::Saved => {}
        }
    }
}

/// The image's folder and file name (`write_failed` for a name that is not plain text).
fn split_image(image: &Path) -> Result<(&Path, &str), EngineError> {
    let invalid = || EngineError::Write {
        cause: std::io::Error::from(std::io::ErrorKind::InvalidInput),
    };
    let name = image
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(invalid)?;
    let folder = match image.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    Ok((folder, name))
}

/// Stands for a disk in [`Destination::check`] before the disk is opened: no bytes, no volume, only which disk it is.
struct DiskOnly(DiskId);

impl Source for DiskOnly {
    fn len(&self) -> u64 {
        0
    }
    fn read_at(&self, _: u64, _: &mut [u8]) -> Result<usize, EngineError> {
        Ok(0)
    }
    fn volume(&self) -> Option<VolumeId> {
        None
    }
    fn disk(&self) -> Option<DiskId> {
        Some(self.0.clone())
    }
}

/// `copy`: with a disk, the image's folder is checked against the disk first (before anything is unmounted), then the
/// pre-flight check (exit 2 on any issue, before anything is written; ruling P12).
pub(crate) fn run_copy(ops: &dyn DeviceOps, args: &CopyArgs) -> Result<Exit, EngineError> {
    let (folder, name) = split_image(&args.image)?;
    let opts = |sector| CopyOptions {
        sector,
        retries: args.retries,
        ..CopyOptions::default()
    };
    let cancel = CancelToken::default();
    let lines = |size| Lines { size, last: None };
    let summary = match device_name(&args.source)? {
        Some(disk) => {
            if args.allow_same_volume {
                return Err(EngineError::DestinationOnSource);
            }
            // Checked once, against the whole disk, while it is still mounted and untouched.
            let dest = Destination::check(ops, &DiskOnly(disk.clone()), folder)?;
            let checked = check_disk(ops, &disk, Some(&args.image), None, !args.no_unmount);
            if !checked.issues.is_empty() {
                emit_preflight(&disk, &checked.issues);
                if checked.guard.is_some() {
                    remount_hint(&disk);
                }
                return Ok(Exit::Issues);
            }
            let guard = checked.guard;
            let result = (|| {
                let info = checked.disk.ok_or(EngineError::DeviceNotFound)?;
                let source = DeviceSource::new(ops.open(&disk)?, disk.clone());
                let identity = SourceIdentity::of_device(&info, &source)?;
                let mut out = lines(source.len());
                copy(
                    &source,
                    &identity,
                    &dest,
                    name,
                    &opts(source.scan_sector()),
                    &cancel,
                    &mut |e| out.on_event(e),
                )
            })();
            // Whatever happened after the unmount, say how to mount the card again.
            if guard.is_some() {
                remount_hint(&disk);
            }
            drop(guard);
            result?
        }
        None => {
            check_regular_file(&args.source)?;
            let file = FileSource::open(&args.source)?;
            let wrapped = UnknownVolume(&file);
            let source: &dyn Source = if args.allow_same_volume {
                &wrapped
            } else {
                &file
            };
            let identity = SourceIdentity::of_image(&file)?;
            let dest = Destination::check(ops, source, folder)?;
            let mut out = lines(source.len());
            copy(
                source,
                &identity,
                &dest,
                name,
                &opts(512),
                &cancel,
                &mut |e| out.on_event(e),
            )?
        }
    };
    emit_json(&CopyLine::Summary {
        totals: summary.totals.into(),
        complete: summary.complete,
    });
    Ok(Exit::Done)
}
