//! The engine's own binary encoding for saved scan state (ruling D26): integers little-endian, `bool` as one byte, strings
//! and byte strings with a `u32` length, sequences with a `u32` count, enums by their `as_str` name. Decoding checks every
//! length and count against what remains, so damaged or hostile bytes give [`EngineError::SessionCorrupt`], never a
//! panic nor an allocation larger than the input.

use std::sync::Arc;

use crate::EngineError;
use crate::formats::{FileKind, Fragment, Health};
use crate::identity::SourceIdentity;
use crate::scan::{Event, Finding, ScanSummary};

/// Most nested `Mapped` identities decoded (a rescue map refuses deeper ones).
const MAX_IDENTITY_DEPTH: usize = 4;

/// An encoder: appends values to a byte buffer.
#[derive(Default)]
pub(crate) struct Enc(Vec<u8>);

impl Enc {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn into_bytes(self) -> Vec<u8> {
        self.0
    }

    pub(crate) fn u8(&mut self, v: u8) {
        self.0.push(v);
    }

    pub(crate) fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }

    pub(crate) fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }

    pub(crate) fn bool(&mut self, v: bool) {
        self.u8(u8::from(v));
    }

    /// A count or length. Every one the engine writes is bounded far below `u32::MAX` by its caps.
    pub(crate) fn len(&mut self, n: usize) {
        debug_assert!(u32::try_from(n).is_ok(), "length {n} does not fit a u32");
        self.u32(n as u32);
    }

    pub(crate) fn bytes(&mut self, v: &[u8]) {
        self.len(v.len());
        self.0.extend_from_slice(v);
    }

    pub(crate) fn str(&mut self, v: &str) {
        self.bytes(v.as_bytes());
    }

    /// A sequence: its count, then each item.
    pub(crate) fn seq<T: Codec>(&mut self, items: &[T]) {
        self.len(items.len());
        for item in items {
            item.enc(self);
        }
    }

    pub(crate) fn opt<T: Codec>(&mut self, v: Option<&T>) {
        self.bool(v.is_some());
        if let Some(v) = v {
            v.enc(self);
        }
    }

    pub(crate) fn put<T: Codec>(&mut self, v: &T) {
        v.enc(self);
    }
}

/// A decoder over a byte slice.
pub(crate) struct Dec<'a> {
    buf: &'a [u8],
    pos: usize,
}

fn corrupt<T>() -> Result<T, EngineError> {
    Err(EngineError::SessionCorrupt)
}

impl<'a> Dec<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// Bytes not yet decoded.
    pub(crate) fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// `Ok` when everything was decoded (trailing bytes are corruption).
    pub(crate) fn finish(&self) -> Result<(), EngineError> {
        if self.remaining() == 0 {
            Ok(())
        } else {
            corrupt()
        }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], EngineError> {
        if n > self.remaining() {
            return corrupt();
        }
        let out = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    pub(crate) fn u8(&mut self) -> Result<u8, EngineError> {
        Ok(self.take(1)?[0])
    }

    pub(crate) fn u32(&mut self) -> Result<u32, EngineError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub(crate) fn u64(&mut self) -> Result<u64, EngineError> {
        let mut a = [0u8; 8];
        a.copy_from_slice(self.take(8)?);
        Ok(u64::from_le_bytes(a))
    }

    pub(crate) fn bool(&mut self) -> Result<bool, EngineError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => corrupt(),
        }
    }

    /// A count of items each at least one byte long: never more than the bytes that remain.
    pub(crate) fn count(&mut self) -> Result<usize, EngineError> {
        let n = self.u32()? as usize;
        if n > self.remaining() {
            return corrupt();
        }
        Ok(n)
    }

    pub(crate) fn bytes(&mut self) -> Result<&'a [u8], EngineError> {
        let n = self.u32()? as usize;
        self.take(n)
    }

    pub(crate) fn str(&mut self) -> Result<String, EngineError> {
        String::from_utf8(self.bytes()?.to_vec()).or_else(|_| corrupt())
    }

    pub(crate) fn seq<T: Codec>(&mut self) -> Result<Vec<T>, EngineError> {
        let n = self.count()?;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            out.push(T::dec(self)?);
        }
        Ok(out)
    }

    pub(crate) fn opt<T: Codec>(&mut self) -> Result<Option<T>, EngineError> {
        Ok(if self.bool()? {
            Some(T::dec(self)?)
        } else {
            None
        })
    }

    pub(crate) fn get<T: Codec>(&mut self) -> Result<T, EngineError> {
        T::dec(self)
    }

    /// The enum value among `all` whose name ([`as_str`]-style `name`) is the next string.
    pub(crate) fn named<T: Copy>(
        &mut self,
        all: &[T],
        name: impl Fn(T) -> &'static str,
    ) -> Result<T, EngineError> {
        let s = self.bytes()?;
        all.iter()
            .copied()
            .find(|&v| name(v).as_bytes() == s)
            .map_or_else(corrupt, Ok)
    }
}

