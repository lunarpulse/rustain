//! Per-session write authority for [`FileSystemStorage`](super::FileSystemStorage) (D-24).
//!
//! One authority exists per *physical* sessions root (canonical path, so aliases collapse)
//! and *validated* session id. It lives in `<root>/.rustain-authority/`, outside both
//! transcript layouts and outside the deletable `<id>/` directory:
//!
//! * `<id>.lock`: the lock inode. `flock(LOCK_EX)` between processes, a registry mutex
//!   between tasks. Never replaced or unlinked.
//! * `<id>.seq`: eight bytes, memory-mapped by every process: the cross-process capture
//!   sequence counter (one atomic `fetch_add`, no lock, never blocks the capturing thread).
//! * `<id>.auth.json`: the [`Record`]: incarnation, accepted version, accepted sequence,
//!   tombstone, and at most one prepared operation.
//!
//! An operation runs as an *owned* task: it keeps the lock through every started phase even if
//! the caller that awaits it is dropped. A multi-file operation writes its successor record
//! with the publication witness and the remaining effects (`Prepared`) before the first
//! changed artifact, so a process that dies mid-way is recovered by the next opener, which
//! either proves the publication happened and runs the missing effects or proves nothing
//! changed. The crash class is process exit only; no `fsync` promise is made.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Weak};

use parking_lot::Mutex;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::domain::errors::StorageError;
use crate::domain::models::persist_fence::{Base, SequenceIssuer, Stamp};

/// Directory (inside the sessions root) holding every authority file.
pub(crate) const AUTHORITY_DIR: &str = ".rustain-authority";

/// Hex SHA-256 of `bytes`: the publication witness of a transcript or log.
pub(crate) fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// The writer operation that produced the current version.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WriterOp {
    pub lineage: String,
    pub op: u64,
}

/// One idempotent whole-file effect, replayable by recovery. Paths are relative to the root.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Effect {
    Write { path: String, content: String },
    Remove { path: String },
    RemoveDir { path: String },
}

/// How recovery proves the first authoritative publication happened.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Witness {
    /// The file at `path` hashes to `digest` once published.
    File { path: String, digest: String },
}

/// A prepared operation: successor record plus the evidence and effects to finish it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Prepared {
    pub op: String,
    pub witness: Witness,
    pub next: Box<Record>,
    pub effects: Vec<Effect>,
}

/// The persisted authority of one session.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Record {
    pub epoch: String,
    pub version: u64,
    #[serde(default)]
    pub tombstone: bool,
    #[serde(default)]
    pub accepted_seq: Option<u64>,
    #[serde(default)]
    pub last_writer: Option<WriterOp>,
    /// Version at which the current run of one writer lineage's operations began: that
    /// lineage's captures based on any version from here on are its own successors.
    #[serde(default)]
    pub run_start: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prepared: Option<Box<Prepared>>,
    /// Effects of an already published version that still have to finish.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending: Vec<Effect>,
}

impl Record {
    pub(crate) fn base(&self) -> Base {
        Base::Accepted {
            epoch: self.epoch.clone(),
            version: self.version,
        }
    }
}

/// What kind of write is asking for admission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Intent {
    /// Fenced save of a conversation: the stamp decides, and the capture must also be newer
    /// than the writer lineage's last accepted operation (older bytes never replace newer).
    Save,
    /// An operation that rests on the accepted history a capture names but writes none of
    /// the caller's bytes (checkpoint, truncate, fork, rewind): a competing writer refuses
    /// it, the capture's own lineage having been accepted since does not.
    Dependent,
    /// Explicit creation: also allowed over a tombstone.
    Create,
    /// Local derived operation (truncate, checkpoint, fork source): no stamp, only liveness.
    Derived,
}

/// The decision of an admission check.
#[derive(Debug)]
pub(crate) struct Admit {
    /// A new incarnation starts (first accepted write, or re-creation after delete).
    pub new_epoch: bool,
    /// The stamp was accepted as the validated causal successor of its own lineage.
    pub chained: bool,
    /// The operation does not change the transcript's rows and names no writer, so the last
    /// stamped writer's run continues through it (an id-only checkpoint).
    pub keep_writer: bool,
}

