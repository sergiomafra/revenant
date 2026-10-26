//! The scan with the file systems: `Volume` events, findings with their origin, one finding per file (the merge of the
//! file system's placement with the carving, rulings D3, D5, D6, D7).

use std::io::Write;

use revenant_core::filesystem::FsKind;
use revenant_core::formats::{FileKind, Format, Health, Measurement, all_formats};
use revenant_core::source::{FileSource, Source, VolumeId};
use revenant_core::{
    CancelToken, EngineError, Event, Finding, Fragment, FsState, Placement, ScanOptions,
    ScanSummary, scan,
};
use revenant_testkit::fs::{DeleteStyle, FsBuilder, FsType, Partitioning, disk};
use revenant_testkit::{
    AvcOptions, AvcVideo, Mp4Options, TestExif, TiffRaw, TiffRawOptions, TiffVariant, avc_video,
    jpeg, jpeg_smooth, mp4, tiff_raw,
};

const MIB: usize = 1 << 20;
/// Where every test volume starts (MBR entry at LBA 8192).
const VOL: u64 = 8192 * 512;
const DIR: &str = "DCIM/100MSDCF";

/// FAT32, 48 MiB, 512-byte clusters.
fn card() -> FsBuilder {
    FsBuilder::new(FsType::Fat32, 48 * MIB as u64, 512)
}

/// The volume of `b` at LBA 8192 of a disk 8 MiB longer than the volume.
fn placed(b: &FsBuilder) -> Vec<u8> {
    let v = b.build();
    let len = v.len() + 8 * MIB;
    disk(Partitioning::Mbr { first_lba: 8192 }, &v, len)
}

/// Absolute offset of cluster `c` of `b` once placed.
fn at(b: &FsBuilder, c: u32) -> usize {
    (VOL + b.cluster_offset(c)) as usize
}

/// Absolute pieces of `clusters` (file order) holding `size` bytes, consecutive clusters joined.
fn pieces_of(b: &FsBuilder, clusters: &[u32], size: u64, cluster: u64) -> Vec<Fragment> {
    let mut out: Vec<Fragment> = Vec::new();
    let mut left = size;
    for &c in clusters {
        let len = cluster.min(left);
        if len == 0 {
            break;
        }
        left -= len;
        let start = at(b, c) as u64;
        match out.last_mut() {
            Some(p) if p.start + p.len == start => p.len += len,
            _ => out.push(Fragment { start, len }),
        }
    }
    out
}

/// Fails every read touching one of `bad` (absolute `(start, end)`).
struct Failing {
    inner: FileSource,
    bad: Vec<(u64, u64)>,
}

impl Source for Failing {
    fn len(&self) -> u64 {
        self.inner.len()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, EngineError> {
        let end = offset + buf.len() as u64;
        if self.bad.iter().any(|&(s, e)| offset < e && s < end) {
            return Err(EngineError::Read {
                offset,
                cause: std::io::Error::other("injected"),
            });
        }
        self.inner.read_at(offset, buf)
    }

    fn volume(&self) -> Option<VolumeId> {
        None
    }
}

fn source(bytes: &[u8], bad: &[(u64, u64)]) -> (tempfile::NamedTempFile, Failing) {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(bytes).unwrap();
    file.flush().unwrap();
    let inner = FileSource::open(file.path()).unwrap();
    (
        file,
        Failing {
            inner,
            bad: bad.to_vec(),
        },
    )
}

fn run_on(src: &dyn Source, opts: &ScanOptions) -> (Result<ScanSummary, EngineError>, Vec<Event>) {
    let mut events = Vec::new();
    let r = scan(
        src,
        &all_formats(),
        opts,
        &CancelToken::default(),
        &mut |e| events.push(e),
    );
    (r, events)
}

fn events_with(bytes: &[u8], bad: &[(u64, u64)], opts: &ScanOptions) -> Vec<Event> {
    let (_file, src) = source(bytes, bad);
    let (r, events) = run_on(&src, opts);
    r.unwrap();
    events
}

fn events(bytes: &[u8]) -> Vec<Event> {
    events_with(bytes, &[], &ScanOptions::default())
}

fn founds(events: &[Event]) -> Vec<&Finding> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Found(f) => Some(f),
            _ => None,
        })
        .collect()
}

/// The one `Found` whose origin path is `path`.
fn named<'a>(events: &'a [Event], path: &str) -> &'a Finding {
    let hits: Vec<_> = founds(events)
        .into_iter()
        .filter(|f| f.origin.as_ref().is_some_and(|o| o.path() == path))
        .collect();
    assert_eq!(hits.len(), 1, "{path}: {:#?}", summary(events));
    hits[0]
}

/// (start, kind, health, path) of every `Found`, for failure messages.
fn summary(events: &[Event]) -> Vec<(u64, FileKind, Health, Option<String>)> {
    founds(events)
        .into_iter()
        .map(|f| {
            (
                f.start,
                f.kind,
                f.health,
                f.origin.as_ref().map(|o| o.path()),
            )
        })
        .collect()
}

/// A live `DSC09000.XML` in `DCIM/100MSDCF`: a deleted short name there gets its lost first letter `D` from it (ruling
/// D9); it is no photo, so it makes no finding.
fn sibling(b: &mut FsBuilder) {
    b.add(&format!("{DIR}/DSC09000.XML"), b"<xml/>");
}

fn arw_with_preview() -> TiffRaw {
    tiff_raw(&TiffRawOptions {
        variant: TiffVariant::Arw,
        big_endian: false,
        raw_len: 300_000,
        seed: 7,
        preview: Some((320, 240)),
        preview_sector_aligned: true,
        exif: TestExif::default(),
    })
}

/// The files of `live_files`, as written.
struct Live {
    arw: TiffRaw,
    jpg: Vec<u8>,
    mp4: AvcVideo,
    j36: Vec<u8>,
    j37: Vec<u8>,
}

