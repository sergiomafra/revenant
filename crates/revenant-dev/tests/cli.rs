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
fn bench_with_session_adds_a_session_line() {
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
            "dense-jpeg",
            "--session",
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
    assert_eq!(events[0]["session"], false);
    assert_eq!(events[1]["session"], true);
    assert_eq!(events[1]["image"], "dense-jpeg");
    assert_eq!(events[1]["found"], events[0]["found"]);
    assert!(events[1]["mb_per_s"].as_f64().unwrap() > 0.0);
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

// ---- Part 1.5: copies, maps, digests and saved scans (image files only: no device is ever touched here). ----

const MIB: usize = 1 << 20;

fn dev() -> Command {
    Command::cargo_bin("revenant-dev").unwrap()
}

/// `bytes` written to `dir/name`.
fn file_in(dir: &std::path::Path, name: &str, bytes: &[u8]) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    path
}

/// A 16 MiB noise image with two photos in each 4 MiB block (the default scan block), so a scan emits two `found`
/// lines, then a block boundary, and so on.
fn photos_image() -> Vec<u8> {
    let mut bytes = std::fs::read(DiskImage::noise(16 * MIB, 11).save().path()).unwrap();
    for (i, at) in [MIB / 2, 5 * MIB / 2].iter().cycle().take(8).enumerate() {
        let at = at + (i / 2) * 4 * MIB;
        let photo = jpeg(64, 48, i as u64 + 1, 0);
        bytes[at..at + photo.len()].copy_from_slice(&photo);
    }
    bytes
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// A rescue map of `size` bytes saved beside `image`: all rescued except `holes` (start, len), left untried.
fn save_map(image: &std::path::Path, size: u64, holes: &[(u64, u64)]) {
    use revenant_core::SourceIdentity;
    use revenant_core::rescue::{RegionState, RescueMap, map_path};
    let mut map = RescueMap::new(size, 512, SourceIdentity::Image { size, digest: 1 });
    assert!(map.set(0, size, RegionState::Rescued));
    for &(start, len) in holes {
        assert!(map.set(start, len, RegionState::Untried));
    }
    map.save(&map_path(image)).unwrap();
}

fn without(mut v: serde_json::Value, keys: &[&str]) -> serde_json::Value {
    for k in keys {
        v.as_object_mut().unwrap().remove(*k);
    }
    v
}

#[test]
fn copy_of_an_image_file_is_identical() {
    let dir = tempfile::tempdir().unwrap();
    let bytes = photos_image();
    let img = file_in(dir.path(), "card.img", &bytes);
    let out_dir = tempfile::tempdir().unwrap();
    let out = out_dir.path().join("out.img");
    let run = || {
        let o = dev()
            .arg("copy")
            .arg(&img)
            .arg(&out)
            .arg("--allow-same-volume")
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        lines(&o)
    };
    let events = run();
    assert_eq!(std::fs::read(&out).unwrap(), bytes);
    assert!(out_dir.path().join("out.img.rvmap").exists());
    assert!(events.iter().any(|e| e["event"] == "copy_progress"));
    let last = events.last().unwrap();
    assert_eq!(last["event"], "copy_summary", "{last}");
    assert_eq!(last["complete"], true);
    assert_eq!(last["rescued"], bytes.len());
    assert_eq!(last["untried"], 0);
    // A second run resumes from the map: nothing left to copy, the image unchanged.
    let again = run();
    assert_eq!(again.last().unwrap()["complete"], true);
    assert_eq!(std::fs::read(&out).unwrap(), bytes);
    // Onto the image's own volume without the flag: refused before anything is created.
    let fresh = dir.path().join("again.img");
    dev()
        .arg("copy")
        .arg(&img)
        .arg(&fresh)
        .assert()
        .code(1)
        .stderr(predicates::str::contains("error: destination_on_source"));
    assert!(!fresh.exists());
}

#[test]
fn scan_with_a_map_marks_holes_unreadable() {
    // The image of `unrescued_zeros_are_never_intact` (core): a JPEG at 1 MiB whose all-padding APP1 payload holds a
    // 127-sector run the copy never read (zeros on disk).
    let dir = tempfile::tempdir().unwrap();
    let size = 8 * MIB;
    let at = MIB;
    let photo = revenant_testkit::jpeg(640, 480, 21, 192 * 1024);
    let seg2 = 2 + 4 + 65_533;
    assert_eq!(photo[seg2..seg2 + 2], [0xFF, 0xE1]);
    let payload = (at + seg2 + 4) as u64..(at + seg2 + 4 + 65_533) as u64;
    let run_start = payload.start.next_multiple_of(512);
    let run_end = payload.end / 512 * 512;
    let mut bytes = std::fs::read(DiskImage::noise(size, 8).save().path()).unwrap();
    bytes[at..at + photo.len()].copy_from_slice(&photo);
    bytes[run_start as usize..run_end as usize].fill(0);
    let img = file_in(dir.path(), "card.img", &bytes);
    save_map(&img, size as u64, &[(run_start, run_end - run_start)]);
    let scan = || {
        lines(
            &dev()
                .arg("scan")
                .arg(&img)
                .assert()
                .success()
                .get_output()
                .stdout,
        )
    };

    let events = scan();
    assert_eq!(events[0]["event"], "map", "{}", events[0]);
    assert_eq!(events[0]["unrescued"], run_end - run_start);
    assert_eq!(events[0]["rescued"], size as u64 - (run_end - run_start));
    assert!(
        events.iter().any(|e| e["event"] == "unreadable"
            && e["start"] == run_start
            && e["len"] == run_end - run_start),
        "{events:?}"
    );
    let health = |events: &[serde_json::Value]| {
        events
            .iter()
            .find(|e| e["event"] == "found" && e["start"] == at)
            .expect("the photo")["health"]
            .clone()
    };
    assert_ne!(health(&events), "intact");

    // Without the map the zeros look like data: intact, no map line.
    std::fs::remove_file(dir.path().join("card.img.rvmap")).unwrap();
    let plain = scan();
    assert_ne!(plain[0]["event"], "map");
    assert_eq!(health(&plain), "intact");
}

#[test]
fn scan_session_resumes_and_replays() {
    let dir = tempfile::tempdir().unwrap();
    let img = file_in(dir.path(), "card.img", &photos_image());
    let full = lines(
        &dev()
            .arg("scan")
            .arg(&img)
            .assert()
            .success()
            .get_output()
            .stdout,
    );
    assert!(full.iter().filter(|e| e["event"] == "found").count() >= 8);
    let session = dir.path().join("session");
    let extracted = dir.path().join("out");
    std::fs::create_dir(&extracted).unwrap();
    // Run 1: a checkpoint every MiB, stopped after the 7th event (the first block boundary comes before it).
    let first = dev()
        .arg("scan")
        .arg(&img)
        .arg("--session")
        .arg(&session)
        .arg("--allow-same-volume")
        .args(["--checkpoint-mib", "1", "--stop-after-events", "7"])
        .assert()
        .code(3)
        .get_output()
        .stdout
        .clone();
    assert!(session.join("session.rvs").exists());
    let first = lines(&first);
    assert!(first.iter().all(|e| e.get("replayed").is_none()));
    // Run 2: resumed, extracting.
    let second = lines(
        &dev()
            .arg("scan")
            .arg(&img)
            .arg("--session")
            .arg(&session)
            .arg("--extract")
            .arg(&extracted)
            .arg("--allow-same-volume")
            .assert()
            .success()
            .get_output()
            .stdout,
    );
    let replayed = second.iter().filter(|e| e["replayed"] == true).count();
    assert!(replayed > 0, "{second:?}");
    assert!(
        second[..replayed].iter().all(|e| e["replayed"] == true),
        "the replayed lines come first"
    );
    assert!(
        second[..replayed].iter().all(|e| e.get("file").is_none()),
        "replayed lines are not extracted again"
    );
    let written = second[replayed..]
        .iter()
        .filter(|e| e.get("file").is_some())
        .count();
    assert!(written > 0);
    assert_eq!(std::fs::read_dir(&extracted).unwrap().count(), written);
    let stripped: Vec<_> = second
        .into_iter()
        .map(|e| without(e, &["replayed", "file"]))
        .collect();
    assert_eq!(stripped, full);
}

#[test]
fn finished_session_scan_reads_nothing_new() {
    let dir = tempfile::tempdir().unwrap();
    let img = file_in(dir.path(), "card.img", &photos_image());
    let session = dir.path().join("session");
    let run = || {
        lines(
            &dev()
                .arg("scan")
                .arg(&img)
                .arg("--session")
                .arg(&session)
                .arg("--allow-same-volume")
                .assert()
                .success()
                .get_output()
                .stdout,
        )
    };
    let first = run();
    let second = run();
    let (summary, rest) = second.split_last().unwrap();
    assert_eq!(summary["event"], "summary");
    assert_eq!(summary, first.last().unwrap());
    assert!(!rest.is_empty());
    assert!(rest.iter().all(|e| e["replayed"] == true), "{rest:?}");
    let rest: Vec<_> = rest
        .iter()
        .map(|e| without(e.clone(), &["replayed"]))
        .collect();
    assert_eq!(rest, first[..first.len() - 1]);
}

#[test]
fn preflight_needs_a_whole_disk() {
    dev()
        .args(["preflight", "disk4s1"])
        .assert()
        .code(1)
        .stderr(predicates::str::contains("error: not_whole_disk"));
}

#[test]
fn digest_of_an_image() {
    let dir = tempfile::tempdir().unwrap();
    let bytes = photos_image();
    let img = file_in(dir.path(), "card.img", &bytes[..bytes.len() - 1000]);
    let out = lines(
        &dev()
            .arg("digest")
            .arg(&img)
            .assert()
            .success()
            .get_output()
            .stdout,
    );
    assert_eq!(out.len(), 1);
    assert_eq!(out[0]["event"], "digest");
    assert_eq!(out[0]["bytes"], bytes.len() - 1000);
    assert_eq!(
        out[0]["fnv64"],
        format!("{:016x}", fnv1a64(&bytes[..bytes.len() - 1000]))
    );
    assert_eq!(out[0]["unreadable_bytes"], 0);
}

#[test]
fn disk_failing_line() {
    // An image file has no bad sectors, so a map supplies them: 20 isolated sectors never rescued.
    let dir = tempfile::tempdir().unwrap();
    let size = 8 * MIB;
    let img = file_in(
        dir.path(),
        "card.img",
        &std::fs::read(DiskImage::noise(size, 5).save().path()).unwrap(),
    );
    let holes: Vec<(u64, u64)> = (0..20u64).map(|i| (i * 256 * 1024 + 4096, 512)).collect();
    save_map(&img, size as u64, &holes);
    let events = lines(
        &dev()
            .arg("scan")
            .arg(&img)
            .assert()
            .success()
            .get_output()
            .stdout,
    );
    let failing: Vec<_> = events
        .iter()
        .filter(|e| e["event"] == "disk_failing")
        .collect();
    assert_eq!(failing.len(), 1, "{events:?}");
    assert!(
        failing[0]["unreadable_runs"].as_u64().unwrap() >= 16,
        "{}",
        failing[0]
    );
    assert_eq!(
        events.iter().filter(|e| e["event"] == "unreadable").count(),
        20
    );
}
