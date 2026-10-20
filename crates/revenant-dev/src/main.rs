//! `revenant-dev`: developer command line for the Revenant engine. Not the end-user app.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use revenant_core::formats::all_formats;
use revenant_core::source::{FileSource, Source, VolumeId, volume_of};
use revenant_core::{
    CancelToken, EngineError, Event, Finding, Inspection, Preview, ScanOptions, extract, inspect,
    scan,
};
use serde::Serialize;

#[derive(Parser)]
#[command(
    version,
    about = "Revenant developer command line (not the end-user app)"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Scan a disk image file read-only; one JSON line per event on stdout, progress on stderr.
    Scan {
        /// Disk image file to scan.
        image: PathBuf,
        /// Extract each finding into this directory as it is found (never onto the source's volume).
        #[arg(long, value_name = "DIR")]
        extract: Option<PathBuf>,
        /// Write each finding's embedded preview JPEG into this directory (never onto the source's volume).
        #[arg(long, value_name = "DIR")]
        previews: Option<PathBuf>,
        /// Developer only: use when scanning an image FILE, never a card. Treats the source's volume as unknown so
        /// extraction (and previews) onto the same volume is allowed.
        #[arg(long)]
        allow_same_volume: bool,
    },
}

#[derive(Serialize)]
#[serde(tag = "event", rename_all = "lowercase")]
enum Line {
    Found(Box<FoundLine>),
    Unreadable { start: u64, len: u64 },
    Summary { found: u64, unreadable_bytes: u64 },
}

#[derive(Serialize)]
struct PreviewLine {
    start: u64,
    len: u64,
    width: u32,
    height: u32,
}

/// Where a finding's extracted file and preview went (each a path or an error code, when requested).
#[derive(Default)]
struct Outputs {
    file: Option<String>,
    error: Option<&'static str>,
    preview_file: Option<String>,
    preview_error: Option<&'static str>,
}

#[derive(Serialize)]
struct FoundLine {
    id: u64,
    start: u64,
    len: u64,
    kind: &'static str,
    health: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    make: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    lens: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    serial: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    iso: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    captured_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    height: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    preview: Option<PreviewLine>,
    #[serde(skip_serializing_if = "Option::is_none")]
    file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    preview_file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    preview_error: Option<&'static str>,
}

/// Wraps a source so that its volume is unknown (developer-only escape hatch for scanning image files).
struct UnknownVolume<'a>(&'a dyn Source);

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
}

/// Returns false when stdout is closed (e.g. `| head`), so the caller can stop.
fn emit(line: &Line) -> bool {
    let mut out = std::io::stdout().lock();
    writeln!(out, "{}", serde_json::to_string(line).expect("serialize")).is_ok()
}

fn found_line(f: &Finding, details: Inspection, out: Outputs) -> Line {
    let m = details.metadata;
    Line::Found(Box::new(FoundLine {
        id: f.id,
        start: f.start,
        len: f.len,
        kind: f.kind.as_str(),
        health: match f.health {
            revenant_core::formats::Health::Intact => "intact",
            revenant_core::formats::Health::Partial => "partial",
            revenant_core::formats::Health::Damaged => "damaged",
        },
        make: m.make,
        model: m.model,
        lens: m.lens,
        serial: m.serial,
        iso: m.iso,
        captured_at: m.captured_at,
        width: m.width,
        height: m.height,
        preview: details.preview.map(|p| PreviewLine {
            start: p.start,
            len: p.len,
            width: p.width,
            height: p.height,
        }),
        file: out.file,
        error: out.error,
        preview_file: out.preview_file,
        preview_error: out.preview_error,
    }))
}

/// How many `_N` suffixes are tried for a preview's file name (as `extract` does).
const MAX_NAME_ATTEMPTS: u32 = 10_000;

