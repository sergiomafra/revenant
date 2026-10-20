//! Opt-in checks of the synthetic file-system builders against the real macOS drivers (`hdiutil`, `fsck_msdos`,
//! `fsck_exfat`). Nothing here runs unless the machine is macOS, `/usr/bin/hdiutil` exists and `REVENANT_OS_FS=1`.
//!
//! Safety rules (this module is the only place that talks to `hdiutil`):
//! - only image FILES inside a temp directory are ever attached, always with `-nobrowse -noverify -noautofsck`;
//! - the only device ever passed to `hdiutil detach` is one that `hdiutil attach` just printed for such an image;
//! - no `diskutil`, no `sudo`, no other disk, nothing is installed;
//! - every attach is undone by a drop guard, also on panic.

use std::path::{Path, PathBuf};
use std::process::Command;

const HDIUTIL: &str = "/usr/bin/hdiutil";
const FSCK_MSDOS: &str = "/sbin/fsck_msdos";
const FSCK_EXFAT: &str = "/sbin/fsck_exfat";

/// The file system `hdiutil create -fs` formats.
#[derive(Clone, Copy, Debug)]
pub enum OsFs {
    Fat16,
    Fat32,
    Exfat,
}

impl OsFs {
    fn name(self) -> &'static str {
        match self {
            OsFs::Fat16 => "MS-DOS FAT16",
            OsFs::Fat32 => "MS-DOS FAT32",
            OsFs::Exfat => "ExFAT",
        }
    }
}

/// The partition scheme `hdiutil create -layout` writes.
#[derive(Clone, Copy, Debug)]
pub enum OsLayout {
    Mbr,
    Gpt,
    None,
}

impl OsLayout {
    fn name(self) -> &'static str {
        match self {
            OsLayout::Mbr => "MBRSPUD",
            OsLayout::Gpt => "GPTSPUD",
            OsLayout::None => "NONE",
        }
    }
}

/// True only on macOS with `REVENANT_OS_FS=1` and `/usr/bin/hdiutil` present. Prints why when false.
pub fn available() -> bool {
    if !cfg!(target_os = "macos") {
        eprintln!("skipped: not macOS");
        return false;
    }
    if std::env::var("REVENANT_OS_FS").as_deref() != Ok("1") {
        eprintln!("skipped: set REVENANT_OS_FS=1 to check against the macOS drivers");
        return false;
    }
    if !Path::new(HDIUTIL).exists() {
        eprintln!("skipped: {HDIUTIL} not found");
        return false;
    }
    true
}

/// A disk image created by `hdiutil` inside a temp directory (deleted on drop).
pub struct OsImage {
    pub path: PathBuf,
    _dir: tempfile::TempDir,
}