/// `DCIM/100MSDCF`: DSC01234.ARW (preview sector-aligned), DSC01235.JPG, C0001.MP4 in 2 runs 4 MiB apart, DSC01236.JPG
/// in 2 runs with DSC01237.JPG written whole in the gap (ruling Q7b). `cluster` is the volume's cluster size.
fn live_files(b: &mut FsBuilder, cluster: u64) -> Live {
    let arw = arw_with_preview();
    b.add(&format!("{DIR}/DSC01234.ARW"), &arw.bytes);
    let jpg = jpeg(64, 64, 11, 2000);
    b.add(&format!("{DIR}/DSC01235.JPG"), &jpg);
    let mp4 = avc_video(&AvcOptions {
        samples: 60,
        seed: 5,
        mov: false,
        mdat_pad: 0,
        small_nals: false,
    });
    let need = |len: usize| (len as u64).div_ceil(cluster) as u32;
    let first = (8 * MIB as u64 / cluster) as u32 + 2;
    let gap = (4 * MIB as u64 / cluster) as u32;
    let n = need(mp4.bytes.len());
    let h = n / 2;
    let runs: Vec<u32> = (first..first + h)
        .chain(first + h + gap..first + gap + n)
        .collect();
    b.add_at(&format!("{DIR}/C0001.MP4"), &mp4.bytes, &runs);
    let j36 = jpeg_smooth(1024, 768, 1);
    let j37 = jpeg_smooth(1024, 768, 2);
    let s = (16 * MIB as u64 / cluster) as u32 + 2;
    let (n36, n37) = (need(j36.len()), need(j37.len()));
    let h36 = n36 / 2;
    assert!(h36 >= 1);
    let c37: Vec<u32> = (s + h36..s + h36 + n37).collect();
    let c36: Vec<u32> = (s..s + h36).chain(s + h36 + n37..s + n37 + n36).collect();
    b.add_at(&format!("{DIR}/DSC01236.JPG"), &j36, &c36);
    b.add_at(&format!("{DIR}/DSC01237.JPG"), &j37, &c37);
    Live {
        arw,
        jpg,
        mp4,
        j36,
        j37,
    }
}

/// Every `Found` has an origin and each file of `live_files` comes out once, `Intact`, with its file-system pieces.
fn check_live_files(b: &FsBuilder, live: &Live, ev: &[Event], cluster: u64) {
    let all = founds(ev);
    assert!(all.iter().all(|f| f.origin.is_some()), "{:#?}", summary(ev));
    let files: [(&str, usize); 5] = [
        ("DSC01234.ARW", live.arw.bytes.len()),
        ("DSC01235.JPG", live.jpg.len()),
        ("C0001.MP4", live.mp4.bytes.len()),
        ("DSC01236.JPG", live.j36.len()),
        ("DSC01237.JPG", live.j37.len()),
    ];
    assert_eq!(all.len(), files.len(), "{:#?}", summary(ev));
    for (name, len) in files {
        let path = format!("{DIR}/{name}");
        let f = named(ev, &path);
        assert_eq!(f.health, Health::Intact, "{path}: {f:?}");
        assert_eq!(f.len, len as u64, "{path}");
        let o = f.origin.as_ref().unwrap();
        assert_eq!(o.state, FsState::Live);
        assert_eq!(o.placement, Placement::Chain);
        let pieces = pieces_of(b, &b.clusters(&path), len as u64, cluster);
        assert_eq!(f.start, pieces[0].start, "{path}");
        if pieces.len() == 1 {
            assert!(f.fragments.is_empty(), "{path}");
        } else {
            assert_eq!(f.fragments, pieces, "{path}");
        }
    }
    assert_eq!(named(ev, &format!("{DIR}/C0001.MP4")).fragments.len(), 2);
    assert_eq!(named(ev, &format!("{DIR}/DSC01236.JPG")).fragments.len(), 2);
}

#[test]
fn volume_event_comes_first() {
    let mut b = card();
    b.add(&format!("{DIR}/DSC01235.JPG"), &jpeg(64, 64, 11, 2000));
    let ev = events(&placed(&b));
    match &ev[0] {
        Event::Volume {
            volume,
            listing_truncated,
        } => {
            assert_eq!(volume.fs, FsKind::Fat32);
            assert_eq!(volume.offset, VOL);
            assert_eq!(volume.index, 0);
            assert!(!listing_truncated);
        }
        e => panic!("first event {e:?}"),
    }
    assert!(matches!(ev[1], Event::Progress { .. }), "{:?}", ev[1]);
    assert_eq!(
        ev.iter()
            .filter(|e| matches!(e, Event::Volume { .. }))
            .count(),
        1
    );
}

#[test]
fn live_files_come_out_once_with_names() {
    let mut b = card();
    let live = live_files(&mut b, 512);
    let ev = events(&placed(&b));
    check_live_files(&b, &live, &ev, 512);
    // The ARW's preview is inside an `Intact` finding: no finding of its own.
    let arw = named(&ev, &format!("{DIR}/DSC01234.ARW"));
    let preview = arw.start + live.arw.preview.clone().unwrap().start as u64;
    assert!(founds(&ev).iter().all(|f| f.start != preview));
    let o = arw.origin.as_ref().unwrap();
    assert_eq!(o.name(), "DSC01234.ARW");
    assert_eq!(o.folder(), Some(DIR));
    assert_eq!(o.size, live.arw.bytes.len() as u64);
    assert!(o.modified.is_some());
}

#[test]
fn live_chain_beats_the_gap_search_limits() {
    let mut b = card();
    let j = jpeg_smooth(1024, 768, 1);
    let n = j.len().div_ceil(512) as u32;
    // An 8 KiB first piece at cluster 3000, the rest before it, from cluster 100 (ruling Q8).
    let clusters: Vec<u32> = (3000..3016).chain(100..100 + n - 16).collect();
    b.add_at(&format!("{DIR}/DSC00001.JPG"), &j, &clusters);
    let ev = events(&placed(&b));
    let f = named(&ev, &format!("{DIR}/DSC00001.JPG"));
    assert_eq!(f.health, Health::Intact, "{f:?}");
    assert_eq!(f.fragments, pieces_of(&b, &clusters, j.len() as u64, 512));
    assert_eq!(f.fragments.len(), 2);
}

