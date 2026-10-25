# revenant-dev

Developer command line for the Revenant engine (`revenant-core`). It is a tool for developers and testers, **not** the
end-user app (that comes later, with a graphical interface). It reads its source read-only.

## Build

```sh
cargo build --release -p revenant-dev
# binary: target/release/revenant-dev
```

## Usage

```sh
revenant-dev scan <IMAGE> [--extract <DIR>] [--previews <DIR>] [--allow-same-volume] [--threads N]
```

- `<IMAGE>`: a disk image file (scanning devices directly comes in a later part).
- `--extract <DIR>`: copies each finding into `<DIR>` as soon as it is found. `<DIR>` must already exist and be a folder;
  otherwise the command stops before scanning with `error: open_failed` (exit 1). The engine refuses to extract onto the
  source's own volume (error `destination_on_source`, exit 1), so use a folder on another disk.
- `--previews <DIR>`: writes each finding's embedded preview JPEG (the one in the `preview` field) into `<DIR>` as
  `revenant_<start as 12 hex digits>.preview.jpg` (`_2`, `_3`… if the name exists; never overwrites), the same naming as
  `--extract`, so a file and its preview sit side by side. Same folder rules as `--extract`: `<DIR>` must exist
  (`open_failed` otherwise) and must not be on the source's volume (`destination_on_source`, exit 1, checked before
  scanning).
- `--allow-same-volume`: **developer only: use when scanning an image FILE, never a card.** Treats the source's volume as
  unknown, so extraction (and previews) onto the same volume is allowed. Tests use it because the image and the output folder live in the
  same temporary directory.
- `--threads N`: threads that measure candidate files (default 0 = one per core, at most 16; 1 = measure on the scan
  thread; capped at 64). The output is identical for any value.

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
  line. These lines come after every `found` line, just before the summary, whose `updated` counts them. Today: a
  camera video whose index (`moov`) ended up elsewhere on the card, found `damaged` and then put back together from its
  two pieces:

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

## Benchmark

```sh
cargo run --release -p revenant-dev -- bench [--size-mib N=1024] [--runs N=3] [--images a,b,..=all] [--dir DIR=temp] [--threads N=0]
```

Writes each synthetic image into a fresh temp dir under `--dir`, scans it `--runs` times and prints one JSON line per image
(`image`, `bytes`, `placed` = files put in the image, `found` = findings of the best run, `threads` = the measuring
threads actually used (`--threads 0` resolves to one per core, at most 16), `runs`, `best_seconds`, `mb_per_s` with
MB = 10^6 bytes). The temp files are removed at the end. Always measure with a release build.

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

## Making a card image (read-only)

Work on an image of the card, not on the card itself. Imaging only reads from the card. Revenant cannot make the image
yet (its own bad-sector-tolerant copy comes in a later part); until then we recommend
[GNU ddrescue](https://www.gnu.org/software/ddrescue/), an external tool (GPL, installed separately, not part of
Revenant). It copies the easily readable areas first and comes back to the areas around bad sectors at the end, leaves
only the sectors it could not read as zeros (so every other byte stays at its offset), and keeps a map file, so an
interrupted copy resumes where it stopped. Do not use `dd bs=4m conv=noerror,sync`: it replaces the whole 4 MiB block
around each bad sector with zeros.

macOS:

**Double-check the device name with `diskutil list` before running `ddrescue`, and never swap the two paths: the second
one is written to.**

```sh
diskutil list                          # find the card: /dev/diskN
diskutil unmountDisk /dev/diskN        # unmount without ejecting
sudo ddrescue /dev/rdiskN card.img card.map
```

Replace `N` with the card's number. `card.img` (and `card.map`) must be on a different disk than the card, with enough
free space. `/dev/rdiskN` is the raw device, which already bypasses the system's cache. On Linux, the same copy is
`sudo ddrescue -d /dev/sdX card.img card.map` (`-d`: direct disc access, bypassing the cache). Running the same command
again with the same map file resumes an interrupted copy; adding `-r3` retries the unreadable sectors up to three more
times.

Windows: use a read-only imaging tool (for example FTK Imager, which can create a raw image of a physical drive) rather than
`dd`, and save the image to a different drive than the card.

Then:

```sh
revenant-dev scan card.img --extract ~/recovered --allow-same-volume   # only if ~/recovered is on the same disk as card.img
```

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