/// Admit or refuse a write under authority.
pub(crate) fn admit(
    record: Option<&Record>,
    transcript_exists: bool,
    stamp: Option<&Stamp>,
    intent: Intent,
) -> Result<Admit, StorageError> {
    let stale = |why: &str| Err(StorageError::Stale(why.to_string()));
    match record {
        None => match (intent, stamp.map(|s| &s.base)) {
            (Intent::Derived, _) if transcript_exists => Ok(Admit {
                new_epoch: true,
                chained: false,
                keep_writer: false,
            }),
            (Intent::Derived, _) => stale("session does not exist"),
            (_, Some(Base::Legacy)) if transcript_exists => Ok(Admit {
                new_epoch: true,
                chained: false,
                keep_writer: false,
            }),
            (_, Some(Base::Absent)) if !transcript_exists => Ok(Admit {
                new_epoch: true,
                chained: false,
                keep_writer: false,
            }),
            // Accepted by another sessions root (the lineage came from a different store) and
            // nothing here: a creation in this root, not an update of an existing session.
            (_, Some(Base::Accepted { .. })) if !transcript_exists => Ok(Admit {
                new_epoch: true,
                chained: false,
                keep_writer: false,
            }),
            (_, Some(Base::Absent)) => stale("the session exists now; this capture saw it missing"),
            (_, _) => stale("the session changed identity since this capture was loaded"),
        },
        Some(rec) if rec.tombstone => match (intent, stamp.map(|s| &s.base)) {
            (Intent::Create, Some(Base::Absent)) => Ok(Admit {
                new_epoch: true,
                chained: false,
                keep_writer: false,
            }),
            _ => stale("the session was deleted"),
        },
        Some(rec) => {
            let Some(stamp) = stamp else {
                return Ok(Admit {
                    new_epoch: false,
                    chained: false,
                    keep_writer: false,
                });
            };
            let own = rec.last_writer.as_ref().is_some_and(|w| {
                *w.lineage == *stamp.lineage && (intent != Intent::Save || w.op < stamp.op)
            });
            let (same_base, own_run) = match &stamp.base {
                Base::Accepted { epoch, version } => {
                    if *epoch != rec.epoch {
                        return stale("the session was deleted and recreated since this capture");
                    }
                    (*version == rec.version, own && rec.run_start <= *version)
                }
                // First observation of this session: only its own lineage's creation chains.
                Base::Absent | Base::Legacy => (false, own && rec.run_start == 0),
            };
            if !same_base && !own_run {
                return stale("an older snapshot cannot replace newer accepted history");
            }
            if intent == Intent::Save
                && let (Some(seq), Some(accepted)) = (stamp.seq, rec.accepted_seq)
                && seq <= accepted
            {
                return stale("a capture not newer than the last accepted one");
            }
            // The writer's run continues through every operation of its own lineage, on the
            // current base or not: only another writer (or a stampless operation) starts a
            // new run. Restarting it on a same-base write of the same lineage would fence
            // that lineage's still-live captures after two of its own saves.
            Ok(Admit {
                new_epoch: false,
                chained: own_run,
                keep_writer: false,
            })
        }
    }
}

