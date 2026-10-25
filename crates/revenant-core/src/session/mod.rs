//! A saved scan (ruling D25, design §5.5): a scan that writes, into a folder of its own, what it needs to be taken up
//! again after a cancel, a disconnected card, a crash or a power loss, with exactly the events of an uninterrupted scan;
//! and a finished scan's results, read back without the source.
//!
//! A session is an empty folder chosen by the caller (checked with [`Destination::check`]: never on the source) holding
//! `session.rvs` (the source's identity, the sector and block sizes, file systems on or off), `listing.rvs` (the
//! file-system listing, saved once), `events.rvs` (every event but `Rate`, appended as it is emitted) and two
//! checkpoints written alternately, `checkpoint-a.rvs` and `checkpoint-b.rvs` (a generation, the log's length and event
//! count at that point, and the scan's state or, once finished, its summary). Opening takes the newest valid
//! checkpoint and drops what the log holds past it. See [`store`] for the files and how they are written.

mod store;

use std::cell::RefCell;
use std::io::{BufReader, BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::codec::{Codec, Dec, Enc, from_bytes};
use crate::device::SystemDevices;
use crate::filesystem::Listing;
use crate::formats::Format;
use crate::scan::{Checkpoint, Resume, ScanState, scan_with};
use crate::source::Source;
use crate::{
    CancelToken, Destination, EngineError, Event, ScanOptions, ScanSummary, SourceIdentity,
};
use store::{CHECKPOINTS, EVENTS, HEADER_LEN, Kind, LISTING, MANIFEST};

/// How often a running session saves a checkpoint.
#[derive(Clone, Debug)]
pub struct SessionOptions {
    /// After this many bytes scanned since the last checkpoint (default 256 MiB)...
    pub checkpoint_bytes: u64,
    /// ...or this much time (default 30 s), whichever comes first, at the next block boundary.
    pub checkpoint_every: Duration,
}

impl Default for SessionOptions {
    fn default() -> Self {
        Self {
            checkpoint_bytes: 256 << 20,
            checkpoint_every: Duration::from_secs(30),
        }
    }
}

/// Where a session stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionState {
    /// Nothing saved to resume from: the scan starts from the beginning.
    Fresh,
    /// Stopped with `read` of `total` bytes scanned at its last checkpoint.
    Interrupted { read: u64, total: u64 },
    /// The scan finished with this summary: its results are read back without the source.
    Finished(ScanSummary),
}

/// What `session.rvs` records.
struct Manifest {
    identity: SourceIdentity,
    sector: u64,
    block: u64,
    filesystems: bool,
}

impl Codec for Manifest {
    fn enc(&self, e: &mut Enc) {
        let Manifest {
            identity,
            sector,
            block,
            filesystems,
        } = self;
        e.put(identity);
        e.u64(*sector);
        e.u64(*block);
        e.bool(*filesystems);
    }
    fn dec(d: &mut Dec) -> Result<Self, EngineError> {
        Ok(Manifest {
            identity: d.get()?,
            sector: d.u64()?,
            block: d.u64()?,
            filesystems: d.bool()?,
        })
    }
}

/// A checkpoint: its generation, the log's length and event count at that point, and either the scan's state (to
/// resume from) or its summary (finished).
struct Saved {
    generation: u64,
    log_len: u64,
    events: u64,
    finished: Option<ScanSummary>,
    state: Option<ScanState>,
}

impl Codec for Saved {
    fn enc(&self, e: &mut Enc) {
        let Saved {
            generation,
            log_len,
            events,
            finished,
            state,
        } = self;
        e.u64(*generation);
        e.u64(*log_len);
        e.u64(*events);
        e.put(finished);
        e.put(state);
    }
    fn dec(d: &mut Dec) -> Result<Self, EngineError> {
        let saved = Saved {
            generation: d.get()?,
            log_len: d.get()?,
            events: d.get()?,
            finished: d.get()?,
            state: d.get()?,
        };
        // Exactly one of the two, and a state that agrees with the count.
        let consistent = match (&saved.finished, &saved.state) {
            (Some(_), None) => true,
            (None, Some(s)) => s.events() == saved.events,
            _ => false,
        };
        if !consistent || saved.generation == 0 || saved.log_len < HEADER_LEN {
            return Err(EngineError::SessionCorrupt);
        }
        Ok(saved)
    }
}

