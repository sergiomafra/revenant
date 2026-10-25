//! Revenant recovery engine.

// The integration-test support (`tests/support`: failing sources, fake devices), written against `revenant_core`, shared
// with the unit tests (ruling P6).
#[cfg(test)]
extern crate self as revenant_core;
#[cfg(test)]
#[path = "../tests/support/mod.rs"]
pub(crate) mod test_support;

pub(crate) mod budget;
pub(crate) mod cache;
pub mod device;
pub mod error;
pub mod extract;
pub mod filesystem;
pub mod formats;
pub mod group;
pub(crate) mod identity;
pub mod inspect;
pub mod naming;
pub(crate) mod pairing;
pub mod rescue;
pub mod scan;
pub mod source;
pub(crate) mod tiff;

pub use error::EngineError;
pub use extract::{Destination, extract, extract_as, extract_to};
pub use filesystem::{FsOrigin, FsState, FsTime, Placement, Volume};
pub use formats::Fragment;
pub use identity::SourceIdentity;
pub use inspect::{Inspection, Metadata, Preview, inspect};
pub use scan::{CancelToken, Event, Finding, ScanOptions, ScanSummary, scan};
