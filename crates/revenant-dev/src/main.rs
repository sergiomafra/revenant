//! `revenant-dev`: developer command line for the Revenant engine. Not the end-user app.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

mod bench;

use clap::{Parser, Subcommand};
use revenant_core::formats::all_formats;
use revenant_core::source::{FileSource, Source, VolumeId, volume_of};
use revenant_core::{
    CancelToken, EngineError, Event, Finding, Fragment, Inspection, Preview, ScanOptions, extract,
    inspect, scan,
};
use revenant_testkit::BenchImage;
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
        /// Threads measuring candidate files (0 = one per core, at most 16). The output is the same for any value.
        #[arg(long, value_name = "N", default_value_t = 0)]
        threads: usize,
    },
    /// Measure scan throughput on synthetic images written to a temp dir (page-cache speed, not a card reader's).
    Bench {
        /// Size of each image in MiB.
        #[arg(long, value_name = "N", default_value_t = 1024, value_parser = clap::value_parser!(u64).range(1..=1_048_576))]
        size_mib: u64,
        /// Scans per image; the best run is reported.
        #[arg(long, value_name = "N", default_value_t = 3, value_parser = clap::value_parser!(u32).range(1..))]
        runs: u32,
        /// Images to measure: noise, dense-jpeg, dense-raw, video, cut-jpeg.
        #[arg(
            long,
            value_delimiter = ',',
            value_parser = parse_bench_image,
            default_values = ["noise", "dense-jpeg", "dense-raw", "video", "cut-jpeg"]
        )]
        images: Vec<BenchImage>,
        /// Folder for the temporary images (default: the system temp dir).
        #[arg(long, value_name = "DIR")]
        dir: Option<PathBuf>,
        /// Threads measuring candidate files (0 = one per core, at most 16).
        #[arg(long, value_name = "N", default_value_t = 0)]
        threads: usize,
    },
}

fn parse_bench_image(s: &str) -> Result<BenchImage, String> {
    BenchImage::parse(s).ok_or_else(|| format!("unknown image '{s}'"))
}

#[derive(Serialize)]
#[serde(tag = "event", rename_all = "lowercase")]
enum Line {
    Found(Box<FoundLine>),
    Unreadable {
        start: u64,
        len: u64,
    },
    /// A better version of an earlier `found` line with the same `id` (same fields).
    Updated(Box<FoundLine>),
    Summary {
        found: u64,
        unreadable_bytes: u64,
        updated: u64,
    },
}

#[derive(Serialize)]
struct PreviewLine {
    start: u64,
    len: u64,
    width: u32,
    height: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    fragments: Option<Vec<FragmentLine>>,
}

/// One piece of a file found in pieces (absolute range in the source).
#[derive(Serialize)]
struct FragmentLine {
    start: u64,
    len: u64,
}

/// The `fragments` key: present only when the file is in pieces.
fn fragment_lines(fragments: &[Fragment]) -> Option<Vec<FragmentLine>> {
    (!fragments.is_empty()).then(|| {
        fragments
            .iter()
            .map(|p| FragmentLine {
                start: p.start,
                len: p.len,
            })
            .collect()
    })
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
    fragments: Option<Vec<FragmentLine>>,
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
    Line::Found(finding_fields(f, details, out))
}

/// The fields of a `found` (or `updated`) line.
fn finding_fields(f: &Finding, details: Inspection, out: Outputs) -> Box<FoundLine> {
    let m = details.metadata;
    Box::new(FoundLine {
        id: f.id,
        start: f.start,
        len: f.len,
        kind: f.kind.as_str(),
        health: match f.health {
            revenant_core::formats::Health::Intact => "intact",
            revenant_core::formats::Health::Partial => "partial",
            revenant_core::formats::Health::Damaged => "damaged",
        },
        fragments: fragment_lines(&f.fragments),
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
            fragments: fragment_lines(&p.fragments),
        }),
        file: out.file,
        error: out.error,
        preview_file: out.preview_file,
        preview_error: out.preview_error,
    })
}

/// How many `_N` suffixes are tried for a preview's file name (as `extract` does).
const MAX_NAME_ATTEMPTS: u32 = 10_000;

