//! Durable single-writer JSONL journal for one orchestration room.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::domain::models::{
    AgentId, JournalEntry, JournalRecord, JournaledTerminalCheckpoint, NODE_JOURNAL_SCHEMA_VERSION,
    NodeCheckpoint, OrchestrationRoom, OrchestrationRoomId, RoomEvent,
};

/// Append-only journal for one orchestration room.
///
/// Appends are serialized both in-process (the async guard) and across
/// processes (an OS advisory `flock` covering the whole read-tail → repair →
/// append → fsync critical section). The sequence number is **re-derived from
/// the durable tail under the lock on every append** — never cached — so a
/// second daemon/ACP/TUI opening the same room cannot allocate a duplicate
/// `seq` and poison the log (R1: one ordered append-only journal per room).
pub struct NodeJournal {
    path: PathBuf,
    lock_path: PathBuf,
    room_id: OrchestrationRoomId,
    /// In-process serialization; cross-process safety is the file `flock`.
    append_guard: tokio::sync::Mutex<()>,
}

impl NodeJournal {
    /// Open the workspace's durable orchestration room. The deterministic
    /// workspace-derived id lets the singleton process find the same ordered
    /// log after a crash without a second mutable pointer file.
    pub async fn open_workspace(workspace: &Path) -> Result<Self, JournalError> {
        let room_id = OrchestrationRoomId::parse(format!(
            "room-{}",
            crate::infrastructure::paths::workspace_hash(workspace)
        ))
        .expect("workspace hash produces a valid room id");
        Self::open(workspace, room_id).await
    }

