//! A minimal reader for the XML property lists `diskutil -plist` and `ioreg -a` print.
//!
//! It mirrors the testkit's reader (`os_fs.rs`), which the engine cannot depend on, and reads every value type. Hostile or
//! broken input is refused, never a panic: nesting is bounded ([`MAX_DEPTH`]) and the work is linear in the text.

/// Deepest nesting of dicts and arrays accepted (the root is depth 0).
const MAX_DEPTH: u32 = 32;

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Plist {
    /// Entries in document order.
    Dict(Vec<(String, Plist)>),
    Array(Vec<Plist>),
    /// A `<string>` (or a `<date>`, kept as its text).
    Str(String),
    /// An `<integer>`. A value above `i64::MAX` (an unsigned 64-bit one) wraps, as two's complement.
    Int(i64),
    Bool(bool),
    Data(Vec<u8>),
    Real(f64),
}

impl Plist {
    /// The value of `key` in a dict (the first entry with it); `None` for anything else.
    pub(crate) fn get(&self, key: &str) -> Option<&Plist> {
        match self {
            Plist::Dict(d) => d.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub(crate) fn as_int(&self) -> Option<i64> {
        match self {
            Plist::Int(v) => Some(*v),
            _ => None,
        }
    }

    pub(crate) fn as_str(&self) -> Option<&str> {
        match self {
            Plist::Str(s) => Some(s),
            _ => None,
        }
    }

    pub(crate) fn as_bool(&self) -> Option<bool> {
        match self {
            Plist::Bool(v) => Some(*v),
            _ => None,
        }
    }

    pub(crate) fn as_array(&self) -> Option<&[Plist]> {
        match self {
            Plist::Array(a) => Some(a),
            _ => None,
        }
    }
}

/// The root value of a plist document; `None` on anything malformed (including a missing `</plist>`).
pub(crate) fn parse(text: &str) -> Option<Plist> {
    let body = &text[text.find("<plist")?..];
    let mut rest = &body[body.find('>')? + 1..];
    let root = value(&mut rest, 0)?;
    let (before, end) = tag(&mut rest)?;
    (end == "/plist" && before.trim().is_empty()).then_some(root)
}

/// The next tag (without its angle brackets, trimmed) and the text before it; comments are skipped.
fn tag<'a>(rest: &mut &'a str) -> Option<(&'a str, &'a str)> {
    loop {
        let open = rest.find('<')?;
        if rest[open..].starts_with("<!--") {
            // Comments only ever sit between elements: one inside a text is refused.
            if !rest[..open].trim().is_empty() {
                return None;
            }
            let close = rest[open..].find("-->")? + open;
            *rest = &rest[close + 3..];
            continue;
        }
        let close = rest[open..].find('>')? + open;
        let before = &rest[..open];
        let t = rest[open + 1..close].trim();
        *rest = &rest[close + 1..];
        return Some((before, t));
    }
}

/// The text of a leaf element whose opening tag was just read: everything up to `</name>`, which must come next.
fn leaf<'a>(rest: &mut &'a str, name: &str) -> Option<&'a str> {
    let (text, end) = tag(rest)?;
    (end.strip_prefix('/') == Some(name)).then_some(text)
}

