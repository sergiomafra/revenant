//! Names of recovered files: the original name when the file system gave one, else a readable one from the capture
//! date and the camera, else a name from where the file starts. Every name is a single safe file name.

use crate::inspect::Metadata;
use crate::scan::Finding;

/// Longest file name written, in bytes.
const MAX_NAME_BYTES: usize = 200;
/// Longest extension kept when cutting a long name, in bytes (without the dot).
const MAX_EXT_BYTES: usize = 16;
/// Longest camera part of a readable name, in chars.
const MAX_CAMERA_CHARS: usize = 24;

/// The name to write `f` under (D14): the sanitized original name; else `YYYY-MM-DD_HH-MM-SS[_<camera>].<EXT>` from
/// `m.captured_at` (or the origin's modified time), `EXT` being the kind's extension in upper case; else
/// `revenant_<start as 12 hex digits>.<ext>`.
pub fn file_name(f: &Finding, m: Option<&Metadata>) -> String {
    let origin = f.origin.as_ref();
    if let Some(name) = origin.and_then(|o| sanitize(o.name())) {
        return name;
    }
    let stamp = m
        .and_then(|m| m.captured_at.as_deref())
        .and_then(stamp_of)
        .or_else(|| {
            origin
                .and_then(|o| o.modified.as_ref())
                .and_then(|t| stamp_of(&t.local))
        });
    match stamp {
        Some(stamp) => {
            let camera = camera_tag(
                m.and_then(|m| m.make.as_deref()),
                m.and_then(|m| m.model.as_deref()),
            );
            let ext = f.kind.extension().to_uppercase();
            if camera.is_empty() {
                format!("{stamp}.{ext}")
            } else {
                format!("{stamp}_{camera}.{ext}")
            }
        }
        None => fallback_name(f),
    }
}

/// `revenant_<start as 12 hex digits>.<ext>`.
pub(crate) fn fallback_name(f: &Finding) -> String {
    format!("revenant_{:012x}.{}", f.start, f.kind.extension())
}

/// "YYYY-MM-DDTHH:MM:SS..." → "YYYY-MM-DD_HH-MM-SS"; `None` when the text does not have that shape.
fn stamp_of(text: &str) -> Option<String> {
    let b = text.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let shape_ok = b[..19].iter().enumerate().all(|(i, &c)| match i {
        4 | 7 => c == b'-',
        10 => c == b'T' || c == b' ',
        13 | 16 => c == b':',
        _ => c.is_ascii_digit(),
    });
    if !shape_ok {
        return None;
    }
    Some(format!(
        "{}_{}-{}-{}",
        &text[..10],
        &text[11..13],
        &text[14..16],
        &text[17..19]
    ))
}

/// Camera part of a readable name: the model, minus the make's first word when the model starts with it and a space;
/// only ASCII letters, digits and '-'; at most 24 chars; "" when unknown.
pub fn camera_tag(make: Option<&str>, model: Option<&str>) -> String {
    let Some(model) = model.map(str::trim) else {
        return String::new();
    };
    let mut rest = model;
    if let Some(word) = make.and_then(|m| m.split_whitespace().next())
        && let Some(head) = model.get(..word.len())
        && head.eq_ignore_ascii_case(word)
        && model[word.len()..].starts_with(' ')
    {
        rest = &model[word.len()..];
    }
    rest.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(MAX_CAMERA_CHARS)
        .collect()
}

fn is_format_control(c: char) -> bool {
    matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}')
}

/// Whether `c` never appears in a written name: separators, Windows-forbidden characters, controls (C0, DEL, C1), line
/// and paragraph separators, format controls and U+FFFD.
pub(crate) fn is_unsafe_char(c: char) -> bool {
    matches!(
        c,
        '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\u{FFFD}' | '\u{2028}' | '\u{2029}'
    ) || c < ' '
        || ('\u{7F}'..='\u{9F}').contains(&c)
        || is_format_control(c)
}

/// Whether `stem` (what precedes the first '.', trailing spaces trimmed) is a Windows device name.
fn is_reserved(stem: &str) -> bool {
    let upper = stem.to_uppercase();
    if matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) {
        return true;
    }
    for prefix in ["COM", "LPT"] {
        if let Some(rest) = upper.strip_prefix(prefix) {
            let mut chars = rest.chars();
            if let (Some(c), None) = (chars.next(), chars.next())
                && (c.is_ascii_digit() || matches!(c, '\u{00B9}' | '\u{00B2}' | '\u{00B3}'))
            {
                return true;
            }
        }
    }
    false
}

