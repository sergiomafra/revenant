//! The reassembly step of the scan (part 1.6): fragmented JPEGs put back together after the scan and the pairing, as
//! `Updated` events; never over a live file or another `Intact` file, never re-reading a known bad sector, bounded on a
//! hostile card, cancelled promptly, the same for any `threads` and `cache_bytes`.

mod support;

use std::sync::Mutex;

use revenant_core::formats::{FileKind, Health, all_formats};
use revenant_core::source::{Source, VolumeId};
use revenant_core::{
    CancelToken, EngineError, Event, Finding, Fragment, FsState, ScanOptions, ScanSummary, extract,
    scan,
};
use revenant_testkit::fs::{DeleteStyle, FsBuilder};
use revenant_testkit::{jpeg, jpeg_smooth};
use support::cards::{ThreePiece, live_in_three_runs, three_piece};
use support::{SimSource, VOLUME_AT, exfat_card, final_view, on_disk};

const KIB: u64 = 1 << 10;
const MIB: u64 = 1 << 20;
const CLUSTER: u64 = 32 * KIB;

fn opts(threads: usize, cache_bytes: usize) -> ScanOptions {
    ScanOptions {
        threads,
        cache_bytes,
        ..ScanOptions::default()
    }
}

fn run(src: &dyn Source, opts: &ScanOptions) -> (Result<ScanSummary, EngineError>, Vec<Event>) {
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

fn events_of(src: &dyn Source, opts: &ScanOptions) -> Vec<Event> {
    let (r, events) = run(src, opts);
    r.unwrap();
    events
}

/// The final finding starting at `start`.
fn at(view: &std::collections::BTreeMap<u64, Finding>, start: u64) -> &Finding {
    view.values()
        .find(|f| f.start == start)
        .unwrap_or_else(|| panic!("a finding at {start}: {view:#?}"))
}

/// The bytes of `finding`, extracted through the public API.
fn extracted(src: &dyn Source, finding: &Finding) -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    let path = extract(src, finding, dir.path()).unwrap();
    std::fs::read(path).unwrap()
}

fn updated(events: &[Event]) -> Vec<&Finding> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Updated(f) => Some(f),
            _ => None,
        })
        .collect()
}

#[test]
fn three_piece_jpeg_on_a_quick_formatted_card_is_reassembled() {
    let ThreePiece {
        bytes,
        r,
        r_pieces,
        q,
        q_pieces,
        ..
    } = three_piece();
    let src = SimSource::new(bytes);
    let events = events_of(&src, &ScanOptions::default());
    let view = final_view(&events);
    // R is put back together in its 3 pieces, by content alone: at most `Partial` (ruling Q37), with R's exact bytes.
    let rf = at(&view, r_pieces[0].start);
    assert_eq!(rf.kind, FileKind::Jpeg);
    assert_eq!(rf.fragments, r_pieces, "{rf:#?}");
    assert_eq!(rf.len, r.len() as u64);
    assert_eq!(rf.health, Health::Partial);
    assert!(
        rf.origin.is_none(),
        "no name is left after the quick format"
    );
    assert!(
        updated(&events).iter().any(|u| u.id == rf.id),
        "R comes out as an Updated"
    );
    assert_eq!(extracted(&src, rf), r);
    // Q, in 2 pieces, is found whole by the 1.3 two-piece join (`Partial`: one cluster per piece). The reassembly finds
    // the same 2 pieces, at most `Partial` by content alone (Q37): nothing better, so no `Updated`.
    let qf = at(&view, q_pieces[0].start);
    assert_eq!(qf.fragments, q_pieces, "{qf:#?}");
    assert_eq!(qf.health, Health::Partial);
    assert!(updated(&events).iter().all(|u| u.id != qf.id));
    assert_eq!(extracted(&src, qf), q);
}

