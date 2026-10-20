use revenant_core::source::{FileSource, Fragmented, Source, VolumeId, volume_of};
use revenant_core::{EngineError, Fragment};
use std::path::{Path, PathBuf};

fn make_file(dir: &Path) -> PathBuf {
    let path = dir.join("image.bin");
    let data: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(&path, data).unwrap();
    path
}

#[test]
fn reads_middle_and_end() {
    let dir = tempfile::tempdir().unwrap();
    let path = make_file(dir.path());
    let s = FileSource::open(&path).unwrap();
    assert_eq!(s.len(), 1000);
    let mut b = [0u8; 10];
    assert_eq!(s.read_at(500, &mut b).unwrap(), 10);
    assert_eq!(b[0], (500 % 251) as u8);
    assert_eq!(s.read_at(995, &mut b).unwrap(), 5);
    assert_eq!(s.read_at(1000, &mut b).unwrap(), 0);
    assert_eq!(s.read_at(5000, &mut b).unwrap(), 0);
}

#[test]
fn never_modifies_the_source() {
    let dir = tempfile::tempdir().unwrap();
    let path = make_file(dir.path());
    let before = std::fs::read(&path).unwrap();
    let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
    {
        let s = FileSource::open(&path).unwrap();
        let mut buf = vec![0u8; 256];
        let mut off = 0;
        while s.read_at(off, &mut buf).unwrap() > 0 {
            off += buf.len() as u64;
        }
    }
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), mtime);
}

#[test]
fn opens_read_only_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = make_file(dir.path());
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&path, perms).unwrap();
    let s = FileSource::open(&path).unwrap();
    assert_eq!(s.len(), 1000);
}

#[test]
fn same_volume_for_file_and_its_dir() {
    let dir = tempfile::tempdir().unwrap();
    let path = make_file(dir.path());
    assert_eq!(
        volume_of(dir.path()).unwrap(),
        FileSource::open(&path).unwrap().volume().unwrap()
    );
}

#[test]
fn opening_missing_file_gives_code() {
    let missing = tempfile::tempdir().unwrap().path().join("does-not-exist");
    assert_eq!(
        FileSource::open(&missing).err().unwrap().code(),
        "open_failed"
    );
}

#[test]
fn source_has_no_write_path() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/source");
    let mut checked = 0;
    for entry in std::fs::read_dir(src).unwrap() {
        let path = entry.unwrap().path();
        let text = std::fs::read_to_string(&path).unwrap();
        for banned in [
            "write(true)",
            ".append(",
            "create(true)",
            "create_new(",
            "GENERIC_WRITE",
        ] {
            assert!(
                !text.contains(banned),
                "{} contains {banned}",
                path.display()
            );
        }
        checked += 1;
    }
    assert!(checked >= 3);
}

/// Fails every read that touches `[bad, bad + 1)`, reporting the first bad byte as the error offset.
struct FailsAt {
    inner: FileSource,
    bad: u64,
}

impl Source for FailsAt {
    fn len(&self) -> u64 {
        self.inner.len()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, EngineError> {
        if offset <= self.bad && self.bad < offset + buf.len() as u64 {
            return Err(EngineError::Read {
                offset: self.bad,
                cause: std::io::Error::other("injected"),
            });
        }
        self.inner.read_at(offset, buf)
    }

    fn volume(&self) -> Option<VolumeId> {
        Some(VolumeId(7))
    }
}

#[test]
fn fragmented_view_maps_reads_and_errors() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("image.bin");
    let src: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(&path, &src).unwrap();
    let file = FileSource::open(&path).unwrap();
    let pieces = vec![
        Fragment {
            start: 100,
            len: 10,
        },
        Fragment { start: 50, len: 20 },
    ];
    let view = Fragmented::new(&file, pieces.clone());
    assert_eq!(view.len(), 30);
    assert_eq!(view.volume(), file.volume());

    let mut b = [0u8; 10];
    assert_eq!(view.read_at(5, &mut b).unwrap(), 10);
    assert_eq!(b[..5], src[105..110]);
    assert_eq!(b[5..], src[50..55]);
    let mut all = [0u8; 30];
    assert_eq!(view.read_at(0, &mut all).unwrap(), 30);
    assert_eq!(all[..10], src[100..110]);
    assert_eq!(all[10..], src[50..70]);
    // Past the logical end: short, then nothing.
    assert_eq!(view.read_at(25, &mut b).unwrap(), 5);
    assert_eq!(b[..5], src[65..70]);
    assert_eq!(view.read_at(30, &mut b).unwrap(), 0);
    assert_eq!(view.read_at(u64::MAX, &mut b).unwrap(), 0);

    assert_eq!(
        view.absolute(5, 10),
        vec![
            Fragment { start: 105, len: 5 },
            Fragment { start: 50, len: 5 },
        ]
    );
    assert_eq!(view.absolute(0, 30), pieces);
    assert_eq!(view.absolute(12, 3), vec![Fragment { start: 52, len: 3 }]);
    assert_eq!(
        view.absolute(20, 100),
        vec![Fragment { start: 60, len: 10 }]
    );
    assert_eq!(view.absolute(30, 5), vec![]);
    // Pieces that touch in the source merge back into one.
    let touching = Fragmented::new(
        &file,
        vec![
            Fragment { start: 0, len: 512 },
            Fragment {
                start: 512,
                len: 512,
            },
        ],
    );
    assert_eq!(
        touching.absolute(100, 800),
        vec![Fragment {
            start: 100,
            len: 800
        }]
    );

    let failing = FailsAt {
        inner: FileSource::open(&path).unwrap(),
        bad: 60,
    };
    let view = Fragmented::new(&failing, pieces);
    assert_eq!(view.volume(), Some(VolumeId(7)));
    match view.read_at(0, &mut all) {
        Err(EngineError::Read { offset, .. }) => assert_eq!(offset, 60),
        other => panic!("expected a read error, got {other:?}"),
    }
    // A read that misses the bad byte succeeds.
    assert_eq!(view.read_at(0, &mut b).unwrap(), 10);
}
