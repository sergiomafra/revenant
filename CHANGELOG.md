# Changelog

All notable changes to this project are documented in this file.
The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- JPEG photo detection: exact length from the marker structure (an embedded EXIF thumbnail does not end the file), health
  from a real decode.
- MP4/MOV video detection: exact length from the box structure, health from the `moov` chunk and sample tables; a video
  whose size was never written (battery died mid-recording) or whose data was partly overwritten ends where the next file
  starts.
- Sector scan of a disk image (including formatted cards) with progress, cancellation and unreadable-region reporting; each
  file is classified as intact, partial or damaged. Unreadable sectors never hide a file (a file containing one is never
  intact), and files inside a partial or damaged file are still found.
- Extraction: copies a found file into a chosen folder without overwriting, never past the end of the source, and refuses to
  write onto the source's own volume.
- `revenant-dev`: developer command line (`scan <image> [--extract <dir>]`) that prints findings as JSON lines.

### Changed

- A random byte pattern that looks like a JPEG or MP4 signature no longer cuts another file: "another file starts here" now
  requires a valid header, not just a signature.
- A complete video followed by an unreadable sector stays intact; Motion-JPEG videos (`mjpa`, `mjpb`, `jpeg`) are no longer
  cut by the JPEG frames inside their data.
- JPEGs with DAC/JPG segments or `FF` fill bytes before a marker are measured instead of skipped; a JPEG too large to decode
  (over 210 megapixels) with a complete structure is partial instead of damaged.