/// A value the engine saves.
pub(crate) trait Codec: Sized {
    fn enc(&self, e: &mut Enc);
    fn dec(d: &mut Dec) -> Result<Self, EngineError>;
}

impl Codec for u64 {
    fn enc(&self, e: &mut Enc) {
        e.u64(*self);
    }
    fn dec(d: &mut Dec) -> Result<Self, EngineError> {
        d.u64()
    }
}

impl Codec for u32 {
    fn enc(&self, e: &mut Enc) {
        e.u32(*self);
    }
    fn dec(d: &mut Dec) -> Result<Self, EngineError> {
        d.u32()
    }
}

impl Codec for bool {
    fn enc(&self, e: &mut Enc) {
        e.bool(*self);
    }
    fn dec(d: &mut Dec) -> Result<Self, EngineError> {
        d.bool()
    }
}

impl Codec for String {
    fn enc(&self, e: &mut Enc) {
        e.str(self);
    }
    fn dec(d: &mut Dec) -> Result<Self, EngineError> {
        d.str()
    }
}

impl Codec for Arc<str> {
    fn enc(&self, e: &mut Enc) {
        e.str(self);
    }
    fn dec(d: &mut Dec) -> Result<Self, EngineError> {
        Ok(d.str()?.into())
    }
}

impl<A: Codec, B: Codec> Codec for (A, B) {
    fn enc(&self, e: &mut Enc) {
        self.0.enc(e);
        self.1.enc(e);
    }
    fn dec(d: &mut Dec) -> Result<Self, EngineError> {
        Ok((A::dec(d)?, B::dec(d)?))
    }
}

impl<T: Codec> Codec for Vec<T> {
    fn enc(&self, e: &mut Enc) {
        e.seq(self);
    }
    fn dec(d: &mut Dec) -> Result<Self, EngineError> {
        d.seq()
    }
}

impl<T: Codec> Codec for Option<T> {
    fn enc(&self, e: &mut Enc) {
        e.opt(self.as_ref());
    }
    fn dec(d: &mut Dec) -> Result<Self, EngineError> {
        d.opt()
    }
}

impl Codec for Fragment {
    fn enc(&self, e: &mut Enc) {
        e.u64(self.start);
        e.u64(self.len);
    }
    fn dec(d: &mut Dec) -> Result<Self, EngineError> {
        Ok(Fragment {
            start: d.u64()?,
            len: d.u64()?,
        })
    }
}

/// Every `FileKind`; [`kinds_exhaustive`] fails to compile when a variant is added and not listed here.
pub(crate) const FILE_KINDS: [FileKind; 13] = [
    FileKind::Jpeg,
    FileKind::Mp4,
    FileKind::Mov,
    FileKind::Cr2,
    FileKind::Nef,
    FileKind::Arw,
    FileKind::Dng,
    FileKind::Orf,
    FileKind::Rw2,
    FileKind::Raf,
    FileKind::Cr3,
    FileKind::Heic,
    FileKind::Tiff,
];