/// What the newest checkpoint says, in memory.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Point {
    /// 0 before the first checkpoint.
    generation: u64,
    log_len: u64,
    events: u64,
    state: SessionState,
}

impl Point {
    fn fresh() -> Self {
        Point {
            generation: 0,
            log_len: HEADER_LEN,
            events: 0,
            state: SessionState::Fresh,
        }
    }
}

/// A saved scan in its folder. Made with [`Session::create`] or reopened with [`Session::open`]; run (or resumed, or
/// replayed when finished) with [`scan_session`].
pub struct Session {
    dir: PathBuf,
    manifest: Manifest,
    point: Point,
    /// The checked folder: the one it was created or reopened ([`Session::open_checked`]) with; `None` for a session
    /// reopened with [`Session::open`].
    dest: Option<Destination>,
    /// What an interrupted session resumes from, when already read.
    resume: Option<Resume>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("dir", &self.dir)
            .field("identity", &self.manifest.identity)
            .field("state", &self.point.state)
            .field("replay_len", &self.point.events)
            .finish_non_exhaustive()
    }
}

impl Session {
    /// A new session in `dest.dir()`, which must be empty (`session_not_empty`). Stores `identity` and `opts.sector`,
    /// `opts.block` and `opts.filesystems` (ruling D25): a resume always scans with these. Refuses
    /// (`destination_on_source`) a folder no longer on the volume it was checked on; `write_failed` when a file cannot
    /// be written (or the identity nests deeper than a session reads back).
    pub fn create(
        dest: &Destination,
        identity: SourceIdentity,
        opts: &ScanOptions,
    ) -> Result<Session, EngineError> {
        dest.still_checked()?;
        let dir = dest.dir();
        let mut entries = std::fs::read_dir(dir).map_err(|cause| EngineError::Open { cause })?;
        if entries.next().is_some() {
            return Err(EngineError::SessionNotEmpty);
        }
        let manifest = Manifest {
            identity,
            sector: opts.sector,
            block: opts.block as u64,
            filesystems: opts.filesystems,
        };
        let payload = encode(&manifest);
        // A manifest that saves always reads back (an identity nested too deep would not).
        if from_bytes::<Manifest>(&payload).is_err() {
            return Err(EngineError::Write {
                cause: std::io::Error::from(std::io::ErrorKind::InvalidInput),
            });
        }
        // The log first, the manifest last: a folder with a manifest is a whole session.
        store::create(dir, EVENTS, &store::header(Kind::Events))?;
        store::create(dir, MANIFEST, &store::framed(Kind::Manifest, &payload))?;
        store::sync_dir(dir);
        Ok(Session {
            dir: dir.to_path_buf(),
            manifest,
            point: Point::fresh(),
            dest: Some(dest.clone()),
            resume: None,
        })
    }