/// The successor record of an admitted operation.
pub(crate) fn next_record(
    record: Option<&Record>,
    admit: &Admit,
    stamp: Option<&Stamp>,
    seq: u64,
) -> Record {
    let writer = stamp.map(|s| WriterOp {
        lineage: s.lineage.to_string(),
        op: s.op,
    });
    match record {
        // An id-only checkpoint names no writer and leaves the transcript's rows alone: the
        // version advances, and the last stamped writer's run continues through it. Any other
        // lineage is refused by the base it no longer matches.
        Some(rec) if !admit.new_epoch && admit.keep_writer => Record {
            epoch: rec.epoch.clone(),
            version: rec.version + 1,
            tombstone: false,
            accepted_seq: Some(rec.accepted_seq.map_or(seq, |a| a.max(seq))),
            last_writer: rec.last_writer.clone(),
            run_start: rec.run_start,
            prepared: None,
            pending: Vec::new(),
        },
        Some(rec) if !admit.new_epoch => Record {
            epoch: rec.epoch.clone(),
            version: rec.version + 1,
            tombstone: false,
            accepted_seq: Some(rec.accepted_seq.map_or(seq, |a| a.max(seq))),
            last_writer: writer,
            run_start: if admit.chained {
                rec.run_start
            } else {
                rec.version
            },
            prepared: None,
            pending: Vec::new(),
        },
        _ => Record {
            epoch: nanoid::nanoid!(),
            version: 1,
            tombstone: false,
            accepted_seq: Some(seq),
            last_writer: writer,
            run_start: 0,
            prepared: None,
            pending: Vec::new(),
        },
    }
}

/// The tombstone that replaces a live record on delete.
pub(crate) fn tombstone_record(record: Option<&Record>) -> Record {
    let (epoch, version, accepted_seq) = match record {
        Some(r) => (r.epoch.clone(), r.version + 1, r.accepted_seq),
        None => (nanoid::nanoid!(), 1, None),
    };
    Record {
        epoch,
        version,
        tombstone: true,
        accepted_seq,
        last_writer: None,
        run_start: version - 1,
        prepared: None,
        pending: Vec::new(),
    }
}

// ── sequence cell ────────────────────────────────────────────────────────────

/// Cross-process monotonic counter backed by a mapped file.
#[derive(Debug)]
pub(crate) struct SeqCell {
    #[cfg(unix)]
    ptr: *mut u64,
    #[cfg(not(unix))]
    local: std::sync::atomic::AtomicU64,
}

// SAFETY: the mapped word is only touched through atomic operations.
#[cfg(unix)]
unsafe impl Send for SeqCell {}
#[cfg(unix)]
unsafe impl Sync for SeqCell {}

#[cfg(unix)]
impl SeqCell {
    fn open(path: &Path, create: bool) -> std::io::Result<Option<Self>> {
        use std::os::unix::fs::OpenOptionsExt;
        use std::os::unix::io::AsRawFd;
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(create)
            .mode(0o600)
            .open(path)
        {
            Ok(file) => file,
            Err(e) if !create && e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let fd = file.as_raw_fd();
        // SAFETY: `fd` is valid for the call; growing a file to eight bytes is idempotent
        // and race-free between processes (a concurrent grow to the same size is a no-op).
        if file.metadata()?.len() < 8 && unsafe { libc::ftruncate(fd, 8) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: maps eight bytes of a file we just sized to eight bytes; the mapping
        // outlives the descriptor (MAP_SHARED keeps the file referenced).
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                8,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Some(Self { ptr: ptr.cast() }))
    }

    fn next(&self) -> u64 {
        // SAFETY: `ptr` is an eight-byte, eight-aligned (page-aligned) shared mapping.
        unsafe { std::sync::atomic::AtomicU64::from_ptr(self.ptr) }
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1
    }
}

#[cfg(unix)]
impl Drop for SeqCell {
    fn drop(&mut self) {
        // SAFETY: unmaps the exact mapping created in `open`.
        unsafe {
            libc::munmap(self.ptr.cast(), 8);
        }
    }
}

#[cfg(not(unix))]
impl SeqCell {
    fn open(_path: &Path, _create: bool) -> std::io::Result<Option<Self>> {
        Ok(Some(Self {
            local: Default::default(),
        }))
    }

