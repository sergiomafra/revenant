//! The rescue copy's map (region states, crash-safe saving) and an image read through its map.

use revenant_core::formats::{FileKind, Health, all_formats};
use revenant_core::rescue::{MapTotals, MappedSource, Pass, RegionState, RescueMap, map_path};
use revenant_core::source::{FileSource, Source};
use revenant_core::{CancelToken, EngineError, Event, ScanOptions, SourceIdentity, scan};
use revenant_testkit as testkit;
use std::path::Path;
use testkit::DiskImage;

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;

/// Deterministic test randomness (SplitMix64).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

const STATES: [RegionState; 5] = [
    RegionState::Untried,
    RegionState::Rescued,
    RegionState::ToTrim,
    RegionState::ToScrape,
    RegionState::Bad,
];

fn device_identity() -> SourceIdentity {
    SourceIdentity::Device {
        size: 64 * MIB,
        model: Some("SD Card Reader".into()),
        serial: None,
        table: 0x0123_4567_89ab_cdef,
    }
}

fn image_identity(size: u64) -> SourceIdentity {
    SourceIdentity::Image {
        size,
        digest: 0xfeed_beef,
    }
}

/// Regions are ascending, contiguous from 0, merged (neighbors differ) and sum to the size.
fn assert_well_formed(map: &RescueMap) {
    let regions = map.regions();
    let mut at = 0;
    for (i, &(start, len, state)) in regions.iter().enumerate() {
        assert_eq!(start, at, "{regions:?}");
        assert!(len > 0, "{regions:?}");
        if i > 0 {
            assert_ne!(regions[i - 1].2, state, "not merged: {regions:?}");
        }
        at += len;
    }
    assert_eq!(at, map.size());
}

fn totals_of(map: &RescueMap) -> MapTotals {
    let mut t = MapTotals::default();
    for (_, len, state) in map.regions() {
        match state {
            RegionState::Untried => t.untried += len,
            RegionState::Rescued => t.rescued += len,
            RegionState::ToTrim => t.to_trim += len,
            RegionState::ToScrape => t.to_scrape += len,
            RegionState::Bad => t.bad += len,
        }
    }
    t
}

fn random_map(seed: u64, calls: usize) -> RescueMap {
    let mut rng = Rng(seed);
    let mut map = RescueMap::new(64 * MIB, 512, device_identity());
    let sectors = 64 * MIB / 512;
    for _ in 0..calls {
        let start = rng.below(sectors) * 512;
        let len = (1 + rng.below(4096)) * 512;
        let state = STATES[rng.below(5) as usize];
        assert!(map.set(start, len, state));
    }
    map.set_pass(Pass::Retry(3), 12 * MIB);
    map
}

#[test]
fn map_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("card.img.rvmap");
    let map = random_map(5, 200);
    assert!(map.regions().len() > 100, "{}", map.regions().len());
    assert_well_formed(&map);
    map.save(&path).unwrap();
    let loaded = RescueMap::load(&path).unwrap();
    assert_eq!(loaded, map);
    assert_eq!(loaded.regions(), map.regions());
    assert_eq!(loaded.pass(), (Pass::Retry(3), 12 * MIB));
    assert_eq!(loaded.identity(), &device_identity());
    assert_eq!((loaded.size(), loaded.sector()), (64 * MIB, 512));
    assert_eq!(loaded.totals(), totals_of(&map));
    assert!(!dir.path().join("card.img.rvmap.tmp").exists());

    // Every pass and an image identity round-trip too.
    for pass in [
        Pass::Fast,
        Pass::Skipped,
        Pass::Trim,
        Pass::Scrape,
        Pass::Retry(0),
        Pass::Done,
    ] {
        let mut m = RescueMap::new(MIB + 512, 4096, image_identity(MIB + 512));
        m.set(4096, 8192, RegionState::Bad);
        m.set(MIB, 4096, RegionState::Rescued); // the end clamps to the size
        m.set_pass(pass, 4096);
        m.save(&path).unwrap();
        assert_eq!(RescueMap::load(&path).unwrap(), m, "{pass:?}");
    }
    let names: Vec<_> = STATES.iter().map(|s| s.as_str()).collect();
    assert_eq!(names, ["untried", "rescued", "to_trim", "to_scrape", "bad"]);
    let passes: Vec<_> = [
        Pass::Fast,
        Pass::Skipped,
        Pass::Trim,
        Pass::Scrape,
        Pass::Retry(2),
        Pass::Done,
    ]
    .iter()
    .map(|p| p.as_str())
    .collect();
    assert_eq!(
        passes,
        ["fast", "skipped", "trim", "scrape", "retry", "done"]
    );
    assert_eq!(
        map_path(Path::new("/x/card.img")),
        Path::new("/x/card.img.rvmap")
    );
}

