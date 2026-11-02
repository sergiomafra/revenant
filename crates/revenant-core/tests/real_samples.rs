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

/// The restart interval a JPEG's header declares (`DRI`, before its first `SOS`); 0 without one.
fn restart_interval(j: &[u8]) -> u16 {
    let mut at = 2;
    while at + 4 <= j.len() && j[at] == 0xFF {
        let marker = j[at + 1];
        let len = usize::from(u16::from_be_bytes([j[at + 2], j[at + 3]]));
        if marker == 0xDA {
            break;
        }
        if marker == 0xDD {
            return u16::from_be_bytes([j[at + 4], j[at + 5]]);
        }
        at += 2 + len;
    }
    0
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
/// photo is found joined and `Intact`, pieces exact (joined but `Partial` when the preview uses restart markers, ruling
/// Q16; [`KNOWN_PARTIAL`] aside). The same first piece with the tail of the
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
            // Q16: a photo with restart markers is joined but at most Partial.
            let health = if restart_interval(p) > 0 {
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
        "own tails as expected: {own_ok}/{cases} ({} known Partial, {on_restart} joined Partial for their restart markers); \
         foreign tails not Intact: {foreign_ok}/{cases}",
        KNOWN_PARTIAL.len()
    );
    for f in &failures {
        println!("    {f}");
    }
    assert!(failures.is_empty(), "{} failing cases", failures.len());
}

/// Every embedded JPEG a TIFF-based file's IFDs point at (`JPEGInterchangeFormat`/`Length`, IFD0 chain and SubIFDs), as
/// (offset, length), and the `TileByteCounts` of each IFD that has tiles. A plain little reader of the tags (the test's
/// own, so it measures the file and not the engine).
struct TiffParts {
    jpegs: Vec<(usize, usize)>,
    tile_counts: Vec<Vec<u64>>,
}

fn tiff_parts(b: &[u8]) -> Option<TiffParts> {
    let le = match b.get(..2)? {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let u16_at = |o: usize| -> Option<u64> {
        let s: [u8; 2] = b.get(o..o + 2)?.try_into().ok()?;
        Some(u64::from(if le {
            u16::from_le_bytes(s)
        } else {
            u16::from_be_bytes(s)
        }))
    };
    let u32_at = |o: usize| -> Option<u64> {
        let s: [u8; 4] = b.get(o..o + 4)?.try_into().ok()?;
        Some(u64::from(if le {
            u32::from_le_bytes(s)
        } else {
            u32::from_be_bytes(s)
        }))
    };
    // The values of an entry of type SHORT (3) or LONG (4).
    let values = |e: usize| -> Option<Vec<u64>> {
        let (t, n) = (u16_at(e + 2)?, u32_at(e + 4)? as usize);
        let size = match t {
            3 => 2,
            4 => 4,
            _ => return Some(Vec::new()),
        };
        let at = if size * n <= 4 {
            e + 8
        } else {
            u32_at(e + 8)? as usize
        };
        (0..n.min(1 << 16))
            .map(|i| {
                if size == 2 {
                    u16_at(at + 2 * i)
                } else {
                    u32_at(at + 4 * i)
                }
            })
            .collect()
    };
    let mut parts = TiffParts {
        jpegs: Vec::new(),
        tile_counts: Vec::new(),
    };
    let mut todo = vec![u32_at(4)? as usize];
    let mut seen = std::collections::HashSet::new();
    while let Some(ifd) = todo.pop() {
        if ifd == 0 || !seen.insert(ifd) || seen.len() > 64 {
            continue;
        }
        let Some(n) = u16_at(ifd) else { continue };
        let (mut jpeg, mut jpeg_len) = (None, None);
        for i in 0..n as usize {
            let e = ifd + 2 + 12 * i;
            let Some(tag) = u16_at(e) else { break };
            let v = values(e).unwrap_or_default();
            match tag {
                0x201 => jpeg = v.first().copied(),
                0x202 => jpeg_len = v.first().copied(),
                0x145 => parts.tile_counts.push(v),
                0x14A => todo.extend(v.iter().map(|&o| o as usize)),
                _ => {}
            }
        }
        if let (Some(o), Some(l)) = (jpeg, jpeg_len)
            && (o + l) as usize <= b.len()
            && l > 0
        {
            parts.jpegs.push((o as usize, l as usize));
        }
        if let Some(next) = u32_at(ifd + 2 + 12 * n as usize) {
            todo.push(next as usize);
        }
    }
    Some(parts)
}

/// The Sony samples: every embedded JPEG (the previews and thumbnails of an ARW) with its size and restart interval
/// (DRI, design D21: printed, nothing asserted about its value), and whether the lossless sensor tiles have equal
/// `TileByteCounts` (if they did, a foreign tile could match a tile boundary's exact count by its size alone: Q15). At
/// least one Sony sample must be in the folder (C5).
#[test]
#[ignore = "needs downloaded samples: scripts/fetch-samples.sh, then REVENANT_SAMPLES=samples/cache"]
fn sony_restart_intervals() {
    let dir = samples_dir();
    let mut sony = 0;
    for s in samples() {
        let Ok(bytes) = std::fs::read(dir.join(&s.name)) else {
            continue;
        };
        let parts = tiff_parts(&bytes);
        let is_sony =
            s.name.starts_with("sony") || bytes.windows(5).take(1 << 16).any(|w| w == b"SONY\0");
        if !is_sony {
            continue;
        }
        sony += 1;
        let Some(parts) = parts else {
            println!("{}: not a TIFF", s.name);
            continue;
        };
        for &(o, l) in &parts.jpegs {
            let j = &bytes[o..o + l];
            let size = sof_size(j).map_or("?".into(), |(w, h)| format!("{w}x{h}"));
            println!(
                "{}: JPEG at {o} ({l} B), {size}, DRI {}",
                s.name,
                restart_interval(j)
            );
        }
        if parts.tile_counts.is_empty() {
            println!("{}: no tiles (strip sensor data)", s.name);
        }
        for counts in &parts.tile_counts {
            let (min, max) = (counts.iter().min(), counts.iter().max());
            let distinct = counts
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len();
            println!(
                "{}: {} tiles, TileByteCounts {min:?}..{max:?}, {distinct} distinct, all equal: {}",
                s.name,
                counts.len(),
                distinct == 1
            );
        }
    }
    assert!(sony > 0, "no Sony sample in {}", dir.display());
}

/// Width and height from a JPEG's first SOF marker.
fn sof_size(j: &[u8]) -> Option<(u16, u16)> {
    let mut at = 2;
    while at + 9 <= j.len() && j[at] == 0xFF {
        let marker = j[at + 1];
        let len = usize::from(u16::from_be_bytes([j[at + 2], j[at + 3]]));
        if matches!(marker, 0xC0..=0xCF) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
            let h = u16::from_be_bytes([j[at + 5], j[at + 6]]);
            let w = u16::from_be_bytes([j[at + 7], j[at + 8]]);
            return Some((w, h));
        }
        if marker == 0xDA {
            break;
        }
        at += 2 + len;
    }
    None
}