#[test]
fn deleted_files_keep_their_name_and_health() {
    let mut b = card();
    sibling(&mut b);
    let j = jpeg(64, 64, 11, 2000);
    b.add(&format!("{DIR}/DSC01235.JPG"), &j);
    b.delete(&format!("{DIR}/DSC01235.JPG"), DeleteStyle::Camera);
    let ev = events(&placed(&b));
    let f = named(&ev, &format!("{DIR}/DSC01235.JPG"));
    let o = f.origin.as_ref().unwrap();
    assert_eq!(o.state, FsState::Deleted);
    assert_eq!(o.placement, Placement::Guessed);
    assert_eq!(f.health, Health::Intact);
    assert_eq!(f.len, j.len() as u64);
    assert_eq!(founds(&ev).len(), 1, "{:#?}", summary(&ev));
}

#[test]
fn photo_written_over_a_raw_preview_is_found() {
    let mut b = card();
    sibling(&mut b);
    let arw = arw_with_preview();
    let arw_path = format!("{DIR}/DSC01234.ARW");
    let cl = b.add(&arw_path, &arw.bytes);
    b.delete(&arw_path, DeleteStyle::Camera);
    // The preview starts at a sector of the file, so at one of its 512-byte clusters.
    let preview = arw.preview.clone().unwrap();
    assert_eq!(preview.start % 512, 0);
    let first = cl[preview.start / 512];
    let j = jpeg(64, 64, 11, 2000);
    let n = j.len().div_ceil(512) as u32;
    let jc: Vec<u32> = (first..first + n).collect();
    b.add_at(&format!("{DIR}/DSC09999.JPG"), &j, &jc);
    let ev = events(&placed(&b));
    let photo = named(&ev, &format!("{DIR}/DSC09999.JPG"));
    assert_eq!(photo.origin.as_ref().unwrap().state, FsState::Live);
    assert_eq!(photo.health, Health::Intact);
    assert_eq!(photo.start, at(&b, first) as u64);
    let raw = named(&ev, &arw_path);
    assert_eq!(raw.origin.as_ref().unwrap().state, FsState::Deleted);
    assert_ne!(raw.health, Health::Intact, "{raw:?}");
}

#[test]
fn carved_file_over_live_clusters_is_not_intact() {
    let mut b = card();
    let j = jpeg(64, 64, 21, 0);
    let first = 20_000u32;
    let n = j.len().div_ceil(512) as u32;
    let last = first + n - 1;
    let tail = &j[(n as usize - 1) * 512..];
    b.add_at("MISC/notes.txt", tail, &[last]);
    let mut d = placed(&b);
    let o = at(&b, first);
    d[o..o + j.len()].copy_from_slice(&j);
    let ev = events(&d);
    let all = founds(&ev);
    assert_eq!(all.len(), 1, "{:#?}", summary(&ev));
    assert_eq!(all[0].start, o as u64);
    assert_eq!(all[0].len, j.len() as u64);
    assert!(all[0].origin.is_none());
    assert_eq!(all[0].health, Health::Partial);
}

#[test]
fn forged_live_file_does_not_hide_a_deleted_photo() {
    let mut b = card();
    let j = jpeg(64, 64, 22, 0);
    // DATA.BIN's chain covers the photo, which starts at its 5th cluster.
    let mut data = vec![0u8; 4 * 512];
    data.extend_from_slice(&j);
    data.resize(data.len().div_ceil(512) * 512 + 4096, 0);
    let first = 20_000u32;
    let n = (data.len() / 512) as u32;
    let chain: Vec<u32> = (first..first + n).collect();
    b.add_at("MISC/DATA.BIN", &data, &chain);
    let ev = events(&placed(&b));
    let all = founds(&ev);
    assert_eq!(all.len(), 1, "{:#?}", summary(&ev));
    assert_eq!(all[0].start, at(&b, first + 4) as u64);
    assert!(all[0].origin.is_none());
    assert_eq!(all[0].health, Health::Partial);
}

#[test]
fn live_non_photo_file_consumes_only_its_head() {
    let mut b = card();
    // The THM is itself a JPEG holding a thumbnail JPEG at a sector of its APP1: that measures `Intact` at the THM's
    // first cluster, so its range is skipped and the embedded thumbnail is no finding (ruling R10).
    let thumb = revenant_testkit::jpeg_with_thumbnail_at(32, 32, 3, 1024);
    let inner = jpeg(64, 64, 4, 0);
    let mut data = thumb.clone();
    data.resize(thumb.len().div_ceil(512) * 512, 0);
    let inner_at = data.len();
    data.extend_from_slice(&inner);
    let cl = b.add(&format!("{DIR}/INFO.THM"), &data);
    let ev = events(&placed(&b));
    let all = founds(&ev);
    assert_eq!(all.len(), 1, "{:#?}", summary(&ev));
    assert_eq!(all[0].start, at(&b, cl[inner_at / 512]) as u64);
    assert_eq!(all[0].len, inner.len() as u64);
    assert!(all[0].origin.is_none());
    assert_eq!(all[0].health, Health::Partial);
    assert!(founds(&ev).iter().all(|f| f.start != at(&b, cl[2]) as u64));
}

#[test]
fn dedup_keeps_distinct_files() {
    let mut b = card();
    let mut extra = 1900;
    let d = loop {
        let d = jpeg(16, 16, 1, extra);
        if d.len() > 5 * 512 && d.len() <= 6 * 512 {
            break d;
        }
        extra = if d.len() <= 5 * 512 {
            extra + 64
        } else {
            extra - 32
        };
    };
    let x = jpeg(16, 16, 2, 0);
    assert!(x.len() <= 4 * 512);
    sibling(&mut b);
    let path = format!("{DIR}/DSC00010.JPG");
    let cl = b.add(&path, &d);
    assert_eq!(cl.len(), 6);
    b.delete(&path, DeleteStyle::Camera);
    let mut img = placed(&b);
    let xo = at(&b, cl[2]);
    img[xo..xo + x.len()].copy_from_slice(&x);
    let ev = events(&img);
    let df = named(&ev, &path);
    assert_eq!(df.start, at(&b, cl[0]) as u64);
    assert_eq!(df.origin.as_ref().unwrap().state, FsState::Deleted);
    assert_ne!(df.health, Health::Intact);
    let xf: Vec<_> = founds(&ev)
        .into_iter()
        .filter(|f| f.start == xo as u64)
        .collect();
    assert_eq!(xf.len(), 1, "{:#?}", summary(&ev));
    assert!(xf[0].origin.is_none());
    assert_eq!(xf[0].health, Health::Intact);
    assert_eq!(xf[0].len, x.len() as u64);
}

