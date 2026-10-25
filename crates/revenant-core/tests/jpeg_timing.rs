//! A timing bound, in a test binary of its own so that no other test competes for the CPU while it runs.

use revenant_core::formats::{Format, Health, Jpeg};
use revenant_core::source::FileSource;
use revenant_testkit as testkit;
use std::time::{Duration, Instant};

#[test]
fn a_crafted_huge_frame_is_measured_quickly() {
    // P1 of the Q8 probes: a flat 16384×12800 frame (210 megapixels) with 32 KiB of its data (about 21 rows of MCUs),
    // then 64 MiB of sectors `[00; 510] FF D9`. Measuring it once ran zune-jpeg's lenient decode over the whole declared
    // frame (about 1.1 s in release); now the entropy decoder alone judges it (Q10), and the gap search is bounded.
    let flat = testkit::jpeg_flat_with_restarts(16384, 12800, 0);
    let sos = flat.windows(2).position(|w| w == [0xFF, 0xDA]).unwrap();
    let scan_start = sos + 2 + usize::from(u16::from_be_bytes([flat[sos + 2], flat[sos + 3]]));
    let c = (scan_start + 32 * 1024).next_multiple_of(512);
    let mut img = flat[..c].to_vec();
    let mut sector = [0u8; 512];
    sector[510..].copy_from_slice(&[0xFF, 0xD9]);
    while img.len() < c + (64 << 20) {
        img.extend_from_slice(&sector);
    }
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), &img).unwrap();
    let src = FileSource::open(file.path()).unwrap();
    let mut best = Duration::MAX;
    for _ in 0..3 {
        let t = Instant::now();
        let m = Jpeg.measure(&src, 0).expect("a JPEG");
        best = best.min(t.elapsed());
        assert_eq!(m.health, Health::Partial, "{m:?}");
    }
    let bound = if cfg!(debug_assertions) {
        Duration::from_secs(10)
    } else {
        // Ruling R5: generous enough for a loaded machine, still far below the ~1.1 s of the old lenient decode. That no
        // zune-jpeg decode runs at all is asserted by the unit test
        // `formats::jpeg::tests::a_crafted_huge_frame_is_partial_without_the_lenient_decode`.
        Duration::from_millis(600)
    };
    assert!(best < bound, "{best:?}");
}