#[test]
fn map_merges_and_covers_the_whole_size() {
    let size = 64 * KIB;
    let mut map = RescueMap::new(size, 512, image_identity(size));
    assert_eq!(map.regions(), [(0, size, RegionState::Untried)]);
    assert_eq!(map.pass(), (Pass::Fast, 0));
    assert!(map.set(0, 4096, RegionState::Rescued));
    assert!(map.set(4096, 4096, RegionState::Rescued));
    assert_eq!(
        map.regions(),
        [
            (0, 8192, RegionState::Rescued),
            (8192, size - 8192, RegionState::Untried)
        ]
    );
    assert!(map.set(2048, 1024, RegionState::Bad));
    assert_eq!(
        map.regions(),
        [
            (0, 2048, RegionState::Rescued),
            (2048, 1024, RegionState::Bad),
            (3072, 5120, RegionState::Rescued),
            (8192, size - 8192, RegionState::Untried)
        ]
    );
    assert_well_formed(&map);
    assert_eq!(
        map.totals(),
        MapTotals {
            untried: size - 8192,
            rescued: 7168,
            bad: 1024,
            ..MapTotals::default()
        }
    );
    assert_eq!(map.state_at(2047), RegionState::Rescued);
    assert_eq!(map.state_at(2048), RegionState::Bad);
    assert_eq!(map.state_at(3071), RegionState::Bad);
    assert_eq!(map.state_at(3072), RegionState::Rescued);
    assert_eq!(map.state_at(size - 1), RegionState::Untried);
    // Healing the hole merges back into one region; a range past the end clamps.
    assert!(map.set(2048, 1024, RegionState::Rescued));
    assert!(map.set(8192, size, RegionState::Rescued));
    assert_eq!(map.regions(), [(0, size, RegionState::Rescued)]);
    assert_eq!(map.totals().rescued, size);

    // Many random sets keep the map well formed and the totals right.
    let map = random_map(9, 500);
    assert_well_formed(&map);
    assert_eq!(map.totals(), totals_of(&map));
    for (start, len, state) in map.regions() {
        assert_eq!(map.state_at(start), state);
        assert_eq!(map.state_at(start + len - 1), state);
    }
}

