//! Byte-level checks of the synthetic photos and videos (the testkit never depends on the engine it tests).

use revenant_testkit::{AvcOptions, VideoCodec, VideoLayout, avc_video, camera_video, jpeg_burst};

/// End of the first SOS segment (its header), from the SOI.
fn sos_end(j: &[u8]) -> usize {
    let mut at = 2;
    loop {
        assert_eq!(j[at], 0xFF, "a marker at {at}");
        let len = u16::from_be_bytes([j[at + 2], j[at + 3]]) as usize;
        if j[at + 1] == 0xDA {
            return at + 2 + len;
        }
        at += 2 + len;
    }
}

#[test]
fn burst_photos_share_a_frame_and_differ() {
    let burst = jpeg_burst(512, 384, 7, 3);
    assert_eq!(burst.len(), 3);
    for j in &burst {
        let img = image::load_from_memory_with_format(j, image::ImageFormat::Jpeg).unwrap();
        assert_eq!((img.width(), img.height()), (512, 384));
    }
    let head = &burst[0][..sos_end(&burst[0])];
    for j in &burst[1..] {
        assert_eq!(&j[..sos_end(j)], head, "same tables, frame and scan header");
    }
    for i in 0..3 {
        for k in i + 1..3 {
            assert!(burst[i] != burst[k], "photos {i} and {k} differ");
        }
    }
    assert_eq!(burst[0], revenant_testkit::jpeg_smooth(512, 384, 7));
}

fn opts(seed: u64, small_nals: bool) -> AvcOptions {
    AvcOptions {
        samples: 10,
        seed,
        mov: false,
        mdat_pad: 300,
        small_nals,
    }
}

#[test]
fn avc_video_is_camera_video_moov_at_end() {
    for seed in 1..4 {
        for small in [false, true] {
            let o = opts(seed, small);
            let a = avc_video(&o);
            let c = camera_video(&o, VideoCodec::Avc, VideoLayout::MoovAtEnd);
            assert!(a.bytes == c.bytes, "seed {seed}");
            assert_eq!((a.moov, a.mdat_payload), (c.moov, c.mdat_payload));
        }
    }
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes(b[..4].try_into().unwrap())
}

/// `(kind, start, end)` of the boxes in `b[from..to]`.
fn boxes(b: &[u8], from: usize, to: usize) -> Vec<([u8; 4], usize, usize)> {
    let mut out = Vec::new();
    let mut at = from;
    while at < to {
        let size = match be32(&b[at..]) {
            0 => to - at,
            s => s as usize,
        };
        out.push((b[at + 4..at + 8].try_into().unwrap(), at, at + size));
        at += size;
    }
    assert_eq!(at, to);
    out
}

fn child<'a>(b: &'a [u8], path: &[&[u8; 4]]) -> &'a [u8] {
    let mut cur = b;
    for kind in path {
        let (_, s, e) = boxes(cur, 0, cur.len())
            .into_iter()
            .find(|(k, _, _)| k == *kind)
            .unwrap_or_else(|| panic!("no {}", String::from_utf8_lossy(*kind)));
        cur = &cur[s + 8..e];
    }
    cur
}

/// The NAL units of one sample: `(type, temporal id plus 1)` for HEVC, `(type, 1)` for AVC. Panics unless the
/// length-prefixed units cover the sample exactly.
fn nals(sample: &[u8], hevc: bool) -> Vec<(u8, u8)> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < sample.len() {
        let len = be32(&sample[at..]) as usize;
        let h = &sample[at + 4..];
        assert_eq!(h[0] & 0x80, 0, "forbidden zero bit");
        out.push(if hevc {
            assert!(len >= 2);
            assert_eq!(h[1] >> 3, 0, "nuh_layer_id 0");
            ((h[0] >> 1) & 0x3F, h[1] & 7)
        } else {
            (h[0] & 0x1F, 1)
        });
        at += 4 + len;
    }
    assert_eq!(at, sample.len(), "the NAL units cover the sample");
    out
}

