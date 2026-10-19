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
