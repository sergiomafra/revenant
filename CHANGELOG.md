# Changelog

All notable changes to this project are documented in this file.
The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- MP4/MOV detection: exact length from the box structure, health from the `moov` chunk and sample tables.
- Sector scan: finds photos and videos in a disk image with progress, cancellation, and unreadable-region reporting.
- Extraction: copies a found file into a chosen folder without overwriting, and refuses to write onto the source's own volume.
- Engine: finds JPEG photos and MP4/MOV videos in card images, including formatted cards; classifies them as
  intact/partial/damaged; recovers them to another folder.
- Unreadable sectors no longer hide files: a photo or video next to or containing a bad sector is still found, and is
  never labelled intact if it contains one.
- `revenant-dev`: developer command line (`scan <image> [--extract <dir>]`) that prints findings as JSON lines.
