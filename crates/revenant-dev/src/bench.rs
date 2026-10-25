//! `revenant-dev bench`: scan throughput on synthetic disk images.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::time::Instant;

use revenant_core::device::SystemDevices;
use revenant_core::formats::all_formats;
use revenant_core::session::{Session, SessionOptions, scan_session};
use revenant_core::source::{FileSource, Source, VolumeId};
use revenant_core::{CancelToken, Destination, EngineError, ScanOptions, SourceIdentity, scan};
use revenant_testkit::{BenchImage, write_bench_image};
use serde::Serialize;

#[derive(Serialize)]
struct BenchLine {
    event: &'static str,
    image: &'static str,
    bytes: u64,
    placed: u64,
    found: u64,
    threads: usize,
    runs: u32,
    /// Scanned through a saved session (`--session`).
    session: bool,
    best_seconds: f64,
    mb_per_s: f64,
}

fn io_err(cause: std::io::Error) -> EngineError {
    EngineError::Write { cause }
}

/// Writes each image to a fresh temp dir under `dir` (the system temp dir if `None`), scans it `runs` times with
/// `threads` measuring threads (0 = auto) and prints one JSON line per image; with `session`, scans it `runs` times more
/// through a saved session in a fresh folder of the temp dir (default checkpoint cadence) and prints a second line with
/// `"session":true`. The temp dir is removed at the end.
pub fn run_bench(
    size_mib: u64,
    runs: u32,
    images: &[BenchImage],
    dir: Option<&Path>,
    threads: usize,
    session: bool,
) -> Result<(), EngineError> {
    let opts = ScanOptions {
        threads,
        ..ScanOptions::default()
    };
    let tmp = match dir {
        Some(d) => tempfile::tempdir_in(d),
        None => tempfile::tempdir(),
    }
    .map_err(io_err)?;
    let bytes = size_mib
        .checked_mul(1024 * 1024)
        .ok_or_else(|| io_err(std::io::Error::other("size too large")))?;
    let formats = all_formats();
    for &kind in images {
        let path = tmp.path().join(format!("{}.img", kind.name()));
        let placed = {
            let mut w = BufWriter::with_capacity(1 << 20, File::create(&path).map_err(io_err)?);
            let placed = write_bench_image(kind, bytes, &mut w).map_err(io_err)?;
            w.flush().map_err(io_err)?;
            placed
        };
        let source = FileSource::open(&path)?;
        let mut best = (f64::INFINITY, 0u64);
        for _ in 0..runs {
            let start = Instant::now();
            let summary = scan(
                &source,
                &formats,
                &opts,
                &CancelToken::default(),
                &mut |_| {},
            )?;
            let secs = start.elapsed().as_secs_f64();
            if secs < best.0 {
                best = (secs, summary.found);
            }
        }
        let mut lines = vec![(false, best)];
        if session {
            lines.push((
                true,
                best_through_session(&source, tmp.path(), &opts, runs)?,
            ));
        }
        drop(source);
        std::fs::remove_file(&path).map_err(io_err)?;
        for (session, best) in lines {
            let line = BenchLine {
                event: "bench",
                image: kind.name(),
                bytes,
                placed,
                found: best.1,
                threads: opts.resolved_threads(),
                runs,
                session,
                best_seconds: (best.0 * 100.0).round() / 100.0,
                mb_per_s: ((bytes as f64 / 1e6 / best.0) * 10.0).round() / 10.0,
            };
            let mut out = std::io::stdout().lock();
            if writeln!(out, "{}", serde_json::to_string(&line).expect("serialize")).is_err() {
                return Ok(());
            }
        }
    }
    Ok(())
}

/// The bench's own synthetic image, as a source that lives on no volume: the session folder sits next to it in the bench's
/// temp dir (the same volume, which `Destination::check` refuses for a real image so that a card's image is never
/// written beside), and nothing here is anyone's card. Only for checking the bench's session folder.
struct Synthetic<'a>(&'a FileSource);

impl Source for Synthetic<'_> {
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

/// The best of `runs` scans of `source` through a new session each time (in a fresh folder under `tmp`, removed after
/// the run): its seconds (creating the session included) and findings.
fn best_through_session(
    source: &FileSource,
    tmp: &Path,
    opts: &ScanOptions,
    runs: u32,
) -> Result<(f64, u64), EngineError> {
    let formats = all_formats();
    let identity = SourceIdentity::of_image(source)?;
    let mut best = (f64::INFINITY, 0u64);
    for _ in 0..runs {
        let folder = tempfile::tempdir_in(tmp).map_err(io_err)?;
        let dest = Destination::check(&SystemDevices, &Synthetic(source), folder.path())?;
        let start = Instant::now();
        let mut session = Session::create(&dest, identity.clone(), opts)?;
        let summary = scan_session(
            source,
            &identity,
            &formats,
            opts,
            &mut session,
            &SessionOptions::default(),
            &CancelToken::default(),
            &mut |_| {},
        )?;
        let secs = start.elapsed().as_secs_f64();
        if secs < best.0 {
            best = (secs, summary.found);
        }
        folder.close().map_err(io_err)?;
    }
    Ok(best)
}
