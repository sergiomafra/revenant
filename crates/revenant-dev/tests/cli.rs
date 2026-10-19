use assert_cmd::Command;
use revenant_testkit::{DiskImage, Mp4Options, jpeg, mp4};

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
fn extract_failure_is_reported_per_finding_and_scan_continues() {
    let (img, _, _) = image();
    let dest = tempfile::tempdir().unwrap();
    let missing = dest.path().join("not-created");
    let out = Command::cargo_bin("revenant-dev")
        .unwrap()
        .arg("scan")
        .arg(img.path())
        .arg("--extract")
        .arg(&missing)
        .arg("--allow-same-volume")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let events = lines(&out);
    let found: Vec<_> = events.iter().filter(|e| e["event"] == "found").collect();
    assert_eq!(found.len(), 2);
    for f in &found {
        assert_eq!(f["error"], "open_failed");
        assert!(f.get("file").is_none());
    }
    assert_eq!(events.last().unwrap()["event"], "summary");
    assert_eq!(events.last().unwrap()["found"], 2);
}