#[allow(dead_code)]
fn kinds_exhaustive(k: FileKind) {
    match k {
        FileKind::Jpeg
        | FileKind::Mp4
        | FileKind::Mov
        | FileKind::Cr2
        | FileKind::Nef
        | FileKind::Arw
        | FileKind::Dng
        | FileKind::Orf
        | FileKind::Rw2
        | FileKind::Raf
        | FileKind::Cr3
        | FileKind::Heic
        | FileKind::Tiff => {}
    }
}

impl Codec for FileKind {
    fn enc(&self, e: &mut Enc) {
        e.str(self.as_str());
    }
    fn dec(d: &mut Dec) -> Result<Self, EngineError> {
        d.named(&FILE_KINDS, FileKind::as_str)
    }
}

const HEALTHS: [Health; 3] = [Health::Intact, Health::Partial, Health::Damaged];

impl Codec for Health {
    fn enc(&self, e: &mut Enc) {
        e.str(self.as_str());
    }
    fn dec(d: &mut Dec) -> Result<Self, EngineError> {
        d.named(&HEALTHS, Health::as_str)
    }
}

impl Codec for Finding {
    fn enc(&self, e: &mut Enc) {
        let Finding {
            id,
            start,
            len,
            kind,
            health,
            fragments,
            origin,
        } = self;
        e.u64(*id);
        e.u64(*start);
        e.u64(*len);
        e.put(kind);
        e.put(health);
        e.seq(fragments);
        e.opt(origin.as_ref());
    }
    fn dec(d: &mut Dec) -> Result<Self, EngineError> {
        Ok(Finding {
            id: d.u64()?,
            start: d.u64()?,
            len: d.u64()?,
            kind: d.get()?,
            health: d.get()?,
            fragments: d.seq()?,
            origin: d.opt()?,
        })
    }
}

impl Codec for ScanSummary {
    fn enc(&self, e: &mut Enc) {
        let ScanSummary {
            found,
            unreadable_bytes,
            updated,
            listing_truncated,
        } = self;
        e.u64(*found);
        e.u64(*unreadable_bytes);
        e.u64(*updated);
        e.bool(*listing_truncated);
    }
    fn dec(d: &mut Dec) -> Result<Self, EngineError> {
        Ok(ScanSummary {
            found: d.u64()?,
            unreadable_bytes: d.u64()?,
            updated: d.u64()?,
            listing_truncated: d.bool()?,
        })
    }
}

/// Event tags. `Rate` has one too, so encoding never fails, although a session never stores it (ruling C21).
const EV_VOLUME: u8 = 0;
const EV_FOUND: u8 = 1;
const EV_PROGRESS: u8 = 2;
const EV_UNREADABLE: u8 = 3;
const EV_UPDATED: u8 = 4;
const EV_DISK_FAILING: u8 = 5;
const EV_RATE: u8 = 6;

impl Codec for Event {
    fn enc(&self, e: &mut Enc) {
        match self {
            Event::Volume {
                volume,
                listing_truncated,
            } => {
                e.u8(EV_VOLUME);
                e.put(volume);
                e.bool(*listing_truncated);
            }
            Event::Found(f) => {
                e.u8(EV_FOUND);
                e.put(f);
            }
            Event::Progress { read, total } => {
                e.u8(EV_PROGRESS);
                e.u64(*read);
                e.u64(*total);
            }
            Event::Unreadable { start, len } => {
                e.u8(EV_UNREADABLE);
                e.u64(*start);
                e.u64(*len);
            }
            Event::Updated(f) => {
                e.u8(EV_UPDATED);
                e.put(f);
            }
            Event::DiskFailing {
                unreadable_runs,
                unreadable_bytes,
                read,
            } => {
                e.u8(EV_DISK_FAILING);
                e.u64(*unreadable_runs);
                e.u64(*unreadable_bytes);
                e.u64(*read);
            }
            Event::Rate {
                bytes_per_sec,
                eta_secs,
                stalls,
            } => {
                e.u8(EV_RATE);
                e.u64(*bytes_per_sec);
                e.opt(eta_secs.as_ref());
                e.u64(*stalls);
            }
        }
    }
    fn dec(d: &mut Dec) -> Result<Self, EngineError> {
        Ok(match d.u8()? {
            EV_VOLUME => Event::Volume {
                volume: d.get()?,
                listing_truncated: d.bool()?,
            },
            EV_FOUND => Event::Found(d.get()?),
            EV_PROGRESS => Event::Progress {
                read: d.u64()?,
                total: d.u64()?,
            },
            EV_UNREADABLE => Event::Unreadable {
                start: d.u64()?,
                len: d.u64()?,
            },
            EV_UPDATED => Event::Updated(d.get()?),
            EV_DISK_FAILING => Event::DiskFailing {
                unreadable_runs: d.u64()?,
                unreadable_bytes: d.u64()?,
                read: d.u64()?,
            },
            EV_RATE => Event::Rate {
                bytes_per_sec: d.u64()?,
                eta_secs: d.opt()?,
                stalls: d.u64()?,
            },
            _ => return corrupt(),
        })
    }
}

