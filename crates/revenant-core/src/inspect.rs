//! What a found file says about itself: the photo details the results screen shows (camera, lens, serial, ISO, capture
//! time, dimensions) and the embedded JPEG it can show as a preview.
//!
//! [`inspect`] never fails and never panics on hostile data: it reads through the same bounded readers as the detectors
//! (the EXIF/TIFF walk, the box and segment walkers), every read lies inside the finding, and every value is validated
//! or dropped (dates must be real and from 1990 on, strings plain printable ASCII, ISO above zero, previews must have a
//! parseable SOF and lie inside the finding).

use crate::formats::isobmff::{
    be32, be64, child, children, cr3_full_jpeg, cr3_prvw, heic_exif_item, load, top_level,
};
use crate::formats::jpeg::{JPEG_MAX_LEN, exif_tiff, sof_dims};
use crate::formats::raf::embedded_jpeg;
use crate::formats::{FileKind, Fragment, Reader};
use crate::scan::{Finding, ranges};
use crate::source::{Fragmented, Source};
use crate::tiff::{TIFF_MAX_LEN, TiffInfo, walk, walk_exif_ifd};

/// Photo details read from a found file. Every field is `None` when the file does not hold it or holds something
/// invalid.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Metadata {
    pub make: Option<String>,
    pub model: Option<String>,
    pub lens: Option<String>,
    pub serial: Option<String>,
    pub iso: Option<u32>,
    /// Camera-local capture time "YYYY-MM-DDTHH:MM:SS" (EXIF DateTimeOriginal; for MP4/MOV the `mvhd` creation time in
    /// UTC).
    pub captured_at: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

/// An embedded JPEG inside the found file (absolute range in the source), ready to show as the file's preview.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Preview {
    /// Where the JPEG starts (the first piece's start when it is in pieces).
    pub start: u64,
    /// Total length of the JPEG (the sum of its pieces when it has any).
    pub len: u64,
    pub width: u32,
    pub height: u32,
    /// Empty when the JPEG is one range of the source. Otherwise at least 2 absolute pieces in file order: the first
    /// starts at `start`, the lengths sum to `len`, no two overlap (a preview of a file found in pieces may span them).
    pub fragments: Vec<Fragment>,
}

