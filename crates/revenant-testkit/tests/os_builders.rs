//! Opt-in: the synthetic FAT/exFAT volumes, read back by the real macOS drivers.
//! Run with `REVENANT_OS_FS=1 cargo test -p revenant-testkit --test os_builders -- --ignored`.

use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::Command;

use revenant_testkit::fs::{DeleteStyle, FsBuilder, FsStamp, FsType, Partitioning, disk};
use revenant_testkit::jpeg;
use revenant_testkit::os_fs::{OsFs, OsImage, OsLayout, available, fsck, with_raw_mounted};

const MIB: usize = 1 << 20;

/// Local wall-clock "YYYY-MM-DD HH:MM:SS" of a Unix time, from the system `date`.
fn local_time(secs: i64) -> String {
    let out = Command::new("/bin/date")
        .args(["-r", &secs.to_string(), "+%Y-%m-%d %H:%M:%S"])
        .output()
        .expect("date");
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with('.'))
        .collect();
    v.sort();
    v
}

#[test]
#[ignore]
fn builder_images_mount_on_macos() {
    if !available() {
        return;
    }
    let cases = [
        (FsType::Fat12, 2 * MIB, 1024u32, OsFs::Fat16),
        (FsType::Fat16, 16 * MIB, 2048, OsFs::Fat16),
        (FsType::Fat32, 48 * MIB, 512, OsFs::Fat32),
        (FsType::Exfat, 16 * MIB, 32 * 1024, OsFs::Exfat),
    ];
    for (fs, len, cluster, os) in cases {
        let jpeg1 = jpeg(320, 240, 1, 0);
        let long = jpeg(320, 240, 2, 0);
        let frag = jpeg(320, 240, 3, 3000);
        let gone = jpeg(320, 240, 4, 0);
        let mut b = FsBuilder::new(fs, len as u64, cluster);
        b.mkdir("DCIM");
        b.mkdir("DCIM/100TEST");
        b.add("DCIM/100TEST/DSC00001.JPG", &jpeg1);
        b.add("DCIM/100TEST/Long name photo.jpg", &long);
        // DSC00003.JPG in two runs with a gap between them.
        let first = b.add("DCIM/100TEST/filler.bin", &vec![7u8; 4 * cluster as usize]);
        b.delete("DCIM/100TEST/filler.bin", DeleteStyle::Camera);
        let base = first[0];
        let per = (frag.len() as u32).div_ceil(cluster);
        let half = per / 2;
        let mut clusters: Vec<u32> = (0..half).map(|i| base + i).collect();
        clusters.extend((half..per).map(|i| base + 8 + i));
        b.add_at("DCIM/100TEST/DSC00003.JPG", &frag, &clusters);
        b.add("DCIM/100TEST/DSC00004.JPG", &gone);
        b.delete("DCIM/100TEST/DSC00004.JPG", DeleteStyle::Camera);
        let stamp = FsStamp {
            year: 2025,
            month: 12,
            day: 14,
            hour: 7,
            min: 12,
            sec: 6,
            utc_offset_min: None,
        };
        b.stamp("DCIM/100TEST/DSC00001.JPG", stamp, stamp);
        let volume = b.build();
        let image = disk(
            Partitioning::Mbr { first_lba: 2048 },
            &volume,
            len + 4 * MIB,
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("card.img");
        std::fs::write(&path, &image).unwrap();

        with_raw_mounted(&path, |mount| {
            let d = mount.join("DCIM/100TEST");
            assert_eq!(
                names(&d),
                ["DSC00001.JPG", "DSC00003.JPG", "Long name photo.jpg"],
                "{fs:?}"
            );
            assert_eq!(
                std::fs::read(d.join("DSC00001.JPG")).unwrap(),
                jpeg1,
                "{fs:?}"
            );
            assert_eq!(
                std::fs::read(d.join("DSC00003.JPG")).unwrap(),
                frag,
                "{fs:?}"
            );
            assert_eq!(
                std::fs::read(d.join("Long name photo.jpg")).unwrap(),
                long,
                "{fs:?}"
            );
            let m = std::fs::metadata(d.join("DSC00001.JPG")).unwrap();
            assert_eq!(local_time(m.mtime()), "2025-12-14 07:12:06", "{fs:?}");
        });
        match fsck(&path, os) {
            Some(clean) => assert!(clean, "{fs:?}: fsck reported problems"),
            None => eprintln!("{fs:?}: fsck not run"),
        }
    }
}

/// The OS-formatted image helper itself: create, mount read-write, write a file, mount the raw bytes read-only.
#[test]
#[ignore]
fn os_formatted_image_round_trips() {
    if !available() {
        return;
    }
    let Some(img) = OsImage::create(OsFs::Fat32, OsLayout::Mbr, 40) else {
        eprintln!("skipped: hdiutil create failed");
        return;
    };
    img.with_mounted(|m| std::fs::write(m.join("HELLO.TXT"), b"hi").unwrap());
    with_raw_mounted(&img.path, |m| {
        assert_eq!(std::fs::read(m.join("HELLO.TXT")).unwrap(), b"hi");
    });
    assert_eq!(fsck(&img.path, OsFs::Fat32), Some(true));
}
