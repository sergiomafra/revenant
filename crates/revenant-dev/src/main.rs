//! `revenant-dev`: developer command line for the Revenant engine. Not the end-user app.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

mod bench;

use clap::{Parser, Subcommand, ValueEnum};
use revenant_core::extract::create_named;
use revenant_core::formats::all_formats;
use revenant_core::group::{self, Camera, Group, GroupInput, GroupLabel};
use revenant_core::naming::file_name;
use revenant_core::source::{FileSource, Source, VolumeId, volume_of};
use revenant_core::{
    CancelToken, EngineError, Event, Finding, Fragment, Inspection, Metadata, Preview, ScanOptions,
    Volume, extract_as, inspect, scan,
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
        /// After the last found/updated line, print the results grouped by session, day, camera or folder (one
        /// `group` line per group, before the summary).
        #[arg(long, value_enum, value_name = "BY")]
        group: Option<GroupBy>,
    },
    /// Measure scan throughput on synthetic images written to a temp dir (page-cache speed, not a card reader's).
    Bench {
        /// Size of each image in MiB.
        #[arg(long, value_name = "N", default_value_t = 1024, value_parser = clap::value_parser!(u64).range(1..=1_048_576))]
        size_mib: u64,
        /// Scans per image; the best run is reported.
        #[arg(long, value_name = "N", default_value_t = 3, value_parser = clap::value_parser!(u32).range(1..))]
        runs: u32,
        /// Images to measure: noise, dense-jpeg, dense-raw, video, cut-jpeg, exfat-card.
        #[arg(
            long,
            value_delimiter = ',',
            value_parser = parse_bench_image,
            default_values = ["noise", "dense-jpeg", "dense-raw", "video", "cut-jpeg", "exfat-card"]
        )]
        images: Vec<BenchImage>,
        /// Folder for the temporary images (default: the system temp dir).
        #[arg(long, value_name = "DIR")]
        dir: Option<PathBuf>,
        /// Threads measuring candidate files (0 = one per core, at most 16).
        #[arg(long, value_name = "N", default_value_t = 0)]
        threads: usize,
        /// Also scan each image through a saved session (one more line per image, `"session":true`).
        #[arg(long)]
        session: bool,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum GroupBy {
    Session,
    Day,
    Camera,
    Folder,
}

fn parse_bench_image(s: &str) -> Result<BenchImage, String> {
    BenchImage::parse(s).ok_or_else(|| format!("unknown image '{s}'"))
}

#[derive(Serialize)]
#[serde(tag = "event", rename_all = "lowercase")]
enum Line {
    /// A FAT/exFAT volume of the source, before every other line.
    Volume(Box<VolumeLine>),
    Found(Box<FoundLine>),
    Unreadable {
        start: u64,
        len: u64,
    },
    /// Many unreadable sectors early in the scan: the disk is failing, make a copy first (once, after a progress step).
    #[serde(rename = "disk_failing")]
    DiskFailing {
        unreadable_runs: u64,
        unreadable_bytes: u64,
        read: u64,
    },
    /// A better version of an earlier `found` line with the same `id` (same fields).
    Updated(Box<FoundLine>),
    /// One group of the results (`scan --group`), or `available: false` for a grouping the scan cannot offer.
    Group {
        by: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        label: Option<LabelLine>,
        #[serde(skip_serializing_if = "Option::is_none")]
        ids: Option<Vec<u64>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        available: Option<bool>,
    },
    Summary {
        found: u64,
        unreadable_bytes: u64,
        updated: u64,
        listing_truncated: bool,
    },
}

#[derive(Serialize)]
struct CameraLine {
    #[serde(skip_serializing_if = "Option::is_none")]
    make: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    serial: Option<String>,
}

impl From<Camera> for CameraLine {
    fn from(c: Camera) -> Self {
        CameraLine {
            make: c.make,
            model: c.model,
            serial: c.serial,
        }
    }
}