    pub async fn open(
        workspace: &Path,
        room_id: OrchestrationRoomId,
    ) -> Result<Self, JournalError> {
        let directory = workspace.join(".rustain").join("rooms");
        let path = directory.join(format!("{}.jsonl", room_id.as_str()));
        let lock_path = directory.join(format!("{}.lock", room_id.as_str()));

        // Create the dir + journal file and make the new directory entry itself
        // durable: without a parent-directory fsync a power loss right after the
        // first checkpoint's `sync_all` can still lose the file entry, taking
        // the "durable" checkpoint with it.
        let create_dir = directory.clone();
        let create_path = path.clone();
        tokio::task::spawn_blocking(move || -> Result<(), JournalError> {
            std::fs::create_dir_all(&create_dir)?;
            drop(open_or_create_append(&create_path)?);
            sync_directory(&create_dir)?;
            Ok(())
        })
        .await
        .expect("journal open task panicked")?;

        Ok(Self {
            path,
            lock_path,
            room_id,
            append_guard: tokio::sync::Mutex::new(()),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub async fn append_checkpoint(
        &self,
        checkpoint: NodeCheckpoint,
    ) -> Result<JournalEntry, JournalError> {
        self.append(JournalRecord::Checkpoint(checkpoint)).await
    }

    pub async fn append_room(&self, event: RoomEvent) -> Result<JournalEntry, JournalError> {
        self.append(JournalRecord::Room(event)).await
    }

    pub async fn append_alias(
        &self,
        node: AgentId,
        alias: String,
    ) -> Result<JournalEntry, JournalError> {
        self.append(JournalRecord::AliasBound { node, alias }).await
    }

    pub async fn append_successor(
        &self,
        predecessor: AgentId,
        successor: AgentId,
        alias: String,
    ) -> Result<JournalEntry, JournalError> {
        self.append(JournalRecord::Successor {
            predecessor,
            successor,
            alias,
        })
        .await
    }

    pub async fn append_obligation_accepted(
        &self,
        node: AgentId,
        correlation_id: crate::domain::models::CorrelationId,
    ) -> Result<JournalEntry, JournalError> {
        self.append(JournalRecord::ObligationAccepted {
            node,
            correlation_id,
        })
        .await
    }

    pub async fn append_obligation_discharged(
        &self,
        node: AgentId,
        correlation_id: crate::domain::models::CorrelationId,
    ) -> Result<JournalEntry, JournalError> {
        self.append(JournalRecord::ObligationDischarged {
            node,
            correlation_id,
        })
        .await
    }

    pub async fn append_obligation_violation(
        &self,
        node: AgentId,
        correlation_id: crate::domain::models::CorrelationId,
    ) -> Result<JournalEntry, JournalError> {
        self.append(JournalRecord::ObligationViolation {
            node,
            correlation_id,
        })
        .await
    }

    /// Record a hazard exactly once per node per waiting epoch. Returns the new
    /// entry, or `None` if a hazard for this `(node, waiting_since)` is already
    /// journaled (idempotent across re-evaluation and restart).
    pub async fn append_hazard_once(
        &self,
        node: AgentId,
        waiting_since: i64,
        dwell_ms: i64,
    ) -> Result<Option<JournalEntry>, JournalError> {
        let _guard = self.append_guard.lock().await;
        let path = self.path.clone();
        let lock_path = self.lock_path.clone();
        tokio::task::spawn_blocking(move || {
            let _lock = FileLock::acquire_exclusive(&lock_path)?;
            let (entries, valid_len, file_len) = parse_journal(&path)?;
            let already = entries.iter().any(|entry| {
                matches!(
                    &entry.record,
                    JournalRecord::HazardRaised { node: n, waiting_since: w, .. }
                        if *n == node && *w == waiting_since
                )
            });
            if already {
                return Ok(None);
            }
            let entry = append_records_locked(
                &path,
                &entries,
                valid_len,
                file_len,
                vec![JournalRecord::HazardRaised {
                    node,
                    waiting_since,
                    dwell_ms,
                }],
            )?
            .pop();
            Ok(entry)
        })
        .await
        .expect("journal hazard task panicked")
    }

    /// Journaled `MustReport` violations, in durable order.
    pub async fn obligation_violations(
        &self,
    ) -> Result<Vec<(AgentId, crate::domain::models::CorrelationId)>, JournalError> {
        Ok(self
            .load()
            .await?
            .into_iter()
            .filter_map(|entry| match entry.record {
                JournalRecord::ObligationViolation {
                    node,
                    correlation_id,
                } => Some((node, correlation_id)),
                _ => None,
            })
            .collect())
    }

    pub async fn append(&self, record: JournalRecord) -> Result<JournalEntry, JournalError> {
        self.append_batch(vec![record])
            .await?
            .pop()
            .ok_or(JournalError::EmptyBatch)
    }

    /// Append records under one ordering critical section and one durability
    /// sync. The `flock` guarantees the sequence re-derived from the tail is
    /// authoritative for the whole read-repair-append window.
    pub async fn append_batch(
        &self,
        records: Vec<JournalRecord>,
    ) -> Result<Vec<JournalEntry>, JournalError> {
        if records.is_empty() {
            return Err(JournalError::EmptyBatch);
        }
        let _guard = self.append_guard.lock().await;
        let path = self.path.clone();
        let lock_path = self.lock_path.clone();
        tokio::task::spawn_blocking(move || {
            let _lock = FileLock::acquire_exclusive(&lock_path)?;
            let (entries, valid_len, file_len) = parse_journal(&path)?;
            append_records_locked(&path, &entries, valid_len, file_len, records)
        })
        .await
        .expect("journal append task panicked")
    }

    /// Append a group of records as ONE atomic journal line (`JournalRecord::
    /// Batch`). Unlike `append_batch` (N lines), a crash mid-write leaves an
    /// incomplete final line that the torn-tail repair discards WHOLE — so a
    /// cascade's terminal checkpoints are all-or-nothing on recovery (R7). The
    /// records are transparently flattened back on `load`.
    pub async fn append_atomic_batch(
        &self,
        records: Vec<JournalRecord>,
    ) -> Result<JournalEntry, JournalError> {
        if records.is_empty() {
            return Err(JournalError::EmptyBatch);
        }
        self.append(JournalRecord::Batch(records)).await
    }

    /// Load the canonical prefix. A torn or malformed trailing line is ignored;
    /// corruption anywhere before the tail fails closed.
    pub async fn load(&self) -> Result<Vec<JournalEntry>, JournalError> {
        let _guard = self.append_guard.lock().await;
        let path = self.path.clone();
        let lock_path = self.lock_path.clone();
        tokio::task::spawn_blocking(move || {
            let _lock = FileLock::acquire_shared(&lock_path)?;
            let (entries, _, _) = parse_journal(&path)?;
            Ok(flatten_batches(entries))
        })
        .await
        .expect("journal load task panicked")
    }

    /// Return a prune proof only when the latest durable checkpoint for this
    /// node is terminal.
    pub async fn journaled_terminal(
        &self,
        node: &AgentId,
    ) -> Result<Option<JournaledTerminalCheckpoint>, JournalError> {
        let latest = self
            .load()
            .await?
            .into_iter()
            .filter_map(|entry| match entry.record {
                JournalRecord::Checkpoint(checkpoint) if checkpoint.id == *node => {
                    Some((entry.seq, checkpoint))
                }
                _ => None,
            })
            .next_back();
        Ok(latest.and_then(|(seq, checkpoint)| {
            checkpoint
                .state
                .is_terminal()
                .then(|| JournaledTerminalCheckpoint::new(checkpoint, seq))
        }))
    }

    /// Replay journaled room events through the production domain fold.
    /// Foreign-host registrations receive a derived unavailable marker; no
    /// live handle is fabricated and the journal remains the only store.
    pub async fn project_room(
        &self,
        current_host_id: &str,
    ) -> Result<OrchestrationRoom, JournalError> {
        let entries = self.load().await?;
        let events = entries.into_iter().filter_map(|entry| match entry.record {
            JournalRecord::Room(event) => Some(event),
            _ => None,
        });
        Ok(OrchestrationRoom::project_for_host(
            self.room_id.clone(),
            events,
            current_host_id,
        ))
    }
}

/// Story 17.2c (D4): the ledger's durable conservation-head recorder. Each
/// snapshot is written as its OWN single-record atomic batch (fsynced under the
/// cross-process flock, torn-tail-safe like every other record), so a caller's
/// write-ahead flush is all-or-nothing. Reuses the D2 batch primitive — no
/// second log.
#[async_trait::async_trait]
impl crate::domain::ports::LedgerJournalSink for NodeJournal {
    async fn journal_conservation(
        &self,
        record: crate::domain::models::LedgerConservationRecord,
    ) -> Result<(), crate::domain::ports::LedgerJournalError> {
        self.append_atomic_batch(vec![JournalRecord::LedgerConservation(record)])
            .await
            .map(|_| ())
            .map_err(|error| crate::domain::ports::LedgerJournalError(error.to_string()))
    }
}

/// Repair a torn trailing record (only the last line can be partial in an
/// append log), then append `records` with a freshly re-derived sequence.
/// The caller MUST hold the exclusive file lock.
fn append_records_locked(
    path: &Path,
    entries: &[JournalEntry],
    valid_len: usize,
    file_len: usize,
    records: Vec<JournalRecord>,
) -> Result<Vec<JournalEntry>, JournalError> {
    // A non-crash mid-write error (ENOSPC/EIO) can leave a torn tail that a
    // later append would concatenate into a corrupt middle record. Truncate to
    // the last valid offset under the lock before writing.
    if valid_len != file_len {
        let file = std::fs::OpenOptions::new().write(true).open(path)?;
        file.set_len(valid_len as u64)?;
        file.sync_all()?;
    }

    let mut seq = entries.last().map_or(Ok(1u64), |entry| {
        entry
            .seq
            .checked_add(1)
            .ok_or(JournalError::SequenceExhausted)
    })?;
    let mut out = Vec::with_capacity(records.len());
    let mut encoded = Vec::new();
    for record in records {
        let entry = JournalEntry::new(seq, record);
        encoded.extend_from_slice(&serde_json::to_vec(&entry)?);
        encoded.push(b'\n');
        out.push(entry);
        seq = seq.checked_add(1).ok_or(JournalError::SequenceExhausted)?;
    }

    let mut file = open_or_create_append(path)?;
    file.write_all(&encoded)?;
    file.sync_all()?;
    Ok(out)
}

/// An OS advisory file lock (unix `flock`). Released on drop.
struct FileLock {
    #[allow(dead_code)]
    file: std::fs::File,
}

impl FileLock {
    fn acquire_exclusive(path: &Path) -> Result<Self, JournalError> {
        Self::acquire(path, true)
    }

    fn acquire_shared(path: &Path) -> Result<Self, JournalError> {
        Self::acquire(path, false)
    }

    fn acquire(path: &Path, exclusive: bool) -> Result<Self, JournalError> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            let op = if exclusive {
                libc::LOCK_EX
            } else {
                libc::LOCK_SH
            };
            // SAFETY: fd is valid and owned by `file` for the duration.
            let ret = unsafe { libc::flock(file.as_raw_fd(), op) };
            if ret != 0 {
                return Err(JournalError::Io(std::io::Error::last_os_error()));
            }
        }
        Ok(Self { file })
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            // SAFETY: fd is valid until `file` drops after this.
            unsafe {
                libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
            }
        }
    }
}

