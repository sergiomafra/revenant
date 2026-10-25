//! The rescue copy: a whole source copied into a sparse image with its map, around bad sectors, resumable, stopping on a
//! vanished device without marking anything bad, and refusing a destination that cannot hold it.

mod support;

use std::path::{Path, PathBuf};
use std::time::Duration;

use revenant_core::device::{DestInfo, FAT_MAX_FILE};
use revenant_core::rescue::{
    CopyEvent, CopyOptions, CopySummary, Pass, RegionState, RescueMap, copy, map_path,
};
use revenant_core::source::{DiskId, Source};
use revenant_core::{CancelToken, Destination, EngineError, SourceIdentity};
use support::{FakeDevices, SimSource};

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;
const IMAGE: &str = "card.img";

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

fn data(len: u64, seed: u64) -> Vec<u8> {
    let mut rng = Rng(seed);
    let mut out = Vec::with_capacity(len as usize);
    while (out.len() as u64) < len {
        out.extend_from_slice(&rng.next().to_le_bytes());
    }
    out.truncate(len as usize);
    out
}

fn identity(source: &dyn Source) -> SourceIdentity {
    SourceIdentity::Image {
        size: source.len(),
        digest: 0xabcd,
    }
}

/// A destination for an image source (no device info: `ops` is not asked).
fn dest_for(source: &dyn Source, dir: &Path) -> Destination {
    Destination::check(&FakeDevices::new(), source, dir).unwrap()
}

fn image_path(dir: &Path) -> PathBuf {
    std::fs::canonicalize(dir).unwrap().join(IMAGE)
}

fn load_map(dir: &Path) -> RescueMap {
    RescueMap::load(&map_path(&image_path(dir))).unwrap()
}

struct Run {
    result: Result<CopySummary, EngineError>,
    events: Vec<CopyEvent>,
}

fn run_copy(source: &dyn Source, dest: &Destination, opts: &CopyOptions) -> Run {
    run_with(source, &identity(source), dest, opts, |_, _, _| {})
}

/// Runs a copy calling `hook(index, event, cancel)` on every event.
fn run_with(
    source: &dyn Source,
    id: &SourceIdentity,
    dest: &Destination,
    opts: &CopyOptions,
    mut hook: impl FnMut(usize, &CopyEvent, &CancelToken),
) -> Run {
    let cancel = CancelToken::default();
    let mut events = Vec::new();
    let result = copy(source, id, dest, IMAGE, opts, &cancel, &mut |e| {
        hook(events.len(), &e, &cancel);
        events.push(e);
    });
    Run { result, events }
}

fn code<T>(r: &Result<T, EngineError>) -> Option<&'static str> {
    r.as_ref().err().map(|e| e.code())
}

fn bad_regions(map: &RescueMap) -> Vec<(u64, u64)> {
    map.regions()
        .into_iter()
        .filter(|r| r.2 == RegionState::Bad)
        .map(|r| (r.0, r.1))
        .collect()
}

fn touches(off: u64, len: usize, (s, l): (u64, u64)) -> bool {
    off < s + l && off + len as u64 > s
}

const BAD_RUNS: [(u64, u64); 3] = [
    (3 * MIB + 512, 512),
    (9 * MIB, 4096),
    (20 * MIB - 256 * KIB, MIB + 512),
];

fn bad_source(bytes: &[u8]) -> SimSource {
    let mut s = SimSource::new(bytes.to_vec());
    for (start, len) in BAD_RUNS {
        s = s.bad(start, len);
    }
    s
}

#[test]
fn clean_copy_is_byte_identical() {
    let dir = tempfile::tempdir().unwrap();
    let bytes = data(32 * MIB, 1);
    let source = SimSource::new(bytes.clone());
    let dest = dest_for(&source, dir.path());
    let run = run_copy(&source, &dest, &CopyOptions::default());
    let summary = run.result.unwrap();
    assert!(summary.complete);
    assert_eq!(summary.totals.rescued, 32 * MIB);
    assert_eq!(std::fs::read(image_path(dir.path())).unwrap(), bytes);
    let map = load_map(dir.path());
    assert_eq!(map.regions(), [(0, 32 * MIB, RegionState::Rescued)]);
    assert_eq!(map.pass(), (Pass::Done, 32 * MIB));
    assert_eq!(map.identity(), &identity(&source));
    assert_eq!(map.totals(), summary.totals);
    // Each byte read once, in 1 MiB blocks.
    assert_eq!(source.reads().len(), 32);
}

