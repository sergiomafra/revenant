use revenant_core::formats::{FileKind, Health, all_formats};
use revenant_core::source::FileSource;
use revenant_core::{CancelToken, Event, Finding, Fragment, ScanOptions, inspect, scan};
use revenant_testkit as testkit;
use testkit::{DiskImage, Mp4Options, TestExif, TiffRawOptions, TiffVariant};

const AT: usize = 8192;

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

/// Puts `bytes` at [`AT`] in a noise image, scans it and returns the finding at [`AT`] with the source.
fn find(bytes: &[u8], kind: FileKind) -> (FileSource, Finding, tempfile::NamedTempFile) {
    let mut img = DiskImage::noise(AT + bytes.len() + 8192, 99);
    img.put(AT, bytes);
    let file = img.save();
    let source = FileSource::open(file.path()).unwrap();
    let mut found = Vec::new();
    scan(
        &source,
        &all_formats(),
        &ScanOptions::default(),
        &CancelToken::default(),
        &mut |e| {
            if let Event::Found(f) = e {
                found.push(f);
            }
        },
    )
    .unwrap();
    let f = found
        .into_iter()
        .find(|f| f.start == AT as u64)
        .unwrap_or_else(|| panic!("{kind:?} not found"));
    assert_eq!(f.kind, kind);
    (source, f, file)
}

fn assert_exif(m: &revenant_core::Metadata) {
    assert_eq!(m.make.as_deref(), Some("Acme"));
    assert_eq!(m.model.as_deref(), Some("Model X"));
    assert_eq!(m.lens.as_deref(), Some("Lens 50mm f/1.8"));
    assert_eq!(m.serial.as_deref(), Some("SN12345"));
    assert_eq!(m.iso, Some(400));
    assert_eq!(m.captured_at.as_deref(), Some("2024-05-06T07:08:09"));
}

#[test]
fn jpeg_exif_and_self_preview() {
    let jpeg = testkit::jpeg_with_exif(64, 48, 1, &exif());
    let (source, f, _keep) = find(&jpeg, FileKind::Jpeg);
    let i = inspect(&source, &f);
    assert_exif(&i.metadata);
    assert_eq!((i.metadata.width, i.metadata.height), (Some(64), Some(48)));
    let p = i.preview.unwrap();
    assert_eq!((p.start, p.len), (f.start, f.len));
    assert_eq!((p.width, p.height), (64, 48));
}

#[test]
fn each_raw_kind_metadata_and_largest_preview() {
    let kinds = [
        (TiffVariant::Cr2, FileKind::Cr2),
        (TiffVariant::Nef, FileKind::Nef),
        (TiffVariant::Arw, FileKind::Arw),
        (TiffVariant::Dng, FileKind::Dng),
        (TiffVariant::Orf, FileKind::Orf),
        (TiffVariant::Rw2, FileKind::Rw2),
    ];
    for big_endian in [false, true] {
        for (variant, kind) in kinds {
            let mut e = exif();
            // Kind detection reads the Make of NEF and ARW.
            e.make = Some(
                match variant {
                    TiffVariant::Nef => "NIKON CORPORATION",
                    TiffVariant::Arw => "SONY",
                    _ => "Acme",
                }
                .into(),
            );
            let raw = testkit::tiff_raw(&TiffRawOptions {
                variant,
                big_endian,
                raw_len: 3000,
                seed: 7,
                preview: Some((96, 64)),
                preview_sector_aligned: false,
                exif: e.clone(),
            });
            let (source, f, _keep) = find(&raw.bytes, kind);
            let i = inspect(&source, &f);
            let label = format!("{variant:?} be={big_endian}");
            assert_eq!(i.metadata.make, e.make, "{label}");
            assert_eq!(i.metadata.model.as_deref(), Some("Model X"), "{label}");
            assert_eq!(i.metadata.iso, Some(400), "{label}");
            assert_eq!(
                i.metadata.captured_at.as_deref(),
                Some("2024-05-06T07:08:09"),
                "{label}"
            );
            assert_eq!(
                i.metadata.lens.as_deref(),
                Some("Lens 50mm f/1.8"),
                "{label}"
            );
            assert_eq!(i.metadata.serial.as_deref(), Some("SN12345"), "{label}");
            let pr = raw.preview.clone().unwrap();
            let p = i.preview.unwrap_or_else(|| panic!("{label}: no preview"));
            assert_eq!(p.start, (AT + pr.start) as u64, "{label}");
            assert_eq!(p.len, pr.len() as u64, "{label}");
            assert_eq!((p.width, p.height), (96, 64), "{label}");
        }
    }

    // RAF: EXIF and preview of the embedded JPEG.
    let raf = testkit::raf(2000, 3, &exif());
    let (source, f, _keep) = find(&raf, FileKind::Raf);
    let i = inspect(&source, &f);
    assert_exif(&i.metadata);
    let jpeg_at = u32::from_be_bytes(raf[0x54..0x58].try_into().unwrap());
    let jpeg_len = u32::from_be_bytes(raf[0x58..0x5C].try_into().unwrap());
    let p = i.preview.unwrap();
    assert_eq!(
        (p.start, p.len, p.width, p.height),
        (
            AT as u64 + u64::from(jpeg_at),
            u64::from(jpeg_len),
            160,
            120
        )
    );

    // CR3: Make/Model from CMT1, the rest from CMT2; the full-size JPEG in `mdat` beats the PRVW one.
    let cr3 = testkit::cr3(4, &exif());
    let (source, f, _keep) = find(&cr3, FileKind::Cr3);
    let i = inspect(&source, &f);
    assert_exif(&i.metadata);
    let p = i.preview.unwrap();
    assert_eq!((p.width, p.height), (640, 480));
    assert_eq!(p.start % 512, 0);
    assert_eq!(&cr3[(p.start as usize - AT)..][..3], [0xFF, 0xD8, 0xFF]);
}

