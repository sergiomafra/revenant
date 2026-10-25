//! The operating system's view of whole disks: what macOS tools say about them (sizes, sector sizes).
//!
//! Windows asks its devices directly (`source/windows.rs`); other systems are not supported.

#[cfg(target_os = "macos")]
pub(crate) mod macos;
#[cfg(any(target_os = "macos", test))]
pub(crate) mod macos_parse;
#[cfg(any(target_os = "macos", test))]
pub(crate) mod plist;