/// A safe single file name, or `None` when nothing is left of `name`.
///
/// Each of `/ \ : * ? " < > |`, the control chars (below 0x20 and 0x7F), the format controls (U+200B–200F,
/// U+202A–202E, U+2066–2069, U+FEFF) and U+FFFD become '_'; leading and trailing '.' and ' ' are trimmed; a Windows
/// reserved stem (`CON`, `NUL`, `COM1`… in any case, with or without extension) gets a '_' in front; at most 200 bytes
/// (before any `_N` suffix of extraction), cut on a char boundary, keeping an extension of up to 16 bytes.
pub fn sanitize(name: &str) -> Option<String> {
    let replaced: String = name
        .chars()
        .map(|c| if is_unsafe_char(c) { '_' } else { c })
        .collect();
    let trimmed = replaced.trim_matches(['.', ' ']);
    if trimmed.is_empty() {
        return None;
    }
    let stem = trimmed
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end_matches(' ');
    let mut out = if is_reserved(stem) {
        format!("_{trimmed}")
    } else {
        trimmed.to_string()
    };
    if out.len() > MAX_NAME_BYTES {
        out = cut(&out);
    }
    (!out.is_empty()).then_some(out)
}

/// Cuts `name` to [`MAX_NAME_BYTES`] on a char boundary, keeping a short extension.
fn cut(name: &str) -> String {
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 && name.len() - i - 1 <= MAX_EXT_BYTES => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    let mut end = MAX_NAME_BYTES.saturating_sub(ext.len()).min(stem.len());
    while !stem.is_char_boundary(end) {
        end -= 1;
    }
    let stem = stem[..end].trim_end_matches(['.', ' ']);
    format!("{stem}{ext}")
}

