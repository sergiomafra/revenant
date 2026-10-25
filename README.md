# Revenant

Revenant is a free, open-source app for Mac and Windows that recovers photos and videos from SD cards and external drives,
including ones that were corrupted, formatted or overwritten by mistake. It aims at paid-tool recovery quality, with a clear
preview of what can be recovered before you commit, and it never writes to the card or drive it reads from.

## Status

Under construction. There is no app to download yet. The recovery engine (`revenant-core`) can already find JPEG photos,
MP4/MOV videos and camera RAW photos (CR2, CR3, NEF, ARW, RAF, DNG, ORF, RW2) and HEIC in a disk image (including
formatted cards), classify them as intact, partial or damaged, read their photo details (camera, lens, ISO, capture
time) and embedded preview, and recover them to another folder. It puts back together a JPEG photo split in two pieces
on the card and a camera video whose index (`moov`) ended up away from its frames. It reads each sector of the card once
(putting a broken photo or video back together may read parts of it again: up to a 64 MiB window per photo), never
retries a known unreadable sector, and checks the files it finds on every core: on an 8-core / 16-thread Intel Core
i9-9980HK, with the default threads, it scans a photo-dense 1 GiB image at about 500 MB/s (JPEG) to 1,600 MB/s (RAW)
from the page cache, against 22 and 158 MB/s before, faster than an SD card reader delivers (details in
[crates/revenant-core](crates/revenant-core/README.md#performance)). It reads FAT12/16/32 and exFAT file systems (on MBR and GPT
disks, and after a quick format through the old folders) to bring back deleted photos and videos with their original names, folders
and dates, tells a file the file system lists apart from the same photo found by its signature (each photo comes out
once), gives files without a name a readable one (`2025-12-14_07-12-05_ILCE-7M4.JPG`), and groups the results by
session, day, camera and folder. It is tested against real camera files as well as synthetic ones. More engine work
comes next (reading cards directly), then a graphical app.

## Trying the engine (developers)

```sh
cargo build --release -p revenant-dev
target/release/revenant-dev scan card.img --extract ~/recovered
target/release/revenant-dev disks                               # the connected disks
sudo target/release/revenant-dev scan disk4 --extract ~/recovered   # a card read directly (macOS)
```

This prints one JSON line per finding, with its photo details and preview. It reads a disk image file or a whole card
directly, read-only (as root on macOS, from a terminal run as administrator on Windows: `revenant-dev scan
PhysicalDrive2`); do not open the card in Finder or Explorer meanwhile. It can also copy a failing card into an image
with a rescue map (`revenant-dev copy disk4 card.img`) and save a scan to resume it later (`--session`). See
[crates/revenant-dev](crates/revenant-dev/README.md) for the output format, reading a card directly, copying a card and
saved scans.

## License

[GPL-3.0-only](LICENSE), copyright Sérgio Mafra.

## Contributing

Issues and pull requests are welcome. Everything in the repository (code, comments, commits, docs) is in English. Before
opening a pull request, run `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test --workspace`.
Dependencies must be under permissive licenses (see `deny.toml`).
