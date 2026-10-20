//! Opt-in test against real camera files (the list is `samples/samples.toml`; `scripts/fetch-samples.sh` downloads them).
//!
//! Ignored by default. Run it with the cache directory in `REVENANT_SAMPLES`:
//!
//! ```sh
//! scripts/fetch-samples.sh
//! REVENANT_SAMPLES=samples/cache cargo test -p revenant-core --test real_samples -- --ignored
//! ```
//!
//! For each sample: embed it in a noise disk image at a sector start, scan, and expect exactly one finding of the
//! expected kind at that offset, `Intact`, with the file's exact length; `inspect` must give make and model and (except
//! HEIC) a preview whose JPEG decodes in strict mode. Every sample is checked and every failure listed before the test
//! fails.
//!
//! One allowance on the length: the finding may stop before the end of the file when everything after it is zero
//! padding shorter than [`MAX_PADDING`]. Sony pads ARW files with zeros after the raw strip, to a 64 KiB (ILCE-7S) or
//! 4 KiB (ILCE-7M5) multiple; no IFD references those bytes, so the recovered file opens exactly like the original, and on
//! a card the engine cannot tell that padding from zeroed free space after the file.

use std::path::{Path, PathBuf};

use revenant_core::formats::{Health, all_formats};
use revenant_core::source::{FileSource, Source};
use revenant_core::{CancelToken, Event, Finding, ScanOptions, inspect, scan};
use revenant_testkit::DiskImage;
use zune_core::bytestream::ZCursor;
use zune_core::colorspace::ColorSpace;
use zune_core::options::DecoderOptions;
use zune_jpeg::JpegDecoder;

/// Where each sample sits in its disk image: a sector start that is not a block start.
const AT: usize = 1024 * 1024 + 512;
/// Noise after the sample.
const TAIL: usize = 1024 * 1024;
/// Longest unreferenced zero padding accepted at the end of a sample (see the module comment).
const MAX_PADDING: usize = 64 * 1024;

struct Sample {
    name: String,
    kind: String,
}

/// Reads `name` and `kind` of every `[[sample]]` in `samples.toml` (simple `key = "value"` lines).
fn samples() -> Vec<Sample> {
    let list = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../samples/samples.toml");
    let text = std::fs::read_to_string(&list).expect("samples/samples.toml");
    let mut out: Vec<Sample> = Vec::new();
    for line in text.lines().map(str::trim) {
        if line == "[[sample]]" {
            out.push(Sample {
                name: String::new(),
                kind: String::new(),
            });
            continue;
        }
        let Some((key, value)) = line.split_once(" = ") else {
            continue;
        };
        let value = value.trim_matches('"').to_string();
        if let Some(s) = out.last_mut() {
            match key {
                "name" => s.name = value,
                "kind" => s.kind = value,
                _ => {}
            }
        }
    }
    assert!(!out.is_empty(), "no samples in {}", list.display());
    for s in &out {
        assert!(!s.name.is_empty() && !s.kind.is_empty(), "incomplete entry");
    }
    out
}

fn findings(source: &dyn Source) -> Vec<Finding> {
    let mut found = Vec::new();
    scan(
        source,
        &all_formats(),
        &ScanOptions::default(),
        &CancelToken::default(),
        &mut |e| {
            if let Event::Found(f) = e {
                found.push(f);
            }
        },
    )
    .expect("scan");
    found
}

fn strict_decode(data: &[u8]) -> Result<(), String> {
    let options = DecoderOptions::default()
        .jpeg_set_out_colorspace(ColorSpace::Luma)
        .set_strict_mode(true);
    JpegDecoder::new_with_options(ZCursor::new(data), options)
        .decode()
        .map(|_| ())
        .map_err(|e| format!("{e:?}"))
}