    /// Reopens the session in `dir`: the newest valid checkpoint, and the event log cut to it. `open_failed` when there
    /// is no session there (no manifest) or it cannot be read; `session_version` when its manifest was written by a
    /// version this engine cannot read; `session_corrupt` when the manifest or the log's header is damaged, when
    /// checkpoints exist but none is valid, or when the listing a checkpoint needs is missing or damaged. A damaged
    /// newest checkpoint falls back to the other one. Writes nothing.
    pub fn open(dir: &Path) -> Result<Session, EngineError> {
        let manifest = read_manifest(dir)?.ok_or_else(|| EngineError::Open {
            cause: std::io::Error::from(std::io::ErrorKind::NotFound),
        })?;
        let log_len = log_file_len(dir)?;
        let mut newest: Option<Saved> = None;
        let mut found = false;
        for name in CHECKPOINTS {
            let saved = match store::read_file(&dir.join(name), store::MAX_CHECKPOINT_FILE) {
                Ok(None) => continue,
                Ok(Some(bytes)) => parse_checkpoint(&bytes),
                Err(e) => Err(e),
            };
            found = true;
            if let Ok(s) = saved
                && s.log_len <= log_len
                && newest.as_ref().is_none_or(|n| s.generation > n.generation)
            {
                newest = Some(s);
            }
        }
        let point = match &newest {
            None if found => return Err(EngineError::SessionCorrupt),
            None => Point::fresh(),
            Some(s) => Point {
                generation: s.generation,
                log_len: s.log_len,
                events: s.events,
                state: match (&s.finished, &s.state) {
                    (Some(summary), _) => SessionState::Finished(summary.clone()),
                    (None, Some(state)) => SessionState::Interrupted {
                        read: state.position(),
                        total: state.total(),
                    },
                    (None, None) => return Err(EngineError::SessionCorrupt),
                },
            },
        };
        let resume = match newest.and_then(|s| s.state) {
            Some(state) => Some(Resume {
                listing: read_listing(dir)?,
                state,
            }),
            None => None,
        };
        Ok(Session {
            dir: dir.to_path_buf(),
            manifest,
            point,
            dest: None,
            resume,
        })
    }

    /// Reopens the session in `dest.dir()` like [`Session::open`], with its folder already checked by the caller
    /// ([`Destination::check`] against the source it will be run on, with the caller's [`crate::device::DeviceOps`]):
    /// the way to resume a device's session. Writes nothing.
    pub fn open_checked(dest: &Destination) -> Result<Session, EngineError> {
        let mut session = Session::open(dest.dir())?;
        session.dest = Some(dest.clone());
        Ok(session)
    }

    /// The session's folder.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The source the session was made from.
    pub fn identity(&self) -> &SourceIdentity {
        &self.manifest.identity
    }

    pub fn state(&self) -> &SessionState {
        &self.point.state
    }

    /// Events stored up to the checkpoint: the first `replay_len()` events of [`scan_session`] are these, replayed.
    /// `Rate` events are never stored and never counted (ruling P4).
    pub fn replay_len(&self) -> u64 {
        self.point.events
    }

    /// The stored events, without any source (a finished session's results). `session_corrupt` when the log does not
    /// hold them intact.
    pub fn events(&self) -> Result<Vec<Event>, EngineError> {
        let mut out = Vec::new();
        self.replay(&mut |e| out.push(e))?;
        Ok(out)
    }

    /// Hands every stored event to `on_event`, in order.
    fn replay(&self, on_event: &mut dyn FnMut(Event)) -> Result<(), EngineError> {
        let file =
            store::open_regular(&self.dir.join(EVENTS))?.ok_or(EngineError::SessionCorrupt)?;
        let mut reader = BufReader::with_capacity(1 << 20, file);
        let mut head = [0u8; HEADER_LEN as usize];
        std::io::Read::read_exact(&mut reader, &mut head)
            .map_err(|_| EngineError::SessionCorrupt)?;
        store::check_header(Kind::Events, &head)?;
        let mut records = store::Records::new(reader, self.point.log_len - HEADER_LEN);
        let mut count = 0u64;
        while let Some(data) = records.next_record()? {
            let event: Event = from_bytes(&data)?;
            if matches!(event, Event::Rate { .. }) || count == self.point.events {
                return Err(EngineError::SessionCorrupt);
            }
            count += 1;
            on_event(event);
        }
        if count != self.point.events {
            return Err(EngineError::SessionCorrupt);
        }
        Ok(())
    }