/// A group's label: `kind` is the variant name in snake_case, the other keys are its fields (`camera` is omitted when
/// the group's camera is unknown).
#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum LabelLine {
    Session {
        #[serde(skip_serializing_if = "Option::is_none")]
        camera: Option<CameraLine>,
        first: String,
        last: String,
    },
    UndatedSession {
        #[serde(skip_serializing_if = "Option::is_none")]
        camera: Option<CameraLine>,
    },
    Day {
        day: String,
    },
    Undated,
    Camera {
        camera: CameraLine,
    },
    UnknownCamera,
    Folder {
        folder: String,
    },
    NoFolder,
}

impl From<GroupLabel> for LabelLine {
    fn from(l: GroupLabel) -> Self {
        match l {
            GroupLabel::Session {
                camera,
                first,
                last,
            } => LabelLine::Session {
                camera: camera.map(Into::into),
                first,
                last,
            },
            GroupLabel::UndatedSession { camera } => LabelLine::UndatedSession {
                camera: camera.map(Into::into),
            },
            GroupLabel::Day(day) => LabelLine::Day { day },
            GroupLabel::Undated => LabelLine::Undated,
            GroupLabel::Camera(camera) => LabelLine::Camera {
                camera: camera.into(),
            },
            GroupLabel::UnknownCamera => LabelLine::UnknownCamera,
            GroupLabel::Folder(folder) => LabelLine::Folder { folder },
            GroupLabel::NoFolder => LabelLine::NoFolder,
        }
    }
}

/// The `group` lines of the results: each id with its latest finding and metadata.
fn group_lines(by: GroupBy, latest: &BTreeMap<u64, (Finding, Metadata)>) -> Vec<Line> {
    let items: Vec<GroupInput> = latest
        .values()
        .map(|(f, m)| GroupInput::new(f, Some(m)))
        .collect();
    let (name, groups): (&'static str, Option<Vec<Group>>) = match by {
        GroupBy::Session => ("session", Some(group::by_session(&items))),
        GroupBy::Day => ("day", Some(group::by_day(&items))),
        GroupBy::Camera => ("camera", Some(group::by_camera(&items))),
        GroupBy::Folder => ("folder", group::by_folder(&items)),
    };
    match groups {
        Some(groups) => groups
            .into_iter()
            .map(|g| Line::Group {
                by: name,
                label: Some(g.label.into()),
                ids: Some(g.ids),
                available: None,
            })
            .collect(),
        None => vec![Line::Group {
            by: name,
            label: None,
            ids: None,
            available: Some(false),
        }],
    }
}

#[derive(Serialize)]
struct VolumeLine {
    index: u32,
    fs: &'static str,
    scheme: &'static str,
    offset: u64,
    len: u64,
    sector_size: u32,
    cluster_size: u64,
    heap_offset: u64,
    cluster_count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    serial: u32,
    from_backup: bool,
    listing_truncated: bool,
}

