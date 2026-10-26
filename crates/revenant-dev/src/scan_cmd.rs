//! `scan`: a disk image file, a rescue copy's image read through its map, or a whole disk read directly; optionally
//! through a saved session that resumes after a crash or a disconnected card.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use revenant_core::device::{DeviceOps, DiskInfo, UnmountGuard, reconnect};
use revenant_core::formats::all_formats;
use revenant_core::naming::file_name;
use revenant_core::rescue::{MappedSource, map_path};
use revenant_core::session::{Session, SessionOptions, scan_session};
use revenant_core::source::{DeviceSource, DiskId, FileSource, Source, VolumeId};
use revenant_core::{
    CancelToken, Destination, EngineError, Event, Finding, Inspection, Metadata, Preview,
    ScanOptions, ScanSummary, SourceIdentity, extract_to, inspect, scan,
};
use serde::Serialize;

use crate::devices::{check_regular_file, device_name, open_device, remount_hint, report_unmount};
use crate::output::{
    Line, Outputs, emit, emit_json, emit_marked, finding_fields, found_line, group_lines,
    volume_line,
};
use crate::{Exit, GroupBy};

/// How often a disconnected card is looked for.
const RECONNECT_POLL: Duration = Duration::from_millis(500);

#[derive(clap::Args, Default)]
pub(crate) struct ScanArgs {
    /// A disk image file (read through `<FILE>.rvmap` when that rescue map sits beside it), or a whole disk read
    /// directly: "disk4" (macOS, as root) or "PhysicalDrive2" (Windows, as Administrator).
    #[arg(value_name = "SOURCE")]
    pub(crate) source: PathBuf,
    /// Extract each finding into this directory as it is found (never onto the source: its volume, or any volume of
    /// the disk being read).
    #[arg(long, value_name = "DIR")]
    pub(crate) extract: Option<PathBuf>,
    /// Write each finding's embedded preview JPEG into this directory (same rule as `--extract`).
    #[arg(long, value_name = "DIR")]
    pub(crate) previews: Option<PathBuf>,
    /// Save the scan into this folder (created when absent), or resume the scan saved there.
    #[arg(long, value_name = "DIR")]
    pub(crate) session: Option<PathBuf>,
    /// Developer only: use when scanning an image FILE, never a card (refused for a disk). Treats the source's volume
    /// as unknown so extraction, previews and the session onto the same volume are allowed.
    #[arg(long)]
    pub(crate) allow_same_volume: bool,
    /// Read a disk with its volumes mounted (by default they are unmounted first; the system may write to them).
    #[arg(long)]
    pub(crate) no_unmount: bool,
    /// Threads measuring candidate files (0 = one per core, at most 16). The output is the same for any value.
    #[arg(long, value_name = "N", default_value_t = 0)]
    pub(crate) threads: usize,
    /// After the last found/updated line, print the results grouped by session, day, camera or folder (one
    /// `group` line per group, before the summary).
    #[arg(long, value_enum, value_name = "BY")]
    pub(crate) group: Option<GroupBy>,
    /// Tests: cancel the scan after this many events (exit 3).
    #[arg(long, value_name = "N", hide = true)]
    pub(crate) stop_after_events: Option<u64>,
    /// Tests: a session checkpoint after this many MiB scanned (default 256).
    #[arg(long, value_name = "N", hide = true, default_value_t = 256,
        value_parser = clap::value_parser!(u64).range(1..=1_048_576))]
    pub(crate) checkpoint_mib: u64,
}

/// Wraps a source so that its volume is unknown (developer-only escape hatch for scanning image files); the disk it
/// reads, if any, is still told.
pub(crate) struct UnknownVolume<'a>(pub(crate) &'a dyn Source);

impl Source for UnknownVolume<'_> {
    fn len(&self) -> u64 {
        self.0.len()
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, EngineError> {
        self.0.read_at(offset, buf)
    }
    fn volume(&self) -> Option<VolumeId> {
        None
    }
    fn disk(&self) -> Option<DiskId> {
        self.0.disk()
    }
}

/// What is scanned.
enum Input {
    Device {
        source: DeviceSource,
        info: DiskInfo,
    },
    Mapped(MappedSource),
    File(FileSource),
}

