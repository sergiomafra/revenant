# revenant-dev

Developer command line for the Revenant engine (`revenant-core`). It is a tool for developers and testers, **not** the
end-user app (that comes later, with a graphical interface). It reads its source read-only: a disk image file, or a
whole card or disk read directly.

## Build

```sh
cargo build --release -p revenant-dev
# binary: target/release/revenant-dev
```

## Usage

```sh
revenant-dev scan <SOURCE> [--extract <DIR>] [--previews <DIR>] [--session <DIR>] [--allow-same-volume] [--no-unmount] [--threads N] [--group session|day|camera|folder]
revenant-dev disks
revenant-dev preflight <DISK> [--copy-to <IMAGE>] [--session <DIR>] [--no-unmount]
revenant-dev unmount <DISK>
revenant-dev remount <DISK>
revenant-dev copy <SOURCE> <IMAGE> [--retries N] [--no-unmount] [--allow-same-volume]
revenant-dev digest <SOURCE>
```

Every command prints JSON lines on stdout, progress and hints on stderr. A fatal error prints `error: <code>` on stderr
and exits with 1; `preflight` and `copy` exit with 2 when the pre-flight check found issues.

- `<SOURCE>`: a disk image file, or a whole disk: `disk4` (also `/dev/disk4`, `/dev/rdisk4`) on macOS, `PhysicalDrive2`
  (also `\\.\PhysicalDrive2`) on Windows (see [Reading a card directly](#reading-a-card-directly)). A whole-disk name
  always means the disk (write `./disk4` for a file of that name); a partition (`disk4s1`) is refused with
  `not_whole_disk`. An image file with its rescue map beside it (`<SOURCE>.rvmap`, written by `copy`) is read through
  the map (see [Copying a card](#copying-a-card)).
- `--extract <DIR>`: copies each finding into `<DIR>` as soon as it is found, under its original name when the file
  system listed one, else `YYYY-MM-DD_HH-MM-SS[_<camera>].<EXT>` from its details, else `revenant_<start as 12 hex digits>.<ext>`
  (`_2`, `_3`… if the name exists; see "Names" in the core README). `<DIR>` must already exist and be a folder;
  otherwise the command stops before scanning with `error: open_failed` (exit 1). The folder is checked once, before
  scanning, against the source: never on an image file's own volume, and, for a disk, never on any volume of that disk
  (`destination_on_source`, exit 1) nor on a folder whose disks the system cannot tell (`destination_unknown`), so use
  a folder on another disk. Every file is then written only into that checked folder.
- `--previews <DIR>`: writes each finding's embedded preview JPEG (the one in the `preview` field) into `<DIR>` as
  `<stem of the file's name>.preview.jpg` (`DSC01235.ARW` gives `DSC01235.preview.jpg`; `_2`, `_3`… if the name exists; never overwrites), the same naming as
  `--extract`, so a file and its preview sit side by side. Same folder rules as `--extract`: `<DIR>` must exist
  (`open_failed` otherwise) and must not be on the source's volume (`destination_on_source`, exit 1, checked before
  scanning).
- `--session <DIR>`: saves the scan into `<DIR>` (created when absent), or resumes the scan saved there; see
  [Saved scans](#saved-scans). Same folder rules as `--extract`.
- `--allow-same-volume`: **developer only: use when scanning an image FILE, never a card.** Treats the source's volume as
  unknown, so extraction, previews and the session onto the same volume are allowed. Tests use it because the image and
  the output folder live in the same temporary directory. Refused for a disk (`destination_on_source`).
- `--no-unmount`: reads a disk with its volumes mounted (by default they are unmounted first; see below).
- `--threads N`: threads that measure candidate files (default 0 = one per core, at most 16; 1 = measure on the scan
  thread; capped at 64). The output is identical for any value.
- `--group session|day|camera|folder`: after the last `found`/`updated` line and before `summary`, prints one line per
  group of the results (see "Grouping" in the core README), using each file's latest finding and details:
  `{"event":"group","by":"session","label":{"kind":"session","camera":{"make":"Sony","model":"ILCE-7M4","serial":"111"},"first":"2025-12-14T08:00:00","last":"2025-12-14T08:30:00"},"ids":[1,2]}`.
  `label.kind` is the variant name in snake_case (`session`, `undated_session`, `day`, `undated`, `camera`,
  `unknown_camera`, `folder`, `no_folder`) with its fields: `session` has `camera`, `first`, `last`; `undated_session`
  has `camera`; `day` has `day` (`YYYY-MM-DD`); `camera` has `camera`; `folder` has `folder`. A `camera` object has `make`,
  `model`, `serial` (each left out when unknown), and the `camera` key itself is left out when the camera is unknown. `--group folder` on a
  scan where no file has a folder prints one line, `{"event":"group","by":"folder","available":false}`.

### Output

stdout is pure JSON, one object per line; progress goes to stderr as human-readable text.

```json
{"event":"found","id":1,"start":0,"len":12345,"kind":"jpeg","health":"intact","width":64,"height":48,"preview":{"start":0,"len":12345,"width":64,"height":48}}
{"event":"unreadable","start":1044480,"len":4096}
{"event":"found","id":2,"start":1049088,"len":13507425,"kind":"nef","health":"intact","make":"NIKON CORPORATION","model":"NIKON Z 6","lens":"NIKKOR Z 24-70mm f/4 S","iso":100,"captured_at":"2019-11-29T17:15:20","width":4548,"height":3030,"preview":{"start":3215360,"len":432488,"width":3024,"height":2016}}
{"event":"summary","found":2,"unreadable_bytes":4096,"updated":0,"listing_truncated":false}
```

- When the image holds FAT12/16/32 or exFAT volumes, a `volume` line per volume comes first (before any other line), in
  offset order; images with no volume print none:

  ```json
  {"event":"volume","index":0,"fs":"fat32","scheme":"mbr","offset":4194304,"len":50331648,"sector_size":512,"cluster_size":512,"heap_offset":4290048,"cluster_count":98000,"label":"REVENANT","serial":305419896,"from_backup":false,"listing_truncated":false}
  ```

  `fs`: `fat12`, `fat16`, `fat32` or `exfat`; `scheme`: `none`, `mbr` or `gpt`; `offset`, `len` and `heap_offset` are
  absolute bytes in the image; `label` is omitted when the volume has none; `from_backup` is true when the backup boot
  sector had to be used. `listing_truncated` is true when a limit of the file-system listing dropped or degraded some
  files (some may lack their name, folder or dates); it is the same on every `volume` line and in the summary.
- A `found` (or `updated`) line for a file the file system lists also carries where it came from: `name`, `path`
  (`folder/name`, root-relative, `/`-separated; the name alone when the folder is unknown), `state` (`live`, `deleted` or
  `orphaned`), `placement` (`chain`, `recorded`, `guessed` or `carved`: how its clusters were found), `volume` (the
  `index` of its `volume` line), `fs_size` (the size the directory entry records) and, when the entry has them,
  `created` and `modified` (`YYYY-MM-DDTHH:MM:SS` as recorded, with `+HH:MM`/`-HH:MM` when the volume stores the UTC
  offset):

  ```json
  {"event":"found","id":1,"start":4296192,"len":2612,"kind":"jpeg","health":"intact","name":"DSC01234.JPG","path":"DCIM/100MSDCF/DSC01234.JPG","state":"live","placement":"chain","volume":0,"fs_size":2612,"modified":"2025-12-14T07:12:06","width":64,"height":48}
  ```

  Files found by the byte scan alone carry none of these keys, and lines without them are exactly as before.
- A card with many unreadable sectors early on gets one `disk_failing` line, right after the scan reaches the first block
  where the `unreadable` lines so far add up to 16 runs or 4 MiB (`read`: bytes scanned at that point). It means: stop
  and make an image of the card first (see "Making a card image" below), then scan the image.

  ```json
  {"event":"disk_failing","unreadable_runs":16,"unreadable_bytes":8192,"read":5242880}
  ```
- `kind`: `jpeg`, `mp4`, `mov`, `cr2`, `nef`, `arw`, `dng`, `orf`, `rw2`, `raf`, `cr3`, `heic` or `tiff`.
  `health`: `intact`, `partial` or `damaged`.
- `health` follows the engine: a file containing unreadable sectors is never `intact`. Files inside a `partial` or
  `damaged` finding are reported as findings of their own, so their ranges may overlap.
- Photo details, each present only when the file holds a valid value: `make`, `model`, `lens`, `serial`, `iso`,
  `captured_at` (`YYYY-MM-DDTHH:MM:SS`, camera-local time; for MP4/MOV the UTC creation time), `width`, `height`.
  `preview` is the embedded JPEG the results screen shows: its absolute range in the image (`start`, `len`) and its size
  (`width`, `height`); absent when there is none (MP4/MOV, HEIC).
- `fragments`: present only when the file was found in pieces: `[{"start":…,"len":…},…]`, the absolute ranges in file
  order (the first starts at `start`, the lengths sum to `len`). `preview` gets the same key under the same rule, and
  `--previews` writes all of a preview's pieces in order. Contiguous files have no `fragments` key. A JPEG put back
  together from two pieces, with a small photo found in the gap:

  ```json
  {"event":"found","id":1,"start":65536,"len":71816,"kind":"jpeg","health":"intact","fragments":[{"start":65536,"len":36352},{"start":3145728,"len":35464}],"width":1024,"height":768,"preview":{"start":65536,"len":71816,"width":1024,"height":768,"fragments":[{"start":65536,"len":36352},{"start":3145728,"len":35464}]}}
  {"event":"found","id":2,"start":101888,"len":8250,"kind":"jpeg","health":"intact","width":64,"height":64,"preview":{"start":101888,"len":8250,"width":64,"height":64}}
  ```
- `updated`: a better version of an earlier `found` line with the same `id`, with the same fields; it replaces that
  line. These lines come after every `found` line, just before the summary, whose `updated` counts them, one per `id`
  in `id` order. Today: a file found by its signature on a quick-formatted card that an old folder names (the same line
  with its `name`, `path` such as `?/100MSDCF/DSC01234.JPG`, `state` `orphaned` and `placement` `carved`), and a
  camera video whose index (`moov`) ended up elsewhere on the card, found `damaged` and then put back together from its
  two pieces (both in one line when both apply):

  ```json
  {"event":"found","id":1,"start":1048576,"len":55090,"kind":"mp4","health":"damaged"}
  {"event":"updated","id":1,"start":1048576,"len":56144,"kind":"mp4","health":"intact","fragments":[{"start":1048576,"len":22528},{"start":12582912,"len":33616}]}
  ```

  With `--extract` and `--previews`, an updated finding is written again: same naming, so it gets the next free `_N`
  suffix (for example `revenant_000000100000_2.mp4` next to the damaged `revenant_000000100000.mp4`).
- With `--extract`, a `found` line gets `"file":"<path>"`. If extracting that one finding fails (for example
  `write_failed` when the folder is not writable), it gets `"error":"<code>"` instead and the scan continues.
- With `--previews`, a `found` line that has a `preview` gets `"preview_file":"<path>"`, or `"preview_error":"<code>"`
  (`read_failed`, `write_failed`) if writing that preview failed; the scan continues.
- Fatal errors print `error: <code>` to stderr and exit with 1 (for example `open_failed`, `destination_on_source`).
- Reading a disk: an `unmount` line comes first, `{"event":"unmount","disk":"disk4","held":0}` (`held`: the volume locks
  kept while reading, always 0 on macOS), or, when the unmount failed,
  `{"event":"unmount","disk":"disk4","error":"unmount_failed","volumes":["disk4s1"]}` with the volumes still mounted; the
  scan then goes on with them mounted (the system may write to them). stderr ends with how to remount.
- Reading an image with its rescue map: a first line `{"event":"map","rescued":15990784,"unrescued":786432}` (bytes);
  every byte the copy did not rescue reads as unreadable (an `unreadable` line), never as zeros, and no file touching
  it is `intact`.
- With `--session`, the lines a resumed scan replays from the saved scan carry `"replayed":true` (and no `file` or
  `preview_file`: they were written by the run that found them); the summary and `group` lines never do.
- The progress line on stderr also shows the reading speed and the time left: `scanned 42% (… of … bytes), 85.3 MB/s,
  ETA 3m12s`.

Other commands:

- `disks`: one line per connected whole disk (sorted by id); `vendor`, `model`, `serial` and a volume's `fs`, `label` and
  `mount_point` are left out when the system does not know them, `solid_state` is `null` when it does not say:

  ```json
  {"event":"disk","id":"disk4","size":63864569856,"logical_sector":512,"physical_sector":512,"vendor":"Generic","model":"SD Card Reader","serial":"000000000820","removable":true,"internal":false,"system":false,"virtual":false,"solid_state":null,"volumes":[{"id":"disk4s1","fs":"exfat","label":"CARD","mount_point":"/Volumes/CARD","size":63847792640}]}
  ```
- `preflight <DISK> [--copy-to <IMAGE>] [--session <DIR>] [--no-unmount]`: the pre-flight check (see "Pre-flight check"
  in the core README): permission, the disk answering (first, middle and last sector, 10 s each at most), the unmount
  (unless `--no-unmount`; the disk stays unmounted), the copy's destination (`--copy-to` is the image FILE: its folder is
  checked, its map is `<IMAGE>.rvmap`) and a session to resume. One line; exit 2 when `issues` is not empty:

  ```json
  {"event":"preflight","disk":"disk4","issues":[{"code":"destination_too_small","need":63864569856,"free":10737418240}]}
  ```

  Issue codes: `permission_denied`, `device_not_found`, `system_disk`, `open_failed`, `device_not_responding` and
  `sample_unreadable` (with `offset`), `unmount_failed` (with `volumes`), `destination_too_small` (`need`, `free`),
  `destination_on_source`, `destination_unknown`, `destination_file_too_large` (`limit`), `session_other_source`,
  `session_corrupt`.
- `unmount <DISK>`: `{"event":"unmount","disk":"disk4","held":0}`; `remount <DISK>`: `{"event":"remount","disk":"disk4"}`.
- `copy <SOURCE> <IMAGE>`: `copy_progress` lines (one each time the pass or the percent of `pos` changes), a
  `copy_bad` line per region marked bad, then the summary (bytes in each state):

  ```json
  {"event":"copy_progress","pass":"fast","pos":1048576,"untried":63863521280,"rescued":1048576,"to_trim":0,"to_scrape":0,"bad":0,"bytes_per_sec":41943040,"eta_secs":1523}
  {"event":"copy_bad","start":52428800,"len":512}
  {"event":"copy_summary","untried":0,"rescued":63864569344,"to_trim":0,"to_scrape":0,"bad":512,"complete":true}
  ```

  `pass`: `fast`, `skipped`, `trim`, `scrape`, `retry` (with `"retry":N`) or `done`; `eta_secs` is left out until known.
- `digest <SOURCE>`: `{"event":"digest","bytes":63864569856,"fnv64":"8f3c2a91d04e5b67","unreadable_bytes":0}`: the
  FNV-1a 64 of every byte (16 hex digits), sectors that cannot be read hashed as zeros and counted. A file is read
  plainly (a map beside it is not used), a disk directly (not unmounted). Compare a card with its copy: equal digests
  and no unreadable bytes mean an identical copy.

## Benchmark

```sh
cargo run --release -p revenant-dev -- bench [--size-mib N=1024] [--runs N=3] [--images a,b,..=all] [--dir DIR=temp] [--threads N=0] [--session]
```

Writes each synthetic image into a fresh temp dir under `--dir`, scans it `--runs` times and prints one JSON line per image
(`image`, `bytes`, `placed` = files put in the image, `found` = findings of the best run, `threads` = the measuring
threads actually used (`--threads 0` resolves to one per core, at most 16), `runs`, `session`, `best_seconds`,
`mb_per_s` with MB = 10^6 bytes). With `--session`, each image is then scanned `--runs` times more through a saved
session (a new one each time, in a fresh folder of the temp dir, default checkpoint cadence: every 256 MiB or 30 s) and
a second line with `"session":true` follows the plain one (`"session":false`); its time includes creating the session.
The temp files are removed at the end. Always measure with a release build.

Images (files go back to back, each at the next 512-byte boundary, the rest is seeded noise):

- `noise`: no files; the cost of reading and scanning bytes that match nothing.
- `dense-jpeg`: 8 distinct camera-like JPEGs cycled (3000x2000 from 512 MiB up, else 320x240).
- `dense-raw`: 4 TIFF-based RAWs (ARW, NEF, CR2, DNG) with a JPEG preview, cycled.
- `video`: MP4 files with `moov` at the end (256 MiB `mdat` from 512 MiB up, else 8 MiB).
- `exfat-card`: an exFAT volume (4 KiB clusters) on an MBR disk (first sector 8192) that fills the image, holding the
  `dense-jpeg` photos as files in `DCIM/100TEST`, every 4th one deleted; `placed` counts them all and `found` should
  equal it. It measures the scan with the file-system listing. It is built in memory, so its peak memory is about
  twice the image size (about 2 GiB at the default `--size-mib`): the builder keeps a copy of every file and `build()`
  allocates the image.
- `cut-jpeg`: like `dense-jpeg` but the second half of every file is noise. It has no speed target (each cut JPEG starts
  a gap search that may re-read its 64 MiB window); it is only measured and reported.

The numbers are page-cache throughput (the first write leaves the image in the cache): an upper bound for a card reader,
not a prediction of it. The measured numbers (before and after part 1.3, and the machine) are in the engine's README,
[crates/revenant-core](../revenant-core/README.md#performance) ("Performance").

Compare only runs of the same `--size-mib`; the baseline is 1024 MiB. `--runs 1` may under-report, because the first run
can overlap the writeback of the image. The `dense-jpeg` images are noise-encoded (about 11 MB per 3000x2000 file, more
entropy than real photos), so cross-check final speed claims with the real samples.

## Reading a card directly

`scan`, `copy` and `digest` read a whole card or disk directly, read-only, which needs root on macOS and Administrator
on Windows:

```sh
revenant-dev disks                                   # find the card: removable, the right size, not "system"
sudo revenant-dev scan disk4 --extract ~/recovered   # macOS
```

On Windows, open a terminal with "Run as administrator", then `revenant-dev disks` and
`revenant-dev scan PhysicalDrive2 --extract D:\recovered`.

- **Do not open the card in Finder or Explorer, and stop using it** (no camera, no copying files) until the recovery is
  over: every write to it can overwrite a deleted photo.
- Before reading, the card's volumes are **unmounted, not ejected**, so the system stops writing to it while it stays
  connected (`unmount` line; `--no-unmount` reads it mounted). Revenant never mounts it again by itself: run
  `revenant-dev remount disk4` when done (stderr says so). On Windows the volumes are locked while the command runs and
  Windows may mount them again once it ends.
- The disk holding the running system is refused (`system_disk`). `--extract`, `--previews` and `--session` are checked
  once, before anything is read, against the whole card: a folder on any of its volumes is refused
  (`destination_on_source`), and so is one whose disks the system cannot tell (`destination_unknown`, a network folder
  for example). `--allow-same-volume` is refused for a disk.
- A bad sector is read once, at the card's physical sector size. Many unreadable sectors early on give a
  `disk_failing` line: stop and copy the card first ([Copying a card](#copying-a-card)).
- If the card is disconnected (a loose cable, the computer sleeping) during a scan with `--session`, stderr asks to plug
  it back in; the scan waits (checking every half second), finds the same card again even under another disk name
  (another card is never taken for it), unmounts it again and goes on in the same session. The system may write to the
  card in the short window between plugging it in and that unmount. Without `--session` the scan stops with
  `error: device_gone`.

## Copying a card

A failing card should be copied once, then scanned from the copy. `copy` writes an image file and its rescue map
(`<IMAGE>.rvmap`): the readable parts first, the damaged areas skipped and come back to later, sector by sector, each bad
sector read once.

```sh
sudo revenant-dev preflight disk4 --copy-to /Volumes/Backup/card.img   # optional: everything known to be wrong, at once
sudo revenant-dev copy disk4 /Volumes/Backup/card.img                   # the image's folder must not be on the card
revenant-dev scan /Volumes/Backup/card.img --extract ~/recovered          # read through its map
revenant-dev remount disk4                                                # when done with the card
```

- `copy` with a disk runs the pre-flight check first (unmounting unless `--no-unmount`) and, if it finds any issue (no
  permission, a card that does not answer, a destination on the card, without room for the whole card, or on FAT32 for
  a card over 4 GiB), prints the `preflight` line and exits with 2 before writing anything.
- Interrupted (a crash, Ctrl-C, the card pulled out), the same command resumes from the map; pulling the card out never
  marks sectors bad. A map made from another card is refused (`map_other_source`).
- `--retries N` reads each bad sector up to N more times at the end (default 0, so a dying card is not hammered); it
  can be asked on a later run of the same copy.
- Scanning the image: with its map beside it, what the copy did not rescue is unreadable, never zeros (`map` line, see
  Output); without the map, those zeros would look like data.
- `copy` of an image file works the same (a copy of a copy, or tests); `--allow-same-volume` lets the new image go onto
  the source image's volume.
- `revenant-dev digest disk4` and `revenant-dev digest card.img` compare the copy with the card.

## Saved scans

`--session <DIR>` saves the scan into a folder of its own (created when absent; the same rules as `--extract`, so never
on the card), so it can be taken up again after Ctrl-C, a crash, a pulled cable or a power loss:

```sh
sudo revenant-dev scan disk4 --session ~/scans/card1 --extract ~/recovered
# … interrupted; later, the same command:
sudo revenant-dev scan disk4 --session ~/scans/card1 --extract ~/recovered
```

The resumed run first prints the lines saved so far, each with `"replayed":true` (not extracted again), then scans on
from the last checkpoint (every 256 MiB or 30 s): the whole output is the uninterrupted scan's. Running it on a finished
scan only prints its results again. A session made from another card (or from a copy whose image changed) is refused
(`session_other_source`). Sessions also work for image files (`revenant-dev scan card.img --session …`).

## Making a card image (read-only)

Work on an image of the card, not on the card itself, when the card is failing or will be scanned more than once. Make it
with `revenant-dev copy` ([Copying a card](#copying-a-card)): it only reads from the card, copies the readable areas first
and the areas around bad sectors at the end, reads each bad sector once, and keeps a map, so an interrupted copy resumes
where it stopped and a scan of the image knows which parts were never rescued. External tools such as `dd` or GNU
ddrescue are no longer needed (and `dd bs=4m conv=noerror,sync` replaces the whole block around each bad sector with
zeros, which then look like data).

## Checking against real file systems (macOS, opt-in)

The synthetic FAT12/16/32 and exFAT volumes in `revenant-testkit` are checked against the real macOS drivers, so the
builders cannot drift from what cameras and operating systems write:

```sh
REVENANT_OS_FS=1 cargo test -p revenant-testkit --test os_builders -- --ignored
```

For each file system the test builds a volume with a `DCIM/100TEST` folder (a plain name, a long name, a fragmented file,
a deleted file, a stamped modification time), places it on an MBR disk image in a temp directory, attaches it
read-only with `hdiutil`, and compares the names, the file bytes and the modification time as the OS reads them. It also
runs `fsck_msdos -n` / `fsck_exfat -n` on the attached partition. A second test formats an image with the OS itself
(`hdiutil create`) to check the helper. Without `REVENANT_OS_FS=1`, off macOS, or without `/usr/bin/hdiutil`, the tests
print `skipped: <reason>` and pass.

It only ever attaches image files it created in a temp directory (`-nobrowse -noverify -noautofsck`, read-only except
for the helper's own OS-formatted image), detaches only the device `hdiutil attach` printed for them (also on panic),
and never calls `diskutil`, `sudo` or any real disk, so the "no real disks" rule still holds.

Another opt-in test, in the engine, formats images with the OS (FAT32 and exFAT on MBR, exFAT on GPT, FAT16 without a
table), writes photos, a RAW, a video and a few bigger files through the mount (deleting some), and scans the result end
to end:

```sh
REVENANT_OS_FS=1 cargo test -p revenant-core --test os_filesystems -- --ignored --test-threads=1
```

It checks that every live file is found once with its path, `Intact` and extracting to the bytes written, and that each
deleted photo whose clusters were not reused is found `Deleted` and `Intact`. Run it with `--test-threads=1`: every image's
volume is named `RVT`, so two mounted at once would clash.

## Real camera samples

`samples/samples.toml` lists real camera files (one each of CR2, CR3, NEF, ARW, RAF, DNG, ORF, RW2; about 84 MB, all CC0
from [raw.pixls.us](https://raw.pixls.us/), with URL, SHA-256 and license line). They are test data, never committed:

```sh
scripts/fetch-samples.sh   # downloads into samples/cache/ (gitignored) and checks every SHA-256
REVENANT_SAMPLES=samples/cache cargo test -p revenant-core --test real_samples -- --ignored --nocapture
target/release/revenant-dev scan samples/cache/nikon-z6.nef --previews /tmp/previews --allow-same-volume
```

The script only downloads (over https only, redirects included) and checks files; nothing in them is ever executed.
Known limits the samples do not cover: the old Olympus `OLYMP\0` MakerNote is not read, and the Sony DSLR-A100 ARW comes
out `damaged` (see `crates/revenant-core/README.md`).