#[test]
fn reassembly_events_identical_across_threads_and_caches() {
    let src = SimSource::new(three_piece().bytes);
    let base = events_of(&src, &opts(1, 256 << 20));
    assert!(!updated(&base).is_empty());
    for threads in [1, 2, 8, 16] {
        for cache in [256 << 20, 1 << 20] {
            assert_eq!(
                events_of(&src, &opts(threads, cache)),
                base,
                "threads {threads}, cache {cache}"
            );
        }
    }
}

/// Absolute pieces of `clusters` (file order) holding `size` bytes of a volume on [`on_disk`], consecutive ones joined.
fn pieces_of(b: &FsBuilder, clusters: &[u32], size: u64) -> Vec<Fragment> {
    let mut out: Vec<Fragment> = Vec::new();
    let mut left = size;
    for &c in clusters {
        let len = CLUSTER.min(left);
        left -= len;
        let start = VOLUME_AT + b.cluster_offset(c);
        match out.last_mut() {
            Some(p) if p.start + p.len == start => p.len += len,
            _ => out.push(Fragment { start, len }),
        }
    }
    out
}

fn overlap(a: &Fragment, b: &Fragment) -> bool {
    a.start < b.start + b.len && b.start < a.start + a.len
}

/// Clusters `next..next + n`, moving `next` past them.
fn take(next: &mut u32, n: u32) -> Vec<u32> {
    let c: Vec<u32> = (*next..*next + n).collect();
    *next += n;
    c
}

fn clusters_for(len: usize) -> u32 {
    (len as u64).div_ceil(CLUSTER) as u32
}

