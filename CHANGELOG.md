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
- A found file can be described as several pieces of the source (`fragments`), the groundwork for reassembling fragmented
  photos and videos: extraction writes the pieces in order, photo details and previews are read across them, and
  `revenant-dev` adds a `fragments` key to a `found` line (and its `preview`) only when there are pieces. Contiguous files
  are unchanged.
- A photo split in two pieces on the card (another file was written in the middle of it) is put back together: the rest of
  the photo is searched for in the next 64 MiB, and the photo is recovered whole, its pieces in order. It is reported
  intact only when exactly one continuation fits, every possible one was tried and the photo has no restart markers;
  otherwise it is partial, though still recovered whole (for example a photo from a camera that writes restart markers,
  or two photos from one camera written interleaved). The search is bounded (a first piece of at least 16 KiB, at most
  4096 tries reading at most 64 MiB) and stops at once when the scan is cancelled.
- A camera video whose index ended up elsewhere on the card (its `moov`, written last, landed away from the frames) is put
  back together after the scan: the damaged video is reported again, whole, with its two pieces in order (a new
  `updated` event). It is reported intact only when exactly one break point between the pieces fits and nothing was
  left unchecked; real H.264/HEVC videos with large slices will usually come out partial (they play, at most one frame
  may be damaged). A moov that fits two videos, or a video that fits two moovs, is left alone; the first piece never
  runs past where another file starts, and a video with another file starting inside its second piece is at most
  partial. The search is bounded
  (at most 4096 candidates, 1 GiB of indexes loaded, 256 pairs checked) and stops when the scan is cancelled.
- Real camera samples: an opt-in test (and CI job) runs the engine over real CR2, CR3, NEF, ARW, RAF, DNG, ORF and RW2
  files (CC0, from raw.pixls.us), downloaded on demand by `scripts/fetch-samples.sh` and checked by SHA-256.
- FAT and exFAT volumes are found on MBR (including logical partitions and disks with 4096-byte sectors), GPT and
  unpartitioned media, also with a damaged boot sector (the backup boot sector is used) or a lost partition table (the
  usual partition starts are probed).
- The live files of FAT12/16/32 volumes are listed with their original folders and names (long names included), their
  creation and modification times and where their pieces are on the disk, even when a file is fragmented or a FAT
  sector is unreadable (the FAT's second copy is used). A damaged or forged volume (directory loops, chains that loop
  or cross each other) is read in bounded time, and the files it touches are marked as not trusted.
- Deleted photos and videos on FAT12/16/32 volumes are listed with their folders and names, including the contents of
  deleted folders: the lost first letter comes back from the long name, or from similar names nearby; where the file
  probably lies is guessed from the clusters that are free today (never from clusters another file now uses); on FAT32
  cards where Windows deleted the file, its real starting point is found by checking the few places it can be. Deleted
  files are never trusted more than the evidence allows: a guess that had to step over other files is marked as not
  trusted.

### Changed

- A JPEG cut before its first row of pixels (for example right after its header, when another file was written over the
  rest or the rest of the card was erased) is damaged, not partial: nothing of the photo would show; so is one whose data
  breaks within its first row before an end-of-image marker. A JPEG cut or broken later is partial: the rows before the
  break show (a common JPEG whose data breaks in the middle used to be damaged when no decoder accepted the rest).
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
- The scan and the file detectors share what they read (a 256 MiB cache by default), so each sector of a card is read
  once (the search for the rest of a broken photo re-reads that photo, the 64 MiB after it and at most 64 MiB more; a
  cache under about 128 MiB re-reads more), and known unreadable sectors are never retried (retrying a bad sector on a
  failing card can take seconds each time).
- Scanning uses every core: the card is still read in order, while the files found are checked in parallel. The results
  are the same, in the same order, whatever the number of threads; cancelling stops the scan without reporting anything
  more. The JPEG checks share a memory budget (512 MiB at once), so many large photos checked together cannot exhaust
  memory.
- Scanning is much faster. Measured with `revenant-dev bench` (release build, 1 GiB synthetic images read from the page
  cache, best of 3) on an Intel Core i9-9980HK (8 cores / 16 threads), before → after, in MB/s: photo-dense JPEG
  21.8 → 503.9, photo-dense RAW 158.1 → 1576.9, cut JPEGs 26.8 → 419.3, videos 797.2 → 2296.6, empty space
  765.6 → 2381.4. With a single thread: 102.8, 589.5, 85.6, 2216.3 and 2265.4. These are upper bounds for a card
  reader, which delivers at most about 100 MB/s (UHS-I) to 300 MB/s (UHS-II).

### Fixed

- Half a photo followed by zeros, by another photo's data or by its own data with a gap, then an end-of-image marker, was
  reported intact (the decoder used for the check fills in what is missing), as a file of its own or as a RAW's preview.
  Common JPEGs are now verified by Revenant's own decoder, which checks every block is there (and is faster); such a
  photo is partial, and put back together when the rest of it is found.
- A file written in the space a RAW reserves for its preview, past the preview's end, was hidden as part of the RAW; it
  is now found.
- A small file written inside a Canon CR3's `PRVW` preview was hidden and the CR3 reported intact; a CR3 is now intact only
  if both its preview and its full-size JPEG decode, so the file inside is found.
