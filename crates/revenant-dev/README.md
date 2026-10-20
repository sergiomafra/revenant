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
revenant-dev scan <IMAGE> [--extract <DIR>] [--allow-same-volume]
```

- `<IMAGE>`: a disk image file (scanning devices directly comes in a later part).
- `--extract <DIR>`: copies each finding into `<DIR>` as soon as it is found. `<DIR>` must already exist and be a folder;
  otherwise the command stops before scanning with `error: open_failed` (exit 1). The engine refuses to extract onto the
  source's own volume (error `destination_on_source`, exit 1), so use a folder on another disk.
- `--allow-same-volume`: **developer only: use when scanning an image FILE, never a card.** Treats the source's volume as
  unknown, so extraction onto the same volume is allowed. Tests use it because the image and the output folder live in the
  same temporary directory.

### Output

stdout is pure JSON, one object per line; progress goes to stderr as human-readable text.

```json
{"event":"found","id":1,"start":0,"len":12345,"kind":"jpeg","health":"intact"}
{"event":"unreadable","start":1048576,"len":4096}
{"event":"summary","found":1,"unreadable_bytes":4096}
```

- `kind`: `jpeg`, `mp4`, `mov`, `cr2`, `nef`, `arw`, `dng`, `orf`, `rw2`, `raf`, `cr3`, `heic` or `tiff`. `health`: `intact`, `partial` or `damaged`.
- `health` follows the engine: a file containing unreadable sectors is never `intact`. Files inside a `partial` or
  `damaged` finding are reported as findings of their own, so their ranges may overlap.
- With `--extract`, a `found` line gets `"file":"<path>"`. If extracting that one finding fails (for example
  `write_failed` when the folder is not writable), it gets `"error":"<code>"` instead and the scan continues.
- Fatal errors print `error: <code>` to stderr and exit with 1 (for example `open_failed`, `destination_on_source`).

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
