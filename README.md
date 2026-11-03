# Revenant

Revenant is a free, open-source app for Mac and Windows that recovers photos and videos from SD cards and external drives,
including ones that were corrupted, formatted or overwritten by mistake. It aims at paid-tool recovery quality, with a clear
preview of what can be recovered before you commit, and it never writes to the card or drive it reads from.

## Status

Under construction. There is no app to download yet. The recovery engine (`revenant-core`) can already find JPEG photos,
MP4/MOV videos and camera RAW photos (CR2, CR3, NEF, ARW, RAF, DNG, ORF, RW2) and HEIC in a disk image (including
formatted cards), classify them as intact, partial or damaged, read their photo details (camera, lens, ISO, capture
time) and embedded preview, and recover them to another folder. It puts back together photos (JPEG), RAW photos and
camera videos (MP4/MOV, H.264 and H.265, with the index at the end, at the start or in fragments) split into pieces on
the card, around other files or deleted and partly shot over, from the card's free space. A file in pieces comes back
**intact** only with proof: the file system gives its place (a live file's cluster chain, or a deleted exFAT file's
surviving chain that nothing on the card contradicts), a video's frames check out across every cut, a lossless RAW is
cut exactly between its image blocks, or a JPEG in two pieces passes the two-piece rule. Otherwise it comes back
**partial**, its bytes in the most likely place: a JPEG in more pieces placed by its content alone (a near-identical
photo of the same burst could have lent a piece), a RAW cut inside its image data or its preview, a video whose cuts
cannot be checked. A JPEG without restart markers in three or more pieces is often not put back at all, and a piece
more than 256 MiB away is not looked for. It reads each sector of the card once (putting a file in pieces back
together reads parts of the card again, within a budget: 512 MiB per file, 16 GiB per scan), never
retries a known unreadable sector, and checks the files it finds on every core: on an 8-core / 16-thread Intel Core
i9-9980HK, with the default threads, it scans a photo-dense 1 GiB image at about 500 MB/s (JPEG) to 1,600 MB/s (RAW)
from the page cache, against 22 and 158 MB/s before, faster than an SD card reader delivers (details in
[crates/revenant-core](crates/revenant-core/README.md#performance)). It reads FAT12/16/32 and exFAT file systems (on MBR and GPT
disks, and after a quick format through the old folders) to bring back deleted photos and videos with their original names, folders
and dates, tells a file the file system lists apart from the same photo found by its signature (each photo comes out
once), gives files without a name a readable one (`2025-12-14_07-12-05_ILCE-7M4.JPG`), and groups the results by
session, day, camera and folder. It also reads a card or disk directly, on Mac and Windows, read-only: it lists the
connected disks, refuses the one holding the system, unmounts the card without ejecting it, checks everything it already
knows to be wrong before a long read (permission, a disk that does not answer, room for a copy), copies a failing card into
an image with a rescue map that tolerates bad sectors and can be resumed, warns early when a card is failing, and saves a
scan so it resumes with the same results after a crash or a pulled cable. It is tested against real camera files as well
as synthetic ones and, optionally, against disk images attached to the computer. The graphical app has started: it opens on the list of connected cards and disks, asks for the system's permission to
read the one you choose (read-only; the search itself comes next), and has Settings (theme and language: Português,
Español, English), About and an error screen (Tauri 2 and React, in [crates/revenant-app](crates/revenant-app/README.md)
and `app/`); its logo and app icon are in [assets/brand](assets/brand/README.md).

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

## Running the app (developers)

```sh
cd app
npm ci
npm run tauri dev                       # the window, with live reload
npm run tauri build -- --bundles app    # target/release/bundle/macos/Revenant.app
```

See [crates/revenant-app](crates/revenant-app/README.md).

## License

[GPL-3.0-only](LICENSE), copyright Sérgio Mafra.

## Contributing

Issues and pull requests are welcome. Everything in the repository (code, comments, commits, docs) is in English. Before
opening a pull request, run `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test --workspace`.
Dependencies that ship inside the app must be under permissive licenses, or MPL-2.0 if unmodified (see `deny.toml`;
`node scripts/check-npm-licenses.mjs app` checks the npm side). Build-only npm tools may use any open license except
GPL and AGPL.
