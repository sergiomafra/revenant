//! Failure-simulating sources shared by the integration tests (`mod support;`).
//!
//! [`SimSource`] serves bytes from memory (or virtual zeros) and can make chosen ranges unreadable, flaky or slow, make
//! the whole device vanish or shrink after some reads, and logs every read it receives. [`MemDevice`] is an in-memory
//! [`RawDevice`] that insists on aligned reads. [`FakeDevices`] is a [`DeviceOps`] over in-memory disks.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use revenant_core::EngineError;
use revenant_core::device::{DestInfo, DeviceOps, DiskInfo, UnmountGuard};
use revenant_core::source::{DiskId, Geometry, RawDevice, Source, VolumeId};

enum Data {
    Bytes(Vec<u8>),
    /// `len` bytes of zeros generated on read (nothing allocated).
    Zeros(u64),
}

/// A range whose first `left` reads fail.
struct Flaky {
    start: u64,
    end: u64,
    left: u32,
}

#[derive(Default)]
struct State {
    /// Every `read_at` call, in order, as `(offset, buf.len())`.
    reads: Vec<(u64, usize)>,
    /// `read_at` calls so far (not reset by `clear_reads`).
    calls: u64,
    flaky: Vec<Flaky>,
    /// A read reached `gone_at` (or `gone_after` passed): every read fails until `revive`.
    gone: bool,
    /// `revive` was called: the vanishing triggers are off.
    revived: bool,
}

/// An in-memory [`Source`] that simulates a failing device. `Send + Sync`; the read log and the flaky counters live
/// behind a mutex.
pub struct SimSource {
    data: Data,
    /// Ranges `[start, end)` a read touching fails: `Read { offset: first bad byte in the read }`.
    bad: Vec<(u64, u64)>,
    /// Ranges `[start, end)` and the delay of each read call touching them.
    slow: Vec<(u64, u64, Duration)>,
    gone_at: Option<u64>,
    gone_after: Option<u64>,
    /// After this many reads, reads return short past the new length (`len()` unchanged).
    shrink: Option<(u64, u64)>,
    disk: Option<DiskId>,
    state: Mutex<State>,
}

impl SimSource {
    pub fn new(data: Vec<u8>) -> SimSource {
        Self::with(Data::Bytes(data))
    }

    /// `len` bytes of zeros, generated on read (no `Vec` of that size).
    pub fn zeros(len: u64) -> SimSource {
        Self::with(Data::Zeros(len))
    }

    fn with(data: Data) -> SimSource {
        SimSource {
            data,
            bad: Vec::new(),
            slow: Vec::new(),
            gone_at: None,
            gone_after: None,
            shrink: None,
            disk: None,
            state: Mutex::new(State::default()),
        }
    }

    /// A read touching `[start, start + len)` fails with `Read { offset: its first bad byte }`.
    pub fn bad(mut self, start: u64, len: u64) -> Self {
        self.bad.push((start, start + len));
        self
    }

    /// The first `failures` reads touching `[start, start + len)` fail like [`SimSource::bad`], then they succeed.
    pub fn flaky(self, start: u64, len: u64, failures: u32) -> Self {
        self.lock().flaky.push(Flaky {
            start,
            end: start + len,
            left: failures,
        });
        self
    }

    /// Each read call touching `[start, start + len)` sleeps `per_read` first.
    pub fn slow(mut self, start: u64, len: u64, per_read: Duration) -> Self {
        self.slow.push((start, start + len, per_read));
        self
    }

    /// The first read reaching `offset` and every later read fail with `DeviceGone { offset: the read's offset }`.
    pub fn gone_at(mut self, offset: u64) -> Self {
        self.gone_at = Some(offset);
        self
    }

    /// Read call number `reads + 1` and every later one fail with `DeviceGone`.
    pub fn gone_after(mut self, reads: u64) -> Self {
        self.gone_after = Some(reads);
        self
    }

