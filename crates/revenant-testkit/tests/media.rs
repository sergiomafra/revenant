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

/// Mean absolute difference of two equal-length sample buffers.
fn mean_abs_diff(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len());
    let sum: u64 = a
        .iter()
        .zip(b)
        .map(|(&x, &y)| u64::from(x.abs_diff(y)))
        .sum();
    sum as f64 / a.len() as f64
}

#[test]
fn burst_photos_share_a_frame_and_differ() {
    let burst = jpeg_burst(512, 384, 7, 3);
    assert_eq!(burst.len(), 3);
    let decoded: Vec<Vec<u8>> = burst
        .iter()
        .map(|j| {
            let img = image::load_from_memory_with_format(j, image::ImageFormat::Jpeg).unwrap();
            assert_eq!((img.width(), img.height()), (512, 384));
            img.to_rgb8().into_raw()
        })
        .collect();
    // True look-alikes: neighbouring photos differ by about a level per sample (0.78 for seed 7), far below another
    // scene (about 53 for jpeg_smooth seed 8), and never by nothing.
    for w in decoded.windows(2) {
        let mad = mean_abs_diff(&w[0], &w[1]);
        assert!(mad > 0.1 && mad < 4.0, "mean absolute difference {mad}");
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

/// Counts the difference codes of the lossless JPEG `j` (one DHT, SOF3, no restart unless `restarts`) with a plain
/// bit-by-bit canonical decoder, checking the segments on the way. Returns `(samples, largest SSSS)`.
fn count_lossless_samples(j: &[u8], w: usize, h: usize, n: usize, p: u8) -> (usize, u32) {
    assert_eq!(&j[..2], [0xFF, 0xD8]);
    assert_eq!(&j[j.len() - 2..], [0xFF, 0xD9]);
    let (mut at, mut counts, mut vals, mut seen_sof) = (2, [0u8; 16], Vec::new(), false);
    let scan = loop {
        assert_eq!(j[at], 0xFF);
        let len = u16::from_be_bytes([j[at + 2], j[at + 3]]) as usize;
        let body = &j[at + 4..at + 2 + len];
        match j[at + 1] {
            0xC4 => {
                assert_eq!(body[0], 0x00);
                counts.copy_from_slice(&body[1..17]);
                vals = body[17..].to_vec();
            }
            0xC3 => {
                seen_sof = true;
                assert_eq!(body[0], p);
                assert_eq!(u16::from_be_bytes([body[1], body[2]]) as usize, h);
                assert_eq!(u16::from_be_bytes([body[3], body[4]]) as usize, w);
                assert_eq!(body[5] as usize, n);
                assert_eq!(body.len(), 6 + 3 * n);
            }
            0xDA => {
                assert!(seen_sof);
                assert_eq!(body[0] as usize, n);
                assert_eq!(&body[1 + 2 * n..], [1, 0, 0]);
                break at + 2 + len;
            }
            m => panic!("unexpected marker {m:#x}"),
        }
        at += 2 + len;
    };
    assert_eq!(vals.len(), 17);
    // Entropy-coded bytes with the stuffed zeros removed.
    let data = &j[scan..j.len() - 2];
    let mut bits = Vec::new();
    let mut i = 0;
    while i < data.len() {
        let b = data[i];
        if b == 0xFF {
            assert_eq!(data[i + 1], 0, "only stuffed FF in the scan");
            i += 1;
        }
        bits.extend((0..8).rev().map(|k| (b >> k) & 1));
        i += 1;
    }
    let (mut pos, mut samples, mut big) = (0, 0, 0);
    while samples < w * h * n {
        let (mut code, mut first, mut index) = (0u32, 0u32, 0usize);
        let mut sym = None;
        for &count in &counts {
            code = (code << 1) | u32::from(bits[pos]);
            pos += 1;
            let c = u32::from(count);
            if code < first + c {
                sym = Some(vals[index + (code - first) as usize]);
                break;
            }
            index += c as usize;
            first = (first + c) << 1;
        }
        let ssss = u32::from(sym.expect("a valid code"));
        assert!(ssss <= 16);
        if ssss < 16 {
            pos += ssss as usize;
        }
        big = big.max(ssss);
        samples += 1;
    }
    assert!(bits.len() - pos < 8, "only padding is left");
    assert!(bits[pos..].iter().all(|&b| b == 1), "padding is 1-bits");
    (samples, big)
}

#[test]
fn lossless_jpeg_has_a_valid_frame_and_exact_codes() {
    for (w, h, n, p) in [
        (64, 48, 1u8, 14u8),
        (33, 17, 2, 12),
        (20, 20, 4, 16),
        (9, 9, 3, 8),
    ] {
        let a = revenant_testkit::lossless_jpeg(w as u32, h as u32, n, p, 5);
        let (count, _) = count_lossless_samples(&a, w, h, n as usize, p);
        assert_eq!(count, w * h * n as usize);
        // Deterministic per seed, different across seeds.
        assert_eq!(
            a,
            revenant_testkit::lossless_jpeg(w as u32, h as u32, n, p, 5)
        );
        assert_ne!(
            a,
            revenant_testkit::lossless_jpeg(w as u32, h as u32, n, p, 6)
        );
    }
    // A wide-precision image uses the large categories (rare outliers).
    let j = revenant_testkit::lossless_jpeg(256, 256, 1, 16, 3);
    assert!(count_lossless_samples(&j, 256, 256, 1, 16).1 >= 12);
}

/// Little-endian u32 at `at`.
fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

/// The entries of the little-endian IFD at `at`: (tag, type, count, value field) and the next-IFD pointer.
fn ifd_entries(b: &[u8], at: usize) -> (Vec<(u16, u16, u32, u32)>, u32) {
    let n = u16::from_le_bytes([b[at], b[at + 1]]) as usize;
    let e = (0..n)
        .map(|i| {
            let p = at + 2 + 12 * i;
            (
                u16::from_le_bytes([b[p], b[p + 1]]),
                u16::from_le_bytes([b[p + 2], b[p + 3]]),
                le32(b, p + 4),
                le32(b, p + 8),
            )
        })
        .collect();
    (e, le32(b, at + 2 + 12 * n))
}

fn tag(e: &[(u16, u16, u32, u32)], t: u16) -> Option<(u16, u32, u32)> {
    e.iter()
        .find(|x| x.0 == t)
        .map(|&(_, typ, count, v)| (typ, count, v))
}

#[test]
fn arw_lays_out_like_a_sony_raw() {
    use revenant_testkit::{ArwCompression, ArwOptions, arw};
    for compression in [
        ArwCompression::Uncompressed,
        ArwCompression::Lossless,
        ArwCompression::Lossy,
    ] {
        let a = arw(&ArwOptions {
            compression,
            thumbnail: true,
            ..ArwOptions::default()
        });
        let b = &a.bytes;
        assert_eq!(&b[..4], b"II*\0");
        let (e0, next) = ifd_entries(b, le32(b, 4) as usize);
        let (_, n, make_at) = tag(&e0, 0x010F).unwrap();
        assert_eq!(
            &b[make_at as usize..make_at as usize + n as usize],
            b"SONY\0"
        );
        // The preview, pointed at by IFD0, before the raw data; the thumbnail by IFD1, right before the preview.
        let p = a.preview.clone().unwrap();
        assert_eq!(tag(&e0, 0x0201).unwrap().2 as usize, p.start);
        assert_eq!(tag(&e0, 0x0202).unwrap().2 as usize, p.len());
        assert_eq!(&b[p.start..p.start + 2], [0xFF, 0xD8]);
        assert_eq!(&b[p.end - 2..p.end], [0xFF, 0xD9]);
        let t = a.thumbnail.clone().unwrap();
        let (e1, _) = ifd_entries(b, next as usize);
        assert_eq!(tag(&e1, 0x0201).unwrap().2 as usize, t.start);
        assert_eq!(tag(&e1, 0x0202).unwrap().2 as usize, t.len());
        assert_eq!((t.end, t.len() % 512, t.start % 512), (p.start, 0, 0));
        assert_eq!(&b[t.end - 2..t.end], [0xFF, 0xD9]);
        assert!(p.end <= a.raw.start && a.raw.end == b.len());
        // The SubIFD holds the raw data.
        let (_, _, sub) = tag(&e0, 0x014A).unwrap();
        let (es, _) = ifd_entries(b, sub as usize);
        let code = tag(&es, 0x0103).unwrap().2 & 0xFFFF;
        match compression {
            ArwCompression::Uncompressed | ArwCompression::Lossy => {
                assert_eq!(
                    code,
                    if compression == ArwCompression::Lossy {
                        32767
                    } else {
                        1
                    }
                );
                assert_eq!(tag(&es, 0x0111).unwrap().2 as usize, a.raw.start);
                assert_eq!(tag(&es, 0x0117).unwrap().2 as usize, a.raw.len());
                assert!(a.tiles.is_empty());
            }
            ArwCompression::Lossless => {
                assert_eq!(code, 7);
                assert_eq!(a.tiles.len(), 4 * 3, "1024×768 in tiles of 256");
                let (_, n, offs) = tag(&es, 0x0144).unwrap();
                let (_, _, cnts) = tag(&es, 0x0145).unwrap();
                assert_eq!(n as usize, a.tiles.len());
                for (k, r) in a.tiles.iter().enumerate() {
                    assert_eq!(le32(b, offs as usize + 4 * k) as usize, r.start);
                    assert_eq!(le32(b, cnts as usize + 4 * k) as usize, r.len());
                    assert_eq!(&b[r.start..r.start + 2], [0xFF, 0xD8]);
                    assert_eq!(&b[r.end - 2..r.end], [0xFF, 0xD9]);
                    let (count, _) = count_lossless_samples(&b[r.clone()], 256, 256, 1, 14);
                    assert_eq!(count, 256 * 256);
                }
                assert_eq!(a.tiles[0].start, a.raw.start);
                assert!(a.tiles.windows(2).all(|w| w[0].end == w[1].start));
            }
        }
    }
}

#[test]
fn arw_tiles_can_end_on_a_grid() {
    use revenant_testkit::{ArwCompression, ArwOptions, arw};
    let grid = 32 << 10;
    let a = arw(&ArwOptions {
        compression: ArwCompression::Lossless,
        align: grid,
        aligned_tiles: vec![2, 5, 8],
        ..ArwOptions::default()
    });
    assert_eq!(a.tiles[0].start % grid as usize, 0);
    for (k, t) in a.tiles.iter().enumerate() {
        assert_eq!(
            t.end % grid as usize == 0,
            [2, 5, 8].contains(&k),
            "tile {k}: {t:?}"
        );
        // Padded with fill bytes before the EOI: still a whole frame of exact samples.
        let tile = &a.bytes[t.clone()];
        let mut data = tile[..tile.len() - 2].to_vec();
        while data.last() == Some(&0xFF) {
            data.pop();
        }
        data.extend_from_slice(&[0xFF, 0xD9]);
        assert_eq!(data.len() < tile.len(), [2, 5, 8].contains(&k), "tile {k}");
        let (count, _) = count_lossless_samples(&data, 256, 256, 1, 14);
        assert_eq!(count, 256 * 256);
    }
    // Two seeds of the same options: the same frame, different bytes.
    let b = arw(&ArwOptions {
        compression: ArwCompression::Lossless,
        seed: 2,
        ..ArwOptions::default()
    });
    let a = arw(&ArwOptions {
        compression: ArwCompression::Lossless,
        ..ArwOptions::default()
    });
    assert_ne!(a.bytes, b.bytes);
    assert_eq!(a.tiles.len(), b.tiles.len());
}