#[test]
fn fs_size_mismatch_is_not_intact() {
    let j = jpeg_smooth(1024, 768, 1);
    let size = 2 * j.len();
    let noise: Vec<u8> = (0..size as u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    let second = jpeg(64, 64, 5, 0);
    let video = mp4(&Mp4Options {
        mdat_len: 10_000,
        moov_at_end: true,
        mov: false,
        mdat_64bit: false,
    });
    let tails: [(&str, Vec<u8>, Health); 4] = [
        ("noise", noise[..j.len()].to_vec(), Health::Partial),
        ("zeros", vec![0; j.len()], Health::Intact),
        ("mpf", second.clone(), Health::Intact),
        ("motion", video.clone(), Health::Intact),
    ];
    for (what, tail, want) in tails {
        let mut data = j.clone();
        data.extend_from_slice(&tail);
        data.resize(size, 0);
        let mut b = card();
        b.add(&format!("{DIR}/DSC00001.JPG"), &data);
        let ev = events(&placed(&b));
        let f = named(&ev, &format!("{DIR}/DSC00001.JPG"));
        assert_eq!(f.len, size as u64, "{what}");
        assert_eq!(f.kind, FileKind::Jpeg, "{what}");
        assert_eq!(f.health, want, "{what}: {f:?}");
    }
}

#[test]
fn corrupt_count_in_an_fs_file_is_bounded_by_its_size() {
    let mut arw = tiff_raw(&TiffRawOptions {
        variant: TiffVariant::Arw,
        big_endian: false,
        raw_len: 200_000,
        seed: 9,
        preview: None,
        preview_sector_aligned: false,
        exif: TestExif::default(),
    });
    // The raw strip's StripByteCounts entry (tag 279, LONG, count 1) holds the raw length.
    let mut entry = vec![0x17, 0x01, 0x04, 0x00, 0x01, 0x00, 0x00, 0x00];
    entry.extend_from_slice(&(arw.raw.len() as u32).to_le_bytes());
    let pos = arw
        .bytes
        .windows(12)
        .position(|w| w == entry.as_slice())
        .expect("StripByteCounts entry");
    arw.bytes[pos + 8..pos + 12].copy_from_slice(&0x3000_0000u32.to_le_bytes());
    let mut b = card();
    b.add(&format!("{DIR}/DSC01234.ARW"), &arw.bytes);
    let ev = events(&placed(&b));
    let f = named(&ev, &format!("{DIR}/DSC01234.ARW"));
    assert_eq!(f.len, arw.bytes.len() as u64);
    assert_eq!(f.health, Health::Partial, "{f:?}");
}

#[test]
fn carving_join_of_another_size_is_not_used() {
    let a = jpeg_smooth(1024, 768, 1);
    let c = (a.len() / 2).div_ceil(512) * 512;
    let size = a.len() + 4096;
    let mut data = a[..c].to_vec();
    data.extend((0..(size - c) as u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 11) as u8));
    let mut b = card();
    sibling(&mut b);
    let path = format!("{DIR}/DSC00002.JPG");
    let cl = b.add(&path, &data);
    b.delete(&path, DeleteStyle::Camera);
    let mut img = placed(&b);
    let tail_at = at(&b, cl[0]) + 2 * MIB;
    img[tail_at..tail_at + a.len() - c].copy_from_slice(&a[c..]);
    // Without the file system, the carving joins the photo `Intact` at the entry's start.
    let carved = events_with(
        &img,
        &[],
        &ScanOptions {
            filesystems: false,
            ..ScanOptions::default()
        },
    );
    let joined = founds(&carved)
        .into_iter()
        .find(|f| f.start == at(&b, cl[0]) as u64)
        .expect("carved");
    assert_eq!(
        (joined.health, joined.len),
        (Health::Intact, a.len() as u64)
    );
    let ev = events(&img);
    let f = named(&ev, &path);
    let o = f.origin.as_ref().unwrap();
    assert_eq!(
        (o.state, o.placement),
        (FsState::Deleted, Placement::Guessed)
    );
    assert_ne!(f.health, Health::Intact);
    assert!(f.fragments.is_empty());
    assert_eq!(f.len, size as u64);
}

#[test]
fn deleted_guess_prefers_a_better_carving_of_the_same_size() {
    let d = jpeg_smooth(1024, 768, 3);
    let live = jpeg(64, 64, 9, 0);
    let (nd, nl) = (
        d.len().div_ceil(512) as u32,
        live.len().div_ceil(512) as u32,
    );
    let (x, h) = (1000u32, 60u32);
    let dc: Vec<u32> = (x..x + h).chain(x + h + nl..x + nl + nd).collect();
    let lc: Vec<u32> = (x + h..x + h + nl).collect();
    let mut b = card();
    let path = format!("{DIR}/DSC00002.JPG");
    b.add_at(&path, &d, &dc);
    b.add_at(&format!("{DIR}/DSC00001.JPG"), &live, &lc);
    b.delete(&path, DeleteStyle::Camera);
    let ev = events(&placed(&b));
    let f = named(&ev, &path);
    assert_eq!(f.health, Health::Intact, "{f:?}");
    assert_eq!(f.fragments, pieces_of(&b, &dc, d.len() as u64, 512));
    let o = f.origin.as_ref().unwrap();
    assert_eq!(
        (o.state, o.placement),
        (FsState::Deleted, Placement::Carved)
    );
    assert_eq!(
        named(&ev, &format!("{DIR}/DSC00001.JPG")).health,
        Health::Intact
    );
}

