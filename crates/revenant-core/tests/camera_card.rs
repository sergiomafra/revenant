//! `looks_like_camera_card`: a mounted volume with a `DCIM` folder at its root. Only the mounted folder is looked at.

mod support;

use std::path::Path;

use revenant_core::device::{DiskInfo, VolumeInfo, looks_like_camera_card};

fn disk_with(mount_point: Option<&Path>) -> DiskInfo {
    let mut disk = support::disk_info("disk4", 1 << 30);
    disk.volumes = vec![VolumeInfo {
        id: "disk4s1".into(),
        fs: Some("exfat".into()),
        label: None,
        mount_point: mount_point.map(Path::to_path_buf),
        size: 1 << 30,
    }];
    disk
}

#[test]
fn dcim_folder_is_a_camera_card() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("DCIM")).unwrap();
    assert!(looks_like_camera_card(&disk_with(Some(dir.path()))));
}

#[test]
fn dcim_in_any_case_is_a_camera_card() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("dcim")).unwrap();
    assert!(looks_like_camera_card(&disk_with(Some(dir.path()))));
}

#[test]
fn no_dcim_is_not_a_camera_card() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("Documents")).unwrap();
    assert!(!looks_like_camera_card(&disk_with(Some(dir.path()))));
}

#[test]
fn a_plain_file_named_dcim_is_not_a_camera_card() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("DCIM"), b"x").unwrap();
    assert!(!looks_like_camera_card(&disk_with(Some(dir.path()))));
}

#[test]
fn unmounted_volume_is_not_a_camera_card() {
    assert!(!looks_like_camera_card(&disk_with(None)));
}

#[test]
fn missing_mount_point_is_not_a_camera_card() {
    let dir = tempfile::tempdir().unwrap();
    let gone = dir.path().join("gone");
    assert!(!looks_like_camera_card(&disk_with(Some(&gone))));
}
