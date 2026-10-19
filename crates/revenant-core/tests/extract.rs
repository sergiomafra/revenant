use revenant_core::formats::{FileKind, Health};
use revenant_core::source::{FileSource, Source, VolumeId, volume_of};
use revenant_core::{EngineError, Finding, extract};
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