impl Input {
    /// A file: through its map when `<file>.rvmap` exists.
    fn open_file(path: &Path) -> Result<Input, EngineError> {
        check_regular_file(path)?;
        if map_path(path).exists() {
            Ok(Input::Mapped(MappedSource::open(path)?))
        } else {
            Ok(Input::File(FileSource::open(path)?))
        }
    }

    fn source(&self) -> &dyn Source {
        match self {
            Input::Device { source, .. } => source,
            Input::Mapped(m) => m,
            Input::File(f) => f,
        }
    }

    fn identity(&self) -> Result<SourceIdentity, EngineError> {
        match self {
            Input::Device { source, info } => SourceIdentity::of_device(info, source),
            Input::Mapped(m) => SourceIdentity::of_mapped(m),
            Input::File(f) => SourceIdentity::of_image(f),
        }
    }
}

/// Where a scan writes, each folder checked once against the source (ruling P2).
#[derive(Default)]
pub(crate) struct Targets {
    pub(crate) extract: Option<Destination>,
    pub(crate) previews: Option<Destination>,
    pub(crate) session: Option<Destination>,
}

/// The folder must exist and be a directory (`open_failed` otherwise).
fn check_folder(dir: &Path) -> Result<(), EngineError> {
    match std::fs::metadata(dir) {
        Ok(m) if m.is_dir() => Ok(()),
        Ok(_) => Err(EngineError::Open {
            cause: std::io::Error::other("not a directory"),
        }),
        Err(cause) => Err(EngineError::Open { cause }),
    }
}

impl Targets {
    /// Checks each folder given against `source` with `ops` ([`Destination::check`]: never the source's volume, never a
    /// volume of the disk it reads). `extract` and `previews` must exist. `session` is created when absent, and only
    /// after its parent folder passed the same check (a refused folder is never created).
    pub(crate) fn check(
        ops: &dyn DeviceOps,
        source: &dyn Source,
        extract: Option<&Path>,
        previews: Option<&Path>,
        session: Option<&Path>,
    ) -> Result<Targets, EngineError> {
        let existing = |dir: &Path| {
            check_folder(dir)?;
            Destination::check(ops, source, dir)
        };
        Ok(Targets {
            extract: extract.map(existing).transpose()?,
            previews: previews.map(existing).transpose()?,
            session: session
                .map(|dir| session_folder(ops, source, dir))
                .transpose()?,
        })
    }
}

/// The session folder checked; when absent, its parent is checked first, then it is created and checked itself.
fn session_folder(
    ops: &dyn DeviceOps,
    source: &dyn Source,
    dir: &Path,
) -> Result<Destination, EngineError> {
    if std::fs::symlink_metadata(dir).is_ok() {
        check_folder(dir)?;
        return Destination::check(ops, source, dir);
    }
    let parent = match dir.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    check_folder(parent)?;
    Destination::check(ops, source, parent)?;
    std::fs::create_dir(dir).map_err(|cause| EngineError::Write { cause })?;
    Destination::check(ops, source, dir).inspect_err(|_| {
        let _ = std::fs::remove_dir(dir);
    })
}

/// The progress line on stderr: once per percent, with the speed and time left from the `Rate` that follows it.
#[derive(Default)]
struct ProgressLine {
    last_pct: Option<u64>,
    pending: Option<(u64, u64)>,
}

impl ProgressLine {
    fn progress(&mut self, read: u64, total: u64) {
        self.flush();
        let pct = (read * 100).checked_div(total).unwrap_or(100);
        if self.last_pct != Some(pct) {
            self.last_pct = Some(pct);
            self.pending = Some((read, total));
        }
    }

    fn rate(&mut self, bytes_per_sec: u64, eta_secs: Option<u64>) {
        if let Some((read, total)) = self.pending.take() {
            let eta = eta_secs.map_or_else(
                || "unknown".to_string(),
                |s| format!("{}m{:02}s", s / 60, s % 60),
            );
            eprintln!(
                "{}, {:.1} MB/s, ETA {eta}",
                Self::text(read, total),
                bytes_per_sec as f64 / 1e6
            );
        }
    }

