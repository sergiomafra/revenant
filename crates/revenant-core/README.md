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

`source::FileSource` implements it for regular files and device nodes, opened read-only (`pread` on Unix, `seek_read` on
Windows). `source::volume_of(path)` returns the `VolumeId` of any existing path (device id on Unix, volume serial number on
Windows), so extraction onto the source's own volume can be refused. A test scans `src/source/` to ensure no write-capable open
flags are ever introduced.

## Formats: JPEG

`formats::Format` is the detector interface: `recognizes(head)` checks a signature at a sector start and
`measure(source, start)` returns a `Measurement { len, kind, health }` found by walking the file's structure, or `None` when
the bytes are not a file of that format. `None` never means "could not read": a read error does not make a file vanish.
`formats::all_formats()` lists the supported detectors.

Detectors read the source through one shared reader, in 1 MiB windows aligned to 512-byte sectors (never a whole file at
once). When a window cannot be read it is re-read sector by sector: unreadable sectors read as zeros and are remembered, and
only the end of the source ends a walk for lack of data. A file whose measured range contains an unreadable sector is never
`Intact` (details per format below).

`formats::Jpeg` follows the marker segments by their declared lengths (so the thumbnail embedded in an APP1/EXIF segment does
not end the file), requires DQT and SOF before SOS, and scans the entropy-coded data up to EOI (`FF D9`), up to
`JPEG_MAX_LEN` (64 MiB). If the header walk breaks on an unreadable sector, the search for EOI continues from there. Health:

- `Intact`: EOI found, no unreadable sector inside, and `zune-jpeg` decodes the bytes in strict mode.
- `Partial`: no EOI before the end of the source, `JPEG_MAX_LEN`, or a 512-byte sector that starts another recognized file
  (the length stops there); or EOI found and only the lenient decode succeeds; or EOI found, the file contains unreadable
  sectors and a decode (strict or lenient) of the zero-filled bytes succeeds. The length never exceeds `JPEG_MAX_LEN`.
- `Damaged`: EOI found but neither decode succeeds, or the declared size is over 210 megapixels (not decoded). The length is
  still exact.

The validation decode outputs grayscale (1 byte per pixel) to bound memory.

Synthetic JPEGs and disk images for tests come from the `revenant-testkit` workspace crate (`publish = false`).

## Formats: MP4/MOV

`formats::IsoBmff` recognizes a `ftyp` box (declared size 8..=4096) at a sector start and measures the file by walking the
top-level boxes (`ftyp moov mdat free skip wide uuid meta pdin moof mfra styp sidx udta`) from their declared sizes. Size 1
means a 64-bit `largesize`; size 0 (only for `mdat`, e.g. a recording cut by a dead battery) runs to the first sector where
another file starts, or to the end of the source. The walk stops at an unaccepted type, a
size under 8, or the end of the source; a box that runs past the end counts only up to it. The major brand `qt  ` gives
`FileKind::Mov`, anything else `Mp4`. Every size is checked against the source length; only `moov` is loaded into memory, and
only up to `MOOV_MAX_LEN` (16 MiB).

Walk boundaries: `ftyp` is accepted only as the first box. Once both `moov` and `mdat` are seen, only trailing
`free skip wide uuid meta udta mfra` boxes continue the file (plus `moof sidx styp mdat` when it is fragmented); anything else
ends it, and a trailing box that overruns the source is left out. The walk also stops at a box that starts on a 512-byte sector
where a recognized file starts. Inside `mdat`, every 512-byte sector start (absolute offset) is checked as well: where a
recognized file of any format starts, the video ends there. An `mdat` whose declared size is cut this way counts as a box
declared larger than what exists (below). Chunk offsets are relative to the start of the file (its `ftyp`), not to the source.

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
  no `moov` was seen before it).

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
  partial file comes out instead of none. Bytes past the source's end are also zeros.
- A write error returns `write_failed` and removes the incomplete file.

## Trying it

The `revenant-dev` crate is a command line over this engine: `revenant-dev scan <image> [--extract <dir>]` prints each finding
as a JSON line. See `crates/revenant-dev/README.md`.