    fn next(&self) -> u64 {
        self.local.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1
    }
}

/// The store's capture-sequence source for one session; arms a lineage.
#[derive(Debug)]
pub(crate) struct Issuer {
    cell: Arc<SeqCell>,
}

impl SequenceIssuer for Issuer {
    fn issue(&self) -> Option<u64> {
        Some(self.cell.next())
    }
}

// ── registry and locking ─────────────────────────────────────────────────────

type Key = (PathBuf, String);
type Registry<T> = LazyLock<Mutex<HashMap<Key, Weak<T>>>>;

static MUTEXES: Registry<tokio::sync::Mutex<()>> = LazyLock::new(Default::default);
static CELLS: Registry<SeqCell> = LazyLock::new(Default::default);

fn shared<T>(
    registry: &Registry<T>,
    key: &Key,
    make: impl FnOnce() -> std::io::Result<T>,
) -> std::io::Result<Arc<T>> {
    let mut map = registry.lock();
    if let Some(live) = map.get(key).and_then(Weak::upgrade) {
        return Ok(live);
    }
    map.retain(|_, weak| weak.strong_count() > 0);
    let fresh = Arc::new(make()?);
    map.insert(key.clone(), Arc::downgrade(&fresh));
    Ok(fresh)
}

#[cfg(unix)]
struct LockFile(std::fs::File);

/// The result of one lock attempt.
enum Lock {
    Held(LockFile),
    /// The lock file does not exist (and was not asked to be created).
    Missing,
    /// Another holder has it (non-blocking attempts only).
    Busy,
}

#[cfg(unix)]
fn flock(path: &Path, create: bool, wait: bool) -> std::io::Result<Lock> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .truncate(false)
        .mode(0o600)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if !create && e.kind() == std::io::ErrorKind::NotFound => return Ok(Lock::Missing),
        Err(e) => return Err(e),
    };
    let op = if wait {
        libc::LOCK_EX
    } else {
        libc::LOCK_EX | libc::LOCK_NB
    };
    loop {
        // SAFETY: the descriptor is valid and owned by `file` for the whole call.
        if unsafe { libc::flock(file.as_raw_fd(), op) } == 0 {
            return Ok(Lock::Held(LockFile(file)));
        }
        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(libc::EWOULDBLOCK) if !wait => return Ok(Lock::Busy),
            _ => return Err(error),
        }
    }
}

#[cfg(not(unix))]
struct LockFile;

#[cfg(not(unix))]
fn flock(_path: &Path, _create: bool, _wait: bool) -> std::io::Result<Lock> {
    Ok(Lock::Held(LockFile))
}

/// How an opener wants the authority.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Open {
    /// Writer: create the authority files, wait for the holder.
    Write,
    /// Reader: use the authority only if it exists, wait for the holder.
    Existing,
    /// Recovery sweep: use it only if it exists and nobody holds it right now.
    TryExisting,
}

/// Exclusive hold on one session's authority, with its record loaded.
pub(crate) struct Held {
    root: PathBuf,
    id: String,
    record: Option<Record>,
    cell: Option<Arc<SeqCell>>,
    _task: tokio::sync::OwnedMutexGuard<()>,
    _flock: Option<LockFile>,
}

fn io_err(what: &str, e: impl std::fmt::Display) -> StorageError {
    StorageError::IoError(format!("{what}: {e}"))
}

/// The validated physical key of a session: canonical root + valid id.
async fn key_for(sessions_dir: &Path, id: &str, create: bool) -> Result<Option<Key>, StorageError> {
    let id = crate::infrastructure::utils::sanitize_id(id)
        .map_err(|_| StorageError::InvalidId(id.chars().take(64).collect()))?
        .to_string();
    if create {
        tokio::fs::create_dir_all(sessions_dir.join(AUTHORITY_DIR))
            .await
            .map_err(|e| io_err("create authority dir", e))?;
    }
    match tokio::fs::canonicalize(sessions_dir).await {
        Ok(root) => Ok(Some((root, id))),
        Err(e) if !create && e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_err("resolve sessions root", e)),
    }
}

/// The outcome of opening an authority.
pub(crate) enum Opened {
    Held(Held),
    /// Another operation holds it right now (recovery sweep only).
    Busy,
    /// This session never had an authority.
    Absent,
}

