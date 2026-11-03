//! Asking the system for permission to read a disk, and proving it works with one sector.

use revenant_core::EngineError;
use revenant_core::device::{DeviceOps, SystemDevices};
use revenant_core::source::{DiskId, RawDevice, open_with_system_prompt};
use serde::Serialize;

use crate::ErrorView;
use crate::disks::TASK_FAILED;

/// What the interface learns when access was granted. The access itself is released before this is returned.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AccessView {
    pub id: String,
    pub size: u64,
}

/// What opens a disk: the system's permission prompt in the app, a fake in tests.
pub type Opener<'a> = dyn Fn(&DiskId) -> Result<Box<dyn RawDevice>, EngineError> + 'a;

/// The disk must be listed and must not be the system's.
fn check_listed(ops: &dyn DeviceOps, id: &DiskId) -> Result<(), ErrorView> {
    let disks = ops.list().map_err(|e| ErrorView::from(&e))?;
    match disks.iter().find(|d| d.id == *id) {
        None => Err(ErrorView::from(&EngineError::DeviceNotFound)),
        Some(d) if d.system => Err(ErrorView::from(&EngineError::SystemDisk)),
        Some(_) => Ok(()),
    }
}

/// Opens a disk through `opener`, reads its first sector, releases it.
pub fn access_with(
    ops: &dyn DeviceOps,
    opener: &Opener<'_>,
    disk_id: &str,
) -> Result<AccessView, ErrorView> {
    let id = DiskId::parse(disk_id).map_err(|e| ErrorView::from(&e))?;
    check_listed(ops, &id)?;
    let device = opener(&id).map_err(|e| ErrorView::from(&e))?;
    let geometry = device.geometry();
    let mut sector = vec![0u8; geometry.logical_sector.max(1) as usize];
    device
        .read_aligned(0, &mut sector)
        .map_err(|e| ErrorView::from(&e))?;
    // The password prompt can stay open for a long time: the disk may be gone by now.
    let still_there = check_listed(ops, &id);
    // Release the access before answering, whatever the check says.
    drop(device);
    still_there?;
    Ok(AccessView {
        id: id.as_str().to_string(),
        size: geometry.size,
    })
}

/// Asks the system for permission to read the disk (it shows its own password prompt), off the window thread.
#[tauri::command]
pub async fn request_access(disk_id: String) -> Result<AccessView, ErrorView> {
    tauri::async_runtime::spawn_blocking(move || {
        access_with(&SystemDevices, &open_with_system_prompt, &disk_id)
    })
    .await
    .map_err(|_| ErrorView { code: TASK_FAILED })?
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use revenant_core::source::Geometry;

    use super::*;
    use crate::disks::fakes::{FakeOps, disk};

    /// A disk of `data` in memory that counts its reads and remembers where they were.
    struct MemDevice {
        data: Vec<u8>,
        reads: Arc<Mutex<Vec<(u64, usize)>>>,
        alive: Arc<AtomicUsize>,
    }

    impl Drop for MemDevice {
        fn drop(&mut self) {
            self.alive.fetch_sub(1, Ordering::SeqCst);
        }
    }

    impl RawDevice for MemDevice {
        fn geometry(&self) -> Geometry {
            Geometry {
                size: self.data.len() as u64,
                logical_sector: 512,
                physical_sector: 512,
            }
        }
        fn read_aligned(&self, offset: u64, buf: &mut [u8]) -> Result<usize, EngineError> {
            self.reads.lock().unwrap().push((offset, buf.len()));
            let start = offset as usize;
            let n = buf.len().min(self.data.len().saturating_sub(start));
            buf[..n].copy_from_slice(&self.data[start..start + n]);
            Ok(n)
        }
    }

    struct Rig {
        reads: Arc<Mutex<Vec<(u64, usize)>>>,
        alive: Arc<AtomicUsize>,
        opens: AtomicUsize,
    }

    impl Rig {
        fn new() -> Rig {
            Rig {
                reads: Arc::default(),
                alive: Arc::default(),
                opens: AtomicUsize::new(0),
            }
        }
        fn open(&self) -> Result<Box<dyn RawDevice>, EngineError> {
            self.opens.fetch_add(1, Ordering::SeqCst);
            self.alive.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(MemDevice {
                data: vec![7; 4096],
                reads: self.reads.clone(),
                alive: self.alive.clone(),
            }))
        }
    }

    #[test]
    fn request_access_reads_the_first_sector() {
        let ops = FakeOps::new(vec![disk("disk4")]);
        let rig = Rig::new();
        let got = access_with(&ops, &|_| rig.open(), "disk4").unwrap();
        assert_eq!(
            got,
            AccessView {
                id: "disk4".into(),
                size: 4096
            }
        );
        assert_eq!(
            *rig.reads.lock().unwrap(),
            [(0, 512)],
            "one read of one sector"
        );
        assert_eq!(rig.alive.load(Ordering::SeqCst), 0, "access released");
    }

    #[test]
    fn request_access_refuses_the_system_disk() {
        let mut sys = disk("disk0");
        sys.system = true;
        let ops = FakeOps::new(vec![sys]);
        let rig = Rig::new();
        let err = access_with(&ops, &|_| rig.open(), "disk0").unwrap_err();
        assert_eq!(err.code, "system_disk");
        assert_eq!(rig.opens.load(Ordering::SeqCst), 0, "never opened");
    }

    #[test]
    fn request_access_unknown_disk() {
        let ops = FakeOps::new(vec![disk("disk4")]);
        let rig = Rig::new();
        let err = access_with(&ops, &|_| rig.open(), "disk9").unwrap_err();
        assert_eq!(err.code, "device_not_found");
        assert_eq!(rig.opens.load(Ordering::SeqCst), 0);
        let err = access_with(&ops, &|_| rig.open(), "disk4s1").unwrap_err();
        assert_eq!(err.code, "not_whole_disk");
    }

    #[test]
    fn request_access_denied() {
        let ops = FakeOps::new(vec![disk("disk4")]);
        let err = access_with(&ops, &|_| Err(EngineError::PermissionDenied), "disk4").unwrap_err();
        assert_eq!(err.code, "permission_denied");
    }

    #[test]
    fn request_access_disk_gone_after_prompt() {
        let ops = FakeOps::new(vec![disk("disk4")]);
        let rig = Rig::new();
        let opener = |_: &DiskId| {
            *ops.disks.lock().unwrap() = Ok(Vec::new());
            rig.open()
        };
        let err = access_with(&ops, &opener, "disk4").unwrap_err();
        assert_eq!(err.code, "device_not_found");
        assert_eq!(rig.alive.load(Ordering::SeqCst), 0, "access released");
    }
}
