//! macOS system tools: run by absolute path, output bounded, time bounded, any failure `system_tool_failed` (never a
//! panic).

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::plist::{self, Plist};
use super::{DestInfo, DiskInfo, macos_parse};
use crate::EngineError;
use crate::source::{DiskId, Geometry};

pub(crate) const DISKUTIL: &str = "/usr/sbin/diskutil";
pub(crate) const IOREG: &str = "/usr/sbin/ioreg";
pub(crate) const DF: &str = "/bin/df";

/// Most bytes of output read from a tool; more is a failure.
const MAX_OUTPUT: u64 = 16 << 20;
/// Longest a tool may run: a tool stuck on a dying disk must not hang a listing or an open.
pub(crate) const TOOL_TIMEOUT: Duration = Duration::from_secs(30);
/// How often a tool that closed its output is checked for having exited.
const EXIT_POLL: Duration = Duration::from_millis(10);

/// Runs `tool` (an absolute path) with `args` and returns its standard output, within [`TOOL_TIMEOUT`].
pub(crate) fn run(tool: &'static str, args: &[&str]) -> Result<String, EngineError> {
    run_within(tool, args, TOOL_TIMEOUT)
}

/// Runs `tool` with `args` and returns its standard output. Fails with `SystemTool { tool }` when it cannot start, exits
/// unsuccessfully, prints more than 16 MiB or something that is not UTF-8, or has not finished within `limit` (it is
/// killed then).
pub(crate) fn run_within(
    tool: &'static str,
    args: &[&str],
    limit: Duration,
) -> Result<String, EngineError> {
    let failed = || EngineError::SystemTool { tool };
    let deadline = Instant::now() + limit;
    let mut child = Command::new(tool)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| failed())?;
    let Some(stdout) = child.stdout.take() else {
        stop(&mut child);
        return Err(failed());
    };
    // The output is read on its own thread so the wait for it can time out; after a kill the pipe closes and the thread
    // ends by itself.
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut out = Vec::new();
        let read = stdout.take(MAX_OUTPUT + 1).read_to_end(&mut out);
        let _ = tx.send(read.map(|_| out));
    });
    let out = match rx.recv_timeout(limit) {
        Ok(Ok(out)) if out.len() as u64 <= MAX_OUTPUT => out,
        _ => {
            stop(&mut child);
            return Err(failed());
        }
    };
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(EXIT_POLL),
            _ => {
                stop(&mut child);
                return Err(failed());
            }
        }
    }
    String::from_utf8(out).map_err(|_| failed())
}