#[test]
fn bad_sectors_are_isolated() {
    let dir = tempfile::tempdir().unwrap();
    let bytes = data(32 * MIB, 2);
    let source = bad_source(&bytes);
    let dest = dest_for(&source, dir.path());
    let summary = run_copy(&source, &dest, &CopyOptions::default())
        .result
        .unwrap();
    assert!(summary.complete);
    let map = load_map(dir.path());
    assert_eq!(bad_regions(&map), BAD_RUNS);
    assert_eq!(
        summary.totals.bad,
        BAD_RUNS.iter().map(|r| r.1).sum::<u64>()
    );
    assert_eq!(summary.totals.rescued + summary.totals.bad, 32 * MIB);
    let image = std::fs::read(image_path(dir.path())).unwrap();
    assert_eq!(image.len(), bytes.len());
    for (start, len, state) in map.regions() {
        let r = start as usize..(start + len) as usize;
        match state {
            RegionState::Rescued => assert!(image[r.clone()] == bytes[r], "{start}"),
            RegionState::Bad => assert!(image[r].iter().all(|&b| b == 0), "{start}"),
            other => panic!("{other:?} left at {start}"),
        }
    }
}

#[test]
fn fast_pass_skips_ahead_and_comes_back() {
    let dir = tempfile::tempdir().unwrap();
    let bad = (4 * MIB, 8 * MIB);
    // A second, later failing block: the jump starts again from 64 KiB after the good blocks in between.
    let later = (40 * MIB, MIB);
    let source = SimSource::zeros(64 * MIB)
        .bad(bad.0, bad.1)
        .bad(later.0, later.1);
    let dest = dest_for(&source, dir.path());
    let summary = run_copy(&source, &dest, &CopyOptions::default())
        .result
        .unwrap();
    assert!(summary.complete);
    let reads = source.reads();
    let failing: Vec<usize> = (0..reads.len())
        .filter(|&i| touches(reads[i].0, reads[i].1, bad))
        .collect();
    let (f1, f2) = (failing[0], failing[1]);
    // The first failing chunk, then a jump of 64 KiB past it; the second (consecutive) failure jumps 128 KiB.
    assert_eq!(reads[f1], (4 * MIB, MIB as usize));
    assert_eq!(f2, f1 + 1, "{:?}", &reads[f1..f1 + 3]);
    let end1 = reads[f1].0 + reads[f1].1 as u64;
    assert!(reads[f1 + 1].0 >= end1 + 64 * KIB, "{:?}", reads[f1 + 1]);
    let end2 = reads[f2].0 + reads[f2].1 as u64;
    assert!(reads[f2 + 1].0 >= end2 + 128 * KIB, "{:?}", reads[f2 + 1]);
    let l1 = (0..reads.len())
        .find(|&i| touches(reads[i].0, reads[i].1, later))
        .unwrap();
    assert_eq!(reads[l1], (40 * MIB, MIB as usize));
    assert_eq!(reads[l1 + 1].0, 41 * MIB + 64 * KIB, "{:?}", reads[l1 + 1]);
    let map = load_map(dir.path());
    assert_eq!(bad_regions(&map), [bad, later]);
    // Trim reads the run's two edges (start forward, end backward) before any of its middle.
    let sectors: Vec<u64> = failing
        .iter()
        .filter(|&&i| reads[i].1 == 512)
        .map(|&i| reads[i].0)
        .collect();
    assert_eq!(sectors[..2], [bad.0, bad.0 + bad.1 - 512]);
    let chunk_reads = failing.iter().filter(|&&i| reads[i].1 > 512).count();
    let sector_reads = failing.len() - chunk_reads;
    // One ToTrim run: its two edges, then the middle sector by sector.
    assert!(
        failing.len() <= chunk_reads + (8 * MIB / 512) as usize + 2,
        "{} reads, {chunk_reads} chunks",
        failing.len()
    );
    assert_eq!(sector_reads, (8 * MIB / 512) as usize);
}