    /// The scan options a run of this session uses: its own sector, block and file systems, the rest from `opts`.
    fn options(&self, opts: &ScanOptions) -> Result<ScanOptions, EngineError> {
        let m = &self.manifest;
        Ok(ScanOptions {
            sector: m.sector,
            block: usize::try_from(m.block).map_err(|_| EngineError::SessionCorrupt)?,
            filesystems: m.filesystems,
            cache_bytes: opts.cache_bytes,
            threads: opts.threads,
            rates: opts.rates,
            #[cfg(test)]
            no_reuse: opts.no_reuse,
            #[cfg(test)]
            drop_fs_jobs: opts.drop_fs_jobs,
        })
    }

    /// What an interrupted session resumes from (read again from the folder when a failed run used it up).
    fn take_resume(&mut self) -> Result<Option<Resume>, EngineError> {
        if !matches!(self.point.state, SessionState::Interrupted { .. }) {
            return Ok(None);
        }
        if self.resume.is_none() {
            let again = Session::open(&self.dir)?;
            if again.point != self.point {
                return Err(EngineError::SessionCorrupt);
            }
            self.resume = again.resume;
        }
        self.resume
            .take()
            .map(Some)
            .ok_or(EngineError::SessionCorrupt)
    }
}

fn encode<T: Codec>(v: &T) -> Vec<u8> {
    let mut e = Enc::new();
    v.enc(&mut e);
    e.into_bytes()
}

/// The manifest in `dir`; `None` when there is none.
fn read_manifest(dir: &Path) -> Result<Option<Manifest>, EngineError> {
    match store::read_file(&dir.join(MANIFEST), store::MAX_MANIFEST_FILE)? {
        None => Ok(None),
        Some(bytes) => from_bytes(store::unframe(Kind::Manifest, &bytes)?).map(Some),
    }
}

/// The event log's length, its header checked.
fn log_file_len(dir: &Path) -> Result<u64, EngineError> {
    let mut file = store::open_regular(&dir.join(EVENTS))?.ok_or(EngineError::SessionCorrupt)?;
    let mut head = [0u8; HEADER_LEN as usize];
    std::io::Read::read_exact(&mut file, &mut head).map_err(|_| EngineError::SessionCorrupt)?;
    store::check_header(Kind::Events, &head)?;
    let len = file
        .metadata()
        .map_err(|cause| EngineError::Open { cause })?
        .len();
    Ok(len)
}

fn parse_checkpoint(bytes: &[u8]) -> Result<Saved, EngineError> {
    from_bytes(store::unframe(Kind::Checkpoint, bytes)?)
}

fn read_listing(dir: &Path) -> Result<Listing, EngineError> {
    let bytes = store::read_file(&dir.join(LISTING), store::MAX_LISTING_FILE)?
        .ok_or(EngineError::SessionCorrupt)?;
    from_bytes(store::unframe(Kind::Listing, &bytes)?)
}

/// The identity a session in `dir` was made from; `None` when `dir` holds no session (no manifest, or no folder).
pub(crate) fn saved_identity(dir: &Path) -> Result<Option<SourceIdentity>, EngineError> {
    Ok(read_manifest(dir)?.map(|m| m.identity))
}

/// For the pre-flight: `Ok` when `dir` holds no session or a session of `identity` (`same_source`);
/// `session_other_source` for a session of another source; `session_version` or `session_corrupt` for a manifest that
/// cannot be read (the pre-flight reports both as `Issue::SessionCorrupt`, ruling C11). Only the manifest is read.
pub fn check_session(dir: &Path, identity: &SourceIdentity) -> Result<(), EngineError> {
    match saved_identity(dir)? {
        Some(saved) if !saved.same_source(identity) => Err(EngineError::SessionOtherSource),
        _ => Ok(()),
    }
}

