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
    let dev = partition_device(&att)?.replacen("/dev/disk", "/dev/rdisk", 1);
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

/// The last device `hdiutil info` lists for the attached image (its partition; the whole disk of a table-less image).
/// The plist printed by a `-nomount` attach can list the whole disk only, so the slices are looked up again, briefly
/// retrying while they are probed.
fn partition_device(att: &Attached) -> Option<String> {
    let mut best = att.devices.last().cloned();
    for _ in 0..10 {
        let found = info().and_then(|all| {
            all.into_iter()
                .find(|(_, devs)| devs.contains(&att.disk))
                .and_then(|(_, devs)| devs.last().cloned())
        });
        if found.is_some() {
            best = found;
        }
        if best.as_ref() != Some(&att.disk) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
    best
}

/// One attached image; detaches in `Drop`.
struct Attached {
    /// The whole-disk device `hdiutil attach` printed first, confirmed by `hdiutil info` to belong to our image (the
    /// only thing ever detached).
    disk: String,
    /// Every device printed, in order.
    devices: Vec<String>,
    mount: Option<PathBuf>,
}

impl Attached {
    fn new(image: &Path, readonly: bool, mount: bool, extra: &[&str]) -> Attached {
        let image = checked_image(image);
        let mut cmd = Command::new(HDIUTIL);
        cmd.arg("attach")
            .args(["-plist", "-nobrowse", "-noverify", "-noautofsck"]);
        if readonly {
            cmd.arg("-readonly");
        }
        if !mount {
            cmd.arg("-nomount");
        }
        cmd.args(extra).arg(&image);
        let out = cmd.output().expect("hdiutil attach did not start");
        assert!(
            out.status.success(),
            "hdiutil attach failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        // From here on the image is attached: any failure detaches it before panicking.
        let parsed = parse_attach(&String::from_utf8_lossy(&out.stdout));
        let Some((devices, mount_point)) = parsed else {
            detach_by_path(&image);
            panic!(
                "could not read the plist printed by hdiutil attach (detached {})",
                image.display()
            );
        };
        let disk = devices[0].clone();
        if !info_confirms(&image, &disk) {
            detach_by_path(&image);
            panic!(
                "hdiutil info does not show {disk} as {} (detached it by image path)",
                image.display()
            );
        }
        Attached {
            disk,
            devices,
            mount: mount_point,
        }
    }
}

/// The canonical path of `image`, which must be a file under the (canonical, non-system) temp directory.
fn checked_image(image: &Path) -> PathBuf {
    let tmp = std::env::temp_dir()
        .canonicalize()
        .expect("the temp dir must exist");
    assert_tmp_ok(&tmp);
    let image = image
        .canonicalize()
        .unwrap_or_else(|e| panic!("image {} unreadable: {e}", image.display()));
    assert!(
        image.is_file(),
        "only image files are attached: {}",
        image.display()
    );
    assert!(
        image.starts_with(&tmp),
        "the image must live in the temp dir: {}",
        image.display()
    );
    image
}

/// Refuses `/` and top-level system directories as the temp dir.
fn assert_tmp_ok(tmp: &Path) {
    let depth = tmp
        .components()
        .filter(|c| matches!(c, std::path::Component::Normal(_)))
        .count();
    let first = tmp
        .components()
        .nth(1)
        .and_then(|c| c.as_os_str().to_str().map(str::to_owned));
    let system = [
        "System",
        "usr",
        "bin",
        "sbin",
        "etc",
        "dev",
        "Volumes",
        "Library",
        "Applications",
        "opt",
    ];
    assert!(
        depth >= 2 && !first.as_deref().is_some_and(|f| system.contains(&f)),
        "refusing to use {} as the temp dir",
        tmp.display()
    );
}

/// Devices (in order, whole disk first) and the first mount point of an `hdiutil attach -plist` reply.
fn parse_attach(text: &str) -> Option<(Vec<String>, Option<PathBuf>)> {
    let root = plist::parse(text)?;
    let entities = root.get("system-entities")?.as_array()?;
    let mut devices = Vec::new();
    let mut mount = None;
    for e in entities {
        let dev = e.get("dev-entry")?.as_str()?;
        if !is_disk_device(dev) {
            return None;
        }
        devices.push(dev.to_string());
        if mount.is_none() {
            mount = e
                .get("mount-point")
                .and_then(|m| m.as_str())
                .map(PathBuf::from);
        }
    }
    if devices.is_empty() {
        None
    } else {
        Some((devices, mount))
    }
}

/// `hdiutil info -plist`: (image-path, devices) of every attached image.
fn info() -> Option<Vec<(String, Vec<String>)>> {
    let out = Command::new(HDIUTIL)
        .args(["info", "-plist"])
        .output()
        .ok()?;
    let root = plist::parse(&String::from_utf8_lossy(&out.stdout))?;
    let mut v = Vec::new();
    for img in root.get("images")?.as_array()? {
        let path = img.get("image-path").and_then(|p| p.as_str()).unwrap_or("");
        let devs = img
            .get("system-entities")
            .and_then(|e| e.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|e| e.get("dev-entry").and_then(|d| d.as_str()))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        v.push((path.to_string(), devs));
    }
    Some(v)
}

/// Whether `hdiutil info` lists `dev` under the image at `image`.
fn info_confirms(image: &Path, dev: &str) -> bool {
    info().is_some_and(|all| {
        all.iter()
            .any(|(p, devs)| same_file(p, image) && devs.iter().any(|d| d == dev))
    })
}

fn same_file(reported: &str, image: &Path) -> bool {
    Path::new(reported).canonicalize().is_ok_and(|p| p == image)
}

/// Best effort: detaches every attached image whose `image-path` is `image` (its whole-disk device is the first
/// device `hdiutil info` lists for it).
fn detach_by_path(image: &Path) {
    let Some(all) = info() else {
        eprintln!(
            "WARNING: could not read hdiutil info; {} may still be attached",
            image.display()
        );
        return;
    };
    for (p, devs) in all {
        if same_file(&p, image)
            && let Some(dev) = devs.first().filter(|d| is_disk_device(d))
        {
            detach(dev);
        }
    }
}

fn detach(dev: &str) {
    assert!(is_disk_device(dev));
    for force in [false, true] {
        let mut cmd = Command::new(HDIUTIL);
        cmd.arg("detach");
        if force {
            cmd.arg("-force");
        }
        if cmd.arg(dev).output().is_ok_and(|o| o.status.success()) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    eprintln!("WARNING: could not detach {dev}");
}

/// A minimal reader for the XML property lists `hdiutil` prints: dict, array, string; the rest reads as `Other`.
mod plist {
    pub enum Value {
        Dict(Vec<(String, Value)>),
        Array(Vec<Value>),
        Str(String),
        Other,
    }

    impl Value {
        pub fn get(&self, key: &str) -> Option<&Value> {
            match self {
                Value::Dict(d) => d.iter().find(|(k, _)| k == key).map(|(_, v)| v),
                _ => None,
            }
        }
        pub fn as_array(&self) -> Option<&[Value]> {
            match self {
                Value::Array(a) => Some(a),
                _ => None,
            }
        }
        pub fn as_str(&self) -> Option<&str> {
            match self {
                Value::Str(s) => Some(s),
                _ => None,
            }
        }
    }

    fn unescape(s: &str) -> String {
        s.replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
            .replace("&amp;", "&")
    }

    /// The root value of a plist document; `None` on anything malformed. Depth is bounded.
    pub fn parse(text: &str) -> Option<Value> {
        let body = &text[text.find("<plist")?..];
        let body = &body[body.find('>')? + 1..];
        let mut rest = body;
        let v = value(&mut rest, 0)?;
        Some(v)
    }

    /// Next tag (without the angle brackets) and the text before it.
    fn tag<'a>(rest: &mut &'a str) -> Option<(&'a str, &'a str)> {
        let open = rest.find('<')?;
        let close = rest[open..].find('>')? + open;
        let before = &rest[..open];
        let t = &rest[open + 1..close];
        *rest = &rest[close + 1..];
        Some((before, t))
    }

    fn value(rest: &mut &str, depth: u32) -> Option<Value> {
        if depth > 32 {
            return None;
        }
        let (_, t) = tag(rest)?;
        match t {
            "dict" => {
                let mut items = Vec::new();
                loop {
                    let (_, t) = tag(rest)?;
                    match t {
                        "/dict" => return Some(Value::Dict(items)),
                        "key" => {
                            let (text, end) = tag(rest)?;
                            if end != "/key" {
                                return None;
                            }
                            items.push((unescape(text), value(rest, depth + 1)?));
                        }
                        _ => return None,
                    }
                }
            }
            "array" => {
                let mut items = Vec::new();
                loop {
                    let save = *rest;
                    let (_, t) = tag(rest)?;
                    if t == "/array" {
                        return Some(Value::Array(items));
                    }
                    *rest = save;
                    items.push(value(rest, depth + 1)?);
                }
            }
            "string" => {
                let (text, end) = tag(rest)?;
                (end == "/string").then(|| Value::Str(unescape(text)))
            }
            "dict/" => Some(Value::Dict(Vec::new())),
            "array/" => Some(Value::Array(Vec::new())),
            "string/" => Some(Value::Str(String::new())),
            t if t.ends_with('/') => Some(Value::Other),
            t => {
                let (_, end) = tag(rest)?;
                (end.strip_prefix('/') == Some(t)).then_some(Value::Other)
            }
        }
    }
}

impl Drop for Attached {
    fn drop(&mut self) {
        detach(&self.disk);
    }
}

/// `/dev/diskN` (whole disk or slice).
fn is_disk_device(s: &str) -> bool {
    s.strip_prefix("/dev/disk")
        .is_some_and(|r| !r.is_empty() && r.starts_with(|c: char| c.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ATTACH: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict><key>system-entities</key><array>
<dict><key>content-hint</key><string>FDisk_partition_scheme</string><key>dev-entry</key><string>/dev/disk4</string><key>potentially-mountable</key><false/></dict>
<dict><key>dev-entry</key><string>/dev/disk4s1</string><key>mount-point</key><string>/Volumes/RVT &amp; 1</string><key>size</key><integer>5</integer></dict>
</array></dict></plist>"#;

    #[test]
    fn attach_plist_is_read() {
        let (devs, mount) = parse_attach(ATTACH).unwrap();
        assert_eq!(devs, ["/dev/disk4", "/dev/disk4s1"]);
        assert_eq!(mount, Some(PathBuf::from("/Volumes/RVT & 1")));
    }

    #[test]
    fn bad_plists_are_refused() {
        assert!(parse_attach("").is_none());
        assert!(parse_attach(&ATTACH.replace("/dev/disk4s1", "/Volumes/x")).is_none());
        assert!(parse_attach(&ATTACH.replace("</array>", "")).is_none());
        assert!(parse_attach(&ATTACH.replace("dev-entry", "dev")).is_none());
        let deep = format!(
            "<plist>{}{}</plist>",
            "<array>".repeat(100),
            "</array>".repeat(100)
        );
        assert!(plist::parse(&deep).is_none());
    }

    #[test]
    fn system_temp_dirs_are_refused() {
        for bad in [
            "/",
            "/usr",
            "/usr/tmp",
            "/private",
            "/System/x",
            "/Volumes/Disk",
        ] {
            assert!(
                std::panic::catch_unwind(|| assert_tmp_ok(Path::new(bad))).is_err(),
                "{bad}"
            );
        }
        assert_tmp_ok(Path::new("/private/tmp"));
        assert_tmp_ok(Path::new("/private/var/folders/ab/cd/T"));
    }
}
