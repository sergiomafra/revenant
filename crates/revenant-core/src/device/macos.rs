//! macOS system tools: run by absolute path, output bounded, any failure `system_tool_failed` (never a panic).

use std::io::Read;
use std::process::{Command, Stdio};

use super::macos_parse;
use super::plist::{self, Plist};
use crate::EngineError;
use crate::source::Geometry;

pub(crate) const DISKUTIL: &str = "/usr/sbin/diskutil";
pub(crate) const IOREG: &str = "/usr/sbin/ioreg";

/// Most bytes of output read from a tool; more is a failure.
const MAX_OUTPUT: u64 = 16 << 20;

/// Runs `tool` (an absolute path) with `args` and returns its standard output. Fails with `SystemTool { tool }` when it
/// cannot start, exits unsuccessfully, prints more than 16 MiB, or prints something that is not UTF-8.
pub(crate) fn run(tool: &'static str, args: &[&str]) -> Result<String, EngineError> {
    let failed = || EngineError::SystemTool { tool };
    let mut child = Command::new(tool)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| failed())?;
    let mut out = Vec::new();
    let read = child.stdout.take().ok_or_else(failed).and_then(|s| {
        s.take(MAX_OUTPUT + 1)
            .read_to_end(&mut out)
            .map_err(|_| failed())
    });
    if read.is_err() || out.len() as u64 > MAX_OUTPUT {
        // Stop a tool that is still printing; its status no longer matters.
        let _ = child.kill();
        let _ = child.wait();
        return Err(failed());
    }
    let status = child.wait().map_err(|_| failed())?;
    if !status.success() {
        return Err(failed());
    }
    String::from_utf8(out).map_err(|_| failed())
}

fn plist_of(tool: &'static str, args: &[&str]) -> Result<Plist, EngineError> {
    plist::parse(&run(tool, args)?).ok_or(EngineError::SystemTool { tool })
}

/// The geometry of the whole disk `bsd` ("disk4"), from `diskutil info -plist` and `ioreg` (ruling D5).
pub(crate) fn geometry(bsd: &str) -> Result<Geometry, EngineError> {
    let info = plist_of(DISKUTIL, &["info", "-plist", bsd])?;
    let ioreg = plist_of(IOREG, &["-a", "-r", "-c", "IOBlockStorageDevice", "-l"])?;
    macos_parse::geometry(&info, Some(&ioreg), bsd)
        .ok_or(EngineError::SystemTool { tool: DISKUTIL })
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
}
