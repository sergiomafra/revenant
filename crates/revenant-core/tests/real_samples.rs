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
//! One allowance on the length, for ARW only: the finding may stop before the end of the file when everything after it
//! is zero padding shorter than [`MAX_PADDING`]. Sony pads ARW files with zeros after the raw strip, to a 64 KiB (ILCE-7S) or
//! 4 KiB (ILCE-7M5) multiple; no IFD references those bytes, so the recovered file opens exactly like the original, and on
//! a card the engine cannot tell that padding from zeroed free space after the file.
//!
//! A second test, [`real_previews_split_in_two`], splits each sample's preview in two and expects the photo put back
//! together (see its comment).

use std::path::{Path, PathBuf};

use revenant_core::formats::{Health, all_formats};
use revenant_core::source::{FileSource, Source};
use revenant_core::{CancelToken, Event, Finding, Fragment, ScanOptions, inspect, scan};
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
        .filter(|tail| s.kind == "arw" && tail.len() < MAX_PADDING && tail.iter().all(|&b| b == 0));
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

/// The samples cache directory from `REVENANT_SAMPLES` (a relative path is taken from the workspace root, where the
/// script puts the cache, not the crate directory).
fn samples_dir() -> PathBuf {
    let dir = PathBuf::from(
        std::env::var_os("REVENANT_SAMPLES")
            .expect("set REVENANT_SAMPLES to the samples cache directory"),
    );
    if dir.is_relative() {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(dir)
    } else {
        dir
    }
}