/// Checks one sample; returns what went wrong (empty = pass) and a one-line summary.
fn check(dir: &Path, s: &Sample) -> (Vec<String>, String) {
    let mut errors = Vec::new();
    let path: PathBuf = dir.join(&s.name);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => {
            return (
                vec![format!("cannot read {}: {e}", path.display())],
                String::new(),
            );
        }
    };
    let mut img = DiskImage::noise(AT + bytes.len() + TAIL, 0x5A3);
    img.put(AT, &bytes);
    let file = img.save();
    let source = FileSource::open(file.path()).expect("open image");
    let found = findings(&source);
    let all: Vec<String> = found
        .iter()
        .map(|f| format!("{}@{}+{} {:?}", f.kind.as_str(), f.start, f.len, f.health))
        .collect();
    let of_kind: Vec<&Finding> = found.iter().filter(|f| f.kind.as_str() == s.kind).collect();
    if of_kind.len() != 1 {
        errors.push(format!("expected one {} finding, got: {all:?}", s.kind));
    }
    let Some(f) = found.iter().find(|f| f.start == AT as u64) else {
        errors.push(format!("nothing found at {AT}; findings: {all:?}"));
        return (errors, String::new());
    };
    if f.kind.as_str() != s.kind {
        errors.push(format!("kind {} (expected {})", f.kind.as_str(), s.kind));
    }
    if f.health != Health::Intact {
        errors.push(format!("health {:?} (expected Intact)", f.health));
    }
    let mut note = String::new();
    let padding = usize::try_from(f.len)
        .ok()
        .and_then(|l| bytes.get(l..))
        .filter(|tail| tail.len() < MAX_PADDING && tail.iter().all(|&b| b == 0));
    match padding {
        _ if f.len == bytes.len() as u64 => {}
        Some(tail) => note = format!(" (+{} bytes of trailing zero padding)", tail.len()),
        None => errors.push(format!("len {} (file is {} bytes)", f.len, bytes.len())),
    }
    let i = inspect(&source, f);
    let m = &i.metadata;
    if m.make.is_none() || m.model.is_none() {
        errors.push(format!("make/model missing: {m:?}"));
    }
    let preview = match (&i.preview, s.kind.as_str()) {
        (Some(p), "heic") => {
            errors.push(format!("HEIC has no JPEG preview, got {p:?}"));
            String::from("-")
        }
        (None, "heic") => String::from("-"),
        (None, _) => {
            errors.push("no preview".into());
            String::from("none")
        }
        (Some(p), _) => {
            let start = p.start as usize;
            match source_slice(&source, start, p.len as usize) {
                Some(data) => {
                    if let Err(e) = strict_decode(&data) {
                        errors.push(format!("preview {p:?} does not strict-decode: {e}"));
                    }
                }
                None => errors.push(format!("preview {p:?} unreadable")),
            }
            format!(
                "{}x{} @+{} ({} B)",
                p.width,
                p.height,
                p.start - f.start,
                p.len
            )
        }
    };
    let summary = format!(
        "{} {} {:?} len={}{note} | {} {} | lens={:?} iso={:?} at={:?} size={:?}x{:?} | preview {preview} | findings {all:?}",
        s.name,
        f.kind.as_str(),
        f.health,
        f.len,
        m.make.as_deref().unwrap_or("?"),
        m.model.as_deref().unwrap_or("?"),
        m.lens,
        m.iso,
        m.captured_at,
        m.width,
        m.height,
    );
    (errors, summary)
}

fn source_slice(source: &dyn Source, start: usize, len: usize) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; len];
    (source.read_at(start as u64, &mut buf).ok()? == len).then_some(buf)
}

#[test]
#[ignore = "needs downloaded samples: scripts/fetch-samples.sh, then REVENANT_SAMPLES=samples/cache"]
fn real_camera_samples() {
    let dir = PathBuf::from(
        std::env::var_os("REVENANT_SAMPLES")
            .expect("set REVENANT_SAMPLES to the samples cache directory"),
    );
    // A relative path is taken from the workspace root (where the script puts the cache), not the crate directory.
    let dir = if dir.is_relative() {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(dir)
    } else {
        dir
    };
    let mut failures = Vec::new();
    for s in samples() {
        let (errors, summary) = check(&dir, &s);
        println!(
            "{} {summary}",
            if errors.is_empty() { "PASS" } else { "FAIL" }
        );
        for e in &errors {
            println!("    {e}");
        }
        if !errors.is_empty() {
            failures.push(s.name);
        }
    }
    assert!(failures.is_empty(), "failing samples: {failures:?}");
}
