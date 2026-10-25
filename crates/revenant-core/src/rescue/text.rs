//! The map file's text format (our own), UTF-8 with decimal numbers:
//!
//! ```text
//! revenant-rescue-map 1
//! size <u64> sector <u32>
//! identity device <size> <hex model or -> <hex serial or -> <table hex> | identity image <size> <digest hex>
//! pass <fast|skipped|trim|scrape|retry:N|done> <pos>
//! <start> <len> <state>          (one line per region, ascending)
//! end <crc32 of every byte before this line, 8 lowercase hex digits>
//! ```
//!
//! A mapped image's identity (`mapped <size> <digest hex> <its origin's identity>`) nests at most [`MAX_NESTING`] deep.
//! Decoding is strict: one space between fields, no leading zeros or signs, every line ending in `\n`, nothing after the
//! `end` line; anything else is refused.

use std::collections::BTreeMap;
use std::fmt::Write;

use super::map::{MAX_MAP_REGIONS, Pass, RegionState, RescueMap};
use crate::SourceIdentity;
use crate::filesystem::crc32;

const MAGIC: &str = "revenant-rescue-map 1";
/// Deepest `mapped` identity read back.
const MAX_NESTING: usize = 4;

pub(super) fn encode(map: &RescueMap) -> Vec<u8> {
    let mut out = String::new();
    let (pass, pos) = map.pass();
    let pass = match pass {
        Pass::Retry(n) => format!("retry:{n}"),
        p => p.as_str().to_string(),
    };
    let mut identity = String::new();
    encode_identity(map.identity(), &mut identity);
    // Writing into a String cannot fail.
    let _ = write!(
        out,
        "{MAGIC}\nsize {} sector {}\nidentity {identity}\npass {pass} {pos}\n",
        map.size(),
        map.sector()
    );
    for (start, len, state) in map.regions() {
        let _ = writeln!(out, "{start} {len} {}", state.as_str());
    }
    let crc = crc32(out.as_bytes());
    let _ = writeln!(out, "end {crc:08x}");
    out.into_bytes()
}

fn encode_identity(id: &SourceIdentity, out: &mut String) {
    let hex_or_dash = |s: &Option<String>| match s {
        Some(s) => s.bytes().map(|b| format!("{b:02x}")).collect(),
        None => "-".to_string(),
    };
    let _ = match id {
        SourceIdentity::Device {
            size,
            model,
            serial,
            table,
        } => write!(
            out,
            "device {size} {} {} {table:016x}",
            hex_or_dash(model),
            hex_or_dash(serial)
        ),
        SourceIdentity::Image { size, digest } => write!(out, "image {size} {digest:016x}"),
        SourceIdentity::Mapped {
            size,
            digest,
            origin,
        } => {
            let _ = write!(out, "mapped {size} {digest:016x} ");
            encode_identity(origin, out);
            Ok(())
        }
    };
}

