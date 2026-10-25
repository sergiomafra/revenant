/// Errors produced by the engine. Each has a stable machine-readable code; no UI text lives here.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("open_failed")]
    Open {
        #[source]
        cause: std::io::Error,
    },
    #[error("read_failed")]
    Read {
        offset: u64,
        #[source]
        cause: std::io::Error,
    },
    #[error("destination_on_source")]
    DestinationOnSource,
    #[error("write_failed")]
    Write {
        #[source]
        cause: std::io::Error,
    },
    #[error("cancelled")]
    Cancelled,
    /// The source vanished (a card pulled out, a cable cut): never a bad sector. `offset` is the offset of the read that
    /// failed, or the first byte a short read did not return.
    #[error("device_gone")]
    DeviceGone { offset: u64 },
    /// Opening a device was refused (`EACCES`/`EPERM`, `ERROR_ACCESS_DENIED`): it needs root or Administrator.
    #[error("permission_denied")]
    PermissionDenied,
    /// No such device (it was never there, or it is gone before opening).
    #[error("device_not_found")]
    DeviceNotFound,
    /// The name is not a whole disk (a partition, a volume, or not a disk name at all).
    #[error("not_whole_disk")]
    NotWholeDisk,
    /// A system tool (`tool`: its absolute path) failed, or printed something that could not be read.
    #[error("system_tool_failed")]
    SystemTool { tool: &'static str },
    /// Reading devices is not supported on this operating system.
    #[error("unsupported_platform")]
    Unsupported,
    /// The source is a device and the disks the destination folder lives on could not be determined (a network
    /// folder, or the system query failed): writing there could land on the source itself.
    #[error("destination_unknown")]
    DestinationUnknown,
    /// Some volumes of a disk could not be unmounted (in use): `volumes` are those still mounted (macOS "disk4s1",
    /// Windows the volume GUID path), empty when the system did not say which.
    #[error("unmount_failed")]
    UnmountFailed { volumes: Vec<String> },
    /// The system refused to mount a disk's volumes again.
    #[error("remount_failed")]
    RemountFailed,
    /// The disk holds the running system: the engine never unmounts it.
    #[error("system_disk")]
    SystemDisk,
    /// A rescue map file is damaged: cut short, checksum wrong, or not a map at all.
    #[error("map_corrupt")]
    MapCorrupt,
    /// A rescue map does not describe its image (the sizes differ).
    #[error("map_mismatch")]
    MapMismatch,
    /// A rescue map was made from another source than the one being copied.
    #[error("map_other_source")]
    MapOtherSource,
    /// A saved scan state or session record is damaged: cut short, a length past its end, or a value out of range.
    #[error("session_corrupt")]
    SessionCorrupt,
    /// A saved session was written by a version of the engine this one cannot read.
    #[error("session_version")]
    SessionVersion,
    /// A saved session was made from another source than the one given to resume it.
    #[error("session_other_source")]
    SessionOtherSource,
    /// A new session needs an empty folder, and the one given holds something.
    #[error("session_not_empty")]
    SessionNotEmpty,
}

impl EngineError {
    /// Stable `snake_case` code identifying the error kind.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Open { .. } => "open_failed",
            Self::Read { .. } => "read_failed",
            Self::DestinationOnSource => "destination_on_source",
            Self::Write { .. } => "write_failed",
            Self::Cancelled => "cancelled",
            Self::DeviceGone { .. } => "device_gone",
            Self::PermissionDenied => "permission_denied",
            Self::DeviceNotFound => "device_not_found",
            Self::NotWholeDisk => "not_whole_disk",
            Self::SystemTool { .. } => "system_tool_failed",
            Self::Unsupported => "unsupported_platform",
            Self::DestinationUnknown => "destination_unknown",
            Self::UnmountFailed { .. } => "unmount_failed",
            Self::RemountFailed => "remount_failed",
            Self::SystemDisk => "system_disk",
            Self::MapCorrupt => "map_corrupt",
            Self::MapMismatch => "map_mismatch",
            Self::MapOtherSource => "map_other_source",
            Self::SessionCorrupt => "session_corrupt",
            Self::SessionVersion => "session_version",
            Self::SessionOtherSource => "session_other_source",
            Self::SessionNotEmpty => "session_not_empty",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_stable() {
        assert_eq!(
            EngineError::DestinationOnSource.code(),
            "destination_on_source"
        );
        assert_eq!(EngineError::Cancelled.code(), "cancelled");
        assert_eq!(EngineError::DeviceGone { offset: 0 }.code(), "device_gone");
        assert_eq!(EngineError::PermissionDenied.code(), "permission_denied");
        assert_eq!(EngineError::DeviceNotFound.code(), "device_not_found");
        assert_eq!(EngineError::NotWholeDisk.code(), "not_whole_disk");
        assert_eq!(
            EngineError::SystemTool {
                tool: "/usr/sbin/diskutil"
            }
            .code(),
            "system_tool_failed"
        );
        assert_eq!(EngineError::Unsupported.code(), "unsupported_platform");
        assert_eq!(
            EngineError::DestinationUnknown.code(),
            "destination_unknown"
        );
        assert_eq!(
            EngineError::UnmountFailed {
                volumes: vec!["disk4s1".into()]
            }
            .code(),
            "unmount_failed"
        );
        assert_eq!(EngineError::RemountFailed.code(), "remount_failed");
        assert_eq!(EngineError::SystemDisk.code(), "system_disk");
        assert_eq!(EngineError::MapCorrupt.code(), "map_corrupt");
        assert_eq!(EngineError::MapMismatch.code(), "map_mismatch");
        assert_eq!(EngineError::MapOtherSource.code(), "map_other_source");
        assert_eq!(EngineError::SessionCorrupt.code(), "session_corrupt");
        assert_eq!(EngineError::SessionVersion.code(), "session_version");
        assert_eq!(
            EngineError::SessionOtherSource.code(),
            "session_other_source"
        );
        assert_eq!(EngineError::SessionNotEmpty.code(), "session_not_empty");
        let io = || std::io::Error::other("x");
        assert_eq!(
            EngineError::Read {
                offset: 512,
                cause: io()
            }
            .code(),
            "read_failed"
        );
        assert_eq!(EngineError::Open { cause: io() }.code(), "open_failed");
        assert_eq!(EngineError::Write { cause: io() }.code(), "write_failed");
    }
}