fn volume_line(v: &Volume, listing_truncated: bool) -> Line {
    Line::Volume(Box::new(VolumeLine {
        index: v.index,
        fs: v.fs.as_str(),
        scheme: v.scheme.as_str(),
        offset: v.offset,
        len: v.len,
        sector_size: v.sector_size,
        cluster_size: v.cluster_size,
        heap_offset: v.heap_offset,
        cluster_count: v.cluster_count,
        label: v.label.clone(),
        serial: v.serial,
        from_backup: v.from_backup,
        listing_truncated,
    }))
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
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    placement: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    volume: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fs_size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    created: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    modified: Option<String>,
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
        name: f.origin.as_ref().map(|o| o.name().to_string()),
        path: f.origin.as_ref().map(|o| o.path()),
        state: f.origin.as_ref().map(|o| o.state.as_str()),
        placement: f.origin.as_ref().map(|o| o.placement.as_str()),
        volume: f.origin.as_ref().map(|o| o.volume),
        fs_size: f.origin.as_ref().map(|o| o.size),
        created: f
            .origin
            .as_ref()
            .and_then(|o| o.created.as_ref().map(ToString::to_string)),
        modified: f
            .origin
            .as_ref()
            .and_then(|o| o.modified.as_ref().map(ToString::to_string)),
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

/// Writes the preview's bytes (all its pieces, in order) to `<dir>/<stem of the file's name>.preview.jpg` (`_2`, `_3`…
/// if it exists; never overwrites), so a file and its preview sit side by side. A read or write error removes the
/// incomplete file.
fn write_preview(
    source: &dyn Source,
    f: &Finding,
    p: &Preview,
    dir: &Path,
    file_name: &str,
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
    let (path, mut file) = create_named(dir, f, file_name, Some(".preview.jpg"))?;
    match file.write_all(&data).and_then(|()| file.flush()) {
        Ok(()) => Ok(path),
        Err(cause) => {
            drop(file);
            let _ = std::fs::remove_file(&path);
            Err(EngineError::Write { cause })
        }
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

/// Extracts the finding and writes its preview (when asked) and reads its details; `Err` only for a fatal error
/// (`destination_on_source`).
fn finding_line(
    source: &dyn Source,
    f: &Finding,
    dest: Option<&Path>,
    previews: Option<&Path>,
) -> Result<(Inspection, Outputs), EngineError> {
    let mut out = Outputs::default();
    let details = inspect(source, f);
    let name = file_name(f, Some(&details.metadata));
    if let Some(dest) = dest {
        match extract_as(source, f, dest, &name) {
            Ok(p) => out.file = Some(p.display().to_string()),
            Err(EngineError::DestinationOnSource) => return Err(EngineError::DestinationOnSource),
            Err(e) => out.error = Some(e.code()),
        }
    }
    if let (Some(dir), Some(p)) = (previews, &details.preview) {
        match write_preview(source, f, p, dir, &name) {
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
    group: Option<GroupBy>,
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
    // With `--group`: each id's latest finding and metadata (an `updated` replaces its `found`).
    let mut latest: BTreeMap<u64, (Finding, Metadata)> = BTreeMap::new();
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
                    if group.is_some() {
                        latest.insert(f.id, (f.clone(), details.metadata.clone()));
                    }
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
                    if group.is_some() {
                        latest.insert(f.id, (f.clone(), details.metadata.clone()));
                    }
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
            Event::Volume {
                volume,
                listing_truncated,
            } => {
                if !emit(&volume_line(&volume, listing_truncated)) {
                    cancel.cancel();
                }
            }
            Event::DiskFailing {
                unreadable_runs,
                unreadable_bytes,
                read,
            } => {
                if !emit(&Line::DiskFailing {
                    unreadable_runs,
                    unreadable_bytes,
                    read,
                }) {
                    cancel.cancel();
                }
            }
            // Timing-based and opt-in: the dev CLI does not ask for it.
            Event::Rate { .. } => {}
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
    if let Some(by) = group {
        for line in group_lines(by, &latest) {
            if !emit(&line) {
                return Ok(());
            }
        }
    }
    emit(&Line::Summary {
        found: summary.found,
        unreadable_bytes: summary.unreadable_bytes,
        updated: summary.updated,
        listing_truncated: summary.listing_truncated,
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
            session,
        } => bench::run_bench(size_mib, runs, &images, dir.as_deref(), threads, session),
        Command::Scan {
            image,
            extract,
            previews,
            allow_same_volume,
            threads,
            group,
        } => run_scan(
            &image,
            extract.as_deref(),
            previews.as_deref(),
            allow_same_volume,
            threads,
            group,
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
            origin: None,
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
            origin: None,
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
        let path = write_preview(&source, &f, &p, out.path(), "revenant_000000001000.jpg").unwrap();
        let mut expected = bytes[4096..5096].to_vec();
        expected.extend_from_slice(&bytes[512..1012]);
        assert_eq!(std::fs::read(path).unwrap(), expected);
    }
}