async fn open(sessions_dir: &Path, id: &str, how: Open) -> Result<Opened, StorageError> {
    let create = how == Open::Write;
    let Some(key) = key_for(sessions_dir, id, create).await? else {
        return Ok(Opened::Absent);
    };
    let mutex = shared(&MUTEXES, &key, || Ok(tokio::sync::Mutex::new(())))
        .map_err(|e| io_err("authority registry", e))?;
    let guard = if how == Open::TryExisting {
        match mutex.try_lock_owned() {
            Ok(guard) => guard,
            Err(_) => return Ok(Opened::Busy),
        }
    } else {
        mutex.lock_owned().await
    };
    let lock_path = key.0.join(AUTHORITY_DIR).join(format!("{}.lock", key.1));
    let wait = how != Open::TryExisting;
    let locked = tokio::task::spawn_blocking(move || flock(&lock_path, create, wait))
        .await
        .map_err(|e| io_err("authority lock task", e))?
        .map_err(|e| io_err("lock session authority", e))?;
    let locked = match locked {
        Lock::Held(locked) => locked,
        Lock::Missing => return Ok(Opened::Absent),
        Lock::Busy => return Ok(Opened::Busy),
    };
    let mut held = Held {
        root: key.0,
        id: key.1,
        record: None,
        cell: None,
        _task: guard,
        _flock: Some(locked),
    };
    held.record = held.read_record().await?;
    Ok(Opened::Held(held))
}

/// Acquire the authority for a write: waits for the holder, creates the files.
pub(crate) async fn acquire(sessions_dir: &Path, id: &str) -> Result<Held, StorageError> {
    match open(sessions_dir, id, Open::Write).await? {
        Opened::Held(held) => Ok(held),
        _ => Err(StorageError::Other("authority unavailable".into())),
    }
}

/// Acquire the authority for a read, or `None` when this session never had one.
pub(crate) async fn acquire_existing(
    sessions_dir: &Path,
    id: &str,
) -> Result<Option<Held>, StorageError> {
    Ok(match open(sessions_dir, id, Open::Existing).await? {
        Opened::Held(held) => Some(held),
        _ => None,
    })
}

/// Acquire an existing authority only if no operation holds it right now.
pub(crate) async fn try_acquire_existing(
    sessions_dir: &Path,
    id: &str,
) -> Result<Opened, StorageError> {
    open(sessions_dir, id, Open::TryExisting).await
}

impl Held {
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    /// The record, unless it only carries a prepared marker of a first write.
    pub(crate) fn live(&self) -> Option<&Record> {
        self.record.as_ref().filter(|r| !r.epoch.is_empty())
    }

    /// The raw record, including a marker with no accepted state yet.
    pub(crate) fn record(&self) -> Option<&Record> {
        self.record.as_ref()
    }

    fn record_path(&self) -> PathBuf {
        self.root
            .join(AUTHORITY_DIR)
            .join(format!("{}.auth.json", self.id))
    }

    async fn read_record(&self) -> Result<Option<Record>, StorageError> {
        match tokio::fs::read(self.record_path()).await {
            Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|e| {
                StorageError::Indeterminate(format!(
                    "authority record for {} is unreadable: {e}",
                    self.id
                ))
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io_err("read authority record", e)),
        }
    }

    /// Replace the record (atomic rename) and remember it.
    pub(crate) async fn write_record(&mut self, record: Record) -> Result<(), StorageError> {
        let bytes = serde_json::to_vec(&record)
            .map_err(|e| StorageError::SerializationError(e.to_string()))?;
        super::FileSystemStorage::atomic_write(self.record_path(), bytes)
            .await
            .map_err(|(_, e)| io_err("write authority record", e))?;
        self.record = Some(record);
        Ok(())
    }