    fn flush(&mut self) {
        if let Some((read, total)) = self.pending.take() {
            eprintln!("{}", Self::text(read, total));
        }
    }

    fn text(read: u64, total: u64) -> String {
        let pct = (read * 100).checked_div(total).unwrap_or(100);
        format!("scanned {pct}% ({read} of {total} bytes)")
    }
}

/// Turns the engine's events into lines, extracting and writing previews on the way.
struct Printer<'a> {
    targets: &'a Targets,
    group: Option<GroupBy>,
    /// With `--group`: each id's latest finding and metadata (an `updated` replaces its `found`).
    latest: BTreeMap<u64, (Finding, Metadata)>,
    progress: ProgressLine,
    /// Events (all but `Rate`) handed out by the current run of the scan.
    seen: u64,
    /// Events already handled, by this run or an earlier one (a run after a reconnect hands them out again).
    done: u64,
    /// The first `replay_len` events come from the session's log: marked `"replayed":true`, never written again.
    replay_len: u64,
    stop_after: Option<u64>,
    stopped: bool,
    closed: bool,
    fatal: Option<EngineError>,
}

impl Printer<'_> {
    fn new<'a>(targets: &'a Targets, args: &ScanArgs) -> Printer<'a> {
        Printer {
            targets,
            group: args.group,
            latest: BTreeMap::new(),
            progress: ProgressLine::default(),
            seen: 0,
            done: 0,
            replay_len: 0,
            stop_after: args.stop_after_events,
            stopped: false,
            closed: false,
            fatal: None,
        }
    }

    /// Handles one event of the current run; asks `cancel` to stop on a fatal error, a closed stdout or
    /// `--stop-after-events`.
    fn on_event(&mut self, source: &dyn Source, event: Event, cancel: &CancelToken) {
        if let Event::Rate {
            bytes_per_sec,
            eta_secs,
            ..
        } = event
        {
            self.progress.rate(bytes_per_sec, eta_secs);
            return;
        }
        let index = self.seen;
        self.seen += 1;
        if index < self.done || self.fatal.is_some() || self.closed {
            return;
        }
        let replayed = index < self.replay_len;
        let line = match event {
            Event::Progress { read, total } => {
                self.progress.progress(read, total);
                None
            }
            Event::Found(f) => self
                .finding(source, &f, replayed)
                .map(|(details, out)| found_line(&f, details, out)),
            // An updated finding is written again (`extract` and the previews pick a new `_N` name).
            Event::Updated(f) => self
                .finding(source, &f, replayed)
                .map(|(details, out)| Line::Updated(finding_fields(&f, details, out))),
            Event::Unreadable { start, len } => Some(Line::Unreadable { start, len }),
            Event::Volume {
                volume,
                listing_truncated,
            } => Some(volume_line(&volume, listing_truncated)),
            Event::DiskFailing {
                unreadable_runs,
                unreadable_bytes,
                read,
            } => Some(Line::DiskFailing {
                unreadable_runs,
                unreadable_bytes,
                read,
            }),
            Event::Rate { .. } => None,
        };
        if self.fatal.is_some() {
            // Not handled: a run after a reconnect hands this event out again.
            cancel.cancel();
            return;
        }
        if let Some(line) = line {
            self.progress.flush();
            if !emit_marked(&line, replayed) {
                self.closed = true;
                cancel.cancel();
            }
        }
        self.done = index + 1;
        if self.stop_after == Some(self.done) {
            self.stopped = true;
            cancel.cancel();
        }
    }

    /// Reads the finding's details and, unless it is replayed (written by the run that found it), extracts it and
    /// writes its preview. `None` on a fatal error (kept in `fatal`, [`stops_the_scan`]): the folder no longer where it
    /// was checked, or the card gone while copying from it.
    fn finding(
        &mut self,
        source: &dyn Source,
        f: &Finding,
        replayed: bool,
    ) -> Option<(Inspection, Outputs)> {
        let mut out = Outputs::default();
        let details = inspect(source, f);
        let name = file_name(f, Some(&details.metadata));
        if !replayed {
            if let Some(dest) = &self.targets.extract {
                match extract_to(source, f, dest, &name) {
                    Ok(p) => out.file = Some(p.display().to_string()),
                    Err(e) if stops_the_scan(&e) => {
                        self.fatal = Some(e);
                        return None;
                    }
                    Err(e) => out.error = Some(e.code()),
                }
            }
            if let (Some(dest), Some(p)) = (&self.targets.previews, &details.preview) {
                match write_preview(source, f, p, dest, &name) {
                    Ok(path) => out.preview_file = Some(path.display().to_string()),
                    Err(e) if stops_the_scan(&e) => {
                        self.fatal = Some(e);
                        return None;
                    }
                    Err(e) => out.preview_error = Some(e.code()),
                }
            }
        }
        if self.group.is_some() {
            self.latest
                .insert(f.id, (f.clone(), details.metadata.clone()));
        }
        Some((details, out))
    }

    /// After the scan: the `group` lines and the summary, or how it stopped.
    fn finish(mut self, result: Result<ScanSummary, EngineError>) -> Result<Exit, EngineError> {
        self.progress.flush();
        if let Some(e) = self.fatal.take() {
            return Err(e);
        }
        let summary = match result {
            Err(EngineError::Cancelled) if self.closed => return Ok(Exit::Done),
            Err(EngineError::Cancelled) if self.stopped => return Ok(Exit::Stopped),
            Err(e) => return Err(e),
            Ok(summary) => summary,
        };
        if self.closed {
            return Ok(Exit::Done);
        }
        if let Some(by) = self.group {
            for line in group_lines(by, &self.latest) {
                if !emit(&line) {
                    return Ok(Exit::Done);
                }
            }
        }
        emit(&Line::Summary {
            found: summary.found,
            unreadable_bytes: summary.unreadable_bytes,
            updated: summary.updated,
            listing_truncated: summary.listing_truncated,
        });
        Ok(Exit::Done)
    }
}