#[test]
fn pieces_never_overlap_live_files_or_another_intact() {
    // Live photos L1..L4 around 3 deleted photos D0..D2 in 3, 2 and 2 FAT-chained runs (exFAT, Camera delete: the
    // chains stay in the FAT, the bitmap frees them), and a photo C left by an earlier use of the card (no entry) in 3
    // runs: after C1 comes L1, then COPY.BIN, a live file holding a copy of C2's bytes, and C's true second run lies
    // further on. The copy is the first candidate after C1; being live, it is never a piece. Each of C's runs ends
    // where a live photo starts (a run followed at once by another smooth photo's run decodes on into it: the walk
    // finds no break there, and the search then only gets a `Partial` path).
    let mut b = exfat_card();
    let lives: Vec<Vec<u8>> = (1..=4).map(|k| jpeg(512, 384, 20 + k, 0)).collect();
    let dels = [
        jpeg_smooth(2048, 1536, 31),
        jpeg_smooth(1600, 1200, 32),
        jpeg_smooth(1600, 1200, 33),
    ];
    let c = jpeg_smooth(2048, 1536, 34);
    let first = b.add("L0.JPG", &jpeg(256, 256, 19, 0));
    let mut next = *first.last().unwrap() + 1;
    let n: Vec<u32> = dels.iter().map(|d| clusters_for(d.len())).collect();
    let nc = clusters_for(c.len());
    let (d1, d2, d3, c1) = (n[0] / 3, n[1] / 2, n[2] / 2, nc / 3);
    let mut dc: [Vec<u32>; 3] = Default::default();
    let mut lc: Vec<Vec<u32>> = Vec::new();
    let mut cc = take(&mut next, c1);
    lc.push(take(&mut next, clusters_for(lives[0].len())));
    let copy = take(&mut next, c1);
    dc[0].extend(take(&mut next, d1));
    lc.push(take(&mut next, clusters_for(lives[1].len())));
    dc[1].extend(take(&mut next, d2));
    dc[0].extend(take(&mut next, d1));
    cc.extend(take(&mut next, c1));
    lc.push(take(&mut next, clusters_for(lives[2].len())));
    dc[2].extend(take(&mut next, d3));
    dc[1].extend(take(&mut next, n[1] - d2));
    cc.extend(take(&mut next, nc - 2 * c1));
    lc.push(take(&mut next, clusters_for(lives[3].len())));
    dc[0].extend(take(&mut next, n[0] - 2 * d1));
    dc[2].extend(take(&mut next, n[2] - d3));
    for (k, d) in dels.iter().enumerate() {
        b.add_at(&format!("D{k}.JPG"), d, &dc[k]);
    }
    for (k, l) in lives.iter().enumerate() {
        b.add_at(&format!("L{}.JPG", k + 1), l, &lc[k]);
    }
    let c2 = (c1 as usize * CLUSTER as usize)..(2 * c1 as usize * CLUSTER as usize);
    b.add_at("COPY.BIN", &c[c2], &copy);
    let mut live_pieces: Vec<Fragment> = lc
        .iter()
        .zip(&lives)
        .flat_map(|(c, l)| pieces_of(&b, c, l.len() as u64))
        .collect();
    live_pieces.extend(pieces_of(&b, &first, first.len() as u64 * CLUSTER));
    live_pieces.extend(pieces_of(&b, &copy, copy.len() as u64 * CLUSTER));
    for k in 0..3 {
        b.delete(&format!("D{k}.JPG"), DeleteStyle::Camera);
    }
    let mut volume = b.build();
    // C's runs, in clusters no entry holds.
    let c_pieces = pieces_of(&b, &cc, c.len() as u64);
    let mut from = 0usize;
    for p in &c_pieces {
        let at = (p.start - VOLUME_AT) as usize;
        volume[at..at + p.len as usize].copy_from_slice(&c[from..from + p.len as usize]);
        from += p.len as usize;
    }
    let src = SimSource::new(on_disk(&volume));
    let events = events_of(&src, &ScanOptions::default());
    let view = final_view(&events);
    let deleted = view
        .values()
        .filter(|f| {
            f.origin
                .as_ref()
                .is_some_and(|o| o.state == FsState::Deleted)
        })
        .count();
    assert_eq!(deleted, 3, "{view:#?}");
    // C is put back together, around the live copy.
    let cf = at(&view, c_pieces[0].start);
    assert!(updated(&events).iter().any(|u| u.id == cf.id), "{cf:#?}");
    assert_eq!(cf.fragments, c_pieces);
    for f in view.values() {
        if f.origin.as_ref().is_some_and(|o| o.state == FsState::Live) {
            continue;
        }
        for p in f.ranges() {
            for l in &live_pieces {
                assert!(!overlap(&p, l), "{f:#?} over the live piece {l:?}");
            }
        }
    }
    let intact: Vec<&Finding> = view
        .values()
        .filter(|f| f.health == Health::Intact)
        .collect();
    for (i, a) in intact.iter().enumerate() {
        for b in &intact[i + 1..] {
            // A RAW and its own nested preview are the same file (ruling Q16); none on this card.
            for p in a.ranges() {
                for q in b.ranges() {
                    assert!(!overlap(&p, &q), "{a:#?} and {b:#?} share bytes");
                }
            }
        }
    }
}

#[test]
fn reassembly_never_rereads_known_bad_sectors() {
    let t = three_piece();
    let r2 = t.r_pieces[1];
    let bad = (r2.start + r2.len / 2) / 512 * 512;
    let src = SimSource::new(t.bytes).bad(bad, 512);
    for (threads, cache) in [(1, 1 << 20), (8, 256 << 20)] {
        src.clear_reads();
        let events = events_of(&src, &opts(threads, cache));
        let view = final_view(&events);
        assert_eq!(at(&view, t.r_pieces[0].start).health, Health::Partial);
        let reads = src.reads();
        let touches = |&(at, len): &(u64, usize)| at < bad + 512 && bad < at + len as u64;
        let first = reads
            .iter()
            .position(|&(at, len)| at == bad && len == 512)
            .expect("the scan resolves the bad sector on its own");
        let again: Vec<_> = reads[first + 1..].iter().filter(|r| touches(r)).collect();
        assert!(
            again.is_empty(),
            "threads {threads}: read again after its resolution: {again:?}"
        );
    }
}