#[test]
fn heic_metadata_without_preview() {
    let heic = testkit::heic(4000, 5, &exif());
    let (source, f, _keep) = find(&heic, FileKind::Heic);
    let i = inspect(&source, &f);
    assert_exif(&i.metadata);
    assert_eq!(i.preview, None);
}

#[test]
fn mp4_creation_time_and_size() {
    // 2024-05-06 07:08:09 UTC as seconds since 1904-01-01.
    let unix: u64 = 1_714_979_289;
    let qt = (unix + 2_082_844_800) as u32;
    for mov in [false, true] {
        let opts = Mp4Options {
            mdat_len: 4000,
            moov_at_end: true,
            mov,
            mdat_64bit: false,
        };
        let bytes = testkit::mp4_dated(&opts, qt, 1920, 1080);
        let kind = if mov { FileKind::Mov } else { FileKind::Mp4 };
        let (source, f, _keep) = find(&bytes, kind);
        let i = inspect(&source, &f);
        assert_eq!(
            i.metadata.captured_at.as_deref(),
            Some("2024-05-06T07:08:09")
        );
        assert_eq!(
            (i.metadata.width, i.metadata.height),
            (Some(1920), Some(1080))
        );
        assert_eq!(i.preview, None);
    }
    // Version 1 boxes (64-bit times): the track size sits 12 bytes further on.
    for mov in [false, true] {
        let opts = Mp4Options {
            mdat_len: 4000,
            moov_at_end: false,
            mov,
            mdat_64bit: true,
        };
        let bytes = testkit::mp4_dated_v1(&opts, unix + 2_082_844_800, 3840, 2160);
        let kind = if mov { FileKind::Mov } else { FileKind::Mp4 };
        let (source, f, _keep) = find(&bytes, kind);
        let m = inspect(&source, &f).metadata;
        assert_eq!(m.captured_at.as_deref(), Some("2024-05-06T07:08:09"));
        assert_eq!((m.width, m.height), (Some(3840), Some(2160)));
    }
    // A zero-sized track header gives no size.
    let flat = testkit::mp4_dated(
        &Mp4Options {
            mdat_len: 4000,
            moov_at_end: true,
            mov: false,
            mdat_64bit: false,
        },
        qt,
        0,
        0,
    );
    let (source, f, _keep) = find(&flat, FileKind::Mp4);
    let m = inspect(&source, &f).metadata;
    assert_eq!((m.width, m.height), (None, None));
    // A video with a zero creation time and no tkhd: nothing to report.
    let plain = testkit::mp4(&Mp4Options {
        mdat_len: 4000,
        moov_at_end: false,
        mov: false,
        mdat_64bit: false,
    });
    let (source, f, _keep) = find(&plain, FileKind::Mp4);
    assert_eq!(inspect(&source, &f).metadata, Default::default());
}