fn open_or_create_append(path: &Path) -> Result<std::fs::File, JournalError> {
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    Ok(options.open(path)?)
}

fn sync_directory(directory: &Path) -> Result<(), JournalError> {
    #[cfg(unix)]
    {
        let handle = std::fs::File::open(directory)?;
        handle.sync_all()?;
    }
    Ok(())
}

/// Expand any atomic `JournalRecord::Batch` line into its individual records,
/// each inheriting the batch line's sequence number. Downstream folds (room
/// projection, recovery, terminal-proof, obligations) then see a flat stream
/// and need no batch awareness; the atomicity was already enforced at write
/// time (one line = all-or-nothing under the torn-tail repair).
fn flatten_batches(entries: Vec<JournalEntry>) -> Vec<JournalEntry> {
    let mut out = Vec::with_capacity(entries.len());
    for entry in entries {
        match entry.record {
            JournalRecord::Batch(records) => {
                for record in records {
                    out.push(JournalEntry {
                        schema_version: entry.schema_version,
                        seq: entry.seq,
                        record,
                    });
                }
            }
            _ => out.push(entry),
        }
    }
    out
}

fn parse_journal(path: &Path) -> Result<(Vec<JournalEntry>, usize, usize), JournalError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((Vec::new(), 0, 0));
        }
        Err(error) => return Err(error.into()),
    };
    parse_entries(&bytes)
}