#[test]
fn map_survives_a_torn_write() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("card.img.rvmap");
    let v1 = random_map(11, 50);
    v1.save(&path).unwrap();
    let mut v2 = v1.clone();
    v2.set(0, 8 * MIB, RegionState::Rescued);
    v2.set_pass(Pass::Trim, 0);
    let other = dir.path().join("v2.rvmap");
    v2.save(&other).unwrap();
    let text = std::fs::read(&other).unwrap();

    // A crash during save leaves half of v2 in the temporary file: the map is still v1.
    let tmp = dir.path().join("card.img.rvmap.tmp");
    std::fs::write(&tmp, &text[..text.len() / 2]).unwrap();
    assert_eq!(RescueMap::load(&path).unwrap(), v1);
    // The next save replaces the leftover.
    v2.save(&path).unwrap();
    assert_eq!(RescueMap::load(&path).unwrap(), v2);
    assert!(!tmp.exists());

    // A map file cut anywhere is refused, never read as something else.
    let torn = dir.path().join("torn.rvmap");
    let mut rng = Rng(77);
    let mut cuts: Vec<usize> = (0..18)
        .map(|_| rng.below(text.len() as u64) as usize)
        .collect();
    cuts.extend([text.len() - 1, text.len() - 5]);
    for cut in cuts {
        std::fs::write(&torn, &text[..cut]).unwrap();
        let err = RescueMap::load(&torn).unwrap_err();
        assert_eq!(err.code(), "map_corrupt", "cut at {cut}");
    }

    // One digit changed in a region line: refused.
    let s = String::from_utf8(text.clone()).unwrap();
    let line_at = s.find("\n0 ").unwrap() + 1; // the first region line
    let digit = s[line_at..]
        .find(|c: char| c.is_ascii_digit() && c != '0')
        .unwrap()
        + line_at;
    let mut flipped = text.clone();
    flipped[digit] = if flipped[digit] == b'9' {
        b'8'
    } else {
        flipped[digit] + 1
    };
    std::fs::write(&torn, &flipped).unwrap();
    assert_eq!(RescueMap::load(&torn).unwrap_err().code(), "map_corrupt");
    // One digit changed in the pass position (still a valid position): only the checksum tells.
    let pos_at = s.find("pass trim ").unwrap() + "pass trim ".len();
    let mut moved = text.clone();
    assert_eq!(moved[pos_at], b'0');
    moved[pos_at] = b'1';
    std::fs::write(&torn, &moved).unwrap();
    assert_eq!(RescueMap::load(&torn).unwrap_err().code(), "map_corrupt");
    // Garbage after the end line, or a missing file's worth of nothing: refused.
    let mut longer = text.clone();
    longer.extend_from_slice(b"0 512 bad\n");
    std::fs::write(&torn, &longer).unwrap();
    assert_eq!(RescueMap::load(&torn).unwrap_err().code(), "map_corrupt");
    std::fs::write(&torn, b"").unwrap();
    assert_eq!(RescueMap::load(&torn).unwrap_err().code(), "map_corrupt");
}

/// Saves `bytes` as an image and `map` next to it.
fn image_with_map(dir: &Path, bytes: &[u8], map: &RescueMap) -> std::path::PathBuf {
    let image = dir.join("card.img");
    std::fs::write(&image, bytes).unwrap();
    map.save(&map_path(&image)).unwrap();
    image
}

fn read(source: &dyn Source, offset: u64, len: usize) -> Result<Vec<u8>, EngineError> {
    let mut buf = vec![0u8; len];
    let n = source.read_at(offset, &mut buf)?;
    buf.truncate(n);
    Ok(buf)
}