/// Kills a tool whose result no longer matters and reaps it.
fn stop(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn plist_of(tool: &'static str, args: &[&str]) -> Result<Plist, EngineError> {
    plist::parse(&run(tool, args)?).ok_or(EngineError::SystemTool { tool })
}

fn info(target: &str) -> Result<Plist, EngineError> {
    plist_of(DISKUTIL, &["info", "-plist", target])
}

fn ioreg() -> Result<Plist, EngineError> {
    plist_of(IOREG, &["-a", "-r", "-c", "IOBlockStorageDevice", "-l"])
}

/// The geometry of the whole disk `bsd` ("disk4"), from `diskutil info -plist` and `ioreg` (ruling D5), with the
/// `diskutil info` it was read from (for [`confirm`]).
pub(crate) fn geometry(bsd: &str) -> Result<(Geometry, Plist), EngineError> {
    let info = info(bsd)?;
    let ioreg = ioreg()?;
    let geometry = macos_parse::geometry(&info, Some(&ioreg), bsd)
        .ok_or(EngineError::SystemTool { tool: DISKUTIL })?;
    Ok((geometry, info))
}

/// After opening `bsd`: asks `diskutil` again and fails with `device_not_found` unless it is still the medium `before`
/// described (a card swapped between the geometry query and the open would otherwise be read with the old geometry).
pub(crate) fn confirm(bsd: &str, before: &Plist) -> Result<(), EngineError> {
    let now = info(bsd)?;
    if macos_parse::same_media(before, &now) {
        Ok(())
    } else {
        Err(EngineError::DeviceNotFound)
    }
}

/// The connected whole disks (ruling D8): `diskutil list -plist`, `diskutil info -plist` of each whole disk and its
/// partitions, `diskutil info -plist /`, one `ioreg`. A disk or partition whose info fails (it left meanwhile) is left
/// out; the list, the root info (or one naming no disk) and `ioreg` failing fail the listing.
pub(crate) fn list() -> Result<Vec<DiskInfo>, EngineError> {
    let list = plist_of(DISKUTIL, &["list", "-plist"])?;
    let mut infos = BTreeMap::new();
    for entry in macos_parse::whole_disks(&list) {
        let Some(bsd) = entry.get("DeviceIdentifier").and_then(Plist::as_str) else {
            continue;
        };
        let ids = std::iter::once(bsd).chain(macos_parse::partition_ids(entry));
        for id in ids.filter(|id| macos_parse::is_bsd_name(id)) {
            if let Ok(i) = info(id) {
                infos.insert(id.to_string(), i);
            }
        }
    }
    let root = info("/")?;
    let ioreg = ioreg()?;
    // A root info naming no disk fails the listing rather than show no system disk.
    macos_parse::disks(&list, &infos, &root, Some(&ioreg))
        .ok_or(EngineError::SystemTool { tool: DISKUTIL })
}

/// Where `dir` lives (ruling D12): `df -P -k` names the volume's node and free space, `diskutil info -plist <node>` its
/// disks and file system. A folder not on a `/dev` node (a network share) fails.
pub(crate) fn destination(dir: &Path) -> Result<DestInfo, EngineError> {
    let dir = dir.to_str().ok_or(EngineError::SystemTool { tool: DF })?;
    let df = run(DF, &["-P", "-k", "--", dir])?;
    let (node, _) = macos_parse::df_line(&df).ok_or(EngineError::SystemTool { tool: DF })?;
    if !node
        .strip_prefix("/dev/")
        .is_some_and(macos_parse::is_bsd_name)
    {
        return Err(EngineError::SystemTool { tool: DF });
    }
    let info = info(&node)?;
    macos_parse::destination(&df, &info).ok_or(EngineError::SystemTool { tool: DISKUTIL })
}

/// Unmounts every volume of `disk` without ejecting it (ruling D10): `diskutil unmountDisk` (never `force`; it also
/// unmounts the volumes of an APFS container on the disk, and succeeds when nothing is mounted), then a listing to
/// confirm. A disk no longer listed is `device_not_found`; the tool failing, or a volume of the disk still mounted, is
/// `unmount_failed` with the volumes the listing shows mounted (an APFS container's volumes are not among them, so they
/// may be empty).
pub(crate) fn unmount(disk: &DiskId) -> Result<(), EngineError> {
    let ok = run(DISKUTIL, &["unmountDisk", disk.as_str()]).is_ok();
    let after = list()?;
    if !after.iter().any(|d| d.id == *disk) {
        return Err(EngineError::DeviceNotFound);
    }
    macos_parse::unmount_result(ok, &after, disk)
        .map_err(|volumes| EngineError::UnmountFailed { volumes })
}

/// Mounts the volumes of `disk` again (`diskutil mountDisk`); any failure is `remount_failed`.
pub(crate) fn remount(disk: &DiskId) -> Result<(), EngineError> {
    run(DISKUTIL, &["mountDisk", disk.as_str()])
        .map(drop)
        .map_err(|_| EngineError::RemountFailed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_failures_are_system_tool_failed() {
        let code = |r: Result<String, EngineError>| r.unwrap_err().code();
        assert_eq!(code(run("/usr/bin/false", &[])), "system_tool_failed");
        assert_eq!(code(run("/nonexistent/tool", &[])), "system_tool_failed");
        assert_eq!(run("/bin/echo", &["hi"]).unwrap(), "hi\n");
    }

    #[test]
    fn a_hung_tool_is_stopped_in_time() {
        let limit = Duration::from_secs(1);
        // Still printing nothing after the limit.
        let t = Instant::now();
        let r = run_within("/bin/sleep", &["20"], limit);
        assert_eq!(r.unwrap_err().code(), "system_tool_failed");
        assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
        // Output closed, but the tool never exits.
        let t = Instant::now();
        let r = run_within(
            "/bin/sh",
            &["-c", "echo hi; exec 1>&-; exec /bin/sleep 20"],
            limit,
        );
        assert_eq!(r.unwrap_err().code(), "system_tool_failed");
        assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
        // A quick tool is unaffected (with the real limit: a loaded machine may take a while to start it).
        assert_eq!(
            run_within("/bin/echo", &["ok"], TOOL_TIMEOUT).unwrap(),
            "ok\n"
        );
    }
}