#[test]
fn fs_events_identical_across_threads_and_caches() {
    let mut b = card();
    let live = live_files(&mut b, 512);
    let gone = jpeg(64, 64, 31, 0);
    b.add(&format!("{DIR}/DSC00100.JPG"), &gone);
    let raw = arw_with_preview();
    b.add(&format!("{DIR}/DSC00101.ARW"), &raw.bytes);
    b.delete(&format!("{DIR}/DSC00100.JPG"), DeleteStyle::Camera);
    b.delete(&format!("{DIR}/DSC00101.ARW"), DeleteStyle::Camera);
    let img = placed(&b);
    let jpg = at(&b, b.clusters(&format!("{DIR}/DSC01235.JPG"))[0]) as u64 + 1024;
    // A sector of the first FAT (FAT32: 32 reserved sectors).
    let fat = VOL + 32 * 512 + 3 * 512;
    let bad = [(jpg, jpg + 512), (fat, fat + 512)];
    let opts = |threads, cache_bytes| ScanOptions {
        threads,
        cache_bytes,
        ..ScanOptions::default()
    };
    let base = events_with(&img, &bad, &opts(1, 256 * MIB));
    assert!(base.iter().any(|e| matches!(e, Event::Unreadable { .. })));
    let j = named(&base, &format!("{DIR}/DSC01235.JPG"));
    assert_eq!(j.health, Health::Partial);
    assert_eq!(j.len, live.jpg.len() as u64);
    named(&base, &format!("{DIR}/DSC00100.JPG"));
    named(&base, &format!("{DIR}/DSC00101.ARW"));
    for (threads, cache) in [
        (2, 256 * MIB),
        (8, 256 * MIB),
        (16, 256 * MIB),
        (8, MIB),
        (1, MIB),
        (8, 256 * MIB),
        (8, 256 * MIB),
    ] {
        assert_eq!(
            events_with(&img, &bad, &opts(threads, cache)),
            base,
            "threads {threads}, cache {cache}"
        );
    }
}

#[test]
fn cancel_inside_the_first_volume_callback() {
    let mut b = card();
    b.add(&format!("{DIR}/DSC01235.JPG"), &jpeg(64, 64, 11, 2000));
    let (_file, src) = source(&placed(&b), &[]);
    let cancel = CancelToken::default();
    let mut seen = Vec::new();
    let r = scan(
        &src,
        &all_formats(),
        &ScanOptions::default(),
        &cancel,
        &mut |e| {
            if matches!(e, Event::Volume { .. }) {
                cancel.cancel();
            }
            seen.push(e);
        },
    );
    assert!(matches!(r, Err(EngineError::Cancelled)), "{r:?}");
    assert_eq!(seen.len(), 1);
    assert!(matches!(seen[0], Event::Volume { .. }));
}

#[test]
fn filesystems_off_is_the_old_scan() {
    let mut b = card();
    live_files(&mut b, 512);
    let img = placed(&b);
    let off = events_with(
        &img,
        &[],
        &ScanOptions {
            filesystems: false,
            ..ScanOptions::default()
        },
    );
    assert!(!off.iter().any(|e| matches!(e, Event::Volume { .. })));
    assert!(founds(&off).iter().all(|f| f.origin.is_none()));
    // The same card with its partition table and boot sectors erased: no volume is found, so the scan is the
    // carving alone.
    let mut erased = img.clone();
    erased[..512].fill(0);
    erased[VOL as usize..VOL as usize + 32 * 512].fill(0);
    let carving = events(&erased);
    assert!(!carving.iter().any(|e| matches!(e, Event::Volume { .. })));
    assert_eq!(off, carving);
    assert!(!founds(&off).is_empty());
}

#[test]
fn exfat_card_works_the_same() {
    let cluster = 32 * 1024;
    let mut b = FsBuilder::new(FsType::Exfat, 32 * MIB as u64, cluster as u32);
    let live = live_files(&mut b, cluster);
    let ev = events(&placed(&b));
    match &ev[0] {
        Event::Volume { volume, .. } => assert_eq!(volume.fs, FsKind::Exfat),
        e => panic!("first event {e:?}"),
    }
    check_live_files(&b, &live, &ev, cluster);
}

#[test]
fn moov_inside_a_live_file_is_no_orphan() {
    // A video with no entry cut in two: its first piece in free clusters, the rest (with its `moov`) as the contents of a
    // live MISC/DATA.BIN. The `moov` belongs to the live file, so the video is not paired with it.
    let v = avc_video(&AvcOptions {
        samples: 120,
        seed: 3,
        mov: false,
        mdat_pad: 0,
        small_nals: true,
    });
    let p = &v.mdat_payload;
    let l1 = (p.start + p.len() * 2 / 5).div_ceil(512) * 512;
    let mut b = card();
    let rest = &v.bytes[l1..];
    let n = rest.len().div_ceil(512) as u32;
    let chain: Vec<u32> = (40_000..40_000 + n).collect();
    b.add_at("MISC/DATA.BIN", rest, &chain);
    let mut img = placed(&b);
    let first = at(&b, 20_000);
    img[first..first + l1].copy_from_slice(&v.bytes[..l1]);
    let updated = |ev: &[Event]| ev.iter().filter(|e| matches!(e, Event::Updated(_))).count();
    let off = events_with(
        &img,
        &[],
        &ScanOptions {
            filesystems: false,
            ..ScanOptions::default()
        },
    );
    assert_eq!(updated(&off), 1, "the carving alone pairs them");
    let ev = events(&img);
    assert_eq!(updated(&ev), 0);
    let video = founds(&ev)
        .into_iter()
        .find(|f| f.start == first as u64)
        .expect("the video's first piece");
    assert_eq!(video.health, Health::Damaged);
}

