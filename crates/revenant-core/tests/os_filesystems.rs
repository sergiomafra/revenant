//! Opt-in: volumes of images that macOS itself formatted (`hdiutil create`) and wrote to (attached by `OsImage` only).
//! Run with `REVENANT_OS_FS=1 cargo test -p revenant-core --test os_filesystems -- --ignored --test-threads=1` (one at
//! a time: every image's volume is named `RVT`, and a second one mounted at once would land on `/Volumes/RVT 1`).

use std::collections::BTreeMap;
use std::path::Path;

use revenant_core::filesystem::{FsKind, FsState, read, volumes};
use revenant_core::formats::{Health, all_formats};
use revenant_core::scan::{CancelToken, Event, Finding, ScanOptions, scan};
use revenant_core::source::{FileSource, Source, VolumeId};
use revenant_core::{EngineError, extract};
use revenant_testkit::os_fs::{OsFs, OsImage, OsLayout, available};
use revenant_testkit::{Mp4Options, TestExif, TiffRawOptions, TiffVariant, jpeg, mp4, tiff_raw};

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
    lists_live_and_deleted(&[
        (OsFs::Fat32, OsLayout::Mbr, 64),
        (OsFs::Fat16, OsLayout::None, 32),
    ]);
}

#[test]
#[ignore]
fn os_made_exfat_lists_live_and_deleted() {
    lists_live_and_deleted(&[
        (OsFs::Exfat, OsLayout::Mbr, 64),
        (OsFs::Exfat, OsLayout::Gpt, 64),
    ]);
}

