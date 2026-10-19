# Changelog

All notable changes to this project are documented in this file.
The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- MP4/MOV detection: exact length from the box structure, health from the `moov` chunk and sample tables.
- Sector scan: finds photos and videos in a disk image with progress, cancellation, and unreadable-region reporting.
