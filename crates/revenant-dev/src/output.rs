//! The JSON lines of a scan (`volume`, `found`, `updated`, `unreadable`, `disk_failing`, `group`, `summary`) and how
//! every line is written to stdout.

use std::collections::BTreeMap;
use std::io::Write;

use revenant_core::group::{self, Camera, Group, GroupInput, GroupLabel};
use revenant_core::{Finding, Fragment, Inspection, Metadata, Volume};
use serde::Serialize;

use crate::GroupBy;

#[derive(Serialize)]
#[serde(tag = "event", rename_all = "lowercase")]
pub(crate) enum Line {
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
pub(crate) struct CameraLine {
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
pub(crate) enum LabelLine {
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
pub(crate) fn group_lines(by: GroupBy, latest: &BTreeMap<u64, (Finding, Metadata)>) -> Vec<Line> {
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
pub(crate) struct VolumeLine {
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

pub(crate) fn volume_line(v: &Volume, listing_truncated: bool) -> Line {
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
pub(crate) struct PreviewLine {
    start: u64,
    len: u64,
    width: u32,
    height: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    fragments: Option<Vec<FragmentLine>>,
}

/// One piece of a file found in pieces (absolute range in the source).
#[derive(Serialize)]
pub(crate) struct FragmentLine {
    start: u64,
    len: u64,
}

/// The `fragments` key: present only when the file is in pieces.
pub(crate) fn fragment_lines(fragments: &[Fragment]) -> Option<Vec<FragmentLine>> {
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
pub(crate) struct Outputs {
    pub(crate) file: Option<String>,
    pub(crate) error: Option<&'static str>,
    pub(crate) preview_file: Option<String>,
    pub(crate) preview_error: Option<&'static str>,
}

#[derive(Serialize)]
pub(crate) struct FoundLine {
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

/// Returns false when stdout is closed (e.g. `| head`), so the caller can stop.
pub(crate) fn emit(line: &Line) -> bool {
    let mut out = std::io::stdout().lock();
    writeln!(out, "{}", serde_json::to_string(line).expect("serialize")).is_ok()
}

pub(crate) fn found_line(f: &Finding, details: Inspection, out: Outputs) -> Line {
    Line::Found(finding_fields(f, details, out))
}

/// The fields of a `found` (or `updated`) line.
pub(crate) fn finding_fields(f: &Finding, details: Inspection, out: Outputs) -> Box<FoundLine> {
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

/// Writes any serializable line; false when stdout is closed.
pub(crate) fn emit_json<T: Serialize>(line: &T) -> bool {
    let mut out = std::io::stdout().lock();
    writeln!(out, "{}", serde_json::to_string(line).expect("serialize")).is_ok()
}

/// Writes `line` with `"replayed":true` added when it comes from a saved session's log; false when stdout is closed.
pub(crate) fn emit_marked(line: &Line, replayed: bool) -> bool {
    if !replayed {
        return emit(line);
    }
    let mut value = serde_json::to_value(line).expect("serialize");
    if let Some(map) = value.as_object_mut() {
        map.insert("replayed".into(), serde_json::Value::Bool(true));
    }
    emit_json(&value)
}