/// A disk image in memory.
struct Image(Vec<u8>);

impl Source for Image {
    fn len(&self) -> u64 {
        self.0.len() as u64
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, revenant_core::EngineError> {
        let start = (offset as usize).min(self.0.len());
        let n = buf.len().min(self.0.len() - start);
        buf[..n].copy_from_slice(&self.0[start..start + n]);
        Ok(n)
    }
    fn volume(&self) -> Option<revenant_core::source::VolumeId> {
        None
    }
}

/// Cluster of the split-in-three card.
const SPLIT_CLUSTER: u32 = 32 * 1024;

/// `data` cut in three at multiples of [`SPLIT_CLUSTER`] (near a third and two thirds), as cluster counts.
fn thirds(len: usize) -> [usize; 3] {
    let c = SPLIT_CLUSTER as usize;
    let a = (len / 3 / c).max(1);
    let b = (2 * len / 3 / c).max(a + 1) - a;
    [a, b, len.div_ceil(c) - a - b]
}

/// Previews the search does not put back from three pieces (measured in part 1.6, Task 11): restart interval 0, so
/// nothing in the data marks where a piece ends. Out of step, their data decodes on without an error, the walk of a
/// piece breaks far past its true end, and the true cut is not among the break points tried: the file stays the file
/// system's one-run guess, `Partial` (never `Intact`), which holds the bytes of the runs after its first piece (the
/// next preview's among them). The previews with restart markers (X-S10, K-r, TG-7) do come back exact.
const KNOWN_MISSED: [&str; 5] = [
    "canon-eos-kiss-f.cr2",
    "canon-eos-r6.cr3",
    "nikon-z6.nef",
    "sony-ilce-7s.arw",
    "panasonic-dmc-lx7.rw2",
];

/// Every sample's largest preview cut in three at 32 KiB multiples on an exFAT card (32 KiB clusters, deleted on a
/// computer that clears the chain: placed by content alone), its pieces out of order, interleaved with the three pieces
/// of the next sample's preview (deleted the same way): each preview comes back with its exact bytes, never `Intact`
/// (a JPEG placed by content alone in two or more pieces is at most `Partial`, Q37 and ruling Q58), and no finding holds
/// runs of both previews; [`KNOWN_MISSED`] aside, which must still never be `Intact` (nor any finding holding runs of
/// both previews). The DRI of each preview is printed (D21). Every case is checked and every failure listed.
#[test]
#[ignore = "needs downloaded samples: scripts/fetch-samples.sh, then REVENANT_SAMPLES=samples/cache"]
fn real_previews_split_in_three() {
    use revenant_testkit::fs::{DeleteStyle, FsBuilder, FsType, mbr_head};
    let dir = samples_dir();
    let list = samples();
    let previews: Vec<Vec<u8>> = list.iter().map(|s| preview_of(&dir, s)).collect();
    let mut failures = Vec::new();
    for (i, s) in list.iter().enumerate() {
        let p = &previews[i];
        let q = &previews[(i + 1) % previews.len()];
        let (tp, tq) = (thirds(p.len()), thirds(q.len()));
        let c = u64::from(SPLIT_CLUSTER);
        let need = (p.len() + q.len()) as u64 / c + 64;
        let mut b = FsBuilder::new(FsType::Exfat, (need * c).max(16 << 20), SPLIT_CLUSTER);
        b.mkdir("DCIM");
        let first = *b.clusters("DCIM").last().unwrap() + 1;
        // On the card: P1 Q1 P3 Q2 P2 Q3 (cluster runs, one after the other).
        let order = [(0usize, 0usize), (1, 0), (0, 2), (1, 1), (0, 1), (1, 2)];
        let mut next = first;
        let mut runs: [[Vec<u32>; 3]; 2] = Default::default();
        for (who, k) in order {
            let n = if who == 0 { tp[k] } else { tq[k] } as u32;
            runs[who][k] = (next..next + n).collect();
            next += n;
        }
        let clusters = |who: usize| runs[who].concat();
        b.add_at("DCIM/P.JPG", p, &clusters(0));
        b.add_at("DCIM/Q.JPG", q, &clusters(1));
        b.delete("DCIM/P.JPG", DeleteStyle::ZeroChain);
        b.delete("DCIM/Q.JPG", DeleteStyle::ZeroChain);
        let byte_runs = |who: usize| -> Vec<(u64, u64)> {
            runs[who]
                .iter()
                .map(|r| (b.cluster_offset(r[0]), b.cluster_offset(r[r.len() - 1]) + c))
                .collect()
        };
        let mut volume = b.build();
        let head = mbr_head(&mut volume, 8192);
        let shift = head.len() as u64;
        let mut disk = head;
        disk.extend_from_slice(&volume);
        let (p_runs, q_runs): (Vec<_>, Vec<_>) = (
            byte_runs(0)
                .iter()
                .map(|&(a, z)| (a + shift, z + shift))
                .collect(),
            byte_runs(1)
                .iter()
                .map(|&(a, z)| (a + shift, z + shift))
                .collect(),
        );
        let src = Image(disk);
        let mut last: std::collections::BTreeMap<u64, Finding> = Default::default();
        scan(
            &src,
            &all_formats(),
            &ScanOptions::default(),
            &CancelToken::default(),
            &mut |e| match e {
                Event::Found(f) | Event::Updated(f) => {
                    last.insert(f.id, f);
                }
                _ => {}
            },
        )
        .expect("scan");
        let bytes_of = |f: &Finding| -> Vec<u8> {
            f.ranges()
                .iter()
                .flat_map(|r| src.0[r.start as usize..(r.start + r.len) as usize].to_vec())
                .collect()
        };
        let touches = |f: &Finding, runs: &[(u64, u64)]| {
            f.ranges().iter().any(|r| {
                runs.iter()
                    .any(|&(a, z)| r.start < z && a < r.start + r.len)
            })
        };
        let dri = restart_interval(p);
        let at = p_runs[0].0;
        let known = |name: &str| KNOWN_MISSED.contains(&name);
        let next = &list[(i + 1) % list.len()].name;
        match last.values().find(|f| f.start == at) {
            Some(f) if f.health == Health::Intact => failures.push(format!(
                "{}: Intact, never for content in pieces: {f:?}",
                s.name
            )),
            Some(f) if bytes_of(f) == *p => println!(
                "{}: DRI {dri}, {:?} in {} pieces, exact bytes{}",
                s.name,
                f.health,
                f.ranges().len(),
                if known(&s.name) {
                    " (a known miss: remove it from KNOWN_MISSED)"
                } else {
                    ""
                }
            ),
            Some(f) if known(&s.name) => println!(
                "{}: DRI {dri}, known miss: {:?} in {} pieces, not its exact bytes",
                s.name,
                f.health,
                f.ranges().len()
            ),
            other => failures.push(format!(
                "{} (DRI {dri}): not its exact bytes: {other:?}",
                s.name
            )),
        }
        for f in last.values() {
            let both = touches(f, &p_runs) && touches(f, &q_runs);
            // A known miss's one-run guess holds the runs after its first piece (`Partial`, see KNOWN_MISSED).
            let excused = f.health != Health::Intact && (known(&s.name) || known(next));
            if both && !excused {
                failures.push(format!(
                    "{}: a finding holds runs of both previews: {f:?}",
                    s.name
                ));
            }
        }
    }
    for f in &failures {
        println!("    {f}");
    }
    assert!(failures.is_empty(), "{} failing cases", failures.len());
}
