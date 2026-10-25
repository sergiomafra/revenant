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