#[test]
fn a_jpeg_with_many_app_segments_before_the_sof() {
    let plain = testkit::jpeg_with_app_segments(64, 48, 1, 0);
    let many = testkit::jpeg_with_app_segments(64, 48, 1, 20);
    let (source, f0, _k0) = find(&plain, FileKind::Jpeg);
    let (source20, f20, _k20) = find(&many, FileKind::Jpeg);
    assert_eq!(f20.health, f0.health);
    assert_eq!(f20.health, Health::Intact);
    assert_eq!(f20.len, many.len() as u64);
    let i = inspect(&source20, &f20);
    assert_eq!((i.metadata.width, i.metadata.height), (Some(64), Some(48)));
    let p = i.preview.unwrap();
    assert_eq!(
        (p.start, p.len, p.width, p.height),
        (f20.start, f20.len, 64, 48)
    );
    assert!(inspect(&source, &f0).preview.is_some());
}

#[test]
fn a_finding_cut_inside_the_sof_has_no_preview() {
    let jpeg = testkit::jpeg_with_exif(64, 48, 1, &exif());
    let (source, f, _keep) = find(&jpeg, FileKind::Jpeg);
    let sof = jpeg
        .windows(2)
        .position(|w| w == [0xFF, 0xC0] || w == [0xFF, 0xC2])
        .unwrap() as u64;
    // Height complete, width missing: the dimensions would read bytes past the finding.
    for len in [sof + 7, sof + 8] {
        let cut = Finding { len, ..f.clone() };
        assert_eq!(inspect(&source, &cut).preview, None, "len {len}");
    }
    let whole = Finding { len: sof + 9, ..f };
    assert_eq!(inspect(&source, &whole).preview.map(|p| p.width), Some(64));
}

#[test]
fn heic_exif_item_with_a_tiff_offset_prefix() {
    for offset in [6u32, 10] {
        let heic = testkit::heic_with_tiff_offset(4000, 5, &exif(), offset);
        let (source, f, _keep) = find(&heic, FileKind::Heic);
        assert_exif(&inspect(&source, &f).metadata);
    }
}

#[test]
fn garbage_exif_fields_become_none() {
    let bad = TestExif {
        make: Some("Ac\u{e9}me".into()),
        model: Some("  ".into()),
        lens: Some("Len\u{1}s".into()),
        serial: Some("\u{1}\u{2}\u{7f}".into()),
        iso: Some(0),
        date: Some("0000:00:00 00:00:00".into()),
    };
    let jpeg = testkit::jpeg_with_exif(32, 32, 2, &bad);
    let (source, f, _keep) = find(&jpeg, FileKind::Jpeg);
    let m = inspect(&source, &f).metadata;
    assert_eq!(
        (m.make, m.model, m.lens, m.serial, m.iso, m.captured_at),
        (None, None, None, None, None, None)
    );
    // Still a good JPEG: size and preview come from the SOF.
    assert_eq!((m.width, m.height), (Some(32), Some(32)));

    // Empty and NUL-only strings.
    for text in ["", "\0\0\0", "   \0  "] {
        let e = TestExif {
            make: Some(text.into()),
            serial: Some(text.into()),
            ..TestExif::default()
        };
        let jpeg = testkit::jpeg_with_exif(32, 32, 2, &e);
        let (source, f, _keep) = find(&jpeg, FileKind::Jpeg);
        let m = inspect(&source, &f).metadata;
        assert_eq!((m.make, m.serial), (None, None), "{text:?}");
    }

    // A date that parses but is not a real day, and one from before 1990.
    for date in [
        "2023:02:29 10:00:00",
        "1980:01:01 10:00:00",
        "2024:01:01 25:00:00",
    ] {
        let e = TestExif {
            date: Some(date.into()),
            ..exif()
        };
        let jpeg = testkit::jpeg_with_exif(32, 32, 2, &e);
        let (source, f, _keep) = find(&jpeg, FileKind::Jpeg);
        assert_eq!(inspect(&source, &f).metadata.captured_at, None, "{date}");
    }
}