/// Samples in file order as `(offset, size)`, read from the `moov` tables or, when fragmented, from every `trun`.
fn samples(v: &[u8]) -> Vec<(usize, usize)> {
    let top = boxes(v, 0, v.len());
    let moov = top.iter().find(|b| &b.0 == b"moov").unwrap();
    let moov = &v[moov.1 + 8..moov.2];
    let stbl = child(moov, &[b"trak", b"mdia", b"minf", b"stbl"]);
    let stsz = child(stbl, &[b"stsz"]);
    let count = be32(&stsz[8..]) as usize;
    if top.iter().any(|b| &b.0 == b"moof") {
        assert_eq!(count, 0, "empty sample tables");
        let trex = child(moov, &[b"mvex", b"trex"]);
        assert_eq!(be32(&trex[4..]), 1, "track 1");
        let mut out = Vec::new();
        for (i, &(kind, s, e)) in top.iter().enumerate() {
            if &kind != b"moof" {
                continue;
            }
            assert_eq!(&top[i + 1].0, b"mdat", "an mdat after each moof");
            let moof = &v[s + 8..e];
            let seq = be32(&child(moof, &[b"mfhd"])[4..]);
            assert_eq!(seq as usize, out.len() / 8 + 1);
            let tfhd = child(moof, &[b"traf", b"tfhd"]);
            assert_eq!(be32(tfhd) & 0x02_0000, 0x02_0000, "default-base-is-moof");
            assert_eq!(be32(&tfhd[4..]), 1);
            let trun = child(moof, &[b"traf", b"trun"]);
            assert_eq!(
                be32(trun) & 0xFF_FFFF,
                0x201,
                "data offset and sample sizes"
            );
            let n = be32(&trun[4..]) as usize;
            let mut at = s + be32(&trun[8..]) as usize;
            assert_eq!(at, top[i + 1].1 + 8, "data offset: the mdat payload");
            for k in 0..n {
                let size = be32(&trun[12 + 4 * k..]) as usize;
                out.push((at, size));
                at += size;
            }
            assert!(at <= top[i + 1].2);
        }
        return out;
    }
    let sizes: Vec<usize> = (0..count)
        .map(|i| be32(&stsz[12 + 4 * i..]) as usize)
        .collect();
    let stco = child(stbl, &[b"stco"]);
    let chunks: Vec<usize> = (0..be32(&stco[4..]) as usize)
        .map(|i| be32(&stco[8 + 4 * i..]) as usize)
        .collect();
    let mut out = Vec::new();
    for (c, &at) in chunks.iter().enumerate() {
        let mut at = at;
        for &size in sizes.iter().skip(4 * c).take(4) {
            out.push((at, size));
            at += size;
        }
    }
    assert_eq!(out.len(), count);
    out
}

#[test]
fn camera_video_layouts_hold_valid_samples() {
    let layouts = [
        VideoLayout::MoovAtEnd,
        VideoLayout::MoovFirst,
        VideoLayout::MoovFirstMdatSizeZero,
        VideoLayout::Fragmented {
            samples_per_fragment: 8,
        },
    ];
    for codec in [VideoCodec::Avc, VideoCodec::Hevc] {
        let hevc = codec == VideoCodec::Hevc;
        for layout in layouts {
            for small in [false, true] {
                let o = AvcOptions {
                    samples: 21,
                    ..opts(5, small)
                };
                let v = camera_video(&o, codec, layout);
                let b = &v.bytes;
                let top = boxes(b, 0, b.len());
                let kinds: Vec<&[u8; 4]> = top.iter().map(|t| &t.0).collect();
                match layout {
                    VideoLayout::MoovAtEnd => assert_eq!(kinds, [b"ftyp", b"mdat", b"moov"]),
                    VideoLayout::MoovFirst | VideoLayout::MoovFirstMdatSizeZero => {
                        assert_eq!(kinds, [b"ftyp", b"moov", b"mdat"])
                    }
                    VideoLayout::Fragmented { .. } => {
                        assert_eq!(kinds[..2], [b"ftyp", b"moov"]);
                        // 21 samples, 8 per fragment: 3 (moof, mdat) pairs.
                        assert_eq!(kinds.len(), 2 + 2 * 3);
                    }
                }
                let mdat = top.iter().find(|t| &t.0 == b"mdat").unwrap();
                assert_eq!(v.mdat_payload, mdat.1 + 8..mdat.2);
                let moov = top.iter().find(|t| &t.0 == b"moov").unwrap();
                assert_eq!(v.moov, moov.1..moov.2);
                let size_field = be32(&b[mdat.1..]);
                assert_eq!(
                    size_field == 0,
                    layout == VideoLayout::MoovFirstMdatSizeZero
                );
                // The sample entry and its configuration: lengthSizeMinusOne = 3.
                let stsd = child(
                    &b[moov.1 + 8..moov.2],
                    &[b"trak", b"mdia", b"minf", b"stbl", b"stsd"],
                );
                let entry = &stsd[8..];
                let kind: &[u8] = &entry[4..8];
                let entry = &entry[8..be32(entry) as usize];
                if hevc {
                    assert_eq!(kind, b"hvc1");
                    let hvcc = child(&entry[78..], &[b"hvcC"]);
                    assert_eq!(hvcc.len(), 23);
                    assert_eq!(hvcc[0], 1, "configurationVersion");
                    assert_eq!(hvcc[21] & 3, 3);
                    assert_eq!(hvcc[22], 0, "no arrays");
                } else {
                    assert_eq!(kind, b"avc1");
                    let avcc = child(&entry[78..], &[b"avcC"]);
                    assert_eq!(avcc[4] & 3, 3);
                }
                let s = samples(b);
                assert_eq!(s.len(), 21);
                let key_every = match layout {
                    VideoLayout::Fragmented { .. } => 8,
                    _ => 4,
                };
                let mut end = 0;
                for (i, &(at, size)) in s.iter().enumerate() {
                    assert!(at >= end, "samples in order, not overlapping");
                    end = at + size;
                    let (key, other) = if hevc { (19, 1) } else { (5, 1) };
                    for (t, tid) in nals(&b[at..at + size], hevc) {
                        assert_eq!(t, if i % key_every == 0 { key } else { other });
                        assert_eq!(tid, 1);
                    }
                }
                // Contiguous samples from the first mdat's payload start, then the padding at the end of the last mdat.
                assert_eq!(s[0].0, v.mdat_payload.start);
                let last_mdat = top.iter().rev().find(|t| &t.0 == b"mdat").unwrap();
                assert_eq!(last_mdat.2 - end, 300, "mdat_pad after the last sample");
            }
        }
    }
}