    /// After `reads` read calls, reads return short past `new_len` (0 bytes at or past it); `len()` is unchanged.
    pub fn shrink_after(mut self, reads: u64, new_len: u64) -> Self {
        self.shrink = Some((reads, new_len));
        self
    }

    /// The source reads the whole disk `id` (what [`Source::disk`] answers).
    pub fn disk(mut self, id: &str) -> Self {
        self.disk = Some(DiskId::parse(id).expect("a whole-disk id"));
        self
    }

    /// The device is back: reads succeed again and the vanishing triggers (`gone_at`, `gone_after`, `shrink_after`) are
    /// off from now on.
    pub fn revive(&self) {
        let mut st = self.lock();
        st.gone = false;
        st.revived = true;
    }

    /// Every `read_at` call so far (since the last `clear_reads`), in order, as `(offset, buf.len())`.
    pub fn reads(&self) -> Vec<(u64, usize)> {
        self.lock().reads.clone()
    }

    pub fn clear_reads(&self) {
        self.lock().reads.clear();
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn touches(offset: u64, end: u64, (s, e): (u64, u64)) -> bool {
    offset < e && end > s
}

impl Source for SimSource {
    fn len(&self) -> u64 {
        match &self.data {
            Data::Bytes(b) => b.len() as u64,
            Data::Zeros(len) => *len,
        }
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, EngineError> {
        let end = offset.saturating_add(buf.len() as u64);
        let mut len = self.len();
        let delay: Duration = self
            .slow
            .iter()
            .filter(|&&(s, e, _)| touches(offset, end, (s, e)))
            .map(|&(_, _, d)| d)
            .sum();
        {
            let mut st = self.lock();
            st.reads.push((offset, buf.len()));
            st.calls += 1;
            if !st.revived {
                if self.gone_after.is_some_and(|n| st.calls > n)
                    || self.gone_at.is_some_and(|at| end > at)
                {
                    st.gone = true;
                }
                if let Some((after, new_len)) = self.shrink
                    && st.calls > after
                {
                    len = len.min(new_len);
                }
            }
            if st.gone {
                return Err(EngineError::DeviceGone { offset });
            }
        }
        if !delay.is_zero() {
            std::thread::sleep(delay);
        }
        let mut failed = self
            .bad
            .iter()
            .filter(|&&r| touches(offset, end, r))
            .map(|&(s, _)| s.max(offset))
            .min();
        {
            let mut st = self.lock();
            for f in st.flaky.iter_mut() {
                if f.left > 0 && touches(offset, end, (f.start, f.end)) {
                    f.left -= 1;
                    let at = f.start.max(offset);
                    failed = Some(failed.map_or(at, |b| b.min(at)));
                }
            }
        }
        if let Some(at) = failed {
            return Err(EngineError::Read {
                offset: at,
                cause: std::io::Error::other("simulated bad sector"),
            });
        }
        if offset >= len {
            return Ok(0);
        }
        let n = (len - offset).min(buf.len() as u64) as usize;
        match &self.data {
            Data::Bytes(b) => buf[..n].copy_from_slice(&b[offset as usize..offset as usize + n]),
            Data::Zeros(_) => buf[..n].fill(0),
        }
        Ok(n)
    }

    fn volume(&self) -> Option<VolumeId> {
        None
    }

    fn disk(&self) -> Option<DiskId> {
        self.disk.clone()
    }
}

/// An in-memory [`RawDevice`] with sectors of `logical` bytes. It panics on any read whose offset or length is not a whole
/// number of logical sectors (what a real device refuses), can make ranges unreadable and vanish after some reads, and logs
/// every read as `(offset, len)`.
pub struct MemDevice {
    data: Vec<u8>,
    geometry: Geometry,
    /// Ranges `[start, end)` a read touching fails with `Read { offset: the read's offset }`.
    bad: Vec<(u64, u64)>,
    gone_after: Option<u64>,
    reads: Mutex<Vec<(u64, usize)>>,
}

impl MemDevice {
    /// `data.len()` must be a multiple of `logical`; the physical sector equals the logical one.
    pub fn new(data: Vec<u8>, logical: u32) -> MemDevice {
        assert!(data.len().is_multiple_of(logical as usize));
        MemDevice {
            geometry: Geometry {
                size: data.len() as u64,
                logical_sector: logical,
                physical_sector: logical,
            },
            data,
            bad: Vec::new(),
            gone_after: None,
            reads: Mutex::new(Vec::new()),
        }
    }

    /// Reports physical sectors of `physical` bytes (a 512e disk: logical 512, physical 4096).
    pub fn physical(mut self, physical: u32) -> Self {
        self.geometry.physical_sector = physical;
        self
    }

    /// A read touching `[start, start + len)` fails.
    pub fn bad(mut self, start: u64, len: u64) -> Self {
        self.bad.push((start, start + len));
        self
    }

    /// Read number `reads + 1` and every later one fail with `DeviceGone`.
    pub fn gone_after(mut self, reads: u64) -> Self {
        self.gone_after = Some(reads);
        self
    }

    /// Every read so far, in order, as `(offset, len)`.
    pub fn reads(&self) -> Vec<(u64, usize)> {
        self.reads
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl RawDevice for MemDevice {
    fn geometry(&self) -> Geometry {
        self.geometry
    }

    fn read_aligned(&self, offset: u64, buf: &mut [u8]) -> Result<usize, EngineError> {
        let sector = u64::from(self.geometry.logical_sector);
        assert!(
            offset.is_multiple_of(sector) && (buf.len() as u64).is_multiple_of(sector),
            "misaligned device read: offset {offset}, len {}",
            buf.len()
        );
        let calls = {
            let mut reads = self.reads.lock().unwrap_or_else(PoisonError::into_inner);
            reads.push((offset, buf.len()));
            reads.len() as u64
        };
        if self.gone_after.is_some_and(|n| calls > n) {
            return Err(EngineError::DeviceGone { offset });
        }
        let end = offset + buf.len() as u64;
        if self.bad.iter().any(|&r| touches(offset, end, r)) {
            return Err(EngineError::Read {
                offset,
                cause: std::io::Error::other("simulated bad sector"),
            });
        }
        let size = self.data.len() as u64;
        if offset >= size {
            return Ok(0);
        }
        let n = (size - offset).min(buf.len() as u64) as usize;
        buf[..n].copy_from_slice(&self.data[offset as usize..offset as usize + n]);
        Ok(n)
    }
}

/// The [`EngineError`] with code `code` (the ones a fake system can answer); anything else is `open_failed`.
pub fn error_of(code: &str) -> EngineError {
    match code {
        "system_tool_failed" => EngineError::SystemTool { tool: "/fake/tool" },
        "unsupported_platform" => EngineError::Unsupported,
        "permission_denied" => EngineError::PermissionDenied,
        "device_not_found" => EngineError::DeviceNotFound,
        "not_whole_disk" => EngineError::NotWholeDisk,
        "device_gone" => EngineError::DeviceGone { offset: 0 },
        "cancelled" => EngineError::Cancelled,
        "unmount_failed" => EngineError::UnmountFailed {
            volumes: Vec::new(),
        },
        "remount_failed" => EngineError::RemountFailed,
        _ => EngineError::Open {
            cause: std::io::Error::other(code.to_string()),
        },
    }
}

/// One connected fake disk: its info, the bytes it holds (zeros past them up to `info.geometry.size`), whether it is
/// still connected (an open device reads `DeviceGone` once removed), whether its volumes are mounted (they are when it
/// is connected) and the volumes an unmount leaves mounted, if it is set to fail.
struct FakeDisk {
    info: DiskInfo,
    data: Arc<Vec<u8>>,
    present: Arc<AtomicBool>,
    mounted: bool,
    fail_unmount: Option<Vec<String>>,
}

#[derive(Default)]
struct FakeState {
    disks: BTreeMap<DiskId, FakeDisk>,
    destinations: Vec<(PathBuf, Result<DestInfo, &'static str>)>,
    log: Arc<Mutex<Vec<String>>>,
}

/// A [`DeviceOps`] over in-memory disks. Every method takes `&self` (state behind a mutex), so a test can connect and
/// remove disks from another thread while the code under test lists and opens them. Every call is logged: "list",
/// "open disk4", "read disk4 <offset>", "destination <dir>", "unmount disk4", "remount disk4".
#[derive(Default)]
pub struct FakeDevices {
    state: Mutex<FakeState>,
}

impl FakeDevices {
    pub fn new() -> FakeDevices {
        FakeDevices::default()
    }

    /// Connects a disk holding `data`; `info.geometry.size` is set to `data.len()`.
    pub fn add(&self, mut info: DiskInfo, data: Vec<u8>) {
        info.geometry.size = data.len() as u64;
        self.add_virtual(info, data);
    }

    /// Connects a disk of `info.geometry.size` bytes holding `data` at its start and zeros after it (nothing more is
    /// allocated: an 8 GiB disk costs only `data`).
    pub fn add_virtual(&self, info: DiskInfo, data: Vec<u8>) {
        let disk = FakeDisk {
            info: info.clone(),
            data: Arc::new(data),
            present: Arc::new(AtomicBool::new(true)),
            mounted: true,
            fail_unmount: None,
        };
        if let Some(old) = self.lock().disks.insert(info.id, disk) {
            old.present.store(false, Ordering::SeqCst);
        }
    }

    /// Disconnects a disk: it leaves the list and its open devices read `DeviceGone`.
    pub fn remove(&self, id: &str) {
        let id = DiskId::parse(id).expect("a whole-disk id");
        if let Some(old) = self.lock().disks.remove(&id) {
            old.present.store(false, Ordering::SeqCst);
        }
    }

    /// From now on, unmounting disk `id` fails with `unmount_failed` naming `volumes` (still mounted).
    pub fn fail_unmount(&self, id: &str, volumes: &[&str]) {
        let id = DiskId::parse(id).expect("a whole-disk id");
        if let Some(d) = self.lock().disks.get_mut(&id) {
            d.fail_unmount = Some(volumes.iter().map(|v| v.to_string()).collect());
        }
    }

    /// Whether disk `id` is connected with its volumes mounted (true from `add` until a successful `unmount`, and again
    /// after `remount`).
    pub fn mounted(&self, id: &str) -> bool {
        let id = DiskId::parse(id).expect("a whole-disk id");
        self.lock().disks.get(&id).is_some_and(|d| d.mounted)
    }

    /// What `destination(dir)` answers for `dir` (resolved, when it exists) and any folder inside it (the longest matching
    /// `dir` wins): the info, or the error with that code. A folder never set answers `system_tool_failed`.
    pub fn set_destination(&self, dir: &Path, answer: Result<DestInfo, &'static str>) {
        // Stored resolved, as `Destination::check` asks about resolved folders (`/var` is `/private/var` on macOS).
        let dir = &std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        let mut st = self.lock();
        st.destinations.retain(|(d, _)| d != dir);
        st.destinations.push((dir.to_path_buf(), answer));
    }

    /// Every call so far, in order.
    pub fn log(&self) -> Vec<String> {
        let log = self.lock().log.clone();
        log.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    fn lock(&self) -> MutexGuard<'_, FakeState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn note(&self, entry: String) {
        let log = self.lock().log.clone();
        log.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(entry);
    }
}

impl DeviceOps for FakeDevices {
    fn list(&self) -> Result<Vec<DiskInfo>, EngineError> {
        self.note("list".into());
        let mut disks: Vec<DiskInfo> = self.lock().disks.values().map(|d| d.info.clone()).collect();
        // Sorted by number, as `DeviceOps::list` promises (`disk2` before `disk10`).
        disks.sort_by(|a, b| {
            let (a, b) = (a.id.as_str(), b.id.as_str());
            (a.len(), a).cmp(&(b.len(), b))
        });
        Ok(disks)
    }

    fn open(&self, disk: &DiskId) -> Result<Box<dyn RawDevice>, EngineError> {
        self.note(format!("open {}", disk.as_str()));
        let st = self.lock();
        let d = st.disks.get(disk).ok_or(EngineError::DeviceNotFound)?;
        Ok(Box::new(FakeDevice {
            id: disk.clone(),
            geometry: d.info.geometry,
            data: d.data.clone(),
            present: d.present.clone(),
            log: st.log.clone(),
        }))
    }

    fn destination(&self, dir: &Path) -> Result<DestInfo, EngineError> {
        self.note(format!("destination {}", dir.display()));
        let st = self.lock();
        let answer = st
            .destinations
            .iter()
            .filter(|(d, _)| dir.starts_with(d))
            .max_by_key(|(d, _)| d.as_os_str().len())
            .map(|(_, a)| a.clone());
        match answer {
            Some(Ok(info)) => Ok(info),
            Some(Err(code)) => Err(error_of(code)),
            None => Err(error_of("system_tool_failed")),
        }
    }

    fn unmount(&self, disk: &DiskId) -> Result<UnmountGuard, EngineError> {
        self.note(format!("unmount {}", disk.as_str()));
        let mut st = self.lock();
        let d = st.disks.get_mut(disk).ok_or(EngineError::DeviceNotFound)?;
        if let Some(volumes) = &d.fail_unmount {
            return Err(EngineError::UnmountFailed {
                volumes: volumes.clone(),
            });
        }
        d.mounted = false;
        Ok(UnmountGuard::empty())
    }

    fn remount(&self, disk: &DiskId, guard: Option<UnmountGuard>) -> Result<(), EngineError> {
        drop(guard);
        self.note(format!("remount {}", disk.as_str()));
        let mut st = self.lock();
        let d = st.disks.get_mut(disk).ok_or(EngineError::RemountFailed)?;
        d.mounted = true;
        Ok(())
    }
}

/// An open fake disk: aligned reads only (it panics on others, as [`MemDevice`]), zeros past its data, `DeviceGone`
/// once removed.
struct FakeDevice {
    id: DiskId,
    geometry: Geometry,
    data: Arc<Vec<u8>>,
    present: Arc<AtomicBool>,
    log: Arc<Mutex<Vec<String>>>,
}

impl RawDevice for FakeDevice {
    fn geometry(&self) -> Geometry {
        self.geometry
    }

    fn read_aligned(&self, offset: u64, buf: &mut [u8]) -> Result<usize, EngineError> {
        let sector = u64::from(self.geometry.logical_sector);
        assert!(
            offset.is_multiple_of(sector) && (buf.len() as u64).is_multiple_of(sector),
            "misaligned device read: offset {offset}, len {}",
            buf.len()
        );
        self.log
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(format!("read {} {offset}", self.id.as_str()));
        if !self.present.load(Ordering::SeqCst) {
            return Err(EngineError::DeviceGone { offset });
        }
        let size = self.geometry.size;
        if offset >= size {
            return Ok(0);
        }
        let n = (size - offset).min(buf.len() as u64) as usize;
        let have = (self.data.len() as u64)
            .saturating_sub(offset)
            .min(n as u64) as usize;
        if have > 0 {
            buf[..have].copy_from_slice(&self.data[offset as usize..offset as usize + have]);
        }
        buf[have..n].fill(0);
        Ok(n)
    }
}

/// A [`DiskInfo`] for a fake disk `id` of `size` bytes with 512-byte sectors: removable, external, nothing else set.
pub fn disk_info(id: &str, size: u64) -> DiskInfo {
    DiskInfo {
        id: DiskId::parse(id).expect("a whole-disk id"),
        geometry: Geometry {
            size,
            logical_sector: 512,
            physical_sector: 512,
        },
        vendor: None,
        model: Some("Fake Card".into()),
        serial: None,
        removable: true,
        internal: false,
        system: false,
        virtual_disk: false,
        solid_state: None,
        volumes: Vec::new(),
    }
}