    fn cell(&mut self, create: bool) -> Result<Option<Arc<SeqCell>>, StorageError> {
        if let Some(cell) = &self.cell {
            return Ok(Some(cell.clone()));
        }
        let key = (self.root.clone(), self.id.clone());
        let path = self
            .root
            .join(AUTHORITY_DIR)
            .join(format!("{}.seq", self.id));
        let found = {
            let map = CELLS.lock();
            map.get(&key).and_then(Weak::upgrade)
        };
        let cell = match found {
            Some(cell) => Some(cell),
            None => {
                match SeqCell::open(&path, create).map_err(|e| io_err("open sequence cell", e))? {
                    Some(opened) => Some(
                        shared(&CELLS, &key, || Ok(opened))
                            .map_err(|e| io_err("sequence registry", e))?,
                    ),
                    None => None,
                }
            }
        };
        self.cell.clone_from(&cell);
        Ok(cell)
    }

    /// A sequence for an unarmed capture, issued under the lock at admission.
    pub(crate) fn issue_seq(&mut self) -> Result<u64, StorageError> {
        self.cell(true)?
            .map(|cell| cell.next())
            .ok_or_else(|| StorageError::Other("sequence cell unavailable".into()))
    }

    /// The issuer that arms a lineage once this session has an accepted record.
    pub(crate) fn issuer(&mut self) -> Option<Arc<dyn SequenceIssuer>> {
        self.cell(false)
            .ok()
            .flatten()
            .map(|cell| Arc::new(Issuer { cell }) as Arc<dyn SequenceIssuer>)
    }

    /// Resolve a root-relative effect path, refusing anything that escapes the root.
    pub(crate) fn resolve(&self, relative: &str) -> Result<PathBuf, StorageError> {
        let path = Path::new(relative);
        if path.is_absolute()
            || path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(StorageError::Indeterminate(format!(
                "effect path escapes the sessions root: {relative}"
            )));
        }
        Ok(self.root.join(path))
    }

    /// Run effects in order. Whole-file, idempotent; a failure stops at that effect.
    pub(crate) async fn run_effects(&self, effects: &[Effect]) -> Result<(), StorageError> {
        for effect in effects {
            match effect {
                Effect::Write { path, content } => {
                    let target = self.resolve(path)?;
                    if let Some(parent) = target.parent() {
                        tokio::fs::create_dir_all(parent)
                            .await
                            .map_err(|e| io_err("create effect dir", e))?;
                    }
                    super::FileSystemStorage::atomic_write(target, content.clone().into_bytes())
                        .await
                        .map_err(|(_, e)| io_err("write effect", e))?;
                }
                Effect::Remove { path } => {
                    match tokio::fs::remove_file(self.resolve(path)?).await {
                        Ok(()) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => return Err(io_err("remove effect", e)),
                    }
                }
                Effect::RemoveDir { path } => {
                    match tokio::fs::remove_dir_all(self.resolve(path)?).await {
                        Ok(()) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => return Err(io_err("remove dir effect", e)),
                    }
                }
            }
        }
        Ok(())
    }

    /// Whether the witness file currently hashes to the prepared digest.
    pub(crate) async fn witness_published(&self, witness: &Witness) -> Result<bool, StorageError> {
        let Witness::File { path, digest: want } = witness;
        match tokio::fs::read(self.resolve(path)?).await {
            Ok(bytes) => Ok(digest(&bytes) == *want),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(io_err("read witness file", e)),
        }
    }
}

/// Run `op` as an owned task holding the authority: dropping the caller that awaits the
/// result does not stop the operation or release the lock until it finishes.
pub(crate) async fn owned<T, F, Fut>(held: Held, op: F) -> Result<T, StorageError>
where
    T: Send + 'static,
    F: FnOnce(Held) -> Fut + Send + 'static,
    Fut: Future<Output = Result<T, StorageError>> + Send + 'static,
{
    tokio::spawn(op(held)).await.map_err(|e| {
        StorageError::Indeterminate(format!("session operation did not finish: {e}"))
    })?
}

/// Test-only fault and pause points inside a commit, keyed by the canonical sessions root so
/// parallel tests never share one. `Fail` returns early *without cleanup* (a process that died
/// there); `Pause` parks the operation at the point until released.
#[cfg(test)]
pub(crate) mod fault {
    use parking_lot::Mutex;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::LazyLock;