/// The running session's writer: the event log, appended as events come, and the checkpoints.
struct Writer<'a> {
    dest: Destination,
    log: BufWriter<std::fs::File>,
    log_len: u64,
    events: u64,
    /// The first write that failed: the next checkpoint returns it.
    failed: Option<EngineError>,
    point: Point,
    sopts: &'a SessionOptions,
    last_read: u64,
    last_at: Instant,
}

impl<'a> Writer<'a> {
    /// The log reopened and cut to the session's checkpoint (whatever lies past it is dropped).
    fn open(
        dest: Destination,
        point: Point,
        sopts: &'a SessionOptions,
        from: u64,
    ) -> Result<Self, EngineError> {
        let write = |cause| EngineError::Write { cause };
        dest.still_checked()?;
        let mut file = store::open_log(&dest.dir().join(EVENTS))?;
        file.set_len(point.log_len).map_err(write)?;
        file.seek(SeekFrom::Start(point.log_len)).map_err(write)?;
        Ok(Writer {
            dest,
            log: BufWriter::with_capacity(1 << 20, file),
            log_len: point.log_len,
            events: point.events,
            failed: None,
            point,
            sopts,
            last_read: from,
            last_at: Instant::now(),
        })
    }

    /// Appends `event` (never a `Rate`) to the log; a failure is kept for the next checkpoint.
    fn append(&mut self, event: &Event) {
        if self.failed.is_some() {
            return;
        }
        let data = encode(event);
        let result = if data.len() > store::MAX_RECORD {
            Err(std::io::Error::from(std::io::ErrorKind::InvalidInput))
        } else {
            self.log.write_all(&store::record(&data))
        };
        match result {
            Ok(()) => {
                self.log_len += 8 + data.len() as u64;
                self.events += 1;
            }
            Err(cause) => self.failed = Some(EngineError::Write { cause }),
        }
    }