/// A listed RW2 that measures `Intact` but whose merged finding is only `Partial` (here bytes that are neither padding
/// nor files after its structure, ruling D6) still hides its own JpgFromRaw, and the thumbnail inside it (ruling R24).
#[test]
fn listed_partial_raw_hides_its_own_jpegs() {
    let r = revenant_testkit::panasonic_rw2(&revenant_testkit::Rw2Options {
        width: 280,
        height: 200,
        raw_format: 4,
        bits: 12,
        raw_len: 65_536,
        strip_len: 0,
        seed: 3,
        exif: TestExif::default(),
    });
    let mut data = r.bytes.clone();
    let mut x = 5u64;
    data.extend((0..3000).map(|_| {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x as u8
    }));
    let mut b = card();
    let path = format!("{DIR}/P1000001.RW2");
    b.add(&path, &data);
    let ev = events(&placed(&b));
    let raw = named(&ev, &path);
    assert_eq!((raw.kind, raw.health), (FileKind::Rw2, Health::Partial));
    assert_eq!(founds(&ev).len(), 1, "{:#?}", summary(&ev));
}

// ---- After a quick format: names from the old folders (ruling D11) ----

/// The volume of `b`, quick-formatted, at LBA 8192 of a disk 8 MiB longer than the volume.
fn quick_placed(b: &FsBuilder) -> Vec<u8> {
    let v = b.build_quick_formatted();
    let len = v.len() + 8 * MIB;
    disk(Partitioning::Mbr { first_lba: 8192 }, &v, len)
}

fn updates(events: &[Event]) -> Vec<&Finding> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Updated(f) => Some(f),
            _ => None,
        })
        .collect()
}

fn summary_of(bytes: &[u8], opts: &ScanOptions) -> (ScanSummary, Vec<Event>) {
    let (_file, src) = source(bytes, &[]);
    let (r, events) = run_on(&src, opts);
    (r.unwrap(), events)
}

/// `DCIM/100MSDCF`: DSC01234.JPG, DSC01235.ARW, C0001.MP4, each contiguous; their bytes by name.
fn quick_files(b: &mut FsBuilder) -> Vec<(&'static str, Vec<u8>)> {
    let files = vec![
        ("DSC01234.JPG", jpeg(64, 64, 11, 2000)),
        ("DSC01235.ARW", arw_with_preview().bytes),
        (
            "C0001.MP4",
            avc_video(&AvcOptions {
                samples: 60,
                seed: 5,
                mov: false,
                mdat_pad: 0,
                small_nals: false,
            })
            .bytes,
        ),
    ];
    for (name, data) in &files {
        b.add(&format!("{DIR}/{name}"), data);
    }
    files
}

/// Each file of `files` is found once without an origin, then updated once with its old name: the same finding, with
/// `origin { state: Orphaned, placement: Carved, path "?/100MSDCF/<name>" }`.
fn check_quick(b: &FsBuilder, files: &[(&str, Vec<u8>)], img: &[u8]) {
    let (sum, ev) = summary_of(img, &ScanOptions::default());
    let found = founds(&ev);
    assert_eq!(found.len(), files.len(), "{:#?}", summary(&ev));
    assert!(
        found.iter().all(|f| f.origin.is_none()),
        "{:#?}",
        summary(&ev)
    );
    let up = updates(&ev);
    assert_eq!(up.len(), files.len(), "{up:#?}");
    assert_eq!(sum.updated, files.len() as u64);
    // Every `Updated` comes after every `Found`, in id order.
    let first_update = ev
        .iter()
        .position(|e| matches!(e, Event::Updated(_)))
        .unwrap();
    assert!(
        ev[first_update..]
            .iter()
            .all(|e| !matches!(e, Event::Found(_)))
    );
    assert!(up.windows(2).all(|w| w[0].id < w[1].id));
    for (name, data) in files {
        let start = at(b, b.clusters(&format!("{DIR}/{name}"))[0]) as u64;
        let f = found.iter().find(|f| f.start == start).expect(name);
        let u = up.iter().find(|u| u.id == f.id).expect(name);
        let o = u.origin.as_ref().expect("an origin");
        assert_eq!(o.path(), format!("?/100MSDCF/{name}"));
        assert_eq!(o.folder(), Some("?/100MSDCF"));
        assert_eq!(o.state, FsState::Orphaned);
        assert_eq!(o.placement, Placement::Carved);
        assert_eq!(o.size, data.len() as u64);
        assert_eq!(o.volume, 0);
        assert!(o.modified.is_some());
        // Health and everything else unchanged.
        assert_eq!(
            &Finding {
                origin: None,
                ..(*u).clone()
            },
            *f
        );
        assert_eq!(f.health, Health::Intact, "{name}");
    }
}

#[test]
fn quick_formatted_fat32_card_keeps_names() {
    let mut b = card();
    let files = quick_files(&mut b);
    check_quick(&b, &files, &quick_placed(&b));
}

#[test]
fn quick_formatted_exfat_card_keeps_names() {
    let mut b = FsBuilder::new(FsType::Exfat, 32 * MIB as u64, 32 * 1024);
    let files = quick_files(&mut b);
    check_quick(&b, &files, &quick_placed(&b));
}

/// A JPEG and C0001.MP4 written in 2 runs 4 MiB apart (its `moov` in the second), then a quick format.
fn quick_split_video(cluster: u64, b: &mut FsBuilder) -> AvcVideo {
    b.add(&format!("{DIR}/DSC01234.JPG"), &jpeg(64, 64, 11, 2000));
    let mp4 = avc_video(&AvcOptions {
        samples: 60,
        seed: 5,
        mov: false,
        mdat_pad: 0,
        small_nals: false,
    });
    let n = (mp4.bytes.len() as u64).div_ceil(cluster) as u32;
    let first = (8 * MIB as u64 / cluster) as u32 + 2;
    let gap = (4 * MIB as u64 / cluster) as u32;
    let h = n / 2;
    let runs: Vec<u32> = (first..first + h)
        .chain(first + h + gap..first + gap + n)
        .collect();
    b.add_at(&format!("{DIR}/C0001.MP4"), &mp4.bytes, &runs);
    mp4
}