fn value(rest: &mut &str, depth: u32) -> Option<Plist> {
    if depth > MAX_DEPTH {
        return None;
    }
    let (before, t) = tag(rest)?;
    if !before.trim().is_empty() {
        return None;
    }
    match t {
        "dict" => {
            let mut items = Vec::new();
            loop {
                let (before, t) = tag(rest)?;
                if !before.trim().is_empty() {
                    return None;
                }
                match t {
                    "/dict" => return Some(Plist::Dict(items)),
                    "key" => {
                        let key = unescape(leaf(rest, "key")?)?;
                        items.push((key, value(rest, depth + 1)?));
                    }
                    _ => return None,
                }
            }
        }
        "array" => {
            let mut items = Vec::new();
            loop {
                let save = *rest;
                let (before, t) = tag(rest)?;
                if t == "/array" && before.trim().is_empty() {
                    return Some(Plist::Array(items));
                }
                *rest = save;
                items.push(value(rest, depth + 1)?);
            }
        }
        "dict/" => Some(Plist::Dict(Vec::new())),
        "array/" => Some(Plist::Array(Vec::new())),
        "string/" => Some(Plist::Str(String::new())),
        "data/" => Some(Plist::Data(Vec::new())),
        "true/" => Some(Plist::Bool(true)),
        "false/" => Some(Plist::Bool(false)),
        "string" => Some(Plist::Str(unescape(leaf(rest, "string")?)?)),
        "date" => Some(Plist::Str(unescape(leaf(rest, "date")?)?)),
        "integer" => integer(leaf(rest, "integer")?.trim()).map(Plist::Int),
        "real" => leaf(rest, "real")?.trim().parse().ok().map(Plist::Real),
        "data" => base64(leaf(rest, "data")?).map(Plist::Data),
        _ => None,
    }
}

fn integer(s: &str) -> Option<i64> {
    let (neg, digits) = match s.strip_prefix('-') {
        Some(d) => (true, d),
        None => (false, s),
    };
    let magnitude = match digits
        .strip_prefix("0x")
        .or_else(|| digits.strip_prefix("0X"))
    {
        Some(hex) => u64::from_str_radix(hex, 16).ok()?,
        None if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) => {
            digits.parse::<u64>().ok()?
        }
        None => return None,
    };
    if neg {
        0i64.checked_sub_unsigned(magnitude)
    } else {
        Some(magnitude as i64)
    }
}

/// Resolves the XML entities (`&lt;` `&gt;` `&amp;` `&quot;` `&apos;`, `&#N;`, `&#xH;`); `None` on an unknown or broken one.
fn unescape(s: &str) -> Option<String> {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let semi = rest[amp..].find(';')? + amp;
        let name = &rest[amp + 1..semi];
        let c = match name {
            "lt" => '<',
            "gt" => '>',
            "amp" => '&',
            "quot" => '"',
            "apos" => '\'',
            _ => {
                let code = match name.strip_prefix("#x").or_else(|| name.strip_prefix("#X")) {
                    Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                    None => name.strip_prefix('#')?.parse().ok()?,
                };
                char::from_u32(code)?
            }
        };
        out.push(c);
        rest = &rest[semi + 1..];
    }
    out.push_str(rest);
    Some(out)
}