    fn check(&mut self) -> Result<(), EngineError> {
        match self.failed.take() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn listing(&mut self, listing: &Listing) -> Result<(), EngineError> {
        self.check()?;
        self.dest.still_checked()?;
        store::replace(
            self.dest.dir(),
            LISTING,
            &store::framed(Kind::Listing, &encode(listing)),
        )
    }

    /// Whether a checkpoint is due at the boundary `read`.
    fn due(&self, read: u64) -> bool {
        read.saturating_sub(self.last_read) >= self.sopts.checkpoint_bytes
            || self.last_at.elapsed() >= self.sopts.checkpoint_every
    }

    /// Saves a checkpoint of everything logged so far: the log synced first, then the checkpoint written over the
    /// older of the two.
    fn checkpoint(
        &mut self,
        events: u64,
        finished: Option<ScanSummary>,
        state: Option<ScanState>,
    ) -> Result<(), EngineError> {
        let write = |cause| EngineError::Write { cause };
        self.check()?;
        debug_assert_eq!(events, self.events, "every event but Rate is logged");
        if events != self.events {
            return Err(EngineError::SessionCorrupt);
        }
        self.dest.still_checked()?;
        self.log.flush().map_err(write)?;
        self.log.get_ref().sync_data().map_err(write)?;
        let generation = self.point.generation + 1;
        let new_state = match (&finished, &state) {
            (Some(summary), _) => SessionState::Finished(summary.clone()),
            (None, Some(s)) => SessionState::Interrupted {
                read: s.position(),
                total: s.total(),
            },
            (None, None) => return Err(EngineError::SessionCorrupt),
        };
        let saved = Saved {
            generation,
            log_len: self.log_len,
            events,
            finished,
            state,
        };
        store::replace(
            self.dest.dir(),
            CHECKPOINTS[(generation % 2) as usize],
            &store::framed(Kind::Checkpoint, &encode(&saved)),
        )?;
        self.point = Point {
            generation,
            log_len: self.log_len,
            events,
            state: new_state,
        };
        self.last_at = Instant::now();
        Ok(())
    }
}

/// Runs the session: replays the stored events, then scans on from the checkpoint (or from the start), checkpointing
/// by `sopts`, and saves the summary once the scan finishes. `identity` must be `same_source` as the session's
/// (`session_other_source`, checked before anything is read or written). The session's sector, block and file systems
/// win over `opts`; threads, cache size and rates come from `opts`. A finished session only replays: the source is not
/// read and nothing is written.
///
/// The session writes only into its checked folder: the [`Destination`] it was created or reopened
/// ([`Session::open_checked`]) with. A session reopened with [`Session::open`] has none: for an image source its folder
/// is checked now with the volume rule alone (no system asked); for a device source (`source.disk()` is `Some`) it is
/// refused (`destination_unknown`) before anything is read or written, since only the caller's
/// [`crate::device::DeviceOps`] can tell which disks the folder lives on.
///
/// A checkpoint is saved at a block boundary once `sopts.checkpoint_bytes` were scanned or `sopts.checkpoint_every`
/// passed since the last one, after the pairing step, and at the end. A cancel or a vanished source seen at a block
/// boundary saves one there; elsewhere the last one stands, and a resume emits again (identically) what came after it.
/// On any error the session stays at its last checkpoint, ready to be run again.
#[allow(clippy::too_many_arguments)]
pub fn scan_session(
    source: &dyn Source,
    identity: &SourceIdentity,
    formats: &[Box<dyn Format>],
    opts: &ScanOptions,
    session: &mut Session,
    sopts: &SessionOptions,
    cancel: &CancelToken,
    on_event: &mut dyn FnMut(Event),
) -> Result<ScanSummary, EngineError> {
    if !session.manifest.identity.same_source(identity) {
        return Err(EngineError::SessionOtherSource);
    }
    if let SessionState::Finished(summary) = &session.point.state {
        session.replay(on_event)?;
        return Ok(summary.clone());
    }
    let opts = session.options(opts)?;
    let dest = match &session.dest {
        Some(d) => d.clone(),
        None if source.disk().is_some() => return Err(EngineError::DestinationUnknown),
        // An image source: only the volume rule applies, and `Destination::check` never calls the ops.
        None => Destination::check(&SystemDevices, source, &session.dir)?,
    };
    let resume = session.take_resume()?;
    let from = resume.as_ref().map_or(0, |r| r.state.position());
    let writer = match Writer::open(dest, session.point.clone(), sopts, from) {
        Ok(w) => w,
        Err(e) => {
            session.resume = resume;
            return Err(e);
        }
    };
    if let Err(e) = session.replay(on_event) {
        session.resume = resume;
        return Err(e);
    }
    let writer = RefCell::new(writer);
    let mut on = |event: Event| {
        if !matches!(event, Event::Rate { .. }) {
            writer.borrow_mut().append(&event);
        }
        on_event(event);
    };
    let mut hook = |c: Checkpoint| -> Result<(), EngineError> {
        let mut w = writer.borrow_mut();
        match c {
            Checkpoint::Listing(listing) => w.listing(listing),
            Checkpoint::Boundary {
                read,
                events,
                state,
                stopping,
            } => {
                w.check()?;
                if stopping || w.due(read) {
                    w.checkpoint(events, None, Some(state()))?;
                    w.last_read = read;
                }
                Ok(())
            }
            Checkpoint::Paired { events, state } => w.checkpoint(events, None, Some(state())),
        }
    };
    let result = scan_with(
        source,
        formats,
        &opts,
        cancel,
        &mut on,
        resume,
        Some(&mut hook),
    );
    let mut w = writer.into_inner();
    let result = result.and_then(|summary| {
        let events = w.events;
        w.checkpoint(events, Some(summary.clone()), None)?;
        Ok(summary)
    });
    session.point = w.point.clone();
    session.dest = Some(w.dest.clone());
    session.resume = None;
    result
}

#[cfg(test)]
mod tests;
