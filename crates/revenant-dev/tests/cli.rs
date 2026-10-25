use assert_cmd::Command;
use revenant_testkit::fs::{FsBuilder, FsStamp, FsType, Partitioning, disk};
use revenant_testkit::{
    DiskImage, Mp4Options, TestExif, TiffRaw, TiffRawOptions, TiffVariant, jpeg, jpeg_with_exif,
    mp4, tiff_raw,
};

fn image() -> (tempfile::NamedTempFile, Vec<u8>, Vec<u8>) {
    let photo = jpeg(64, 48, 1, 0);
    let video = mp4(&Mp4Options {
        mdat_len: 3000,
        moov_at_end: false,
        mov: false,
        mdat_64bit: false,
    });
    let mut img = DiskImage::noise(256 * 1024, 7);
    img.put(0, &photo).put(64 * 1024, &video);
    (img.save(), photo, video)
}

fn lines(out: &[u8]) -> Vec<serde_json::Value> {
    String::from_utf8(out.to_vec())
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).expect("stdout line is JSON"))
        .collect()
}

#[test]
fn scan_lists_findings_as_json() {
    let (img, _, _) = image();
    let out = Command::cargo_bin("revenant-dev")
        .unwrap()
        .arg("scan")
        .arg(img.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let events = lines(&out);
    let found: Vec<_> = events.iter().filter(|e| e["event"] == "found").collect();
    assert_eq!(found.len(), 2);
    assert_eq!(found[0]["id"], 1);
    assert_eq!(found[0]["start"], 0);
    assert_eq!(found[0]["kind"], "jpeg");
    assert_eq!(found[0]["health"], "intact");
    assert_eq!(found[1]["kind"], "mp4");
    let summary = events.last().unwrap();
    assert_eq!(summary["event"], "summary");
    assert_eq!(summary["found"], 2);
    assert_eq!(summary["unreadable_bytes"], 0);
}

#[test]
fn extract_writes_files() {
    let (img, photo, video) = image();
    let dest = tempfile::tempdir().unwrap();
    let out = Command::cargo_bin("revenant-dev")
        .unwrap()
        .arg("scan")
        .arg(img.path())
        .arg("--extract")
        .arg(dest.path())
        .arg("--allow-same-volume")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let events = lines(&out);
    let files: Vec<_> = events
        .iter()
        .filter(|e| e["event"] == "found")
        .map(|e| std::fs::read(e["file"].as_str().expect("file field")).unwrap())
        .collect();
    assert_eq!(files, vec![photo, video]);
}

#[test]
fn extract_to_same_volume_is_refused() {
    let (img, _, _) = image();
    let dest = tempfile::tempdir().unwrap();
    Command::cargo_bin("revenant-dev")
        .unwrap()
        .arg("scan")
        .arg(img.path())
        .arg("--extract")
        .arg(dest.path())
        .assert()
        .code(1)
        .stderr(predicates::str::contains("error: destination_on_source"));
    assert_eq!(std::fs::read_dir(dest.path()).unwrap().count(), 0);
}

#[test]
fn missing_image_exits_with_code() {
    Command::cargo_bin("revenant-dev")
        .unwrap()
        .args(["scan", "/definitely/not/here.img"])
        .assert()
        .code(1)
        .stderr(predicates::str::contains("error: open_failed"));
}

#[test]
fn missing_extract_folder_fails_before_scanning() {
    let (img, _, _) = image();
    let dest = tempfile::tempdir().unwrap();
    let file = dest.path().join("a-file");
    std::fs::write(&file, b"").unwrap();
    for bad in [dest.path().join("not-created"), file] {
        let out = Command::cargo_bin("revenant-dev")
            .unwrap()
            .arg("scan")
            .arg(img.path())
            .arg("--extract")
            .arg(&bad)
            .arg("--allow-same-volume")
            .assert()
            .code(1)
            .stderr(predicates::str::contains("error: open_failed"))
            .get_output()
            .clone();
        assert!(out.stdout.is_empty(), "{bad:?}: nothing is scanned");
        assert!(
            !String::from_utf8_lossy(&out.stderr).contains("scanned"),
            "{bad:?}: no progress"
        );
    }
}

/// Per-finding extraction failures: the folder exists but files cannot be created in it (read-only directory). Unix
/// only: a read-only attribute on a Windows directory does not prevent creating files in it.
#[cfg(unix)]
#[test]
fn extract_failure_is_reported_per_finding_and_scan_continues() {
    use std::os::unix::fs::PermissionsExt;
    let (img, _, _) = image();
    let dest = tempfile::tempdir().unwrap();
    let locked = dest.path().join("read-only");
    std::fs::create_dir(&locked).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
    let out = Command::cargo_bin("revenant-dev")
        .unwrap()
        .arg("scan")
        .arg(img.path())
        .arg("--extract")
        .arg(&locked)
        .arg("--allow-same-volume")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    let events = lines(&out);
    let found: Vec<_> = events.iter().filter(|e| e["event"] == "found").collect();
    assert_eq!(found.len(), 2);
    for f in &found {
        assert_eq!(f["error"], "write_failed");
        assert!(f.get("file").is_none());
    }
    assert_eq!(events.last().unwrap()["event"], "summary");
    assert_eq!(events.last().unwrap()["found"], 2);
}

fn exif() -> TestExif {
    TestExif {
        make: Some("Acme".into()),
        model: Some("Model X".into()),
        lens: Some("Lens 50mm f/1.8".into()),
        serial: Some("SN12345".into()),
        iso: Some(400),
        date: Some("2024:05:06 07:08:09".into()),
    }
}

/// A CR2 with EXIF and a 320x240 preview at 8192 in a noise image.
fn raw_image() -> (tempfile::NamedTempFile, TiffRaw) {
    let raw = tiff_raw(&TiffRawOptions {
        variant: TiffVariant::Cr2,
        big_endian: false,
        raw_len: 20_000,
        seed: 5,
        preview: Some((320, 240)),
        preview_sector_aligned: false,
        exif: exif(),
    });
    let mut img = DiskImage::noise(8192 + raw.bytes.len() + 8192, 11);
    img.put(8192, &raw.bytes);
    (img.save(), raw)
}

#[test]
fn found_lines_carry_metadata_and_preview() {
    let (img, raw) = raw_image();
    let out = Command::cargo_bin("revenant-dev")
        .unwrap()
        .arg("scan")
        .arg(img.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let events = lines(&out);
    let found: Vec<_> = events.iter().filter(|e| e["event"] == "found").collect();
    assert_eq!(found.len(), 1, "{events:?}");
    let f = found[0];
    assert_eq!(f["kind"], "cr2");
    assert_eq!(f["health"], "intact");
    assert_eq!(f["make"], "Acme");
    assert_eq!(f["model"], "Model X");
    assert_eq!(f["lens"], "Lens 50mm f/1.8");
    assert_eq!(f["serial"], "SN12345");
    assert_eq!(f["iso"], 400);
    assert_eq!(f["captured_at"], "2024-05-06T07:08:09");
    // The photo's size is the raw image's (the SubIFD's 6000x4000), not the preview's.
    assert_eq!(f["width"], 6000);
    assert_eq!(f["height"], 4000);
    let p = raw.preview.clone().unwrap();
    assert_eq!(
        f["preview"],
        serde_json::json!({
            "start": 8192 + p.start,
            "len": p.len(),
            "width": 320,
            "height": 240,
        })
    );
}

#[test]
fn unknown_details_are_omitted() {
    let (img, _, _) = image();
    let out = Command::cargo_bin("revenant-dev")
        .unwrap()
        .arg("scan")
        .arg(img.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let events = lines(&out);
    let video = events.iter().find(|e| e["kind"] == "mp4").unwrap();
    for key in ["make", "model", "lens", "serial", "iso", "preview"] {
        assert!(video.get(key).is_none(), "{key} in {video}");
    }
    let photo = events.iter().find(|e| e["kind"] == "jpeg").unwrap();
    assert!(photo.get("make").is_none(), "{photo}");
    assert_eq!(photo["preview"]["width"], 64);
}

#[test]
fn previews_are_written() {
    let (img, raw) = raw_image();
    let dest = tempfile::tempdir().unwrap();
    let out = Command::cargo_bin("revenant-dev")
        .unwrap()
        .arg("scan")
        .arg(img.path())
        .arg("--previews")
        .arg(dest.path())
        .arg("--allow-same-volume")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let events = lines(&out);
    let f = events.iter().find(|e| e["event"] == "found").unwrap();
    let path = f["preview_file"].as_str().expect("preview_file field");
    assert_eq!(
        std::path::Path::new(path).file_name().unwrap(),
        "2024-05-06_07-08-09_ModelX.preview.jpg"
    );
    let written = std::fs::read(path).unwrap();
    assert_eq!(written, raw.bytes[raw.preview.clone().unwrap()]);
    assert_eq!(std::fs::read_dir(dest.path()).unwrap().count(), 1);
}

#[test]
fn previews_to_same_volume_are_refused() {
    let (img, _) = raw_image();
    let dest = tempfile::tempdir().unwrap();
    Command::cargo_bin("revenant-dev")
        .unwrap()
        .arg("scan")
        .arg(img.path())
        .arg("--previews")
        .arg(dest.path())
        .assert()
        .code(1)
        .stderr(predicates::str::contains("error: destination_on_source"));
    assert_eq!(std::fs::read_dir(dest.path()).unwrap().count(), 0);
}

#[test]
fn missing_previews_folder_fails_before_scanning() {
    let (img, _) = raw_image();
    let dest = tempfile::tempdir().unwrap();
    let out = Command::cargo_bin("revenant-dev")
        .unwrap()
        .arg("scan")
        .arg(img.path())
        .arg("--previews")
        .arg(dest.path().join("not-created"))
        .arg("--allow-same-volume")
        .assert()
        .code(1)
        .stderr(predicates::str::contains("error: open_failed"))
        .get_output()
        .clone();
    assert!(out.stdout.is_empty());
}

#[test]
fn bench_reports_each_image() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::cargo_bin("revenant-dev")
        .unwrap()
        .args([
            "bench",
            "--size-mib",
            "8",
            "--runs",
            "1",
            "--images",
            "noise,dense-jpeg",
            "--dir",
        ])
        .arg(dir.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let events = lines(&out);
    assert_eq!(events.len(), 2);
    for e in &events {
        assert_eq!(e["event"], "bench");
        assert_eq!(e["bytes"], 8 * 1024 * 1024);
        assert_eq!(e["runs"], 1);
        assert!(e["mb_per_s"].as_f64().unwrap() > 0.0);
    }
    assert_eq!(events[0]["image"], "noise");
    assert_eq!(events[0]["placed"], 0);
    assert_eq!(events[1]["image"], "dense-jpeg");
    assert!(events[1]["placed"].as_u64().unwrap() >= 4);
    assert_eq!(events[1]["found"], events[1]["placed"]);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn bench_rejects_an_unknown_image() {
    let dir = tempfile::tempdir().unwrap();
    Command::cargo_bin("revenant-dev")
        .unwrap()
        .args(["bench", "--images", "noise,bogus", "--dir"])
        .arg(dir.path())
        .assert()
        .code(2)
        .stdout("")
        .stderr(predicates::str::contains("unknown image 'bogus'"));
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn threads_option_gives_the_same_output() {
    let (img, _, _) = image();
    let stdout = |threads: &str| {
        Command::cargo_bin("revenant-dev")
            .unwrap()
            .args(["scan", "--threads", threads])
            .arg(img.path())
            .assert()
            .success()
            .get_output()
            .stdout
            .clone()
    };
    let one = stdout("1");
    assert_eq!(lines(&one).len(), 3);
    assert_eq!(stdout("8"), one);
}

#[test]
fn bench_reports_its_thread_count() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::cargo_bin("revenant-dev")
        .unwrap()
        .args([
            "bench",
            "--size-mib",
            "4",
            "--runs",
            "1",
            "--images",
            "noise",
            "--threads",
            "3",
            "--dir",
        ])
        .arg(dir.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_eq!(lines(&out)[0]["threads"], 3);
}

#[test]
fn updated_lines_follow_found_lines() {
    // A camera video split in two: its moov ended up 11 MiB further on.
    let v = revenant_testkit::avc_video(&revenant_testkit::AvcOptions {
        samples: 120,
        seed: 1,
        mov: false,
        mdat_pad: 0,
        small_nals: true,
    });
    let p = &v.mdat_payload;
    let l1 = (p.start + p.len() * 2 / 5).div_ceil(512) * 512;
    let mut img = DiskImage::noise(24 << 20, 7);
    img.put(1 << 20, &v.bytes[..l1])
        .put(12 << 20, &v.bytes[l1..]);
    let img = img.save();
    let dest = tempfile::tempdir().unwrap();
    let out = Command::cargo_bin("revenant-dev")
        .unwrap()
        .arg("scan")
        .arg(img.path())
        .arg("--extract")
        .arg(dest.path())
        .arg("--allow-same-volume")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let events = lines(&out);
    let kinds: Vec<&str> = events
        .iter()
        .map(|e| e["event"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, vec!["found", "updated", "summary"], "{events:?}");
    let (found, updated) = (&events[0], &events[1]);
    assert_eq!(
        (found["id"].as_u64(), found["health"].as_str()),
        (Some(1), Some("damaged"))
    );
    assert!(found.get("fragments").is_none());
    assert_eq!(updated["id"], 1);
    assert_eq!(updated["kind"], "mp4");
    assert_eq!(updated["health"], "intact");
    assert_eq!(updated["len"], v.bytes.len() as u64);
    assert_eq!(
        updated["fragments"],
        serde_json::json!([
            {"start": 1 << 20, "len": l1},
            {"start": 12 << 20, "len": v.bytes.len() - l1}
        ])
    );
    // The updated finding is extracted again, under a new name.
    let (first, second) = (
        found["file"].as_str().unwrap(),
        updated["file"].as_str().unwrap(),
    );
    assert_ne!(first, second);
    assert!(second.ends_with("_2.mp4"), "{second}");
    assert_eq!(std::fs::read(second).unwrap(), v.bytes);
    assert_eq!(events[2]["updated"], 1);
    assert_eq!(events[2]["found"], 1);
}

#[test]
fn volume_and_origin_keys() {
    let named = jpeg(64, 48, 3, 0);
    let stray = jpeg(64, 48, 4, 0);
    let mut b = FsBuilder::new(FsType::Fat32, 48 << 20, 512);
    b.mkdir("DCIM/100MSDCF");
    b.add("DCIM/100MSDCF/DSC01234.JPG", &named);
    let at = FsStamp {
        year: 2025,
        month: 12,
        day: 14,
        hour: 7,
        min: 12,
        sec: 6,
        utc_offset_min: None,
    };
    b.stamp("DCIM/100MSDCF/DSC01234.JPG", at, at);
    let free = b.cluster_offset(b.cluster_count() - 200) as usize;
    let volume = b.build();
    let mut bytes = disk(
        Partitioning::Mbr { first_lba: 8192 },
        &volume,
        (4 << 20) + volume.len() + 4096,
    );
    bytes[(4 << 20) + free..(4 << 20) + free + stray.len()].copy_from_slice(&stray);
    let mut img = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut img, &bytes).unwrap();
    let out = Command::cargo_bin("revenant-dev")
        .unwrap()
        .arg("scan")
        .arg(img.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let events = lines(&out);
    let v = &events[0];
    assert_eq!(v["event"], "volume");
    assert_eq!(v["index"], 0);
    assert_eq!(v["fs"], "fat32");
    assert_eq!(v["scheme"], "mbr");
    assert_eq!(v["offset"], 4194304);
    assert_eq!(v["sector_size"], 512);
    assert_eq!(v["cluster_size"], 512);
    assert_eq!(v["label"], "REVENANT");
    assert_eq!(v["serial"], 0x1234_5678);
    assert_eq!(v["from_backup"], false);
    assert_eq!(v["listing_truncated"], false);
    let found: Vec<_> = events.iter().filter(|e| e["event"] == "found").collect();
    assert_eq!(found.len(), 2, "{events:?}");
    let a = found.iter().find(|f| f.get("name").is_some()).unwrap();
    assert_eq!(a["name"], "DSC01234.JPG");
    assert_eq!(a["path"], "DCIM/100MSDCF/DSC01234.JPG");
    assert_eq!(a["state"], "live");
    assert_eq!(a["placement"], "chain");
    assert_eq!(a["volume"], 0);
    assert_eq!(a["fs_size"], named.len());
    assert_eq!(a["created"], "2025-12-14T07:12:06");
    assert_eq!(a["modified"], "2025-12-14T07:12:06");
    let other = found.iter().find(|f| f.get("name").is_none()).unwrap();
    for k in [
        "path",
        "state",
        "placement",
        "volume",
        "fs_size",
        "created",
        "modified",
    ] {
        assert!(other.get(k).is_none(), "{k}: {other}");
    }
    let last = events.last().unwrap();
    assert_eq!(last["event"], "summary");
    assert_eq!(last["listing_truncated"], false);
}

#[test]
fn bench_knows_the_exfat_card() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::cargo_bin("revenant-dev")
        .unwrap()
        .args([
            "bench",
            "--size-mib",
            "16",
            "--runs",
            "1",
            "--images",
            "exfat-card",
            "--dir",
        ])
        .arg(dir.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let events = lines(&out);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["image"], "exfat-card");
    assert!(events[0]["placed"].as_u64().unwrap() >= 4);
    assert_eq!(events[0]["found"], events[0]["placed"]);
    let out = Command::cargo_bin("revenant-dev")
        .unwrap()
        .args(["bench", "--size-mib", "16", "--runs", "1", "--dir"])
        .arg(dir.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let names: Vec<_> = lines(&out)
        .iter()
        .map(|e| e["image"].as_str().unwrap().to_string())
        .collect();
    assert!(names.contains(&"exfat-card".to_string()), "{names:?}");
}

#[test]
fn extract_uses_original_and_readable_names() {
    let named = jpeg(64, 48, 3, 0);
    let raw = tiff_raw(&TiffRawOptions {
        variant: TiffVariant::Arw,
        big_endian: false,
        raw_len: 20_000,
        seed: 5,
        preview: Some((320, 240)),
        preview_sector_aligned: false,
        exif: exif(),
    });
    let carved = revenant_testkit::jpeg_with_exif(
        64,
        48,
        9,
        &TestExif {
            make: Some("SONY".into()),
            model: Some("ILCE-7M4".into()),
            lens: None,
            serial: None,
            iso: None,
            date: Some("2025:12:14 07:12:05".into()),
        },
    );
    let mut b = FsBuilder::new(FsType::Fat32, 48 << 20, 512);
    b.mkdir("DCIM/100MSDCF");
    b.add("DCIM/100MSDCF/DSC01234.JPG", &named);
    b.add("DCIM/100MSDCF/DSC01235.ARW", &raw.bytes);
    let free = b.cluster_offset(b.cluster_count() - 200) as usize;
    let volume = b.build();
    let mut bytes = disk(
        Partitioning::Mbr { first_lba: 8192 },
        &volume,
        (4 << 20) + volume.len() + 4096,
    );
    bytes[(4 << 20) + free..(4 << 20) + free + carved.len()].copy_from_slice(&carved);
    let mut img = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut img, &bytes).unwrap();
    let dest = tempfile::tempdir().unwrap();
    let previews = tempfile::tempdir().unwrap();
    let out = Command::cargo_bin("revenant-dev")
        .unwrap()
        .arg("scan")
        .arg(img.path())
        .arg("--extract")
        .arg(dest.path())
        .arg("--previews")
        .arg(previews.path())
        .arg("--allow-same-volume")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let events = lines(&out);
    let files: Vec<String> = events
        .iter()
        .filter_map(|e| e["file"].as_str())
        .map(|p| {
            std::path::Path::new(p)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    for want in [
        "DSC01234.JPG",
        "DSC01235.ARW",
        "2025-12-14_07-12-05_ILCE-7M4.JPG",
    ] {
        assert!(files.iter().any(|f| f == want), "{want} in {files:?}");
    }
    let previews: Vec<String> = std::fs::read_dir(previews.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        previews.iter().any(|f| f == "DSC01235.preview.jpg"),
        "{previews:?}"
    );
}

#[test]
fn group_lines_come_before_the_summary() {
    let at = |date: &str| TestExif {
        date: Some(date.into()),
        ..exif()
    };
    let mut img = DiskImage::noise(256 * 1024, 7);
    img.put(8192, &jpeg_with_exif(64, 48, 1, &at("2025:12:14 08:00:00")))
        .put(
            96 * 1024,
            &jpeg_with_exif(64, 48, 2, &at("2025:12:14 11:00:00")),
        );
    let img = img.save();
    let run = |by: &str| {
        let out = Command::cargo_bin("revenant-dev")
            .unwrap()
            .args(["scan", "--group", by])
            .arg(img.path())
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        lines(&out)
    };
    let events = run("session");
    let kinds: Vec<&str> = events
        .iter()
        .map(|e| e["event"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        vec!["found", "found", "group", "group", "summary"],
        "{events:?}"
    );
    assert_eq!(events[2]["by"], "session");
    assert_eq!(
        events[2]["label"],
        serde_json::json!({
            "kind": "session",
            "camera": {"make": "Acme", "model": "Model X", "serial": "SN12345"},
            "first": "2025-12-14T08:00:00",
            "last": "2025-12-14T08:00:00",
        })
    );
    assert_eq!(events[2]["ids"], serde_json::json!([1]));
    assert_eq!(events[3]["label"]["first"], "2025-12-14T11:00:00");
    assert_eq!(events[3]["ids"], serde_json::json!([2]));

    let days = run("day");
    assert_eq!(days.len(), 4, "{days:?}");
    assert_eq!(
        days[2]["label"],
        serde_json::json!({"kind": "day", "day": "2025-12-14"})
    );
    assert_eq!(days[2]["ids"], serde_json::json!([1, 2]));
    let cams = run("camera");
    assert_eq!(cams[2]["label"]["kind"], "camera");

    // Files found only by their signature have no folder: the grouping is not available.
    let folders = run("folder");
    assert_eq!(folders.len(), 4, "{folders:?}");
    assert_eq!(
        folders[2],
        serde_json::json!({"event": "group", "by": "folder", "available": false})
    );
    assert_eq!(folders[3]["event"], "summary");
}

#[test]
fn an_unknown_camera_label_has_no_camera_key() {
    let (img, _, _) = image();
    let out = Command::cargo_bin("revenant-dev")
        .unwrap()
        .args(["scan", "--group", "session"])
        .arg(img.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let groups: Vec<_> = lines(&out)
        .into_iter()
        .filter(|e| e["event"] == "group")
        .collect();
    assert!(!groups.is_empty());
    for g in &groups {
        assert!(g["label"].get("camera").is_none(), "{g}");
    }
    assert!(
        groups
            .iter()
            .any(|g| g["label"]["kind"] == "undated_session")
    );
}