/// Writes the preview's bytes (all its pieces, in order) to `<dir>/revenant_<finding start as 12 hex digits>.preview.jpg` (`_2`, `_3`… if it
/// exists; never overwrites), the same naming as `extract` with a `.preview.jpg` suffix. A read or write error removes
/// the incomplete file.
fn write_preview(
    source: &dyn Source,
    f: &Finding,
    p: &Preview,
    dir: &Path,
) -> Result<PathBuf, EngineError> {
    // The engine bounds a preview's length (JPEG_MAX_LEN), so it fits in memory; its pieces are written in order.
    let mut data = Vec::with_capacity(p.len as usize);
    for range in p.ranges() {
        let mut piece = vec![0u8; range.len as usize];
        let n = source.read_at(range.start, &mut piece)?;
        if n != piece.len() {
            return Err(EngineError::Read {
                offset: range.start + n as u64,
                cause: std::io::ErrorKind::UnexpectedEof.into(),
            });
        }
        data.extend_from_slice(&piece);
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

/// Extracts the finding and writes its preview (when asked) and reads its details; `Err` only for a fatal error
/// (`destination_on_source`).
fn finding_line(
    source: &dyn Source,
    f: &Finding,
    dest: Option<&Path>,
    previews: Option<&Path>,
) -> Result<(Inspection, Outputs), EngineError> {
    let mut out = Outputs::default();
    if let Some(dest) = dest {
        match extract(source, f, dest) {
            Ok(p) => out.file = Some(p.display().to_string()),
            Err(EngineError::DestinationOnSource) => return Err(EngineError::DestinationOnSource),
            Err(e) => out.error = Some(e.code()),
        }
    }
    let details = inspect(source, f);
    if let (Some(dir), Some(p)) = (previews, &details.preview) {
        match write_preview(source, f, p, dir) {
            Ok(path) => out.preview_file = Some(path.display().to_string()),
            Err(e) => out.preview_error = Some(e.code()),
        }
    }
    Ok((details, out))
}

fn run_scan(
    image: &Path,
    dest: Option<&Path>,
    previews: Option<&Path>,
    allow_same_volume: bool,
    threads: usize,
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
        &ScanOptions {
            threads,
            ..ScanOptions::default()
        },
        &cancel,
        &mut |event| match event {
            Event::Found(f) => match finding_line(source, &f, dest, previews) {
                Ok((details, out)) => {
                    if !emit(&found_line(&f, details, out)) {
                        cancel.cancel();
                    }
                }
                Err(e) => {
                    fatal = Some(e);
                    cancel.cancel();
                }
            },
            // An updated finding is written again (`extract` and the previews pick a new `_N` name).
            Event::Updated(f) => match finding_line(source, &f, dest, previews) {
                Ok((details, out)) => {
                    if !emit(&Line::Updated(finding_fields(&f, details, out))) {
                        cancel.cancel();
                    }
                }
                Err(e) => {
                    fatal = Some(e);
                    cancel.cancel();
                }
            },
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
        updated: summary.updated,
    });
    Ok(())
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Bench {
            size_mib,
            runs,
            images,
            dir,
            threads,
        } => bench::run_bench(size_mib, runs, &images, dir.as_deref(), threads),
        Command::Scan {
            image,
            extract,
            previews,
            allow_same_volume,
            threads,
        } => run_scan(
            &image,
            extract.as_deref(),
            previews.as_deref(),
            allow_same_volume,
            threads,
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

#[cfg(test)]
mod tests {
    use super::*;
    use revenant_core::formats::{FileKind, Health};

    fn json(f: &Finding) -> serde_json::Value {
        let line = found_line(f, Inspection::default(), Outputs::default());
        serde_json::to_value(&line).unwrap()
    }

    #[test]
    fn fragments_key_only_for_pieces() {
        let contiguous = Finding {
            id: 1,
            start: 8192,
            len: 2500,
            kind: FileKind::Jpeg,
            health: Health::Intact,
            fragments: Vec::new(),
        };
        let v = json(&contiguous);
        assert_eq!(v["event"], "found");
        assert!(v.get("fragments").is_none(), "{v}");

        let pieces = Finding {
            fragments: vec![
                Fragment {
                    start: 8192,
                    len: 1000,
                },
                Fragment {
                    start: 4096,
                    len: 1500,
                },
            ],
            ..contiguous.clone()
        };
        let v = json(&pieces);
        assert_eq!(
            v["fragments"],
            serde_json::json!([{"start": 8192, "len": 1000}, {"start": 4096, "len": 1500}])
        );

        // The preview follows the same rule.
        let preview = |fragments: Vec<Fragment>| {
            let details = Inspection {
                preview: Some(Preview {
                    start: 8192,
                    len: 2500,
                    width: 64,
                    height: 48,
                    fragments,
                }),
                ..Inspection::default()
            };
            serde_json::to_value(found_line(&contiguous, details, Outputs::default())).unwrap()
        };
        assert!(preview(Vec::new())["preview"].get("fragments").is_none());
        assert_eq!(
            preview(pieces.fragments.clone())["preview"]["fragments"]
                .as_array()
                .map(Vec::len),
            Some(2)
        );
    }

    #[test]
    fn previews_write_every_piece_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("image.bin");
        let bytes: Vec<u8> = (0..8192u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&image, &bytes).unwrap();
        let source = FileSource::open(&image).unwrap();
        let f = Finding {
            id: 1,
            start: 4096,
            len: 3000,
            kind: FileKind::Jpeg,
            health: Health::Intact,
            fragments: Vec::new(),
        };
        let p = Preview {
            start: 4096,
            len: 1500,
            width: 64,
            height: 48,
            fragments: vec![
                Fragment {
                    start: 4096,
                    len: 1000,
                },
                Fragment {
                    start: 512,
                    len: 500,
                },
            ],
        };
        let out = tempfile::tempdir().unwrap();
        let path = write_preview(&source, &f, &p, out.path()).unwrap();
        let mut expected = bytes[4096..5096].to_vec();
        expected.extend_from_slice(&bytes[512..1012]);
        assert_eq!(std::fs::read(path).unwrap(), expected);
    }
}
