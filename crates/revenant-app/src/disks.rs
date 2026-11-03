//! The disk list the interface shows: the engine's disks, classified and ordered.

use revenant_core::device::{
    Connection, DeviceOps, DiskInfo, SystemDevices, VolumeInfo, looks_like_camera_card,
};
use serde::Serialize;

use crate::ErrorView;

/// The error code when the background task running a command fails (it panicked or was cancelled).
pub const TASK_FAILED: &str = "unknown";

/// What kind of disk it is, for the interface to group and label.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiskKind {
    Card,
    External,
    Internal,
    System,
}

/// The pieces of a disk's name; the interface builds the text (label, else brand and model, else "Disk" and size).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DiskName {
    pub label: Option<String>,
    pub vendor: Option<String>,
    pub model: Option<String>,
}

/// A disk as the interface sees it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DiskView {
    pub id: String,
    pub name: DiskName,
    pub size: u64,
    pub fs: Option<String>,
    pub connection: &'static str,
    pub kind: DiskKind,
    pub solid_state: bool,
    pub camera_card: bool,
    pub selectable: bool,
    pub highlighted: bool,
}

/// The EFI system partition (every GPT disk has one, labelled `EFI`): it names nothing about the disk.
fn is_efi(v: &VolumeInfo) -> bool {
    v.label
        .as_deref()
        .is_some_and(|l| l.trim().eq_ignore_ascii_case("EFI"))
}

/// Classifies a disk; `None` for disk images the system attached (they are never offered).
pub fn classify(disk: &DiskInfo, camera_card: bool) -> Option<DiskView> {
    if disk.virtual_disk {
        return None;
    }
    let solid_state = disk.solid_state == Some(true);
    let kind = if disk.system {
        DiskKind::System
    } else if disk.removable && (disk.connection == Connection::SdCard || !solid_state) {
        DiskKind::Card
    } else if !disk.internal {
        DiskKind::External
    } else {
        DiskKind::Internal
    };
    let volumes = || disk.volumes.iter().filter(|v| !is_efi(v));
    Some(DiskView {
        id: disk.id.as_str().to_string(),
        name: DiskName {
            label: volumes().find_map(|v| v.label.clone()),
            vendor: disk.vendor.clone(),
            model: disk.model.clone(),
        },
        size: disk.geometry.size,
        fs: volumes().find_map(|v| v.fs.clone()),
        connection: disk.connection.code(),
        kind,
        solid_state,
        camera_card,
        selectable: kind != DiskKind::System,
        highlighted: matches!(kind, DiskKind::Card | DiskKind::External),
    })
}

/// The listed disks in display order, from any `DeviceOps`.
pub fn list_with(
    ops: &dyn DeviceOps,
    camera: impl Fn(&DiskInfo) -> bool,
) -> Result<Vec<DiskView>, ErrorView> {
    let mut views: Vec<DiskView> = ops
        .list()
        .map_err(|e| ErrorView::from(&e))?
        .iter()
        .filter_map(|d| classify(d, camera(d)))
        .collect();
    // Stable: inside each group the engine's order stays.
    views.sort_by_key(|v| match v.kind {
        DiskKind::Card | DiskKind::External => 0,
        DiskKind::Internal => 1,
        DiskKind::System => 2,
    });
    Ok(views)
}

/// Lists the connected disks, off the window thread.
#[tauri::command]
pub async fn list_disks() -> Result<Vec<DiskView>, ErrorView> {
    tauri::async_runtime::spawn_blocking(|| list_with(&SystemDevices, looks_like_camera_card))
        .await
        .map_err(|_| ErrorView { code: TASK_FAILED })?
}

#[cfg(test)]
pub(crate) mod fakes {
    use std::path::Path;
    use std::sync::Mutex;

    use revenant_core::EngineError;
    use revenant_core::device::{
        Connection, DestInfo, DeviceOps, DiskInfo, UnmountGuard, VolumeInfo,
    };
    use revenant_core::source::{DiskId, Geometry, RawDevice};

    pub fn disk(id: &str) -> DiskInfo {
        DiskInfo {
            id: DiskId::parse(id).unwrap(),
            geometry: Geometry {
                size: 1_000_000,
                logical_sector: 512,
                physical_sector: 512,
            },
            vendor: None,
            model: None,
            serial: None,
            removable: false,
            internal: true,
            connection: Connection::Internal,
            system: false,
            virtual_disk: false,
            solid_state: None,
            volumes: Vec::new(),
        }
    }

    pub fn volume(label: Option<&str>) -> VolumeInfo {
        VolumeInfo {
            id: "x".into(),
            fs: Some("exfat".into()),
            label: label.map(str::to_string),
            mount_point: None,
            size: 10,
        }
    }

    /// A `DeviceOps` listing whatever is in `disks` (which a test may change); everything else is unsupported.
    pub struct FakeOps {
        pub disks: Mutex<Result<Vec<DiskInfo>, &'static str>>,
    }

    impl FakeOps {
        pub fn new(disks: Vec<DiskInfo>) -> FakeOps {
            FakeOps {
                disks: Mutex::new(Ok(disks)),
            }
        }
    }