#[test]
fn orphan_events_identical_across_threads_and_caches() {
    let mut b = card();
    quick_split_video(512, &mut b);
    let img = quick_placed(&b);
    let opts = |threads, cache_bytes| ScanOptions {
        threads,
        cache_bytes,
        ..ScanOptions::default()
    };
    let base = events_with(&img, &[], &opts(1, 256 * MIB));
    let up = updates(&base);
    assert_eq!(up.len(), 2, "{up:#?}");
    assert!(up.iter().all(|u| u.origin.is_some()));
    assert!(up.iter().any(|u| u.fragments.len() == 2));
    for (threads, cache) in [
        (2, 256 * MIB),
        (8, 256 * MIB),
        (16, 256 * MIB),
        (8, MIB),
        (1, MIB),
    ] {
        assert_eq!(
            events_with(&img, &[], &opts(threads, cache)),
            base,
            "threads {threads}, cache {cache}"
        );
    }
}

/// A recorded size past the photo: noise after it makes the named finding `Partial` (ruling D6); padding after it, or
/// the exact size, keeps it `Intact`.
#[test]
fn orphan_entry_size_mismatch_is_partial() {
    let mut b = card();
    let jpg = jpeg(64, 64, 11, 2000);
    let mut noisy = jpg.clone();
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    noisy.extend((0..jpg.len() / 2).map(|_| {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x as u8
    }));
    let mut padded = jpeg(64, 64, 12, 2000);
    let clean = padded.len();
    padded.resize(clean * 3 / 2, 0);
    b.add(&format!("{DIR}/NOISE.JPG"), &noisy);
    b.add(&format!("{DIR}/PADDED.JPG"), &padded);
    b.add(&format!("{DIR}/EXACT.JPG"), &jpeg(64, 64, 13, 2000));
    let ev = events(&quick_placed(&b));
    let health = |name: &str| {
        let u = updates(&ev)
            .into_iter()
            .find(|u| u.origin.as_ref().is_some_and(|o| o.name() == name))
            .unwrap_or_else(|| panic!("{name}: {:#?}", updates(&ev)));
        let f = founds(&ev).into_iter().find(|f| f.id == u.id).unwrap();
        assert_eq!(f.health, Health::Intact, "{name} as carved");
        u.health
    };
    assert_eq!(health("NOISE.JPG"), Health::Partial);
    assert_eq!(health("PADDED.JPG"), Health::Intact);
    assert_eq!(health("EXACT.JPG"), Health::Intact);
}

#[test]
fn orphan_finder_ignores_live_and_noise() {
    // The card before its quick format: every folder is live, every photo has its live origin.
    let mut b = card();
    quick_files(&mut b);
    let ev = events(&placed(&b));
    assert!(updates(&ev).is_empty(), "{:#?}", updates(&ev));
    // A deleted folder (walked by the file-system phase, not live) whose photo's clusters a live file took over: the
    // photo found inside that file has no origin, and the folder's intact entry KEEP.JPG pointing at it (left intact
    // by a raw entry, as a crashed delete would) does not name it: a walked directory is no old folder.
    let mut b = card();
    b.mkdir("MISC");
    let jpg = jpeg(64, 64, 21, 0);
    let clusters = b.add(&format!("{DIR}/OLD/DSC09999.JPG"), &jpg);
    let mut keep = [0u8; 32];
    keep[..11].copy_from_slice(b"KEEP    JPG");
    keep[11] = 0x20;
    keep[20..22].copy_from_slice(&((clusters[0] >> 16) as u16).to_le_bytes());
    keep[26..28].copy_from_slice(&(clusters[0] as u16).to_le_bytes());
    keep[28..32].copy_from_slice(&(jpg.len() as u32).to_le_bytes());
    b.raw_entry(&format!("{DIR}/OLD"), keep);
    b.delete(&format!("{DIR}/OLD"), DeleteStyle::Camera);
    let mut data = vec![0x5Au8; 512];
    data.extend_from_slice(&jpg);
    let mut chain = vec![30_000];
    chain.extend_from_slice(&clusters);
    b.add_at("MISC/DATA.BIN", &data, &chain);
    let ev = events(&placed(&b));
    let inside = founds(&ev)
        .into_iter()
        .find(|f| f.start == at(&b, clusters[0]) as u64)
        .unwrap_or_else(|| panic!("{:#?}", summary(&ev)));
    assert!(inside.origin.is_none());
    assert!(updates(&ev).is_empty(), "{:#?}", updates(&ev));
    // Noise with fake "." and ".." entries every 512 bytes and no volume.
    let mut noise = vec![0u8; 16 * MIB];
    let mut x = 0x2545_F491_4F6C_DD1Du64;
    for byte in noise.iter_mut() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *byte = x as u8;
    }
    for s in (0..noise.len()).step_by(512) {
        noise[s..s + 11].copy_from_slice(b".          ");
        noise[s + 11] = 0x10;
        noise[s + 32..s + 43].copy_from_slice(b"..         ");
        noise[s + 43] = 0x10;
        noise[s + 64..s + 75].copy_from_slice(b"DSC00001JPG");
        noise[s + 75] = 0x20;
    }
    let j = jpeg(64, 64, 22, 0);
    noise[MIB..MIB + j.len()].copy_from_slice(&j);
    let ev = events(&noise);
    assert!(!ev.iter().any(|e| matches!(e, Event::Volume { .. })));
    assert!(!founds(&ev).is_empty());
    assert!(updates(&ev).is_empty(), "{:#?}", updates(&ev));
}

