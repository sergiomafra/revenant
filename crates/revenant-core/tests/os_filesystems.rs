//! Opt-in: volumes of images that macOS itself formatted (`hdiutil create`, never attached here).
//! Run with `REVENANT_OS_FS=1 cargo test -p revenant-core --test os_filesystems -- --ignored`.

use revenant_core::filesystem::{FsKind, volumes};
use revenant_core::source::FileSource;
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
