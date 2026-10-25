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
        "system_disk" => EngineError::SystemDisk,
        _ => EngineError::Open {
            cause: std::io::Error::other(code.to_string()),
        },
    }
}

/// Read faults of a fake disk, shared with its open devices (set after they are opened, they still apply).
#[derive(Default)]
struct Faults {
    /// Ranges `[start, end)` a read touching fails with `Read { offset: the read's offset }`.
    bad: Vec<(u64, u64)>,
    /// Offsets a read touching blocks until the [`FakeDevices`] is dropped (then fails with `Read`).
    hang: Vec<u64>,
}

/// One connected fake disk: its info, the bytes it holds (zeros past them up to `info.geometry.size`), whether it is
/// still connected (an open device reads `DeviceGone` once removed), whether its volumes are mounted (they are when it
/// is connected), the volumes an unmount leaves mounted, if it is set to fail, whether opening it is refused, and its
/// read faults.
struct FakeDisk {
    info: DiskInfo,
    data: Arc<Vec<u8>>,
    present: Arc<AtomicBool>,
    mounted: bool,
    fail_unmount: Option<Vec<String>>,
    /// Opening it fails with this code.
    fail_open: Option<&'static str>,
    faults: Arc<Mutex<Faults>>,
}

#[derive(Default)]
struct FakeState {
    disks: BTreeMap<DiskId, FakeDisk>,
    destinations: Vec<(PathBuf, Result<DestInfo, &'static str>)>,
    log: Arc<Mutex<Vec<String>>>,
    /// Listing fails with this code.
    fail_list: Option<&'static str>,
    /// Set when the `FakeDevices` is dropped: hanging reads give up.
    released: Arc<AtomicBool>,
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
            fail_open: None,
            faults: Arc::default(),
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

    /// From now on, opening disk `id` fails with `permission_denied`.
    pub fn deny_open(&self, id: &str) {
        self.fail_open(id, "permission_denied");
    }

    /// From now on, opening disk `id` fails with the error of code `code` (see [`error_of`]).
    pub fn fail_open(&self, id: &str, code: &'static str) {
        self.with_disk(id, |d| d.fail_open = Some(code));
    }

    /// From now on, listing fails with the error of code `code` (`None`: it works again).
    pub fn fail_list(&self, code: Option<&'static str>) {
        self.lock().fail_list = code;
    }

    /// From now on, a read of disk `id` touching byte `offset` never returns while this `FakeDevices` lives (a device
    /// that stopped answering); it fails with `read_failed` once the `FakeDevices` is dropped. Also on devices already
    /// open.
    pub fn hang_reads(&self, id: &str, offset: u64) {
        self.with_disk(id, |d| d.faults.lock().unwrap().hang.push(offset));
    }

    /// From now on, a read of disk `id` touching `[offset, offset + len)` fails with `read_failed` (also on devices
    /// already open).
    pub fn bad(&self, id: &str, offset: u64, len: u64) {
        self.with_disk(id, |d| {
            d.faults.lock().unwrap().bad.push((offset, offset + len))
        });
    }

    fn with_disk(&self, id: &str, f: impl FnOnce(&mut FakeDisk)) {
        let id = DiskId::parse(id).expect("a whole-disk id");
        f(self.lock().disks.get_mut(&id).expect("a connected disk"));
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
        if let Some(code) = self.lock().fail_list {
            return Err(error_of(code));
        }
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
        if let Some(code) = d.fail_open {
            return Err(error_of(code));
        }
        Ok(Box::new(FakeDevice {
            id: disk.clone(),
            geometry: d.info.geometry,
            data: d.data.clone(),
            present: d.present.clone(),
            faults: d.faults.clone(),
            released: st.released.clone(),
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
        if d.info.system {
            return Err(EngineError::SystemDisk);
        }
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

impl Drop for FakeDevices {
    fn drop(&mut self) {
        self.lock().released.store(true, Ordering::SeqCst);
    }
}

/// An open fake disk: aligned reads only (it panics on others, as [`MemDevice`]), zeros past its data, `DeviceGone`
/// once removed, and the disk's read faults.
struct FakeDevice {
    id: DiskId,
    geometry: Geometry,
    data: Arc<Vec<u8>>,
    present: Arc<AtomicBool>,
    faults: Arc<Mutex<Faults>>,
    released: Arc<AtomicBool>,
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
        let end = offset + buf.len() as u64;
        let (hangs, bad) = {
            let f = self.faults.lock().unwrap_or_else(PoisonError::into_inner);
            (
                f.hang.iter().any(|&at| offset <= at && at < end),
                f.bad.iter().any(|&r| touches(offset, end, r)),
            )
        };
        if hangs {
            while !self.released.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        if hangs || bad {
            return Err(EngineError::Read {
                offset,
                cause: std::io::Error::other("simulated bad sector"),
            });
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

/// The cards of the resumable-scan tests (Task 9), built once here and shared with the session tests (Task 10). Each
/// one is a [`SimSource`] in memory with its unreadable ranges set.
pub mod cards {
    use revenant_testkit::fs::{DeleteStyle, FsBuilder, FsType, Partitioning, disk};
    use revenant_testkit::{
        AvcOptions, AvcVideo, Mp4Options, Rw2Options, TestExif, TiffRaw, TiffRawOptions,
        TiffVariant, avc_video, jpeg, jpeg_smooth, mp4, panasonic_rw2, tiff_raw,
    };

    use super::SimSource;

    const KIB: u64 = 1 << 10;
    const MIB: u64 = 1 << 20;
    /// Where every card volume starts (MBR entry at LBA 8192).
    const VOL: u64 = 8192 * 512;
    const DIR: &str = "DCIM/100MSDCF";

    /// A card: its source and what the tests look at.
    pub struct Card {
        pub source: SimSource,
        /// The unreadable ranges `[start, end)` the source simulates, ascending.
        pub bad: Vec<(u64, u64)>,
        /// Card A: its separated `moov` box; card E: the video; as `[start, end)`.
        pub file: Option<(u64, u64)>,
        /// Cards B and C: where the volume's data clusters start (cluster 2); 0 for the others.
        pub data_start: u64,
    }

    fn card(bytes: Vec<u8>, bad: Vec<(u64, u64)>) -> Card {
        let mut source = SimSource::new(bytes);
        for &(s, e) in &bad {
            source = source.bad(s, e - s);
        }
        Card {
            source,
            bad,
            file: None,
            data_start: 0,
        }
    }

    /// Deterministic noise (SplitMix64).
    pub fn noise(len: u64, seed: u64) -> Vec<u8> {
        let mut state = seed;
        let mut out = Vec::with_capacity(len as usize + 8);
        while (out.len() as u64) < len {
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^= z >> 31;
            out.extend_from_slice(&z.to_le_bytes());
        }
        out.truncate(len as usize);
        out
    }

    fn put(img: &mut [u8], at: u64, data: &[u8]) {
        img[at as usize..at as usize + data.len()].copy_from_slice(data);
    }

    /// The pairing tests' camera video (`moov` at the end, small NAL units), its `mdat` padded so that its `moov` starts
    /// 8 bytes before a sector boundary (504 modulo 512).
    fn split_video() -> AvcVideo {
        let with = |mdat_pad| {
            avc_video(&AvcOptions {
                samples: 120,
                seed: 1,
                mov: false,
                mdat_pad,
                small_nals: true,
            })
        };
        let r = with(0).moov.start % 512;
        let v = with((504 + 512 - r) % 512);
        assert_eq!(v.moov.start % 512, 504);
        v
    }

    /// Where A's video starts. Its second part (the rest of its `mdat`, then its `moov`) starts about 6 MiB after the
    /// first part ends, placed so that the `moov` header straddles a 1 MiB block boundary (8 bytes on each side).
    pub const A_VIDEO: u64 = 20 * MIB;

    /// A: 48 MiB of noise, 8 JPEGs (two across a 1 MiB block boundary), 3 unreadable runs (one inside a JPEG, one
    /// between the video's two parts, one in the noise) and, unless `video` is false, a video whose `moov` lies about
    /// 6 MiB after its first part (paired after the scan); `file` is then the `moov` box.
    pub fn a_with(video: bool) -> Card {
        let mut img = noise(48 * MIB, 41);
        let photos: [(u64, Vec<u8>); 8] = [
            (64 * KIB, jpeg(64, 64, 1, 0)),
            (MIB - 4 * KIB, jpeg_smooth(512, 384, 2)),
            (3 * MIB + 512, jpeg(96, 64, 3, 2000)),
            (7 * MIB - 8 * KIB, jpeg_smooth(640, 480, 4)),
            (12 * MIB, jpeg(64, 96, 5, 0)),
            (30 * MIB, jpeg_smooth(800, 600, 6)),
            (33 * MIB + 4096, jpeg(64, 64, 7, 4000)),
            (45 * MIB - 512, jpeg(128, 128, 8, 0)),
        ];
        for (at, data) in &photos {
            put(&mut img, *at, data);
        }
        let mut bad = vec![
            (30 * MIB + 8 * KIB, 30 * MIB + 8 * KIB + 1024),
            (40 * MIB + 512, 40 * MIB + 1024),
        ];
        let mut moov = None;
        if video {
            let v = split_video();
            let p = &v.mdat_payload;
            let cut = ((p.start + p.len() * 2 / 5) as u64).div_ceil(512) * 512;
            put(&mut img, A_VIDEO, &v.bytes[..cut as usize]);
            let to_moov = v.moov.start as u64 - cut;
            let boundary = (A_VIDEO + cut + 6 * MIB + to_moov + 8).next_multiple_of(MIB);
            let rest = boundary - 8 - to_moov;
            assert_eq!(rest % 512, 0);
            put(&mut img, rest, &v.bytes[cut as usize..]);
            moov = Some((boundary - 8, boundary - 8 + v.moov.len() as u64));
        }
        bad.push((A_VIDEO + 3 * MIB, A_VIDEO + 3 * MIB + 512));
        bad.sort_unstable();
        let mut c = card(img, bad);
        c.file = moov;
        c
    }

    /// Card A with its video (see [`a_with`]).
    pub fn a() -> Card {
        a_with(true)
    }

    /// Card A with one more unreadable sector, inside the video's first part (8 KiB in): the pairing, after the scan,
    /// reads around it, far below the last positions (it may then fail to pair; the events are what they are).
    pub fn a_bad_in_video() -> Card {
        let mut c = a();
        let at = A_VIDEO + 8 * KIB;
        c.source = c.source.bad(at, 512);
        c.bad.push((at, at + 512));
        c.bad.sort_unstable();
        c
    }

    /// FAT32, 48 MiB, 512-byte clusters.
    fn fat32() -> FsBuilder {
        FsBuilder::new(FsType::Fat32, 48 * MIB, 512)
    }

    /// `v` at LBA 8192 of a disk 8 MiB longer than it.
    fn placed(v: &[u8]) -> Vec<u8> {
        disk(
            Partitioning::Mbr { first_lba: 8192 },
            v,
            v.len() + 8 * MIB as usize,
        )
    }

    fn at(b: &FsBuilder, c: u32) -> u64 {
        VOL + b.cluster_offset(c)
    }

    fn arw_with_preview() -> TiffRaw {
        tiff_raw(&TiffRawOptions {
            variant: TiffVariant::Arw,
            big_endian: false,
            raw_len: 300_000,
            seed: 7,
            preview: Some((320, 240)),
            preview_sector_aligned: true,
            exif: TestExif::default(),
        })
    }

    fn avc(samples: usize) -> AvcVideo {
        avc_video(&AvcOptions {
            samples,
            seed: 5,
            mov: false,
            mdat_pad: 0,
            small_nals: false,
        })
    }

    /// `data` written in 2 runs of clusters 4 MiB apart, from 8 MiB into the volume's heap.
    fn two_runs(b: &mut FsBuilder, path: &str, data: &[u8]) {
        let cluster = 512;
        let n = (data.len() as u64).div_ceil(cluster) as u32;
        let first = (8 * MIB / cluster) as u32 + 2;
        let gap = (4 * MIB / cluster) as u32;
        let h = n / 2;
        let runs: Vec<u32> = (first..first + h)
            .chain(first + h + gap..first + gap + n)
            .collect();
        b.add_at(path, data, &runs);
    }

    /// B: the 1.4 FAT32 card of `fs_events_identical_across_threads_and_caches`: live files (an ARW, a JPEG, a video in
    /// 2 runs, a JPEG in 2 runs around another one), a deleted JPEG and a deleted ARW, 2 unreadable sectors (one inside
    /// a live JPEG, one in the first FAT).
    pub fn b() -> Card {
        let mut b = fat32();
        b.add(&format!("{DIR}/DSC01234.ARW"), &arw_with_preview().bytes);
        b.add(&format!("{DIR}/DSC01235.JPG"), &jpeg(64, 64, 11, 2000));
        two_runs(&mut b, &format!("{DIR}/C0001.MP4"), &avc(60).bytes);
        let (j36, j37) = (jpeg_smooth(1024, 768, 1), jpeg_smooth(1024, 768, 2));
        let need = |len: usize| (len as u64).div_ceil(512) as u32;
        let s = (16 * MIB / 512) as u32 + 2;
        let (n36, n37) = (need(j36.len()), need(j37.len()));
        let h36 = n36 / 2;
        let c37: Vec<u32> = (s + h36..s + h36 + n37).collect();
        let c36: Vec<u32> = (s..s + h36).chain(s + h36 + n37..s + n37 + n36).collect();
        b.add_at(&format!("{DIR}/DSC01236.JPG"), &j36, &c36);
        b.add_at(&format!("{DIR}/DSC01237.JPG"), &j37, &c37);
        b.add(&format!("{DIR}/DSC00100.JPG"), &jpeg(64, 64, 31, 0));
        b.add(&format!("{DIR}/DSC00101.ARW"), &arw_with_preview().bytes);
        b.delete(&format!("{DIR}/DSC00100.JPG"), DeleteStyle::Camera);
        b.delete(&format!("{DIR}/DSC00101.ARW"), DeleteStyle::Camera);
        let img = placed(&b.build());
        let jpg = at(&b, b.clusters(&format!("{DIR}/DSC01235.JPG"))[0]) + 1024;
        // A sector of the first FAT (FAT32: 32 reserved sectors).
        let fat = VOL + 32 * 512 + 3 * 512;
        let mut c = card(img, vec![(fat, fat + 512), (jpg, jpg + 512)]);
        c.data_start = at(&b, 2);
        c
    }

    /// C: a quick-formatted FAT32 card (old folders, ruling D11) holding a JPEG and a video written in 2 runs 4 MiB
    /// apart (its `moov` in the second: paired after the scan), with one unreadable sector in the free space between the
    /// runs.
    pub fn c() -> Card {
        let mut b = fat32();
        b.add(&format!("{DIR}/DSC01234.JPG"), &jpeg(64, 64, 11, 2000));
        two_runs(&mut b, &format!("{DIR}/C0001.MP4"), &avc(60).bytes);
        let img = placed(&b.build_quick_formatted());
        let gap = at(&b, (10 * MIB / 512) as u32 + 2);
        let mut c = card(img, vec![(gap, gap + 512)]);
        c.data_start = at(&b, 2);
        c
    }

    /// D: 4 MiB of noise and a `Partial` Panasonic RW2 starting 512 bytes before 1 MiB, so its JpgFromRaw (an own file,
    /// ruling D12) starts at the 1 MiB block boundary; a foreign JPEG inside its raw data is still a finding.
    pub fn d() -> Card {
        let f = panasonic_rw2(&Rw2Options {
            width: 280,
            height: 200,
            raw_format: 5,
            bits: 12,
            raw_len: 98_304,
            strip_len: 0,
            seed: 3,
            exif: TestExif::default(),
        });
        let mut img = noise(4 * MIB, 43);
        let start = MIB - 512;
        put(&mut img, start, &f.bytes);
        let foreign = start + (f.raw.start as u64 + 8192).next_multiple_of(512);
        put(&mut img, foreign, &jpeg(32, 32, 5, 0));
        card(img, Vec::new())
    }

    /// E: 48 MiB of noise and a 40 MiB contiguous MP4 (index at the end) from 2 MiB + 512 bytes, across many blocks.
    pub fn e() -> Card {
        let video = mp4(&Mp4Options {
            mdat_len: 40 * MIB as usize,
            moov_at_end: true,
            mov: false,
            mdat_64bit: false,
        });
        let mut img = noise(48 * MIB, 44);
        let start = 2 * MIB + 512;
        put(&mut img, start, &video);
        let mut c = card(img, Vec::new());
        c.file = Some((start, start + video.len() as u64));
        c
    }

    /// 16 MiB of noise with 20 isolated unreadable sectors 256 KiB apart from 1 MiB on (`DiskFailing` fires at a block
    /// boundary), then a run across the 8 MiB boundary (`[8 MiB - 1 KiB, 8 MiB + 1 KiB)`) and one ending at the 10 MiB
    /// boundary: at both boundaries an unreadable run is still open.
    pub fn failing() -> Card {
        let mut bad: Vec<(u64, u64)> = (0..20)
            .map(|i| (MIB + i * 256 * KIB, MIB + i * 256 * KIB + 512))
            .collect();
        bad.push((8 * MIB - KIB, 8 * MIB + KIB));
        bad.push((10 * MIB - 512, 10 * MIB));
        card(noise(16 * MIB, 45), bad)
    }
}