#[test]
fn no_exif_gives_default_metadata() {
    let jpeg = testkit::jpeg(40, 30, 3, 0);
    let (source, f, _keep) = find(&jpeg, FileKind::Jpeg);
    let i = inspect(&source, &f);
    assert_eq!(
        i.metadata,
        revenant_core::Metadata {
            width: Some(40),
            height: Some(30),
            ..Default::default()
        }
    );
    assert!(i.preview.is_some());

    let raw = testkit::tiff_raw(&TiffRawOptions {
        variant: TiffVariant::Plain,
        big_endian: false,
        raw_len: 3000,
        seed: 1,
        preview: None,
        preview_sector_aligned: false,
        exif: TestExif::default(),
    });
    let (source, f, _keep) = find(&raw.bytes, FileKind::Tiff);
    let i = inspect(&source, &f);
    assert_eq!(i.preview, None);
    assert_eq!(i.metadata.model, None);
    assert_eq!(i.metadata.iso, None);
    assert_eq!(i.metadata.captured_at, None);
}

#[test]
fn a_preview_outside_the_finding_is_dropped() {
    let raw = testkit::tiff_raw(&TiffRawOptions {
        variant: TiffVariant::Dng,
        big_endian: false,
        raw_len: 3000,
        seed: 7,
        preview: Some((96, 64)),
        preview_sector_aligned: false,
        exif: exif(),
    });
    let (source, f, _keep) = find(&raw.bytes, FileKind::Dng);
    let pr = raw.preview.clone().unwrap();
    assert!(inspect(&source, &f).preview.is_some());
    // A finding that ends inside the preview keeps no preview ...
    let cut = Finding {
        len: (pr.start + pr.len() / 2) as u64,
        ..f.clone()
    };
    assert_eq!(inspect(&source, &cut).preview, None);
    // ... and one that ends right after it keeps it.
    let exact = Finding {
        len: pr.end as u64,
        ..f
    };
    assert_eq!(
        inspect(&source, &exact).preview.map(|p| p.len),
        Some(pr.len() as u64)
    );
}

#[test]
fn cr3_chunk_jpeg_outside_the_finding_is_dropped() {
    let cr3 = testkit::cr3(4, &exif());
    let (source, f, _keep) = find(&cr3, FileKind::Cr3);
    let full = inspect(&source, &f).preview.unwrap();
    assert_eq!((full.width, full.height), (640, 480));
    // Cut in the middle of the full-size JPEG: only the PRVW one lies inside.
    let cut = Finding {
        len: full.start - f.start + full.len / 2,
        ..f.clone()
    };
    let p = inspect(&source, &cut).preview.unwrap();
    assert_eq!((p.width, p.height), (160, 120));
    // Ending exactly at its last byte keeps it.
    let exact = Finding {
        len: full.start - f.start + full.len,
        ..f
    };
    assert_eq!(inspect(&source, &exact).preview, Some(full));
}

#[test]
fn noise_never_panics_for_any_kind() {
    let img = DiskImage::noise(200_000, 5);
    let file = img.save();
    let source = FileSource::open(file.path()).unwrap();
    let kinds = [
        FileKind::Jpeg,
        FileKind::Mp4,
        FileKind::Mov,
        FileKind::Cr2,
        FileKind::Nef,
        FileKind::Arw,
        FileKind::Dng,
        FileKind::Orf,
        FileKind::Rw2,
        FileKind::Tiff,
        FileKind::Raf,
        FileKind::Cr3,
        FileKind::Heic,
    ];
    for kind in kinds {
        for (start, len) in [
            (0, 200_000),
            (512, 100),
            (199_990, 5000),
            (300_000, 10),
            (0, 0),
        ] {
            let f = Finding {
                id: 1,
                start,
                len,
                kind,
                health: Health::Partial,
                fragments: Vec::new(),
            };
            let _ = inspect(&source, &f);
        }
    }
}

#[test]
fn rw2_preview_is_the_jpg_from_raw() {
    let f = testkit::panasonic_rw2(&testkit::Rw2Options {
        width: 280,
        height: 200,
        raw_format: 4,
        bits: 12,
        raw_len: 65_536,
        strip_len: 0,
        seed: 3,
        exif: exif(),
    });
    let (source, found, _keep) = find(&f.bytes, FileKind::Rw2);
    let i = inspect(&source, &found);
    assert_exif(&i.metadata);
    let p = i.preview.expect("preview");
    let range = f.preview.unwrap();
    assert_eq!(
        (p.start, p.len, p.width, p.height),
        ((AT + range.start) as u64, range.len() as u64, 160, 120)
    );
}

