# revenant-core

The Revenant recovery engine (library). It never writes to the source being read, and contains no UI text.

## Current state

Every engine error is an `EngineError`; each variant has a stable `snake_case` code, returned by
`EngineError::code()`:

| Variant               | Code                    |
| --------------------- | ----------------------- |
| `Open`                | `open_failed`           |
| `Read`                | `read_failed`           |
| `DestinationOnSource` | `destination_on_source` |
| `Write`               | `write_failed`          |
| `Cancelled`           | `cancelled`             |
| `DeviceGone`          | `device_gone`           |
| `PermissionDenied`    | `permission_denied`     |
| `DeviceNotFound`      | `device_not_found`      |
| `NotWholeDisk`        | `not_whole_disk`        |
| `SystemTool`          | `system_tool_failed`    |
| `Unsupported`         | `unsupported_platform`  |
| `DestinationUnknown`  | `destination_unknown`   |
| `UnmountFailed`       | `unmount_failed`        |
| `RemountFailed`       | `remount_failed`        |
| `SystemDisk`          | `system_disk`           |
| `MapCorrupt`          | `map_corrupt`           |
| `MapMismatch`         | `map_mismatch`          |
| `MapOtherSource`      | `map_other_source`      |
| `SessionCorrupt`      | `session_corrupt`       |
| `SessionVersion`      | `session_version`       |
| `SessionOtherSource`  | `session_other_source`  |
| `SessionNotEmpty`     | `session_not_empty`     |

```rust
use revenant_core::EngineError;
assert_eq!(EngineError::Cancelled.code(), "cancelled");
```

## Source

`source::Source` is the engine's read-only disk-access layer: `len()`, `read_at(offset, buf)` (positional, no shared cursor; fills
the buffer except at the end of the source, returns 0 past the end), `volume()` and `disk()` (the whole disk the source reads
when it is a device, else `None`; `Fragmented` and the scan's cache answer their inner source's). There is no write method.

`source::FileSource` implements it for regular files (disk image files), opened read-only (`pread` on Unix, `seek_read` on
Windows). Anything else is refused (`open_failed`), checked on the opened handle (so a node swapped in after an earlier
check is caught): a device node, a FIFO (opened without waiting for a writer on Unix), a socket, a folder. `source::volume_of(path)` returns the `VolumeId` of any existing
path (device id on Unix, volume serial number on Windows), so extraction onto the source's own volume can be refused. A test
scans `src/source/` to ensure no write-capable open flags are ever introduced, and another checks that the image's bytes and
modification time are unchanged after a full scan that extracts every finding.

A source that vanishes (a card pulled out, a cable cut) is `EngineError::DeviceGone { offset }` (`device_gone`; `offset`
is that of the read that failed, or the first byte a short read did not return), never a bad sector: a source returns it
when it can tell, and a read that comes back short before the length the source announced
counts as vanished too. The scan stops with it (below), extraction stops with it and removes the incomplete file, and the
scan's cache never remembers those sectors as unreadable: it keeps the first offset and loads nothing from the source
again (what it already holds is still served).

### Devices