fn parse_entries(bytes: &[u8]) -> Result<(Vec<JournalEntry>, usize, usize), JournalError> {
    let file_len = bytes.len();
    let mut entries = Vec::new();
    let mut valid_len = 0usize;
    let mut expected_seq = 1u64;
    let mut lines = bytes.split_inclusive(|byte| *byte == b'\n').peekable();

    while let Some(raw_line) = lines.next() {
        let is_last = lines.peek().is_none();
        let newline_terminated = raw_line.last() == Some(&b'\n');
        if is_last && !newline_terminated {
            break;
        }
        let line = raw_line.strip_suffix(b"\n").unwrap_or(raw_line);
        let parsed = serde_json::from_slice::<JournalEntry>(line);
        let entry = match parsed {
            Ok(entry) => entry,
            Err(_) if is_last => break,
            Err(source) => {
                return Err(JournalError::CorruptRecord {
                    line: entries.len() + 1,
                    source,
                });
            }
        };
        if entry.schema_version != NODE_JOURNAL_SCHEMA_VERSION {
            return Err(JournalError::UnsupportedSchema {
                found: entry.schema_version,
                supported: NODE_JOURNAL_SCHEMA_VERSION,
            });
        }
        if entry.seq != expected_seq {
            return Err(JournalError::InvalidSequence {
                expected: expected_seq,
                found: entry.seq,
            });
        }
        expected_seq = expected_seq
            .checked_add(1)
            .ok_or(JournalError::SequenceExhausted)?;
        entries.push(entry);
        valid_len += raw_line.len();
    }

    Ok((entries, valid_len, file_len))
}

#[non_exhaustive]
#[derive(Debug, Error)]
pub enum JournalError {
    #[error("journal I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("journal serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("journal record {line} is corrupt: {source}")]
    CorruptRecord {
        line: usize,
        #[source]
        source: serde_json::Error,
    },
    #[error("unsupported journal schema {found}; supported schema is {supported}")]
    UnsupportedSchema { found: u32, supported: u32 },
    #[error("journal sequence is not contiguous: expected {expected}, found {found}")]
    InvalidSequence { expected: u64, found: u64 },
    #[error("journal append batch must contain at least one record")]
    EmptyBatch,
    #[error("journal sequence space exhausted")]
    SequenceExhausted,
}

#[non_exhaustive]
#[derive(Debug, Error)]
pub enum RecoveryError {
    #[error(transparent)]
    Journal(#[from] JournalError),
    #[error("node recovery requires the daemon-singleton lock")]
    SingletonLockRequired,
    #[error("daemon singleton is already held by a live process")]
    SingletonBusy,
    #[error("daemon singleton lock I/O failed: {0}")]
    LockIo(std::io::Error),
    #[error("node restore failed: {0}")]
    Restore(String),
}
