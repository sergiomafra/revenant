# revenant-core

The Revenant recovery engine (library). It never writes to the source being read, and contains no UI text.

## Current state

For now the crate only defines `EngineError`, the engine's error type. Every variant has a stable `snake_case` code, returned by
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
the bytes are not a file of that format. `formats::all_formats()` lists the supported detectors.

`formats::Jpeg` follows the marker segments by their declared lengths (so the thumbnail embedded in an APP1/EXIF segment does
not end the file), requires DQT and SOF before SOS, and scans the entropy-coded data up to EOI (`FF D9`). The source is read in
1 MiB windows, never whole, up to `JPEG_MAX_LEN` (64 MiB). Health:

- `Intact`: EOI found and `zune-jpeg` decodes the bytes in strict mode.
- `Partial`: no EOI before the end of the source, `JPEG_MAX_LEN`, or a 512-byte sector that starts another recognized file
  (the length stops there); or EOI found and only the lenient decode succeeds. The length never exceeds `JPEG_MAX_LEN`.
- `Damaged`: EOI found but neither decode succeeds, or the declared size is over 210 megapixels (not decoded). The length is
  still exact.

The validation decode outputs grayscale (1 byte per pixel) to bound memory.

Synthetic JPEGs and disk images for tests come from the `revenant-testkit` workspace crate (`publish = false`).

## Formats: MP4/MOV

`formats::IsoBmff` recognizes a `ftyp` box (declared size 8..=4096) at a sector start and measures the file by walking the
top-level boxes (`ftyp moov mdat free skip wide uuid meta pdin moof mfra styp sidx udta`) from their declared sizes. Size 1
means a 64-bit `largesize`; size 0 (only for `mdat`) runs to the end of the source. The walk stops at an unaccepted type, a
size under 8, or the end of the source; a box that runs past the end counts only up to it. The major brand `qt  ` gives
`FileKind::Mov`, anything else `Mp4`. Every size is checked against the source length; only `moov` is loaded into memory, and
only up to `MOOV_MAX_LEN` (16 MiB).

Walk boundaries: `ftyp` is accepted only as the first box. Once both `moov` and `mdat` are seen, only trailing
`free skip wide uuid meta udta mfra` boxes continue the file (plus `moof sidx styp mdat` when it is fragmented); anything else
ends it, and a trailing box that overruns the source is left out. The walk also stops at a box that starts on a 512-byte sector
that another format recognizes (checked at box starts, not inside boxes). A size-0 `mdat` measures to the end of the source.
Chunk offsets are relative to the start of the file (its `ftyp`), not to the source.

- No `mdat`: not a video (`None`).
- No `moov`: `Damaged` (the media data is there but not the index; rebuilding it is a later product milestone).
- `moov` larger than 16 MiB: `Damaged`.
- A box declared larger than the source (cut mid-`mdat`, or mid-`moov`): `Partial`.
- Otherwise the `stco`/`co64` and `stsz` tables of each `trak` are checked: any chunk offset at or past the measured end, or
  `first chunk offset + total sample size` past it, gives `Partial`; else `Intact`. Fragmented files (`moof`) have no such
  tables and are `Intact` when their boxes are complete.