impl Preview {
    /// The JPEG's byte ranges in file order: `[Fragment { start, len }]` when contiguous, else `fragments`.
    pub fn ranges(&self) -> Vec<Fragment> {
        ranges(self.start, self.len, &self.fragments)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Inspection {
    pub metadata: Metadata,
    pub preview: Option<Preview>,
}

/// Earliest year accepted for a capture time (before digital cameras were common: a zeroed or reset clock).
const MIN_YEAR: u32 = 1990;
/// Seconds between 1904-01-01 (the QuickTime epoch) and 1970-01-01.
const QT_TO_UNIX: i64 = 2_082_844_800;
/// Canon's `uuid` that holds `CMT1`/`CMT2` inside `moov`.
const CR3_META_UUID: [u8; 16] = [
    0x85, 0xc0, 0xb6, 0x87, 0x82, 0x0f, 0x11, 0xe0, 0x81, 0x11, 0xf4, 0xce, 0x46, 0x2b, 0x6a, 0x48,
];

/// Reads the photo details and the embedded preview of the file `finding` describes. Never fails: what cannot be read
/// is `None`.
///
/// A file found in pieces is inspected as one contiguous file read through a [`Fragmented`] view of its pieces; its
/// preview's range is mapped back to absolute pieces of the source.
pub fn inspect(source: &dyn Source, finding: &Finding) -> Inspection {
    if !finding.fragments.is_empty() {
        return inspect_fragmented(source, finding);
    }
    let mut r = Reader::new(source);
    let r = &mut r;
    let f = Span {
        start: finding.start,
        len: finding.len.min(source.len().saturating_sub(finding.start)),
    };
    match finding.kind {
        FileKind::Jpeg => jpeg(r, f, f),
        FileKind::Cr2
        | FileKind::Nef
        | FileKind::Arw
        | FileKind::Dng
        | FileKind::Orf
        | FileKind::Rw2
        | FileKind::Tiff => tiff_raw(r, f),
        FileKind::Raf => raf(r, f),
        FileKind::Cr3 => cr3(r, f),
        FileKind::Heic => heic(r, f),
        FileKind::Mp4 | FileKind::Mov => video(r, f),
    }
}

/// Inspects a file in pieces through a logical view of them, then makes the preview's range absolute again.
fn inspect_fragmented(source: &dyn Source, finding: &Finding) -> Inspection {
    let view = Fragmented::new(source, finding.fragments.clone());
    let logical = Finding {
        start: 0,
        fragments: Vec::new(),
        ..finding.clone()
    };
    let Inspection { metadata, preview } = inspect(&view, &logical);
    let preview = preview.and_then(|p| {
        let mut pieces = view.absolute(p.start, p.len);
        let first = pieces.first()?;
        let start = first.start;
        if pieces.len() == 1 {
            pieces.clear();
        }
        Some(Preview {
            start,
            fragments: pieces,
            ..p
        })
    });
    Inspection { metadata, preview }
}

/// An absolute byte range of the source.
#[derive(Clone, Copy)]
struct Span {
    start: u64,
    len: u64,
}

impl Span {
    fn end(self) -> u64 {
        self.start.saturating_add(self.len)
    }

    /// The range `[start, start + len)` when it lies inside `self`.
    fn sub(self, start: u64, len: u64) -> Option<Span> {
        let end = start.checked_add(len)?;
        (start >= self.start && end <= self.end()).then_some(Span { start, len })
    }
}

/// A JPEG (`jpeg`, inside the finding `file`): EXIF from its APP1, size from its SOF, itself as the preview.
fn jpeg(r: &mut Reader, file: Span, jpeg: Span) -> Inspection {
    let Some(jpeg) = file.sub(jpeg.start, jpeg.len) else {
        return Inspection::default();
    };
    let mut metadata = exif_tiff(r, jpeg.start, jpeg.len)
        .and_then(|(base, len)| walk(r, base, len))
        .map(|info| metadata_of(&info))
        .unwrap_or_default();
    let preview = best_preview(r, file, &[(jpeg.start, jpeg.len)]);
    if let Some(p) = &preview {
        metadata.width = Some(p.width);
        metadata.height = Some(p.height);
    }
    Inspection { metadata, preview }
}

/// TIFF-based RAW (and plain TIFF): fields from the IFD walk, the largest embedded JPEG as the preview.
fn tiff_raw(r: &mut Reader, f: Span) -> Inspection {
    let Some(info) = walk(r, f.start, f.len.min(TIFF_MAX_LEN)) else {
        return Inspection::default();
    };
    let candidates: Vec<(u64, u64)> = info
        .jpegs
        .iter()
        .map(|&(o, l)| (f.start.saturating_add(o), l))
        .collect();
    Inspection {
        metadata: metadata_of(&info),
        preview: best_preview(r, f, &candidates),
    }
}

/// Fujifilm RAF: EXIF and preview from the embedded JPEG.
fn raf(r: &mut Reader, f: Span) -> Inspection {
    match embedded_jpeg(r, f.start) {
        Some((off, len)) => jpeg(
            r,
            f,
            Span {
                start: f.start.saturating_add(off),
                len,
            },
        ),
        None => Inspection::default(),
    }
}

/// Canon CR3: Make/Model from `CMT1`, the Exif fields from `CMT2`; the larger of the `PRVW` JPEG and the first chunk of
/// the first track (when it is a JPEG) as the preview.
fn cr3(r: &mut Reader, f: Span) -> Inspection {
    let top = top_level(r, f.start, f.len);
    let mut candidates: Vec<(u64, u64)> = cr3_prvw(r, &top.uuids).into_iter().collect();
    let mut metadata = Metadata::default();
    if let Some((at, len)) = top.moov
        && let Some(moov) = load(r, at, len)
    {
        let mut garbled = false;
        // The same candidates the CR3's health is judged on.
        candidates.extend(cr3_full_jpeg(r, f.start, &moov));
        let meta = children(&moov, &mut garbled)
            .into_iter()
            .find(|(k, p)| k == b"uuid" && p.starts_with(&CR3_META_UUID))
            .map(|(_, p)| {
                let boxes = children(&p[16..], &mut garbled);
                let cmt = |name: &[u8; 4]| {
                    boxes
                        .iter()
                        .find(|(k, _)| k == name)
                        .map(|&(_, b)| (at + offset_in(&moov, b), b.len() as u64))
                };
                (cmt(b"CMT1"), cmt(b"CMT2"))
            });
        if let Some((cmt1, cmt2)) = meta {
            let ifd0 = cmt1.and_then(|(o, l)| walk(r, o, l));
            let exif = cmt2.and_then(|(o, l)| walk_exif_ifd(r, o, l));
            metadata = merge(ifd0.as_ref(), exif.as_ref());
        }
    }
    Inspection {
        metadata,
        preview: best_preview(r, f, &candidates),
    }
}

/// Offset of `inner` (a sub-slice of `outer`) from the start of `outer`.
fn offset_in(outer: &[u8], inner: &[u8]) -> u64 {
    (inner.as_ptr() as usize).saturating_sub(outer.as_ptr() as usize) as u64
}

/// HEIC: the Exif item listed in `iloc` (a 4-byte offset to the TIFF header precedes it); no preview, the pixels are HEVC.
fn heic(r: &mut Reader, f: Span) -> Inspection {
    let top = top_level(r, f.start, f.len);
    let metadata = (|| {
        let (at, len) = top.meta?;
        let meta = load(r, at, len)?;
        let (off, len) = heic_exif_item(&meta)?;
        let item = f.sub(f.start.checked_add(off)?, len)?;
        let mut prefix = [0u8; 4];
        if len < 4 || r.read(item.start, &mut prefix) != 4 {
            return None;
        }
        let skip = 4u64.checked_add(u64::from(be32(&prefix)))?;
        let tiff_len = len.checked_sub(skip)?;
        let info = walk(r, item.start + skip, tiff_len)?;
        Some(metadata_of(&info))
    })();
    Inspection {
        metadata: metadata.unwrap_or_default(),
        preview: None,
    }
}

/// MP4/MOV: capture time from `mvhd`, size from the first video track's `tkhd`.
fn video(r: &mut Reader, f: Span) -> Inspection {
    let top = top_level(r, f.start, f.len);
    let mut metadata = Metadata::default();
    if let Some((at, len)) = top.moov
        && let Some(moov) = load(r, at, len)
    {
        metadata.captured_at = movie_creation(&moov);
        if let Some((w, h)) = video_size(&moov) {
            metadata.width = Some(w);
            metadata.height = Some(h);
        }
    }
    Inspection {
        metadata,
        preview: None,
    }
}

/// `mvhd` creation time of a `moov` payload.
fn movie_creation(moov: &[u8]) -> Option<String> {
    let mut garbled = false;
    let mvhd = child(moov, b"mvhd", &mut garbled)?;
    let secs = match *mvhd.first()? {
        0 => u64::from(be32(mvhd.get(4..8)?)),
        1 => be64(mvhd.get(4..12)?),
        _ => return None,
    };
    quicktime_date(secs)
}

/// Pixel size from the `tkhd` of the first track whose handler is `vide`.
fn video_size(moov: &[u8]) -> Option<(u32, u32)> {
    let mut garbled = false;
    children(moov, &mut garbled)
        .into_iter()
        .filter(|(k, _)| k == b"trak")
        .find_map(|(_, trak)| {
            let mut garbled = false;
            let video = child(trak, b"mdia", &mut garbled)
                .and_then(|m| child(m, b"hdlr", &mut garbled))
                .is_some_and(|p| p.get(8..12) == Some(b"vide"));
            if !video {
                return None;
            }
            let tkhd = child(trak, b"tkhd", &mut garbled)?;
            // Width and height are 16.16 fixed point at the end of the box, after a 36-byte matrix.
            let at = match *tkhd.first()? {
                0 => 76,
                1 => 88,
                _ => return None,
            };
            let w = be32(tkhd.get(at..at + 4)?) >> 16;
            let h = be32(tkhd.get(at + 4..at + 8)?) >> 16;
            (w > 0 && h > 0).then_some((w, h))
        })
}

/// The largest by pixel count of the candidate JPEGs (`(absolute offset, length)`) that start with SOI, lie inside the
/// finding, are at most [`JPEG_MAX_LEN`] long and have a parseable SOF.
fn best_preview(r: &mut Reader, file: Span, candidates: &[(u64, u64)]) -> Option<Preview> {
    let mut best: Option<Preview> = None;
    for &(start, len) in candidates {
        if len > JPEG_MAX_LEN || file.sub(start, len).is_none() {
            continue;
        }
        let mut magic = [0u8; 3];
        if r.read(start, &mut magic) != 3 || magic != [0xFF, 0xD8, 0xFF] {
            continue;
        }
        let Some((width, height)) = sof_dims(r, start, len) else {
            continue;
        };
        let pixels = u64::from(width) * u64::from(height);
        let larger = best
            .as_ref()
            .is_none_or(|b| pixels > u64::from(b.width) * u64::from(b.height));
        if pixels > 0 && larger {
            best = Some(Preview {
                start,
                len,
                width,
                height,
                fragments: Vec::new(),
            });
        }
    }
    best
}

fn metadata_of(info: &TiffInfo) -> Metadata {
    merge(Some(info), None)
}

/// Fields of `a` first, then those `b` adds; every value validated.
fn merge(a: Option<&TiffInfo>, b: Option<&TiffInfo>) -> Metadata {
    let infos = [a, b];
    let text = |get: fn(&TiffInfo) -> &Option<String>| {
        infos
            .iter()
            .flatten()
            .find_map(|i| get(i).as_deref().and_then(clean_string))
    };
    Metadata {
        make: text(|i| &i.make),
        model: text(|i| &i.model),
        lens: text(|i| &i.lens),
        serial: text(|i| &i.serial),
        iso: infos
            .iter()
            .flatten()
            .find_map(|i| i.iso.filter(|&v| v > 0)),
        captured_at: infos
            .iter()
            .flatten()
            .find_map(|i| i.date_original.as_deref().and_then(exif_date)),
        width: infos
            .iter()
            .flatten()
            .find_map(|i| i.width.filter(|&v| v > 0)),
        height: infos
            .iter()
            .flatten()
            .find_map(|i| i.height.filter(|&v| v > 0)),
    }
}

/// A string made of printable ASCII, trimmed, non-empty.
fn clean_string(s: &str) -> Option<String> {
    let s = s.trim();
    (!s.is_empty() && s.bytes().all(|b| b.is_ascii() && !b.is_ascii_control()))
        .then(|| s.to_string())
}

/// "YYYY:MM:DD HH:MM:SS" as "YYYY-MM-DDTHH:MM:SS", if it is a real date from [`MIN_YEAR`] on.
fn exif_date(s: &str) -> Option<String> {
    let b = s.trim().as_bytes();
    if b.len() != 19
        || [(4, b':'), (7, b':'), (10, b' '), (13, b':'), (16, b':')]
            .iter()
            .any(|&(i, c)| b[i] != c)
    {
        return None;
    }
    let num = |from: usize, to: usize| -> Option<u32> {
        let digits = &b[from..to];
        digits.iter().all(u8::is_ascii_digit).then(|| {
            digits
                .iter()
                .fold(0, |acc, &d| acc * 10 + u32::from(d - b'0'))
        })
    };
    format_date(
        num(0, 4)?,
        num(5, 7)?,
        num(8, 10)?,
        num(11, 13)?,
        num(14, 16)?,
        num(17, 19)?,
    )
}

/// "YYYY-MM-DDTHH:MM:SS" if the fields make a real date and time in [`MIN_YEAR`]..=9999.
fn format_date(y: u32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> Option<String> {
    let valid = (MIN_YEAR..=9999).contains(&y)
        && (1..=12).contains(&mo)
        && (1..=days_in_month(y, mo)).contains(&d)
        && h < 24
        && mi < 60
        && s < 60;
    valid.then(|| format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}"))
}

fn days_in_month(y: u32, m: u32) -> u32 {
    match m {
        2 if y.is_multiple_of(4) && (!y.is_multiple_of(100) || y.is_multiple_of(400)) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Seconds since 1904-01-01 UTC as "YYYY-MM-DDTHH:MM:SS" (0 means unset), if the date is valid.
fn quicktime_date(secs: u64) -> Option<String> {
    if secs == 0 {
        return None;
    }
    let unix = i64::try_from(secs).ok()?.checked_sub(QT_TO_UNIX)?;
    let (y, mo, d) = civil_from_days(unix.div_euclid(86_400));
    let rem = unix.rem_euclid(86_400) as u32;
    format_date(
        u32::try_from(y).ok()?,
        mo,
        d,
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
    )
}

/// (year, month, day) of the day `days` after 1970-01-01 in the proleptic Gregorian calendar.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // day of era, 0..=146096
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // year of era
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day of year, March-based
    let mp = (5 * doy + 2) / 153; // month, March = 0
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_from_days_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        assert_eq!(civil_from_days(59), (1970, 3, 1));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        assert_eq!(civil_from_days(11_017), (2000, 3, 1));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
        assert_eq!(civil_from_days(19_783), (2024, 3, 1));
        assert_eq!(civil_from_days(-25_567), (1900, 1, 1));
        assert_eq!(civil_from_days(-25_509), (1900, 2, 28));
        assert_eq!(civil_from_days(-25_508), (1900, 3, 1)); // 1900 is not a leap year
    }

    #[test]
    fn quicktime_dates() {
        assert_eq!(quicktime_date(0), None);
        // 1970-01-01 is before the minimum year.
        assert_eq!(quicktime_date(QT_TO_UNIX as u64), None);
        // 2000-02-29 12:34:56 UTC.
        let secs = (11_016 * 86_400 + 12 * 3600 + 34 * 60 + 56 + QT_TO_UNIX) as u64;
        assert_eq!(quicktime_date(secs).as_deref(), Some("2000-02-29T12:34:56"));
        assert_eq!(quicktime_date(u64::MAX), None);
        assert_eq!(
            quicktime_date(u64::from(u32::MAX)).as_deref(),
            Some("2040-02-06T06:28:15")
        );
    }

    #[test]
    fn exif_dates_are_validated() {
        let ok = |s: &str| exif_date(s);
        assert_eq!(
            ok("2024:05:06 07:08:09").as_deref(),
            Some("2024-05-06T07:08:09")
        );
        assert_eq!(
            ok("2000:02:29 00:00:00").as_deref(),
            Some("2000-02-29T00:00:00")
        );
        assert_eq!(ok("2023:02:29 00:00:00"), None);
        assert_eq!(ok("2100:02:29 00:00:00"), None); // 2100 is not a leap year
        assert_eq!(
            ok("2400:02:29 00:00:00").as_deref(),
            Some("2400-02-29T00:00:00")
        );
        assert_eq!(ok("2024:04:31 00:00:00"), None);
        assert_eq!(ok("2024:13:01 00:00:00"), None);
        assert_eq!(ok("2024:00:10 00:00:00"), None);
        assert_eq!(ok("2024:01:00 00:00:00"), None);
        assert_eq!(ok("2024:01:01 24:00:00"), None);
        assert_eq!(ok("2024:01:01 00:60:00"), None);
        assert_eq!(ok("2024:01:01 00:00:60"), None);
        assert_eq!(ok("1989:12:31 23:59:59"), None);
        assert_eq!(
            ok("1990:01:01 00:00:00").as_deref(),
            Some("1990-01-01T00:00:00")
        );
        assert_eq!(ok("0000:00:00 00:00:00"), None);
        assert_eq!(ok("2024-05-06 07:08:09"), None);
        assert_eq!(ok("2024:05:06 07:08"), None);
        assert_eq!(ok("2024:05:+6 07:08:09"), None);
    }

    #[test]
    fn strings_are_cleaned() {
        assert_eq!(clean_string("  Acme \t").as_deref(), Some("Acme"));
        assert_eq!(clean_string("   "), None);
        assert_eq!(clean_string("Ac\u{e9}me"), None);
        assert_eq!(clean_string("A\x01B"), None);
    }
}
