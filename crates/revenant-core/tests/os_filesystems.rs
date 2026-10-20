//! Opt-in: volumes of images that macOS itself formatted (`hdiutil create`) and wrote to (attached by `OsImage` only).
//! Run with `REVENANT_OS_FS=1 cargo test -p revenant-core --test os_filesystems -- --ignored`.

use std::collections::BTreeMap;
use std::path::Path;

use revenant_core::filesystem::{FsKind, FsState, read, volumes};
use revenant_core::scan::CancelToken;
use revenant_core::source::FileSource;
use revenant_testkit::jpeg;
use revenant_testkit::os_fs::{OsFs, OsImage, OsLayout, available};

/// The first LBA of the image's only partition, read here from its MBR entry or its GPT entry (0 without a table).
fn first_lba(bytes: &[u8], layout: OsLayout) -> u64 {
    let u32_at = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as u64;
    let u64_at = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
    match layout {
        OsLayout::None => 0,
        OsLayout::Mbr => u32_at(446 + 8),
        OsLayout::Gpt => {
            assert_eq!(&bytes[512..520], b"EFI PART");
            u64_at(u64_at(512 + 72) as usize * 512 + 32)
        }
    }
}

#[test]
#[ignore]
fn os_made_volumes_are_found() {
    if !available() {
        return;
    }
    for (os, layout, size, fs) in [
        (OsFs::Fat32, OsLayout::Mbr, 64, FsKind::Fat32),
        (OsFs::Exfat, OsLayout::Gpt, 64, FsKind::Exfat),
        (OsFs::Fat16, OsLayout::None, 32, FsKind::Fat16),
    ] {
        let img = OsImage::create(os, layout, size).expect("hdiutil create");
        let bytes = std::fs::read(&img.path).unwrap();
        let lba = first_lba(&bytes, layout);
        let v = volumes(&FileSource::open(&img.path).unwrap());
        assert_eq!(v.len(), 1, "{os:?} {layout:?}: {v:?}");
        assert_eq!(v[0].fs, fs, "{os:?} {layout:?}");
        assert_eq!(v[0].offset, 512 * lba, "{os:?} {layout:?}");
        assert_eq!(v[0].label.as_deref(), Some("RVT"), "{os:?} {layout:?}");
        assert!(!v[0].from_backup);
    }
}

/// Every regular file under `dir`, by path relative to `root` (`/`-separated).
fn files_under(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
    for e in std::fs::read_dir(dir).unwrap() {
        let e = e.unwrap();
        let ty = e.file_type().unwrap();
        if ty.is_dir() {
            files_under(root, &e.path(), out);
        } else if ty.is_file() {
            let rel = e
                .path()
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            out.insert(rel, std::fs::read(e.path()).unwrap());
        }
    }
}

#[test]
#[ignore]
fn os_made_fat_lists_live_and_deleted() {
    if !available() {
        return;
    }
    let names = [
        "DSC00001.JPG",
        "DSC00002.JPG",
        "DSC00003.JPG",
        "Long name photo.jpg",
        "DSC00005.JPG",
        "DSC00006.JPG",
    ];
    let gone = ["DSC00003.JPG", "Long name photo.jpg"];
    for (os, layout, size) in [
        (OsFs::Fat32, OsLayout::Mbr, 64),
        (OsFs::Fat16, OsLayout::None, 32),
    ] {
        let img = OsImage::create(os, layout, size).expect("hdiutil create");
        let photos: Vec<Vec<u8>> = (0..names.len())
            .map(|i| jpeg(320, 240, 50 + i as u64, 0))
            .collect();
        let mut mounted = BTreeMap::new();
        img.with_mounted(|m| {
            let dir = m.join("DCIM/100TEST");
            std::fs::create_dir_all(&dir).unwrap();
            for (n, data) in names.iter().zip(&photos) {
                std::fs::write(dir.join(n), data).unwrap();
            }
            for n in gone {
                std::fs::remove_file(dir.join(n)).unwrap();
            }
            files_under(m, m, &mut mounted);
        });
        let image = std::fs::read(&img.path).unwrap();
        let l = read(
            &FileSource::open(&img.path).unwrap(),
            &CancelToken::default(),
        );
        let bytes_of = |f: &revenant_core::filesystem::FsFile| -> Vec<u8> {
            f.pieces
                .iter()
                .flat_map(|p| {
                    image[p.start as usize..(p.start + p.len) as usize]
                        .iter()
                        .copied()
                })
                .collect()
        };
        let live: Vec<_> = l
            .files
            .iter()
            .filter(|f| f.origin.state == FsState::Live)
            .collect();
        for n in names.iter().filter(|n| !gone.contains(n)) {
            let path = format!("DCIM/100TEST/{n}");
            assert!(
                live.iter().any(|f| f.origin.path() == path),
                "{os:?}: {path} not listed live"
            );
        }
        for f in &live {
            let path = f.origin.path();
            let want = mounted
                .get(&path)
                .unwrap_or_else(|| panic!("{os:?}: {path} not on the mount"));
            assert_eq!(&bytes_of(f), want, "{os:?}: {path}");
            assert!(f.trusted, "{os:?}: {path}");
        }
        for n in gone {
            let i = names.iter().position(|x| *x == n).unwrap();
            let found: Vec<_> = l
                .files
                .iter()
                .filter(|f| {
                    f.origin.state == FsState::Deleted
                        && f.origin.folder() == Some("DCIM/100TEST")
                        && if n.len() > 12 {
                            f.origin.name() == n
                        } else {
                            f.origin.name()[1..] == n[1..]
                        }
                })
                .collect();
            assert_eq!(
                found.len(),
                1,
                "{os:?}: {n} in {:?}",
                l.files.iter().map(|f| f.origin.path()).collect::<Vec<_>>()
            );
            assert_eq!(bytes_of(found[0]), photos[i], "{os:?}: {n}");
        }
    }
}