    impl DeviceOps for FakeOps {
        fn list(&self) -> Result<Vec<DiskInfo>, EngineError> {
            match &*self.disks.lock().unwrap() {
                Ok(d) => Ok(d.clone()),
                Err(_) => Err(EngineError::SystemTool { tool: "diskutil" }),
            }
        }
        fn open(&self, _: &DiskId) -> Result<Box<dyn RawDevice>, EngineError> {
            Err(EngineError::Unsupported)
        }
        fn destination(&self, _: &Path) -> Result<DestInfo, EngineError> {
            Err(EngineError::Unsupported)
        }
        fn unmount(&self, _: &DiskId) -> Result<UnmountGuard, EngineError> {
            Err(EngineError::Unsupported)
        }
        fn remount(&self, _: &DiskId, _: Option<UnmountGuard>) -> Result<(), EngineError> {
            Err(EngineError::Unsupported)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fakes::*;
    use super::*;
    use revenant_core::device::Connection;

    fn view(d: &DiskInfo) -> DiskView {
        classify(d, false).expect("listed")
    }

    #[test]
    fn classify_kinds() {
        let mut sd = disk("disk4");
        sd.removable = true;
        sd.internal = false;
        sd.connection = Connection::SdCard;
        sd.solid_state = Some(true);
        let v = classify(&sd, true).unwrap();
        assert_eq!(v.kind, DiskKind::Card);
        assert!(v.highlighted && v.selectable && v.camera_card);
        assert_eq!(v.connection, "sd_card");
        assert_eq!(v.id, "disk4");

        let mut stick = disk("disk5");
        stick.removable = true;
        stick.internal = false;
        stick.connection = Connection::Usb;
        assert_eq!(view(&stick).kind, DiskKind::Card, "removable, not an SSD");

        let mut hdd = disk("disk6");
        hdd.internal = false;
        hdd.connection = Connection::Usb;
        hdd.solid_state = Some(false);
        let v = view(&hdd);
        assert_eq!(
            (v.kind, v.highlighted, v.solid_state),
            (DiskKind::External, true, false)
        );

        let mut ssd = hdd.clone();
        ssd.solid_state = Some(true);
        let v = view(&ssd);
        assert_eq!((v.kind, v.solid_state), (DiskKind::External, true));

        let v = view(&disk("disk2"));
        assert_eq!(v.kind, DiskKind::Internal);
        assert!(!v.highlighted && v.selectable);

        let mut sys = disk("disk0");
        sys.system = true;
        let v = view(&sys);
        assert_eq!(v.kind, DiskKind::System);
        assert!(!v.selectable && !v.highlighted);

        let mut img = disk("disk9");
        img.virtual_disk = true;
        assert!(classify(&img, false).is_none());
    }

    fn mixed() -> FakeOps {
        let mut sys = disk("disk0");
        sys.system = true;
        let internal = disk("disk1");
        let mut ext = disk("disk2");
        ext.internal = false;
        let mut card = disk("disk3");
        card.removable = true;
        card.internal = false;
        card.connection = Connection::SdCard;
        let mut img = disk("disk4");
        img.virtual_disk = true;
        FakeOps::new(vec![sys, internal, ext, card, img])
    }

    #[test]
    fn list_orders_highlighted_first() {
        let ids: Vec<String> = list_with(&mixed(), |_| false)
            .unwrap()
            .into_iter()
            .map(|d| d.id)
            .collect();
        assert_eq!(ids, ["disk2", "disk3", "disk1", "disk0"]);
    }

    #[test]
    fn list_hides_virtual_disks() {
        let list = list_with(&mixed(), |_| false).unwrap();
        assert!(list.iter().all(|d| d.id != "disk4"));
    }

    #[test]
    fn efi_partition_names_nothing() {
        // A Mac's internal disk: the EFI system partition, then the APFS store (no label).
        let mut d = disk("disk0");
        let mut efi = volume(Some("EFI"));
        efi.fs = Some("msdos".into());
        let mut store = volume(None);
        store.fs = Some("apfs".into());
        d.volumes = vec![efi, store];
        let v = view(&d);
        assert_eq!(v.name.label, None, "EFI is not the disk's name");
        assert_eq!(v.fs.as_deref(), Some("apfs"), "nor its file system");

        let mut efi = volume(Some("efi"));
        efi.fs = Some("FAT32".into());
        d.volumes = vec![efi];
        let v = view(&d);
        assert_eq!((v.name.label, v.fs), (None, None));
    }

    #[test]
    fn display_name_falls_back() {
        let mut d = disk("disk3");
        d.volumes = vec![volume(None), volume(Some("EOS_DIGITAL"))];
        d.vendor = Some("SanDisk".into());
        d.model = Some("Ultra".into());
        let n = view(&d).name;
        assert_eq!(
            n.label.as_deref(),
            Some("EOS_DIGITAL"),
            "first volume with a label"
        );

        d.volumes = vec![volume(None)];
        let n = view(&d).name;
        assert_eq!(
            (n.label, n.vendor.as_deref(), n.model.as_deref()),
            (None, Some("SanDisk"), Some("Ultra"))
        );

        d.vendor = None;
        d.model = None;
        let n = view(&d).name;
        assert_eq!((n.label, n.vendor, n.model), (None, None, None));
    }

    #[test]
    fn list_error_is_a_code() {
        let ops = FakeOps::new(Vec::new());
        *ops.disks.lock().unwrap() = Err("x");
        assert_eq!(
            list_with(&ops, |_| false),
            Err(ErrorView {
                code: "system_tool_failed"
            })
        );
    }
}