/// Standard base64, whitespace ignored, padding optional; `None` on any other character.
fn base64(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut acc = 0u32;
    let mut bits = 0;
    let mut padding = false;
    for b in s.bytes() {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => {
                padding = true;
                continue;
            }
            b if b.is_ascii_whitespace() => continue,
            _ => return None,
        };
        if padding {
            return None;
        }
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const INFO: &str = include_str!("../../tests/fixtures/diskutil/info-sdcard.plist");
    const IOREG: &str = include_str!("../../tests/fixtures/diskutil/ioreg-sdcard.plist");

    #[test]
    fn plist_parses_diskutil_output() {
        let info = parse(INFO).unwrap();
        assert_eq!(info.get("DeviceBlockSize"), Some(&Plist::Int(512)));
        assert_eq!(info.get("TotalSize"), Some(&Plist::Int(64 << 20)));
        assert_eq!(
            info.get("DeviceIdentifier"),
            Some(&Plist::Str("disk2".into()))
        );
        assert_eq!(info.get("Removable"), Some(&Plist::Bool(true)));
        assert_eq!(info.get("Internal"), Some(&Plist::Bool(false)));
        assert_eq!(info.get("VolumeName"), Some(&Plist::Str(String::new())));
        assert_eq!(
            info.get("SMARTDeviceSpecificKeysMayVaryNotGuaranteed"),
            Some(&Plist::Dict(Vec::new()))
        );
        assert_eq!(info.get("Missing"), None);

        let ioreg = parse(IOREG).unwrap();
        let devices = ioreg.as_array().unwrap();
        assert_eq!(devices.len(), 4);
        let image = &devices[3];
        let location = image
            .get("Protocol Characteristics")
            .and_then(|p| p.get("Virtual Interface Location Path"));
        assert_eq!(
            location,
            Some(&Plist::Data(b"/private/tmp/TESTUSER/card.img".to_vec()))
        );
    }

    #[test]
    fn plist_reads_every_value_type() {
        let doc = r#"<?xml version="1.0"?><plist version="1.0"><dict>
            <key>a&amp;b &lt;&#65;&#x42;&gt;</key><string>x &quot;y&quot; &apos;z&apos;</string>
            <key>n</key><integer>-42</integer>
            <key>big</key><integer>18446744073709551615</integer>
            <key>hex</key><integer>0x10</integer>
            <key>r</key><real>1.5</real>
            <key>d</key><data>
              aGVs
              bG8=
            </data>
            <key>t</key><date>2026-10-25T10:00:00Z</date>
            <key>e</key><array/>
            <key>f</key><false/>
        </dict></plist>"#;
        let p = parse(doc).unwrap();
        assert_eq!(
            p.get("a&b <AB>").and_then(Plist::as_str),
            Some(r#"x "y" 'z'"#)
        );
        assert_eq!(p.get("n").and_then(Plist::as_int), Some(-42));
        assert_eq!(p.get("big").and_then(Plist::as_int), Some(-1));
        assert_eq!(p.get("hex").and_then(Plist::as_int), Some(16));
        assert_eq!(p.get("r"), Some(&Plist::Real(1.5)));
        assert_eq!(p.get("d"), Some(&Plist::Data(b"hello".to_vec())));
        assert_eq!(
            p.get("t").and_then(Plist::as_str),
            Some("2026-10-25T10:00:00Z")
        );
        assert_eq!(p.get("e").and_then(Plist::as_array), Some(&[][..]));
        assert_eq!(p.get("f"), Some(&Plist::Bool(false)));
    }

    fn nested(levels: usize) -> String {
        let mut s = String::from("<plist>");
        for _ in 0..levels {
            s.push_str("<dict><key>k</key>");
        }
        s.push_str("<true/>");
        for _ in 0..levels {
            s.push_str("</dict>");
        }
        s.push_str("</plist>");
        s
    }

    #[test]
    fn plist_rejects_deep_or_broken_input() {
        assert!(parse(&nested(32)).is_some());
        assert!(parse(&nested(33)).is_none());
        assert!(parse(&nested(40)).is_none());
        // Every truncation before the closing `</plist>` is refused, without a panic.
        let end = INFO.find("</plist>").unwrap() + "</plist>".len();
        for cut in 0..end {
            if let Some(prefix) = INFO.get(..cut) {
                assert!(parse(prefix).is_none(), "cut at {cut}");
            }
        }
        assert!(parse(&INFO[..end]).is_some());
        for bad in [
            "<plist><integer>12a</integer></plist>",
            "<plist><integer></integer></plist>",
            "<plist><integer>-</integer></plist>",
            "<plist><string>a &bogus; b</string></plist>",
            "<plist><string>a &amp b</string></plist>",
            "<plist><string>&#xD800;</string></plist>",
            "<plist><data>a$b</data></plist>",
            "<plist><data>aG==Vs</data></plist>",
            "<plist><real>x</real></plist>",
            "<plist><dict><key>a</key></dict></plist>",
            "<plist><dict><string>a</string></dict></plist>",
            "<plist><dict>junk<key>a</key><true/></dict></plist>",
            "<plist><string>a</integer></plist>",
            "<plist><array><true/></plist>",
            "<plist><true/><true/></plist>",
            "<plist><true/></plist",
            "<plist><unknown/></plist>",
            "<plist><!-- open comment <true/></plist>",
            "no plist here",
            "",
        ] {
            assert!(parse(bad).is_none(), "{bad}");
        }
        assert_eq!(
            parse("<plist><!-- a <comment> --><true/><!-- b --></plist>"),
            Some(Plist::Bool(true))
        );
    }
}
