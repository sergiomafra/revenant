use revenant_core::filesystem::{FsOrigin, FsState, Placement};
use revenant_core::formats::all_formats;
use revenant_core::formats::{FileKind, Health};
use revenant_core::source::{FileSource, Source, VolumeId, volume_of};
use revenant_core::{
    CancelToken, EngineError, Event, Finding, Fragment, ScanOptions, extract, extract_as, scan,
};
use revenant_testkit as testkit;
use testkit::DiskImage;

const MIB: usize = 1024 * 1024;

/// Wraps a source, overriding its volume and optionally failing reads that touch `bad`.
struct TestSource {
    inner: FileSource,
    volume: Option<VolumeId>,
    bad: Option<(u64, u64)>,
}

impl Source for TestSource {
    fn len(&self) -> u64 {
        self.inner.len()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, EngineError> {
        if let Some((s, e)) = self.bad
            && offset < e
            && offset + buf.len() as u64 > s
        {
            return Err(EngineError::Read {
                offset,
                cause: std::io::Error::other("injected"),
            });
        }
        self.inner.read_at(offset, buf)
    }

    fn volume(&self) -> Option<VolumeId> {
        self.volume
    }
}

struct Fixture {
    _img: tempfile::NamedTempFile,
    source: TestSource,
    finding: Finding,
    bytes: Vec<u8>,
    image: Vec<u8>,
}

fn fixture(volume: Option<VolumeId>, bad: Option<(u64, u64)>, jpeg_len: usize) -> Fixture {
    let bytes = testkit::jpeg(64, 64, 1, jpeg_len);
    let start = 4096usize;
    let mut disk = DiskImage::noise(4 * MIB, 3);
    disk.put(start, &bytes);
    let img = disk.save();
    let image = std::fs::read(img.path()).unwrap();
    let source = TestSource {
        inner: FileSource::open(img.path()).unwrap(),
        volume,
        bad,
    };
    let finding = Finding {
        id: 1,
        start: start as u64,
        len: bytes.len() as u64,
        kind: FileKind::Jpeg,
        health: Health::Intact,
        fragments: Vec::new(),
        origin: None,
    };
    Fixture {
        _img: img,
        source,
        finding,
        bytes,
        image,
    }
}

fn other() -> Option<VolumeId> {
    Some(VolumeId(u64::MAX))
}

#[test]
fn extracts_identical_bytes() {
    let f = fixture(other(), None, 0);
    let dir = tempfile::tempdir().unwrap();
    let path = run_extract(&f, dir.path());
    assert_eq!(path.file_name().unwrap(), "revenant_000000001000.jpg");
    assert_eq!(std::fs::read(path).unwrap(), f.bytes);
}

fn run_extract(f: &Fixture, dir: &std::path::Path) -> std::path::PathBuf {
    extract(&f.source, &f.finding, dir).unwrap()
}

#[test]
fn extracts_across_chunk_boundaries() {
    let mut f = fixture(other(), None, 0);
    f.finding.start = 100;
    f.finding.len = (2 * MIB + 777) as u64;
    let dir = tempfile::tempdir().unwrap();
    let path = run_extract(&f, dir.path());
    assert_eq!(
        std::fs::read(path).unwrap(),
        f.image[100..100 + 2 * MIB + 777]
    );
}

#[test]
fn never_overwrites() {
    let f = fixture(other(), None, 0);
    let dir = tempfile::tempdir().unwrap();
    let first = run_extract(&f, dir.path());
    let second = run_extract(&f, dir.path());
    let third = run_extract(&f, dir.path());
    assert_eq!(second.file_name().unwrap(), "revenant_000000001000_2.jpg");
    assert_eq!(third.file_name().unwrap(), "revenant_000000001000_3.jpg");
    assert_eq!(std::fs::read(first).unwrap(), f.bytes);
}

#[test]
fn refuses_destination_on_source_volume() {
    let dir = tempfile::tempdir().unwrap();
    let f = fixture(Some(volume_of(dir.path()).unwrap()), None, 0);
    let err = extract(&f.source, &f.finding, dir.path()).unwrap_err();
    assert_eq!(err.code(), "destination_on_source");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn accepts_other_volume() {
    let f = fixture(other(), None, 0);
    let dir = tempfile::tempdir().unwrap();
    assert!(extract(&f.source, &f.finding, dir.path()).is_ok());
}

#[test]
fn accepts_unknown_volume() {
    let f = fixture(None, None, 0);
    let dir = tempfile::tempdir().unwrap();
    assert!(extract(&f.source, &f.finding, dir.path()).is_ok());
}

#[test]
fn missing_destination_is_open_failed() {
    let f = fixture(other(), None, 0);
    let dir = tempfile::tempdir().unwrap();
    let err = extract(&f.source, &f.finding, &dir.path().join("nope")).unwrap_err();
    assert_eq!(err.code(), "open_failed");
}

#[test]
fn unreadable_sector_becomes_zeros() {
    // Bad sector is the absolute 512-byte sector at 4096 + 1024.
    let bad = (4096 + 1024, 4096 + 1536);
    let f = fixture(other(), Some(bad), 8000);
    assert!(f.finding.len > 2048);
    let dir = tempfile::tempdir().unwrap();
    let path = run_extract(&f, dir.path());
    let out = std::fs::read(path).unwrap();
    let mut expected = f.bytes.clone();
    expected[1024..1536].fill(0);
    assert_eq!(out.len(), expected.len());
    assert_eq!(out, expected);
}

#[test]
fn length_past_the_source_end_is_clamped() {
    let mut f = fixture(other(), None, 0);
    let source_len = f.image.len() as u64;
    for len in [source_len * 2, u64::MAX] {
        f.finding.len = len;
        let dir = tempfile::tempdir().unwrap();
        let path = run_extract(&f, dir.path());
        let out = std::fs::read(path).unwrap();
        assert_eq!(
            out.len() as u64,
            source_len - f.finding.start,
            "len = {len}"
        );
        assert_eq!(out, f.image[f.finding.start as usize..]);
    }
}

#[test]
fn extracts_a_fragmented_finding_in_file_order() {
    let mut f = fixture(other(), None, 0);
    f.finding.start = 8192;
    f.finding.len = 2500;
    f.finding.fragments = vec![
        Fragment {
            start: 8192,
            len: 1000,
        },
        Fragment {
            start: 4096,
            len: 1500,
        },
    ];
    let dir = tempfile::tempdir().unwrap();
    let path = run_extract(&f, dir.path());
    assert_eq!(path.file_name().unwrap(), "revenant_000000002000.jpg");
    let mut expected = f.image[8192..9192].to_vec();
    expected.extend_from_slice(&f.image[4096..5596]);
    assert_eq!(std::fs::read(path).unwrap(), expected);
}

#[test]
fn a_fragment_past_the_source_end_is_clamped() {
    let mut f = fixture(other(), None, 0);
    let end = f.image.len() as u64;
    f.finding.start = end - 100;
    f.finding.len = 1100;
    f.finding.fragments = vec![
        Fragment {
            start: end - 100,
            len: 600,
        },
        Fragment {
            start: 4096,
            len: 500,
        },
    ];
    let dir = tempfile::tempdir().unwrap();
    let path = run_extract(&f, dir.path());
    let mut expected = f.image[end as usize - 100..].to_vec();
    expected.extend_from_slice(&f.image[4096..4596]);
    assert_eq!(std::fs::read(path).unwrap(), expected);
}

fn origin(name: &str) -> FsOrigin {
    FsOrigin {
        volume: 0,
        folder: Some("DCIM".into()),
        name: name.to_string(),
        state: FsState::Live,
        placement: Placement::Chain,
        size: 0,
        created: None,
        modified: None,
    }
}

fn names_in(dir: &std::path::Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

/// A synthetic image holding two different JPEGs at 4096 and 8192, with a finding for each, named as given.
fn two_files(a: &str, b: &str) -> (Fixture, Finding, Vec<u8>, Vec<u8>) {
    let one = testkit::jpeg(64, 64, 11, 0);
    let two = testkit::jpeg(64, 64, 12, 0);
    let mut disk = DiskImage::noise(MIB, 5);
    disk.put(4096, &one).put(65536, &two);
    let img = disk.save();
    let image = std::fs::read(img.path()).unwrap();
    let source = TestSource {
        inner: FileSource::open(img.path()).unwrap(),
        volume: other(),
        bad: None,
    };
    let mk = |id, start: usize, len: usize, name: &str| Finding {
        id,
        start: start as u64,
        len: len as u64,
        kind: FileKind::Jpeg,
        health: Health::Intact,
        fragments: Vec::new(),
        origin: Some(origin(name)),
    };
    let first = mk(1, 4096, one.len(), a);
    let second = mk(2, 65536, two.len(), b);
    let f = Fixture {
        _img: img,
        source,
        finding: first,
        bytes: one.clone(),
        image,
    };
    (f, second, one, two)
}

#[test]
fn original_names_collide_with_suffixes() {
    let (f, second, one, two) = two_files("DSC00001.JPG", "DSC00001.JPG");
    let dir = tempfile::tempdir().unwrap();
    let a = extract(&f.source, &f.finding, dir.path()).unwrap();
    let b = extract(&f.source, &second, dir.path()).unwrap();
    assert_eq!(a.file_name().unwrap(), "DSC00001.JPG");
    assert_eq!(b.file_name().unwrap(), "DSC00001_2.JPG");
    assert_eq!(std::fs::read(a).unwrap(), one);
    assert_eq!(std::fs::read(b).unwrap(), two);
}

#[test]
fn extract_as_names_the_file_and_falls_back() {
    let f = fixture(other(), None, 0);
    let dir = tempfile::tempdir().unwrap();
    let p = extract_as(&f.source, &f.finding, dir.path(), "my photo.JPG").unwrap();
    assert_eq!(p.file_name().unwrap(), "my photo.JPG");
    let p = extract_as(&f.source, &f.finding, dir.path(), "..").unwrap();
    assert_eq!(p.file_name().unwrap(), "revenant_000000001000.jpg");
    let p = extract_as(&f.source, &f.finding, dir.path(), "../../evil.jpg").unwrap();
    assert_eq!(p.parent().unwrap(), dir.path());
    assert_eq!(p.file_name().unwrap(), "_.._evil.jpg");
}

#[test]
fn case_insensitive_names_do_not_overwrite() {
    // Whether or not the volume folds case, both files must survive with their own bytes.
    for (a, b) in [
        ("DSC00001.JPG", "dsc00001.jpg"),
        ("caf\u{e9}.jpg", "cafe\u{301}.jpg"),
    ] {
        let (f, second, one, two) = two_files(a, b);
        let dir = tempfile::tempdir().unwrap();
        let p1 = extract(&f.source, &f.finding, dir.path()).unwrap();
        let p2 = extract(&f.source, &second, dir.path()).unwrap();
        assert_ne!(p1, p2);
        assert_eq!(std::fs::read(&p1).unwrap(), one);
        assert_eq!(std::fs::read(&p2).unwrap(), two);
        assert_eq!(names_in(dir.path()).len(), 2);
    }
}

fn utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// Scans `bytes` and returns the source, kept alive with its file, and the findings.
fn scan_findings(bytes: &[u8]) -> (tempfile::NamedTempFile, TestSource, Vec<Finding>) {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut file, bytes).unwrap();
    let source = TestSource {
        inner: FileSource::open(file.path()).unwrap(),
        volume: other(),
        bad: None,
    };
    let mut found = Vec::new();
    scan(
        &source,
        &all_formats(),
        &ScanOptions::default(),
        &CancelToken::default(),
        &mut |e| {
            if let Event::Found(f) = e {
                found.push(f);
            }
        },
    )
    .unwrap();
    (file, source, found)
}

#[test]
fn hostile_names_stay_inside_the_destination() {
    use testkit::fs::{FsBuilder, FsType, Partitioning, disk};
    let long = format!("{}.jpg", "a".repeat(250));
    let mut hostile: Vec<Vec<u16>> = [
        "../../x.jpg",
        "/abs/x.jpg",
        "C:\\x.jpg",
        "...jpg",
        "CON.JPG",
        "nul.jpg",
        "..\\..\\x.jpg",
        "COM0.jpg",
        "CON .jpg",
        "x\u{FFFD}.jpg",
        "foto\u{202E}gpj.jpg",
        "a*?\"<>|b.jpg",
        long.as_str(),
    ]
    .iter()
    .map(|n| utf16(n))
    .collect();
    let mut lone = utf16("lone");
    lone.push(0xD800);
    lone.extend(utf16(".jpg"));
    hostile.push(lone);

    for ty in [FsType::Fat32, FsType::Exfat] {
        let mut b = FsBuilder::new(ty, 48 * MIB as u64, 512);
        for (i, name) in hostile.iter().enumerate() {
            b.add_utf16("", name, &testkit::jpeg(64, 48, 100 + i as u64, 0));
        }
        let volume = b.build();
        let bytes = disk(
            Partitioning::Mbr { first_lba: 8192 },
            &volume,
            volume.len() + (8 << 20),
        );
        let (_file, source, found) = scan_findings(&bytes);
        assert_eq!(found.len(), hostile.len(), "{ty:?}");
        let root = tempfile::tempdir().unwrap();
        let dest = root.path().join("dest");
        std::fs::create_dir(&dest).unwrap();
        let before = names_in(root.path());
        for f in &found {
            let p = extract(&source, f, &dest).unwrap();
            assert_eq!(p.parent().unwrap(), dest, "{ty:?} {:?}", f.origin);
        }
        assert_eq!(names_in(&dest).len(), hostile.len(), "{ty:?}");
        assert_eq!(names_in(root.path()), before);
        assert!(
            names_in(&dest)
                .iter()
                .all(|n| n.len() <= 200 && !n.contains(['/', '\\', ':']))
        );
    }
}

#[test]
fn create_named_stays_inside_the_directory() {
    use revenant_core::extract::create_named;
    let f = fixture(other(), None, 0);
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("d");
    std::fs::create_dir(&dir).unwrap();
    let abs = root.path().join("abs").display().to_string();
    for name in ["../x.jpg", abs.as_str(), "a/../../b.jpg", "..", "CON.jpg"] {
        let (p, _file) = create_named(&dir, &f.finding, name, None).unwrap();
        assert_eq!(p.parent().unwrap(), dir, "{name}");
    }
    assert_eq!(names_in(root.path()), vec!["d".to_string()]);
    // A hostile suffix is refused and nothing is created.
    let before = names_in(&dir);
    for suffix in ["/../evil", ".a/b", "x", ".a\\b", ".a\u{0}", ".\u{202E}jpg"] {
        let res = create_named(&dir, &f.finding, "ok.jpg", Some(suffix));
        assert!(matches!(res, Err(EngineError::Write { .. })), "{suffix:?}");
    }
    assert_eq!(names_in(&dir), before);
    assert_eq!(names_in(root.path()), vec!["d".to_string()]);
    let (p, _f) = create_named(&dir, &f.finding, "DSC1.ARW", Some(".preview.jpg")).unwrap();
    assert_eq!(p.file_name().unwrap(), "DSC1.preview.jpg");
}