#[test]
fn mapped_source_reads_rescued_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let size = 4 * MIB;
    let bytes = DiskImage::noise(size as usize, 3).save();
    let bytes = std::fs::read(bytes.path()).unwrap();
    let mut map = RescueMap::new(size, 512, image_identity(size));
    map.set(0, size, RegionState::Rescued);
    map.set(MIB, 64 * KIB, RegionState::Untried);
    let image = image_with_map(dir.path(), &bytes, &map);
    let source = MappedSource::open(&image).unwrap();
    assert_eq!(source.len(), size);
    assert_eq!(source.map(), &map);
    assert_eq!(source.volume(), FileSource::open(&image).unwrap().volume());

    let m = MIB as usize;
    assert_eq!(read(&source, 0, m).unwrap(), &bytes[..m]);
    match read(&source, MIB - 512, 1024) {
        Err(EngineError::Read { offset, .. }) => assert_eq!(offset, MIB),
        other => panic!("{other:?}"),
    }
    match read(&source, MIB + 4096, 512) {
        Err(EngineError::Read { offset, .. }) => assert_eq!(offset, MIB + 4096),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        read(&source, 2 * MIB, 4096).unwrap(),
        &bytes[2 * m..2 * m + 4096]
    );
    assert_eq!(
        read(&source, MIB + 64 * KIB, 512).unwrap(),
        &bytes[m + 64 * 1024..m + 64 * 1024 + 512]
    );
    // Past the end: short, as any source.
    assert_eq!(
        read(&source, size - 512, 4096).unwrap(),
        &bytes[size as usize - 512..]
    );
    assert_eq!(read(&source, size, 512).unwrap(), Vec::<u8>::new());

    // Every non-Rescued state is unreadable.
    for state in [RegionState::ToTrim, RegionState::ToScrape, RegionState::Bad] {
        map.set(MIB, 64 * KIB, state);
        map.save(&map_path(&image)).unwrap();
        let source = MappedSource::open(&image).unwrap();
        assert!(read(&source, MIB, 512).is_err(), "{state:?}");
    }
}

fn scan_events(source: &dyn Source) -> Vec<Event> {
    let mut events = Vec::new();
    scan(
        source,
        &all_formats(),
        &ScanOptions::default(),
        &CancelToken::default(),
        &mut |e| events.push(e),
    )
    .unwrap();
    events
}

fn jpeg_at(events: &[Event], start: u64) -> Health {
    events
        .iter()
        .find_map(|e| match e {
            Event::Found(f) if f.start == start && f.kind == FileKind::Jpeg => Some(f.health),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no jpeg at {start}"))
}

fn unreadable(events: &[Event]) -> Vec<(u64, u64)> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Unreadable { start, len } => Some((*start, *len)),
            _ => None,
        })
        .collect()
}

#[test]
fn unrescued_zeros_are_never_intact() {
    let dir = tempfile::tempdir().unwrap();
    let size = 8 * MIB;
    let at = MIB as usize;
    let jpeg = testkit::jpeg(640, 480, 21, 192 * 1024);
    // P11: the second APP1 segment is all padding (free content): its payload follows the first segment
    // (2 + 4 + 65533 bytes) and its own 4-byte header.
    let seg2 = 2 + 4 + 65_533;
    assert_eq!(jpeg[seg2..seg2 + 2], [0xFF, 0xE1]);
    let payload = (at + seg2 + 4) as u64..(at + seg2 + 4 + 65_533) as u64;
    let run_start = payload.start.next_multiple_of(512);
    let run_end = payload.end / 512 * 512;
    // The longest sector-aligned run inside one APP1 payload (a payload is at most 65,533 bytes, so it cannot hold
    // 64 KiB: 127 sectors).
    assert_eq!(run_end - run_start, 127 * 512);

    let mut bytes = std::fs::read(DiskImage::noise(size as usize, 8).save().path()).unwrap();
    bytes[at..at + jpeg.len()].copy_from_slice(&jpeg);
    // What a copy leaves where it read nothing: zeros.
    bytes[run_start as usize..run_end as usize].fill(0);
    let mut map = RescueMap::new(size, 512, image_identity(size));
    map.set(0, size, RegionState::Rescued);
    map.set(run_start, run_end - run_start, RegionState::Untried);
    let image = image_with_map(dir.path(), &bytes, &map);

    // Read as a plain file the zeros look like data and the JPEG is whole.
    let plain = scan_events(&FileSource::open(&image).unwrap());
    assert_eq!(jpeg_at(&plain, MIB), Health::Intact);
    assert!(unreadable(&plain).is_empty());

    // Read through its map, the run is unreadable and the JPEG is not Intact.
    let mapped = scan_events(&MappedSource::open(&image).unwrap());
    assert_ne!(jpeg_at(&mapped, MIB), Health::Intact);
    assert_eq!(unreadable(&mapped), [(run_start, run_end - run_start)]);
}

