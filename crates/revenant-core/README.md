# revenant-core

The Revenant recovery engine (library). It never writes to the source being read, and contains no UI text.

## Current state

For now the crate only defines `EngineError`, the engine's error type. Every variant has a stable `snake_case` code, returned by
`EngineError::code()`:

| Variant               | Code                    |
| --------------------- | ----------------------- |
| `Open`                | `open_failed`           |
| `Read`                | `read_failed`           |
| `DestinationOnSource` | `destination_on_source` |
| `Write`               | `write_failed`          |
| `Cancelled`           | `cancelled`             |

```rust
use revenant_core::EngineError;
assert_eq!(EngineError::Cancelled.code(), "cancelled");
```

## Source

`source::Source` is the engine's read-only disk-access layer: `len()`, `read_at(offset, buf)` (positional, no shared cursor; fills
the buffer except at the end of the source, returns 0 past the end) and `volume()`. There is no write method.

`source::FileSource` implements it for regular files and device nodes, opened read-only (`pread` on Unix, `seek_read` on
Windows). `source::volume_of(path)` returns the `VolumeId` of any existing path (device id on Unix, volume serial number on
Windows), so extraction onto the source's own volume can be refused. A test scans `src/source/` to ensure no write-capable open
flags are ever introduced.