#[test]
fn orf_preview_comes_from_the_olympus_makernote() {
    for om_system in [false, true] {
        let f = testkit::olympus_orf(om_system, 50_000, 4, &exif());
        let (source, found, _keep) = find(&f.bytes, FileKind::Orf);
        let i = inspect(&source, &found);
        assert_exif(&i.metadata);
        let p = i.preview.expect("preview");
        let range = f.preview.unwrap();
        // The larger of the MakerNote's two JPEGs (thumbnail 64x48, preview 320x240).
        assert_eq!(
            (p.start, p.len, p.width, p.height),
            ((AT + range.start) as u64, range.len() as u64, 320, 240),
            "om_system {om_system}"
        );
    }
}

/// A CR3 is judged on the same JPEG `inspect` shows as its preview: the largest of `PRVW` and the full-size JPEG. A
/// full-size JPEG that does not decode cleanly makes the CR3 Partial even though its `PRVW` decodes.
#[test]
fn cr3_health_is_judged_on_the_preview_inspect_shows() {
    let clean = testkit::cr3(4, &exif());
    let (source, f, _keep) = find(&clean, FileKind::Cr3);
    let shown = inspect(&source, &f).preview.unwrap();
    assert_eq!((shown.width, shown.height), (640, 480));
    assert_eq!(f.health, Health::Intact);

    let corruptions: [(&str, Vec<u8>); 2] = [
        ("lenient only", [0xFF, 0x01].repeat(4)),
        ("undecodable", [0xFF, 0xC4].repeat(32)),
    ];
    for (name, patch) in corruptions {
        let mut cr3 = clean.clone();
        let mid = (shown.start + shown.len / 2) as usize - AT;
        cr3[mid..mid + patch.len()].copy_from_slice(&patch);
        let (source, f, _keep) = find(&cr3, FileKind::Cr3);
        assert_eq!(
            (f.len, f.health),
            (cr3.len() as u64, Health::Partial),
            "{name}"
        );
        assert_eq!(inspect(&source, &f).preview, Some(shown.clone()), "{name}");
    }
}

#[test]
fn fragmented_jpeg_inspects_like_the_whole_file() {
    let jpeg = testkit::jpeg_with_exif(640, 480, 3, &exif());
    let len = jpeg.len() as u64;
    let split = (len / 2 / 512 + 1) * 512;
    assert!(split < len);
    let (contiguous_source, contiguous, _keep) = find(&jpeg, FileKind::Jpeg);
    let whole = inspect(&contiguous_source, &contiguous);
    assert!(whole.preview.is_some());

    const SECOND: usize = 1 << 20;
    let mut img = DiskImage::noise(SECOND + jpeg.len() + 8192, 41);
    img.put(AT, &jpeg[..split as usize]);
    img.put(SECOND, &jpeg[split as usize..]);
    let file = img.save();
    let source = FileSource::open(file.path()).unwrap();
    let pieces = vec![
        Fragment {
            start: AT as u64,
            len: split,
        },
        Fragment {
            start: SECOND as u64,
            len: len - split,
        },
    ];
    let f = Finding {
        id: 1,
        start: AT as u64,
        len,
        kind: FileKind::Jpeg,
        health: Health::Intact,
        fragments: pieces.clone(),
    };
    let i = inspect(&source, &f);
    assert_eq!(i.metadata, whole.metadata);
    assert_exif(&i.metadata);
    let p = i.preview.unwrap();
    assert_eq!((p.start, p.len), (AT as u64, len));
    assert_eq!((p.width, p.height), (640, 480));
    assert_eq!(p.fragments, pieces);
    assert_eq!(p.ranges(), pieces);
    // The contiguous file's preview is one range with no pieces.
    let wp = whole.preview.unwrap();
    assert!(wp.fragments.is_empty());
    assert_eq!(
        wp.ranges(),
        vec![Fragment {
            start: AT as u64,
            len
        }]
    );
}