    pub(crate) enum Action {
        Fail,
        Pause {
            reached: tokio::sync::oneshot::Sender<()>,
            release: tokio::sync::oneshot::Receiver<()>,
        },
    }

    static ARMED: LazyLock<Mutex<HashMap<(PathBuf, &'static str), Action>>> =
        LazyLock::new(Default::default);

    pub(crate) fn arm(sessions_dir: &Path, point: &'static str, action: Action) {
        let root = std::fs::canonicalize(sessions_dir).expect("sessions dir exists before arming");
        ARMED.lock().insert((root, point), action);
    }

    /// `true` when the operation must stop here as if the process had died.
    pub(crate) async fn hit(root: &Path, point: &'static str) -> bool {
        let action = ARMED.lock().remove(&(root.to_path_buf(), point));
        match action {
            None => false,
            Some(Action::Fail) => true,
            Some(Action::Pause { reached, release }) => {
                let _ = reached.send(());
                let _ = release.await;
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::persist_fence::PersistTag;
    use tempfile::TempDir;

    fn stamp(tag: &PersistTag) -> Stamp {
        tag.capture()
    }

    #[test]
    fn a_missing_record_admits_only_the_matching_first_observation() {
        let absent = stamp(&PersistTag::loaded(Base::Absent));
        let legacy = stamp(&PersistTag::loaded(Base::Legacy));
        assert!(
            admit(None, false, Some(&absent), Intent::Save)
                .unwrap()
                .new_epoch
        );
        assert!(matches!(
            admit(None, true, Some(&absent), Intent::Save),
            Err(StorageError::Stale(_))
        ));
        assert!(
            admit(None, true, Some(&legacy), Intent::Save)
                .unwrap()
                .new_epoch
        );
        assert!(matches!(
            admit(None, false, Some(&legacy), Intent::Save),
            Err(StorageError::Stale(_))
        ));
    }

    #[test]
    fn a_deleted_session_refuses_everything_but_explicit_creation() {
        let tomb = tombstone_record(None);
        let absent = stamp(&PersistTag::loaded(Base::Absent));
        let old = stamp(&PersistTag::loaded(tomb.base()));
        assert!(matches!(
            admit(Some(&tomb), false, Some(&old), Intent::Save),
            Err(StorageError::Stale(_))
        ));
        assert!(matches!(
            admit(Some(&tomb), false, Some(&absent), Intent::Save),
            Err(StorageError::Stale(_))
        ));
        assert!(
            admit(Some(&tomb), false, Some(&absent), Intent::Create)
                .unwrap()
                .new_epoch
        );
    }

    #[derive(Debug)]
    struct Fixed(u64);
    impl SequenceIssuer for Fixed {
        fn issue(&self) -> Option<u64> {
            Some(self.0)
        }
    }

    /// The sequence is compared on its own: a capture on the current base whose sequence is not
    /// newer than the last accepted one is refused even though the base check alone passes.
    #[test]
    fn an_old_sequence_is_refused_even_on_the_current_base() {
        let rec = Record {
            epoch: "e".into(),
            version: 3,
            accepted_seq: Some(10),
            ..Default::default()
        };
        let armed = |seq: u64| {
            let issuer: Arc<dyn SequenceIssuer> = Arc::new(Fixed(seq));
            PersistTag::loaded_armed(rec.base(), Some(issuer)).capture()
        };
        assert!(
            matches!(
                admit(Some(&rec), true, Some(&armed(10)), Intent::Save),
                Err(StorageError::Stale(_))
            ),
            "equal sequence"
        );
        assert!(
            matches!(
                admit(Some(&rec), true, Some(&armed(4)), Intent::Save),
                Err(StorageError::Stale(_))
            ),
            "older sequence"
        );
        assert!(
            admit(Some(&rec), true, Some(&armed(11)), Intent::Save).is_ok(),
            "newer sequence on the current base"
        );
        assert!(
            admit(Some(&rec), true, Some(&armed(4)), Intent::Dependent).is_ok(),
            "a dependent operation writes none of the caller's bytes"
        );
    }

    #[test]
    fn a_live_record_needs_the_current_base_and_a_newer_sequence() {
        let rec = Record {
            epoch: "e".into(),
            version: 3,
            accepted_seq: Some(10),
            ..Default::default()
        };
        let tag = PersistTag::loaded(rec.base());
        let current = stamp(&tag);
        assert!(admit(Some(&rec), true, Some(&current), Intent::Save).is_ok());
        let old = stamp(&PersistTag::loaded(Base::Accepted {
            epoch: "e".into(),
            version: 2,
        }));
        assert!(matches!(
            admit(Some(&rec), true, Some(&old), Intent::Save),
            Err(StorageError::Stale(_))
        ));
        let other = stamp(&PersistTag::loaded(Base::Accepted {
            epoch: "x".into(),
            version: 3,
        }));
        assert!(matches!(
            admit(Some(&rec), true, Some(&other), Intent::Save),
            Err(StorageError::Stale(_))
        ));
    }

    #[tokio::test]
    async fn an_invalid_id_is_refused_and_creates_nothing() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("sessions");
        for bad in ["", "../x", "a/b", "a b"] {
            assert!(
                matches!(acquire(&root, bad).await, Err(StorageError::InvalidId(_))),
                "{bad}"
            );
        }
        assert!(!root.exists(), "a refused id touches nothing");
    }

    #[tokio::test]
    async fn a_symlink_alias_of_the_root_shares_one_authority() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("sessions");
        std::fs::create_dir_all(&root).unwrap();
        let alias = dir.path().join("alias");
        std::os::unix::fs::symlink(&root, &alias).unwrap();
        let held = acquire(&root, "s1").await.unwrap();
        let blocked = try_acquire_existing(&alias, "s1").await.unwrap();
        assert!(
            matches!(blocked, Opened::Busy),
            "the alias resolves to the held authority"
        );
        drop(held);
        // A blocking open: a concurrently forking test may briefly share the released lock's
        // open file description until its exec, which a non-blocking attempt would report busy.
        assert!(acquire_existing(&alias, "s1").await.unwrap().is_some());
        let other = try_acquire_existing(&alias, "s2").await.unwrap();
        assert!(
            matches!(other, Opened::Absent),
            "another id has no authority yet"
        );
    }

    #[tokio::test]
    async fn the_sequence_is_shared_by_every_mapping_of_the_cell() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("sessions");
        let mut held = acquire(&root, "s1").await.unwrap();
        let first = held.issue_seq().unwrap();
        // A second, independent mapping of the same file (as a second process has).
        let path = held.root().join(AUTHORITY_DIR).join("s1.seq");
        let other = SeqCell::open(&path, false).unwrap().unwrap();
        let second = other.next();
        let third = held.issue_seq().unwrap();
        assert!(first < second && second < third, "{first} {second} {third}");
    }

    #[tokio::test]
    async fn an_owned_operation_outlives_its_dropped_caller() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("sessions");
        let held = acquire(&root, "s1").await.unwrap();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let caller = tokio::spawn(owned(held, move |mut held| async move {
            let _ = started_tx.send(());
            let _ = release_rx.await;
            held.write_record(Record {
                epoch: "e".into(),
                version: 1,
                ..Default::default()
            })
            .await?;
            let _ = done_tx.send(());
            Ok(())
        }));
        started_rx.await.unwrap();
        caller.abort();
        let _ = caller.await;
        assert!(
            matches!(
                try_acquire_existing(&root, "s1").await.unwrap(),
                Opened::Busy
            ),
            "still owned"
        );
        release_tx.send(()).unwrap();
        done_rx.await.unwrap();
        let held = acquire(&root, "s1").await.unwrap();
        assert_eq!(held.record().unwrap().version, 1);
    }
}
