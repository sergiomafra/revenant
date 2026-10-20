//! `revenant-dev bench`: scan throughput on synthetic disk images.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::time::Instant;

use revenant_core::formats::all_formats;
use revenant_core::source::FileSource;
use revenant_core::{CancelToken, EngineError, ScanOptions, scan};
use revenant_testkit::{BenchImage, write_bench_image};
use serde::Serialize;

#[derive(Serialize)]
struct BenchLine {
    event: &'static str,
    image: &'static str,
    bytes: u64,
    placed: u64,
    found: u64,
    runs: u32,
    best_seconds: f64,
    mb_per_s: f64,
}

fn io_err(cause: std::io::Error) -> EngineError {
    EngineError::Write { cause }
}

/// Writes each image to a fresh temp dir under `dir` (the system temp dir if `None`), scans it `runs` times and prints
/// one JSON line per image. The temp dir is removed at the end.
pub fn run_bench(
    size_mib: u64,
    runs: u32,
    images: &[BenchImage],
    dir: Option<&Path>,
) -> Result<(), EngineError> {
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
                &ScanOptions::default(),
                &CancelToken::default(),
                &mut |_| {},
            )?;
            let secs = start.elapsed().as_secs_f64();
            if secs < best.0 {
                best = (secs, summary.found);
            }
        }
        drop(source);
        std::fs::remove_file(&path).map_err(io_err)?;
        let line = BenchLine {
            event: "bench",
            image: kind.name(),
            bytes,
            placed,
            found: best.1,
            runs,
            best_seconds: (best.0 * 100.0).round() / 100.0,
            mb_per_s: ((bytes as f64 / 1e6 / best.0) * 10.0).round() / 10.0,
        };
        let mut out = std::io::stdout().lock();
        if writeln!(out, "{}", serde_json::to_string(&line).expect("serialize")).is_err() {
            break;
        }
    }
    Ok(())
}