/// On each image the OS formats: six photos written to `DCIM/100TEST`, two deleted, all through the OS driver. Every
/// live photo is listed live with the bytes the mount reads and trusted; each deleted one is listed once, by its name,
/// with its original bytes.
fn lists_live_and_deleted(cases: &[(OsFs, OsLayout, u32)]) {
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
    for &(os, layout, size) in cases {
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
                        // The long name exactly; on FAT DSC00003's first letter from its live siblings DSC0000x.
                        && f.origin.name() == n
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

/// The image file seen with no volume, so `extract` may write to a temp dir on the same disk.
struct NoVolume<'a>(&'a FileSource);

impl Source for NoVolume<'_> {
    fn len(&self) -> u64 {
        self.0.len()
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, EngineError> {
        self.0.read_at(offset, buf)
    }
    fn volume(&self) -> Option<VolumeId> {
        None
    }
}

/// On each image the OS formats, through the mount: a folder of photos (one with a long name), a RAW and a video; then a
/// 3 MiB file X, a small Y, X deleted, a 5 MiB Z (likely written in pieces), and two photos deleted. After detaching,
/// the whole scan: every live file is found once with its path, `Intact`, and extracts to the bytes written (Z whatever
/// its pieces); every deleted photo whose clusters were not reused is found `Deleted` and `Intact`, with its name but
/// for the first character, and extracts to the original bytes.
#[test]
#[ignore]
fn os_made_cards_scan_end_to_end() {
    if !available() {
        return;
    }
    let arw = tiff_raw(&TiffRawOptions {
        variant: TiffVariant::Arw,
        big_endian: false,
        raw_len: 200_000,
        seed: 9,
        preview: Some((160, 120)),
        preview_sector_aligned: false,
        exif: TestExif::default(),
    })
    .bytes;
    let video = mp4(&Mp4Options {
        mdat_len: 300_000,
        moov_at_end: false,
        mov: false,
        mdat_64bit: false,
    });
    let files: Vec<(&str, Vec<u8>)> = vec![
        ("DSC00001.JPG", jpeg(320, 240, 61, 0)),
        ("DSC00002.JPG", jpeg(320, 240, 62, 0)),
        ("DSC00003.JPG", jpeg(320, 240, 63, 0)),
        ("Long name photo.jpg", jpeg(320, 240, 64, 0)),
        ("DSC00005.ARW", arw),
        ("MOV00006.MP4", video),
        ("X0000007.JPG", jpeg(1500, 1100, 65, 0)),
        ("Y0000008.JPG", jpeg(800, 600, 66, 0)),
    ];
    let z = ("Z0000009.JPG", jpeg(1900, 1500, 67, 0));
    let deleted_photos = ["DSC00002.JPG", "Long name photo.jpg"];
    for (os, layout, size) in [
        (OsFs::Fat32, OsLayout::Mbr, 64),
        (OsFs::Exfat, OsLayout::Mbr, 64),
        (OsFs::Exfat, OsLayout::Gpt, 64),
        (OsFs::Fat16, OsLayout::None, 32),
    ] {
        let case = format!("{os:?} {layout:?}");
        let img = OsImage::create(os, layout, size).expect("hdiutil create");
        img.with_mounted(|m| {
            let dir = m.join("DCIM/100TEST");
            std::fs::create_dir_all(&dir).unwrap();
            for (n, data) in &files[..6] {
                std::fs::write(dir.join(n), data).unwrap();
            }
            std::fs::write(dir.join(files[6].0), &files[6].1).unwrap();
            std::fs::write(dir.join(files[7].0), &files[7].1).unwrap();
            std::fs::remove_file(dir.join(files[6].0)).unwrap();
            std::fs::write(dir.join(z.0), &z.1).unwrap();
            for n in deleted_photos {
                std::fs::remove_file(dir.join(n)).unwrap();
            }
        });
        let live: Vec<(&str, &Vec<u8>)> = files
            .iter()
            .filter(|(n, _)| !deleted_photos.contains(n) && *n != files[6].0)
            .map(|(n, d)| (*n, d))
            .chain([(z.0, &z.1)])
            .collect();

        let file = FileSource::open(&img.path).unwrap();
        let source = NoVolume(&file);
        let mut found: BTreeMap<u64, Finding> = BTreeMap::new();
        scan(
            &source,
            &all_formats(),
            &ScanOptions::default(),
            &CancelToken::default(),
            &mut |e| match e {
                Event::Found(f) | Event::Updated(f) => {
                    found.insert(f.id, f);
                }
                _ => {}
            },
        )
        .unwrap();
        let out = tempfile::tempdir().unwrap();
        let bytes_of =
            |f: &Finding| std::fs::read(extract(&source, f, out.path()).unwrap()).unwrap();
        let paths = || {
            found
                .values()
                .map(|f| f.origin.as_ref().map(|o| o.path()))
                .collect::<Vec<_>>()
        };

        for (n, want) in &live {
            let path = format!("DCIM/100TEST/{n}");
            let hits: Vec<_> = found
                .values()
                .filter(|f| {
                    f.origin
                        .as_ref()
                        .is_some_and(|o| o.path() == path && o.state == FsState::Live)
                })
                .collect();
            assert_eq!(hits.len(), 1, "{case}: {path} in {:?}", paths());
            assert_eq!(hits[0].health, Health::Intact, "{case}: {path}");
            assert_eq!(&bytes_of(hits[0]), *want, "{case}: {path}");
        }
        for n in deleted_photos {
            let want = &files.iter().find(|(x, _)| *x == n).unwrap().1;
            let tail: String = n.chars().skip(1).collect();
            let hits: Vec<_> = found
                .values()
                .filter(|f| {
                    f.origin.as_ref().is_some_and(|o| {
                        o.state == FsState::Deleted
                            && o.folder() == Some("DCIM/100TEST")
                            && o.name().chars().skip(1).collect::<String>() == tail
                    })
                })
                .collect();
            assert_eq!(hits.len(), 1, "{case}: {n} in {:?}", paths());
            assert_eq!(hits[0].health, Health::Intact, "{case}: {n}");
            assert_eq!(&bytes_of(hits[0]), want, "{case}: {n}");
        }
    }
}
