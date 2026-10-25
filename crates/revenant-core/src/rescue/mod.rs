//! The rescue copy of a failing card (design §5.4): an image file of the whole card and its map, which says what each
//! region of the image holds (ruling D16), and the image read back through its map so that what was never rescued is
//! unreadable to the scan, never zeros (ruling D18).

mod copy;
mod map;
mod mapped;
mod text;

pub use copy::{CopyEvent, CopyOptions, CopySummary, copy};
pub use map::{MAX_MAP_REGIONS, MapTotals, Pass, RegionState, RescueMap, map_path};
pub use mapped::MappedSource;