#[test]
fn mapped_identity_counts_unrescued_as_zero() {
    let dir = tempfile::tempdir().unwrap();
    let size = 4 * MIB;
    let bytes = std::fs::read(DiskImage::noise(size as usize, 4).save().path()).unwrap();
    let hole = (512 * KIB, 64 * KIB);
    let origin = device_identity();
    let mut map = RescueMap::new(size, 512, origin.clone());
    map.set(0, size, RegionState::Rescued);
    map.set(hole.0, hole.1, RegionState::Untried);
    let image = image_with_map(dir.path(), &bytes, &map);
    let id = SourceIdentity::of_mapped(&MappedSource::open(&image).unwrap()).unwrap();

    let mut zeroed = bytes.clone();
    zeroed[hole.0 as usize..(hole.0 + hole.1) as usize].fill(0);
    let zeroed_path = dir.path().join("zeroed.img");
    std::fs::write(&zeroed_path, &zeroed).unwrap();
    let plain = SourceIdentity::of_image(&FileSource::open(&zeroed_path).unwrap()).unwrap();
    let SourceIdentity::Image {
        size: plain_size,
        digest: plain_digest,
    } = plain
    else {
        panic!("{plain:?}")
    };
    match &id {
        SourceIdentity::Mapped {
            size: s,
            digest,
            origin: o,
        } => {
            assert_eq!((*s, *digest), (plain_size, plain_digest));
            assert_eq!(**o, origin);
        }
        other => panic!("{other:?}"),
    }
    assert!(id.same_source(&id.clone()));
    assert!(!id.same_source(&plain) && !plain.same_source(&id));
    assert!(!id.same_source(&origin));

    // Rescuing more of the first MiB changes the identity.
    map.set(hole.0, 32 * KIB, RegionState::Rescued);
    map.save(&map_path(&image)).unwrap();
    let later = SourceIdentity::of_mapped(&MappedSource::open(&image).unwrap()).unwrap();
    assert_ne!(later, id);
    assert!(!later.same_source(&id));
}

#[test]
fn map_mismatch_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let map = RescueMap::new(4 * MIB, 512, image_identity(4 * MIB));
    let image = image_with_map(dir.path(), &vec![0u8; 8 * MIB as usize], &map);
    match MappedSource::open(&image) {
        Err(e) => assert_eq!(e.code(), "map_mismatch"),
        Ok(_) => panic!("opened a map of another size"),
    }
}

#[test]
fn map_nested_deeper_than_the_file_reads_is_not_saved() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("card.img.rvmap");
    let mut id = image_identity(MIB);
    let mut last_saved = None;
    for depth in 1..=5u64 {
        id = SourceIdentity::Mapped {
            size: MIB,
            digest: depth,
            origin: Box::new(id),
        };
        let map = RescueMap::new(MIB, 512, id.clone());
        let saved = map.save(&path);
        if depth <= 4 {
            saved.unwrap();
            assert_eq!(RescueMap::load(&path).unwrap(), map, "depth {depth}");
            last_saved = Some(map);
        } else {
            assert_eq!(saved.unwrap_err().code(), "write_failed");
            assert!(!dir.path().join("card.img.rvmap.tmp").exists());
            assert_eq!(RescueMap::load(&path).ok(), last_saved);
        }
    }
}

#[test]
fn a_save_failing_before_the_rename_keeps_the_old_map() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("card.img.rvmap");
    let old = random_map(9, 50);
    old.save(&path).unwrap();
    // The temporary file cannot be created: a folder is in its place.
    std::fs::create_dir(dir.path().join("card.img.rvmap.tmp")).unwrap();
    let mut new = old.clone();
    assert!(new.set(0, 512, RegionState::Bad));
    assert_eq!(new.save(&path).unwrap_err().code(), "write_failed");
    assert_eq!(RescueMap::load(&path).unwrap(), old);
}