/// Writes the preview's bytes to `<dir>/revenant_<finding start as 12 hex digits>.preview.jpg` (`_2`, `_3`… if it
/// exists; never overwrites), the same naming as `extract` with a `.preview.jpg` suffix. A read or write error removes
/// the incomplete file.
fn write_preview(
    source: &dyn Source,
    f: &Finding,
    p: &Preview,
    dir: &Path,
) -> Result<PathBuf, EngineError> {
    // The engine bounds a preview's length (JPEG_MAX_LEN), so it fits in memory.
    let mut data = vec![0u8; p.len as usize];
    let n = source.read_at(p.start, &mut data)?;
    if n != data.len() {
        return Err(EngineError::Read {
            offset: p.start + n as u64,
            cause: std::io::ErrorKind::UnexpectedEof.into(),
        });
    }
    for n in 1..=MAX_NAME_ATTEMPTS {
        let name = if n == 1 {
            format!("revenant_{:012x}.preview.jpg", f.start)
        } else {
            format!("revenant_{:012x}_{n}.preview.jpg", f.start)
        };
        let path = dir.join(name);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                return match file.write_all(&data).and_then(|()| file.flush()) {
                    Ok(()) => Ok(path),
                    Err(cause) => {
                        drop(file);
                        let _ = std::fs::remove_file(&path);
                        Err(EngineError::Write { cause })
                    }
                };
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(cause) => return Err(EngineError::Write { cause }),
        }
    }
    Err(EngineError::Write {
        cause: std::io::Error::other("no free file name"),
    })
}

/// The extraction folder must exist and be a directory; checked once, before scanning.
fn check_folder(dest: &Path) -> Result<(), EngineError> {
    match std::fs::metadata(dest) {
        Ok(m) if m.is_dir() => Ok(()),
        Ok(_) => Err(EngineError::Open {
            cause: std::io::Error::other("not a directory"),
        }),
        Err(cause) => Err(EngineError::Open { cause }),
    }
}

fn run_scan(
    image: &Path,
    dest: Option<&Path>,
    previews: Option<&Path>,
    allow_same_volume: bool,
) -> Result<(), EngineError> {
    let file = FileSource::open(image)?;
    let wrapped = UnknownVolume(&file);
    let source: &dyn Source = if allow_same_volume { &wrapped } else { &file };
    if let Some(dest) = dest {
        check_folder(dest)?;
    }
    if let Some(dir) = previews {
        check_folder(dir)?;
        // Same rule as `extract`: never write onto the source's volume.
        if source.volume() == Some(volume_of(dir)?) {
            return Err(EngineError::DestinationOnSource);
        }
    }

    let cancel = CancelToken::default();
    let mut fatal: Option<EngineError> = None;
    let mut last_pct = u64::MAX;
    let result = scan(
        source,
        &all_formats(),
        &ScanOptions::default(),
        &cancel,
        &mut |event| match event {
            Event::Found(f) => {
                let mut out = Outputs::default();
                if let Some(dest) = dest {
                    match extract(source, &f, dest) {
                        Ok(p) => out.file = Some(p.display().to_string()),
                        Err(EngineError::DestinationOnSource) => {
                            fatal = Some(EngineError::DestinationOnSource);
                            cancel.cancel();
                            return;
                        }
                        Err(e) => out.error = Some(e.code()),
                    }
                }
                let details = inspect(source, &f);
                if let (Some(dir), Some(p)) = (previews, &details.preview) {
                    match write_preview(source, &f, p, dir) {
                        Ok(path) => out.preview_file = Some(path.display().to_string()),
                        Err(e) => out.preview_error = Some(e.code()),
                    }
                }
                if !emit(&found_line(&f, details, out)) {
                    cancel.cancel();
                }
            }
            Event::Unreadable { start, len } => {
                if !emit(&Line::Unreadable { start, len }) {
                    cancel.cancel();
                }
            }
            Event::Progress { read, total } => {
                let pct = (read * 100).checked_div(total).unwrap_or(100);
                if pct != last_pct {
                    last_pct = pct;
                    eprintln!("scanned {pct}% ({read} of {total} bytes)");
                }
            }
        },
    );
    if let Some(e) = fatal {
        return Err(e);
    }
    let summary = result?;
    emit(&Line::Summary {
        found: summary.found,
        unreadable_bytes: summary.unreadable_bytes,
    });
    Ok(())
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Scan {
            image,
            extract,
            previews,
            allow_same_volume,
        } => run_scan(
            &image,
            extract.as_deref(),
            previews.as_deref(),
            allow_same_volume,
        ),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {}", e.code());
            ExitCode::from(1)
        }
    }
}
