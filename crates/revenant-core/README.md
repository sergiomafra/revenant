# revenant-core

The Revenant recovery engine (library). It never writes to the source being read, and contains no UI text.

## Current state

Every engine error is an `EngineError`; each variant has a stable `snake_case` code, returned by
`EngineError::code()`:

| Variant               | Code                    |
| --------------------- | ----------------------- |
| `Open`                | `open_failed`           |
| `Read`                | `read_failed`           |
| `DestinationOnSource` | `destination_on_source` |
| `Write`               | `write_failed`          |
| `Cancelled`           | `cancelled`             |

```rust
use revenant_core::EngineError;
assert_eq!(EngineError::Cancelled.code(), "cancelled");
```

## Source

`source::Source` is the engine's read-only disk-access layer: `len()`, `read_at(offset, buf)` (positional, no shared cursor; fills
the buffer except at the end of the source, returns 0 past the end) and `volume()`. There is no write method.

`source::FileSource` implements it for regular files (disk image files), opened read-only (`pread` on Unix, `seek_read` on
Windows); reading devices directly comes in a later part. `source::volume_of(path)` returns the `VolumeId` of any existing
path (device id on Unix, volume serial number on Windows), so extraction onto the source's own volume can be refused. A test
scans `src/source/` to ensure no write-capable open flags are ever introduced, and another checks that the image's bytes and
modification time are unchanged after a full scan that extracts every finding.

`source::Fragmented::new(source, pieces)` is a read-only logical view of pieces (`Fragment { start, len }`, absolute ranges)
of a source, in the given order: offset 0 is the first byte of the first piece. Each piece is first clamped to the inner
source's end (as `extract` clamps it), and `len()` is the sum of the clamped lengths. A read crosses pieces in order; a read
error is the inner source's, so its offset is absolute in the inner source; `volume()` is the inner source's.
`absolute(start, len)` maps a logical range back to absolute pieces of the source, merging pieces that touch.

## Formats: JPEG

`formats::Format` is the detector interface: `recognizes(head)` checks a signature at a sector start,
`confirms(source, start)` validates the header there (cheap, never recursive, never measures the whole file), and
`measure(source, start)` returns a `Measurement { len, kind, health, fragments }` found by walking the file's structure, or `None` when
the bytes are not a file of that format. `None` never means "could not read": a read error does not make a file vanish.
`formats::all_formats()` lists the supported detectors. `FileKind::as_str()` gives a stable lowercase id (`"jpeg"`, `"mp4"`,
`"mov"`).

"Another file starts here" (used to cut a JPEG's scan data or an `mdat` at a sector start) means a format both `recognizes`
the head and `confirms` it, so a random byte pattern that merely looks like a signature can no longer cut a file. JPEG
confirms with the header walk up to SOS with DQT and SOF present (no scan-data walk); ISO BMFF confirms exactly what `measure`
accepts, by a header-only walk from the `ftyp` (at most 64 top-level box headers, no payload reads, no recursion): it meets
an `mdat`, or the `ftyp` has a HEIC brand and a `meta` is met, or a box header falls in an unreadable sector (it does not
refute the file, as for JPEG). A lone `ftyp` + `free` is not a video and no longer cuts another file. JPEG's `confirms` walks at most 1 MiB of header. `scan` still uses `recognizes` plus `measure` for its candidates.

Detectors read the source through one shared reader, in 1 MiB windows aligned to 512-byte sectors (never a whole file at
once). When a window cannot be read it is re-read sector by sector: unreadable sectors read as zeros and are remembered, and
only the end of the source ends a walk for lack of data. A file whose measured range contains an unreadable sector is never
`Intact` (details per format below).

Each reader takes its window from a small per-thread pool (at most 4 kept, so at most 4 MiB per measuring thread, outside
the scan cache; see the memory bound under Scan) and gives it back when dropped, so measuring allocates nothing after
warm-up; the detectors share one list of formats built once; the JPEG entropy walk jumps from `FF` to
`FF` (still checking every sector start it crosses for another file); a reader loads a `moov`/`meta` payload at most once per
measurement, however many checks need it.

`formats::Jpeg` follows the marker segments by their declared lengths (so the thumbnail embedded in an APP1/EXIF segment does
not end the file; DAC and JPG markers count as segments, and `FF` fill bytes before a marker are skipped), requires DQT and SOF before SOS, and scans the entropy-coded data up to EOI (`FF D9`), up to
`JPEG_MAX_LEN` (64 MiB). If the header walk breaks on an unreadable sector, the search for EOI continues from there. Health:

- `Intact`: EOI found, no unreadable sector inside, and the content verified (ruling Q6). For a frame Revenant's entropy
  decoder covers (below), the evidence is that decoder walking every MCU, the exact count with restart markers in
  sequence, with EOI right after the last one; `zune-jpeg`'s strict mode is not used for these, because it zero-fills past
  an early EOI and ignores data left before EOI, so half a photo followed by zeros, by another photo's tail or by its own
  tail with a sector missing, then an EOI, would pass. Such a file is `Partial` (and may be put back together, see
  "Fragmented JPEG"). For any other frame (progressive, arithmetic, 12-bit, a header the decoder cannot parse), the
  evidence is a strict `zune-jpeg` decode. This one rule (`decode_health`) judges every JPEG: a plain JPEG, a RAW's, RAF's
  or CR3's preview, the own-JPEG check that lets a file head inside a RAW's preview count as the RAW's own data, and a
  reassembled photo. Below, "decodes strictly" means passing it; "decodes leniently" means a result of `Partial` by it.
- `Partial`: no EOI before the end of the source, `JPEG_MAX_LEN`, or a 512-byte sector that starts another recognized file
  (the length stops there), and something of the photo would show (below). Or EOI found, with a frame Revenant's entropy
  decoder covers, and that decoder completes at least one row of MCUs before it fails (ruling Q10, as for a cut file:
  that much of the photo shows; `zune-jpeg` is not run for these frames at all), or walks to EOI but the file contains
  unreadable sectors. Or EOI found, with any other frame, and only the lenient `zune-jpeg` decode succeeds, or the file
  contains unreadable sectors and a decode (strict or lenient) of the zero-filled bytes succeeds, or the declared size is
  over 210 megapixels so it is not decoded ("not fully verifiable"), or a DAC segment (arithmetic coding) makes it one
  `zune-jpeg` cannot decode. The length never exceeds `JPEG_MAX_LEN`.
- `Damaged`: EOI found, with a frame the entropy decoder covers, and that decoder fails before completing one row of MCUs
  (ruling Q9: nothing of the photo would show); or EOI found, with any other frame, and neither `zune-jpeg` decode
  succeeds. The length is still exact. Also `Damaged`: a cut file (no EOI, as above) whose entropy-coded data does not
  hold one full row of MCUs before its first error, the cut, or the first 512-byte sector of one repeated byte value or
  unreadable sector, such as a JPEG cut right after its header and followed by erased (zero-filled) space: nothing of the
  photo would show. Zeros decode as valid blocks under the standard Huffman tables, so they are not counted as the
  photo's data. The length is still where the file was cut.

A cut file is judged by Revenant's own entropy decoder (`formats/jpeg_entropy.rs`, written from ITU-T T.81): it covers
sequential Huffman JPEGs (SOF0/SOF1, 8-bit, 1–4 components all in one scan, sampling factors ≤ 2, at most 210 megapixels,
every referenced Huffman and quantization table defined; DRI restart markers checked in sequence; a run/0 symbol other than EOB and ZRL, a DC
category over 11 or an AC size over 10 is an error) and decodes no pixels, only the Huffman-coded
blocks, counting complete MCUs exactly; it stops at the first row. Any other cut JPEG (progressive, arithmetic, 12-bit, a
header it cannot parse) stays `Partial` as before. On hostile data every loop is bounded by the frame's MCU count, 64
coefficients per block, 16 bits per code and the bytes up to the cut; a DHT whose code lengths cannot form a prefix code is
rejected.

The validation decode outputs grayscale (1 byte per pixel) to bound memory.

### Fragmented JPEG

When a JPEG is not `Intact` and its frame is one the entropy decoder covers, but its contiguous data does not decode to
EOI with every MCU (another file was written in the middle of it, or the rest of it lies further on), `Jpeg::measure`
searches for where the photo's data continues and puts the photo back together from two pieces (`formats/jpeg_join.rs`,
crate-private). All positions are absolute 512-byte sector boundaries:

1. The contiguous data is decoded (keeping each block's luma DC value). The break `x` is the first sign that the data
   stopped being this photo: the byte after the decode error (an error is reported at a marker's `FF`, and the byte after
   it may be the first one that is not this photo's), the cut, or the first sector of one repeated byte value (erased
   space, which would otherwise decode as valid blocks). The first piece may end at any of the 4 sector boundaries at or before
   `x` (`c1`) that leave it at least 16 KiB long (ruling Q8: a few sectors of a photo hold too little to recover, and every
   header could otherwise start a search), each resumed from the decoder's last state at or before it.
2. One forward pass over the window of `JPEG_GAP_MAX` (64 MiB) after the earliest `c1` records, for each sector start `g`,
   the first event at or after it: an `FF D9` (`EOI` at `e`), or a sign the bytes are not entropy-coded data (`FF`
   followed by anything but `00`, `D0`–`D7`, `D9`, `FF`; a sector of one repeated byte value or an unreadable one; a sector
   where another file starts). The candidates are, for each distinct `EOI`, the first 4 sector starts whose first event
   it is; the second piece `[g, e + 2)` therefore ends inside the window.
3. Every pair `(c1, g)` with `g > c1` and a total length within `JPEG_MAX_LEN` is tried, closest `g` first, then latest
   `c1`: the decoder resumes over the two pieces and passes when every MCU decodes with EOI right at the end of the second
   piece and the DC seam test holds. The DC seam test compares the mean `|ΔDC|` between the luma blocks on both sides of the
   seam with the median of the other block-row boundaries: it passes iff the image has at least 4 block rows and the seam
   is at most 3 × that median + 4 (it is what rejects another photo's tail spliced at a restart marker, where the MCU count
   alone would pass). A try stops after the MCU row below the seam if the seam already fails against the rows above it,
   and at an MCU row where its decoder state (position in the second piece, MCU, buffered bits, DC predictors) is one a
   failed try with the same EOI passed through: from there it reads the same bytes and fails the same way (up to 16,384
   states are remembered).
4. The first passing pair is the result: the measurement is the two pieces, with the JPEG health rule above applied to
   them (`Intact`, or `Partial` with an unreadable sector in either piece; a pair passes only when the walk reaches EOI,
   so it is never `Damaged`).

Evidence for `Intact` (rulings Q1, Q7): exactly one candidate in the whole window passed, and every candidate was tried.
A second passing candidate (the search then stops) or hitting a bound makes the reassembled photo at most `Partial`. So
does an EOI with more than 4 sector starts whose first event it is (not all were tried), unless it is the passing
continuation's own EOI (the others lie inside it) or those sectors belong to a file in the gap: the last file head before
them is a JPEG that is `Intact` on its own, contiguously up to that EOI, with a frame (geometry, tables, restart interval)
other than this photo's. A JPEG of the same frame does not count: this photo's rest could lie inside it, as when two
photos from one camera were written interleaved.

Bounds on hostile input (ruling Q8): a first piece of at least 16 KiB; one 8-byte record per sector of the window (at
most 131,072); at most `MAX_JOIN_TRIES` (4096) tries; at most 64 MiB read from the source by the tries and by the checks
of files in the gap, together (each try's reader loads 64 KiB at a time); the first piece's last bytes are read once and
kept in memory (at most 1 MiB). The search looks at the scan's cancel flag every MiB of the window, before every try and
at every MCU row of a try (a measurement reaches the flag through a per-thread scope set by the scan), so a cancelled
scan does not wait for it. Memory per search: the luma DC values of the first piece, of a try and of the joined image
(4 bytes per block, about 13 MiB each at 210 megapixels), the window's records (1 MiB), the remembered states (about
1 MiB) and the first piece's last bytes (1 MiB), held by the measuring thread.

Reading: each sector is read once from the source by the scan and the detectors (this is about reading the card; in
memory the same bytes may be walked several times), except that a JPEG gap search re-reads, through the scan's cache: the photo's contiguous bytes (decoded again, scanned for erased sectors, and decoded once more up to an
erased sector if there is one: at most 3 × 64 MiB), its 64 MiB window once, at most 64 MiB for the tries and the checks
of files in the gap, and the two pieces once more to judge the reassembled photo (at most 64 MiB).

Not covered: progressive, arithmetic-coded or 12-bit JPEGs (any frame the decoder does not cover), a first piece under
16 KiB, three or more pieces, a second piece that lies before the first, a second piece that ends past the window, and
two photos written interleaved (each split in two with the other's pieces in between): they are at most `Partial`, and
a photo of the same frame glued to the other's tail exactly on a restart marker may even come out `Intact` as a
contiguous file (the MCU count and restart markers are all right, and a DC step below the seam threshold is not seen).
More generally, the DC seam test runs only at the joins the search makes, never inside a contiguous file: a cut photo
immediately followed, with no gap, by the tail of a near-identical photo of the same frame (same camera and settings)
whose bytes decode exactly to EOI is a valid JPEG and comes out `Intact`. And a photo whose entropy-coded data holds a
whole 512-byte sector of one repeated byte value is treated as cut there (rulings Q5, C3): camera photos very rarely do
this, but a large flat area coded with optimized Huffman tables could (all `00`); such a photo is `Partial` and is not
put back together.

Synthetic JPEGs and disk images for tests come from the `revenant-testkit` workspace crate (`publish = false`).

## Formats: MP4/MOV

`formats::IsoBmff` recognizes a `ftyp` box (declared size 8..=4096) at a sector start and measures the file by walking the
top-level boxes (`ftyp moov mdat free skip wide uuid meta pdin moof mfra styp sidx udta`) from their declared sizes. Size 1
means a 64-bit `largesize`; size 0 (only for `mdat`, e.g. a recording cut by a dead battery) runs to the first sector where
another file starts, or to the end of the source. The walk stops at an unaccepted type, a
size under 8, or the end of the source; a box that runs past the end counts only up to it. The major brand `qt  ` gives
`FileKind::Mov`, `crx ` gives `Cr3` and, with a HEIC brand and no `moov`, `Heic` (see below); anything else
`Mp4`. Every size is checked against the source length; only `moov` is loaded into memory, and
only up to `MOOV_MAX_LEN` (16 MiB).

Walk boundaries: `ftyp` is accepted only as the first box. Once both `moov` and `mdat` are seen, only trailing
`free skip wide uuid meta udta mfra` boxes continue the file (plus `moof sidx styp mdat` when it is fragmented); anything else
ends it, and a trailing box that overruns the source is left out. The walk also stops at a box that starts on a 512-byte sector
where a recognized file starts. Inside `mdat`, every 512-byte sector start (absolute offset) is checked as well: where a
recognized file of any format starts, the video ends there. An `mdat` whose declared size is cut this way counts as a box
declared larger than what exists (below). Motion-JPEG exception: if the walk was cut inside `mdat`, the box headers are
walked again without the checks inside `mdat` (one read per box, so a cut-off recording does not cost a pass over the rest
of the source), and if (a) that uncut index is `Intact` and (b) a video track (`hdlr` `vide`) has a first `stsd` entry of
`mjpa`, `mjpb` or `jpeg`, the video is walked once more with the checks inside `mdat` skipping that track's chunk offsets
(`stco`/`co64`): the first confirmed file head that is not one of them ends the video. A video whose every head is one of
its frames is whole; a photo written over the video is not at a chunk offset, so it still cuts it. Chunk offsets are relative to the start of the file (its `ftyp`), not to the source.

- No `mdat`: not a video (`None`).
- No `moov`: `Damaged` (the media data is there but not the index; rebuilding it is a later product milestone). After
  the scan such a video may be paired with its `moov` found elsewhere (see "Video with a separated index").
- `moov` larger than 16 MiB: `Damaged`.
- A box declared larger than what exists (cut mid-`mdat` or mid-`moov` by the end of the source, or an `mdat` cut by another
  file written over it): `Partial`.
- Garbled index: if the box walk inside `moov`, `trak`, `mdia`, `minf` or `stbl` ends on an invalid size (or leftover bytes)
  instead of consuming its parent exactly (fewer than 8 trailing zero bytes, a QuickTime terminator, are tolerated), or a non-fragmented file has no `trak` with both `stco`/`co64` and `stsz`:
  `Damaged`.
- Otherwise the `stco`/`co64` and `stsz` tables of each `trak` are checked: any chunk offset at or past the measured end, or
  `first chunk offset + total sample size` past it, gives `Partial`; else `Intact`. Fragmented files (`moof`) have no such
  tables and are `Intact` when their boxes are complete.

Unreadable sectors:

- Inside `moov`: the index is unusable, so `Damaged`; the length still covers `moov`, so extraction keeps it.
- Elsewhere in the file (for example inside `mdat`): an otherwise `Intact` file is `Partial`.
- On a box header: the header cannot be parsed, so the walk ends there as if the file were cut (`Partial`, or `Damaged` when
  no `moov` was seen before it). A header that cannot be read after a complete video (`moov` and `mdat` both seen) is only
  what follows the file: the video stays `Intact` (if nothing else is wrong).

### Video with a separated index

A camera writes `ftyp`, then `mdat` (the frames), then `moov` (the index) last. On a fragmented card the `moov` may end up
elsewhere: the scan then finds the start of the video `Damaged` (no `moov`) and the `moov` nowhere near it. After the
scan, such videos are paired with orphan `moov` boxes (`pairing.rs`, crate-private) and each video put back together is
reported as `Event::Updated` (same `id`, 2 pieces).

Collection during the scan: every block the scan reads is searched for `moov` (keeping the last 15 bytes of each window, so
a header across two windows or blocks is found too). This search runs on the scan thread, the one that reads the source:
it costs about 10% of the page-cache throughput on images with few files (`noise`, `video` in the benchmark), far above
what a card reader delivers, so it is accepted; it could move to the measuring threads later if it ever matters. A box start `q` is a candidate when its 16 bytes are a size in
`[24, MOOV_MAX_LEN + 8]`, `moov`, any 4 bytes and `mvhd`. A candidate is dropped when its header overlaps a piece of a
finding that was `Intact` when added (rounded to whole sectors: those blocks may never be read, so keeping candidates
there would depend on timing) or when it lies inside a non-`Damaged` MP4/MOV/CR3/HEIC finding (its own index). At most
4096 candidates per block are looked at and 4096 per scan are kept (the first ones). The videos are the `Damaged`
MP4/MOV findings (at most 4096, the first ones).

Pairing a video at `s` (the `Found` start) with a candidate at `q`:

1. Header-only walk from `s`: `ftyp`, then other boxes up to an `mdat` with a declared size, no `moov` before it and no
   `moov` header right after it. `P` = where `moov` would sit in the file (the end of `mdat`, relative to `s`); `payload` =
   the start of the `mdat` payload. Anything else is not paired.
2. Pre-filter, before loading anything: `(q − P) ≡ 0 (mod 512)` for some video (the second piece must start on a sector
   boundary). Loads are charged by the 1 MiB cache windows they span, at most 1 GiB per scan; a candidate past that is
   dropped.
3. The `moov` (at most `MOOV_MAX_LEN`) must be readable, not garbled, hold `mvhd` and a `trak`, and have consistent
   `stsz`/`stsc`/`stco`|`co64` tables (present, not truncated, the chunks holding exactly the samples `stsz` lists; at
   most 4,000,000 samples); the checkable samples (H.264 `avc1`/`avc3` with `avcC`, HEVC `hvc1`/`hev1` with `hvcC`,
   Motion-JPEG) must be at least 8. This is judged from the tables without listing a sample.
4. Arithmetic filter: `(q − P) ≡ 0 (mod 512)`, the first sample of any track within 64 KiB after `payload`, and every
   sample ending by `P` (from the chunk table and the sizes, still without listing). Only a `moov` that some video
   passes is listed: its checkable samples are charged to a budget of 16,000,000 per scan before anything is allocated
   (ruling Q11; a `moov` past it is dropped). At most 256 pairs pass the filter; the rest are dropped.
5. Sample checks, never the payload: a NAL sample is walked unit by unit (at most 512): every length at least 1, the
   header's forbidden bit 0, the type 1–23 (H.264) or at most 40 with a non-zero temporal id (HEVC), the units summing
   exactly to the sample size. A Motion-JPEG sample starts `FF D8 FF` and ends `FF D9`. `A(i)` reads sample `i` at
   `s + offset`, `B(i)` at `q − P + offset`. `k` = the first sample failing `A` (binary search; `n` if the last passes).
   Up to 32 evenly spaced samples before `k` must pass `A` and up to 32 after it must pass `B`.
6. Break points `L1`: sector boundaries (relative to `s`) from the start of sample `k − 1` (not its end: a sample cut
   inside its last NAL unit's payload still passes `A`) to the start of sample `k + 1` (or `P`), at least `payload`,
   at most 4096 tried; samples `k − 1` and `k` must pass read across `L1` (bytes before it at `s`, after it at the second
   piece). The pieces are `[s, s + L1)` and `[q − P + L1, … + (P − L1) + moov size)`, inside the source, not
   overlapping.
7. Uniqueness: a video with two or more passing `moov`s, or a `moov` passing for two or more videos, is not paired.

Health of a reassembled video: what the index says against its length (`P` + the `moov` size), as for a contiguous file
(`Damaged` or `Partial` stay so); an unreadable sector in either piece (from the scan's bad-sector map, so it agrees with
the `Unreadable` events) makes it `Partial`; so does more than one passing `L1` (the smallest is kept), or a bound hit
anywhere (the per-block or per-scan candidate cap, the video cap, the load budget, the pair cap, more than 4096 break
points, the sample budget or the step budget below): an unchecked candidate might have passed too. The per-block
candidate cap does not count in a block wholly inside committed skip ranges (such a block may or may not be read, and
all its candidates are dropped anyway), so the result does not depend on timing. Otherwise `Intact`. The checks validate
lengths and header bytes, never the payload, so a break point inside one NAL unit's payload cannot be told from one a
sector away: real H.264/HEVC with large slices (tens of KiB per NAL unit) will usually pair as `Partial` until 1.4
(the file system's cluster size will narrow `L1`). So will most real videos with sound: audio samples interleaved
between video samples `k − 1` and `k` are not checked, so every sector boundary inside them passes too. The kind comes
from the `ftyp`.

Bounds on hostile input: at most 4096 candidates and 4096 videos, 1 GiB of `moov` loads (one `moov` in memory at a
time, plus the list of its checkable samples: at most 4,000,000 per `moov`, about 16 B each, twice that while it is
built), 16,000,000 samples listed per scan, nothing allocated per sample before the filters pass, 256 pairs, 4096
break points per pair, 512 NAL units per sample check, and 2^24 sample-check steps per scan in total (past it no
further pair is checked). Judging a `moov`'s tables costs time in proportion to its bytes (so at most the 1 GiB loaded).
Cancellation is looked at before each `moov`, each pair and each break point; a cancelled scan returns
`Err(Cancelled)` without the remaining `Updated` events.

Not covered: a video in three or more pieces (a middle piece holding fewer than 1/32 of the samples between the two
verified sides may go unnoticed and be paired), a `moov` before its `mdat`, an `mdat` of size 0, two videos whose first
pieces are identical, and fragmented MP4 (`moof`).

## Formats: CR3 and HEIC (ISO BMFF)

The same `IsoBmff` detector picks the kind from the `ftyp` brands: major brand `crx ` is `FileKind::Cr3` (extension
`cr3`); a
HEIC brand (`heic heix heim heis mif1 msf1`) as major or compatible brand, with no `moov` anywhere in the file, is
`FileKind::Heic` (extension `heic`). An MP4 that merely lists `mif1` has a `moov` and stays `Mp4`; an `avif`/`avis`
major
brand is never HEIC.

- CR3: the `moov`/`stco`/`stsz` rules above apply unchanged (a CR3 has `trak`s). In addition both of its JPEGs must
  decode strictly (the raw image itself is never verified) for `Intact`: the `PRVW` JPEG in the top-level `uuid`
  `eaf42b5e-1c98-4b88-b9fb-b7dc406e4d16` and the full-size JPEG (the first chunk of the first track, when it starts with
  a JPEG SOI), each of a measurable size. A missing `PRVW` (or one whose size field does not fit its box), a `PRVW` or
  full-size JPEG that is unmeasurable, decodes leniently or does not decode is `Partial`. This is not "the largest
  decides" as for a TIFF RAW: the `PRVW` sits in a `uuid` before `mdat` and the CR3 walk checks file heads only inside
  `mdat`, so a broken `PRVW` is the only sign that a file was written over it; `Partial` keeps the scan looking for heads
  inside it (only `Intact` suppresses that). `inspect` still shows the larger of the two as the preview. The `PRVW`
  field layout comes from public format notes and the testkit, not yet from real files.
- HEIC: needs a `meta` (at most 16 MiB) whose `iloc` (versions 0, 1, 2; offset, length, base offset and index sizes of
  0, 4 or 8 bytes) lists the items' extents. `Intact` when every extent (construction method 0, data reference 0,
  offsets absolute in the file) ends inside the measured file; `Partial` when one ends past it (or the file is cut, or
  contains an unreadable sector); `Damaged` for a garbled `meta`, no `iloc`, or an `iloc` with no extents. There is no
  `mdat` rule (the data may be in `idat`), but extents the engine cannot check (construction method 1 or 2, an external
  data reference, a zero length meaning "the whole resource", an `iloc` version above 2, more than 65,536 items or
  262,144 extents) make the file at best `Partial`. The pixel data is HEVC (or similar) and is never decoded: `Intact`
  means complete structure with every extent inside the file, not that the image decodes.
- A sector-aligned file head inside `mdat` is normally "another file starts here". For CR3 and HEIC the starts of the
  file's own data (CR3: the chunks of every track, which include its full-size JPEG; HEIC: the `iloc` extents) are
  skipped, so a JPEG the file itself keeps there does not cut it; so is, for a CR3, a head inside its full-size JPEG or
  `PRVW` (the EXIF thumbnail) as long as that JPEG still decodes strictly (ruling P13, as for TIFF RAWs); any other head
  still does, and a file written over part of an own JPEG breaks its decode, so it cuts the CR3. A foreign file that starts
  exactly at a CR3 chunk or HEIC extent start is indistinguishable from the file's own data and does not cut it. The
  checks need the index to be readable: if `moov`/`meta` is missing or unusable, a head inside `mdat` cuts the file as
  before.

## Formats: Fujifilm RAF

`formats::Raf` recognizes the 16-byte magic `FUJIFILMCCD-RAW ` (`FileKind::Raf`, extension `raf`). The header holds
three
big-endian `(offset, length)` pairs at 0x54, 0x5C and 0x64: the embedded JPEG, the CFA header and the CFA (raw sensor)
data.
`confirms` (used for "another file starts here") requires each pair to end within 1 GiB; a head that does not confirm is
not
a file.

- Length: the furthest end of the three blocks (at least the 0x6C-byte header). A block that starts past the end of the
  source is not counted (a corrupted offset never claims the rest of the source), one that starts inside is kept up to
  the end of the source; either makes the file `Partial`. The file also ends at the first 512-aligned sector inside it
  where another file starts (`Partial`), except at the starts of its own blocks and inside its embedded JPEG (the EXIF
  thumbnail) while that JPEG still decodes strictly (ruling P13, as for TIFF RAWs); a file written over part of the JPEG
  breaks its decode, so it cuts the RAF and is reported on its own.
- Health: like a TIFF RAW, only the embedded JPEG can be verified. `Intact` needs a complete file, no unreadable sector
  and a strict decode of the JPEG; no JPEG, or one that decodes leniently or not at all, is `Partial`.

## Formats: TIFF-based RAW

`TiffRaw` finds CR2, NEF, ARW, DNG, ORF, RW2 and generic TIFF (kinds `Cr2`, `Nef`, `Arw`, `Dng`, `Orf`, `Rw2`, `Tiff`;
extensions `cr2`, `nef`, `arw`, `dng`, `orf`, `rw2`, `tif`). It recognizes the TIFF magics with an IFD0 offset between 8
and
64 MiB; `confirms` (used for "another file starts here") requires IFD0 to hold at least 3 entries, all of a valid type.

- A head that does not `confirm` is not a file: `measure` returns `None`, so random bytes that look like a TIFF header
  never become (damaged) findings.
- Kind: the `CR` marker at byte 8 gives CR2; the ORF and RW2 magics give ORF and RW2; `DNGVersion` gives DNG; Make
  starting with `NIKON` gives NEF and `SONY` gives ARW; anything else is a generic TIFF.
- Length: the extent from `tiff::walk` (furthest byte any IFD, value or strip references), walked with the end of the
  source as its limit, so structure that continues past the end of the source is a cut file (`Partial`), not a damaged one;
  a strip that starts inside the source is kept up to its end, while one that starts beyond it is not counted (a
  corrupted offset never claims the rest of the source). Position independence holds only
  at the cap: a range or IFD that starts at or past `TIFF_MAX_LEN` (1 GiB) is a structural error (`Damaged`) wherever the
  source ends. Below the cap the source's end still matters: a range that starts past the source's end is a cut
  (`Partial`) and is not counted, while the same range on a longer source is counted as part of the file. The file is cut at the first 512-aligned sector inside it where
  another file starts, except at the starts of its own embedded previews and of its JPEG-coded strips/tiles (Compression
  6, 7 or 34892: a lossy-JPEG tile, such as a DNG's, legitimately begins with FF D8 and confirms; lossless JPEG has no DQT,
  so it never confirms) and inside its own embedded JPEGs (the EXIF thumbnail in a preview's APP1) as long as
  every own JPEG containing that head (at most `JPEG_MAX_LEN` long) still decodes strictly, and at the end of the
  source; either makes it `Partial`. A file written over part of an own JPEG breaks that decode, so it cuts the RAW and
  is reported on its own. A file written exactly at one of those starts is indistinguishable from the RAW's own data
  and does not cut it.
- Vendor layouts (checked against real files from raw.pixls.us). Panasonic RW2: StripOffsets is a placeholder
  (`0xFFFFFFFF`, its StripByteCounts 0 or an unpacked size) and is ignored; the raw data starts at tag 0x0118 and has no
  length tag, so its length comes from the sensor size (tags 0x0002/0x0003) and packing: 16-byte packets of 14 pixels
  (RawFormat 4, 12-bit: DMC-FZ28, DMC-GF1, DMC-LX7, DC-G100) or 9 (RawFormat 5, 14-bit: DC-GH5S), rounded up to
  0x4000-byte blocks. Only packings checked against real files are used: any other (RawFormat 5 at 12 bits, and the
  newer bodies' RawFormat 6 and 7, included) leaves the length unknown and the file `Partial`, but its raw data is still
  claimed from tag 0x0118 up to a safe upper bound (2 bytes per pixel, rounded up to 0x4000 bytes), so the recovered
  file keeps it; the next file inside that bound or the end of the source ends it as usual. A sensor over 10^9 pixels
  or an overflowing size leaves the length unknown with no bound: the file is measured up to its known structure and is
  `Partial`. The preview is tag 0x002E (JpgFromRaw). Olympus/OM System ORF: the thumbnail and the
  preview live in the MakerNote (see the TIFF reader below); the old `OLYMP\0` MakerNote (E-10 era, offsets relative to
  the TIFF) is not read, so those files keep only their other previews.
- Known misread: the Sony DSLR-A100 (2006) ARW comes out `Damaged`, because its SubIFD pointer leads to scrambled data.
- Sony pads some ARW files with zeros after the raw data (to a 4 KiB or 64 KiB multiple). Nothing references that
  padding, so the finding ends at the last referenced byte; the recovered file opens like the original.
- Health: `Damaged` when the IFD structure is malformed (a cycle, an unknown type, and so on). `Partial` when cut, when
  a sector inside is unreadable, when a size cap was hit (`truncated`, for example a huge legitimate DNG), when there is
  no embedded JPEG preview, or when the largest preview (by SOF pixel count, over all previews) decodes only leniently
  or not at all (the raw data may still be fine). If the size of any preview cannot be measured (no SOF, zero size, over
  the decode cap) no smaller one may vouch for the file, so it is `Partial`. `Intact` only when the largest preview
  decodes strictly. There is no RAW decoder, so the raw data itself is never verified.
- Inside an `Intact` RAW the scan suppresses heads, so its embedded previews are not reported separately.

## TIFF reader (internal)

`tiff::walk` (and `walk_exif_ifd`, for a TIFF whose IFD0 is itself an Exif IFD, as in a CR3's `CMT2`) is the crate-private reader the TIFF-based RAW formats (CR2, NEF, ARW, DNG, ORF, RW2) share. It takes a
`Reader`, the absolute position of the TIFF header and a limit (at most `TIFF_MAX_LEN`, 1 GiB), and returns a `TiffInfo`:
the extent (`end`, relative to the header; the limit clamps every range, whatever references it, and `starts_past_cap` tells a range starting at or past `TIFF_MAX_LEN` from a mere cut), Make/Model/lens/serial/ISO/DateTimeOriginal, the largest image size, whether
`DNGVersion` is present, the embedded JPEG ranges (`JPEGInterchangeFormat`, strips/tiles of Compression 6/7 starting
with FF D8, Panasonic's JpgFromRaw and an Olympus MakerNote's JPEGs) and the other strip/tile ranges (raw data, including
Panasonic's raw data). Header magics `II*\0`, `MM\0*`, `IIRO`, `IIRS`, `MMOR`, `IIU\0`
are accepted; BigTIFF and anything else gives `None`.

It follows the IFD chain, SubIFDs, Exif, GPS (extent only) and Interop IFDs; the next pointer of Exif/GPS/Interop IFDs is
not followed. The data is hostile, so everything is bounded. Each IFD is visited once, a tag counts once per IFD (repeats
are ignored, without raising `garbled`: real cameras occasionally repeat tags), and every value block is read with a single
read. Caps: 64 IFDs, 1024 entries per IFD, 65,536 strip/tile values per tag, 262,144 per walk (which also bounds `data` +
`jpegs`), 16 SubIFDs per tag and 256 bytes per string.

Two flags report trouble. `garbled` is a structural error (cycle, offset or value range past the limit, unknown type, short
value block; the entry is skipped and never extends `end`); within it, `past_limit` marks ranges beyond the limit passed in
(for a caller whose limit is the end of the source, a cut file; a range starting inside is recorded clamped to
the limit, one starting beyond is not counted) and `structural` marks every other error (a malformed
file). `truncated` is a size cap hit (the walk is incomplete, not malformed). Callers make `structural` files `Damaged`
and treat `truncated` as at best `Partial`. First value wins for Make/Model/lens/serial/ISO/date. Each IFD is classified
once, by the SOF of its first strip/tile (they share one Compression): SOF0/1/2 go to `jpegs`; lossless JPEG (SOF3) is
`data`. `jpeg_data_starts` lists the `data` ranges that are JPEG-coded (an IFD with Compression 6, 7 or 34892, or a
JPEGInterchangeFormat range clamped to the limit), the only ones that may begin with FF D8.

Vendor data it reads: in an RW2 (`IIU\0`) IFD0, Panasonic's tags 0x0002/0x0003 (sensor size), 0x000A (bits per sample),
0x002D (raw format), 0x002E (JpgFromRaw, a lossy JPEG goes to `jpegs`) and 0x0118 (raw data offset; the length is computed
with checked arithmetic as described under TIFF-based RAW; an unknown packing or size sets `truncated`, and an unknown
packing with a usable sensor size claims the raw data up to the 2-bytes-per-pixel bound, clamped to the limit;
StripOffsets/StripByteCounts are then ignored). In
an Exif IFD, a MakerNote (0x927C) that starts with `OLYMPUS\0` (IFD at +12) or `OM SYSTEM\0\0\0` (IFD at +16), in the byte
order its header names and with offsets relative to the MakerNote: its thumbnail (0x0100) and the preview of its
CameraSettings IFD (0x2020 → 0x0101 start, 0x0102 length) go to `jpegs` when they lie inside the MakerNote and are lossy
JPEGs. Each MakerNote IFD is read with one read of at most 1024 entries; a MakerNote that does not parse adds nothing and
is not an error (vendor data, not TIFF structure).

## Findings in pieces

A found file may be made of several pieces of the source (a fragmented file: another file was written in the middle of
it). `Fragment { start, len }` (re-exported as `revenant_core::Fragment`) is one absolute range. `Measurement`, `Finding`
and `Preview` carry `fragments: Vec<Fragment>` with one contract: empty for a contiguous file; otherwise at least 2 pieces
in file order, the first starting at `start`, their lengths summing to `len`, no two overlapping. `start` is always the
first piece's start and `len` the total length. `Finding::ranges()` and `Preview::ranges()` give `[Fragment { start, len }]`
for a contiguous file, else the pieces. A measurement that breaks this contract (or has a piece outside the
source) is a detector bug: debug builds assert on it, release builds ignore it (no finding). `formats::Jpeg` produces two
pieces for a photo it puts back together (see "Fragmented JPEG"); the scan copies a measurement's `fragments` into its
finding. After an `Intact` finding in pieces, the scan skips heads only inside
its pieces (each rounded out to whole sectors), so files in the gaps between them are still found.

## Inspect

`inspect(source, &Finding) -> Inspection` reads what a found file says about itself: `Metadata` (make, model, lens, serial,
ISO, `captured_at` as `YYYY-MM-DDTHH:MM:SS`, width, height) and an optional `Preview` (an absolute `start`/`len` range of an
embedded JPEG plus its size from the SOF, ready to show on the results screen). It never fails and never panics: what cannot
be read is `None`, nothing is read outside the finding, and the walks reuse the detectors' caps (`TIFF_MAX_LEN`,
`MOOV_MAX_LEN`, `JPEG_MAX_LEN`).

A finding in pieces is inspected as one contiguous file read through a `source::Fragmented` view of its pieces (the same
finding with `start` 0 and no pieces), so it gives the same details as the whole file would; the preview's logical range
is then mapped back with `absolute()`: its `start` is its first absolute byte, and it gets `fragments` only when it spans
more than one piece.

| Kind                            | Details from                                                      | Preview                                              |
| ------------------------------- | ----------------------------------------------------------------- | ---------------------------------------------------- |
| JPEG                            | first `Exif` APP1; size from the SOF                              | the whole file                                       |
| CR2, NEF, ARW, DNG, ORF, RW2, TIFF | the IFD walk                                                  | the largest embedded lossy JPEG (by SOF pixel count) |
| RAF                             | the embedded JPEG's APP1; size from its SOF                       | the embedded JPEG                                    |
| CR3                             | `CMT1` (make, model), `CMT2` (an Exif IFD: ISO, date, lens, serial) | the larger of `PRVW` and track 1's first chunk if it is a JPEG |
| HEIC                            | the `Exif` item listed in `iloc` (4-byte offset prefix skipped)   | none (HEVC; the app uses the OS decoder)             |
| MP4, MOV                        | `mvhd` creation time (UTC, seconds since 1904) and the first video track's `tkhd` size | none                           |

Values are validated or dropped: a date must be a real calendar date (leap years included), time of day valid and the year
1990 or later; strings are printable ASCII, trimmed and non-empty; ISO is above zero; sizes are above zero. A preview is
kept only if it starts with `FF D8 FF`, its SOF parses and its whole range lies inside the finding.

## Scan

`scan(source, formats, &ScanOptions, &CancelToken, &mut on_event) -> Result<ScanSummary, EngineError>` walks a source in blocks
(`ScanOptions::block`, default 4 MiB, rounded down to whole sectors) and looks at the first 16 bytes of every sector
(`ScanOptions::sector`, default 512, at least 16). The first format that recognizes a head and measures a file there wins. The
scan keeps reading every block. After an `Intact` finding it examines no head inside the file (inside its pieces, for a
file in pieces); after a `Partial` or `Damaged` finding it keeps examining heads, so files inside a broken one are still reported (one bad
file never hides the rest of the disk; a damaged file may then overlap another finding). `measure` reads the whole source
(not just the block), so files may cross block boundaries.

Reading once: the scan and every `measure` read through one shared cache (`CachedSource`, crate-private) of
`ScanOptions::cache_bytes` (default 256 MiB; values below 1 MiB are raised to it). "Read once" is about reading the
source (I/O): bytes already in the cache may be walked several times in memory (a block's heads and its `moov` search, a
file's measurement, a JPEG decoded again by its gap search). For files that fit in the cache, each sector is read once
from the source by the scan and the detectors; a JPEG gap search re-reads the photo, its 64 MiB window and at most
64 MiB for its tries (see "Fragmented JPEG"), the pairing step after the scan reads the headers of the candidate videos,
the `moov`s it loads (at most 1 GiB) and the samples it checks again (see "Video with a separated index"), and a
`Partial` or
`Damaged` finding larger than the cache may be read again (the scan keeps examining heads inside it, so it reads its blocks
itself). The cache reads the source in aligned 1 MiB windows (rounded down to whole sectors) and keeps the most recently used
ones up to `cache_bytes`; a window being loaded is not counted yet, so each reading thread can hold one more window on top.
The buffer of an evicted window is reused by the load that evicted it. A window that cannot be read in
one go is resolved sector by sector (scan sector size: with a sector above 512, the detectors see a scan sector that failed
only in part as wholly unreadable, which only makes health more conservative); each unreadable sector is remembered in a shared bad-sector map, and
a read that overlaps it fails every time without touching the source again, also when its window is loaded again after
being evicted (the load reads around the known bad runs). The map keeps at most 1,048,576 merged runs; past that, new
unreadable sectors are not remembered and are read again when their window is loaded again (slower, with the same results
and events). A block that lies wholly inside an `Intact` finding whose bytes have all gone through the cache already (the
detector walked the file) is not read again; its unreadable sectors come from the bad-sector map, so its `Unreadable` events
are the same. The cache is `Send + Sync`: windows are shared across threads, and a window being loaded by one thread is
waited for by the others, never read twice at once. The scan looks at the heads of a block straight in the cache's windows,
without copying the block.

Threads: the source is still read in order, by the scan thread, while candidate files are measured on worker threads
(`ScanOptions::threads`: 0, the default, means one per available core, at most 16; 1 measures on the scan thread itself;
other values are capped at 64; `ScanOptions::resolved_threads()` gives the number used). The scan thread reads up to
`ScanOptions::lookahead_blocks()` = `min(16, max(1, cache_bytes / (2 × block)))` blocks ahead of the block it is examining,
so the read-ahead fits in half the cache and a block is still cached when it is examined, and queues one job for each
sector start some format recognizes. It then examines the blocks in order exactly as a sequential scan would: a head
inside a skip range committed meanwhile is ignored (its result is thrown away), and for every other head it waits for that
head's measurement. Because a measurement depends only on the source's bytes and unreadable sectors, the events, their
order, the ids and `ScanSummary` are the same for any `threads` and any `cache_bytes`. A JPEG validation decode holds
part of a process-wide budget of 512 MiB (its bytes plus one byte per pixel of the grayscale output) while it runs. The
budget counts only those: the decoder's internal buffers (for example a progressive JPEG's coefficients) come on top.

Memory: at most `cache_bytes` of cached windows, plus for each reading thread (the scan thread and every measuring
thread) one window being loaded (1 MiB) and its reader pool (at most 4 MiB), plus the decode budget (512 MiB, shared by
every scan in the process, plus the decoder's internal buffers), plus what each running measurement holds (for example a
`moov` of at most 16 MiB, or a JPEG gap search's DC values, about 3 × 13 MiB at 210 megapixels, plus 3 MiB), plus about
200 B per recognized sector start in the read-ahead (at most
lookahead × block / sector of them), plus the `moov` candidates (16 B each: at most 4096 per block read ahead, and
4096 kept) and the `Damaged` videos kept for pairing (at most 4096 findings). After the scan, the pairing step holds one
`moov` (at most 16 MiB) and its sample lists (at most 4,000,000 samples, about 32 B each, so up to about 128 MiB). With
the defaults on a 16-thread machine: about 256 MiB + 17 × 5 MiB + 512 MiB, plus the measurements' own buffers.

Events:

- `Found(Finding)`: sequential `id` from 1, in start order. It is emitted once the scan has read past the file's end (the end of its
  farthest piece), so unreadable sectors inside the file are known: they are reported as `Unreadable` events before the
  `Found`, and an `Intact` finding with any of them inside one of its pieces is downgraded to `Partial`. A file found inside a `Partial`/`Damaged` one is emitted right
  after it.
- `Progress { read, total }`: once per block, plus one for an empty source; the last has `read == total`.
- `Unreadable { start, len }`: when a block cannot be read it is re-read sector by sector; each run of consecutive failed
  sectors (also across blocks) is one event and counts in `ScanSummary::unreadable_bytes`. A run is split where a finding
  ends. The scan then continues.
- `Updated(Finding)`: a better version of a finding already reported, which replaces the earlier `Found` with the same
  `id`. Emitted after every `Found` (and the last `Progress`), before `scan` returns, in id order; counted in
  `ScanSummary::updated`. Today: a video put back together with its separated `moov` (see "Video with a separated
  index").

`CancelToken::cancel()` (from any thread or from inside the callback) stops the scan: it is checked before each block examined and each block read ahead, before and after
every event and while waiting for a measurement. Once it is seen no further event is emitted (a cancel from inside the
callback makes that event the last one), the measuring threads drop their remaining jobs, and `scan` returns
`EngineError::Cancelled` after every thread has stopped (a measurement already running is finished first, except a JPEG
gap search, which gives up as soon as it sees the cancel); findings still pending are dropped.

## Performance

Measured with `revenant-dev bench` (release build, 1 GiB images, best of 3 runs; see `crates/revenant-dev/README.md`)
on the development machine: Intel Core i9-9980HK (8 cores / 16 threads), 32 GB of RAM, macOS, Rust 1.99. The images
are read from the page cache (the bench has just written them), so these numbers measure the engine, not a disk: they
are an upper bound for a card reader. "Before" is the start of part 1.3 (commit `e575cd4`: one thread, every detector
reading on its own); "after" is the end of it (commit `718f1f1`), with the default threads (16 here) and with one.
MB = 10^6 bytes.

| Image        | Files | Before (MB/s) | After, 16 threads (MB/s) | After, 1 thread (MB/s) |
| ------------ | ----: | ------------: | -----------------------: | ---------------------: |
| `noise`      |     0 |         765.6 |                   2381.4 |                 2265.4 |
| `dense-jpeg` |    95 |          21.8 |                    503.9 |                  102.8 |
| `dense-raw`  |    37 |         158.1 |                   1576.9 |                  589.5 |
| `video`      |     3 |         797.2 |                   2296.6 |                 2216.3 |
| `cut-jpeg`   |    95 |          26.8 |                    419.3 |                   85.6 |

Every placed file is found in every run. What it means for a card: an SD card reader delivers at most about 100 MB/s
(UHS-I) to about 300 MB/s (UHS-II), so with the default threads the scan keeps up with the reader on every image here,
the photo-dense ones included (`dense-jpeg` 503.9 and `dense-raw` 1576.9 MB/s, above the 300 MB/s goal). With one
thread it does not on the photo-dense JPEG images: `dense-jpeg` is 102.8 MB/s and `cut-jpeg` 85.6 MB/s, below 300 MB/s
(checking JPEGs is CPU work, and one core does it at about that rate). `cut-jpeg` has no speed target: each cut photo
starts a gap search that may re-read its 64 MiB window. The `moov` search on the scan thread costs about 10% on
`noise` and `video`. The bench JPEGs are noise-encoded (about 11 MB per 3000x2000 photo, more entropy than a real
photo), a heavy case; the real camera samples (below) all still come out as one `Intact` finding each.

## Extract

`extract(source, &Finding, dest_dir) -> Result<PathBuf, EngineError>` copies a finding's bytes (in 1 MiB chunks) into `dest_dir`
as `revenant_<start as 12 hex digits>.<ext>`; if that exists, `_2`, `_3`… are tried (files are created with `create_new`, so
nothing is ever overwritten; after 10,000 attempts it fails with `write_failed`).

- Refuses with `destination_on_source` when the destination is on the source's volume (writing to a damaged card could destroy
  what is still recoverable). If the source's volume is unknown (`None`) extraction is allowed. If the destination's volume
  cannot be determined (e.g. the directory does not exist) it returns `open_failed` and writes nothing.
- A finding in pieces is written as its pieces (`Finding::ranges()`) one after another, in file order; the file is still
  named after `start` (the first piece's start).
- A chunk that cannot be read is re-read sector by sector (512 bytes); only the unreadable sectors are written as zeros, so a
  partial file comes out instead of none. Each piece stops at the source's end, even if its length runs past it.
- A write error returns `write_failed` and removes the incomplete file.

## Tested against real files

`tests/real_samples.rs` runs the engine over real camera files (one each of CR2, CR3, NEF, ARW, RAF, DNG, ORF, RW2; all
CC0 from raw.pixls.us, listed with URL and SHA-256 in `samples/samples.toml`, never committed). Each is embedded in a
noise image at a sector start and must come out as exactly one finding of its kind, `Intact`, with the file's length
(for ARW, trailing zero padding under 64 KiB aside), make and model, and a preview that decodes strictly. It is ignored by default:

```sh
scripts/fetch-samples.sh
REVENANT_SAMPLES=samples/cache cargo test -p revenant-core --test real_samples -- --ignored
```

CI runs it in the `samples` job: a failed download (raw.pixls.us down) skips the test without failing the build, while
a failing test fails it. There is no HEIC sample: none with a verifiable CC0/public-domain license was found.

## Trying it

The `revenant-dev` crate is a command line over this engine: `revenant-dev scan <image> [--extract <dir>]
[--previews <dir>]` prints each finding, with its photo details and preview, as a JSON line. See
`crates/revenant-dev/README.md`.