/// The map in `bytes`, or `None` when anything in it is wrong.
pub(super) fn decode(bytes: &[u8]) -> Option<RescueMap> {
    let body_and_end = bytes.strip_suffix(b"\n")?;
    let end_at = body_and_end.iter().rposition(|&b| b == b'\n')? + 1;
    let (body, end) = bytes.split_at(end_at);
    let crc = std::str::from_utf8(end)
        .ok()?
        .strip_prefix("end ")?
        .strip_suffix('\n')?;
    if crc.len() != 8
        || u32::from_str_radix(crc, 16).ok()? != crc32(body)
        || lower_hex(crc).is_none()
    {
        return None;
    }
    let body = std::str::from_utf8(body).ok()?;
    let mut lines = body.strip_suffix('\n')?.split('\n');
    if lines.next()? != MAGIC {
        return None;
    }
    let (size, sector) = match fields(lines.next()?)[..] {
        ["size", size, "sector", sector] => (number(size)?, u32::try_from(number(sector)?).ok()?),
        _ => return None,
    };
    if sector == 0 {
        return None;
    }
    let identity = {
        let line = lines.next()?.strip_prefix("identity ")?;
        let mut tokens = line.split(' ');
        let id = decode_identity(&mut tokens, 0)?;
        if tokens.next().is_some() {
            return None;
        }
        id
    };
    let pass = match fields(lines.next()?)[..] {
        ["pass", pass, pos] => (decode_pass(pass)?, number(pos)?),
        _ => return None,
    };
    if pass.1 > size {
        return None;
    }
    let sector64 = u64::from(sector);
    let mut regions = BTreeMap::new();
    let mut at = 0u64;
    let mut previous = None;
    for line in lines {
        let (start, len, state) = match fields(line)[..] {
            [start, len, state] => (number(start)?, number(len)?, decode_state(state)?),
            _ => return None,
        };
        let end = start.checked_add(len)?;
        let aligned = start % sector64 == 0 && (end == size || end % sector64 == 0);
        if start != at || len == 0 || end > size || !aligned || previous == Some(state) {
            return None;
        }
        if regions.len() == MAX_MAP_REGIONS {
            return None;
        }
        regions.insert(start, state);
        at = end;
        previous = Some(state);
    }
    if at != size {
        return None;
    }
    Some(RescueMap::from_parts(size, sector, identity, regions, pass))
}

fn fields(line: &str) -> Vec<&str> {
    line.split(' ').collect()
}

/// A decimal `u64` written the one way `encode` writes it: digits only, no leading zero (but "0").
fn number(s: &str) -> Option<u64> {
    let canonical =
        !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) && (s == "0" || !s.starts_with('0'));
    canonical.then(|| s.parse().ok()).flatten()
}

fn lower_hex(s: &str) -> Option<&str> {
    s.bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        .then_some(s)
}

/// A 64-bit digest written as 16 lowercase hex digits.
fn digest(s: &str) -> Option<u64> {
    if s.len() != 16 {
        return None;
    }
    u64::from_str_radix(lower_hex(s)?, 16).ok()
}

/// `-` → `None`; otherwise the UTF-8 text whose bytes are written as lowercase hex pairs.
fn hex_text(s: &str) -> Option<Option<String>> {
    if s == "-" {
        return Some(None);
    }
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let bytes = lower_hex(s)?
        .as_bytes()
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok())
        .collect::<Option<Vec<u8>>>()?;
    String::from_utf8(bytes).ok().map(Some)
}

fn decode_identity<'a>(
    tokens: &mut impl Iterator<Item = &'a str>,
    depth: usize,
) -> Option<SourceIdentity> {
    match tokens.next()? {
        "device" => Some(SourceIdentity::Device {
            size: number(tokens.next()?)?,
            model: hex_text(tokens.next()?)?,
            serial: hex_text(tokens.next()?)?,
            table: digest(tokens.next()?)?,
        }),
        "image" => Some(SourceIdentity::Image {
            size: number(tokens.next()?)?,
            digest: digest(tokens.next()?)?,
        }),
        "mapped" if depth < MAX_NESTING => {
            let size = number(tokens.next()?)?;
            let digest = digest(tokens.next()?)?;
            let origin = decode_identity(tokens, depth + 1)?;
            Some(SourceIdentity::Mapped {
                size,
                digest,
                origin: Box::new(origin),
            })
        }
        _ => None,
    }
}

fn decode_pass(s: &str) -> Option<Pass> {
    Some(match s {
        "fast" => Pass::Fast,
        "skipped" => Pass::Skipped,
        "trim" => Pass::Trim,
        "scrape" => Pass::Scrape,
        "done" => Pass::Done,
        _ => Pass::Retry(u32::try_from(number(s.strip_prefix("retry:")?)?).ok()?),
    })
}

fn decode_state(s: &str) -> Option<RegionState> {
    RegionState::ALL
        .into_iter()
        .find(|state| state.as_str() == s)
}