const ID_DEVICE: u8 = 0;
const ID_IMAGE: u8 = 1;
const ID_MAPPED: u8 = 2;

fn dec_identity(d: &mut Dec, depth: usize) -> Result<SourceIdentity, EngineError> {
    Ok(match d.u8()? {
        ID_DEVICE => SourceIdentity::Device {
            size: d.u64()?,
            model: d.opt()?,
            serial: d.opt()?,
            table: d.u64()?,
        },
        ID_IMAGE => SourceIdentity::Image {
            size: d.u64()?,
            digest: d.u64()?,
        },
        ID_MAPPED if depth < MAX_IDENTITY_DEPTH => SourceIdentity::Mapped {
            size: d.u64()?,
            digest: d.u64()?,
            origin: Box::new(dec_identity(d, depth + 1)?),
        },
        _ => return corrupt(),
    })
}

impl Codec for SourceIdentity {
    fn enc(&self, e: &mut Enc) {
        match self {
            SourceIdentity::Device {
                size,
                model,
                serial,
                table,
            } => {
                e.u8(ID_DEVICE);
                e.u64(*size);
                e.opt(model.as_ref());
                e.opt(serial.as_ref());
                e.u64(*table);
            }
            SourceIdentity::Image { size, digest } => {
                e.u8(ID_IMAGE);
                e.u64(*size);
                e.u64(*digest);
            }
            SourceIdentity::Mapped {
                size,
                digest,
                origin,
            } => {
                e.u8(ID_MAPPED);
                e.u64(*size);
                e.u64(*digest);
                e.put(&**origin);
            }
        }
    }
    fn dec(d: &mut Dec) -> Result<Self, EngineError> {
        dec_identity(d, 0)
    }
}

/// `v` encoded alone.
#[cfg(test)]
pub(crate) fn to_bytes<T: Codec>(v: &T) -> Vec<u8> {
    let mut e = Enc::new();
    v.enc(&mut e);
    e.into_bytes()
}