#[test]
fn copy_never_rereads_bad_sectors_without_retries() {
    let dir = tempfile::tempdir().unwrap();
    let bytes = data(32 * MIB, 3);
    let source = bad_source(&bytes);
    let dest = dest_for(&source, dir.path());
    assert!(
        run_copy(&source, &dest, &CopyOptions::default())
            .result
            .unwrap()
            .complete
    );
    let before = load_map(dir.path());

    source.clear_reads();
    let again = run_copy(&source, &dest, &CopyOptions::default());
    assert!(again.result.unwrap().complete);
    assert_eq!(source.reads(), []);
    assert_eq!(load_map(dir.path()), before);

    let retries = CopyOptions {
        retries: 2,
        ..CopyOptions::default()
    };
    let summary = run_copy(&source, &dest, &retries).result.unwrap();
    assert!(summary.complete);
    let mut expected = Vec::new();
    for (start, len) in BAD_RUNS {
        for s in (start..start + len).step_by(512) {
            expected.push((s, 512usize));
        }
    }
    let mut reads = source.reads();
    reads.sort_unstable();
    let mut twice: Vec<_> = expected.iter().flat_map(|&r| [r, r]).collect();
    twice.sort_unstable();
    assert_eq!(reads, twice);
    assert_eq!(bad_regions(&load_map(dir.path())), BAD_RUNS);

    // The two repetitions are counted: asking for two again reads nothing; asking for three reads each once more.
    source.clear_reads();
    run_copy(&source, &dest, &retries).result.unwrap();
    assert_eq!(source.reads(), []);
    let three = CopyOptions {
        retries: 3,
        ..CopyOptions::default()
    };
    run_copy(&source, &dest, &three).result.unwrap();
    let mut reads = source.reads();
    reads.sort_unstable();
    assert_eq!(reads, expected);
}

#[test]
fn copy_stops_when_the_source_shrinks() {
    let dir = tempfile::tempdir().unwrap();
    let source = SimSource::new(data(64 * MIB, 4)).shrink_after(40, 18 * MIB);
    let dest = dest_for(&source, dir.path());
    let run = run_copy(&source, &dest, &CopyOptions::default());
    assert_eq!(code(&run.result), Some("device_gone"));
    let map = load_map(dir.path());
    assert_eq!(map.totals().bad, 0);
    assert_eq!(map.totals().to_trim, 0);
    assert_eq!(map.totals().rescued, 40 * MIB);
    assert_eq!(run.events.last(), Some(&CopyEvent::Saved));
}

fn device_dest(
    fake: &FakeDevices,
    source: &dyn Source,
    dir: &Path,
    free: u64,
    max: Option<u64>,
) -> Destination {
    fake.set_destination(
        dir,
        Ok(DestInfo {
            disks: vec![DiskId::parse("disk2").unwrap()],
            fs: Some("exfat".into()),
            free_bytes: free,
            max_file_bytes: max,
        }),
    );
    Destination::check(fake, source, dir).unwrap()
}

fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn copy_refuses_a_destination_that_cannot_hold_it() {
    let fake = FakeDevices::new();
    let dir = tempfile::tempdir().unwrap();
    let source = SimSource::new(data(32 * MIB, 5)).disk("disk4");
    let small = device_dest(&fake, &source, dir.path(), 10 * MIB, None);
    let run = run_copy(&source, &small, &CopyOptions::default());
    assert_eq!(code(&run.result), Some("write_failed"));
    assert!(entries(dir.path()).is_empty(), "{:?}", entries(dir.path()));
    assert_eq!(source.reads(), []);

    // A FAT32 destination cannot hold a 5 GiB image, however free it is; the image is never split.
    let huge = SimSource::zeros(5 << 30).disk("disk4");
    let fat = device_dest(&fake, &huge, dir.path(), 1 << 40, Some(FAT_MAX_FILE));
    let run = run_copy(&huge, &fat, &CopyOptions::default());
    assert_eq!(code(&run.result), Some("write_failed"));
    assert!(entries(dir.path()).is_empty());

    // A resume only needs what is still missing: stop at 16 MiB rescued, then 20 MiB free is enough, 10 MiB is not.
    let roomy = device_dest(&fake, &source, dir.path(), 1 << 40, None);
    let run = run_with(
        &source,
        &identity(&source),
        &roomy,
        &CopyOptions::default(),
        |_, e, cancel| {
            if let CopyEvent::Progress { totals, .. } = e
                && totals.rescued >= 16 * MIB
            {
                cancel.cancel();
            }
        },
    );
    assert_eq!(code(&run.result), Some("cancelled"));
    assert_eq!(load_map(dir.path()).totals().rescued, 16 * MIB);
    let map_bytes = std::fs::read(map_path(&image_path(dir.path()))).unwrap();
    let tight = device_dest(&fake, &source, dir.path(), 10 * MIB, None);
    source.clear_reads();
    let run = run_copy(&source, &tight, &CopyOptions::default());
    assert_eq!(code(&run.result), Some("write_failed"));
    assert_eq!(source.reads(), []);
    assert_eq!(
        std::fs::read(map_path(&image_path(dir.path()))).unwrap(),
        map_bytes
    );
    let enough = device_dest(&fake, &source, dir.path(), 20 * MIB, None);
    let summary = run_copy(&source, &enough, &CopyOptions::default())
        .result
        .unwrap();
    assert!(summary.complete);
}

