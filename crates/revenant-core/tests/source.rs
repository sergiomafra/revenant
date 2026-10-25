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

/// What would open something for writing. `GENERIC_WRITE` is allowed once, in `source/windows.rs`, inside
/// `lock_volume_for_dismount` (the Windows volume lock exception, CLAUDE.md).
const BANNED: [&str; 9] = [
    "write(true)",
    ".append(",
    "create(true)",
    "create_new(",
    "GENERIC_WRITE",
    "GENERIC_ALL",
    "FILE_ALL_ACCESS",
    "FILE_WRITE_DATA",
    "FILE_APPEND_DATA",
];
const EXCEPTION_FN: &str = "fn lock_volume_for_dismount(";
const EXCEPTION_MARK: &str = "WRITE-ACCESS EXCEPTION (CLAUDE.md)";

/// Every banned string `text` holds, except the one write-access exception when `exception` allows it.
fn write_paths(text: &str, exception: bool) -> Vec<String> {
    let mut found: Vec<String> = BANNED
        .iter()
        .filter(|b| **b != "GENERIC_WRITE" && text.contains(*b))
        .map(|b| b.to_string())
        .collect();
    let lines: Vec<&str> = text.lines().collect();
    let writes: Vec<usize> = (0..lines.len())
        .filter(|&i| lines[i].contains("GENERIC_WRITE"))
        .collect();
    let allowed = exception
        && writes.len() == 1
        && text.matches("GENERIC_WRITE").count() == 1
        && lines
            .iter()
            .position(|l| l.contains(EXCEPTION_FN))
            .is_some_and(|f| {
                // The function ends at its own closing brace: the first line that is just `}`.
                let end = lines[f + 1..]
                    .iter()
                    .position(|l| *l == "}")
                    .map_or(f, |n| f + 1 + n);
                f > 0 && lines[f - 1].contains(EXCEPTION_MARK) && (f..end).contains(&writes[0])
            });
    if !writes.is_empty() && !allowed {
        found.push(format!("GENERIC_WRITE on lines {writes:?}"));
    }
    found
}

#[test]
fn source_has_no_write_path() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files: Vec<PathBuf> = Vec::new();
    for dir in ["source", "device"] {
        for entry in std::fs::read_dir(root.join(dir)).unwrap() {
            files.push(entry.unwrap().path());
        }
    }
    // P14: every module that touches a source; one that does not exist yet is covered once it does.
    files.extend(
        ["identity.rs", "preflight.rs"]
            .iter()
            .map(|f| root.join(f))
            .filter(|p| p.exists()),
    );
    let windows = root.join("source/windows.rs");
    let mut checked = 0;
    for path in &files {
        let text = std::fs::read_to_string(path).unwrap();
        let found = write_paths(&text, *path == windows);
        assert!(found.is_empty(), "{} contains {found:?}", path.display());
        if *path == windows {
            // The exception exists, in its one place (a refactor that drops the marker fails here).
            assert_eq!(
                text.matches("GENERIC_WRITE").count(),
                1,
                "{}",
                path.display()
            );
        }
        checked += 1;
    }
    assert!(files.contains(&windows) && files.contains(&root.join("identity.rs")));
    assert!(checked >= 12, "{checked}");
}

#[test]
fn write_path_check_finds_misplaced_write_access() {
    let mark = format!("// {EXCEPTION_MARK}");
    let good = format!(
        "use x;\n{mark}\npub(crate) {EXCEPTION_FN}v: &str) {{\n    open(GENERIC_WRITE);\n}}\n\nfn other() {{}}\n"
    );
    assert!(write_paths(&good, true).is_empty());
    // Anywhere but the exception's file, or without the exception allowed, it is a write path.
    assert!(!write_paths(&good, false).is_empty());
    // Without the mark right above the function.
    let unmarked = good.replace(&mark, "// lock");
    assert!(!write_paths(&unmarked, true).is_empty());
    // Outside the function (after the next top-level item), or imported at the top.
    let outside = good.replace("    open(GENERIC_WRITE);", "    open(0);")
        + "pub fn more() { open(GENERIC_WRITE); }\n";
    assert!(!write_paths(&outside, true).is_empty());
    let imported = format!("use y::GENERIC_WRITE;\n{good}");
    assert!(!write_paths(&imported, true).is_empty());
    // Twice in the function, even on one line.
    let twice = good.replace("open(GENERIC_WRITE)", "open(GENERIC_WRITE | GENERIC_WRITE)");
    assert!(!write_paths(&twice, true).is_empty());
    // Right after the function's closing brace, before the next `fn`: outside it.
    let after = good
        .replace("    open(GENERIC_WRITE);", "    open(0);")
        .replace(
            "}\n\nfn other",
            "}\nconst W: u32 = GENERIC_WRITE;\n\nfn other",
        );
    assert!(!write_paths(&after, true).is_empty());
    // Other access rights that write are banned everywhere.
    for right in [
        "GENERIC_ALL",
        "FILE_ALL_ACCESS",
        "FILE_WRITE_DATA",
        "FILE_APPEND_DATA",
    ] {
        let other = good.replace(
            "open(GENERIC_WRITE)",
            &format!("open(GENERIC_WRITE | {right})"),
        );
        assert_eq!(write_paths(&other, true), [right]);
    }
    // The other banned strings are never excepted.
    let create = good.replace("open(GENERIC_WRITE)", "open(GENERIC_WRITE).create(true)");
    assert_eq!(write_paths(&create, true), ["create(true)"]);
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

#[test]
fn fragmented_view_clamps_pieces_to_the_source() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("image.bin");
    let src: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(&path, &src).unwrap();
    let file = FileSource::open(&path).unwrap();
    // The first piece runs 104 bytes past the end; the last starts past it.
    let view = Fragmented::new(
        &file,
        vec![
            Fragment {
                start: 4000,
                len: 200,
            },
            Fragment { start: 50, len: 20 },
            Fragment {
                start: 5000,
                len: 10,
            },
        ],
    );
    assert_eq!(view.len(), 96 + 20);
    let mut all = [0u8; 200];
    assert_eq!(view.read_at(0, &mut all).unwrap(), 116);
    assert_eq!(all[..96], src[4000..]);
    assert_eq!(all[96..116], src[50..70]);
    assert_eq!(
        view.absolute(90, 100),
        vec![
            Fragment {
                start: 4090,
                len: 6
            },
            Fragment { start: 50, len: 20 },
        ]
    );
}