/// `bytes` decoded as one `T`, with nothing left over.
pub(crate) fn from_bytes<T: Codec>(bytes: &[u8]) -> Result<T, EngineError> {
    let mut d = Dec::new(bytes);
    let v = T::dec(&mut d)?;
    d.finish()?;
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filesystem::{FsKind, FsOrigin, FsState, FsTime, Placement, Volume};

    fn round_trip<T: Codec + PartialEq + std::fmt::Debug>(v: &T) {
        let bytes = to_bytes(v);
        let back: T = from_bytes(&bytes).unwrap();
        assert_eq!(&back, v);
        assert_eq!(to_bytes(&back), bytes);
        // Every cut is corruption, never a panic.
        for cut in 0..bytes.len() {
            assert!(
                matches!(
                    from_bytes::<T>(&bytes[..cut]),
                    Err(EngineError::SessionCorrupt)
                ),
                "cut at {cut}"
            );
        }
        let mut longer = bytes.clone();
        longer.push(0);
        assert!(from_bytes::<T>(&longer).is_err());
    }

    fn finding(id: u64, origin: Option<FsOrigin>) -> Finding {
        Finding {
            id,
            start: 4096,
            len: 2000,
            kind: FileKind::Cr3,
            health: Health::Partial,
            fragments: vec![
                Fragment {
                    start: 4096,
                    len: 1000,
                },
                Fragment {
                    start: 9000,
                    len: 1000,
                },
            ],
            origin,
        }
    }

    fn origin() -> FsOrigin {
        FsOrigin {
            volume: 1,
            folder: Some("DCIM/100MSDCF".into()),
            name: "ÉCLAIR.JPG".into(),
            state: FsState::Deleted,
            placement: Placement::Guessed,
            size: 2000,
            created: Some(FsTime {
                local: "2026-10-25T11:00:00".into(),
                utc_offset_minutes: Some(-180),
            }),
            modified: None,
        }
    }

    fn volume() -> Volume {
        Volume {
            index: 0,
            fs: FsKind::Exfat,
            offset: 4 << 20,
            len: 32 << 20,
            sector_size: 512,
            cluster_size: 32768,
            heap_offset: 5 << 20,
            cluster_count: 900,
            label: Some("CARD".into()),
            serial: 0xDEAD_BEEF,
            from_backup: true,
            scheme: crate::filesystem::Scheme::Gpt,
        }
    }

    #[test]
    fn every_kind_and_health_round_trips() {
        for k in FILE_KINDS {
            round_trip(&k);
        }
        for h in HEALTHS {
            round_trip(&h);
        }
        // An unknown name is corruption.
        let mut e = Enc::new();
        e.str("gif");
        assert!(matches!(
            from_bytes::<FileKind>(&e.into_bytes()),
            Err(EngineError::SessionCorrupt)
        ));
    }

    #[test]
    fn events_round_trip() {
        for ev in [
            Event::Volume {
                volume: volume(),
                listing_truncated: true,
            },
            Event::Found(finding(1, None)),
            Event::Found(finding(2, Some(origin()))),
            Event::Progress {
                read: 1 << 20,
                total: 8 << 20,
            },
            Event::Unreadable {
                start: 512,
                len: 1024,
            },
            Event::Updated(finding(3, Some(origin()))),
            Event::DiskFailing {
                unreadable_runs: 16,
                unreadable_bytes: 8192,
                read: 5 << 20,
            },
            Event::Rate {
                bytes_per_sec: 10,
                eta_secs: Some(3),
                stalls: 1,
            },
        ] {
            round_trip(&ev);
        }
        round_trip(&ScanSummary {
            found: 3,
            unreadable_bytes: 1024,
            updated: 1,
            listing_truncated: true,
        });
        assert!(matches!(
            from_bytes::<Event>(&[9]),
            Err(EngineError::SessionCorrupt)
        ));
    }

    #[test]
    fn identities_round_trip_with_bounded_nesting() {
        let device = SourceIdentity::Device {
            size: 1 << 30,
            model: Some("SD Reader".into()),
            serial: None,
            table: 42,
        };
        round_trip(&device);
        round_trip(&SourceIdentity::Image { size: 9, digest: 7 });
        let mut nested = device;
        for depth in 1..=MAX_IDENTITY_DEPTH {
            nested = SourceIdentity::Mapped {
                size: 1,
                digest: depth as u64,
                origin: Box::new(nested),
            };
            round_trip(&nested);
        }
        let deeper = SourceIdentity::Mapped {
            size: 1,
            digest: 0,
            origin: Box::new(nested),
        };
        assert!(matches!(
            from_bytes::<SourceIdentity>(&to_bytes(&deeper)),
            Err(EngineError::SessionCorrupt)
        ));
    }

    #[test]
    fn hostile_lengths_are_corruption() {
        // A count far past what remains, a length past the end, invalid UTF-8, a bool that is not 0/1.
        let mut e = Enc::new();
        e.u32(u32::MAX);
        assert!(from_bytes::<Vec<Fragment>>(&e.into_bytes()).is_err());
        let mut e = Enc::new();
        e.u32(100);
        e.u8(b'a');
        assert!(from_bytes::<String>(&e.into_bytes()).is_err());
        let mut e = Enc::new();
        e.bytes(&[0xFF, 0xFE]);
        assert!(from_bytes::<String>(&e.into_bytes()).is_err());
        assert!(from_bytes::<bool>(&[2]).is_err());
    }
}