/// Whether an extraction's or a preview's error stops the scan (kept as fatal) instead of marking that one finding: the
/// folder no longer where it was checked, or the card gone (a session run then waits for it to come back).
pub(crate) fn stops_the_scan(e: &EngineError) -> bool {
    matches!(
        e,
        EngineError::DestinationOnSource | EngineError::DeviceGone { .. }
    )
}

/// Writes the preview's bytes (all its pieces, in order) into the checked folder as `<stem of the file's name>.preview.jpg`
/// (`_2`, `_3`… if it exists; never overwrites), so a file and its preview sit side by side. Every piece is read before
/// the file is created (a read coming back short is `device_gone`); a write error removes the incomplete file.
pub(crate) fn write_preview(
    source: &dyn Source,
    f: &Finding,
    p: &Preview,
    dest: &Destination,
    file_name: &str,
) -> Result<PathBuf, EngineError> {
    // The engine bounds a preview's length (JPEG_MAX_LEN), so it fits in memory; its pieces are written in order.
    let mut data = Vec::with_capacity(p.len as usize);
    for range in p.ranges() {
        let mut piece = vec![0u8; range.len as usize];
        // The engine's preview lies inside the source, so a read coming back short means the source vanished there
        // (ruling D1): `device_gone`, which a session run waits out like any other vanished read.
        let n = source.read_at(range.start, &mut piece)?;
        if n != piece.len() {
            return Err(EngineError::DeviceGone {
                offset: range.start + n as u64,
            });
        }
        data.extend_from_slice(&piece);
    }
    let (path, mut file) = dest.create_named(f, file_name, Some(".preview.jpg"))?;
    match file.write_all(&data).and_then(|()| file.flush()) {
        Ok(()) => Ok(path),
        Err(cause) => {
            drop(file);
            let _ = std::fs::remove_file(&path);
            Err(EngineError::Write { cause })
        }
    }
}

#[derive(Serialize)]
struct MapLine {
    event: &'static str,
    rescued: u64,
    unrescued: u64,
}

