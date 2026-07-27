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
    /// Story 18.2 (AC2, P-2). The sole source of `JournalEntry::
    /// recorded_at_ms`. Each append reads the wall clock once inside the
    /// critical section, then clamps it to the last durable nonlegacy stamp.
    /// Emitters never supply a timestamp, so correct writers cannot persist a
    /// descending nonlegacy time while `seq` advances.
    clock: std::sync::Arc<dyn crate::domain::clock::Clock>,
    /// Story 18.2 structural ratchet — see [`NodeJournal::stamp_reads`].
    #[cfg(any(test, feature = "test-instrumentation"))]
    stamp_reads: std::sync::Arc<std::sync::atomic::AtomicU64>,
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
            clock: std::sync::Arc::new(crate::domain::clock::SystemClock::default()),
            #[cfg(any(test, feature = "test-instrumentation"))]
            stamp_reads: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        })
    }

    /// Replace the wall clock used to stamp `recorded_at_ms`.
    ///
    /// Builder rather than an `open` parameter: production always wants
    /// `SystemClock`, and threading a clock argument through 60-odd call sites
    /// to serve determinism in a handful of tests buys nothing. Follows the
    /// `NodeTree::with_journal` / `with_host_binding` convention.
    #[must_use]
    pub fn with_clock(mut self, clock: std::sync::Arc<dyn crate::domain::clock::Clock>) -> Self {
        self.clock = clock;
        self
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
        let clock = self.clock.clone();
        #[cfg(any(test, feature = "test-instrumentation"))]
        let stamp_reads = self.stamp_reads.clone();
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
            let recorded_at_ms = clamp_recorded_at_ms(&entries, clock.wall_now_ms());
            #[cfg(any(test, feature = "test-instrumentation"))]
            stamp_reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
                recorded_at_ms,
            )?
            .pop();
            Ok(entry)
        })
        .await
        .expect("journal hazard task panicked")
    }

    /// Atomically claim an entire recovered park set under the journal's
    /// cross-process file lock. The operation is compare-and-append: every node
    /// must still be parked and have no unexpired claim owned by another
    /// process, otherwise nothing is written.
    pub async fn claim_parks(
        &self,
        nodes: &[AgentId],
        claim_id: AgentId,
        now_ms: i64,
        lease_duration_ms: i64,
    ) -> Result<bool, JournalError> {
        if nodes.is_empty() {
            return Ok(true);
        }
        let nodes = nodes.to_vec();
        let expires_at_ms = now_ms.saturating_add(lease_duration_ms.max(1));
        let _guard = self.append_guard.lock().await;
        let path = self.path.clone();
        let lock_path = self.lock_path.clone();
        let clock = self.clock.clone();
        #[cfg(any(test, feature = "test-instrumentation"))]
        let stamp_reads = self.stamp_reads.clone();
        tokio::task::spawn_blocking(move || {
            let _lock = FileLock::acquire_exclusive(&lock_path)?;
            let (entries, valid_len, file_len) = parse_journal(&path)?;
            let flat = flatten_batches(entries.clone());
            let mut parked = std::collections::BTreeSet::<AgentId>::new();
            let mut claims = std::collections::BTreeMap::<AgentId, (AgentId, i64)>::new();
            for entry in flat {
                match entry.record {
                    JournalRecord::Parked { node, .. } => {
                        parked.insert(node.clone());
                        claims.remove(&node);
                    }
                    JournalRecord::Unparked { node } => {
                        parked.remove(&node);
                        claims.remove(&node);
                    }
                    JournalRecord::ParkClaimed {
                        node,
                        claim_id,
                        expires_at_ms,
                    } if parked.contains(&node) => {
                        claims.insert(node, (claim_id, expires_at_ms));
                    }
                    JournalRecord::ParkClaimReleased { node, claim_id } => {
                        if claims
                            .get(&node)
                            .is_some_and(|(owner, _)| *owner == claim_id)
                        {
                            claims.remove(&node);
                        }
                    }
                    _ => {}
                }
            }
            let unavailable = nodes.iter().any(|node| {
                !parked.contains(node)
                    || claims
                        .get(node)
                        .is_some_and(|(owner, expires)| *owner != claim_id && *expires > now_ms)
            });
            if unavailable {
                return Ok(false);
            }
            let records = nodes
                .into_iter()
                .filter(|node| {
                    !claims
                        .get(node)
                        .is_some_and(|(owner, expires)| *owner == claim_id && *expires > now_ms)
                })
                .map(|node| JournalRecord::ParkClaimed {
                    node,
                    claim_id: claim_id.clone(),
                    expires_at_ms,
                })
                .collect::<Vec<_>>();
            if !records.is_empty() {
                let recorded_at_ms = clamp_recorded_at_ms(&entries, clock.wall_now_ms());
                #[cfg(any(test, feature = "test-instrumentation"))]
                stamp_reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                append_records_locked(
                    &path,
                    &entries,
                    valid_len,
                    file_len,
                    vec![JournalRecord::Batch(records)],
                    recorded_at_ms,
                )?;
            }
            Ok(true)
        })
        .await
        .expect("journal park-claim task panicked")
    }

    /// Release a failed resume attempt without consuming the parked records.
    /// A release is owner-qualified, so it cannot clear a newer claimant.
    pub async fn release_park_claims(
        &self,
        nodes: &[AgentId],
        claim_id: &AgentId,
    ) -> Result<(), JournalError> {
        if nodes.is_empty() {
            return Ok(());
        }
        self.append_atomic_batch(
            nodes
                .iter()
                .map(|node| JournalRecord::ParkClaimReleased {
                    node: node.clone(),
                    claim_id: claim_id.clone(),
                })
                .collect(),
        )
        .await
        .map(|_| ())
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
        let clock = self.clock.clone();
        #[cfg(any(test, feature = "test-instrumentation"))]
        let stamp_reads = self.stamp_reads.clone();
        tokio::task::spawn_blocking(move || {
            let _lock = FileLock::acquire_exclusive(&lock_path)?;
            let (entries, valid_len, file_len) = parse_journal(&path)?;
            // Story 18.2 (AC2, P-2): ONE clock read, inside the flock, after
            // the tail (and therefore `seq`) is known. Clamp it to the last
            // durable nonlegacy timestamp so a wall-clock rollback cannot
            // produce descending persisted time.
            let recorded_at_ms = clamp_recorded_at_ms(&entries, clock.wall_now_ms());
            #[cfg(any(test, feature = "test-instrumentation"))]
            stamp_reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            append_records_locked(
                &path,
                &entries,
                valid_len,
                file_len,
                records,
                recorded_at_ms,
            )
        })
        .await
        .expect("journal append task panicked")
    }

    /// Story 18.2 structural ratchet (Rule 4): how many times this journal has
    /// read its clock. The invariant "`seq` order never contradicts
    /// `recorded_at_ms` order" cannot be raced into failure — `flock`
    /// serializes correct code — so it is proven by counting instead: exactly
    /// one stamp per append batch, taken inside the lock. A mutant that stamps
    /// per record, or that stamps at the emitter, changes this count.
    ///
    /// Per-instance, never a process-global static: a ratchet an unrelated
    /// test can trip is not a ratchet.
    #[cfg(any(test, feature = "test-instrumentation"))]
    #[must_use]
    pub fn stamp_reads(&self) -> u64 {
        self.stamp_reads.load(std::sync::atomic::Ordering::Relaxed)
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

/// Read-only workspace journal opener for observer surfaces.
///
/// Unlike [`NodeJournal::open_workspace`], constructing this reader performs
/// no filesystem writes: it does not create `.rustain/`, the room journal, or
/// its advisory lock file. A missing journal is the honest empty history for a
/// workspace that has never orchestrated a subagent.
#[derive(Clone, Debug)]
pub struct WorkspaceJournalReader {
    path: PathBuf,
    lock_path: PathBuf,
}

impl WorkspaceJournalReader {
    /// Address the workspace's deterministic room journal without opening it.
    #[must_use]
    pub fn open_workspace(workspace: &Path) -> Self {
        let room_id = OrchestrationRoomId::parse(format!(
            "room-{}",
            crate::infrastructure::paths::workspace_hash(workspace)
        ))
        .expect("workspace hash produces a valid room id");
        Self::open(workspace, room_id)
    }

    /// Address one room journal without creating or modifying it.
    #[must_use]
    pub fn open(workspace: &Path, room_id: OrchestrationRoomId) -> Self {
        let directory = workspace.join(".rustain").join("rooms");
        Self {
            path: directory.join(format!("{}.jsonl", room_id.as_str())),
            lock_path: directory.join(format!("{}.lock", room_id.as_str())),
        }
    }

    fn load_entries_blocking(&self) -> Result<Vec<JournalEntry>, JournalError> {
        // Do not even open the lock path when the journal does not exist: a
        // first `team log` must leave an empty workspace byte-for-byte alone.
        match std::fs::File::open(&self.path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        }

        // Current writers create and lock this sidecar before appending. Legacy
        // journals may predate it; read those without manufacturing a lock
        // file. When it exists, open it read-only so a shared flock still
        // brackets the durable point-in-time read without truncating it.
        let _lock = FileLock::acquire_existing_shared(&self.lock_path)?;
        let (entries, _, _) = parse_journal(&self.path)?;
        Ok(flatten_batches(entries))
    }
}

#[async_trait::async_trait]
impl crate::domain::ports::RoomJournalReader for WorkspaceJournalReader {
    async fn load_entries(
        &self,
    ) -> Result<Vec<JournalEntry>, crate::domain::ports::RoomJournalError> {
        let reader = self.clone();
        tokio::task::spawn_blocking(move || reader.load_entries_blocking())
            .await
            .expect("read-only journal load task panicked")
            .map_err(|error| crate::domain::ports::RoomJournalError::Read(error.to_string()))
    }
}

/// Read side of the room journal (Story 18.2, AC3).
///
/// Exists so `adapters/tui` and `adapters/cli` can fold the durable stream
/// without holding a concrete `NodeJournal` — the same inversion
/// `NodeRoomJournal` performs for the write side.
///
/// `load()` is O(whole file) on every call: there is no tail read and no
/// index. A refreshing viewer must not poll it in a hot loop.
#[async_trait::async_trait]
impl crate::domain::ports::RoomJournalReader for NodeJournal {
    async fn load_entries(
        &self,
    ) -> Result<Vec<JournalEntry>, crate::domain::ports::RoomJournalError> {
        self.load()
            .await
            .map_err(|error| crate::domain::ports::RoomJournalError::Read(error.to_string()))
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
    recorded_at_ms: i64,
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
        let entry = JournalEntry::new(seq, record, recorded_at_ms);
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

/// Clamp a new wall-clock reading to the last durable nonlegacy timestamp.
///
/// The caller holds the cross-process append lock, so the tail inspected here
/// is the tail this append will follow. `0` is the legacy "timestamp absent"
/// sentinel and deliberately does not constrain a current clock reading.
fn clamp_recorded_at_ms(entries: &[JournalEntry], wall_now_ms: i64) -> i64 {
    entries
        .iter()
        .rev()
        .find(|entry| entry.has_timestamp())
        .map_or(wall_now_ms, |entry| wall_now_ms.max(entry.recorded_at_ms))
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

    /// Acquire a shared lock only if a writer has already created its sidecar.
    ///
    /// Observer paths must not create or truncate a lock merely to inspect a
    /// journal. A missing sidecar is valid for a readable legacy journal.
    fn acquire_existing_shared(path: &Path) -> Result<Option<Self>, JournalError> {
        let file = match std::fs::OpenOptions::new().read(true).open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            // SAFETY: fd is valid and owned by `file` for the duration.
            let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH) };
            if ret != 0 {
                return Err(JournalError::Io(std::io::Error::last_os_error()));
            }
        }
        Ok(Some(Self { file }))
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
/// each inheriting the batch line's sequence number **and timestamp**.
/// Downstream folds (room projection, recovery, terminal-proof, obligations)
/// then see a flat stream and need no batch awareness; the atomicity was
/// already enforced at write time (one line = all-or-nothing under the
/// torn-tail repair).
fn flatten_batches(entries: Vec<JournalEntry>) -> Vec<JournalEntry> {
    let mut out = Vec::with_capacity(entries.len());
    for entry in entries {
        match entry.record {
            JournalRecord::Batch(records) => {
                for record in records {
                    out.push(JournalEntry {
                        schema_version: entry.schema_version,
                        seq: entry.seq,
                        recorded_at_ms: entry.recorded_at_ms,
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

#[cfg(test)]
mod park_claim_tests {
    use super::*;

    fn parked_spec(id: AgentId) -> crate::domain::models::orchestration::SpokeSpec {
        crate::domain::models::orchestration::SpokeSpec {
            id,
            label: "parked".into(),
            prompt: "resume".into(),
            effective_model: "test".into(),
            tier: crate::domain::models::ModelTier::Flagship,
            tools_allow: crate::domain::models::ToolPolicy::InheritFromParent,
            waits_for: vec![AgentId::new()],
            role: crate::domain::models::orchestration::SpokeRole::Leaf,
        }
    }

    #[tokio::test]
    async fn park_claim_is_cross_instance_owner_qualified_and_expiring() {
        let workspace = tempfile::tempdir().unwrap();
        let journal_a = NodeJournal::open_workspace(workspace.path()).await.unwrap();
        let journal_b = NodeJournal::open_workspace(workspace.path()).await.unwrap();
        let node = AgentId::new();
        journal_a
            .append(JournalRecord::Parked {
                node: node.clone(),
                producers: Vec::new(),
                spec: parked_spec(AgentId::new()),
                concurrency: 1,
            })
            .await
            .unwrap();

        let claim_a = AgentId::new();
        let claim_b = AgentId::new();
        let claim_c = AgentId::new();
        assert!(
            journal_a
                .claim_parks(std::slice::from_ref(&node), claim_a.clone(), 0, 100)
                .await
                .unwrap()
        );
        assert!(
            !journal_b
                .claim_parks(std::slice::from_ref(&node), claim_b.clone(), 50, 100)
                .await
                .unwrap(),
            "a live claim excludes another process"
        );
        assert!(
            journal_b
                .claim_parks(std::slice::from_ref(&node), claim_b.clone(), 101, 100)
                .await
                .unwrap(),
            "an expired owner can be replaced"
        );
        journal_a
            .release_park_claims(std::slice::from_ref(&node), &claim_a)
            .await
            .unwrap();
        assert!(
            !journal_a
                .claim_parks(std::slice::from_ref(&node), claim_c.clone(), 102, 100)
                .await
                .unwrap(),
            "a stale release cannot erase the newer owner"
        );
        journal_b
            .release_park_claims(std::slice::from_ref(&node), &claim_b)
            .await
            .unwrap();
        assert!(
            journal_a
                .claim_parks(std::slice::from_ref(&node), claim_c, 102, 100)
                .await
                .unwrap()
        );
    }
}

#[cfg(test)]
mod workspace_reader_tests {
    use super::*;
    use crate::domain::ports::RoomJournalReader as _;

    fn write_one_entry(reader: &WorkspaceJournalReader) -> Vec<u8> {
        std::fs::create_dir_all(reader.path.parent().unwrap()).unwrap();
        let entry = JournalEntry::new(
            1,
            JournalRecord::AliasBound {
                node: AgentId::new(),
                alias: "read-only-fixture".to_owned(),
            },
            1_700_000_000_000,
        );
        let mut bytes = serde_json::to_vec(&entry).unwrap();
        bytes.push(b'\n');
        std::fs::write(&reader.path, &bytes).unwrap();
        bytes
    }

    #[tokio::test]
    async fn read_only_workspace_reader_leaves_a_missing_workspace_unmodified() {
        let workspace = tempfile::tempdir().unwrap();
        let reader = WorkspaceJournalReader::open_workspace(workspace.path());

        assert!(reader.load_entries().await.unwrap().is_empty());
        assert!(
            !workspace.path().join(".rustain").exists(),
            "a read-only observer must not create a workspace, journal, or lock"
        );
    }

    #[tokio::test]
    async fn read_only_workspace_reader_reads_without_creating_or_truncating_locks() {
        let workspace = tempfile::tempdir().unwrap();
        let reader = WorkspaceJournalReader::open_workspace(workspace.path());
        let journal_bytes = write_one_entry(&reader);

        let entries = reader.load_entries().await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(std::fs::read(&reader.path).unwrap(), journal_bytes);
        assert!(
            !reader.lock_path.exists(),
            "a readable legacy journal must not need a lock file"
        );

        std::fs::write(&reader.lock_path, b"do-not-truncate").unwrap();
        assert_eq!(reader.load_entries().await.unwrap().len(), 1);
        assert_eq!(
            std::fs::read(&reader.lock_path).unwrap(),
            b"do-not-truncate",
            "shared observer locking must not truncate an existing writer lock"
        );
    }
}

/// Concrete [`crate::domain::ports::RoomJournal`] over a `NodeJournal` +
/// the domain-event bus (Story 17.5a, ADR-17-5-01 D2). Durable-first,
/// bus-second: the journal append must succeed before the bus emit is
/// attempted, matching `orchestrator::persist_room_event`.
pub struct NodeRoomJournal {
    journal: std::sync::Arc<NodeJournal>,
    event_tx: Option<tokio::sync::mpsc::UnboundedSender<crate::domain::events::AppEvent>>,
}

impl NodeRoomJournal {
    pub fn new(
        journal: std::sync::Arc<NodeJournal>,
        event_tx: Option<tokio::sync::mpsc::UnboundedSender<crate::domain::events::AppEvent>>,
    ) -> Self {
        Self { journal, event_tx }
    }
}

#[async_trait::async_trait]
impl crate::domain::ports::RoomJournal for NodeRoomJournal {
    async fn record_event(
        &self,
        event: RoomEvent,
    ) -> Result<(), crate::domain::ports::RoomJournalError> {
        use crate::domain::ports::RoomJournalError;
        self.journal
            .append_room(event.clone())
            .await
            .map_err(|error| RoomJournalError::Append(error.to_string()))?;
        if let Some(tx) = &self.event_tx {
            let _ = tx.send(crate::domain::events::AppEvent::DomainEvent(event.into()));
        }
        Ok(())
    }
}

/// 17.5b — `ArtifactSink` impl backed by the real `ArtifactStore` +
/// `NodeJournal`. The composition root supplies the coordinator `authority`
/// and `host` (orchestrator-only fields the MCP adapter cannot reach — story
/// Task 6 / C4). Writes `ArtifactCreated` then `TicketAssigned`,
/// durable-first / bus-second, mirroring `persist_room_event`.
pub struct JournalArtifactSink {
    store: std::sync::Arc<dyn crate::domain::ports::ArtifactStore>,
    room: std::sync::Arc<dyn crate::domain::ports::RoomJournal>,
    authority: crate::domain::models::CapabilityTokenId,
    host: crate::domain::models::HostBinding,
}

impl JournalArtifactSink {
    pub fn new(
        store: std::sync::Arc<dyn crate::domain::ports::ArtifactStore>,
        room: std::sync::Arc<dyn crate::domain::ports::RoomJournal>,
        authority: crate::domain::models::CapabilityTokenId,
        host: crate::domain::models::HostBinding,
    ) -> Self {
        Self {
            store,
            room,
            authority,
            host,
        }
    }
}

#[async_trait::async_trait]
impl crate::domain::ports::ArtifactSink for JournalArtifactSink {
    async fn write_input_request(
        &self,
        producer: &crate::domain::models::AgentId,
        node: &crate::domain::models::AgentId,
        body: serde_json::Value,
    ) -> Result<crate::domain::models::ArtifactId, crate::domain::ports::ArtifactSinkError> {
        use crate::domain::models::{ArtifactKind, EvidenceArtifactDraft, RoomEvent};
        use crate::domain::ports::{ArtifactSinkError, RoomJournal};
        let bytes = serde_json::to_vec(&body)
            .map_err(|e| ArtifactSinkError::Write(format!("serialize body: {e}")))?;
        let artifact = self
            .store
            .put(
                EvidenceArtifactDraft {
                    kind: ArtifactKind::InputRequest,
                    producer: producer.clone(),
                    authority: self.authority,
                    provenance: Vec::new(),
                    depends_on: Vec::new(),
                    review: None,
                    host: self.host.clone(),
                },
                &bytes,
            )
            .await
            .map_err(|e| ArtifactSinkError::Write(e.to_string()))?;
        let id = artifact.id.clone();
        self.room
            .record_event(RoomEvent::ArtifactCreated { artifact })
            .await
            .map_err(|e| ArtifactSinkError::Write(e.to_string()))?;
        self.room
            .record_event(RoomEvent::TicketAssigned {
                node: node.clone(),
                artifact: id.clone(),
            })
            .await
            .map_err(|e| ArtifactSinkError::Write(e.to_string()))?;
        Ok(id)
    }
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