/// Splits a file name into its stem and its extension with the dot (`("a", ".jpg")`; `("a", "")` without one).
pub(crate) fn split_name(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filesystem::{FsOrigin, FsState, FsTime, Placement};
    use crate::formats::{FileKind, Health};

    fn finding(kind: FileKind, start: u64, origin: Option<FsOrigin>) -> Finding {
        Finding {
            id: 1,
            start,
            len: 10,
            kind,
            health: Health::Intact,
            fragments: Vec::new(),
            origin,
        }
    }

    fn origin(name: &str, state: FsState, modified: Option<&str>) -> FsOrigin {
        FsOrigin {
            volume: 0,
            folder: Some("DCIM".into()),
            name: name.to_string(),
            state,
            placement: Placement::Chain,
            size: 10,
            created: None,
            modified: modified.map(|t| FsTime {
                local: t.to_string(),
                utc_offset_minutes: None,
            }),
        }
    }

    fn meta(at: Option<&str>, make: Option<&str>, model: Option<&str>) -> Metadata {
        Metadata {
            captured_at: at.map(str::to_string),
            make: make.map(str::to_string),
            model: model.map(str::to_string),
            ..Metadata::default()
        }
    }

    #[test]
    fn readable_names() {
        let at = Some("2025-12-14T07:12:05");
        let named = finding(
            FileKind::Arw,
            0,
            Some(origin("DSC01234.ARW", FsState::Live, None)),
        );
        assert_eq!(file_name(&named, None), "DSC01234.ARW");
        let arw = finding(FileKind::Arw, 0, None);
        assert_eq!(
            file_name(&arw, Some(&meta(at, Some("SONY"), Some("ILCE-7M4")))),
            "2025-12-14_07-12-05_ILCE-7M4.ARW"
        );
        let cr3 = finding(FileKind::Cr3, 0, None);
        assert_eq!(
            file_name(&cr3, Some(&meta(at, Some("Canon"), Some("Canon EOS R5")))),
            "2025-12-14_07-12-05_EOSR5.CR3"
        );
        let nef = finding(FileKind::Nef, 0, None);
        let m = meta(at, Some("NIKON CORPORATION"), Some("NIKON Z 6_2"));
        assert_eq!(file_name(&nef, Some(&m)), "2025-12-14_07-12-05_Z62.NEF");
        let jpg = finding(FileKind::Jpeg, 0xA000, None);
        assert_eq!(
            file_name(&jpg, Some(&meta(at, None, None))),
            "2025-12-14_07-12-05.JPG"
        );
        assert_eq!(file_name(&jpg, None), "revenant_00000000a000.jpg");
        assert_eq!(
            file_name(&jpg, Some(&meta(None, Some("SONY"), Some("X")))),
            "revenant_00000000a000.jpg"
        );
        let deleted = finding(
            FileKind::Jpeg,
            0,
            Some(origin("_SC01235.JPG", FsState::Deleted, None)),
        );
        assert_eq!(file_name(&deleted, None), "_SC01235.JPG");
    }

    #[test]
    fn modified_time_is_the_second_choice() {
        let mut o = origin("..", FsState::Live, Some("2024-01-02T03:04:05"));
        let f = finding(FileKind::Jpeg, 7, Some(o.clone()));
        assert_eq!(file_name(&f, None), "2024-01-02_03-04-05.JPG");
        // The capture time wins over the file system's.
        assert_eq!(
            file_name(&f, Some(&meta(Some("2025-12-14T07:12:05"), None, None))),
            "2025-12-14_07-12-05.JPG"
        );
        // A malformed time is no time.
        o.modified = Some(FsTime {
            local: "garbage".into(),
            utc_offset_minutes: None,
        });
        let f = finding(FileKind::Jpeg, 7, Some(o));
        assert_eq!(file_name(&f, None), "revenant_000000000007.jpg");
    }

    #[test]
    fn sanitize_rules() {
        let s = |n: &str| sanitize(n);
        assert_eq!(s("../../x.jpg").as_deref(), Some("_.._x.jpg"));
        assert_eq!(s("/abs/x.jpg").as_deref(), Some("_abs_x.jpg"));
        assert_eq!(s("C:\\x.jpg").as_deref(), Some("C__x.jpg"));
        assert_eq!(s("a\u{0}b.jpg").as_deref(), Some("a_b.jpg"));
        assert_eq!(s(".."), None);
        assert_eq!(s("  "), None);
        assert_eq!(s("CON.JPG").as_deref(), Some("_CON.JPG"));
        assert_eq!(s("nul").as_deref(), Some("_nul"));
        assert_eq!(s("x\u{FFFD}.jpg").as_deref(), Some("x_.jpg"));
        let long = s(&format!("{}.jpg", "a".repeat(300))).unwrap();
        assert_eq!(long.len(), 200);
        assert!(long.ends_with(".jpg"));
        assert_eq!(s("DSC01234.ARW").as_deref(), Some("DSC01234.ARW"));
        assert_eq!(s("COM0").as_deref(), Some("_COM0"));
        assert_eq!(s("lpt\u{00B9}").as_deref(), Some("_lpt\u{00B9}"));
        assert_eq!(s("CONIN$").as_deref(), Some("_CONIN$"));
        assert_eq!(s("CON.tar.gz").as_deref(), Some("_CON.tar.gz"));
        assert_eq!(s("CON .jpg").as_deref(), Some("_CON .jpg"));
        assert_eq!(s("x.jpg.").as_deref(), Some("x.jpg"));
        assert_eq!(s("x.jpg ").as_deref(), Some("x.jpg"));
        assert_eq!(s(&format!("a.{}", "b".repeat(300))).unwrap().len(), 200);
        assert_eq!(s("foto\u{202E}gpj.exe").as_deref(), Some("foto_gpj.exe"));
        // Not reserved: a longer stem, a different word, COM10.
        assert_eq!(s("CONSOLE.jpg").as_deref(), Some("CONSOLE.jpg"));
        assert_eq!(s("COM10").as_deref(), Some("COM10"));
        assert_eq!(s("a\u{7F}b").as_deref(), Some("a_b"));
        assert_eq!(
            s("a\u{85}b\u{9F}c\u{2028}d\u{2029}e").as_deref(),
            Some("a_b_c_d_e")
        );
        assert_eq!(s("a\u{A0}b").as_deref(), Some("a\u{A0}b"));
        assert_eq!(s("a*?\"<>|b").as_deref(), Some("a______b"));
        // Cut on a char boundary: 'é' is two bytes.
        let multi = s(&format!("{}.jpg", "é".repeat(150))).unwrap();
        assert!(multi.len() <= 200 && multi.ends_with(".jpg"));
        // A reserved stem gets its prefix and still fits.
        assert!(s(&format!("CON.{}", "b".repeat(300))).unwrap().len() <= 200);
    }

    #[test]
    fn camera_tags() {
        assert_eq!(camera_tag(None, None), "");
        assert_eq!(camera_tag(Some("SONY"), Some("ILCE-7M4")), "ILCE-7M4");
        assert_eq!(camera_tag(Some("Canon"), Some("Canon EOS R5")), "EOSR5");
        assert_eq!(camera_tag(Some("cAnOn"), Some("CANON eos r5")), "eosr5");
        // The make is only dropped as a whole first word.
        assert_eq!(camera_tag(Some("Can"), Some("Canon R5")), "CanonR5");
        assert_eq!(camera_tag(None, Some(&"x".repeat(40))).len(), 24);
    }
}
