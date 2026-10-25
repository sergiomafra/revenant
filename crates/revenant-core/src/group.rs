//! Grouping of recovered files for the results screen: by shooting session, day, camera and folder. Pure functions over
//! small inputs; the result never depends on the order of the items (only `BTreeMap`s and explicit sorts).

use std::collections::{BTreeMap, BTreeSet};

use crate::formats::FileKind;
use crate::inspect::{Metadata, seconds};
use crate::scan::Finding;

/// A pause longer than this between two files of one camera starts a new session (D15).
pub const SESSION_GAP_SECS: i64 = 2 * 60 * 60;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Camera {
    pub make: Option<String>,
    pub model: Option<String>,
    pub serial: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupInput {
    pub id: u64,
    pub time: Option<String>,
    pub camera: Option<Camera>,
    pub folder: Option<String>,
}

impl GroupInput {
    /// Time (D16): photos use the capture time, else the origin's modified, else created time; MP4/MOV use the
    /// origin's modified, else created time, else the capture time (their `mvhd` date is UTC, the file system's is
    /// local). The first candidate that is a real date wins. The camera is `None` when make and model are both
    /// unknown. The folder is the origin's, whatever its state (C16: the old folders of a quick-formatted card count
    /// as a surviving structure); the root is `Some("")`, and a file with no origin, or whose folder was dropped
    /// (R6), has none.
    pub fn new(f: &Finding, m: Option<&Metadata>) -> GroupInput {
        let captured = m.and_then(|m| m.captured_at.as_deref());
        let modified = f
            .origin
            .as_ref()
            .and_then(|o| o.modified.as_ref())
            .map(|t| t.local.as_str());
        let created = f
            .origin
            .as_ref()
            .and_then(|o| o.created.as_ref())
            .map(|t| t.local.as_str());
        let order = match f.kind {
            FileKind::Mp4 | FileKind::Mov => [modified, created, captured],
            _ => [captured, modified, created],
        };
        let time = order
            .into_iter()
            .flatten()
            .find(|t| seconds(t).is_some())
            .map(str::to_owned);
        let camera = m
            .filter(|m| m.make.is_some() || m.model.is_some())
            .map(|m| Camera {
                make: m.make.clone(),
                model: m.model.clone(),
                serial: m.serial.clone(),
            });
        GroupInput {
            id: f.id,
            time,
            camera,
            folder: f
                .origin
                .as_ref()
                .and_then(|o| o.folder())
                .map(str::to_owned),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GroupLabel {
    Session {
        camera: Option<Camera>,
        first: String,
        last: String,
    },
    UndatedSession {
        camera: Option<Camera>,
    },
    Day(String),
    Undated,
    Camera(Camera),
    UnknownCamera,
    Folder(String),
    NoFolder,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    pub label: GroupLabel,
    pub ids: Vec<u64>,
}

/// (make, model) trimmed and upper-cased.
type Model = (Option<String>, Option<String>);
/// The identity of a camera body: its model and its serial.
type Key = (Model, Option<String>);

/// Per key: the smallest id with its camera, and the smallest id that has a serial with that serial.
type Best<'a> = (u64, &'a Camera, Option<(u64, &'a String)>);
/// Where a session sorts: first time, camera, first id.
type SessionOrder<'a> = (i64, (bool, &'a Option<Key>), u64);

fn norm(s: &Option<String>) -> Option<String> {
    s.as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_uppercase)
}

/// What each item's camera is, by key, and the label of every key. An item without a serial takes the serial of its
/// model when the items have exactly one distinct serial for that model; the label shows the make and model of the
/// item with the smallest id in the key, and the serial of the smallest id that has one.
struct Cameras {
    keys: Vec<Option<Key>>,
    labels: BTreeMap<Key, Camera>,
}

fn cameras(items: &[GroupInput]) -> Cameras {
    let model_of = |c: &Camera| -> Option<Model> {
        let model = (norm(&c.make), norm(&c.model));
        (model.0.is_some() || model.1.is_some()).then_some(model)
    };
    let mut serials: BTreeMap<Model, BTreeSet<String>> = BTreeMap::new();
    for c in items.iter().filter_map(|i| i.camera.as_ref()) {
        if let (Some(model), Some(serial)) = (model_of(c), norm(&c.serial)) {
            serials.entry(model).or_default().insert(serial);
        }
    }
    let keys: Vec<Option<Key>> = items
        .iter()
        .map(|i| {
            let c = i.camera.as_ref()?;
            let model = model_of(c)?;
            let serial = norm(&c.serial).or_else(|| match serials.get(&model) {
                Some(set) if set.len() == 1 => set.iter().next().cloned(),
                _ => None,
            });
            Some((model, serial))
        })
        .collect();
    // (smallest id, camera) for make/model, and (smallest id, serial string) for the serial.
    let mut best: BTreeMap<Key, Best<'_>> = BTreeMap::new();
    for (i, key) in items.iter().zip(&keys) {
        let (Some(key), Some(c)) = (key, i.camera.as_ref()) else {
            continue;
        };
        let own_serial = norm(&c.serial).and(c.serial.as_ref()).map(|s| (i.id, s));
        best.entry(key.clone())
            .and_modify(|(id, cam, serial)| {
                if i.id < *id {
                    (*id, *cam) = (i.id, c);
                }
                if let Some(own) = own_serial
                    && serial.is_none_or(|(sid, _)| own.0 < sid)
                {
                    *serial = Some(own);
                }
            })
            .or_insert((i.id, c, own_serial));
    }
    let labels = best
        .into_iter()
        .map(|(key, (_, c, serial))| {
            let camera = Camera {
                make: c.make.clone(),
                model: c.model.clone(),
                serial: serial.map(|(_, s)| s.clone()),
            };
            (key, camera)
        })
        .collect();
    Cameras { keys, labels }
}

impl Cameras {
    fn label(&self, key: &Option<Key>) -> Option<Camera> {
        key.as_ref().and_then(|k| self.labels.get(k)).cloned()
    }
}

/// `None` keys sort after every camera.
fn key_order(key: &Option<Key>) -> (bool, &Option<Key>) {
    (key.is_none(), key)
}

/// An item's place inside a group: dated items by time, then the undated, then by id.
fn order(secs: Option<i64>, id: u64) -> (bool, Option<i64>, u64) {
    (secs.is_none(), secs, id)
}

fn sorted_ids(mut members: Vec<(Option<i64>, u64)>) -> Vec<u64> {
    members.sort_by_key(|&(secs, id)| order(secs, id));
    members.into_iter().map(|(_, id)| id).collect()
}

/// Shooting sessions: per camera body, files whose times are never more than [`SESSION_GAP_SECS`] apart, in order.
/// Dated sessions come first by (first time, camera, first id), then each camera's undated files by camera
/// (no camera last).
pub fn by_session(items: &[GroupInput]) -> Vec<Group> {
    let cams = cameras(items);
    let mut dated: BTreeMap<Option<Key>, Vec<(i64, u64, &str)>> = BTreeMap::new();
    let mut undated: BTreeMap<Option<Key>, Vec<u64>> = BTreeMap::new();
    for (i, key) in items.iter().zip(&cams.keys) {
        match i.time.as_deref().and_then(|t| Some((seconds(t)?, t))) {
            Some((secs, t)) => dated.entry(key.clone()).or_default().push((secs, i.id, t)),
            None => undated.entry(key.clone()).or_default().push(i.id),
        }
    }
    // (first secs, key, first id) -> group
    let mut sessions: Vec<(SessionOrder<'_>, Group)> = Vec::new();
    for (key, members) in &mut dated {
        members.sort_unstable();
        let mut start = 0;
        for end in 1..=members.len() {
            if end < members.len() && members[end].0 - members[end - 1].0 <= SESSION_GAP_SECS {
                continue;
            }
            let run = &members[start..end];
            let label = GroupLabel::Session {
                camera: cams.label(key),
                first: run[0].2.to_owned(),
                last: run[run.len() - 1].2.to_owned(),
            };
            let group = Group {
                label,
                ids: run.iter().map(|m| m.1).collect(),
            };
            sessions.push(((run[0].0, key_order(key), run[0].1), group));
            start = end;
        }
    }
    sessions.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out: Vec<Group> = sessions.into_iter().map(|(_, g)| g).collect();
    let mut rest: Vec<(&Option<Key>, Vec<u64>)> =
        undated.iter().map(|(k, v)| (k, v.clone())).collect();
    rest.sort_by(|a, b| key_order(a.0).cmp(&key_order(b.0)));
    for (key, mut ids) in rest {
        ids.sort_unstable();
        out.push(Group {
            label: GroupLabel::UndatedSession {
                camera: cams.label(key),
            },
            ids,
        });
    }
    out
}

/// One group per calendar day ("YYYY-MM-DD" of the item's time), ascending; the undated last.
pub fn by_day(items: &[GroupInput]) -> Vec<Group> {
    let mut days: BTreeMap<Option<&str>, Vec<(Option<i64>, u64)>> = BTreeMap::new();
    for i in items {
        let secs = i.time.as_deref().and_then(seconds);
        let day = secs.and(i.time.as_deref()).map(|t| &t[..10]);
        days.entry(day).or_default().push((secs, i.id));
    }
    let mut groups: Vec<Group> = Vec::new();
    let mut undated = None;
    for (day, members) in days {
        let ids = sorted_ids(members);
        match day {
            Some(day) => groups.push(Group {
                label: GroupLabel::Day(day.to_owned()),
                ids,
            }),
            None => undated = Some(ids),
        }
    }
    groups.extend(undated.map(|ids| Group {
        label: GroupLabel::Undated,
        ids,
    }));
    groups
}

/// One group per camera body, ordered by key; the files of unknown camera last.
pub fn by_camera(items: &[GroupInput]) -> Vec<Group> {
    let cams = cameras(items);
    let mut by_key: BTreeMap<Option<Key>, Vec<(Option<i64>, u64)>> = BTreeMap::new();
    for (i, key) in items.iter().zip(&cams.keys) {
        let secs = i.time.as_deref().and_then(seconds);
        by_key.entry(key.clone()).or_default().push((secs, i.id));
    }
    let mut entries: Vec<_> = by_key.into_iter().collect();
    entries.sort_by(|a, b| key_order(&a.0).cmp(&key_order(&b.0)));
    entries
        .into_iter()
        .map(|(key, members)| Group {
            label: match cams.label(&key) {
                Some(camera) => GroupLabel::Camera(camera),
                None => GroupLabel::UnknownCamera,
            },
            ids: sorted_ids(members),
        })
        .collect()
}

/// One group per folder, ascending; the files without a folder last. `None` when no item has a folder (the structure
/// did not survive, so there is nothing to show by folder).
pub fn by_folder(items: &[GroupInput]) -> Option<Vec<Group>> {
    items.iter().any(|i| i.folder.is_some()).then(|| {
        let mut folders: BTreeMap<Option<&str>, Vec<(Option<i64>, u64)>> = BTreeMap::new();
        for i in items {
            let secs = i.time.as_deref().and_then(seconds);
            folders
                .entry(i.folder.as_deref())
                .or_default()
                .push((secs, i.id));
        }
        let mut groups: Vec<Group> = Vec::new();
        let mut none = None;
        for (folder, members) in folders {
            let ids = sorted_ids(members);
            match folder {
                Some(f) => groups.push(Group {
                    label: GroupLabel::Folder(f.to_owned()),
                    ids,
                }),
                None => none = Some(ids),
            }
        }
        groups.extend(none.map(|ids| Group {
            label: GroupLabel::NoFolder,
            ids,
        }));
        groups
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filesystem::{FsOrigin, FsState, FsTime, Placement};
    use crate::formats::Health;

    const D: &str = "2025-12-14T";

    fn cam(make: &str, model: &str, serial: Option<&str>) -> Camera {
        Camera {
            make: Some(make.into()),
            model: Some(model.into()),
            serial: serial.map(Into::into),
        }
    }
    fn a() -> Camera {
        cam("SONY", "ILCE-7M4", Some("111"))
    }
    fn b() -> Camera {
        cam("Canon", "EOS R5", Some("222"))
    }
    fn item(
        id: u64,
        time: Option<&str>,
        camera: Option<Camera>,
        folder: Option<&str>,
    ) -> GroupInput {
        GroupInput {
            id,
            time: time.map(Into::into),
            camera,
            folder: folder.map(Into::into),
        }
    }
    fn session(camera: Camera, first: &str, last: &str) -> GroupLabel {
        GroupLabel::Session {
            camera: Some(camera),
            first: format!("{D}{first}"),
            last: format!("{D}{last}"),
        }
    }
    fn at(t: &str) -> Option<String> {
        Some(format!("{D}{t}"))
    }
    fn it(id: u64, t: &str, c: Camera) -> GroupInput {
        GroupInput {
            id,
            time: at(t),
            camera: Some(c),
            folder: None,
        }
    }
    fn g(label: GroupLabel, ids: &[u64]) -> Group {
        Group {
            label,
            ids: ids.to_vec(),
        }
    }

    #[test]
    fn sessions_split_on_gaps_and_cameras() {
        let items = vec![
            it(1, "08:00:00", a()),
            it(2, "08:30:00", a()),
            it(3, "10:31:00", a()),
            it(4, "09:00:00", b()),
            GroupInput {
                id: 5,
                time: None,
                camera: Some(a()),
                folder: None,
            },
            it(6, "08:15:00", cam("SONY", "ILCE-7M4", None)),
        ];
        assert_eq!(
            by_session(&items),
            vec![
                g(session(a(), "08:00:00", "08:30:00"), &[1, 6, 2]),
                g(session(b(), "09:00:00", "09:00:00"), &[4]),
                g(session(a(), "10:31:00", "10:31:00"), &[3]),
                g(GroupLabel::UndatedSession { camera: Some(a()) }, &[5]),
            ]
        );
    }

    #[test]
    fn exactly_two_hours_is_the_same_session() {
        let items = vec![it(1, "08:00:00", a()), it(2, "10:00:00", a())];
        assert_eq!(
            by_session(&items),
            vec![g(session(a(), "08:00:00", "10:00:00"), &[1, 2])]
        );
        let items = vec![it(1, "08:00:00", a()), it(2, "10:00:01", a())];
        assert_eq!(by_session(&items).len(), 2);
    }

    #[test]
    fn two_bodies_of_one_model_stay_apart() {
        let items = vec![
            it(1, "08:00:00", a()),
            it(2, "08:10:00", cam("SONY", "ILCE-7M4", Some("333"))),
            it(3, "08:05:00", cam("SONY", "ILCE-7M4", None)),
        ];
        let groups = by_session(&items);
        assert_eq!(groups.len(), 3, "{groups:?}");
        assert_eq!(groups[0].ids, vec![1]);
        assert_eq!(groups[1].ids, vec![3]);
        assert_eq!(groups[2].ids, vec![2]);
    }

    #[test]
    fn cameras_compare_trimmed_and_case_insensitive() {
        let items = vec![
            it(1, "08:00:00", cam("SONY", "ILCE-7M4", None)),
            it(2, "08:10:00", cam(" Sony ", "ilce-7m4", None)),
        ];
        assert_eq!(by_session(&items).len(), 1);
    }

    #[test]
    fn a_camera_label_shows_the_strings_of_the_smallest_ids() {
        // One body spelled three ways; id 1 has no serial and takes the model's only serial. Make and model are
        // shown as id 1 wrote them, the serial as id 2 (the smallest id that has one) wrote it.
        let spelled = |id, make: &str, model: &str, serial: Option<&str>| {
            it(
                id,
                "08:00:00",
                Camera {
                    make: Some(make.into()),
                    model: Some(model.into()),
                    serial: serial.map(Into::into),
                },
            )
        };
        let one = spelled(1, "Sony ", "ILCE-7M4", None);
        let two = spelled(2, "SONY", "ilce-7m4", Some("abc1"));
        let three = spelled(3, "sony", "Ilce-7M4", Some(" ABC1"));
        let label = Camera {
            make: Some("Sony ".into()),
            model: Some("ILCE-7M4".into()),
            serial: Some("abc1".into()),
        };
        let orders = [
            [&one, &two, &three],
            [&one, &three, &two],
            [&two, &one, &three],
            [&two, &three, &one],
            [&three, &one, &two],
            [&three, &two, &one],
        ];
        for order in orders {
            let items: Vec<GroupInput> = order.iter().map(|i| (*i).clone()).collect();
            assert_eq!(
                by_camera(&items),
                vec![g(GroupLabel::Camera(label.clone()), &[1, 2, 3])]
            );
            let sessions = by_session(&items);
            assert_eq!(sessions.len(), 1);
            assert_eq!(
                sessions[0].label,
                GroupLabel::Session {
                    camera: Some(label.clone()),
                    first: format!("{D}08:00:00"),
                    last: format!("{D}08:00:00"),
                }
            );
        }
    }

    #[test]
    fn items_without_a_camera_group_together_and_last_when_undated() {
        let items = vec![
            item(1, at("08:00:00").as_deref(), None, None),
            item(2, at("08:10:00").as_deref(), None, None),
            item(3, None, None, None),
            item(4, None, Some(a()), None),
        ];
        let groups = by_session(&items);
        assert_eq!(
            groups,
            vec![
                Group {
                    label: GroupLabel::Session {
                        camera: None,
                        first: format!("{D}08:00:00"),
                        last: format!("{D}08:10:00"),
                    },
                    ids: vec![1, 2]
                },
                g(GroupLabel::UndatedSession { camera: Some(a()) }, &[4]),
                g(GroupLabel::UndatedSession { camera: None }, &[3]),
            ]
        );
    }

    #[test]
    fn an_unparsable_time_is_undated() {
        let items = vec![item(1, Some("yesterday"), Some(a()), None)];
        assert_eq!(
            by_session(&items),
            vec![g(GroupLabel::UndatedSession { camera: Some(a()) }, &[1])]
        );
        assert_eq!(by_day(&items), vec![g(GroupLabel::Undated, &[1])]);
    }

    fn finding(kind: FileKind, origin: Option<FsOrigin>) -> Finding {
        Finding {
            id: 9,
            start: 0,
            len: 10,
            kind,
            health: Health::Intact,
            fragments: Vec::new(),
            origin,
        }
    }
    fn origin(folder: Option<&str>, state: FsState, modified: Option<&str>) -> FsOrigin {
        FsOrigin {
            volume: 0,
            folder: folder.map(Into::into),
            name: "X.MP4".into(),
            state,
            placement: Placement::Chain,
            size: 10,
            created: Some(FsTime {
                local: "2025-12-14T06:00:00".into(),
                utc_offset_minutes: None,
            }),
            modified: modified.map(|t| FsTime {
                local: t.into(),
                utc_offset_minutes: Some(-180),
            }),
        }
    }
    fn meta(captured: &str) -> Metadata {
        Metadata {
            make: Some("SONY".into()),
            model: Some("ILCE-7M4".into()),
            serial: Some("111".into()),
            captured_at: Some(captured.into()),
            ..Metadata::default()
        }
    }

    #[test]
    fn videos_use_the_fs_time_when_present() {
        let m = meta("2025-12-14T11:20:00");
        let o = origin(Some("DCIM"), FsState::Live, Some("2025-12-14T08:20:00"));
        let video = GroupInput::new(&finding(FileKind::Mp4, Some(o)), Some(&m));
        assert_eq!(video.time.as_deref(), Some("2025-12-14T08:20:00"));
        assert_eq!(video.camera, Some(a()));
        let bare = GroupInput::new(&finding(FileKind::Mov, None), Some(&m));
        assert_eq!(bare.time.as_deref(), Some("2025-12-14T11:20:00"));
        assert_eq!(bare.folder, None);

        let mut first = it(1, "08:00:00", a());
        first.id = 1;
        let mut v1 = video.clone();
        v1.id = 2;
        let mut v2 = bare.clone();
        v2.id = 3;
        let groups = by_session(&[first, v1, v2]);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].ids, vec![1, 2]);
        assert_eq!(groups[1].ids, vec![3]);
    }

    #[test]
    fn photos_prefer_the_capture_time_then_the_fs_dates() {
        let m = meta("2025-12-14T11:20:00");
        let o = origin(Some("DCIM"), FsState::Deleted, Some("2025-12-14T08:20:00"));
        let photo = GroupInput::new(&finding(FileKind::Jpeg, Some(o.clone())), Some(&m));
        assert_eq!(photo.time.as_deref(), Some("2025-12-14T11:20:00"));
        let no_meta = GroupInput::new(&finding(FileKind::Jpeg, Some(o.clone())), None);
        assert_eq!(no_meta.time.as_deref(), Some("2025-12-14T08:20:00"));
        assert_eq!(no_meta.camera, None);
        let created_only = FsOrigin {
            modified: None,
            ..o
        };
        let c = GroupInput::new(&finding(FileKind::Jpeg, Some(created_only)), None);
        assert_eq!(c.time.as_deref(), Some("2025-12-14T06:00:00"));
    }

    #[test]
    fn a_camera_needs_a_make_or_a_model() {
        let only_serial = Metadata {
            serial: Some("1".into()),
            ..Metadata::default()
        };
        let i = GroupInput::new(&finding(FileKind::Jpeg, None), Some(&only_serial));
        assert_eq!(i.camera, None);
        let only_model = Metadata {
            model: Some("X".into()),
            ..Metadata::default()
        };
        let i = GroupInput::new(&finding(FileKind::Jpeg, None), Some(&only_model));
        assert_eq!(i.camera.and_then(|c| c.model), Some("X".into()));
    }

    #[test]
    fn every_origin_state_has_a_folder_and_a_dropped_folder_has_none() {
        for state in [FsState::Live, FsState::Deleted, FsState::Orphaned] {
            let o = origin(Some("?/100MSDCF"), state, None);
            let i = GroupInput::new(&finding(FileKind::Jpeg, Some(o)), None);
            assert_eq!(i.folder.as_deref(), Some("?/100MSDCF"));
        }
        let root = GroupInput::new(
            &finding(FileKind::Jpeg, Some(origin(Some(""), FsState::Live, None))),
            None,
        );
        assert_eq!(root.folder.as_deref(), Some(""));
        // Ruling R6/R9: a folder that was dropped is no folder.
        let dropped = GroupInput::new(
            &finding(FileKind::Jpeg, Some(origin(None, FsState::Live, None))),
            None,
        );
        assert_eq!(dropped.folder, None);
        let groups = by_folder(&[
            GroupInput { id: 1, ..dropped },
            GroupInput { id: 2, ..root },
        ])
        .unwrap();
        assert_eq!(
            groups,
            vec![
                g(GroupLabel::Folder(String::new()), &[2]),
                g(GroupLabel::NoFolder, &[1])
            ]
        );
    }

    #[test]
    fn day_camera_and_folder() {
        let items = vec![
            item(
                1,
                Some("2025-12-15T01:00:00"),
                Some(a()),
                Some("DCIM/101MSDCF"),
            ),
            item(
                2,
                Some("2025-12-14T23:00:00"),
                Some(b()),
                Some("DCIM/100MSDCF"),
            ),
            item(3, None, None, None),
            item(
                4,
                Some("2025-12-14T08:00:00"),
                Some(a()),
                Some("DCIM/100MSDCF"),
            ),
        ];
        assert_eq!(
            by_day(&items),
            vec![
                g(GroupLabel::Day("2025-12-14".into()), &[4, 2]),
                g(GroupLabel::Day("2025-12-15".into()), &[1]),
                g(GroupLabel::Undated, &[3]),
            ]
        );
        assert_eq!(
            by_camera(&items),
            vec![
                g(GroupLabel::Camera(b()), &[2]),
                g(GroupLabel::Camera(a()), &[4, 1]),
                g(GroupLabel::UnknownCamera, &[3]),
            ]
        );
        assert_eq!(
            by_folder(&items),
            Some(vec![
                g(GroupLabel::Folder("DCIM/100MSDCF".into()), &[4, 2]),
                g(GroupLabel::Folder("DCIM/101MSDCF".into()), &[1]),
                g(GroupLabel::NoFolder, &[3]),
            ])
        );
        // No folders at all: the structure did not survive.
        let flat: Vec<_> = items
            .iter()
            .cloned()
            .map(|i| GroupInput { folder: None, ..i })
            .collect();
        assert_eq!(by_folder(&flat), None);
        // Only orphaned folders count as a structure (C16).
        let orphans = vec![
            item(1, None, None, Some("?/100MSDCF")),
            item(2, None, None, None),
        ];
        assert_eq!(
            by_folder(&orphans),
            Some(vec![
                g(GroupLabel::Folder("?/100MSDCF".into()), &[1]),
                g(GroupLabel::NoFolder, &[2]),
            ])
        );
        assert_eq!(by_folder(&[]), None);
        assert!(by_session(&[]).is_empty() && by_day(&[]).is_empty() && by_camera(&[]).is_empty());
    }

    #[test]
    fn grouping_is_order_independent() {
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let cams = [a(), b(), cam("Nikon", "Z6", None)];
        let items: Vec<GroupInput> = (1..=500u64)
            .map(|id| {
                let day = next() % 4 + 10;
                let time = (next() % 7 != 0).then(|| {
                    format!(
                        "2025-12-{day:02}T{:02}:{:02}:{:02}",
                        next() % 24,
                        next() % 60,
                        next() % 60
                    )
                });
                let camera = (next() % 9 != 0).then(|| cams[(next() % 3) as usize].clone());
                let folder = (next() % 6 != 0).then(|| format!("DCIM/{}MSDCF", next() % 5));
                GroupInput {
                    id,
                    time,
                    camera,
                    folder,
                }
            })
            .collect();
        let want = (
            by_session(&items),
            by_day(&items),
            by_camera(&items),
            by_folder(&items),
        );
        let mut seen: Vec<u64> = want.0.iter().flat_map(|g| g.ids.clone()).collect();
        seen.sort_unstable();
        assert_eq!(
            seen,
            (1..=500).collect::<Vec<_>>(),
            "every item exactly once"
        );
        for _ in 0..20 {
            let mut shuffled = items.clone();
            for i in (1..shuffled.len()).rev() {
                shuffled.swap(i, (next() % (i as u64 + 1)) as usize);
            }
            assert_eq!(by_session(&shuffled), want.0);
            assert_eq!(by_day(&shuffled), want.1);
            assert_eq!(by_camera(&shuffled), want.2);
            assert_eq!(by_folder(&shuffled), want.3);
        }
    }
}