#[test]
fn orphan_name_and_pairing_merge_into_one_update() {
    let mut b = card();
    let mp4 = quick_split_video(512, &mut b);
    let (sum, ev) = summary_of(&quick_placed(&b), &ScanOptions::default());
    let start = at(&b, b.clusters(&format!("{DIR}/C0001.MP4"))[0]) as u64;
    let video = founds(&ev)
        .into_iter()
        .find(|f| f.start == start)
        .expect("the video's first piece");
    assert_eq!(video.health, Health::Damaged);
    let mine: Vec<_> = updates(&ev)
        .into_iter()
        .filter(|u| u.id == video.id)
        .collect();
    assert_eq!(mine.len(), 1, "{:#?}", updates(&ev));
    let u = mine[0];
    assert_eq!(u.fragments.len(), 2);
    assert_eq!(u.len, mp4.bytes.len() as u64);
    let o = u.origin.as_ref().expect("an origin");
    assert_eq!(o.path(), "?/100MSDCF/C0001.MP4");
    assert_eq!(o.state, FsState::Orphaned);
    // The JPEG's update and the video's: one `Updated` each.
    assert_eq!(sum.updated, 2);
    assert_eq!(updates(&ev).len(), 2);
}

/// Test-only format: a sector starting with `BLINDVID` is an `Intact` MP4 running to the end of its source, measured
/// without reading past the head, like a detector that never reads most of a file's data.
struct Blind;

impl Format for Blind {
    fn name(&self) -> &'static str {
        "blind"
    }

    fn recognizes(&self, head: &[u8]) -> bool {
        head.starts_with(b"BLINDVID")
    }

    fn confirms(&self, _source: &dyn Source, _start: u64) -> bool {
        true
    }

    fn measure(&self, source: &dyn Source, start: u64) -> Option<Measurement> {
        Some(Measurement {
            len: source.len() - start,
            kind: FileKind::Mp4,
            health: Health::Intact,
            fragments: Vec::new(),
        })
    }
}

/// A live video whose second piece lies before its first (the FAT allocator wrapped around on a reused card), with an
/// unreadable sector in that second piece: the scan passes the sector before it reaches the file's start and the
/// detector never reads those bytes, yet the file is `Partial`, the same for any `threads` and `cache_bytes`.
#[test]
fn bad_sector_in_an_earlier_piece_lowers_a_listed_file() {
    let mut b = card();
    let mut video = vec![0x5Au8; 64 * 512];
    video[..8].copy_from_slice(b"BLINDVID");
    let late = (24 * MIB as u64 / 512) as u32 + 2;
    let early = (8 * MIB as u64 / 512) as u32 + 2;
    let runs: Vec<u32> = (late..late + 32).chain(early..early + 32).collect();
    let path = format!("{DIR}/C0002.MP4");
    b.add_at(&path, &video, &runs);
    let img = placed(&b);
    let sector = (at(&b, early) + 8 * 512) as u64;
    let bad = [(sector, sector + 512)];
    let run = |bad: &[(u64, u64)], threads, cache_bytes| {
        let (_file, src) = source(&img, bad);
        let mut events = Vec::new();
        let formats: Vec<Box<dyn Format>> = vec![Box::new(Blind)];
        let opts = ScanOptions {
            threads,
            cache_bytes,
            ..ScanOptions::default()
        };
        scan(&src, &formats, &opts, &CancelToken::default(), &mut |e| {
            events.push(e)
        })
        .unwrap();
        events
    };
    let clean = run(&[], 1, 256 * MIB);
    assert_eq!(named(&clean, &path).health, Health::Intact);
    let base = run(&bad, 1, 256 * MIB);
    assert!(
        base.iter()
            .any(|e| matches!(e, Event::Unreadable { start, .. } if *start == sector))
    );
    let f = named(&base, &path);
    assert_eq!(f.health, Health::Partial, "{f:?}");
    assert_eq!(f.len, video.len() as u64);
    assert_eq!(f.fragments.len(), 2);
    assert!(f.fragments[1].start < f.start);
    for (threads, cache) in [
        (2, 256 * MIB),
        (8, 256 * MIB),
        (16, 256 * MIB),
        (8, MIB),
        (1, MIB),
    ] {
        assert_eq!(
            run(&bad, threads, cache),
            base,
            "threads {threads}, cache {cache}"
        );
    }
}

/// 1.4 T7: a deleted file whose first cluster lies inside an `Intact` finding's range (a skip range) is merged without
/// the carving at its start, even when that carving would win: its placement stays `Guessed`, never `Carved`.
#[test]
fn carving_at_a_skipped_start_is_not_merged() {
    // A Motion-JPEG MOV whose second chunk (one sector in) is a JPEG frame: `Intact`, the frame being its own data.
    let frame = jpeg_smooth(256, 192, 5);
    let mut payload = vec![0x11u8; 484];
    payload.extend_from_slice(&frame);
    payload.resize(payload.len() + 3000, 0x22);
    let mov = revenant_testkit::mp4_with(
        &Mp4Options {
            mdat_len: 0,
            moov_at_end: true,
            mov: true,
            mdat_64bit: false,
        },
        b"mjpa",
        &payload,
        &[0, 484],
    );
    assert_eq!(
        &mov[512..515],
        &[0xFF, 0xD8, 0xFF],
        "the frame starts one sector in"
    );
    // A deleted DSC00002.JPG of the frame's size at the frame's sector (cluster X + 1). One cluster in the middle of
    // its span is allocated in the FAT with no owner (lost), so the guess skips it: a placement worse than the carving.
    let x = 50_000u32;
    let n = frame.len().div_ceil(512) as u32;
    let mut b = card();
    sibling(&mut b);
    let path = format!("{DIR}/DSC00002.JPG");
    b.add_at(&path, &frame, &(x + 1..x + 1 + n).collect::<Vec<_>>());
    b.delete(&path, DeleteStyle::Camera);
    b.set_fat(x + 4, 0x0FFF_FFFF);
    let mut img = placed(&b);
    img[at(&b, x)..at(&b, x) + mov.len()].copy_from_slice(&mov);
    let ev = events(&img);
    let movie = founds(&ev)
        .into_iter()
        .find(|f| f.start == at(&b, x) as u64)
        .expect("the MOV");
    assert_eq!(
        (movie.kind, movie.health, movie.len),
        (FileKind::Mov, Health::Intact, mov.len() as u64)
    );
    let f = named(&ev, &path);
    let o = f.origin.as_ref().unwrap();
    assert_eq!(
        (o.state, o.placement),
        (FsState::Deleted, Placement::Guessed),
        "{f:?}"
    );
}