#[test]
fn retries_rescue_a_flaky_sector() {
    let bytes = data(32 * MIB, 6);
    let dir = tempfile::tempdir().unwrap();
    let source = SimSource::new(bytes.clone()).flaky(7 * MIB, 512, 2);
    let dest = dest_for(&source, dir.path());
    run_copy(&source, &dest, &CopyOptions::default())
        .result
        .unwrap();
    assert_eq!(bad_regions(&load_map(dir.path())), [(7 * MIB, 512)]);

    let dir = tempfile::tempdir().unwrap();
    let source = SimSource::new(bytes.clone()).flaky(7 * MIB, 512, 2);
    let dest = dest_for(&source, dir.path());
    let opts = CopyOptions {
        retries: 3,
        ..CopyOptions::default()
    };
    let summary = run_copy(&source, &dest, &opts).result.unwrap();
    assert!(summary.complete);
    assert_eq!(summary.totals.rescued, 32 * MIB);
    assert_eq!(
        load_map(dir.path()).regions(),
        [(0, 32 * MIB, RegionState::Rescued)]
    );
    assert_eq!(std::fs::read(image_path(dir.path())).unwrap(), bytes);
}

#[test]
fn copy_stops_on_a_vanished_device_without_marking_bad() {
    let dir = tempfile::tempdir().unwrap();
    let bytes = data(32 * MIB, 7);
    let source = SimSource::new(bytes.clone()).gone_at(20 * MIB);
    let dest = dest_for(&source, dir.path());
    let run = run_copy(&source, &dest, &CopyOptions::default());
    assert_eq!(code(&run.result), Some("device_gone"));
    let map = load_map(dir.path());
    assert_eq!(map.totals().bad, 0);
    assert_eq!(map.totals().to_trim, 0);
    assert_eq!(map.totals().to_scrape, 0);
    assert_eq!(map.totals().rescued, 20 * MIB);
    source.revive();
    let summary = run_copy(&source, &dest, &CopyOptions::default())
        .result
        .unwrap();
    assert!(summary.complete);
    assert_eq!(summary.totals.rescued, 32 * MIB);
    assert_eq!(std::fs::read(image_path(dir.path())).unwrap(), bytes);
}

/// A source of 16 MiB with three bad runs at seeded places.
fn seeded_source(bytes: &[u8], seed: u64) -> SimSource {
    let mut rng = Rng(seed ^ 0x55);
    let mut s = SimSource::new(bytes.to_vec());
    for _ in 0..3 {
        let start = rng.below(bytes.len() as u64 / 512) * 512;
        let len = (1 + rng.below(4000)) * 512;
        s = s.bad(start, len.min(bytes.len() as u64 - start));
    }
    s
}

#[test]
fn copy_resumes_from_a_crash_snapshot() {
    let opts = CopyOptions {
        save_bytes: MIB,
        ..CopyOptions::default()
    };
    for seed in 0..5u64 {
        let bytes = data(16 * MIB, 100 + seed);
        let whole_dir = tempfile::tempdir().unwrap();
        let whole_src = seeded_source(&bytes, seed);
        let whole = run_copy(&whole_src, &dest_for(&whole_src, whole_dir.path()), &opts);
        assert!(whole.result.unwrap().complete);
        let expected = load_map(whole_dir.path());

        let dir = tempfile::tempdir().unwrap();
        let aside = tempfile::tempdir().unwrap();
        let image = image_path(dir.path());
        let map = map_path(&image);
        let (img_aside, map_aside) = (aside.path().join("img"), aside.path().join("map"));
        let at = Rng(seed).below(whole.events.len() as u64) as usize;
        let source = seeded_source(&bytes, seed);
        let dest = dest_for(&source, dir.path());
        let run = run_with(&source, &identity(&source), &dest, &opts, |i, _, cancel| {
            if i == at {
                std::fs::copy(&image, &img_aside).unwrap();
                std::fs::copy(&map, &map_aside).unwrap();
                cancel.cancel();
            }
        });
        let _ = run.result;
        std::fs::copy(&img_aside, &image).unwrap();
        std::fs::copy(&map_aside, &map).unwrap();
        let restored = RescueMap::load(&map).unwrap();

        let source = seeded_source(&bytes, seed);
        let summary = run_copy(&source, &dest, &opts).result.unwrap();
        assert!(summary.complete, "seed {seed}");
        let resumed = RescueMap::load(&map).unwrap();
        assert_eq!(resumed, expected, "seed {seed} at event {at}");
        let img = std::fs::read(&image).unwrap();
        for (start, len, state) in resumed.regions() {
            if state == RegionState::Rescued {
                let r = start as usize..(start + len) as usize;
                assert!(img[r.clone()] == bytes[r], "seed {seed}: {start}");
            }
        }
        for (off, len) in source.reads() {
            for (s, l, state) in restored.regions() {
                assert!(
                    state != RegionState::Rescued || !touches(off, len, (s, l)),
                    "seed {seed}: read ({off}, {len}) inside rescued ({s}, {l})"
                );
            }
        }
    }
}