#[test]
#[ignore = "needs downloaded samples: scripts/fetch-samples.sh, then REVENANT_SAMPLES=samples/cache"]
fn real_camera_samples() {
    let dir = samples_dir();
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

/// The bytes of the preview `inspect` shows for sample `s` (its largest embedded JPEG).
fn preview_of(dir: &Path, s: &Sample) -> Vec<u8> {
    let bytes = std::fs::read(dir.join(&s.name)).expect("read sample");
    let mut img = DiskImage::noise(AT + bytes.len() + TAIL, 0x5A3);
    img.put(AT, &bytes);
    let file = img.save();
    let source = FileSource::open(file.path()).expect("open image");
    let f = findings(&source)
        .into_iter()
        .find(|f| f.start == AT as u64)
        .expect("the sample is found");
    let p = inspect(&source, &f).preview.expect("a preview");
    let from = (p.start - AT as u64) as usize;
    let preview = &bytes[from..from + p.len as usize];
    // Alone, it is one `Intact` JPEG: keep the bytes the engine counts (a preview's slot may hold padding after `EOI`).
    let mut img = DiskImage::noise(AT + preview.len() + TAIL, 0x5A5);
    img.put(AT, preview);
    let file = img.save();
    let source = FileSource::open(file.path()).expect("open image");
    let alone = findings(&source)
        .into_iter()
        .find(|f| f.start == AT as u64)
        .expect("the preview is found");
    assert_eq!(alone.health, Health::Intact, "{}: {alone:?}", s.name);
    preview[..alone.len as usize].to_vec()
}

/// Bytes of other data between a split photo's first piece and its tail.
const GAP: usize = 8 << 20;

/// `first` at [`AT`], `tail` [`GAP`] bytes after its end, noise around: the finding at [`AT`].
fn split_finding(first: &[u8], tail: &[u8]) -> Option<Finding> {
    let g = AT + first.len() + GAP;
    let mut img = DiskImage::noise(g + tail.len() + 64 * 1024, 0x5A4);
    img.put(AT, first).put(g, tail);
    let file = img.save();
    let source = FileSource::open(file.path()).expect("open image");
    findings(&source).into_iter().find(|f| f.start == AT as u64)
}

/// Whether a split of the JPEG `j` at `c` falls exactly on a restart marker (`FF D0`–`D7` ends the first piece, starts
/// the second, or is split between them): such a join is at most `Partial` (ruling Q15).
fn on_restart_marker(j: &[u8], c: usize) -> bool {
    let rst = |b: u8| (0xD0..=0xD7).contains(&b);
    (j[c - 2] == 0xFF && rst(j[c - 1]))
        || (j[c] == 0xFF && rst(j[c + 1]))
        || (j[c - 1] == 0xFF && rst(j[c]))
}

/// The first piece's length for a split at `k`/10 of `len`: a whole number of sectors.
fn split_at(len: usize, k: usize) -> usize {
    (len * k / 10) / 512 * 512
}

/// Splits whose own tail is not joined: the seam crosses a band of strong texture (block-row boundaries 46–83 of the
/// EOS Kiss F preview are 3 to 9 times rougher than the image's median, so the DC seam test rejects the true seam).
/// Accepting them by comparing with the rows next to the seam would also glue one more same-frame foreign tail of the
/// tried set (ruling Q14 recorded this cost). They must stay a contiguous `Partial`.
const KNOWN_PARTIAL: [(&str, usize); 2] =
    [("canon-eos-kiss-f.cr2", 3), ("canon-eos-kiss-f.cr2", 5)];

/// Every sample's preview split at k/10 (k = 2..=8) on a sector boundary, with its own tail [`GAP`] bytes later: the
/// photo is found joined and `Intact`, pieces exact (joined but `Partial` when the split falls exactly on a restart
/// marker, ruling Q15; [`KNOWN_PARTIAL`] aside). The same first piece with the tail of the
/// next sample's preview (split at the same k) instead: never `Intact`. Every case is checked and every failure listed
/// before the test fails.
#[test]
#[ignore = "needs downloaded samples: scripts/fetch-samples.sh, then REVENANT_SAMPLES=samples/cache"]
fn real_previews_split_in_two() {
    let dir = samples_dir();
    let list = samples();
    let previews: Vec<Vec<u8>> = list.iter().map(|s| preview_of(&dir, s)).collect();
    let (mut own_ok, mut foreign_ok, mut cases, mut on_restart) = (0, 0, 0, 0);
    let mut failures = Vec::new();
    for (i, s) in list.iter().enumerate() {
        let p = &previews[i];
        let other = &previews[(i + 1) % previews.len()];
        for k in 2..=8 {
            cases += 1;
            let c = split_at(p.len(), k);
            let g = (AT + c + GAP) as u64;
            let own = split_finding(&p[..c], &p[c..]);
            let expected = vec![
                Fragment {
                    start: AT as u64,
                    len: c as u64,
                },
                Fragment {
                    start: g,
                    len: (p.len() - c) as u64,
                },
            ];
            // Q15: a seam exactly on a restart marker is joined but at most Partial.
            let health = if on_restart_marker(p, c) {
                on_restart += 1;
                Health::Partial
            } else {
                Health::Intact
            };
            let joined = own.as_ref().is_some_and(|f| {
                f.health == health && f.len == p.len() as u64 && f.fragments == expected
            });
            let known = KNOWN_PARTIAL.contains(&(s.name.as_str(), k));
            match (joined, known) {
                (true, false) => own_ok += 1,
                // A known exception stays a contiguous `Partial` (never glued to anything else).
                (false, true)
                    if own
                        .as_ref()
                        .is_some_and(|f| f.health == Health::Partial && f.fragments.is_empty()) =>
                {
                    own_ok += 1
                }
                _ => failures.push(format!(
                    "{} {k}/10 own tail{}: {own:?}",
                    s.name,
                    if known { " (known Partial)" } else { "" }
                )),
            }
            let foreign = split_finding(&p[..c], &other[split_at(other.len(), k)..]);
            match &foreign {
                Some(f) if f.health == Health::Intact => {
                    failures.push(format!("{} {k}/10 foreign tail: {foreign:?}", s.name))
                }
                _ => foreign_ok += 1,
            }
        }
    }
    println!(
        "own tails as expected: {own_ok}/{cases} ({} known Partial, {on_restart} joined Partial on a restart marker); \
         foreign tails not Intact: {foreign_ok}/{cases}",
        KNOWN_PARTIAL.len()
    );
    for f in &failures {
        println!("    {f}");
    }
    assert!(failures.is_empty(), "{} failing cases", failures.len());
}
