# Changelog

All notable changes to this project are documented in this file.
The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- JPEG photo detection: exact length from the marker structure (an embedded EXIF thumbnail does not end the file), health
  from a real decode.
- MP4/MOV video detection: exact length from the box structure, health from the `moov` chunk and sample tables; a video
  whose size was never written (battery died mid-recording) or whose data was partly overwritten ends where the next file
  starts. A cut-off recording is measured in time proportional to the recording, not to the rest of the card.
- TIFF-based RAW photo detection (CR2, NEF, ARW, DNG, ORF, RW2) and generic TIFF: exact length from the IFD structure, health
  from the embedded JPEG preview (the raw data itself cannot be verified, so a RAW without a decodable preview is partial).
  Panasonic RW2 raw data, which has no length tag, is measured from the sensor size and packing (a packing not yet checked
  against a real camera keeps the raw data up to a safe upper bound, as a partial file); Olympus/OM System ORF
  previews are read from the MakerNote; a photo written exactly at the start of an uncompressed raw strip now cuts the RAW
  and is found; a thumbnail inside a RAW's own preview (TIFF RAW, RAF, CR3) no longer cuts the
  RAW.
- Fujifilm RAF, Canon CR3 and HEIC photo detection: exact length from the file structure (RAF header blocks; CR3 and HEIC
  boxes), health from the embedded JPEG (RAF; a CR3's preview and full-size JPEG, both of which must decode) or from whether every item extent lies inside the file (HEIC; the
  image data itself is never decoded). A JPEG a CR3 or HEIC keeps inside its own data no longer cuts it.
- Photo details and preview: for any found file, camera make and model, lens, serial number, ISO, capture time and dimensions
  (JPEG, RAW, CR3, HEIC; capture time and size for MP4/MOV), and the embedded JPEG the results screen can show (the largest
  one in a RAW, the `PRVW` or full-size JPEG of a CR3). Values that are not valid (an impossible date, a zero ISO) are left out.
- Sector scan of a disk image (including formatted cards) with progress, cancellation and unreadable-region reporting; each
  file is classified as intact, partial or damaged. Unreadable sectors never hide a file (a file containing one is never
  intact), and files inside a partial or damaged file are still found.
- Extraction: copies a found file into a chosen folder without overwriting, never past the end of the source, and refuses to
  write onto the source's own volume.
- `revenant-dev`: developer command line (`scan <image> [--extract <dir>]`) that prints findings as JSON lines.
- `revenant-dev`: each finding's photo details (`make`, `model`, `lens`, `serial`, `iso`, `captured_at`, `width`,
  `height`) and embedded `preview` in its JSON line, and `--previews <dir>` to save each preview as
  `<name>.preview.jpg`.
- Real camera samples: an opt-in test (and CI job) runs the engine over real CR2, CR3, NEF, ARW, RAF, DNG, ORF and RW2
  files (CC0, from raw.pixls.us), downloaded on demand by `scripts/fetch-samples.sh` and checked by SHA-256.

### Changed

- A RAW with a corrupt offset at or past 1 GiB is damaged wherever the card ends (it used to be partial on a short card).
  Below 1 GiB the end of the card still matters: an offset past the end is a cut file (partial), while the same offset on a
  longer card is counted.
- A stray MP4 signature (an `ftyp` header with no video behind it) no longer cuts a photo that was written over it.
- A random byte pattern that looks like a JPEG or MP4 signature no longer cuts another file: "another file starts here" now
  requires a valid header, not just a signature.
- A complete video followed by an unreadable sector stays intact; Motion-JPEG videos (`mjpa`, `mjpb`, `jpeg`) are no longer
  cut by the JPEG frames inside their data.
- JPEGs with DAC/JPG segments or `FF` fill bytes before a marker are measured instead of skipped; a JPEG too large to decode
  (over 210 megapixels) with a complete structure is partial instead of damaged.

### Fixed

- A small file written inside a Canon CR3's `PRVW` preview was hidden and the CR3 reported intact; a CR3 is now intact only
  if both its preview and its full-size JPEG decode, so the file inside is found.
