//! `revenant-dev`: developer command line for the Revenant engine. Not the end-user app.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use revenant_core::formats::all_formats;
use revenant_core::source::{FileSource, Source, VolumeId};
use revenant_core::{CancelToken, EngineError, Event, Finding, ScanOptions, extract, scan};
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
        /// Developer only: use when scanning an image FILE, never a card. Treats the source's volume as unknown so
        /// extraction onto the same volume is allowed.
        #[arg(long)]
        allow_same_volume: bool,
    },
}

#[derive(Serialize)]
#[serde(tag = "event", rename_all = "lowercase")]
enum Line {
    Found {
        id: u64,
        start: u64,
        len: u64,
        kind: &'static str,
        health: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        file: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<&'static str>,
    },
    Unreadable {
        start: u64,
        len: u64,
    },
    Summary {
        found: u64,
        unreadable_bytes: u64,
    },
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

fn found_line(f: &Finding, file: Option<String>, error: Option<&'static str>) -> Line {
    Line::Found {
        id: f.id,
        start: f.start,
        len: f.len,
        kind: f.kind.as_str(),
        health: match f.health {
            revenant_core::formats::Health::Intact => "intact",
            revenant_core::formats::Health::Partial => "partial",
            revenant_core::formats::Health::Damaged => "damaged",
        },
        file,
        error,
    }
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

fn run_scan(image: &Path, dest: Option<&Path>, allow_same_volume: bool) -> Result<(), EngineError> {
    let file = FileSource::open(image)?;
    let wrapped = UnknownVolume(&file);
    let source: &dyn Source = if allow_same_volume { &wrapped } else { &file };
    if let Some(dest) = dest {
        check_folder(dest)?;
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
                let (mut path, mut error) = (None, None);
                if let Some(dest) = dest {
                    match extract(source, &f, dest) {
                        Ok(p) => path = Some(p.display().to_string()),
                        Err(EngineError::DestinationOnSource) => {
                            fatal = Some(EngineError::DestinationOnSource);
                            cancel.cancel();
                            return;
                        }
                        Err(e) => error = Some(e.code()),
                    }
                }
                if !emit(&found_line(&f, path, error)) {
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
            allow_same_volume,
        } => run_scan(&image, extract.as_deref(), allow_same_volume),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {}", e.code());
            ExitCode::from(1)
        }
    }
}