impl OsImage {
    /// `hdiutil create` of a `size_mib` image formatted by the OS, volume name `RVT`. `None` when `available()` is
    /// false or the command fails.
    pub fn create(fs: OsFs, layout: OsLayout, size_mib: u32) -> Option<OsImage> {
        if !available() {
            return None;
        }
        let dir = tempfile::tempdir().ok()?;
        let path = dir.path().join("card.dmg");
        let out = Command::new(HDIUTIL)
            .args(["create", "-size", &format!("{size_mib}m"), "-fs", fs.name()])
            .args(["-volname", "RVT", "-layout", layout.name()])
            .arg(&path)
            .output()
            .ok()?;
        if !out.status.success() || !path.is_file() {
            eprintln!(
                "hdiutil create failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            return None;
        }
        Some(OsImage { path, _dir: dir })
    }

    /// Attaches read-write, runs `f` with the mount point, always detaches. Panics unless the mount point starts
    /// with `/Volumes/RVT`.
    pub fn with_mounted<R>(&self, f: impl FnOnce(&Path) -> R) -> R {
        let att = Attached::new(&self.path, false, true, &[]);
        let mount = att.mount.clone().expect("no mount point printed");
        assert!(
            mount.starts_with("/Volumes/RVT"),
            "unexpected mount point {}",
            mount.display()
        );
        f(&mount)
    }
}

/// Attaches an existing raw image file read-only, runs `f` with the mount point, detaches.
pub fn with_raw_mounted<R>(image: &Path, f: impl FnOnce(&Path) -> R) -> R {
    let att = Attached::new(
        image,
        true,
        true,
        &["-imagekey", "diskimage-class=CRawDiskImage"],
    );
    let mount = att.mount.clone().expect("no mount point printed");
    f(&mount)
}

/// `fsck_msdos -n` / `fsck_exfat -n` on the image's partition, attached but not mounted. `None` when the tool is
/// missing or cannot run without privileges (the reason is printed).
pub fn fsck(image: &Path, fs: OsFs) -> Option<bool> {
    let tool = match fs {
        OsFs::Fat16 | OsFs::Fat32 => FSCK_MSDOS,
        OsFs::Exfat => FSCK_EXFAT,
    };
    if !Path::new(tool).exists() {
        eprintln!("fsck skipped: {tool} not found");
        return None;
    }
    let att = Attached::new(
        image,
        true,
        false,
        &["-imagekey", "diskimage-class=CRawDiskImage"],
    );
    // The partition is the last device printed; a table-less image prints only the whole disk.
    let dev = att.devices.last()?.replacen("/dev/disk", "/dev/rdisk", 1);
    let out = Command::new(tool).arg("-n").arg(&dev).output().ok()?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    if text.contains("Permission denied") || text.contains("Operation not permitted") {
        eprintln!("fsck skipped: needs privileges ({})", text.trim());
        return None;
    }
    if !out.status.success() {
        eprintln!("{tool} reported problems:\n{text}");
    }
    Some(out.status.success())
}

/// One attached image; detaches in `Drop`.
struct Attached {
    /// The whole-disk device `hdiutil attach` printed first (the only thing ever detached).
    disk: String,
    /// Every device printed, in order.
    devices: Vec<String>,
    mount: Option<PathBuf>,
}

impl Attached {
    fn new(image: &Path, readonly: bool, mount: bool, extra: &[&str]) -> Attached {
        assert!(
            image.is_file(),
            "only image files are attached: {}",
            image.display()
        );
        let tmp = std::env::temp_dir();
        assert!(
            image
                .canonicalize()
                .ok()
                .is_some_and(|p| tmp.canonicalize().ok().is_some_and(|t| p.starts_with(t))),
            "the image must live in the temp dir: {}",
            image.display()
        );
        let mut cmd = Command::new(HDIUTIL);
        cmd.arg("attach")
            .args(["-nobrowse", "-noverify", "-noautofsck"]);
        if readonly {
            cmd.arg("-readonly");
        }
        if !mount {
            cmd.arg("-nomount");
        }
        cmd.args(extra).arg(image);
        let out = cmd.output().expect("hdiutil attach did not start");
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(
            out.status.success(),
            "hdiutil attach failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let mut devices = Vec::new();
        let mut mount_point = None;
        for line in text.lines() {
            let mut cols = line.split('\t').map(str::trim);
            let Some(dev) = cols.next().filter(|d| is_disk_device(d)) else {
                continue;
            };
            devices.push(dev.to_string());
            if let Some(m) = cols.find(|c| c.starts_with('/')) {
                mount_point = Some(PathBuf::from(m));
            }
        }
        let disk = devices
            .first()
            .cloned()
            .unwrap_or_else(|| panic!("hdiutil attach printed no device: {text:?}"));
        Attached {
            disk,
            devices,
            mount: mount_point,
        }
    }
}

impl Drop for Attached {
    fn drop(&mut self) {
        debug_assert!(is_disk_device(&self.disk));
        for force in [false, true] {
            let mut cmd = Command::new(HDIUTIL);
            cmd.arg("detach");
            if force {
                cmd.arg("-force");
            }
            if cmd
                .arg(&self.disk)
                .output()
                .is_ok_and(|o| o.status.success())
            {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        eprintln!("WARNING: could not detach {}", self.disk);
    }
}

/// `/dev/diskN` (whole disk or slice).
fn is_disk_device(s: &str) -> bool {
    s.strip_prefix("/dev/disk")
        .is_some_and(|r| !r.is_empty() && r.starts_with(|c: char| c.is_ascii_digit()))
}
