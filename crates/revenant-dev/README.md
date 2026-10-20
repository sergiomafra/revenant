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
{"event":"summary","found":2,"unreadable_bytes":4096}
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
  `--previews` writes all of a preview's pieces in order. Contiguous files (every file today) have no `fragments` key.
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
- `cut-jpeg`: like `dense-jpeg` but the second half of every file is noise. It has no speed target (each cut JPEG starts
  a gap search that may re-read its 64 MiB window); it is only measured and reported.

The numbers are page-cache throughput (the first write leaves the image in the cache): an upper bound for a card reader,
not a prediction of it.

Compare only runs of the same `--size-mib`; the baseline is 1024 MiB. `--runs 1` may under-report, because the first run
can overlap the writeback of the image. The `dense-jpeg` images are noise-encoded (about 11 MB per 3000x2000 file, more
entropy than real photos), so cross-check final speed claims with the real samples.

## Making a card image (read-only)

Work on an image of the card, not on the card itself. Imaging only reads from the card.

macOS:

**Double-check the device name with `diskutil list` before running `dd`, and never swap `if` and `of`: `of=` is written to.**

```sh
diskutil list                          # find the card: /dev/diskN
diskutil unmountDisk /dev/diskN        # unmount without ejecting
sudo dd if=/dev/rdiskN of=card.img bs=4m conv=noerror,sync
```

Replace `N` with the card's number. `card.img` must be on a different disk than the card, with enough free space.
`conv=noerror,sync` keeps `dd` going past unreadable sectors (plain `dd` stops at the first one), padding them with zeros so
every later byte stays at its offset.

Windows: use a read-only imaging tool (for example FTK Imager, which can create a raw image of a physical drive) rather than
`dd`, and save the image to a different drive than the card.

Then:

```sh
revenant-dev scan card.img --extract ~/recovered --allow-same-volume   # only if ~/recovered is on the same disk as card.img
```

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
