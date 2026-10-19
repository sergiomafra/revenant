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

- `<IMAGE>`: a disk image file or a device node.
- `--extract <DIR>`: copies each finding into `<DIR>` as soon as it is found. The engine refuses to extract onto the
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

- `kind`: `jpeg`, `mp4` or `mov`. `health`: `intact`, `partial` or `damaged`.
- With `--extract`, a `found` line gets `"file":"<path>"`. If extracting that one finding fails, it gets
  `"error":"<code>"` instead and the scan continues.
- Fatal errors print `error: <code>` to stderr and exit with 1 (for example `open_failed`, `destination_on_source`).

## Making a card image (read-only)

Work on an image of the card, not on the card itself. Imaging only reads from the card.

macOS:

```sh
diskutil list                      # find the card, e.g. /dev/disk4
diskutil unmountDisk /dev/disk4    # unmount without ejecting
sudo dd if=/dev/rdisk4 of=card.img bs=4m
```

**Double-check the device name with `diskutil list` before running `dd`, and never swap `if` and `of`: `of=` is written to.**
`card.img` must be on a different disk than the card, with enough free space.

Windows: use a read-only imaging tool (for example FTK Imager, which can create a raw image of a physical drive) rather than
`dd`, and save the image to a different drive than the card.

Then:

```sh
revenant-dev scan card.img --extract ~/recovered --allow-same-volume   # only if ~/recovered is on the same disk as card.img
```
