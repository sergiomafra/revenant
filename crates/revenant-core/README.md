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

## Formats: JPEG

`formats::Format` is the detector interface: `recognizes(head)` checks a signature at a sector start,
`confirms(source, start)` validates the header there (cheap, never recursive, never measures the whole file), and
`measure(source, start)` returns a `Measurement { len, kind, health }` found by walking the file's structure, or `None` when
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

`formats::Jpeg` follows the marker segments by their declared lengths (so the thumbnail embedded in an APP1/EXIF segment does
not end the file; DAC and JPG markers count as segments, and `FF` fill bytes before a marker are skipped), requires DQT and SOF before SOS, and scans the entropy-coded data up to EOI (`FF D9`), up to
`JPEG_MAX_LEN` (64 MiB). If the header walk breaks on an unreadable sector, the search for EOI continues from there. Health:

- `Intact`: EOI found, no unreadable sector inside, and `zune-jpeg` decodes the bytes in strict mode.
- `Partial`: no EOI before the end of the source, `JPEG_MAX_LEN`, or a 512-byte sector that starts another recognized file
  (the length stops there); or EOI found and only the lenient decode succeeds; or EOI found, the file contains unreadable
  sectors and a decode (strict or lenient) of the zero-filled bytes succeeds. The length never exceeds `JPEG_MAX_LEN`.
  Also `Partial`: the structure is complete but the declared size is over 210 megapixels, so it is not decoded ("not fully
  verifiable").
  Also `Partial`: a DAC segment (arithmetic coding) with complete structure, which `zune-jpeg` cannot decode.
- `Damaged`: EOI found but neither decode succeeds. The length is still exact.

The validation decode outputs grayscale (1 byte per pixel) to bound memory.

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
- No `moov`: `Damaged` (the media data is there but not the index; rebuilding it is a later product milestone).
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
  corrupted offset never claims the rest of the source). The health does not depend on where the source ends: a range
  that starts at or past `TIFF_MAX_LEN` (1 GiB) is a structural error (`Damaged`) whether the card ends right after the file
  or 3 GiB later, and one that starts below the cap and runs past the end of the source (or past the cap) is a cut
  (`Partial`). The file is cut at the first 512-aligned sector inside it where
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

## Inspect

`inspect(source, &Finding) -> Inspection` reads what a found file says about itself: `Metadata` (make, model, lens, serial,
ISO, `captured_at` as `YYYY-MM-DDTHH:MM:SS`, width, height) and an optional `Preview` (an absolute `start`/`len` range of an
embedded JPEG plus its size from the SOF, ready to show on the results screen). It never fails and never panics: what cannot
be read is `None`, nothing is read outside the finding, and the walks reuse the detectors' caps (`TIFF_MAX_LEN`,
`MOOV_MAX_LEN`, `JPEG_MAX_LEN`).

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
scan keeps reading every block. After an `Intact` finding it examines no head until the first sector at or after the file's
end; after a `Partial` or `Damaged` finding it keeps examining heads, so files inside a broken one are still reported (one bad
file never hides the rest of the disk; a damaged file may then overlap another finding). `measure` reads the source directly,
so files may cross block boundaries.

Events:

- `Found(Finding)`: sequential `id` from 1, in start order. It is emitted once the scan has read past the file's end, so
  unreadable sectors inside the file are known: they are reported as `Unreadable` events before the `Found`, and an `Intact`
  finding that contains any of them is downgraded to `Partial`. A file found inside a `Partial`/`Damaged` one is emitted right
  after it.
- `Progress { read, total }`: once per block, plus one for an empty source; the last has `read == total`.
- `Unreadable { start, len }`: when a block cannot be read it is re-read sector by sector; each run of consecutive failed
  sectors (also across blocks) is one event and counts in `ScanSummary::unreadable_bytes`. A run is split where a finding
  ends. The scan then continues.

`CancelToken::cancel()` (from any thread or from inside the callback) makes the scan return `EngineError::Cancelled` at the
next block boundary; findings still pending at that point are dropped.

## Extract

`extract(source, &Finding, dest_dir) -> Result<PathBuf, EngineError>` copies a finding's bytes (in 1 MiB chunks) into `dest_dir`
as `revenant_<start as 12 hex digits>.<ext>`; if that exists, `_2`, `_3`… are tried (files are created with `create_new`, so
nothing is ever overwritten; after 10,000 attempts it fails with `write_failed`).

- Refuses with `destination_on_source` when the destination is on the source's volume (writing to a damaged card could destroy
  what is still recoverable). If the source's volume is unknown (`None`) extraction is allowed. If the destination's volume
  cannot be determined (e.g. the directory does not exist) it returns `open_failed` and writes nothing.
- A chunk that cannot be read is re-read sector by sector (512 bytes); only the unreadable sectors are written as zeros, so a
  partial file comes out instead of none. The copy stops at the source's end, even if the finding's length runs past it.
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