/// The hostile image: 256 MiB, no volume; the first 64 KiB of a `jpeg_smooth(2048, 1536, 3)` at 0, then 4096 sectors
/// each holding the photo's next 510 bytes and `FF D9`; zeros after. Returns the image and where the photo starts.
fn hostile() -> Vec<u8> {
    let photo = jpeg_smooth(2048, 1536, 3);
    let first = 64 * KIB as usize;
    assert!(photo.len() > first + 512);
    let mut img = vec![0u8; 256 * MIB as usize];
    img[..first].copy_from_slice(&photo[..first]);
    let mut sector = [0u8; 512];
    sector[..510].copy_from_slice(&photo[first..first + 510]);
    sector[510..].copy_from_slice(&[0xFF, 0xD9]);
    for k in 0..4096 {
        let at = first + k * 512;
        img[at..at + 512].copy_from_slice(&sector);
    }
    img
}

#[test]
fn hostile_card_stays_within_budget() {
    let src = SimSource::new(hostile());
    let events = events_of(&src, &ScanOptions::default());
    let view = final_view(&events);
    let photo = at(&view, 0);
    assert_eq!(photo.kind, FileKind::Jpeg);
    assert_ne!(photo.health, Health::Intact, "{photo:#?}");
}

/// Trips `cancel` at the first read after one that reached the source's end: the first read of the step after the
/// scan (no volume and no video: the reassembly). Counts the reads after it.
struct CancelAfterEnd<'a> {
    inner: &'a SimSource,
    cancel: CancelToken,
    /// (the end was reached, reads since the trip)
    seen: Mutex<(bool, u64)>,
}

impl Source for CancelAfterEnd<'_> {
    fn len(&self) -> u64 {
        self.inner.len()
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, EngineError> {
        {
            let mut s = self.seen.lock().unwrap();
            if self.cancel.is_cancelled() {
                s.1 += 1;
            } else if s.0 {
                self.cancel.cancel();
                s.1 += 1;
            }
            if offset + buf.len() as u64 >= self.inner.len() {
                s.0 = true;
            }
        }
        self.inner.read_at(offset, buf)
    }
    fn volume(&self) -> Option<VolumeId> {
        None
    }
}

#[test]
fn cancel_during_reassembly_is_prompt() {
    let inner = SimSource::new(hostile());
    let cancel = CancelToken::default();
    let src = CancelAfterEnd {
        inner: &inner,
        cancel: cancel.clone(),
        seen: Mutex::new((false, 0)),
    };
    let mut events = Vec::new();
    let r = scan(&src, &all_formats(), &opts(1, 1 << 20), &cancel, &mut |e| {
        events.push(e)
    });
    assert_eq!(r.map(|_| ()).map_err(|e| e.code()), Err("cancelled"));
    let (ended, after) = *src.seen.lock().unwrap();
    assert!(
        ended && cancel.is_cancelled(),
        "the reassembly read the source"
    );
    assert!(after <= 2, "{after} reads after the cancel");
    assert!(
        events.iter().all(|e| !matches!(e, Event::Updated(_))),
        "nothing updated once cancelled"
    );
}

#[test]
fn live_files_are_never_candidates() {
    // A live photo in 3 runs (FAT-chained), live photos between them, an unreadable sector in its second run: found
    // `Partial` with its chain, and nothing updated (its chain says where it is; ruling D2).
    let (card, pieces) = live_in_three_runs();
    let events = events_of(&card.source, &ScanOptions::default());
    let found = events
        .iter()
        .find_map(|e| match e {
            Event::Found(f) if f.start == pieces[0].start => Some(f),
            _ => None,
        })
        .expect("the live photo");
    assert_eq!(found.health, Health::Partial);
    assert_eq!(found.fragments, pieces);
    assert!(updated(&events).is_empty(), "{:#?}", updated(&events));
}