/// `scan`.
pub(crate) fn run_scan(ops: &dyn DeviceOps, args: &ScanArgs) -> Result<Exit, EngineError> {
    let disk = device_name(&args.source)?;
    if disk.is_some() && args.allow_same_volume {
        // The escape hatch is for image files: a card is never written to, through any of its volumes.
        return Err(EngineError::DestinationOnSource);
    }
    let mut input = match &disk {
        Some(d) => {
            let (source, info) = open_device(ops, d)?;
            Input::Device { source, info }
        }
        None => Input::open_file(&args.source)?,
    };
    let targets = {
        let wrapped = UnknownVolume(input.source());
        let source: &dyn Source = if args.allow_same_volume {
            &wrapped
        } else {
            input.source()
        };
        Targets::check(
            ops,
            source,
            args.extract.as_deref(),
            args.previews.as_deref(),
            args.session.as_deref(),
        )?
    };
    let mut guard = None;
    if let Input::Device { info, .. } = &input
        && !args.no_unmount
    {
        guard = report_unmount(&info.id, Some(info), ops.unmount(&info.id));
    }
    if let Input::Mapped(m) = &input {
        let rescued = m.map().totals().rescued;
        emit_json(&MapLine {
            event: "map",
            rescued,
            unrescued: m.len() - rescued,
        });
    }
    let mut opts = ScanOptions {
        threads: args.threads,
        rates: true,
        ..ScanOptions::default()
    };
    if let Input::Device { source, .. } = &input {
        opts.sector = u64::from(source.scan_sector());
    }
    let mut printer = Printer::new(&targets, args);
    let result = match &targets.session {
        None => {
            let cancel = CancelToken::default();
            let wrapped = UnknownVolume(input.source());
            let source: &dyn Source = if args.allow_same_volume {
                &wrapped
            } else {
                input.source()
            };
            scan(source, &all_formats(), &opts, &cancel, &mut |e| {
                printer.on_event(source, e, &cancel)
            })
        }
        Some(dest) => scan_saved(ops, &mut input, &mut guard, dest, &opts, args, &mut printer),
    };
    let exit = printer.finish(result);
    if let (Input::Device { source, .. }, Some(_)) = (&input, &guard) {
        remount_hint(source.disk_id());
    }
    exit
}

/// The scan through its session: created in `dest` when it holds none, else resumed (its stored events replayed
/// first). A disk that vanishes is waited for (plugged back in, even under another name) and the scan goes on in the
/// same session.
fn scan_saved(
    ops: &dyn DeviceOps,
    input: &mut Input,
    guard: &mut Option<UnmountGuard>,
    dest: &Destination,
    opts: &ScanOptions,
    args: &ScanArgs,
    printer: &mut Printer,
) -> Result<ScanSummary, EngineError> {
    let identity = input.identity()?;
    let mut session = match Session::open_checked(dest) {
        Ok(s) => s,
        Err(EngineError::Open { cause }) if cause.kind() == std::io::ErrorKind::NotFound => {
            Session::create(dest, identity.clone(), opts)?
        }
        Err(e) => return Err(e),
    };
    printer.replay_len = session.replay_len();
    let sopts = SessionOptions {
        checkpoint_bytes: args.checkpoint_mib << 20,
        ..SessionOptions::default()
    };
    let formats = all_formats();
    loop {
        let cancel = CancelToken::default();
        printer.seen = 0;
        let result = {
            let wrapped = UnknownVolume(input.source());
            let source: &dyn Source = if args.allow_same_volume {
                &wrapped
            } else {
                input.source()
            };
            scan_session(
                source,
                &identity,
                &formats,
                opts,
                &mut session,
                &sopts,
                &cancel,
                &mut |e| printer.on_event(source, e, &cancel),
            )
        };
        let gone = matches!(result, Err(EngineError::DeviceGone { .. }))
            || matches!(printer.fatal, Some(EngineError::DeviceGone { .. }));
        let Input::Device { info, .. } = &*input else {
            return result;
        };
        if !gone {
            return result;
        }
        printer.fatal = None;
        printer.progress.flush();
        eprintln!(
            "{} was disconnected: plug the card back in; the scan goes on where it stopped",
            info.id.as_str()
        );
        let back = reconnect(ops, &identity, &CancelToken::default(), RECONNECT_POLL)?;
        let mut info = info.clone();
        info.id = back.source.disk_id().clone();
        *guard = report_unmount(&info.id, Some(&info), back.unmount);
        *input = Input::Device {
            source: back.source,
            info,
        };
    }
}
