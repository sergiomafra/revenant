use revenant_core::source::{FileSource, Source, volume_of};
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