`source::DeviceSource` reads a whole card or disk directly, read-only. Only whole disks: `source::DiskId::parse` accepts
`disk4`, `/dev/disk4`, `/dev/rdisk4`, `PhysicalDrive2` and `\\.\PhysicalDrive2` on every platform (canonical names `disk4`,
`PhysicalDrive2`); a partition (`disk4s1`) or anything else is `not_whole_disk`. A device is opened with
`device::SystemDevices.open(&disk)` (see [Disks](#disks)) and wrapped with `DeviceSource::new(raw, disk)`:

```rust,ignore
use revenant_core::device::{DeviceOps, SystemDevices};
use revenant_core::source::{DeviceSource, DiskId};
let disk = DiskId::parse("disk4")?;
let source = DeviceSource::new(SystemDevices.open(&disk)?, disk);
```

`open` reads the raw node on macOS (`/dev/rdisk4`, plain `File::open`) and `\\.\PhysicalDrive2` on Windows (`GENERIC_READ`
only, sharing read and write); it needs root or Administrator (`permission_denied` otherwise), a missing disk is
`device_not_found`, and other systems answer `unsupported_platform`. Its geometry (`Geometry { size, logical_sector,
physical_sector }`) comes on macOS from `diskutil info -plist` (`TotalSize` or `Size`, `DeviceBlockSize`) and `ioreg -a -r
-c IOBlockStorageDevice -l` (the device's `Physical Block Size`, else the logical size); after the node is opened,
`diskutil info` is asked again and the open fails with `device_not_found` unless it still describes the same medium (size,
sector size, names, `DiskUUID`…), so a card swapped between the query and the open is never read with the old geometry.
On Windows the geometry comes from the open handle itself: `IOCTL_DISK_GET_LENGTH_INFO` and the storage access alignment
property (else the drive geometry's bytes per sector).

System tools are run by absolute path (`/usr/sbin/diskutil`, `/usr/sbin/ioreg`, `/bin/df`), with stdin and stderr closed,
their output bounded to 16 MiB and their run time to 30 s (a tool stuck on a dying disk is killed); any failure is
`system_tool_failed`, never a panic.

A device reads only whole logical sectors; `DeviceSource` accepts any offset and length and aligns inside: an aligned
request goes straight to the caller's buffer, a partial sector at either end through a sector buffer aligned to 4096 bytes.
4Kn disks (4096-byte logical sectors) and 512e disks (512 logical, 4096 physical) are transparent. Scans and copies on a
device use `DeviceSource::scan_sector()` (`max(physical, logical)`) as `ScanOptions::sector`, so a bad physical sector is
read once and reported as one unreadable run of its size. A read error is `device_gone` when the error says the device
left (macOS `ENOENT`/`ENXIO`/`ENODEV`; Windows `ERROR_DEVICE_NOT_CONNECTED`, `ERROR_NO_SUCH_DEVICE`, `ERROR_DEV_NOT_EXIST`,
`ERROR_NOT_READY`, `ERROR_FILE_NOT_FOUND`) or when the device is no longer present (macOS: `/dev/diskN` is gone; Windows:
`IOCTL_STORAGE_CHECK_VERIFY2` fails); otherwise it is a bad sector (`read_failed`). `source::RawDevice` (aligned reads
and the geometry) is what `DeviceSource::new(raw, disk)` wraps, for a privileged helper or a test double.

### Disks

`device::DeviceOps` is what the engine asks the operating system about disks (`list`, `open`, `destination`), so a
privileged helper (Module 2.1) or a test double can serve it; `device::SystemDevices` is the running system's (macOS,
Windows; elsewhere every call is `unsupported_platform`). `destination` needs no privilege and may stay outside the helper.

`list()` returns the connected whole disks, sorted by id (`disk2` before `disk10`), each a `DiskInfo`: `id`, `geometry`,
`vendor`, `model`, `serial` (often the card reader's, not the card's), `removable`, `internal`, `system`, `virtual_disk`,
`solid_state` (`None` when the system does not say) and its `volumes` (`VolumeInfo { id, fs, label, mount_point, size }`).

- macOS: the whole disks of `diskutil list -plist`, each completed by `diskutil info -plist` of the disk and of each of
  its partitions, plus one `ioreg -a -r -c IOBlockStorageDevice -l` (vendor, product, serial, physical sector). APFS
  containers the system synthesizes (`disk3` on top of `disk0s2`) are not listed: they are shown through their physical
  disk. The system disk is the one holding `/`: the `ParentWholeDisk` of `diskutil info -plist /` and the disks of its
  `APFSPhysicalStores`. `removable` is `Removable` or `RemovableMedia`, `internal` is `Internal`, `solid_state` is
  `SolidState`, and an image attached with `hdiutil` (`VirtualOrPhysical` `Virtual`, bus `Disk Image`) is `virtual_disk`.
  A disk whose info cannot be read (it left meanwhile) is left out; the list, the info of `/` (or one naming no disk) or
  `ioreg` failing fails the listing with `system_tool_failed`: a listing never silently shows no system disk.
- Windows: `\\.\PhysicalDrive0` to `63`, opened without read access (queries only): `IOCTL_STORAGE_QUERY_PROPERTY` (vendor,
  product, serial, removable media, bus type; the seek penalty for `solid_state`), the drive geometry and the access
  alignment. Every volume (`FindFirstVolumeW`/`FindNextVolumeW`) is attached to the disks of its extents
  (`IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS`), with its file system and label (`GetVolumeInformationW`) and first mount point.
  The system disk holds the volume of the Windows folder (`GetSystemWindowsDirectoryW`, not an environment variable); a
  dynamic volume spanning several disks marks them all, and a system volume that cannot be found or has no extent fails
  the listing (`open_failed`). A destination volume with more extents than the 64 read is refused, never half-checked. USB, SD,
  MMC and 1394 buses are external, virtual buses (a VHD) are `virtual_disk`.

`unmount(&disk)` unmounts every volume of a disk **without ejecting it**, so the system stops writing to the card while
it is read, and returns an `UnmountGuard`; `remount(&disk, Some(guard))` mounts the volumes again. The engine never
remounts by itself: remounting gives the system back the chance to write to the card, so it happens only when asked.
The system disk is never unmounted: `unmount` refuses it with `system_disk` before running anything, and a disk not
listed is `device_not_found`.

- macOS: `diskutil unmountDisk diskN` (never `force`; it also unmounts the volumes of an APFS container on the disk and
  succeeds when nothing is mounted), then a listing confirms it. The tool failing or a volume of the disk still mounted is
  `unmount_failed` with `volumes` (the ones the listing shows mounted; an APFS container's volumes are not listed, so they
  may be empty); a disk gone from the listing is `device_not_found`. The guard holds nothing. `remount` is `diskutil
  mountDisk diskN`.
- Windows: every volume with an extent on the disk is locked and dismounted (`FSCTL_LOCK_VOLUME`, then
  `FSCTL_DISMOUNT_VOLUME`), and the guard keeps the locks for the session so Windows does not mount the volumes again;
  dropping the guard releases them, after which Windows mounts a volume on its next access. A volume that cannot be
  locked (in use) makes the call fail with `unmount_failed` naming it, and the volumes that did lock are released, so
Windows may mount them again. While any local volume's disks cannot be told (its extents query failed), nothing is
locked and the call fails closed with `unmount_failed` naming that volume, since it could lie on the card. A volume
whose drive holds no media (an empty slot of a multi-slot card reader) is not unknown: it lies on no disk, so it never
blocks an unmount; CDs and network volumes (`GetDriveTypeW`) have no disks either. "No media" needs the volume's open
to fail with `ERROR_NOT_READY`, or its extents query to fail with it and the media check on the same handle
(`IOCTL_STORAGE_CHECK_VERIFY2`) to answer `ERROR_NO_MEDIA_IN_DRIVE` or `ERROR_NOT_READY`, and a second try 300 ms later
to say the same; anything else fails closed. Only the unmount makes that second try: the listing, `destination` and the
reconnect polls ask each volume once and never wait (a "no media" volume simply has no disks there), and the unmount
waits 300 ms once for all the volumes that said "no media", then asks only those again (a four-slot reader costs 300 ms,
not 1.2 s). The residual risk: a card that stays not ready through both tries and whose
media check says no media would be skipped and left mounted (it is then unreadable for the scan as well). Each volume is
  opened read-only first; only when Windows refuses the lock or the dismount on that handle (`ERROR_ACCESS_DENIED`) is
  it opened again with write access, solely to repeat the two requests. That handle never reads or writes data (the
  engine's single exception to opening anything for writing, recorded in the project rules and checked by the
  `source_has_no_write_path` test); the card's data is still read only through the read-only `\\.\PhysicalDriveN`.
  `remount` releases the guard and queries each volume, which mounts it.

Locking a volume on Windows needs Administrator. When the unmount fails, a scan can still go ahead with the card mounted: the engine
still never writes to it, but the system may (warning the user is the app's job).

**Reconnecting.** `device::reconnect(&ops, &identity, &cancel, poll)` waits for a card that was disconnected (a loose
cable, the computer sleeping) to come back, and returns `Reconnected { source, unmount }`. Every `poll` (the app uses
500 ms) it lists the disks with the same size and model as the identity (and the same serial when both have one; the
system disk never), opens each candidate read-only only to read its identity sectors, on a thread of its own (a probe),
and compares with `same_source` when the probe answers. A probe is never waited for: a candidate whose reads hang does
not hold up the others, and is not opened again until its probe returns (so a hung disk costs one blocked thread, not
one per poll; at most 16 probes run at once). A candidate that does not match is closed and nothing is unmounted; the
one that matches is closed, unmounted, and only then opened again and its identity read once more (after the unmount):
still the card → returned as `source` (`unmount` holds the guard, or the error when the unmount failed: the card is
then read mounted); not (another card took its name meanwhile) → closed, and waiting goes on. Nothing but the identity
sectors is read before the unmount. Errors: `cancelled` (seen while waiting), `device_not_found` for an identity that is
not a device's, `unsupported_platform`, or the error of opening the card again. The remount window: the system mounts the card by itself
when it comes back and may write to it (`.fseventsd`, a dirty flag) before `reconnect` unmounts it; closing that window
(refusing the mount while a session lasts) is for the privileged helper of Module 2.1.

Both the pre-flight check (below) and `reconnect` open the disk before unmounting it (`reconnect` only to read the
identity sectors): opening is read-only and never writes.

### Destination

`Destination::check(&ops, &source, dir)` checks a destination folder once against the source; extraction (`extract_to`),
and later the copy and the session, write only into a `Destination`. The folder must exist (`open_failed` otherwise) and:

- must not be on the source's volume (an image file's): `destination_on_source`;
- for a device source (`Source::disk()` is `Some`), must not be on any volume of that disk: `ops.destination(dir)` gives
  the disks the folder lives on (`DestInfo { disks, fs, free_bytes, max_file_bytes }`), and the source's disk among them
  is `destination_on_source`. An APFS volume counts as its container and the disks of its physical stores, so a folder
  on an APFS volume whose store is a partition of the card is refused too;
- for a device source, when those disks cannot be told (a network folder, a failed query, no disk) the folder is refused
  with `destination_unknown`: it could be the card itself, and nothing is ever written to the card being recovered.

The folder is resolved first (`std::fs::canonicalize`: symbolic links, and junctions on Windows, are followed), and the
rules and every later write use the resolved folder, so a link pointing into the card is judged as the card;
`extract_to` also refuses (`destination_on_source`) when the folder is no longer on the volume it was checked on. Anything
else written there goes through `dest.create_named(&finding, name, suffix)` (the CLI's previews): the same check, then
a new file in the resolved folder under the extraction's naming rule (never overwriting). Known
limit: a folder inside a mounted disk image (DMG, VHD) whose backing file lives on the card is not detected.

An image source (`disk()` is `None`) only gets the volume rule; `ops` is not called. `dir()` is the folder and `info()`
what it lives on (`None` for an image source). On macOS `destination` runs `df -P -k <dir>` (the volume's node and the
space available) and `diskutil info -plist <node>` (`ParentWholeDisk`, `APFSPhysicalStores`, `FilesystemType`); on Windows
the volume of the folder (`GetVolumePathNameW`, `GetVolumeNameForVolumeMountPointW`), its extents, file system and
`GetDiskFreeSpaceExW`. On FAT (macOS `msdos`, Windows `FAT`/`FAT32`) `max_file_bytes` is 4 GiB − 1 (`device::FAT_MAX_FILE`).

### Source identity

`SourceIdentity` recognizes a source later (a resumed copy, a reconnected card, a reopened session); compare two with
`same_source`, never `==`.

- `SourceIdentity::of_device(&info, &source)` = size, model, serial and `table`, always computed: the FNV-1a 64 digest of
  sector 0, of sectors 1 to 33 when sector 0 is a protective MBR (a GPT), and of the boot sector of the first volume the
  table names (each a logical sector). The bytes the system rewrites while a FAT or exFAT volume is mounted are zeroed
  first (FAT12/16 byte 37, FAT32 byte 65, exFAT bytes 106, 107 and 112; also in sector 0 when the card has no partition
  table), so mounting the card does not change it, while reformatting it does. An unreadable sector counts as zeros; a
  device that is gone is `device_gone`.
- `SourceIdentity::of_image(&source)` = size and the FNV-1a 64 of the first MiB, the MiB at the middle (aligned down to
  1 MiB) and the last MiB.
- `SourceIdentity::of_mapped(&mapped_source)` = a rescue copy's image read through its map (below): `Mapped { size,
  digest, origin }`, the digest `of_image` would give with every byte not rescued counted as zero, and `origin`, the
  identity of the source the map records. Rescuing more of those three MiB (a copy resumed meanwhile) changes it.
- `same_source`: devices match when sizes, models and `table`s are equal, and the serials too when both sides have one
  (readers often report their own serial, so it is only an extra check). Two identical cloned cards look the same. Images and
  mapped images match when equal; a device, an image and a mapped image never match one another.

`source::Fragmented::new(source, pieces)` is a read-only logical view of pieces (`Fragment { start, len }`, absolute ranges)
of a source, in the given order: offset 0 is the first byte of the first piece. Each piece is first clamped to the inner
source's end (as `extract` clamps it), and `len()` is the sum of the clamped lengths. A read crosses pieces in order; a read
error is the inner source's, so its offset is absolute in the inner source; `volume()` is the inner source's.
`absolute(start, len)` maps a logical range back to absolute pieces of the source, merging pieces that touch.

## Pre-flight check

`preflight::preflight(&ops, &PreflightRequest { disk, unmount, copy_to, session, sample_timeout })` is called before any long read
(a scan or a copy of a disk) and returns `Preflight { disk, issues, guard }`: the disk as listed, **every** problem
already known (empty when none), and the unmount's guard when one was asked and succeeded (keep it while the disk is
read). Nothing is written anywhere. In this order:

1. **Find** the disk in the listing: not there is `device_not_found`, a listing that fails is `open_failed`, and the
   system disk is `system_disk` (never opened, unmounted or read); each ends the check.
2. **Open** it read-only: `permission_denied` (root or Administrator needed), `device_not_found` (gone meanwhile) or
   `open_failed` (any other failure); each ends the check.
3. **Unmount** (when `unmount`): a failure is `unmount_failed` with the volumes still mounted (the check goes on: the
   disk can still be read mounted); a disk gone meanwhile is `device_not_found` and ends the check.
4. **Samples**: the first, the middle (`sectors / 2`) and the last logical sector, one after another, each read on its
   own thread and waited for at most `sample_timeout` (callers pass `preflight::SAMPLE_TIMEOUT`, 10 s). A read that does
   not return in time is `device_not_responding { offset }` and ends the sampling; its thread is left behind, blocked
   until the system gives up on the read (a dying disk can hold it for minutes), and the call returns anyway. That
   thread keeps the device open meanwhile, so ejecting the disk may fail as busy until the system gives up. In the
   worst case the whole check waits about 4 × `sample_timeout` (three slow samples, then the one identity read, for a
   resumed copy's map or a session). A failed read is `sample_unreadable { offset }`; a device that vanishes is `device_not_found`.
5. **Copy destination** (when `copy_to`, the image file a copy would write): its parent folder goes through
   `Destination::check` (`destination_on_source`; `destination_unknown` for anything else, including a folder that
   does not exist). Then the same room checks as `rescue::copy` (shared code): the free space against what is still to
   copy, `destination_too_small { need, free }`, and the file system's largest file against the disk's size,
   `destination_file_too_large { limit }` (FAT32: 4 GiB − 1; the image is never split). What is still to copy is the
   whole disk, or, when `map_path(copy_to)` holds a map of this same disk (`same_source` against the identity read now,
   under the same timeout), the bytes the map does not call rescued (its bad bytes count too: whether retries will be
   asked is not known yet). A map of another disk, of another size or that cannot be read counts as a fresh copy (the
   copy itself refuses it).
6. **Session** (when `session`, the folder of a saved session to resume): a folder holding no session is fine; a
   manifest that cannot be read, or of an unsupported version, is `session_corrupt`; a session not made from this disk
   (`same_source` against the identity read now, under the same timeout) is `session_other_source`. The identity is read
   at most once per check (shared with step 5); when the samples did not all answer or it cannot be read in time, the
   disk cannot be told apart and nothing is reported (the resume itself still refuses another disk).

| Issue                     | Code                         |
| ------------------------- | ---------------------------- |
| `PermissionDenied`        | `permission_denied`          |
| `DeviceNotFound`          | `device_not_found`           |
| `SystemDisk`              | `system_disk`                |
| `OpenFailed`              | `open_failed`                |
| `NotResponding`           | `device_not_responding`      |
| `SampleUnreadable`        | `sample_unreadable`          |
| `UnmountFailed`           | `unmount_failed`             |
| `DestinationTooSmall`     | `destination_too_small`      |
| `DestinationOnSource`     | `destination_on_source`      |
| `DestinationUnknown`      | `destination_unknown`        |
| `DestinationFileTooLarge` | `destination_file_too_large` |
| `SessionOtherSource`      | `session_other_source`       |
| `SessionCorrupt`          | `session_corrupt`            |

The issues come in the order of the steps (an unmount failure, then the samples', then the destination's: room before the
file-size limit, then the session's).

## Rescue copy

`revenant_core::rescue` copies a whole card (or disk) into an image file around its bad sectors (`copy`, design §5.4),
keeps the map of what the image holds, and reads the image back through it.

### Copying

```rust,ignore
let dest = Destination::check(&SystemDevices, &device, dir)?; // never on the card's disk
let opts = CopyOptions { sector: device.scan_sector(), ..CopyOptions::default() };
let summary = rescue::copy(&device, &identity, &dest, "card.img", &opts, &cancel, &mut |event| { /* … */ })?;
```

`copy(source, identity, dest, image, opts, cancel, on_event)` writes `dest.dir()/image` and its map
`<image>.rvmap`. The image is created at the source's length and never split into pieces; it is sparse (nothing
allocated until written) only where the file system allows it (APFS, ext4; not HFS+, FAT or NTFS), so the room check
counts the whole length. `image` must be a plain, safe file name, and an existing `image` that is not a regular file (a
link), that has more than one name (a hard link to a file elsewhere on the volume, on every system), or that is no
longer the file checked by the time it is opened (swapped meanwhile; same device and inode on Unix, a reparse point
opened as itself on Windows), is refused (`write_failed`) before anything is written; the folder
must still be on the volume it was checked on (`destination_on_source`). The map's temporary file is always created
new (`create_new`, after removing whatever was there, a link included, never its target), so nothing planted in the
folder is written through.

Before creating or touching anything, when the destination knows what it lives on (`dest.info()`, a device source):
its largest file (`max_file_bytes`, 4 GiB − 1 on FAT32) must hold the whole image, and its free space what is still to
copy (the whole length on a fresh copy; on a resume the bytes still untried, to trim or to scrape, plus the bad ones
when `retries > 0`, so a finished copy needs no room); otherwise `write_failed` and no file is created. A copy from an image file (no `info()`) skips these two checks.

The source is read directly (not through the scan's cache), in passes (`CopyOptions` defaults in brackets):

1. **Fast** — the untried bytes in block-aligned chunks [1 MiB]. A chunk that fails becomes `ToTrim`, and the next
   `skip` bytes are jumped over (left untried): [64 KiB] after the first failure, doubling on each consecutive one up to
   `min(1 GiB, size / 64)`, back to the minimum after a good chunk. A failing area is left early instead of being read
   block by block, so most of a damaged card is copied while it still works.
2. **Skipped** — what the jumps left, in the same chunks, without jumping.
3. **Trim** — each `ToTrim` run sector by sector from its start until a sector fails, then from its end backward until
   one fails (those are `Bad`); the middle becomes `ToScrape`.
4. **Scrape** — each `ToScrape` sector once: read → `Rescued`, failed → `Bad`.
5. **Retry** — only with `retries > 0` [0]: each `Bad` sector read once more per repetition. None by default, so a dying
   card is not hammered on the sectors that already failed; without retries a bad sector is read at most twice (in its
   failing block, then alone). On a finished map, `retries` runs only the repetitions not done yet (a copy that ended
   after `n` stores `Pass::Retry(n)`, one that ended with none `Pass::Done`).

`sector` [512] is the map's and the sector-by-sector passes' granularity: against a device pass
`DeviceSource::scan_sector()`, so a bad physical sector is read once. A source that vanishes (`device_gone`, or a read
that comes back short) or a cancel (`cancelled`) stops the copy: the map is saved and the error returned, and nothing
is marked for the read that stopped it, so pulling the card out never turns sectors bad. A write error is `write_failed`.

Resume by calling `copy` again with the same `image`: the map's identity must be `same_source` as `identity`
(`map_other_source`: another card), and its size and sector the source's and `opts.sector`, the image's length the
map's (`map_mismatch`). The copy goes on from the pass and position the map records and never reads again what is
rescued. The map is saved (after the image's `sync_data`, so it never claims bytes the image does not hold) every
`save_every` [30 s] or `save_bytes` rescued [256 MiB], at each pass change and when the copy stops; a crash or power
loss resumes from the last save. The map is saved right after the image is created (and the image removed if that
fails); an image left without a map anyway (a crash in between) makes every later copy fail (`open_failed`) until it is
deleted.

`on_event` gets `CopyEvent::Progress { pass, pos, totals, bytes_per_sec, eta_secs }` after every read (bytes in each
state, the reading speed smoothed by `rate::RateMeter` with a 10 s half-life, and the seconds left at that speed), `Bad
{ start, len }` when a region is marked bad and `Saved` after each map save; the last `Progress` of a finished copy has
`Pass::Done`. The result is `CopySummary { totals, complete }`, `complete` when nothing is left untried, to trim or to
scrape.

### The map

`RescueMap::new(size, sector, identity)` covers `[0, size)` with merged regions (neighbors always differ in state), all
`Untried` at first. A region is `Untried` (not read yet), `Rescued` (the image holds the card's bytes), `ToTrim` or
`ToScrape` (a failed block whose edges or middle are still to be read sector by sector) or `Bad` (sectors that failed
one by one). `set(start, len, state)` takes multiples of the sector (the end clamps to the size) and merges; `state_at`,
`regions()` (ascending, contiguous from 0, summing to the size) and `totals()` (bytes per state) read it. The map also
records the source's `SourceIdentity` and the copy's pass and position (`pass()`, `set_pass`: `Fast`, `Skipped`, `Trim`,
`Scrape`, `Retry(n)`, `Done`).

The map holds at most `MAX_MAP_REGIONS` (2^20) regions: a `set` that would need more returns false and changes nothing.
The copy then finishes coarsely, never failing: each `ToTrim` and `ToScrape` region becomes `Bad` whole without being
read; the untried bytes are still read in blocks, a failed block is marked `Bad` whole when the map has room, and when
not even that fits, the rest of that untried stretch is marked `Bad` unread. On a disk with millions of scattered errors
good sectors near bad ones are lost that way; nothing unread is ever called rescued.

The map lives next to the image as `<image>.rvmap` (`map_path`), in our own text format (UTF-8, decimal numbers, one
space between fields):

```text
revenant-rescue-map 1
size <u64> sector <u32>
identity device <size> <hex model or -> <hex serial or -> <table hex> | identity image <size> <digest hex>
pass <fast|skipped|trim|scrape|retry:N|done> <pos>
<start> <len> <state>          (one line per region, ascending; state untried|rescued|to_trim|to_scrape|bad)
end <crc32 of every byte before this line, 8 lowercase hex digits>
```

Hex values are lowercase (model and serial as the hex of their UTF-8 bytes; digests as 16 digits). `save(path)` writes
a new `<path>.tmp` (whatever was there removed first, a link but never its target; created with `create_new`, which
never follows a link), `sync_all`s it, renames it over `path` and (on Unix) syncs the folder, so a crash at any moment leaves
the previous map or the new one, never a mix; failures are `write_failed`, except the folder sync, which is best effort
(the file is complete and synced by then; some file systems refuse it, and at worst a crash right after brings back the
previous map). A map whose identity nests `mapped` more than 4 deep is not saved (`write_failed`), so a map that saves
always loads. Two maps are `==` when their contents are. `load(path)` refuses anything that is not
exactly such a file (`map_corrupt`): not a regular file (a link, even to a valid map, a folder, a device or a FIFO,
never followed, read or waited on), cut short anywhere, a checksum that does not match, regions that do not cover the
size, more than 2^20 regions, or a file over 64 MiB; nothing there, or a file that cannot be opened, is `open_failed`.

`MappedSource::open(image)` opens the image read-only with its map (`map_mismatch` when the map's size is not the
image's length). A read touching any byte that is not `Rescued` fails with `Read { offset }` of the first such byte, as
a bad sector of the card would: the scan reports the holes as `Unreadable` and calls nothing in them `Intact`, where a
plain `FileSource` of the image would read the zeros the copy left there as data. `volume()` is the image file's.

## Formats: JPEG

`formats::Format` is the detector interface: `recognizes(head)` checks a signature at a sector start,
`confirms(source, start)` validates the header there (cheap, never recursive, never measures the whole file), and
`measure(source, start)` returns a `Measurement { len, kind, health, fragments }` found by walking the file's structure, or `None` when
the bytes are not a file of that format. `None` never means "could not read": a read error does not make a file vanish.
`formats::all_formats()` lists the supported detectors. `FileKind::as_str()` gives a stable lowercase id (`"jpeg"`, `"mp4"`,
`"mov"`).

"Another file starts here" (used to cut a JPEG's scan data or an `mdat` at a sector start) means a format both `recognizes`
the head and `confirms` it, so a random byte pattern that merely looks like a signature can no longer cut a file. JPEG
confirms with the header walk up to SOS with DQT and SOF present (no scan-data walk); ISO BMFF confirms exactly what `measure`
accepts, by a header-only walk from the `ftyp` (at most 64 top-level box headers, no payload reads, no recursion): it meets
an `mdat`, or the `ftyp` has a HEIC brand and a `meta` is met, or a box header falls in an unreadable sector (it does not
refute the file, as for JPEG). A lone `ftyp` + `free` is not a video and no longer cuts another file. JPEG's `confirms` walks at most 1 MiB of header. `scan` still uses `recognizes` plus `measure` for its candidates.

Detectors read the source through one shared reader, in 1 MiB windows aligned to 512-byte sectors (never a whole file at
once). When a window cannot be read it is re-read sector by sector: unreadable sectors read as zeros and are remembered, and
only the end of the source ends a walk for lack of data. A file whose measured range contains an unreadable sector is never
`Intact` (details per format below).

Each reader takes its window from a small per-thread pool (at most 4 kept, so at most 4 MiB per measuring thread, outside
the scan cache; see the memory bound under Scan) and gives it back when dropped, so measuring allocates nothing after
warm-up; the detectors share one list of formats built once; the JPEG entropy walk jumps from `FF` to
`FF` (still checking every sector start it crosses for another file); a reader loads a `moov`/`meta` payload at most once per
measurement, however many checks need it.

`formats::Jpeg` follows the marker segments by their declared lengths (so the thumbnail embedded in an APP1/EXIF segment does
not end the file; DAC and JPG markers count as segments, and `FF` fill bytes before a marker are skipped), requires DQT and SOF before SOS, and scans the entropy-coded data up to EOI (`FF D9`), up to
`JPEG_MAX_LEN` (64 MiB). If the header walk breaks on an unreadable sector, the search for EOI continues from there. Health:

- `Intact`: EOI found, no unreadable sector inside, and the content verified (ruling Q6). For a frame Revenant's entropy
  decoder covers (below), the evidence is that decoder walking every MCU, the exact count with restart markers in
  sequence, with EOI right after the last one; `zune-jpeg`'s strict mode is not used for these, because it zero-fills past
  an early EOI and ignores data left before EOI, so half a photo followed by zeros, by another photo's tail or by its own
  tail with a sector missing, then an EOI, would pass. Such a file is `Partial` (and may be put back together, see
  "Fragmented JPEG"). For any other frame (progressive, arithmetic, 12-bit, a header the decoder cannot parse), the
  evidence is a strict `zune-jpeg` decode. This one rule (`decode_health`) judges every JPEG: a plain JPEG, a RAW's, RAF's
  or CR3's preview, the own-JPEG check that lets a file head inside a RAW's preview count as the RAW's own data, and a
  reassembled photo. Below, "decodes strictly" means passing it; "decodes leniently" means a result of `Partial` by it.
- `Partial`: no EOI before the end of the source, `JPEG_MAX_LEN`, or a 512-byte sector that starts another recognized file
  (the length stops there), and something of the photo would show (below). Or EOI found, with a frame Revenant's entropy
  decoder covers, and that decoder completes at least one row of MCUs before it fails (ruling Q10, as for a cut file:
  that much of the photo shows; `zune-jpeg` is not run for these frames at all), or walks to EOI but the file contains
  unreadable sectors. Or EOI found, with any other frame, and only the lenient `zune-jpeg` decode succeeds, or the file
  contains unreadable sectors and a decode (strict or lenient) of the zero-filled bytes succeeds, or the declared size is
  over 210 megapixels so it is not decoded ("not fully verifiable"), or a DAC segment (arithmetic coding) makes it one
  `zune-jpeg` cannot decode. The length never exceeds `JPEG_MAX_LEN`.
- `Damaged`: EOI found, with a frame the entropy decoder covers, and that decoder fails before completing one row of MCUs
  (ruling Q9: nothing of the photo would show); or EOI found, with any other frame, and neither `zune-jpeg` decode
  succeeds. The length is still exact. Also `Damaged`: a cut file (no EOI, as above) whose entropy-coded data does not
  hold one full row of MCUs before its first error, the cut, or the first 512-byte sector of one repeated byte value or
  unreadable sector, such as a JPEG cut right after its header and followed by erased (zero-filled) space: nothing of the
  photo would show. Zeros decode as valid blocks under the standard Huffman tables, so they are not counted as the
  photo's data. The length is still where the file was cut.

A cut file is judged by Revenant's own entropy decoder (`formats/jpeg_entropy.rs`, written from ITU-T T.81): it covers
sequential Huffman JPEGs (SOF0/SOF1, 8-bit, 1–4 components all in one scan, sampling factors ≤ 2, at most 210 megapixels,
every referenced Huffman and quantization table defined; DRI restart markers checked in sequence; a run/0 symbol other than EOB and ZRL, a DC
category over 11 or an AC size over 10 is an error) and decodes no pixels, only the Huffman-coded
blocks, counting complete MCUs exactly; it stops at the first row. Any other cut JPEG (progressive, arithmetic, 12-bit, a
header it cannot parse) stays `Partial` as before. On hostile data every loop is bounded by the frame's MCU count, 64
coefficients per block, 16 bits per code and the bytes up to the cut; a DHT whose code lengths cannot form a prefix code is
rejected.

The validation decode outputs grayscale (1 byte per pixel) to bound memory.

### Fragmented JPEG

When a JPEG is not `Intact` and its frame is one the entropy decoder covers, but its contiguous data does not decode to
EOI with every MCU (another file was written in the middle of it, or the rest of it lies further on), `Jpeg::measure`
searches for where the photo's data continues and puts the photo back together from two pieces (`formats/jpeg_join.rs`,
crate-private). All positions are absolute 512-byte sector boundaries:

1. The contiguous data is decoded (keeping each block's luma DC value). The break `x` is the first sign that the data
   stopped being this photo: the byte after the decode error (an error is reported at a marker's `FF`, and the byte after
   it may be the first one that is not this photo's), the cut, or the first sector of one repeated byte value (erased
   space, which would otherwise decode as valid blocks). The first piece may end at any of the 4 sector boundaries at or before
   `x` (`c1`) that leave it at least 16 KiB long (ruling Q8: a few sectors of a photo hold too little to recover, and every
   header could otherwise start a search), each resumed from the decoder's last state at or before it.
2. One forward pass over the window of `JPEG_GAP_MAX` (64 MiB) after the earliest `c1` records, for each sector start `g`,
   the first event at or after it: an `FF D9` (`EOI` at `e`), or a sign the bytes are not entropy-coded data (`FF`
   followed by anything but `00`, `D0`–`D7`, `D9`, `FF`; a sector of one repeated byte value or an unreadable one; a sector
   where another file starts). The candidates are, for each distinct `EOI`, the first 4 sector starts whose first event
   it is; the second piece `[g, e + 2)` therefore ends inside the window.
3. Every pair `(c1, g)` with `g > c1` and a total length within `JPEG_MAX_LEN` is tried, closest `g` first, then latest
   `c1`: the decoder resumes over the two pieces and passes when every MCU decodes with EOI right at the end of the second
   piece and the DC seam test holds. The DC seam test compares the mean `|ΔDC|` between the luma blocks on both sides of the
   seam with the median of the other block-row boundaries: it passes iff the image has at least 4 block rows and the seam
   is at most 3 × that median + 4 (it is what rejects another photo's tail spliced at a restart marker, where the MCU count
   alone would pass; not always, which is why a photo with restart markers is never joined `Intact`). A try stops after the MCU row below the seam when the seam is already sure to fail (ruling Q14):
   rougher than 3 × the `⌊n/2⌋`-th smallest (from 0) of the boundary means above it + 4, `n` being the number of
   boundaries off the seam (their median, the final base, is at most that); with the seam in the upper half of the image
   there is no such bound (the rows below could be rougher than those above, as under a sky) and the try runs on. It also
   stops at an MCU row where its decoder state (position in the second piece, MCU, buffered bits, DC predictors) is one a
   failed try with the same EOI passed through: from there it reads the same bytes and fails the same way (up to 16,384
   states are remembered).
4. The first passing pair is the result: the measurement is the two pieces, with the JPEG health rule above applied to
   them (`Intact`, or `Partial` with an unreadable sector in either piece; a pair passes only when the walk reaches EOI,
   so it is never `Damaged`).

Evidence for `Intact` (rulings Q1, Q7, Q16): exactly one candidate in the whole window passed, every candidate was
tried, and the photo uses no restart markers (no `DRI`, or an interval of 0). With restart markers every DC predictor
resets at each marker, so another photo of the same frame continues validly after the next marker past the seam, and
only the DC seam test would tell the two apart; such a join is still made (the file is recovered whole) but is at most
`Partial`. Many cameras use restart markers (of the real samples: Fujifilm X-S10, OM System TG-7, Pentax K-r).
A second passing candidate (the search then stops) or hitting a bound makes the reassembled photo at most `Partial`. So
does an EOI with more than 4 sector starts whose first event it is (not all were tried), unless it is the passing
continuation's own EOI (the others lie inside it) or those sectors belong to a file in the gap: the last file head before
them is a JPEG that is `Intact` on its own, contiguously up to that EOI, with a frame (geometry, tables, restart interval)
other than this photo's. A JPEG of the same frame does not count: this photo's rest could lie inside it, as when two
photos from one camera were written interleaved.

Bounds on hostile input (ruling Q8): a first piece of at least 16 KiB; one 8-byte record per sector of the window (at
most 131,072); at most `MAX_JOIN_TRIES` (4096) tries; at most 64 MiB read from the source by the tries and by the checks
of files in the gap, together (each try's reader loads 64 KiB at a time); the first piece's last bytes are read once and
kept in memory (at most 1 MiB). The search looks at the scan's cancel flag every MiB of the window, before every try and
at every MCU row of a try (a measurement reaches the flag through a per-thread scope set by the scan), so a cancelled
scan does not wait for it. Memory per search: the luma DC values of the first piece, of a try and of the joined image
(4 bytes per block, about 13 MiB each at 210 megapixels), the window's records (1 MiB), the remembered states (about
1 MiB) and the first piece's last bytes (1 MiB), held by the measuring thread.

Reading: each sector is read once from the source by the scan and the detectors (this is about reading the card; in
memory the same bytes may be walked several times), except that a JPEG gap search re-reads, through the scan's cache:
the photo's contiguous bytes (decoded again, scanned for erased sectors, and decoded once more up to an erased sector if
there is one: at most 3 × 64 MiB), its 64 MiB window once, at most 64 MiB for the tries and the checks of files in the
gap, and the two pieces once more to judge the reassembled photo (at most 64 MiB).

Not covered: progressive, arithmetic-coded or 12-bit JPEGs (any frame the decoder does not cover), a first piece under
16 KiB, three or more pieces, a second piece that lies before the first, a second piece that ends past the window, and
two photos written interleaved (each split in two with the other's pieces in between): they are at most `Partial`, and
a photo of the same frame glued to the other's tail exactly on a restart marker may even come out `Intact` as a
contiguous file (the MCU count and restart markers are all right, and a DC step below the seam threshold is not seen).
More generally, the DC seam test runs only at the joins the search makes, never inside a contiguous file: a cut photo
immediately followed, with no gap, by the tail of a near-identical photo of the same frame (same camera and settings)
whose bytes decode exactly to EOI is a valid JPEG and comes out `Intact`. Likewise, when a photo's own tail is gone
(overwritten) and its window holds the orphan tail of a near-identical photo of the same frame (a burst), that tail may
be the only candidate that passes, and the join comes out `Intact`: uniqueness (Q1) cannot see a tail that is missing.
This holds only for photos without restart markers: with them, another same-frame photo's tail spliced at a marker
passed the DC seam test in 3 of 168 splices built from mirrored or rotated real previews, so such joins are at most
`Partial` (ruling Q16). A photo split inside a band much rougher than the rest of the image is not put back together: the DC seam test compares the seam with the whole image's median
(2 of 56 real preview splits in `tests/real_samples.rs`, both in the EOS Kiss F preview's textured middle, stay
`Partial`; comparing with the rows next to the seam would join them, but would also glue one more same-frame foreign
tail in the set that was tried). And a photo whose entropy-coded data holds a
whole 512-byte sector of one repeated byte value is treated as cut there (rulings Q5, C3): camera photos very rarely do
this, but a large flat area coded with optimized Huffman tables could (all `00`); such a photo is `Partial` and is not
put back together.

Synthetic JPEGs and disk images for tests come from the `revenant-testkit` workspace crate (`publish = false`); its
`fs` module builds FAT12/16/32 and exFAT volumes (with deleted files, raw entries and quick formats) inside MBR or GPT
disks, written from the Microsoft and UEFI specifications.

## Formats: MP4/MOV

`formats::IsoBmff` recognizes a `ftyp` box (declared size 8..=4096) at a sector start and measures the file by walking the
top-level boxes (`ftyp moov mdat free skip wide uuid meta pdin moof mfra styp sidx udta`) from their declared sizes. Size 1
means a 64-bit `largesize`; size 0 (only for `mdat`, e.g. a recording cut by a dead battery) runs to the first sector where
another file starts, or to the end of the source. The walk stops at an unaccepted type, a
size under 8, or the end of the source; a box that runs past the end counts only up to it. The major brand `qt  ` gives
`FileKind::Mov`, `crx ` gives `Cr3` and, with a HEIC brand and no `moov`, `Heic` (see below); anything else
`Mp4`. Every size is checked against the source length; only `moov` is loaded into memory, and
only up to `MOOV_MAX_LEN` (16 MiB).

Walk boundaries: `ftyp` is accepted only as the first box. Once both `moov` and `mdat` are seen, only trailing
`free skip wide uuid meta udta mfra` boxes continue the file (plus `moof sidx styp mdat` when it is fragmented); anything else
ends it, and a trailing box that overruns the source is left out. The walk also stops at a box that starts on a 512-byte sector
where a recognized file starts. Inside `mdat`, every 512-byte sector start (absolute offset) is checked as well: where a
recognized file of any format starts, the video ends there. An `mdat` whose declared size is cut this way counts as a box
declared larger than what exists (below). Motion-JPEG exception: if the walk was cut inside `mdat`, the box headers are
walked again without the checks inside `mdat` (one read per box, so a cut-off recording does not cost a pass over the rest
of the source), and if (a) that uncut index is `Intact` and (b) a video track (`hdlr` `vide`) has a first `stsd` entry of
`mjpa`, `mjpb` or `jpeg`, the video is walked once more with the checks inside `mdat` skipping that track's chunk offsets
(`stco`/`co64`): the first confirmed file head that is not one of them ends the video. A video whose every head is one of
its frames is whole; a photo written over the video is not at a chunk offset, so it still cuts it. Chunk offsets are relative to the start of the file (its `ftyp`), not to the source.

- No `mdat`: not a video (`None`).
- No `moov`: `Damaged` (the media data is there but not the index; rebuilding it is a later product milestone). After
  the scan such a video may be paired with its `moov` found elsewhere (see "Video with a separated index").
- `moov` larger than 16 MiB: `Damaged`.
- A box declared larger than what exists (cut mid-`mdat` or mid-`moov` by the end of the source, or an `mdat` cut by another
  file written over it): `Partial`.
- Garbled index: if the box walk inside `moov`, `trak`, `mdia`, `minf` or `stbl` ends on an invalid size (or leftover bytes)
  instead of consuming its parent exactly (fewer than 8 trailing zero bytes, a QuickTime terminator, are tolerated), or a non-fragmented file has no `trak` with both `stco`/`co64` and `stsz`:
  `Damaged`.
- Otherwise the `stco`/`co64` and `stsz` tables of each `trak` are checked: any chunk offset at or past the measured end, or
  `first chunk offset + total sample size` past it, gives `Partial`; else `Intact`. Fragmented files (`moof`) have no such
  tables and are `Intact` when their boxes are complete.

Unreadable sectors:

- Inside `moov`: the index is unusable, so `Damaged`; the length still covers `moov`, so extraction keeps it.
- Elsewhere in the file (for example inside `mdat`): an otherwise `Intact` file is `Partial`.
- On a box header: the header cannot be parsed, so the walk ends there as if the file were cut (`Partial`, or `Damaged` when
  no `moov` was seen before it). A header that cannot be read after a complete video (`moov` and `mdat` both seen) is only
  what follows the file: the video stays `Intact` (if nothing else is wrong).

### Video with a separated index

A camera writes `ftyp`, then `mdat` (the frames), then `moov` (the index) last. On a fragmented card the `moov` may end up
elsewhere: the scan then finds the start of the video `Damaged` (no `moov`) and the `moov` nowhere near it. After the
scan, such videos are paired with orphan `moov` boxes (`pairing.rs`, crate-private) and each video put back together is
reported as `Event::Updated` (same `id`, 2 pieces).

Collection during the scan: every block the scan reads is searched for `moov` (keeping the last 15 bytes of each window, so
a header across two windows or blocks is found too). This search runs on the scan thread, the one that reads the source:
it costs about 10% of the page-cache throughput on images with few files (`noise`, `video` in the benchmark), far above
what a card reader delivers, so it is accepted; it could move to the measuring threads later if it ever matters. A box
start `q` is a candidate when its 16 bytes are a size in `[24, MOOV_MAX_LEN + 8]`, `moov`, any 4 bytes and `mvhd`. A candidate is dropped when its header overlaps a piece of a
finding that was `Intact` when added (rounded to whole sectors: those blocks may never be read, so keeping candidates
there would depend on timing) or when it lies inside a non-`Damaged` MP4/MOV/CR3/HEIC finding (its own index). At most
4096 candidates per block are looked at and 4096 per scan are kept (the first ones). The videos are the `Damaged`
MP4/MOV findings (at most 4096, the first ones), except those of a live file (its chain is where its bytes are, so it
is not split elsewhere); a deleted file's video keeps its `origin`, with placement `Carved`, when it is put back
together.

Pairing a video at `s` (the `Found` start) with a candidate at `q`:

1. Header-only walk from `s`: `ftyp`, then other boxes up to an `mdat` with a declared size, no `moov` before it and no
   `moov` header right after it. `P` = where `moov` would sit in the file (the end of `mdat`, relative to `s`); `payload` =
   the start of the `mdat` payload. Anything else is not paired.
2. Pre-filter, before loading anything: `(q − P) ≡ 0 (mod 512)` for some video (the second piece must start on a sector
   boundary). Loads are charged by the 1 MiB cache windows they span, at most 1 GiB per scan; a candidate past that is
   dropped.
3. The `moov` (at most `MOOV_MAX_LEN`) must be readable, not garbled, hold `mvhd` and a `trak`, and have consistent
   `stsz`/`stsc`/`stco`|`co64` tables (present, not truncated, the chunks holding exactly the samples `stsz` lists; at
   most 4,000,000 samples); the checkable samples (H.264 `avc1`/`avc3` with `avcC`, HEVC `hvc1`/`hev1` with `hvcC`,
   Motion-JPEG) must be at least 8. This is judged from the tables without listing a sample.
4. Arithmetic filter: `(q − P) ≡ 0 (mod 512)`, the first sample of any track within 64 KiB after `payload`, and every
   sample ending by `P` (from the chunk table and the sizes, still without listing). Only a `moov` that some video
   passes is listed: its checkable samples are charged to a budget of 16,000,000 per scan before anything is allocated
   (ruling Q11; a `moov` past it is dropped). At most 256 pairs pass the filter; the rest are dropped.
5. Sample checks, never the payload: a NAL sample is walked unit by unit (at most 512): every length at least 1, the
   header's forbidden bit 0, the type 1–23 (H.264) or at most 40 with a non-zero temporal id (HEVC), the units summing
   exactly to the sample size. A Motion-JPEG sample starts `FF D8 FF` and ends `FF D9`. `A(i)` reads sample `i` at
   `s + offset`, `B(i)` at `q − P + offset`. `k` = the first sample failing `A` (binary search; `n` if the last passes).
   Up to 32 evenly spaced samples before `k` must pass `A` and up to 32 after it must pass `B`.
6. Break points `L1`: sector boundaries (relative to `s`) from the start of sample `k − 1` (not its end: a sample cut
   inside its last NAL unit's payload still passes `A`) to the start of sample `k + 1` (or `P`), at least `payload`,
   at most 4096 tried; samples `k − 1` and `k` must pass read across `L1` (bytes before it at `s`, after it at the second
   piece). On a FAT/exFAT card (ruling D13), when `s` is a cluster start of a volume the scan found (cluster at least
   4096 bytes, `s` inside its heap), the break points tried first are only those on that volume's cluster grid
   (`(s + L1 − heap) % cluster == 0`) whose second piece `q − P + L1` starts on a cluster of it too, and uniqueness is
   judged among them; when none passes, every sector boundary is tried as above (a card reformatted with another
   cluster size pairs as before). The pieces are `[s, s + L1)` and `[q − P + L1, … + (P − L1) + moov size)`, inside the source, not
   overlapping. The first piece ends by the `Found` video's own end (the scan cut the video where another file starts,
   ruling Q13), unless that end is one of the video's own Motion-JPEG frames, and no other finding may start inside it:
   otherwise the video is not paired.
7. Uniqueness: a video with two or more passing `moov`s, or a `moov` passing for two or more videos, is not paired.

Health of a reassembled video: what the index says against its length (`P` + the `moov` size), as for a contiguous file
(`Damaged` or `Partial` stay so); an unreadable sector in either piece (from the scan's bad-sector map, so it agrees with
the `Unreadable` events) makes it `Partial`; so does a finding starting inside the second piece (ruling Q13: its bytes
would be claimed twice, and a file head cuts a contiguous video the same way; the video's own Motion-JPEG frames, which
the scan finds as JPEGs, do not count; the scan keeps every finding's start, 8 B each, for this), more than one passing
`L1` (the smallest is kept), or a bound hit
anywhere (the per-block or per-scan candidate cap, the video cap, the load budget, the pair cap, more than 4096 break
points, the sample budget or the step budget below): an unchecked candidate might have passed too. The per-block
candidate cap does not count in a block wholly inside committed skip ranges (such a block may or may not be read, and
all its candidates are dropped anyway), so the result does not depend on timing. Otherwise `Intact`. The checks validate
lengths and header bytes, never the payload, so a break point inside one NAL unit's payload cannot be told from one a
sector away: real H.264/HEVC with large slices (tens of KiB per NAL unit) will usually pair as `Partial`, unless the
video lies on the cluster grid of a FAT/exFAT volume (step 6), which leaves one candidate every cluster instead of every
sector. So will most real videos with sound: audio samples interleaved
between video samples `k − 1` and `k` are not checked, so every sector boundary inside them passes too. The kind comes
from the `ftyp`.

Bounds on hostile input: at most 4096 candidates and 4096 videos, 1 GiB of `moov` loads (one `moov` in memory at a
time, plus the list of its checkable samples: at most 4,000,000 per `moov`, about 16 B each, twice that while it is
built), 16,000,000 samples listed per scan, nothing allocated per sample before the filters pass, 256 pairs, 4096
break points per pair, 512 NAL units per sample check, and 2^24 sample-check steps per scan in total (past it no
further pair is checked). Judging a `moov`'s tables costs time in proportion to its bytes (so at most the 1 GiB loaded).
Cancellation is looked at before each `moov`, each pair and each break point; a cancelled scan returns
`Err(Cancelled)` without the remaining `Updated` events.

Not covered: a video in three or more pieces (a middle piece holding fewer than 1/32 of the samples between the two
verified sides may go unnoticed and be paired), a `moov` before its `mdat`, an `mdat` of size 0, two videos whose first
pieces are identical, and fragmented MP4 (`moof`).

## Formats: CR3 and HEIC (ISO BMFF)

The same `IsoBmff` detector picks the kind from the `ftyp` brands: major brand `crx ` is `FileKind::Cr3` (extension
`cr3`); a HEIC brand (`heic heix heim heis mif1 msf1`) as major or compatible brand, with no `moov` in the file, is
`FileKind::Heic` (extension `heic`) when that brand is the major one or the file has a top-level `meta` (ruling D27).
An MP4 that merely lists `mif1` has a `moov` and stays `Mp4`; one that lists `heic` among its compatible brands, lost
its `moov` (a cut before it) and has no `meta` is a `Damaged` `Mp4`; an `avif`/`avis` major brand is never HEIC. The
price: an MP4 cut before its `moov` that carries a stray top-level `meta` and lists `mif1` is still taken for a HEIC.
And a HEIF image sequence (`msf1`/`heis`) laid out `ftyp`, `meta`, `mdat`, `moov` is measured as a HEIC without its
`moov`: once the still images' extents are all inside, the HEIC is complete (below) and the walk stops before the
`moov`, so the still images come out whole and the sequence's tail (its `moov`) is left out.

- CR3: the `moov`/`stco`/`stsz` rules above apply unchanged (a CR3 has `trak`s). In addition both of its JPEGs must
  decode strictly (the raw image itself is never verified) for `Intact`: the `PRVW` JPEG in the top-level `uuid`
  `eaf42b5e-1c98-4b88-b9fb-b7dc406e4d16` and the full-size JPEG (the first chunk of the first track, when it starts with
  a JPEG SOI), each of a measurable size. A missing `PRVW` (or one whose size field does not fit its box), a `PRVW` or
  full-size JPEG that is unmeasurable, decodes leniently or does not decode is `Partial`. This is not "the largest
  decides" as for a TIFF RAW: the `PRVW` sits in a `uuid` before `mdat` and the CR3 walk checks file heads only inside
  `mdat`, so a broken `PRVW` is the only sign that a file was written over it; `Partial` keeps the scan looking for heads
  inside it (only `Intact` suppresses that). `inspect` still shows the larger of the two as the preview. The `PRVW`
  field layout comes from public format notes and the testkit, not yet from real files.
- HEIC: needs a `meta` (at most 16 MiB) whose `iloc` (versions 0, 1, 2; offset, length, base offset and index sizes of
  0, 4 or 8 bytes) lists the items' extents. `Intact` when every extent (construction method 0, data reference 0,
  offsets absolute in the file) ends inside the measured file; `Partial` when one ends past it (or the file is cut, or
  contains an unreadable sector); `Damaged` for a garbled `meta`, no `iloc`, or an `iloc` with no extents. There is no
  `mdat` rule (the data may be in `idat`), but extents the engine cannot check (construction method 1 or 2, an external
  data reference, a zero length meaning "the whole resource", an `iloc` version above 2, more than 65,536 items or
  262,144 extents) make the file at best `Partial`. The pixel data is HEVC (or similar) and is never decoded: `Intact`
  means complete structure with every extent inside the file, not that the image decodes.
  A HEIC is complete once every extent its `iloc` lists (all checkable) ends inside what the walk has covered (ruling
  D27): from there the walk continues only through trailing `free skip wide uuid meta udta mfra` boxes, so a stray
  `moov` after it neither joins it nor turns it into a video, and an unreadable sector right after it is what follows
  the file (it stays `Intact`), as for a video whose `moov` and `mdat` were both seen.
- A sector-aligned file head inside `mdat` is normally "another file starts here". For CR3 and HEIC the starts of the
  file's own data (CR3: the chunks of every track, which include its full-size JPEG; HEIC: the `iloc` extents) are
  skipped, so a JPEG the file itself keeps there does not cut it; so is, for a CR3, a head inside its full-size JPEG or
  `PRVW` (the EXIF thumbnail) as long as that JPEG still decodes strictly (ruling P13, as for TIFF RAWs); any other head
  still does, and a file written over part of an own JPEG breaks its decode, so it cuts the CR3. A foreign file that starts
  exactly at a CR3 chunk or HEIC extent start is indistinguishable from the file's own data and does not cut it. The
  checks need the index to be readable: if `moov`/`meta` is missing or unusable, a head inside `mdat` cuts the file as
  before.
- Own files (`Format::own_files`, ruling D12): a `Partial` CR3's `PRVW` JPEG and full-size JPEG; a JPEG the scan finds
  at one of their starts, wholly inside the CR3, is not reported (see [Scan](#scan)). MP4/MOV (Motion-JPEG frames
  included) and HEIC have none: the frames of a `Partial` Motion-JPEG video are still reported.

## Formats: Fujifilm RAF

`formats::Raf` recognizes the 16-byte magic `FUJIFILMCCD-RAW ` (`FileKind::Raf`, extension `raf`). The header holds
three
big-endian `(offset, length)` pairs at 0x54, 0x5C and 0x64: the embedded JPEG, the CFA header and the CFA (raw sensor)
data.
`confirms` (used for "another file starts here") requires each pair to end within 1 GiB; a head that does not confirm is
not
a file.

- Length: the furthest end of the three blocks (at least the 0x6C-byte header). A block that starts past the end of the
  source is not counted (a corrupted offset never claims the rest of the source), one that starts inside is kept up to
  the end of the source; either makes the file `Partial`. The file also ends at the first 512-aligned sector inside it
  where another file starts (`Partial`), except at the starts of its own blocks and inside its embedded JPEG (the EXIF
  thumbnail) while that JPEG still decodes strictly (ruling P13, as for TIFF RAWs); a file written over part of the JPEG
  breaks its decode, so it cuts the RAF and is reported on its own. A `Partial` RAF's own file (`Format::own_files`) is
  its embedded JPEG: found at its start inside the RAF, it is not reported (ruling D12, see [Scan](#scan)).
- Health: like a TIFF RAW, only the embedded JPEG can be verified. `Intact` needs a complete file, no unreadable sector
  and a strict decode of the JPEG; no JPEG, or one that decodes leniently or not at all, is `Partial`.

## Formats: TIFF-based RAW

`TiffRaw` finds CR2, NEF, ARW, DNG, ORF, RW2 and generic TIFF (kinds `Cr2`, `Nef`, `Arw`, `Dng`, `Orf`, `Rw2`, `Tiff`;
extensions `cr2`, `nef`, `arw`, `dng`, `orf`, `rw2`, `tif`). It recognizes the TIFF magics with an IFD0 offset between 8
and
64 MiB; `confirms` (used for "another file starts here") requires IFD0 to hold at least 3 entries, all of a valid type.

- A head that does not `confirm` is not a file: `measure` returns `None`, so random bytes that look like a TIFF header
  never become (damaged) findings.
- Kind: the `CR` marker at byte 8 gives CR2; the ORF and RW2 magics give ORF and RW2; `DNGVersion` gives DNG; Make
  starting with `NIKON` gives NEF and `SONY` gives ARW; anything else is a generic TIFF.
- Length: the extent from `tiff::walk` (furthest byte any IFD, value or strip references), walked with the end of the
  source as its limit, so structure that continues past the end of the source is a cut file (`Partial`), not a damaged one;
  a strip that starts inside the source is kept up to its end, while one that starts beyond it is not counted (a
  corrupted offset never claims the rest of the source). Position independence holds only
  at the cap: a range or IFD that starts at or past `TIFF_MAX_LEN` (1 GiB) is a structural error (`Damaged`) wherever the
  source ends. Below the cap the source's end still matters: a range that starts past the source's end is a cut
  (`Partial`) and is not counted, while the same range on a longer source is counted as part of the file. The file is cut at the first 512-aligned sector inside it where
  another file starts, except at the starts of its own embedded previews and of its JPEG-coded strips/tiles (Compression
  6, 7 or 34892: a lossy-JPEG tile, such as a DNG's, legitimately begins with FF D8 and confirms; lossless JPEG has no DQT,
  so it never confirms) and inside its own embedded JPEGs (the EXIF thumbnail in a preview's APP1) as long as
  every own JPEG containing that head (at most `JPEG_MAX_LEN` long) still decodes strictly, and at the end of the
  source; either makes it `Partial`. A file written over part of an own JPEG breaks that decode, so it cuts the RAW and
  is reported on its own. A file written exactly at one of those starts is indistinguishable from the RAW's own data
  and does not cut it.
- Vendor layouts (checked against real files from raw.pixls.us). Panasonic RW2: StripOffsets is a placeholder
  (`0xFFFFFFFF`, its StripByteCounts 0 or an unpacked size) and is ignored; the raw data starts at tag 0x0118 and has no
  length tag, so its length comes from the sensor size (tags 0x0002/0x0003) and packing: 16-byte packets of 14 pixels
  (RawFormat 4, 12-bit: DMC-FZ28, DMC-GF1, DMC-LX7, DC-G100) or 9 (RawFormat 5, 14-bit: DC-GH5S), rounded up to
  0x4000-byte blocks. Only packings checked against real files are used: any other (RawFormat 5 at 12 bits, and the
  newer bodies' RawFormat 6 and 7, included) leaves the length unknown and the file `Partial`, but its raw data is still
  claimed from tag 0x0118 up to a safe upper bound (2 bytes per pixel, rounded up to 0x4000 bytes), so the recovered
  file keeps it; the next file inside that bound or the end of the source ends it as usual. A sensor over 10^9 pixels
  or an overflowing size leaves the length unknown with no bound: the file is measured up to its known structure and is
  `Partial`. The preview is tag 0x002E (JpgFromRaw). Olympus/OM System ORF: the thumbnail and the
  preview live in the MakerNote (see the TIFF reader below); the old `OLYMP\0` MakerNote (E-10 era, offsets relative to
  the TIFF) is not read, so those files keep only their other previews.
- Known misread: the Sony DSLR-A100 (2006) ARW comes out `Damaged`, because its SubIFD pointer leads to scrambled data.
- Sony pads some ARW files with zeros after the raw data (to a 4 KiB or 64 KiB multiple). Nothing references that
  padding, so the finding ends at the last referenced byte; the recovered file opens like the original.
- Health: `Damaged` when the IFD structure is malformed (a cycle, an unknown type, and so on). `Partial` when cut, when
  a sector inside is unreadable, when a size cap was hit (`truncated`, for example a huge legitimate DNG), when there is
  no embedded JPEG preview, or when the largest preview (by SOF pixel count, over all previews) decodes only leniently
  or not at all (the raw data may still be fine). If the size of any preview cannot be measured (no SOF, zero size, over
  the decode cap) no smaller one may vouch for the file, so it is `Partial`. `Intact` only when the largest preview
  decodes strictly. There is no RAW decoder, so the raw data itself is never verified.
- Inside an `Intact` RAW the scan suppresses heads, so its embedded previews are not reported separately. A `Partial`
  RAW hides its own JPEGs too (`Format::own_files`: its previews and JPEG-coded strips/tiles, at most 64; ruling D12):
  a JPEG the scan finds at one of their starts, lying wholly inside the RAW, is not reported. A `Damaged` RAW hides
  nothing. See [Scan](#scan).

## TIFF reader (internal)

`tiff::walk` (and `walk_exif_ifd`, for a TIFF whose IFD0 is itself an Exif IFD, as in a CR3's `CMT2`) is the crate-private reader the TIFF-based RAW formats (CR2, NEF, ARW, DNG, ORF, RW2) share. It takes a
`Reader`, the absolute position of the TIFF header and a limit (at most `TIFF_MAX_LEN`, 1 GiB), and returns a `TiffInfo`:
the extent (`end`, relative to the header; the limit clamps every range, whatever references it, and `starts_past_cap` tells a range starting at or past `TIFF_MAX_LEN` from a mere cut), Make/Model/lens/serial/ISO/DateTimeOriginal, the largest image size, whether
`DNGVersion` is present, the embedded JPEG ranges (`JPEGInterchangeFormat`, strips/tiles of Compression 6/7 starting
with FF D8, Panasonic's JpgFromRaw and an Olympus MakerNote's JPEGs) and the other strip/tile ranges (raw data, including
Panasonic's raw data). Header magics `II*\0`, `MM\0*`, `IIRO`, `IIRS`, `MMOR`, `IIU\0`
are accepted; BigTIFF and anything else gives `None`.

It follows the IFD chain, SubIFDs, Exif, GPS (extent only) and Interop IFDs; the next pointer of Exif/GPS/Interop IFDs is
not followed. The data is hostile, so everything is bounded. Each IFD is visited once, a tag counts once per IFD (repeats
are ignored, without raising `garbled`: real cameras occasionally repeat tags), and every value block is read with a single
read. Caps: 64 IFDs, 1024 entries per IFD, 65,536 strip/tile values per tag, 262,144 per walk (which also bounds `data` +
`jpegs`), 16 SubIFDs per tag and 256 bytes per string.

Two flags report trouble. `garbled` is a structural error (cycle, offset or value range past the limit, unknown type, short
value block; the entry is skipped and never extends `end`); within it, `past_limit` marks ranges beyond the limit passed in
(for a caller whose limit is the end of the source, a cut file; a range starting inside is recorded clamped to
the limit, one starting beyond is not counted) and `structural` marks every other error (a malformed
file). `truncated` is a size cap hit (the walk is incomplete, not malformed). Callers make `structural` files `Damaged`
and treat `truncated` as at best `Partial`. First value wins for Make/Model/lens/serial/ISO/date. Each IFD is classified
once, by the SOF of its first strip/tile (they share one Compression): SOF0/1/2 go to `jpegs`; lossless JPEG (SOF3) is
`data`. `jpeg_data_starts` lists the `data` ranges that are JPEG-coded (an IFD with Compression 6, 7 or 34892, or a
JPEGInterchangeFormat range clamped to the limit), the only ones that may begin with FF D8.

Vendor data it reads: in an RW2 (`IIU\0`) IFD0, Panasonic's tags 0x0002/0x0003 (sensor size), 0x000A (bits per sample),
0x002D (raw format), 0x002E (JpgFromRaw, a lossy JPEG goes to `jpegs`) and 0x0118 (raw data offset; the length is computed
with checked arithmetic as described under TIFF-based RAW; an unknown packing or size sets `truncated`, and an unknown
packing with a usable sensor size claims the raw data up to the 2-bytes-per-pixel bound, clamped to the limit;
StripOffsets/StripByteCounts are then ignored). In
an Exif IFD, a MakerNote (0x927C) that starts with `OLYMPUS\0` (IFD at +12) or `OM SYSTEM\0\0\0` (IFD at +16), in the byte
order its header names and with offsets relative to the MakerNote: its thumbnail (0x0100) and the preview of its
CameraSettings IFD (0x2020 → 0x0101 start, 0x0102 length) go to `jpegs` when they lie inside the MakerNote and are lossy
JPEGs. Each MakerNote IFD is read with one read of at most 1024 entries; a MakerNote that does not parse adds nothing and
is not an error (vendor data, not TIFF structure).

## Findings in pieces

A found file may be made of several pieces of the source (a fragmented file: another file was written in the middle of
it). `Fragment { start, len }` (re-exported as `revenant_core::Fragment`) is one absolute range. `Measurement`, `Finding`
and `Preview` carry `fragments: Vec<Fragment>` with one contract: empty for a contiguous file; otherwise at least 2 pieces
in file order, the first starting at `start`, their lengths summing to `len`, no two overlapping. `start` is always the
first piece's start and `len` the total length. `Finding::ranges()` and `Preview::ranges()` give `[Fragment { start, len }]`
for a contiguous file, else the pieces. A measurement that breaks this contract (or has a piece outside the
source) is a detector bug: debug builds assert on it, release builds ignore it (no finding). `formats::Jpeg` produces two
pieces for a photo it puts back together (see "Fragmented JPEG"); the scan copies a measurement's `fragments` into its
finding. After an `Intact` finding in pieces, the scan skips heads only inside
its pieces (each rounded out to whole sectors), so files in the gaps between them are still found.

## File systems: volumes

`filesystem::volumes(source)` lists every FAT12, FAT16, FAT32 and exFAT volume of a source, in offset order (at most 16,
the lowest offsets), as `Volume { index, fs, offset, len, sector_size, cluster_size, heap_offset, cluster_count, label,
serial, from_backup, scheme }`. Offsets are absolute; `len` is what the boot sector says and may run past the source's end;
`cluster_offset(c)` is the absolute offset of cluster `c`, `None` outside `2 ..= cluster_count + 1`. `FsKind::as_str()`
gives `"fat12"`, `"fat16"`, `"fat32"`, `"exfat"`; `Scheme::as_str()` gives `"none"`, `"mbr"`, `"gpt"`. It only reads, and
a broken table or boot sector is never an error: it yields fewer volumes. Written from the public specifications (MBR/EBR
conventions, the UEFI GPT chapter, fatgen103, the Microsoft exFAT specification).

Layouts, tried from sector 0:

- **Superfloppy:** sector 0 is itself a valid boot sector (scheme `none`).
- **GPT:** an MBR entry of type `0xEE`. The header at LBA 1 (512-byte blocks, else at byte 4096 for 4096-byte blocks)
  must have the signature, its own LBA, a header CRC32 that holds, at most 128 entries of 128..=1024 bytes (a multiple of
  128) and an entry-array CRC32 that holds; otherwise the backup header at the primary's AlternateLBA, then at the last
  LBA, is checked the same way. Every entry with a non-zero type GUID and first LBA ≤ last LBA is a candidate. When no
  header checks out (both damaged) and the MBR is a hybrid one, its other (non-`0xEE`) entries are read as an MBR.
- **MBR:** each of the 4 entries with a non-zero type, start and size; an extended entry (`0x05`, `0x0F`, `0x85`) is
  followed through its EBR chain (logical partitions relative to their EBR, links relative to the extended start). A link
  already seen, outside the source, unreadable or without `55 AA` ends a chain; at most 64 EBRs are read in all. LBAs are
  read as 512-byte sectors, and again as 4096-byte sectors (a 4Kn disk; ruling R3): a partition found that second way
  (also through its EBRs) is kept only when its boot sector declares 4096-byte sectors, and an offset found both ways is
  read the first way.
- **Probe:** when no candidate gave a volume (table destroyed, or no table), the sectors 0, 63, 128, 2048, 8192, 32768 and
  65536 are tried (scheme `none`). A probe can land on a volume's backup: sector 6 of a FAT32 (a FAT32 partition at 2042)
  or the backup region at 12 of an exFAT, both valid boot sectors of the same geometry. So when the primary at a probe
  is FAT32 or exFAT and the volume that many sectors earlier (6 or 12, in its own sector size) has the same boot sector
  (its primary, or this one as its backup), the earlier volume is reported if its primary is there, or if its first FAT
  starts where its boot sector says (FAT entry 0 = the media byte then `FF FF`) and the probed one's does not.

Ghosts (1.4 T3): an MBR entry read with 4096-byte sectors can land on the backup boot sector of a 4K volume already
found (sector 6 of a FAT32, the backup region at 12 of an exFAT: an entry at the volume's LBA + 6 or + 12). Such a
candidate, the same file system with the same serial at the found volume's offset + 6 or 12 of its sectors, is dropped:
the volume is reported once. Probe limit: when a probe offset is the backup boot sector of a volume whose primary boot
sector and first FAT sector are both gone, nothing is left to tell the two apart, and the volume is reported at the probe
offset (6 or 12 sectors past its real start).

The partition's own record of where the volume sits (FAT `BPB_HiddSec`, exFAT `PartitionOffset`) is never used for
placement: a volume is where its table entry (or probe) points. The legacy variant in which a logical partition's
`BPB_HiddSec` counts from its own EBR instead of from the start of the disk is not supported: nothing here reads it, and
the testkit writes the absolute value for logical partitions too, as current formatters do.

A FAT boot sector is valid with `55 AA` at 510, a jump (`EB ?? 90` or `E9`), BytsPerSec 512/1024/2048/4096, SecPerClus a
power of two ≤ 128, RsvdSecCnt ≥ 1, NumFATs 1 or 2, a data region starting before the end, and at least one cluster. It
is FAT32 by its BPB (FATSz16 = 0, RootEntCnt = 0, FATSz32 > 0, RootClus ≥ 2, and RootClus inside the heap); otherwise it
needs FATSz16 ≠ 0 and RootEntCnt ≠ 0 (fatgen103: a FAT12/16 BPB has both, so a FAT32 with a corrupt RootClus is refused,
not read as FAT16, and its backup is used) and is FAT12 below 4085 clusters, FAT16 below 65525, and refused above. An exFAT boot sector is valid with `"EXFAT   "` at 3,
bytes 11..64 zero, `55 AA`, BytesPerSectorShift 9..=12, SectorsPerClusterShift ≤ 25 − that, NumberOfFats 1 or 2,
FatOffset ≥ 24, ClusterHeapOffset ≥ FatOffset + FatLength × NumberOfFats, ClusterCount 1..=2^32 − 11, the heap ending
within VolumeLength, and the root cluster inside the heap.

Backups: when the primary boot sector is not valid, the exFAT backup boot sector (sector 12) and the FAT32 one (sector 6)
are tried, for 512- and then 4096-byte sectors, each only with its own sector size (`from_backup: true`); an exFAT
backup whose boot checksum (sector 23) holds is preferred. An exFAT primary whose fields are valid but whose boot
checksum (sector 11) does not hold is replaced by an exFAT backup whose checksum holds, and kept when there is none.

Label: FAT, the volume-label entry among the root's first 16 entries, else the BPB label (only with the extended boot
signature `0x29`, which also gives the serial); printable ASCII is kept, any other byte becomes `_`, trailing spaces are
dropped. exFAT, the `0x83` entry among the first 4 KiB of the root's first cluster (up to the end-of-directory entry).
An empty label or `"NO NAME"` is `None`.

Bounds: at most 16 volumes, 64 EBRs, 128 GPT entries. One call makes at most 1,362 reads of at most 16,588,800 bytes in
all (a test counts them on every hostile case):

- tables: sector 0, at most 6 GPT headers each with its entry array (≤ 128 KiB), 64 EBRs: 77 reads, 833,536 bytes;
- at most 128 candidate offsets from a table (72 from an MBR), each at most 9 reads and 111,104 bytes: the primary, its
  exFAT checksum region (12 sectors of ≤ 4 KiB), 4 backup boot sectors and 2 backup checksum regions, then one label read
  (≤ 4 KiB). An offset that is not a volume costs five 512-byte reads (only a valid exFAT field set costs a checksum
  region), and one refused for its sector size skips the label read;
- when probing, 7 offsets, each at most 19 reads and 219,136 bytes (the offset and the one 6 or 12 sectors earlier, the
  two FAT entry-0 sectors, a label).

No read is larger than 128 KiB, and none is allocated before it is known to lie inside the source. A typical card (one
FAT32 partition at LBA 8192) costs 8 reads, pinned by a test (`typical_card_reads_few_sectors`) so a change that reads
more shows up.

## File systems: FAT

`filesystem::read(source, cancel)` returns a `Listing` of the live and deleted files of every FAT12/16/32 and exFAT
volume `volumes` finds (exFAT: see [File systems: exFAT](#file-systems-exfat); a volume of more than 2^28 clusters is
not walked). It never fails: a broken volume yields fewer files. Cancellation is checked before each directory; a cancelled
read returns what was listed so far. Written from fatgen103 (FAT entries, directory entries, long names). The scan
calls the crate-internal `read_with(meta, probe, cancel)`: metadata through its cache, the FAT32 high-word
heads through a `Probe` over the raw source (see Deleted files); `read` uses the given source for both.

- `Listing.files`: the photo/video files, those whose extension `FileKind::from_extension` knows (jpg jpeg jpe, mp4 m4v,
  mov, cr2, cr3, nef nrw, arw, dng, orf ori, rw2, raf, heic heif hif, tif tiff; any case). Each is an `FsFile`:
  - `origin`: `volume` (the volume's index), `folder` (components joined by `/`, from the root, no leading `/`, `""`
    for the root; `None` when dropped, see Bounds; one string shared by the files of a folder), `name`, `state` (`live`,
    or `deleted`), `placement` (`chain` for a live file, `guessed` for a deleted one; exFAT adds `recorded`; a scan
    finding's origin may say `carved`, see [Deduplication](#deduplication)), `size` as
    recorded, `created` and `modified` as `FsTime` (local time
    `YYYY-MM-DDTHH:MM:SS`, no offset on FAT, exFAT's UTC offset when recorded; `Display` appends `+HH:MM`/`-HH:MM`
    then; `None` when a field is out of range: month, day for that month and year,
    hour, minute, seconds, or a creation 10 ms count above 199). The creation time adds its 10 ms count, floored to the
    second. `name()`, `folder()` (an `Option`) and `path()` (`folder/name`, or the name alone) read them.
  - `pieces`: absolute byte ranges in file order, consecutive clusters joined, holding the first `size` bytes; a piece
    past the end of the source is cut or dropped, and a file with no piece left is not listed.
  - `trusted` (ruling D3): a live file's chain is complete (exactly `ceil(size / cluster)` clusters, the last one
    end-of-chain), with no cycle and no cluster another chain holds, in at most 4096 runs; a deleted file's guess is
    one run of free clusters, as many as its size needs. Either way it says where the bytes probably are, never that
    they are intact: health comes from the detectors (an untrusted file is at most partial, D3).

  Sorted by (first piece start, volume, path). Several entries, live or deleted, may share a first cluster: all are
  listed (the scan picks one).
- `Listing.others`: the first piece of every other live file (any other extension, or none), sorted by start. Such a
  file keeps no name or path. A deleted file of another kind is not listed anywhere.
- `Listing.live`: the merged ranges of every live file (its pieces) and every trusted live directory (its clusters);
  never a deleted file or directory.
  The fixed FAT12/16 root region is not in it, nor is a directory that is not trusted (ruling R8), nor a file or
  directory left out by the piece or photo cap.
- `Listing.truncated` (ruling R7): a cap or budget dropped or degraded something: a file left out by the piece or photo
  cap, a folder dropped by the path cap or budget, a volume whose contested set overflowed, a directory not entered for
  the directory or depth cap, entries not read for the entry cap or a directory chain running past 2 MiB, a volume
  too large to walk, deleted files not listed because the volume's guess steps or the listing's high-word probes ran
  out, or deleted folders not walked because 65,536 were already waiting.

The walk:

- **Names.** A long name is used only when its parts come right before the short entry, ordinals counting down to 1 from
  the part marked last (`0x40`), every part carrying the short name's checksum; its text is the UTF-16 up to the first
  `0x0000`, decoded lossily. Otherwise the 8.3 name: base and extension trimmed, joined by `.` when the extension is not
  empty, lower-cased as the NT bits of byte 12 say (`0x08` base, `0x10` extension), a first byte `0x05` read as `0xE5`,
  any byte outside printable ASCII read as `_`. In every component `/` and NUL become `_`, and an empty one is `_`, so
  `name()`/`folder()` never split one wrongly. Deleted entries (`0xE5`) are read as described under Deleted files;
  the volume label, `.` and `..` are skipped; `0x00` ends a directory. An entry is a long-name part when its attribute byte masked with `0x3F` is `0x0F`
  (fatgen103's mask).
- **Chains.** The first cluster is `hi << 16 | lo` on FAT32, `lo` otherwise. FAT12 entries are packed 12 bits, FAT16 16,
  FAT32 the low 28 bits; end-of-chain is ≥ `0xFF8`/`0xFFF8`/`0x0FFFFFF8`, bad is `0xFF7`/`0xFFF7`/`0x0FFFFFF7`, and a
  value outside the heap is invalid. A chain is followed until end-of-chain, a free, bad or invalid entry, a cluster
  outside the heap or already claimed, `ceil(size / cluster)` clusters, or 4096 runs. A sector of the first FAT that
  cannot be read is read from the next copy; one no copy gives stops the chain there. The FAT32 `ExtFlags` field is not
  honored: with mirroring disabled and an active FAT other than 0, the first FAT is still read first, and the others
  only stand in for its unreadable sectors.
- **Claims.** Every live directory and file claims the clusters of its chain, in walk order (depth first, entries in
  on-disk order). A chain that reaches a cluster another chain claimed stops there and the cluster becomes contested;
  once the walk of the volume ends, every file and directory holding a contested cluster is untrusted. Reaching a
  cluster of its own chain is a cycle: the chain is incomplete, nothing is contested. When more than 65,536 clusters
  would be contested, every live file and directory of that volume is untrusted (ruling C11).
- **Directories.** The root is the fixed region (FAT12/16) or the `RootClus` chain (FAT32). A directory entry whose first
  cluster is outside the heap or already claimed (a loop back to an ancestor, itself, or a second link) is not entered.
  A directory is trusted when its chain ends in end-of-chain within 2 MiB, with no cycle and no contested cluster (ruling
  C10); one that is not trusted is still walked, as far as its chain goes. An unreadable cluster or sector of a
  directory is skipped.

### Deleted files and folders

A deleted entry (byte 0 `0xE5`) keeps its size, times and first cluster; its chain was freed. Only deleted
photos/videos (by extension) of size > 0 are listed, with `state` `deleted` and `placement` `guessed`; they claim
nothing and never enter `live` or `others`.

They are placed only once the volume's live walk is done (ruling R10): every live chain has claimed its clusters by
then, and a claimed cluster is in use for the guesses whatever its FAT entry says (a live chain broken or cross-linked
there). And the live files take the listing's caps first (photo slots, pieces): a deleted file is listed only with
what they left, so deleted entries never push a live file out of `files` or `live`. While the live tree is walked,
the deleted photos/videos wait, never more than the photo slots left (a later live photo drops the last one waiting).

- **First letter (ruling D9).** Byte 0 of the short name is lost. The deleted long-name parts right before the short
  entry (attribute `0x0F`, byte 0 also `0xE5`, so their ordinals are lost too) that carry the nearest part's checksum
  are taken nearest first, at most 20, and read up to the first `0x0000`; when their first character, upper-cased
  (ASCII), as byte 0 makes the short name's checksum match theirs, that byte is restored and the long name is the
  file's name. Otherwise byte 0 is the most common first byte among the short names with the same bytes 1, 2 and 3 —
  live ones, or deleted ones already restored — first in the same directory, then in the whole volume (ties: the
  lowest byte), formatted like the rest of the 8.3 name; otherwise it is `_`. The volume-wide count is taken once the
  live tree is walked (so a later folder's names count), and the names still unknown are settled then, in walk order.
  A long name whose first character is not the short name's byte 0 (a name starting with `.`, `+`, a space or an
  accented letter, all of which the 8.3 name replaces or drops) fails the checksum, so its text is not used: the
  file gets its 8.3 name, first letter from the sibling rule.
- **Placement (ruling D4).** `need = ceil(size / cluster)`. Nothing is listed when the first cluster is outside the
  heap or allocated today (overwritten). From it, clusters free today are taken in order, allocated (or unreadable)
  ones stepped over, until `need` were taken, the heap ends, `2 × need + 1024` clusters were examined, or 4096 runs.
  Fewer than `need` (short): the pieces hold what was found. Stepping over a cluster, or more than one run, makes the
  file untrusted.
- **FAT32 high word (ruling D10, C2, C3).** Windows zeroes a deleted FAT32 entry's high cluster word. On a FAT32
  volume with clusters past `0xFFFF`, a deleted file whose recorded high word is 0 has as candidates `lo` and every
  `h << 16 | lo` (`h ≥ 1`) inside the heap (at most `last >> 16`, ≤ 4096); a candidate counts when it is free today and
  the format of the extension's kind (JPEG; ISO BMFF for mp4/mov/cr3/heic; TIFF RAW for cr2/nef/arw/dng/orf/rw2/tif;
  RAF) recognizes the first 16 bytes of its head. Exactly one candidate: it is the first cluster. None or several:
  the file is not listed. Files with the same `lo` and format share one answer (ruling R10). The checks run by high
  word, every open question in one round, so a round looks at one 256 KiB stretch of the FAT and the FAT is read
  once, in order; each candidate looked up costs a step of the volume's budget (below), paid before the round, and
  cancel is checked before each round and every 1024 lookups. Every head check is one read of 512 bytes through the `Probe` (outside the scan's cache);
  the probe refuses, without reading, a range the cache already knows to be unreadable, and a refused or failed read
  recognizes nothing. At most 65,536 checks per listing (32 MiB); once spent, no more candidates are tried and a file
  whose candidates were not all checked is not listed. Cost (D10): a camera-deleted file whose head was overwritten
  by noise may take the name of another photo that sits at its only recognized candidate.
- **Deleted folders.** A deleted directory met in the live tree waits (at most 65,536 per volume) until the live walk
  ends; then each is walked depth first, in the order met. Its clusters are the D4 guess for 2 MiB (read as needed,
  up to its end-of-directory entry), and it is read only when its first cluster starts with its own `.` entry (for a
  FAT32 directory whose recorded high word is 0 only `lo` is tried, so a folder Windows deleted on FAT32 past cluster
  65,535 is lost, with the files in it; the scan still finds them without names). Every entry in it is deleted, whatever its byte
  0 says; deleted directories inside it are walked too (never twice the same first cluster). It never claims and never
  enters `live`. The directory, depth and entry caps apply as for live ones; a waiting directory counts against the
  directory cap once it is walked, so deleted folders never take room from the live tree.
- **Step budget (ruling C7, R10).** Every cluster examined by the guesses of a volume (deleted folders, then the
  high-word candidates, then the files) costs one step of 2^26; once spent, the volume's remaining deleted files are
  not listed and no further deleted folder is read (in that order, each in walk order). The allocation is read a FAT
  block at a time, so 2^26 steps of guessing take well under a second; FAT32 high-word lookups are dearer per step,
  and a hostile volume that spends the whole budget on them takes about 10 s on the development Mac (a real card asks
  a few of them per deleted file).

Bounds (each lowerable in unit tests): per volume at most 65,536 directories, 4,000,000 directory entries (every
32-byte slot read counts, the end marker included), depth 32 (the root is 0), 2 MiB per directory, 65,536 contested
clusters; per listing at most 262,144 photo/video files (live files of every volume first, ruling D28: when the cap is
full, a live photo/video takes the slot of the last deleted one listed, which is then dropped and the listing marked
truncated; the price is that on a card at the cap the deleted files of later volumes lose their name, though the
carving still finds them) and `MAX_PIECES` (2^20) pieces, counting a file's runs (a
photo/video's pieces, an other file's ranges in `live`) and a directory's runs. A file that would pass either cap is not
listed at all (not in `files`, `others` or `live`); a directory past the piece cap is still walked but its ranges are not
in `live`. Once the entry cap is reached the walk of the volume stops: no further directory is read. Following chains
examines each cluster of a volume at most once, except the clusters of a directory, which are followed twice (the
chain is followed again through the FAT when the directory is read); if a flaky source changes between the two, the directory can
keep the chain it claimed while losing its trust (never the other way round: trust is never given on a re-read). The
deleted-file guesses examine at most 2^26 clusters per volume, and
the high-word checks read at most 65,536 heads per listing.

Paths (ruling R6): a photo/video whose path (folder, `/`, name) would pass `MAX_PATH_BYTES` (4096 bytes) keeps its name
and loses its folder, and so do the files under a directory whose own path would pass it. Folder strings are held once
per directory and shared by its files; the listing pays for each from a 64 MiB budget, and once that runs out new
directories get no folder (their files are listed without one).

Memory (64-bit sizes, allocator overhead not counted). A name is at most 20 long-name parts of 13 UTF-16 units, each
decoded to at most 3 UTF-8 bytes: 780 bytes, allocated exactly. When a volume's walk ends, each pending file is
consumed and its origin moved into the listing, never copied, so every name is held once.

- **Retained, per listed photo/video:** the `FsFile` (152 B) plus its name (≤ 780 B), two time strings (19 B each)
  and 16 B per piece. At the 262,144-file cap: ≤ 780 B × 262,144 ≈ 195 MiB of names, ≈ 242.5 MiB in all.
- **Retained, per other file:** 16 B in `others` plus its ranges in `live`; it keeps no name or path.
- **Per piece:** the 2^20 piece budget bounds the pieces and `others` together (≤ 16 MiB) and `live` (≤ 16 MiB).
- **Folders:** ≤ 64 MiB, plus a 16 B shared-string header per folder (≤ 16 MiB).
- **While a volume is walked, per pending file:** 48 B, plus 8 B per run (Vec capacity, so up to 16 B). A photo/video
  also has a boxed origin of 120 B that holds its (retained) name. With at most 2^20 pending entries: ≤ 48 MiB + 16 MiB
  + 30 MiB.
- **While a directory is read:** its entries are held at once, 48 B each plus the name. With long names that is at
  most 3,120 entries (21 slots each in 2 MiB): ≈ 2.5 MiB. With short names, 65,535 entries of ≤ 12-byte names:
  ≈ 3.8 MiB, ≈ 4.9 MiB at the peak (a growing `Vec` briefly holds the old and the new buffer). On top of that, one cluster (≤ 512 KiB) or the FAT12/16 root region (≤ 2 MiB) is read at a time.
- **The walk itself:**
  - each directory waiting on the stack keeps only its folder, depth and (first cluster, cluster count), 56 B, so
    65,536 of them take ≈ 3.5 MiB; its chain is followed again through the FAT when it is read;
  - the claims bitmap: ≤ 32 MiB at 2^28 clusters;
  - the contested set: ≤ 65,536 clusters, ≈ 1.5 MiB;
  - the FAT cache: ≤ 8 MiB;
  - deleted entries (all freed before `finish`, never at the same time as the listing being built):
    - the first-letter count (D9): while the live tree is walked, 4 B per known short name (≤ 16 MiB at
      4,000,000 entries, ≤ 32 MiB of capacity), then 8 B per distinct (bytes 1–3, byte 0) pair (≤ 32 MiB), the
      first freed once the second is built; names counted inside deleted folders, ≈ 16 B each. The entry cap is
      shared, so the whole count is ≤ ≈ 64 MiB;
    - the deleted photos/videos waiting to be placed: 72 B each besides their names, ≤ 262,144 of them with the
      live photos (≤ 18 MiB; their names count among the 262,144 names above, not on top);
    - the high-word questions: ≤ ≈ 100 B per deleted file whose high word reads 0 (≤ 25 MiB);
    - the deleted folders waiting (≤ 65,536 × 56 B ≈ 3.5 MiB) and those walked (≤ 65,536 first clusters); the
      names still to settle (16 B each); one directory's count (≤ 65,536 × 8 B); one guess (≤ 4096 runs, 64 KiB).

    Measured with a counting allocator (peak heap growth during `read`, release build, a throwaway probe binary):
    4,000,000 live short names with distinct bytes 1–3: 47.5 MiB; 4,000,000 entries inside 62 deleted folders:
    65.4 MiB; 190,000 deleted JPEGs with 255-character long names, high word 0 (probe cap reached): 77.9 MiB; the
    same deleted by a camera, 174,805 listed: 110.2 MiB peak, 92.4 MiB retained.

Combined worst case at the default caps: ≈ 355 MiB retained (deleted files share the photo and piece caps, so they
add nothing). The peak, which adds every transient above at once (including the brief overlap of the old and new
buffers while `files` grows, ≈ 19 MiB, and while `live` is merged, ≈ 16 MiB, and the deleted-entry tables,
≤ 64 + 18 + 25 ≈ 107 MiB), is ≈ 640 MiB. It is a sum no card reaches: the deleted-entry tables are freed before
`finish` builds the listing, and the entry cap keeps the first-letter count and 262,144 long names from coexisting. Before origins were moved, the names alone were held twice at the end of each volume
(+195 MiB).

Reads: metadata, through the given source: the boot sector again (to locate the FATs and the root), the FATs in
32 KiB blocks cached up to 8 MiB (a block whose read fails is read again sector by sector, from each copy in turn; a
block may be read again after the cache is emptied), each directory cluster once (at most 512 KiB per read; a deleted
folder's clusters too), and the FAT12/16 root region once (at most 2 MiB). Besides: up to 65,536 reads of 512 bytes
(32 MiB) for the FAT32 high-word checks, through the `Probe` (the scan's raw source, outside the cache, never a range
the cache knows to be unreadable); the only exception to reading each byte once.

## File systems: exFAT

exFAT volumes go into the same `Listing` as FAT ones, with the same fields, caps, path rules, truncation flag and
sorting (see [File systems: FAT](#file-systems-fat)); what differs is below. Written from Microsoft's exFAT
specification (FAT §4.1, allocation bitmap §7.1, up-case table §7.2, entry sets §6.3, File §7.4, Stream Extension §7.6
and File Name §7.7 entries).

- **Root and bitmap.** The root is the FAT chain from `FirstClusterOfRootDirectory` (at most 256 MiB; trusted, and in
  `live`, when it ends there with no cycle or cross-link, as a FAT directory). Its first allocation-bitmap entry
  (`0x81`, BitmapFlags bit 0 clear) is read as soon as it is met: FirstCluster (byte 20) inside the heap, DataLength
  (byte 24) at least `ceil(ClusterCount / 8)` and at most that rounded up to whole clusters, its FAT chain holding
  those bytes, every byte readable. Otherwise there is no bitmap (below). The bitmap's and the up-case table's
  (`0x82`) clusters are claimed like a live chain. The live photos/videos and directories of the root met before the
  bitmap entry (their trust needs it) wait for it, at most as many photos/videos as the listing has photo slots left
  and 65,536 directories (past either they are dropped and the listing is truncated); any other live file there is
  handled at once.
- **Entry sets.** A File entry (`0x85`; `0x05` deleted) with SecondaryCount (byte 1) in 2..=18, then a Stream
  Extension (`0xC0`/`0x40`), then `ceil(NameLength / 15)` File Name entries (`0xC1`/`0x41`), then any other secondary
  entries the count holds (skipped). NameLength (stream byte 3) is 1..=255 and `1 + ceil(NameLength / 15)` must fit
  in SecondaryCount. The SetChecksum (bytes 2–3 of the File entry) is computed over all `SecondaryCount + 1` entries,
  bytes 2–3 skipped, with every type byte's InUse bit (`0x80`) set first, so a deleted set keeps the checksum of its
  live form; a set that breaks any of these rules is skipped (an entry that breaks a set is read again as the
  possible start of the next one). `0x00` ends a directory; an unreadable stretch of a directory is skipped, and a
  set across it is lost. Names are the UTF-16 units decoded lossily, `/` and NUL becoming `_`.
- **Fields.** File entry: FileAttributes (byte 4, `0x10` directory); created and modified timestamps (bytes 8 and 12:
  the DOS date in the high 16 bits, the time in the low 16), their 10 ms counts (bytes 20 and 21, added and floored to
  the second; above 199 the time is `None`), their UTC offsets (bytes 22 and 23: bit 7 valid, bits 0–6 signed
  15-minute units, `utc_offset_minutes`). Stream: GeneralSecondaryFlags (byte 1: bit 0 AllocationPossible, bit 1
  NoFatChain), ValidDataLength (byte 8), FirstCluster (byte 20), DataLength (byte 24, the `size`). An entry without
  AllocationPossible or with DataLength 0 has no clusters and is not listed.
- **Live files and folders.** NoFatChain: one run of `ceil(DataLength / cluster)` clusters from FirstCluster, claimed;
  otherwise the FAT chain is followed for that many clusters (as on FAT32, but the whole 32-bit entry:
  `0xFFFFFFF7` bad, `0xFFFFFFF8` and above end). Claims, cross-links and the contested set work as on FAT (a run that
  reaches a claimed cluster stops there). A live file is trusted when its run or chain is complete with no conflict,
  every cluster of a NoFatChain run is in use in the bitmap, and ValidDataLength equals DataLength. A directory's
  DataLength is its size, at most 256 MiB (past it only 256 MiB are read, the directory is not trusted and the listing
  is truncated); it is trusted when every cluster that size needs is claimed with no conflict (a FAT chain ending
  there, or a NoFatChain run the bitmap shows in use). A directory whose first cluster is outside the heap or already
  claimed is not entered.
- **Deleted files and folders.** A set is deleted when its File entry's InUse bit is clear, or a folder above it is
  deleted. The File entry decides for the whole set: the InUse bits of its Stream Extension and File Name entries are
  not looked at, so a set whose bits disagree (the File entry in use, a name entry not, or the reverse) is live or
  deleted as its File entry says. Deleted photos/videos wait, and are placed only once the live walk is done, against the live claims (a
  claimed cluster is in use whatever the bitmap says; ruling R10), with what the live files left of the caps:
  - NoFatChain: the run as recorded (`placement` `recorded`, cut at the heap's end); trusted only when every cluster
    of it is free in the bitmap today;
  - FAT-chained: its old FAT chain (exFAT frees clusters in the bitmap only) when it has exactly
    `ceil(DataLength / cluster)` clusters, all free today, the last one's entry end-of-chain (`guessed`, trusted only
    when it is one run); otherwise the FAT rule D4 guess (`guessed`, trusted only when one run and not short);
  - a first cluster outside the heap or in use today: not listed (overwritten);
  - ValidDataLength short of (or past) DataLength: not trusted.

  Deleted folders met in the live tree wait, and are walked once it is done, depth first, never twice the same first
  cluster: placed the same way (at most 256 MiB), a recorded run read only as far as its clusters are free today.
  The old chains, the bitmap checks and the guesses spend the volume's budget of 2^26 cluster examinations (one per
  cluster looked at; a bitmap check stops at the first cluster in use); once spent, the remaining deleted files are
  not listed and no further deleted folder is read, and the listing is truncated. Cancel is checked before each
  directory and every 1024 placements.
- **No bitmap** (missing, unreadable, or out of bounds): every cluster no live chain claims reads free, so the
  placements see the live claims only, and every deleted file is untrusted. Live files skip the bitmap check.

Bounds: as on FAT (65,536 directories and 4,000,000 entries per volume, depth 32, the listing's caps), with 256 MiB per
directory instead of 2 MiB. The work is linear in what is read: each cluster is claimed at most once, the bitmap is
counted 64 clusters at a time, and every deleted placement is paid from the step budget.

Memory (on top of FAT's figures, which hold for the listing itself). The bitmap: one bit per cluster, ≤ 32 MiB at 2^28
clusters, freed with the walk. A directory is read 128 KiB at a time and parsed entry by entry, holding one entry set
(≤ 608 B) besides the chunk, so a 256 MiB directory never sits in memory. The root's entries waiting for its bitmap
entry (formatters write it first, so normally none) take 64 B each plus the name: at most 262,144 photos/videos
(16 MiB; their names are the ones the listing keeps, not extra) and 65,536 directories (4 MiB plus names of
≤ 765 B, ≤ 48 MiB), so ≤ 68 MiB in the worst case, freed once the bitmap entry is met or the root ends. A name is
at most 255 UTF-16 units, ≤ 765 bytes of UTF-8. Deleted photos/videos waiting to be placed: ≈ 100 B each besides
their names, ≤ 262,144 of them with the live photos; deleted folders waiting: ≤ 65,536 × 64 B.

Reads: the boot sector again, the FAT in 32 KiB blocks cached up to 8 MiB (sectors no FAT copy gives stop a chain),
the bitmap once (≤ 128 KiB per read), each directory cluster once (≤ 128 KiB per read; a deleted folder's clusters
too), all through the given source. exFAT has no high-word check: no read goes through the `Probe`.

## File systems: after a quick format

A quick format writes a new boot region, new FATs (exFAT: a new bitmap and up-case table) and an empty root, and leaves
everything else as it was: the old folders' clusters still hold their entries, with each file's name, size, times and
first cluster. The old root is gone, so the top folders' names are lost. The scan (`scan::scan`, with
`ScanOptions::filesystems`) uses those old folders to give the photos and videos it finds by their signature their old
names back (ruling D11). They only ever name findings; they never place data, so a file whose head no format
recognizes does not come back through them.

- **Which clusters.** While reading its blocks, the scan looks at every cluster start of every volume found (on the
  volume's grid as it is today: a quick format with the same geometry keeps the old grid). An old FAT folder starts
  with its `.` entry (11 bytes `".          "`, the directory attribute) then its `..` entry (`"..         "`, the
  directory attribute), whose cluster names its parent (0 for the root); an old exFAT folder starts with an entry set:
  a File entry (`0x85`, or `0x05`) with SecondaryCount 2..=18 whose whole set, within the cluster's first 608 bytes,
  holds together with a valid SetChecksum (the InUse bits forced, as for deleted sets). Not counted: a cluster start
  inside the listing's live ranges or inside any directory the listing walked (live or deleted, trusted or not), and,
  as for orphan `moov` candidates, one whose head (64 bytes on FAT, up to 608 on exFAT) overlaps a committed skip range
  (those blocks may not be read at all). At most 8192 per block (counted only for a block skip ranges do not wholly
  cover) and 65,536 per scan (the first ones in commit order), 16 B each.
- **A folder longer than a cluster (exFAT).** A later cluster of an old exFAT folder whose first bytes happen to
  start a whole entry set is a candidate of its own: the first cluster's reading stops there, and the files listed
  from it on come out under a folder no entry names, as `?/<name>` (FAT continuation clusters never start with `.`, so
  they are never candidates).
- **Which findings.** Every finding emitted without origin that starts on a cluster start of a volume is kept (its id,
  start, length, kind, health and pieces, ≈ 64 B; at most 262,144).
- **After the search and the pairing,** each old folder is read, in offset order, through the scan's cache: its first
  cluster, then the next ones while they lie in the heap, are no other old folder's start, are neither live nor walked,
  can be read and still read as directory entries (FAT: up to the first free entry, long-name parts with a cluster
  word 0 and short entries with printable name bytes, no reserved attribute bit and no `.`; exFAT: known entry types),
  at most 2 MiB. Entries are parsed with the live walk's rules (long names with their checksum, exFAT entry sets with
  their SetChecksum, `/` and NUL in a name become `_`); erased FAT entries and deleted exFAT sets are left out (their
  names are not whole, and their clusters may have been reused). The volume's entry cap (4,000,000) counts these
  entries too.
- **Folder names.** A folder entry gives an old folder its name when its first cluster is that folder's (FAT: and the
  folder's `..` names the folder holding the entry; exFAT has no `..`): the first such entry in offset order. A folder
  no entry names is `?`, so a photo of `DCIM/100MSDCF` whose root entry for `DCIM` is gone comes out as
  `?/100MSDCF/DSC01234.JPG`. At most 32 levels are followed up (and a loop stops): past them the top is `?`. Paths obey
  the listing's caps (ruling R6): a folder string longer than 4096 bytes, or one past the 64 MiB folder budget, is
  dropped (the file keeps its name, `folder()` is `None`), and so is a folder whose path with the file's name would
  pass 4096 bytes.
- **Which name.** A file entry with a photo/video extension and a size names the finding without origin that starts
  at its first cluster with the same kind (MP4 and MOV count as one). When several entries name one finding (a file
  copied to two folders, a cross-link), the latest `modified` wins (a missing one loses), then the lowest folder
  offset.
- **The update.** The finding gets `origin: Some(FsOrigin { state: Orphaned, placement: Carved, .. })` with the entry's
  volume, folder, name, recorded size and times, and comes out again as an `Event::Updated` with the same id. An
  `Intact` finding named by an entry of another size is at most `Partial` when the finding is longer than the size, or
  when the bytes from its end (the end of its last piece) up to that size, contiguous and inside the source, are not
  coherent (the tail check of ruling D6: padding, or up to 8 whole files then padding). Health is otherwise unchanged:
  the old folder is evidence of a name, never of integrity. A finding the pairing also put back together comes out
  once, merged (its two pieces and its origin). All `Updated` events, of the pairing and of the old folders, come out
  one per id, in id order.
- **Truncation.** When a cap drops old folders (per block or per scan) or kept findings, when the entry cap stops the
  old folders' parsing, or when the path cap or budget drops a folder, `ScanSummary::listing_truncated` is set (ruling
  R7; the `Volume` events, emitted before the search, cannot carry it).

Reads: each old folder's clusters once, through the cache, after the search (so read again from the source when the
cache no longer holds them); a folder's clusters stop at the next old folder of any volume, so within a volume no
cluster is read for two folders. The tail check reads, through the cache, the bytes between a named finding's end and
its recorded size.

Memory: the listing's `walked` ranges (merged runs of the directory clusters the walks read, at most 2^20 runs of
16 B per volume before merging, so ≤ 16 MiB per volume; a directory read in one run costs one), the candidates (16 B
each, ≤ 65,536 kept, ≤ 131,072 per block read ahead before the block cap) and the kept
findings (≈ 64 B each plus their pieces, ≤ 262,144) during the search; afterwards, per old folder its name and the
folder string (each folder held once, ≤ 64 MiB of folder strings), one 2 MiB read at a time, and one best entry per
kept finding (≈ 150 B plus its name).

## Inspect

`inspect(source, &Finding) -> Inspection` reads what a found file says about itself: `Metadata` (make, model, lens, serial,
ISO, `captured_at` as `YYYY-MM-DDTHH:MM:SS`, width, height) and an optional `Preview` (an absolute `start`/`len` range of an
embedded JPEG plus its size from the SOF, ready to show on the results screen). It never fails and never panics: what cannot
be read is `None`, nothing is read outside the finding, and the walks reuse the detectors' caps (`TIFF_MAX_LEN`,
`MOOV_MAX_LEN`, `JPEG_MAX_LEN`).

A finding in pieces is inspected as one contiguous file read through a `source::Fragmented` view of its pieces (the same
finding with `start` 0 and no pieces), so it gives the same details as the whole file would; the preview's logical range
is then mapped back with `absolute()`: its `start` is its first absolute byte, and it gets `fragments` only when it spans
more than one piece.

| Kind                            | Details from                                                      | Preview                                              |
| ------------------------------- | ----------------------------------------------------------------- | ---------------------------------------------------- |
| JPEG                            | first `Exif` APP1; size from the SOF                              | the whole file                                       |
| CR2, NEF, ARW, DNG, ORF, RW2, TIFF | the IFD walk                                                  | the largest embedded lossy JPEG (by SOF pixel count) |
| RAF                             | the embedded JPEG's APP1; size from its SOF                       | the embedded JPEG                                    |
| CR3                             | `CMT1` (make, model), `CMT2` (an Exif IFD: ISO, date, lens, serial) | the larger of `PRVW` and track 1's first chunk if it is a JPEG |
| HEIC                            | the `Exif` item listed in `iloc` (4-byte offset prefix skipped)   | none (HEVC; the app uses the OS decoder)             |
| MP4, MOV                        | `mvhd` creation time (UTC, seconds since 1904) and the first video track's `tkhd` size | none                           |

Values are validated or dropped: a date must be a real calendar date (leap years included), time of day valid and the year
1990 or later; strings are printable ASCII, trimmed and non-empty; ISO is above zero; sizes are above zero. A preview is
kept only if it starts with `FF D8 FF`, its SOF parses and its whole range lies inside the finding.

## Scan

`scan(source, formats, &ScanOptions, &CancelToken, &mut on_event) -> Result<ScanSummary, EngineError>` walks a source in blocks
(`ScanOptions::block`, default 4 MiB, rounded down to whole sectors) and looks at the first 16 bytes of every sector
(`ScanOptions::sector`, default 512, at least 16). The first format that recognizes a head and measures a file there wins. The
scan keeps reading every block. After an `Intact` finding it examines no head inside the file (inside its pieces, for a
file in pieces); after a `Partial` or `Damaged` finding it keeps examining heads, so files inside a broken one are still reported (one bad
file never hides the rest of the disk; a damaged file may then overlap another finding). `measure` reads the whole source
(not just the block), so files may cross block boundaries. Before the first block the scan reads the source's file
systems (below), so the files they list come out with their name, folder and dates, once each.

A `Partial` RAW's own JPEGs (rulings D12, R24): for every measurement that is not `Damaged`, the format that measured it
also lists the embedded files it references in its own data (`Format::own_files`: a TIFF RAW's previews and JPEG-coded
strips/tiles, a RAF's embedded JPEG, a CR3's `PRVW` and full-size JPEG; at most 64), in the same measuring step on every
path (a measuring thread, the scan thread, a listed file's job, whose view offsets are mapped back through its pieces),
so the events do not depend on who measured. An `Intact` measurement needs them too: the finding may still come out
`Partial` (a listed file whose placement is not trusted, whose tail is not coherent or whose view joined pieces; a
carved file over live clusters, ruling D7). For a listed file the own files are those of the measurement its finding
took (the view's, or the carving's when it replaced the view). When the finding is `Partial`, the scan keeps their
distinct sector-aligned starts after the RAW's start, each with the RAW's ranges (at most 65,536 at once, in an ordered
map, dropped once the walk is past them). A file found by its signature alone at one of those starts whose ranges lie
wholly inside that RAW's is not reported (it gets no id); when its own measurement is `Intact` it still skips its range
(ruling R10), so a thumbnail inside a hidden preview is not reported either. A listed file is always reported, a
`Damaged` finding hides nothing, and a foreign file elsewhere in the RAW (which cuts it), or one at an own start that
runs past the RAW's end, is found as before.

### File systems in the scan

With `ScanOptions::filesystems` (default `true`; `false` is the scan by signature alone: no `Volume` event, every
`Finding::origin` `None`), the scan first lists the FAT/exFAT files (ruling D1; see [File systems: FAT](#file-systems-fat)
and [File systems: exFAT](#file-systems-exfat)): the metadata through the scan's cache, the FAT32 high-word heads through
a probe over the raw source that refuses, without reading it, any range the cache already knows to be unreadable. Then:

- **Volumes.** One `Event::Volume { volume, listing_truncated }` per volume, in index order, before the first
  `Progress`. `listing_truncated` is the listing's `truncated` flag (ruling R7; the same for every volume, and
  `ScanSummary::listing_truncated`): a cap or budget of the listing dropped or degraded files, so some files found may
  lack their name, folder or dates. A cancel from inside a `Volume` callback stops the scan there.
- **Which files.** Every photo/video of `Listing.files` whose first piece starts at a multiple of `ScanOptions::sector`
  (with a sector above 512 the others are dropped silently; the carving still finds them). When several share a
  first-piece start (deleted entries over each other, a cross-link), one is kept: a live one before a deleted one, then
  the latest `modified` (a missing time loses to a present one), then listing order; the others are not reported.
- **Nothing is skipped because of the file system** (ruling R10): every head the signature rules examine is still
  examined, and only an `Intact` finding skips its pieces, as before.
- **A listed file's finding** is added at the sector holding its first piece, always (whether a format recognizes the
  head there or not, inside a skip range or not, readable or not), before that sector's unreadable and skip checks; the
  carving's head at that sector is consumed. The file is measured through a `source::Fragmented` view of its pieces
  (the detectors at offset 0 of the view), and a deleted file is also measured where it starts, as the carving would
  ("carved"; it counts only when that sector is readable and outside every committed skip range). The two are merged
  (see [Deduplication](#deduplication)). The finding has `origin: Some(FsOrigin)` (volume, folder, name, state,
  placement, recorded size, times); `len` is the sum of its pieces and `fragments` its pieces (empty for one piece),
  unless the carving replaced them. Then the usual rules apply: `Intact` skips its pieces, an unreadable sector inside
  makes it `Partial`, and it is emitted once the scan has read past its farthest piece. A piece that lies before the
  file's start (a FAT allocator that wrapped around on a reused card) was passed before the finding existed, so when the
  finding is added an `Intact` one is lowered to `Partial` if the cache's bad-sector map knows an unreadable sector in
  any of its pieces below that start (everything below it has been read, so this is the same for any `threads` and
  `cache_bytes`), even when the detector never read those bytes.
- **A live file of another kind** (`Listing.others`: `.THM`, `.XML`, ...): the head at its first cluster is consumed
  without a finding; when the measurement there is `Intact`, its range is skipped. Heads inside the file are examined
  as usual (ruling D18: they count as being inside a live file, below).
- **Live clusters (ruling D7).** A finding whose origin is not a live file (found by its signature alone, or deleted)
  and that overlaps `Listing.live` is at most `Partial`, and an orphan `moov` candidate inside `Listing.live` is dropped
  (it belongs to a live file). A forged file system cannot hide a deleted photo: at worst the photo comes out `Partial`.
- **One job, whoever runs it.** A listed file's measurement is queued to the measuring threads like a head's, and never
  dropped for a skip range; when there is no worker result (one thread, a block not read again, a job handed back),
  the commit loop runs the same function, so the events stay the same for any `threads` and `cache_bytes`. For a deleted
  file in one piece whose carving is contiguous and no longer than the piece, the carving read the same bytes, so it
  stands for the view measurement and only the tail check (D6) runs.

Reading once, with the file systems: the metadata goes through the cache like every other read. The exceptions are the
FAT32 high-word heads (at most 65,536 reads of 512 bytes, outside the cache; see Deleted files and folders), the later
pieces of a fragmented file, which its view may read again when the cache no longer holds them, the bytes between
a file's structural end and its recorded size, which the tail check reads (from the cache when the file fits in it),
and the old folders of a quick-formatted card, read after the search (see
[File systems: after a quick format](#file-systems-after-a-quick-format)).
And the listing runs before the first block, so the cache windows it loads (the boot sectors and probe offsets, the
FATs, the directories) are read again by the scan when the cache no longer holds them once the scan reaches them:
directories far into a large card, or, with a cache of a few MiB, the up to 5 windows the volume search loads on a
source with no volume.

Reading once: the scan and every `measure` read through one shared cache (`CachedSource`, crate-private) of
`ScanOptions::cache_bytes` (default 256 MiB; values below 1 MiB are raised to it). "Read once" is about reading the
source (I/O): bytes already in the cache may be walked several times in memory (a block's heads and its `moov` search, a
file's measurement, a JPEG decoded again by its gap search). For files that fit in the cache, each sector is read once
from the source by the scan and the detectors; a JPEG gap search re-reads the photo, its 64 MiB window and at most
64 MiB for its tries (see "Fragmented JPEG"), the pairing step after the scan reads the headers of the candidate videos,
the `moov`s it loads (at most 1 GiB) and the samples it checks again (see "Video with a separated index"), and a
`Partial` or
`Damaged` finding larger than the cache may be read again (the scan keeps examining heads inside it, so it reads its blocks
itself). All of this holds only when the cache is large enough for the work in flight (the read-ahead, the file each
measuring thread is reading and a gap search's 64 MiB window): with less, windows are evicted before the detectors that
need them are done and are read again. Measured with a byte-counting source on the 1 GiB `cut-jpeg` bench image (a gap
search for each of its 95 photos), the source is read 9.46 × with a 32 MiB cache (1 thread; 5.94 × with 16), 3.63 ×
(2.78 ×) with 64 MiB, and 1.00 × from 128 MiB on; `dense-jpeg` is read 1.00 × even with 32 MiB. Keep the default
(256 MiB), or at least about 128 MiB. The cache reads the source in aligned 1 MiB windows (rounded down to whole sectors) and keeps the most recently used
ones up to `cache_bytes`; a window being loaded is not counted yet, so each reading thread can hold one more window on top.
The buffer of an evicted window is reused by the load that evicted it. A window that cannot be read in
one go is resolved sector by sector (scan sector size: with a sector above 512, the detectors see a scan sector that failed
only in part as wholly unreadable, which only makes health more conservative); each unreadable sector is remembered in a shared bad-sector map, and
a read that overlaps it fails every time without touching the source again, also when its window is loaded again after
being evicted (the load reads around the known bad runs). The map keeps at most 1,048,576 merged runs; past that, new
unreadable sectors are not remembered and are read again when their window is loaded again (slower, with the same results
and events). Measured with a synthetic source (`bad_run_cap_measure`, ignored, release): 1 GiB of zeros and then 1 MiB
with an isolated unreadable sector every KiB scans in about 0.4 s and leaves 1024 runs, far under the cap; a dying card
is checked by hand. A block that lies wholly inside an `Intact` finding whose bytes have all gone through the cache already (the
detector walked the file) is not read again; its unreadable sectors come from the bad-sector map, so its `Unreadable` events
are the same. The cache is `Send + Sync`: windows are shared across threads, and a window being loaded by one thread is
waited for by the others, never read twice at once. The scan looks at the heads of a block straight in the cache's windows,
without copying the block.

Threads: the source is still read in order, by the scan thread, while candidate files are measured on worker threads
(`ScanOptions::threads`: 0, the default, means one per available core, at most 16; 1 measures on the scan thread itself;
other values are capped at 64; `ScanOptions::resolved_threads()` gives the number used). The scan thread reads up to
`ScanOptions::lookahead_blocks()` = `min(16, max(1, cache_bytes / (2 × block)))` blocks ahead of the block it is examining,
so the read-ahead fits in half the cache and a block is still cached when it is examined, and queues one job for each
sector start some format recognizes. It then examines the blocks in order exactly as a sequential scan would: a head
inside a skip range committed meanwhile is ignored (its result is thrown away), and for every other head it waits for that
head's measurement. Because a measurement depends only on the source's bytes and unreadable sectors, the events, their
order, the ids and `ScanSummary` are the same for any `threads` and any `cache_bytes`. A JPEG validation decode holds
part of a process-wide budget of 512 MiB (its bytes plus one byte per pixel of the grayscale output) while it runs. The
budget counts only those: the decoder's internal buffers (for example a progressive JPEG's coefficients) come on top.

Memory: at most `cache_bytes` of cached windows, plus for each reading thread (the scan thread and every measuring
thread) one window being loaded (1 MiB) and its reader pool (at most 4 MiB), plus the decode budget (512 MiB, shared by
every scan in the process, plus the decoder's internal buffers), plus what each running measurement holds (for example a
`moov` of at most 16 MiB, or a JPEG gap search's DC values, about 3 × 13 MiB at 210 megapixels, plus 3 MiB), plus about
200 B per recognized sector start in the read-ahead (at most
lookahead × block / sector of them), plus the `moov` candidates (16 B each: at most 4096 per block read ahead, and
4096 kept), the `Damaged` videos kept for pairing (at most 4096 findings), the start of every finding (8 B each) and
the own-file starts of `Partial` RAWs (at most 65,536 at once, about 70 to 90 B each: an ordered-map entry, its `Vec`
and a shared pointer to the RAW's ranges, so at most about 6 MiB, plus each RAW's ranges), the old-directory candidates and the findings they may name (see
[File systems: after a quick format](#file-systems-after-a-quick-format)),
plus the file-system listing, kept for the whole scan: at most 262,144 photo/video files and 2^20 pieces, ≈ 355 MiB
in the worst case at those caps (mostly names; see the Memory paragraph of [File systems: FAT](#file-systems-fat)),
and while it is read before the first block, a peak of ≈ 640 MiB in the worst case (on exFAT, plus the allocation
bitmap, ≤ 32 MiB, and the root entries waiting for it, ≤ 68 MiB), all of it a sum no real card reaches. Each running
measurement of a listed file adds its view (16 B per piece, at most 4096 pieces) and a 64 KiB buffer for the tail
check. After the scan, the pairing step holds one
`moov` (at most 16 MiB) and its sample lists (at most 4,000,000 samples, about 32 B each, so up to about 128 MiB). With
the defaults on a 16-thread machine: about 256 MiB + 17 × 5 MiB + 512 MiB, plus the measurements' own buffers.

Events:

- `Volume { volume, listing_truncated }`: a FAT/exFAT volume, before the first `Progress`, one per volume in index
  order (see [File systems in the scan](#file-systems-in-the-scan)); none with `ScanOptions::filesystems` off.
- `Found(Finding)`: sequential `id` from 1, in start order; `origin` is `Some` when the file system lists the file. It is emitted once the scan has read past the file's end (the end of its
  farthest piece), so unreadable sectors inside the file are known: they are reported as `Unreadable` events before the
  `Found`, and an `Intact` finding with any of them inside one of its pieces is downgraded to `Partial` (also one in a
  piece below the file's start, met before the finding existed: from the bad-sector map). A file found inside a `Partial`/`Damaged` one is emitted right
  after it.
- `Progress { read, total }`: once per block, plus one for an empty source; the last has `read == total`.
- `Unreadable { start, len }`: when a block cannot be read it is re-read sector by sector; each run of consecutive failed
  sectors (also across blocks) is one event and counts in `ScanSummary::unreadable_bytes`. A run is split where a finding
  ends. The scan then continues.
- `DiskFailing { unreadable_runs, unreadable_bytes, read }`: the disk is failing, make a copy first (ruling D20). Once per
  scan, right after the `Progress { read, .. }` of the first block boundary where the `Unreadable` events emitted so far
  reach `scan::FAILING_RUNS` (16) runs or `scan::FAILING_BYTES` (4 MiB); the fields are those counts at that point. Why
  these numbers: a healthy card has no unreadable sector at all, so 16 separate failures (or 4 MiB in one place) already
  say the medium is degrading, and every further pass over a dying card can cost more of it; the earlier the person is
  told to copy it, the more the copy saves. The cost: a card with 16 scattered bad sectors that reads fine elsewhere gets
  the warning too. A run still open at a boundary counts once it is emitted (a run that ends only with the source, after
  the last `Progress`, never flags). Deterministic: it depends only on the `Unreadable` events, so it is the same for any
  `threads` and `cache_bytes`.
- `Rate { bytes_per_sec, eta_secs, stalls }`: only with `ScanOptions::rates` (default `false`, ruling D21), right after
  each `Progress` (after its `DiskFailing` when both fall on the same boundary: the order is always `Progress`,
  `DiskFailing`, `Rate`). `bytes_per_sec` is the reading speed smoothed by `rate::RateMeter` (10 s half-life; 0 until
  known), `eta_secs` the seconds left at that speed (`None` while unknown), `stalls` the cache window loads so far that
  took more than `scan::STALL` (2 s): a device that struggles. It is the only event that depends on timing: it is
  outside the determinism contract (compare event lists with it filtered out) and is never stored in a session.
- `Updated(Finding)`: a better version of a finding already reported, which replaces the earlier `Found` with the same
  `id`. Emitted after every `Found` (and the last `Progress`), before `scan` returns, in id order; counted in
  `ScanSummary::updated`. Today: a video put back together with its separated `moov` (see "Video with a separated
  index"), and a finding an old folder of a quick-formatted card names (see
  [File systems: after a quick format](#file-systems-after-a-quick-format)); one event per id, both merged.

`CancelToken::cancel()` (from any thread or from inside the callback) stops the scan: it is checked before each block
examined and each block read ahead, before and after every event and while waiting for a measurement. A block
boundary's `Progress`, `DiskFailing` and `Rate` count as one event here: checked before and after the group, so a cancel
from inside their callback stops the scan right after the whole boundary (where a saved session can still checkpoint).
Once it is seen no further event is emitted (a cancel from inside the callback makes that event, or that boundary, the
last one), the measuring threads drop
their remaining jobs, and `scan` returns `EngineError::Cancelled` after every thread has stopped (a measurement already
running is finished first, except a JPEG gap search, which gives up as soon as it sees the cancel, and a `zune-jpeg`
validation decode, which is skipped when the cancel is seen before waiting for the decode budget or once the budget is
granted); findings still pending are dropped. Cancellation is also seen inside the reads: the scan's cache checks it
before loading each 1 MiB window, before each stretch of it and before each sector of a window resolved sector by
sector, and then fails the read (keeping nothing), so any measurement still running ends quickly and a slow failing
device stops within about one sector read. The FAT32 high-word heads the listing reads beside the cache are not read at
all once the scan is cancelled or the source has vanished.

A source that vanishes during the scan (`DeviceGone`, or a short read) stops it the same way with
`EngineError::DeviceGone`: it is checked before and after every event and before each block, so the events emitted are
exactly the first ones of the scan the device would have allowed, with no `Unreadable` for the missing sectors. A
file-system listing read while cancelled or after the source vanished (including the FAT32 high-word heads read beside
the cache) is never used: the scan returns that error before emitting any `Volume`.

## Resumable scans

A scan can be stopped and taken up again where it was, with exactly the events it would have emitted had it not stopped.
The saved session ([Sessions](#sessions)) stores this on disk; inside the engine, `scan_with` (crate private; `scan` is
`scan_with` with neither a resume nor a hook) hands a hook three kinds of checkpoint:

- `Listing`, once, after the file-system listing (an empty one with `filesystems: false`) and before any event. A
  listing read while the scan was cancelled or after the source vanished is never handed over (nor any `Volume`
  emitted): the scan returns `cancelled` or `device_gone` instead (ruling P3).
- `Boundary`, at every block boundary, right after its `Progress` and its `DiskFailing` (the fixed order is `Progress`,
  `DiskFailing`, `Rate`), with the bytes read and the events emitted so far. Every event counts except `Rate`, which is
  timing and never part of a saved scan (ruling P4). The state is built only when the hook asks for it. `stopping` says
  the scan was cancelled or the source vanished once the boundary's events were out: the state is still good
  (everything in it was committed before), and the scan returns that error right after the hook.
- `Paired`, after the pairing step, before the `Updated` events.

**What the state holds** (ruling D22): the shape of the scan it belongs to (the sector and block sizes as the scan
uses them, file systems on or off, the source's length: a scan of another shape refuses it, ruling P44), the position
(the next block's start) and the event count; everything the
in-order half of the scan carries from block to block (the summary, the open unreadable run, the `Unreadable` count and
whether `DiskFailing` was sent, the findings not yet emitted, the next id, the orphan `moov` candidates and the ranges of
the ISO BMFF findings that rule some out, the `Damaged` videos kept for pairing, the caps' flags, the start of every
finding, the next listed file, the own-file starts of `Partial` RAWs with their owners' ranges, the old-folder
candidates and the findings on a cluster grid they may name); the skip ranges; the last bytes the `moov` and old-folder
searches saw before the position (a header across the boundary is still found); the cache's **whole** bad map (ruling
P5) and its coverage from the position on. The capture names every field of the commit loop, so a field added later
does not compile until it is saved or rebuilt. Rebuilt on resume, not stored: the cluster grids, the volumes and what the
scan keeps of the listing, from the listing saved once at `Listing`. Encoded with the engine's own binary format (no
`serde`): integers little-endian, enums by their `as_str` name, every length checked against what remains, so damaged
bytes (or a state that cannot belong to the scan: another shape, a position past the end or off a block boundary, runs
out of order, a cap passed) are `session_corrupt`, never a panic. A damaged count reserves at most 1024 items ahead (the
rest grows only as items actually decode), so it cannot make the decoder reserve gigabytes.

**The guarantee**: for a source that answers the same every time (the same bytes, the same unreadable sectors), a scan
resumed from any checkpoint emits exactly the events the uninterrupted scan emitted after it, ids and summary included,
for any `threads` and `cache_bytes` on either side.

**What a resume reads again** (ruling D24): the blocks the stopped scan had read ahead past the position (at most the
read-ahead, `lookahead_blocks × block`, plus what was read since the last saved checkpoint), the pieces below the
position of listed files that start after it, and what the pairing loads (a resume from `Paired` runs the pairing again).
Never: the listing (ruling D23: read again with a fuller bad map it could come out different), a sector of the saved bad
map (the cache starts with it, including runs below the position that the pairing or a file's pieces may touch), or a
block wholly inside an `Intact` finding across the position whose bytes the cache had already read (its coverage comes
back with the state; for that the cache now keeps its coverage from the walk's position on, not from the read-ahead's).
Starting from the position, a scan without file systems or pairing reads nothing below it.

**Memory**: the state is bounded by the caps the scan already has (pending findings, 4096 + 4096 pairing candidates,
65,536 own-file starts, 65,536 old-folder candidates, 262,144 grid findings and one start per finding), plus the bad map
(at most 2^20 runs, 16 bytes each in the saved form: about 16 MiB) and at most 65,536 coverage runs. The listing is
saved once, up to a few hundred MiB in the worst case of the listing's caps.

## Sessions

A session saves a scan into a folder of its own, so it can be taken up again after a cancel, a card pulled out, a crash
of the program or a power loss, with exactly the events of an uninterrupted scan, and so a finished scan's results can be
read back later without the card.

```rust
use revenant_core::session::{Session, SessionOptions, scan_session};
let dest = Destination::check(&ops, &source, folder)?;             // never on the source
let mut session = Session::create(&dest, identity.clone(), &opts)?; // the folder must be empty
let summary = scan_session(&source, &identity, &formats, &opts, &mut session, &SessionOptions::default(), &cancel,
    &mut |event| { /* … */ })?;
// Later, after a crash or a disconnect:
let dest = Destination::check(&ops, &source, folder)?;  // checked again, with the caller's ops
let mut session = Session::open_checked(&dest)?; // state(): Fresh, Interrupted { read, total } or Finished(summary)
scan_session(&source, &identity, &formats, &opts, &mut session, &sopts, &cancel, &mut on_event)?;
```

- **One folder per session, never on the source.** `Session::create(&dest, identity, &opts)` takes a folder checked
  with `Destination::check` (not on the card, through any of its volumes) that must be empty (`session_not_empty`). It
  records the source's identity (see [Source identity](#source-identity)) and the scan's `sector`, `block` and
  `filesystems`: a resume always scans with these, whatever the caller's `opts` say (threads, cache size and rates do
  come from the caller). `Session::open_checked(&dest)` reopens one in a folder the caller checked again with
  `Destination::check` and its own `DeviceOps` (`open_failed` when there is no session there): the way to resume a
  card's session. `Session::open(folder)` reopens one without a check, to read its results (`events()`) or to resume an
  image's session (its folder is then checked with the volume rule alone, no system asked); resuming a device's session
  opened that way is refused (`destination_unknown`, nothing read or written), since only the caller's `DeviceOps` can
  tell which disks the folder lives on.
- **One session per source.** `scan_session` refuses (`session_other_source`, before anything is read or written) an
  `identity` that is not `same_source` as the session's: for a card, size, model and the digest of its identity sectors
  (the serial number counts only when both sides have one), so the same card in another reader resumes and a reformatted
  or different card does not; for a rescue copy's image read through its map (`SourceIdentity::of_mapped`), the map's
  state, so rescuing more of the image meanwhile makes it another source (ruling P7). `check_session(folder, &identity)`
  answers the same from the manifest alone (`Ok` when the folder holds no session), and the pre-flight check reports it.
- **Replay.** `scan_session` first hands out the stored events (`replay_len()` of them), then scans on from the last
  checkpoint: the whole sequence is the uninterrupted scan's, ids and summary included, for any `threads` and
  `cache_bytes` (the guarantee and its condition are those of [Resumable scans](#resumable-scans)). `Rate` events are
  never stored nor counted. A **finished** session only replays: the source is not read and nothing is written, so its
  results come back with the card gone (`events()` reads them without any source).
- **Cadence.** A checkpoint is saved at the next block boundary once `SessionOptions::checkpoint_bytes` (default 256 MiB)
  were scanned or `checkpoint_every` (default 30 s) passed since the last one, after the pairing step, and when the scan
  finishes (with its summary). A cancel or a vanished card seen at a block boundary saves one there; seen anywhere else,
  the last checkpoint stands and the resume redoes (identically) what came after it: at most about 256 MiB or 30 s of
  scanning, plus the read-ahead. A listing read while cancelled or after the card vanished is never saved (ruling P3):
  the resume reads the file systems again.
- **Errors.** `session_other_source`, `session_not_empty`, `session_version` (written by a version this engine cannot
  read), `session_corrupt` (damaged beyond the fallbacks below), `write_failed`, `destination_on_source` (the folder is
  no longer on the volume it was checked on), and the scan's own. On any error the session stays at its last checkpoint,
  ready to run again.

**The folder** (ruling D25): every file starts with `RVNTSESS`, a kind byte and the format version (`u16`, 1).

| File                                      | Holds                                                                                          |
| ----------------------------------------- | ---------------------------------------------------------------------------------------------- |
| `session.rvs`                             | CRC-32, then the identity, `sector`, `block`, `filesystems`                                    |
| `listing.rvs`                             | CRC-32, then the file-system listing, saved once (an empty one with file systems off)         |
| `events.rvs`                              | records `[len u32][crc32 u32][event]`, appended as the events go out (every one but `Rate`)    |
| `checkpoint-a.rvs`, `checkpoint-b.rvs`    | CRC-32, then a generation, the log's length and event count, and the scan state or the summary |

Values use the engine's binary encoding (see [Resumable scans](#resumable-scans)): every length checked, and only a
regular file read (a link, folder, device or FIFO is `session_corrupt`, never followed or waited on), each up to a cap:
manifest 1 MiB, listing 256 MiB, each checkpoint 256 MiB, an event record 32 MiB (the log is streamed, one record at a
time). So a damaged or hostile folder is `session_corrupt`, never a panic, and opening one holds at most one file's
bytes at a time (256 MiB) plus what they decode to: the newest checkpoint's state and the listing, each bounded by the
scan's own caps (see `ScanOptions` and [Resumable scans](#resumable-scans)). The manifest's `sector` must be a power of
two from 512 B to 64 KiB and its `block` a whole number of sectors up to 1 GiB (`session_corrupt` otherwise;
`Session::create` refuses such options with `write_failed`, nothing written). The caps hold for writing too, so a
session that saves always reads back: the walk's caps (262,144 files, paths up to 4,096 bytes) let a crafted card's
listing pass 256 MiB, and a checkpoint grows by 8 bytes per finding (over 10 million findings fit); a listing or
checkpoint over its cap is not saved (`write_failed`, the session stays at its previous checkpoint; a scan without a
session still reads such a card).

**Crash safety.** The log is only appended to. A checkpoint is written after the log is flushed and `sync_data`'d, into
a new temporary file (`sync_all`) renamed over the older of the two checkpoints, then the folder is synced (best effort:
some file systems refuse it, ruling P33); the listing is written the same way. Opening takes the valid checkpoint with
the highest generation (a damaged or torn one, checksum wrong, falls back to the other) and drops whatever the log holds
past the length it names (a record half written when the power went). Both checkpoints damaged is `session_corrupt`.
Nothing follows a link planted in the folder: new files are created exclusively (a planted temporary is removed first,
never its target), a rename replaces a link rather than its target, and the log is reopened only after checking that the
file opened is the regular file looked at (same device and inode on Unix; on Windows a reparse point is opened itself
and refused) and has a single link (on every system: a hard link to a file elsewhere is refused).

**Cost.** A session adds a fixed cost per scan and no measurable cost per byte (ruling P46). The fixed cost is the
syncs that make it crash-safe: two files and the folder synced at create, and at each checkpoint the log's `sync_data`,
the checkpoint's `sync_all` and the folder sync (on macOS `sync_all` is a full flush of the drive's cache, tens of
milliseconds each). Checkpoints come every 256 MiB or 30 s, after the pairing step and at the end. Measured with
`revenant-dev bench --session` (release, best of 3, default cadence, on the machine of [Performance](#performance)):
at 1 GiB the session took 0.3 to 0.4 s more than a plain scan (`noise` 0.73 to 1.11 s, `dense-jpeg` 3.56 to 3.77 s); at
4 GiB 0.08 to 0.2 s more on `noise` (1.88 to 1.99 s plain, 2.07 to 2.10 s through a session), and nothing on
`dense-jpeg` (15.3 s through a session against 16.2 to 16.7 s for a plain scan run in the same place of the bench, after
the plain runs; with the cadence checkpoints off it is the same 15.3 s). The difference does not grow with the size: on
a card read at device speed (100 to 300 MB/s) it is well under 1%.

## Deduplication

A file the file system lists and the carving also finds comes out once: at a listed file's start the finding is the
merged one, and the carving's head there is consumed (see [File systems in the scan](#file-systems-in-the-scan)). The
merge (rulings D3, D5, D6), from the measurement through the view of the file's pieces:

1. Kind and health are the view measurement's; when no format measures the view, the extension's kind, `Damaged`.
2. The health is at most `Partial` when the placement is not trusted (`FsFile::trusted`, ruling D3: a live chain that
   is incomplete, loops or crosses another, a deleted file not in one run of free clusters), when the tail is not
   coherent (D6, below) or when the view measurement is itself in pieces (a join inside the file's own pieces).
3. The pieces are the file system's: `len` their sum, `fragments` them when there are two or more.
4. A deleted file takes the carving at its start instead (its length, kind, health and pieces; `placement` `carved`)
   when the carving is of the same kind, strictly healthier (`Intact` > `Partial` > `Damaged`) and exactly the
   recorded size (D5): the guess was wrong and the signature found the file. A live file never does: its chain is
   where its bytes are. A carving of another length (a join to the tail of another photo of a burst) is not used.

The tail (D6): the structure measured over the view ends at `L`, and the view (the recorded size, as far as the clusters
and the source hold it) has `S` bytes. `L == S` is coherent. Otherwise, at most 8 times: the bytes `[L, S)` are all
`00` or `FF` (padding) → coherent; else a format recognizes the head at `L` and measures a contiguous file of `m` bytes,
not `Damaged`, with `L + m ≤ S` (an MPF image, a motion photo's video) → `L += m` and again; else it is not coherent.
So a corrupt count inside a listed file is bounded by its recorded size (the view ends there), and a file whose size
does not match its contents is at most `Partial`. The file system is evidence of where a file is, never of its
integrity: its facts only ever lower the health a detector gives.

Not merged: a file found by its signature where no listed file starts (no entry, or a deleted entry whose first cluster
was reused), and files inside another one (still found inside a `Partial` or `Damaged` finding, as before). Such a
finding that overlaps a live file is at most `Partial` (D7).

## Performance

Measured with `revenant-dev bench` (release build, 1 GiB images, best of 3 runs; see `crates/revenant-dev/README.md`)
on the development machine: Intel Core i9-9980HK (8 cores / 16 threads), 32 GB of RAM, macOS, Rust 1.99. The images
are read from the page cache (the bench has just written them), so these numbers measure the engine, not a disk: they
are an upper bound for a card reader. "Before" is the start of part 1.3 (commit `e575cd4`: one thread, every detector
reading on its own); "after" is the end of it (commit `718f1f1`), with the default threads (16 here) and with one.
MB = 10^6 bytes.

| Image        | Files | Before (MB/s) | After 1.3, 16 threads (MB/s) | After 1.3, 1 thread (MB/s) | End of 1.4, 16 threads (MB/s) |
| ------------ | ----: | ------------: | ---------------------------: | -------------------------: | ----------------------------: |
| `noise`      |     0 |         765.6 |                       2381.4 |                     2265.4 |                        2207.5 |
| `dense-jpeg` |    95 |          21.8 |                        503.9 |                      102.8 |                         539.8 |
| `dense-raw`  |    37 |         158.1 |                       1576.9 |                      589.5 |                        1420.3 |
| `video`      |     3 |         797.2 |                       2296.6 |                     2216.3 |                        2525.4 |
| `cut-jpeg`   |    95 |          26.8 |                        419.3 |                       85.6 |                         439.0 |
| `exfat-card` |    93 |             - |                            - |                          - |                         459.1 |

The "end of 1.4" column is the head of part 1.4 (default threads, one run of `revenant-dev bench`, best of 3 scans);
`exfat-card` (an exFAT volume holding the `dense-jpeg` photos, every 4th deleted, so the scan also reads the file system)
is new in 1.4, so it has no earlier figure: at 459.1 MB/s it is 0.85 of `dense-jpeg` (539.8), above the 0.8 gate, and it
finds all 93 placed files. The other lines were not compared side by side with part 1.3 (only `dense-jpeg` and `noise`
were, below); between runs on this machine, which throttles, they move by up to about 10% (`dense-raw` 1576.9 to
1420.3, `video` 2296.6 to 2525.4). Against part 1.3's head (`main`, `b5ec340`) built side by side and interleaved, best
of 3 scans, three rounds with a 60 s pause: `dense-jpeg` 581.4 / 584.7 / 591.6 MB/s on `main` against 545.8 / 561.1 /
566.6 on the 1.4 head (4 to 6% slower); `noise` swings the other way as often as not (2794.1 / 2653.3 / 2523.6 on
`main`, 2468.3 / 2397.0 / 2807.7 on the head), so it shows no difference. The `dense-jpeg` gap is not file-system work:
on that image (no volume) the listing step costs about 1.1 ms and the old-folder finder is never created, and a control
rebuild of the head (a one-line change) scanned `dense-jpeg` in 2.11 s with file systems on and 2.14 s off, against
2.13 s on `main`, while the head as first built took 2.26 s. The gap is the code layout of that one build. On this Mac
an A/B needs such a control rebuild to resolve a difference of about 6%.

**End of 1.5 (session lines).** Part 1.5 added no work to the plain scan path except the early-warning counters and the
state a session captures at block boundaries, so the plain lines are expected to stay where 1.4 left them. They did
not: measured at the end of 1.5 (`revenant-dev bench`, default threads, best of 3 scans) the Mac was loaded and
throttling, and every line came out lower, the ones 1.5 cannot touch included: `noise` 1403.6 / 1360.0 MB/s (two
full runs), `dense-jpeg` 305.7 / 275.4, `dense-raw` 938.8 / 841.5, `video` 1529.2 / 1454.1, `cut-jpeg` 244.1 / 248.8,
`exfat-card` 288.3 / 296.8, against 2207.5, 539.8, 1420.3, 2525.4, 439.0 and 459.1 at the end of 1.4. Part 1.5's own
tasks saw the same slow slot (`dense-jpeg` 205 to 220 MB/s in the middle of the part). Because a single build cannot
resolve about 6% on this machine (see above), the 1.4 head (`main`, `08aae30`) was built in a separate target and run
interleaved with the 1.5 head, three rounds with a 30 s pause, best of 3 scans: `noise` 1322.0 / 1724.8 / 1587.6 on
`main` against 1439.8 / 1285.7 / 1313.1 on the 1.5 head; `dense-jpeg` 270.8 / 367.7 / 302.1 against 305.3 / 284.7 /
287.1; `exfat-card` 278.9 / 310.8 / 282.6 against 304.5 / 290.6 / 303.1. The spread of `main` against itself (271 to
368 MB/s on `dense-jpeg`) is larger than any gap between the two builds and the order of the builds changes from round
to round, so this run shows no slowdown from part 1.5 but cannot rule out one of a few percent. The absolute numbers
of the end of 1.4 were taken on a quieter machine and are not reproduced today.

The session lines (`bench --images noise,dense-jpeg --session`, same slot, best of 3, a new session per scan): `noise`
0.71 s (1507.9 MB/s) plain against 1.06 s (1017.2) through a session, `dense-jpeg` 3.61 s (297.2) against 3.97 s
(270.5), that is 0.35 s and 0.36 s more, the fixed cost of the syncs described under [Sessions](#sessions) (ruling P46:
it does not grow with the size; at 4 GiB it was 0.08 to 0.2 s). On a card read at device speed it is under 1%. The
device against the image of the same card, as the opt-in `os_devices` test prints it with images attached by `hdiutil`
(not a card reader): 76 to 79 MB/s on the raw device against 80 to 84 MB/s on the image file (this run), 113 to 127
against 148 to 154 in the part's first run, so reading the raw device costs a little, not a multiple.

Every placed file is found in every run. What it means for a card: an SD card reader delivers at most about 100 MB/s
(UHS-I) to about 300 MB/s (UHS-II), so with the default threads the scan keeps up with the reader on every image here,
the photo-dense ones included (`dense-jpeg` 503.9 and `dense-raw` 1576.9 MB/s, above the 300 MB/s goal). With one
thread it does not on the photo-dense JPEG images: `dense-jpeg` is 102.8 MB/s and `cut-jpeg` 85.6 MB/s, below 300 MB/s
(checking JPEGs is CPU work, and one core does it at about that rate). `cut-jpeg` has no speed target: each cut photo
starts a gap search that may re-read its 64 MiB window. The `moov` search on the scan thread costs about 10% on
`noise` and `video`. The bench JPEGs are noise-encoded (about 11 MB per 3000x2000 photo, more entropy than a real
photo), a heavy case; the real camera samples (below) all still come out as one `Intact` finding each.

## Extract

`extract(source, &Finding, dest_dir) -> Result<PathBuf, EngineError>` copies a finding's bytes (in 1 MiB chunks) into `dest_dir`
under `naming::file_name(finding, None)`: the original name of a finding the file system listed, else
`revenant_<start as 12 hex digits>.<ext>`. `extract_as(source, &Finding, dest_dir, name)` does the same under a name you
choose: `sanitize(name)`, or `revenant_<start>.<ext>` when nothing is left of it. If the name exists, `<stem>_2.<ext>`,
`<stem>_3.<ext>`… are tried. Files are created with `create_new` (O_EXCL), so the volume itself decides what is taken:
nothing is ever overwritten, on case-sensitive and case-insensitive volumes alike and for NFC/NFD forms of one name
(`DSC00001.JPG` and `dsc00001.jpg` give two files; after 10,000 attempts it fails with `write_failed`).

`extract_to(source, &Finding, &Destination, name)` writes into a folder already checked with `Destination::check` (see
[Destination](#destination)); `extract_as` is `Destination::check(&SystemDevices, ..)` followed by `extract_to`, so it checks
on every call (cheap for an image; with a device source it asks the system each time, so check once and use `extract_to`). The
returned path lies in the resolved folder (links followed: on macOS a temporary folder under `/var` comes back under
`/private/var`).

- Refuses with `destination_on_source` when the destination is on the source's volume (writing to a damaged card could destroy
  what is still recoverable) or, for a device source, on any volume of that device; with a device source a destination
  whose disks cannot be told is `destination_unknown`. If an image source's volume is unknown (`None`) extraction is
  allowed. If the destination's volume cannot be determined (e.g. the directory does not exist) it returns `open_failed`
  and writes nothing.
- A finding in pieces is written as its pieces (`Finding::ranges()`) one after another, in file order; the file is still
  named after the original name or, without one, after `start` (the first piece's start).
- A chunk that cannot be read is re-read sector by sector (512 bytes); only the unreadable sectors are written as zeros, so a
  partial file comes out instead of none. Each piece stops at the source's end, even if its length runs past it.
- A write error returns `write_failed` and removes the incomplete file.
- A source that vanishes (`device_gone`, or a read coming back short before the source's end) is never written as zeros:
  the copy stops, the incomplete file is removed and `device_gone` is returned.

## Grouping

`group` (pure functions, no I/O) arranges recovered files for the results screen. Each file becomes a `GroupInput`
(`GroupInput::new(&Finding, Option<&Metadata>)`: id, time, camera, folder) and `by_session`, `by_day`, `by_camera` and
`by_folder` return `Vec<Group>` (a `GroupLabel` and the ids). The result never depends on the order of the input.

- **Time** (photos): the capture time, else the origin's modified time, else its created time. **Videos** (MP4/MOV):
  the origin's modified time, else created, else the capture time (the file system keeps local time, the `mvhd` date is
  UTC). The first candidate that is a real date wins; a file with none is "undated".
- **Camera**: make and model (trimmed, case-insensitive) plus serial; `None` when make and model are both unknown. A file
  without a serial joins the one body of its model when the files have exactly one serial for it; with two or more
  serials it stays apart. The label shows the strings of the file with the smallest id.
- **By session**: per camera, files sorted by (time, id); a pause of more than `SESSION_GAP_SECS` (2 hours; exactly 2 hours
  stays in the session) starts a new one. Sessions are ordered by (first time, camera, first id); each camera's undated
  files form an `UndatedSession` after them. A session label carries the first and last time.
- **By day**: `Day("YYYY-MM-DD")` ascending, `Undated` last. **By camera**: ordered by camera, `UnknownCamera` last.
- **By folder**: the origin's folder, whatever its state: a card after a quick format still has its old folders
  (`?/100MSDCF`) and that counts. A file in the root is `Folder("")`; a file with no origin, or whose folder the listing
  dropped (too long, or past its path budget), is in `NoFolder`, last. `None` when no file has a folder at all (the
  structure did not survive: the screen offers no "by folder").

Ids inside a group go by (time, id), the undated last.

## Names

`naming::file_name(&Finding, Option<&Metadata>)` gives the name a recovered file is written under:

1. The original name from the file system (`origin.name()` alone, never the folder), sanitized. A deleted file keeps the
   name the entry still holds (`_SC01235.JPG`).
2. Else `YYYY-MM-DD_HH-MM-SS[_<camera>].<EXT>` from the capture time (`Metadata::captured_at`; else the origin's modified
   time), `EXT` being the kind's extension in upper case. The camera part is the model without the make's first word,
   only ASCII letters, digits and `-`, at most 24 chars: make `Canon`, model `Canon EOS R5` gives
   `2025-12-14_07-12-05_EOSR5.CR3`.
3. Else `revenant_<start as 12 hex digits>.<ext>` (what 1.3 wrote).

`naming::sanitize(name) -> Option<String>` makes one safe component, so extraction never leaves the destination folder:
`/ \ : * ? " < > |`, control characters (below 0x20, 0x7F and the C1 range U+0080–U+009F), U+2028/U+2029, format controls (U+200B–200F, U+202A–202E, U+2066–2069,
U+FEFF, which can disguise an extension) and U+FFFD become `_`; leading and trailing `.` and spaces are trimmed (`..`
gives `None`); a Windows reserved stem (`CON`, `PRN`, `AUX`, `NUL`, `COM0`–`COM9`, `LPT0`–`LPT9`, their superscript
forms, `CONIN$`, `CONOUT$`; any case; the stem is what precedes the first `.`, so `CON.tar.gz` counts) gets a `_` in
front; at most 200 bytes (before a `_N` suffix), cut on a char boundary, keeping an extension of up to 16 bytes. `Destination::create_named(&Finding, name, suffix)` is the one public way to create a file under a derived name (after the destination check; the CLI writes previews that way): it sanitizes `name`, replaces the extension by `suffix` when given (which must be empty or start with `.` and hold no unsafe char, else `write_failed`) and applies the `_N` rule. A total path over 260
characters on Windows is a write error (`write_failed`) there, never an escape.

## Tested against real files

`tests/real_samples.rs` runs the engine over real camera files (one each of CR2, CR3, NEF, ARW, RAF, DNG, ORF, RW2; all
CC0 from raw.pixls.us, listed with URL and SHA-256 in `samples/samples.toml`, never committed). Each is embedded in a
noise image at a sector start and must come out as exactly one finding of its kind, `Intact`, with the file's length
(for ARW, trailing zero padding under 64 KiB aside), make and model, and a preview that decodes strictly. A second test
splits each sample's preview in two at k/10 (k = 2..=8, on a sector boundary) with its own tail 8 MiB further on: it
must come out joined and `Intact`, or joined and `Partial` for the previews that use restart markers (Q16: 21 of the
56, all of the X-S10, TG-7 and K-r splits), the 2 known `Partial` splits above aside; and with another sample's tail
instead,
never `Intact`. Both are ignored by default:

```sh
scripts/fetch-samples.sh
REVENANT_SAMPLES=samples/cache cargo test -p revenant-core --test real_samples -- --ignored
```

CI runs it in the `samples` job: a failed download (raw.pixls.us down) skips the test without failing the build, while
a failing test fails it. There is no HEIC sample: none with a verifiable CC0/public-domain license was found.

## Trying it

The `revenant-dev` crate is a command line over this engine: `revenant-dev scan <image> [--extract <dir>]
[--previews <dir>]` prints each finding, with its photo details and preview, as a JSON line. See
`crates/revenant-dev/README.md`.