#[test]
fn resume_against_another_source_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let source = SimSource::new(data(8 * MIB, 8));
    let dest = dest_for(&source, dir.path());
    let a = SourceIdentity::Image {
        size: 8 * MIB,
        digest: 0xa,
    };
    let b = SourceIdentity::Image {
        size: 8 * MIB,
        digest: 0xb,
    };
    let opts = CopyOptions::default();
    run_with(&source, &a, &dest, &opts, |_, _, _| {})
        .result
        .unwrap();
    let map_bytes = std::fs::read(map_path(&image_path(dir.path()))).unwrap();
    source.clear_reads();
    let run = run_with(&source, &b, &dest, &opts, |_, _, _| {});
    assert_eq!(code(&run.result), Some("map_other_source"));
    assert_eq!(source.reads(), []);
    assert_eq!(
        std::fs::read(map_path(&image_path(dir.path()))).unwrap(),
        map_bytes
    );

    // A source of another size than the map's.
    let longer = SimSource::new(data(9 * MIB, 8));
    let run = run_with(&longer, &a, &dest, &opts, |_, _, _| {});
    assert_eq!(code(&run.result), Some("map_mismatch"));
}

#[test]
fn copy_progress_has_rate_and_eta() {
    let dir = tempfile::tempdir().unwrap();
    let source = SimSource::new(data(32 * MIB, 9))
        .slow(0, 16 * MIB, Duration::from_millis(20))
        .bad(20 * MIB, 4096);
    let dest = dest_for(&source, dir.path());
    let run = run_copy(&source, &dest, &CopyOptions::default());
    run.result.unwrap();
    let progress: Vec<_> = run
        .events
        .iter()
        .filter_map(|e| match e {
            CopyEvent::Progress {
                pass,
                totals,
                bytes_per_sec,
                eta_secs,
                ..
            } => Some((*pass, *totals, *bytes_per_sec, *eta_secs)),
            _ => None,
        })
        .collect();
    assert!(!progress.is_empty());
    for (_, t, _, _) in &progress {
        assert_eq!(
            t.untried + t.rescued + t.to_trim + t.to_scrape + t.bad,
            32 * MIB
        );
    }
    assert_eq!(progress.last().unwrap().0, Pass::Done);
    assert!(progress.iter().any(|p| p.2 > 0));
    assert!(progress.iter().any(|p| p.3.is_some()));
    assert!(
        run.events.iter().any(
            |e| matches!(e, CopyEvent::Bad { start, len } if *start == 20 * MIB && *len == 512)
        )
    );
    assert!(run.events.contains(&CopyEvent::Saved));
}

#[test]
fn copy_writes_only_a_plain_file_in_the_checked_folder() {
    let dir = tempfile::tempdir().unwrap();
    let source = SimSource::new(data(MIB, 10));
    let dest = dest_for(&source, dir.path());
    let id = identity(&source);
    let opts = CopyOptions::default();
    let cancel = CancelToken::default();
    for name in ["../card.img", "sub/card.img", "..", "", "card:img"] {
        let r = copy(&source, &id, &dest, name, &opts, &cancel, &mut |_| {});
        assert_eq!(code(&r), Some("write_failed"), "{name:?}");
    }
    assert!(entries(dir.path()).is_empty());
    assert_eq!(source.reads(), []);

    // An image name that is a link (to a file elsewhere) is never opened for writing.
    #[cfg(unix)]
    {
        let elsewhere = tempfile::tempdir().unwrap();
        let target = elsewhere.path().join("victim");
        std::fs::write(&target, vec![7u8; MIB as usize]).unwrap();
        std::os::unix::fs::symlink(&target, image_path(dir.path())).unwrap();
        let run = run_copy(&source, &dest, &opts);
        assert_eq!(code(&run.result), Some("write_failed"));
        assert_eq!(std::fs::read(&target).unwrap(), vec![7u8; MIB as usize]);
        assert_eq!(source.reads(), []);
    }
}
