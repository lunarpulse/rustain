//! Story 19.16g — the recipient learns a retract happened.
//!
//! A passive, pull-based reminder that the transparency log holds rows newer
//! than the operator's last **presented** log visit. Two effect-shell parts;
//! the TUI widgets and handlers stay free of I/O:
//!
//! - [`SeenStore`] — the local UI preference
//!   `{workspace}/.rustain/transparency-seen.json`: a schema version, the
//!   room identity, the durable `seen_seq`, and a local `reset_revision`.
//!   Read/merge/write is serialized across local clients by a dedicated,
//!   never-renamed, never-truncated lock file. It is a reminder boundary for
//!   this workspace's operator — ⛔ not a room event, not FR165
//!   acknowledgement, not a receipt, and not an integrity claim.
//! - [`LogAwarenessObserver`] — single-flight observation on a monotonic
//!   ≥ 1 s interval: a cheap journal head probe **with** the file stamp it
//!   read, a full fold only on a cold start or a changed head/stamp, and a
//!   preference re-read on every observation (another local client can
//!   advance `seen_seq` with no journal append).
//!
//! ⛔ Not a subscription: nothing here receives a daemon frame, emits an
//! `AppEvent`, or makes a network request. A completed local task is an
//! effect-shell result, never a remote notification.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use futures::FutureExt as _;

use crate::adapters::tui::state::{LogAwareness, LogAwarenessView};
use crate::domain::models::LogVisitCandidate;
use crate::infrastructure::subagent::node_journal::{JournalFileStamp, WorkspaceJournalReader};

/// Version of the seen-through preference record. An unknown version is not
/// trusted (conservatively counted from zero, shown as unavailable).
pub const SEEN_SCHEMA_VERSION: u32 = 1;

/// Minimum spacing between two scheduled observations. A client scheduling
/// bound, ⛔ not a propagation promise.
pub const OBSERVATION_INTERVAL: Duration = Duration::from_millis(1_000);

/// Largest preference file this reader accepts; the record is ~120 bytes.
const MAX_SEEN_RECORD_BYTES: u64 = 4 * 1024;

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SeenRecord {
    schema_version: u32,
    room: String,
    seen_seq: u64,
    reset_revision: u64,
}

/// The durable seen-through boundary and the reset revision it belongs to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SeenBoundary {
    pub seen_seq: u64,
    pub reset_revision: u64,
}

/// One read of the preference.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SeenLoad {
    /// No preference yet: count from zero — never "start caught up".
    Missing,
    Valid(SeenBoundary),
    /// Unreadable, malformed, unknown-version, wrong-room, symlinked or
    /// non-regular. Counted from zero and shown as unavailable until a
    /// successful read or visit write resolves it.
    Untrusted(String),
}

/// Outcome of merging presented visits into the shared preference.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VisitCommit {
    /// The stored boundary after the merge (unchanged when a newer visit by
    /// another client already covered these candidates).
    Committed(SeenBoundary),
    /// Every candidate was read against another reset revision.
    Stale,
    /// The durable head is below the stored boundary or a candidate: a
    /// suspected reset, never permission to merge a lower visit.
    ResetSuspected,
}

/// Outcome of a serialized reset confirmation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResetConfirmation {
    /// The durable head really is below the stored boundary: `seen_seq = 0`
    /// was persisted under a checked-incremented revision.
    Reset(SeenBoundary),
    /// The durable head is not below the stored boundary: the triggering
    /// observation was merely stale. Reload, never reset.
    StaleObservation,
}

/// The workspace-scoped seen-through preference (see the module doc).
#[derive(Clone, Debug)]
pub struct SeenStore {
    path: PathBuf,
    lock_path: PathBuf,
    room: String,
}

impl SeenStore {
    /// Address the workspace's preference. Performs no I/O.
    #[must_use]
    pub fn for_workspace(workspace: &Path) -> Self {
        Self {
            path: crate::infrastructure::paths::transparency_seen_path(workspace),
            lock_path: crate::infrastructure::paths::transparency_seen_lock_path(workspace),
            room: format!(
                "room-{}",
                crate::infrastructure::paths::workspace_hash(workspace)
            ),
        }
    }

    /// Read the preference without creating anything: a shared lock only when
    /// its sidecar already exists (writes are atomic replacements, so an
    /// unlocked read still sees a whole record).
    #[must_use]
    pub fn load(&self) -> SeenLoad {
        let _lock = match UiLock::shared_existing(&self.lock_path) {
            Ok(lock) => lock,
            Err(error) => return SeenLoad::Untrusted(format!("preference lock: {error}")),
        };
        self.read_record()
    }

    /// Merge presented visits into the shared boundary, serialized with every
    /// other local client. Within one reset revision the merge is `max`, so a
    /// stale client can never lower a newer visit; a candidate read against
    /// another revision is rejected. `durable_head` must read the journal's
    /// **actual** current head (it is called under the preference lock).
    ///
    /// Creates only the preference and its lock — never a journal.
    ///
    /// # Errors
    ///
    /// Lock/read/write failure or an unsupported platform lock; the caller
    /// keeps its previous committed boundary and retains the visits.
    pub fn commit_visits(
        &self,
        visits: &[LogVisitCandidate],
        durable_head: impl FnOnce() -> Result<u64, String>,
    ) -> Result<VisitCommit, String> {
        let _lock = self.lock_for_write()?;
        let stored = self.read_record();
        let head = durable_head()?;
        let (base, trusted) = match stored {
            SeenLoad::Valid(boundary) => (boundary, true),
            SeenLoad::Missing => (SeenBoundary::default(), true),
            SeenLoad::Untrusted(_) => (
                SeenBoundary {
                    seen_seq: 0,
                    reset_revision: visits
                        .iter()
                        .map(|visit| visit.reset_revision)
                        .max()
                        .unwrap_or(0),
                },
                false,
            ),
        };
        if head < base.seen_seq || visits.iter().any(|visit| visit.seen_through > head) {
            return Ok(VisitCommit::ResetSuspected);
        }
        let mut next = base;
        let mut applied = false;
        for visit in visits {
            if visit.reset_revision != base.reset_revision {
                continue;
            }
            next.seen_seq = next.seen_seq.max(visit.seen_through);
            applied = true;
        }
        if !applied {
            return Ok(VisitCommit::Stale);
        }
        if trusted && next == base {
            return Ok(VisitCommit::Committed(base));
        }
        self.write_record(next)?;
        Ok(VisitCommit::Committed(next))
    }

    /// Serialized reset transaction: re-read the stored boundary and the
    /// **actual** durable head under the preference lock. Only a head still
    /// below the stored boundary persists `seen_seq = 0` with a checked
    /// increment of `reset_revision`; otherwise the observation was stale.
    ///
    /// This is conservative reset recovery, ⛔ not authenticated rollback
    /// detection (`DF-18-2-AUTHENTICATED-JOURNAL`).
    ///
    /// # Errors
    ///
    /// Lock/read/write failure or revision exhaustion; nothing is committed.
    pub fn confirm_reset(
        &self,
        durable_head: impl FnOnce() -> Result<u64, String>,
    ) -> Result<ResetConfirmation, String> {
        let _lock = self.lock_for_write()?;
        let SeenLoad::Valid(stored) = self.read_record() else {
            return Ok(ResetConfirmation::StaleObservation);
        };
        if durable_head()? >= stored.seen_seq {
            return Ok(ResetConfirmation::StaleObservation);
        }
        let next = SeenBoundary {
            seen_seq: 0,
            reset_revision: stored
                .reset_revision
                .checked_add(1)
                .ok_or_else(|| "the local reset revision is exhausted".to_owned())?,
        };
        self.write_record(next)?;
        Ok(ResetConfirmation::Reset(next))
    }

    fn lock_for_write(&self) -> Result<UiLock, String> {
        let parent = self
            .lock_path
            .parent()
            .ok_or_else(|| "preference lock path has no parent".to_owned())?;
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        UiLock::exclusive(&self.lock_path).map_err(|error| format!("preference lock: {error}"))
    }

    fn read_record(&self) -> SeenLoad {
        use std::io::Read as _;

        let file = match open_no_follow(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return SeenLoad::Missing,
            Err(error) => return SeenLoad::Untrusted(error.to_string()),
        };
        match file.metadata() {
            Ok(metadata) if metadata.is_file() => {}
            Ok(_) => return SeenLoad::Untrusted("not a regular file".to_owned()),
            Err(error) => return SeenLoad::Untrusted(error.to_string()),
        }
        let mut bytes = Vec::new();
        if let Err(error) = file.take(MAX_SEEN_RECORD_BYTES + 1).read_to_end(&mut bytes) {
            return SeenLoad::Untrusted(error.to_string());
        }
        if bytes.len() as u64 > MAX_SEEN_RECORD_BYTES {
            return SeenLoad::Untrusted("oversized record".to_owned());
        }
        let record: SeenRecord = match serde_json::from_slice(&bytes) {
            Ok(record) => record,
            Err(error) => return SeenLoad::Untrusted(error.to_string()),
        };
        if record.schema_version != SEEN_SCHEMA_VERSION {
            return SeenLoad::Untrusted(format!("unknown version {}", record.schema_version));
        }
        if record.room != self.room {
            return SeenLoad::Untrusted("recorded for another room".to_owned());
        }
        SeenLoad::Valid(SeenBoundary {
            seen_seq: record.seen_seq,
            reset_revision: record.reset_revision,
        })
    }

    fn write_record(&self, boundary: SeenBoundary) -> Result<(), String> {
        // Reject, never follow or replace, a symlinked or non-regular path.
        match std::fs::symlink_metadata(&self.path) {
            Ok(metadata) if !metadata.file_type().is_file() => {
                return Err("the seen preference path is not a regular file".to_owned());
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
        let body = serde_json::to_vec(&SeenRecord {
            schema_version: SEEN_SCHEMA_VERSION,
            room: self.room.clone(),
            seen_seq: boundary.seen_seq,
            reset_revision: boundary.reset_revision,
        })
        .map_err(|error| error.to_string())?;
        crate::infrastructure::transparency::write_private_atomic(
            &self.path,
            &body,
            ".transparency-seen-",
        )
        .map_err(|error| error.to_string())
    }
}

/// Open a path for reading, refusing to follow a final-component symlink.
fn open_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
    }
    #[cfg(not(unix))]
    {
        if std::fs::symlink_metadata(path)?.file_type().is_symlink() {
            return Err(std::io::Error::other("refusing to follow a symlink"));
        }
        std::fs::File::open(path)
    }
}

/// The preference's own advisory lock. Private (`0600`), created without
/// truncation, never renamed, symlink/non-regular paths rejected. Released
/// when the descriptor closes.
struct UiLock {
    _file: std::fs::File,
}

impl UiLock {
    fn exclusive(path: &Path) -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(path)?;
            Self::lock(file, libc::LOCK_EX)
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "advisory preference locking is unsupported on this platform",
            ))
        }
    }

    fn shared_existing(path: &Path) -> std::io::Result<Option<Self>> {
        let file = match open_no_follow(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        #[cfg(unix)]
        {
            Self::lock(file, libc::LOCK_SH).map(Some)
        }
        #[cfg(not(unix))]
        {
            let _ = file;
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "advisory preference locking is unsupported on this platform",
            ))
        }
    }

    #[cfg(unix)]
    fn lock(file: std::fs::File, operation: libc::c_int) -> std::io::Result<Self> {
        use std::os::unix::io::AsRawFd as _;
        if !file.metadata()?.is_file() {
            return Err(std::io::Error::other("the lock path is not a regular file"));
        }
        // SAFETY: the descriptor is valid and owned by `file` for the call.
        if unsafe { libc::flock(file.as_raw_fd(), operation) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self { _file: file })
    }
}

/// The last completed, successful fold: row sequences only (never the rows),
/// with the head and stamp of the read that produced them.
#[derive(Clone, Debug)]
struct ObservedRows {
    head: u64,
    stamp: Option<JournalFileStamp>,
    /// Ascending; equal values for rows of one atomic batch.
    row_seqs: Vec<u64>,
}

struct JobInput {
    observe: bool,
    known: Option<(u64, Option<JournalFileStamp>)>,
    visits: Vec<LogVisitCandidate>,
    confirm_reset: bool,
}

struct JobOutput {
    commit: Option<Result<VisitCommit, String>>,
    reset: Option<Result<ResetConfirmation, String>>,
    observation: Option<Observation>,
}

struct Observation {
    preference: SeenLoad,
    /// `Ok(None)`: head and stamp unchanged, no fold performed.
    rows: Result<Option<ObservedRows>, String>,
}

fn run_job(journal: &WorkspaceJournalReader, store: &SeenStore, input: JobInput) -> JobOutput {
    let durable_head = || {
        journal
            .probe_blocking()
            .map(|probe| probe.head)
            .map_err(|error| error.to_string())
    };
    let commit =
        (!input.visits.is_empty()).then(|| store.commit_visits(&input.visits, durable_head));
    let reset = input
        .confirm_reset
        .then(|| store.confirm_reset(durable_head));
    let observation = input.observe.then(|| Observation {
        preference: store.load(),
        rows: observe_rows(journal, input.known),
    });
    JobOutput {
        commit,
        reset,
        observation,
    }
}

fn observe_rows(
    journal: &WorkspaceJournalReader,
    known: Option<(u64, Option<JournalFileStamp>)>,
) -> Result<Option<ObservedRows>, String> {
    let probe = journal
        .probe_blocking()
        .map_err(|error| error.to_string())?;
    if known == Some((probe.head, probe.stamp)) {
        return Ok(None);
    }
    let snapshot = journal
        .snapshot_blocking()
        .map_err(|error| error.to_string())?;
    let mut row_seqs: Vec<u64> =
        crate::domain::services::transparency::fold_transparency(&snapshot.entries)
            .iter()
            .map(|row| row.seq)
            .collect();
    row_seqs.sort_unstable();
    Ok(Some(ObservedRows {
        head: snapshot.head,
        stamp: snapshot.stamp,
        row_seqs,
    }))
}

struct Active {
    journal: WorkspaceJournalReader,
    store: SeenStore,
    job: Option<tokio::task::JoinHandle<JobOutput>>,
    /// Visits sent with the in-flight job (a prefix of `pending`).
    sent: usize,
    last_observed: Option<Instant>,
    rows: Option<ObservedRows>,
    refold: bool,
    journal_failed: bool,
    boundary: SeenBoundary,
    preference_trusted: bool,
    pending: Vec<LogVisitCandidate>,
    save_failed: bool,
    reset_suspected: bool,
    jobs_started: u64,
    full_reads: u64,
}

/// Single-flight journal/preference observer for one TUI client. Owned by the
/// client's loop; dropping it aborts any outstanding work.
pub struct LogAwarenessObserver {
    active: Option<Active>,
}

impl LogAwarenessObserver {
    /// Observe `workspace`'s room journal. Performs no I/O until the first
    /// tick, and no observation ever creates a file.
    #[must_use]
    pub fn for_workspace(workspace: &Path) -> Self {
        Self {
            active: Some(Active {
                journal: WorkspaceJournalReader::open_workspace(workspace),
                store: SeenStore::for_workspace(workspace),
                job: None,
                sent: 0,
                last_observed: None,
                rows: None,
                refold: false,
                journal_failed: false,
                boundary: SeenBoundary::default(),
                preference_trusted: true,
                pending: Vec::new(),
                save_failed: false,
                reset_suspected: false,
                jobs_started: 0,
                full_reads: 0,
            }),
        }
    }

    /// No journal is composed for this session: the reminder stays hidden.
    #[must_use]
    pub fn disabled() -> Self {
        Self { active: None }
    }

    /// The production tick. Returns whether the reminder changed.
    pub fn tick(&mut self, view: &mut LogAwarenessView) -> bool {
        self.tick_at(Instant::now(), view)
    }

    /// [`Self::tick`] at an explicit monotonic instant: adopt presented
    /// visits, apply a completed job, start at most one new job, and project
    /// the cached count. Never blocks on I/O.
    pub fn tick_at(&mut self, now: Instant, view: &mut LogAwarenessView) -> bool {
        let Some(active) = self.active.as_mut() else {
            return false;
        };
        let before = (view.display, view.reset_revision);
        active.adopt_presented(view);
        if active
            .job
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished)
            && let Some(job) = active.job.take()
        {
            let output = job.now_or_never().expect("a finished task is ready");
            active.apply(output);
        }
        active.maybe_start(now);
        active.project(view);
        before != (view.display, view.reset_revision)
    }

    /// Wait for the in-flight job, if any, and apply it exactly as a tick
    /// would. The deterministic completion seam for keystones.
    pub async fn settle(&mut self, view: &mut LogAwarenessView) -> bool {
        let Some(active) = self.active.as_mut() else {
            return false;
        };
        let before = (view.display, view.reset_revision);
        active.adopt_presented(view);
        if let Some(job) = active.job.take() {
            let output = job.await;
            active.apply(output);
        }
        active.project(view);
        before != (view.display, view.reset_revision)
    }

    /// Structural ratchet: jobs started (at most one in flight at a time).
    #[must_use]
    pub fn jobs_started(&self) -> u64 {
        self.active.as_ref().map_or(0, |active| active.jobs_started)
    }

    /// Structural ratchet: completed full journal folds.
    #[must_use]
    pub fn full_reads(&self) -> u64 {
        self.active.as_ref().map_or(0, |active| active.full_reads)
    }

    /// Whether a job is outstanding.
    #[must_use]
    pub fn in_flight(&self) -> bool {
        self.active
            .as_ref()
            .is_some_and(|active| active.job.is_some())
    }
}

impl Drop for LogAwarenessObserver {
    fn drop(&mut self) {
        if let Some(job) = self.active.as_mut().and_then(|active| active.job.take()) {
            job.abort();
        }
    }
}

impl Active {
    fn adopt_presented(&mut self, view: &mut LogAwarenessView) {
        for visit in view.presented.drain(..) {
            // An empty visit needs no persistence; a visit during a suspected
            // reset is invalidated with every other pending save.
            if visit.seen_through > 0 && !self.reset_suspected {
                self.pending.push(visit);
            }
        }
    }

    fn maybe_start(&mut self, now: Instant) {
        if self.job.is_some() {
            return;
        }
        let observe = self
            .last_observed
            .is_none_or(|last| now.duration_since(last) >= OBSERVATION_INTERVAL);
        if !observe && self.pending.is_empty() {
            return;
        }
        if observe {
            self.last_observed = Some(now);
        }
        let input = JobInput {
            observe,
            known: if self.refold {
                None
            } else {
                self.rows.as_ref().map(|rows| (rows.head, rows.stamp))
            },
            visits: self.pending.clone(),
            confirm_reset: observe && self.reset_suspected,
        };
        self.sent = input.visits.len();
        let journal = self.journal.clone();
        let store = self.store.clone();
        self.job = Some(tokio::task::spawn_blocking(move || {
            run_job(&journal, &store, input)
        }));
        self.jobs_started += 1;
    }

    fn apply(&mut self, output: Result<JobOutput, tokio::task::JoinError>) {
        let sent = std::mem::take(&mut self.sent).min(self.pending.len());
        let Ok(output) = output else {
            // A panicked or aborted job proves nothing: stay unavailable,
            // keep every pending visit, retry on the next due observation.
            self.journal_failed = true;
            self.save_failed = self.save_failed || sent > 0;
            return;
        };
        match output.commit {
            Some(Ok(VisitCommit::Committed(boundary))) => {
                self.pending.drain(..sent);
                self.save_failed = false;
                self.preference_trusted = true;
                self.adopt_boundary(boundary);
            }
            Some(Ok(VisitCommit::Stale)) => {
                self.pending.drain(..sent);
                self.save_failed = false;
            }
            Some(Ok(VisitCommit::ResetSuspected)) => {
                self.pending.clear();
                self.save_failed = false;
                self.reset_suspected = true;
            }
            // The prior committed boundary stays; the visit waits for a
            // later attempt and the reminder reads `log: ?` meanwhile.
            Some(Err(_)) => self.save_failed = true,
            None => {}
        }
        match output.reset {
            Some(Ok(ResetConfirmation::Reset(boundary))) => {
                self.reset_suspected = false;
                self.preference_trusted = true;
                self.adopt_boundary(boundary);
            }
            Some(Ok(ResetConfirmation::StaleObservation)) => {
                self.reset_suspected = false;
                self.refold = true;
            }
            Some(Err(_)) | None => {}
        }
        if let Some(observation) = output.observation {
            match observation.preference {
                SeenLoad::Valid(boundary) => {
                    self.preference_trusted = true;
                    self.adopt_boundary(boundary);
                }
                SeenLoad::Missing => {
                    self.preference_trusted = true;
                    self.adopt_boundary(SeenBoundary::default());
                }
                SeenLoad::Untrusted(_) => {
                    self.preference_trusted = false;
                    self.boundary.seen_seq = 0;
                }
            }
            match observation.rows {
                Ok(Some(rows)) => {
                    self.rows = Some(rows);
                    self.refold = false;
                    self.journal_failed = false;
                    self.full_reads += 1;
                }
                Ok(None) => self.journal_failed = false,
                // The last good rows stay available; the reminder says `?`.
                Err(_) => self.journal_failed = true,
            }
        }
        if self.preference_trusted
            && self
                .rows
                .as_ref()
                .is_some_and(|rows| rows.head < self.boundary.seen_seq)
        {
            // The shipped journal is append-only: a head below the stored
            // boundary is a suspected reset. Invalidate pending saves and
            // confirm under the preference lock on the next observation.
            self.reset_suspected = true;
            self.pending.clear();
        }
    }

    fn adopt_boundary(&mut self, boundary: SeenBoundary) {
        if boundary.reset_revision != self.boundary.reset_revision {
            // A new reset revision invalidates candidates read against the
            // old one and requires a fresh report.
            self.pending
                .retain(|visit| visit.reset_revision == boundary.reset_revision);
            self.refold = true;
        }
        self.boundary = boundary;
    }

    fn project(&self, view: &mut LogAwarenessView) {
        view.reset_revision = self.boundary.reset_revision;
        view.display = if self.journal_failed
            || !self.preference_trusted
            || self.save_failed
            || self.reset_suspected
        {
            LogAwareness::Unavailable
        } else {
            match &self.rows {
                None => LogAwareness::Hidden,
                Some(rows) => {
                    let seen = rows
                        .row_seqs
                        .partition_point(|seq| *seq <= self.boundary.seen_seq);
                    match rows.row_seqs.len() - seen {
                        0 => LogAwareness::Hidden,
                        unseen => LogAwareness::Unseen(unseen),
                    }
                }
            }
        };
    }
}

/// Real-journal fixtures shared by this story's keystones in other modules.
#[cfg(test)]
pub(crate) mod test_support {
    use std::path::Path;

    use crate::domain::models::{Direction, JournalRecord, PeerId, RejectReason, RoomEvent};
    use crate::infrastructure::subagent::NodeJournal;

    /// One transparency-producing room event (a refused inbound envelope).
    pub(crate) fn rejection(tag: u8) -> RoomEvent {
        RoomEvent::RemoteEnvelopeRejected {
            peer: PeerId::from_public_key(&[tag.max(1); 32]).unwrap(),
            reason: RejectReason::Policy {
                detail: "policy".to_owned(),
            },
            direction: Direction::Inbound,
            task: None,
        }
    }

    /// Append `count` row-producing records through the production writer,
    /// one durable line (and `seq`) each.
    pub(crate) async fn append_rows(workspace: &Path, count: usize) {
        if count == 0 {
            return;
        }
        let journal = NodeJournal::open_workspace(workspace).await.unwrap();
        journal
            .append_batch(
                (0..count)
                    .map(|index| JournalRecord::Room(rejection(index as u8)))
                    .collect(),
            )
            .await
            .unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{append_rows, rejection};
    use super::*;
    use crate::domain::models::{AgentId, ItemAddress, ItemId, JournalRecord, PeerId, RoomEvent};
    use crate::infrastructure::subagent::NodeJournal;

    /// Drive one scheduled observation to completion at `now`.
    async fn observe(
        observer: &mut LogAwarenessObserver,
        view: &mut LogAwarenessView,
        now: Instant,
    ) -> LogAwareness {
        observer.tick_at(now, view);
        observer.settle(view).await;
        view.display
    }

    /// Present one visit the way a completed draw does, and persist it.
    async fn present(
        observer: &mut LogAwarenessObserver,
        view: &mut LogAwarenessView,
        now: Instant,
        seen_through: u64,
    ) {
        view.presented.push(LogVisitCandidate {
            seen_through,
            reset_revision: view.reset_revision,
        });
        observer.tick_at(now, view);
        observer.settle(view).await;
    }

    fn at(start: Instant, seconds: u64) -> Instant {
        start + Duration::from_secs(seconds)
    }

    fn stored(workspace: &Path) -> SeenLoad {
        SeenStore::for_workspace(workspace).load()
    }

    #[tokio::test]
    async fn seen_boundary_survives_fresh_client_restart() {
        // K03 / M03.
        let workspace = tempfile::tempdir().unwrap();
        append_rows(workspace.path(), 3).await;
        let t0 = Instant::now();

        let mut view = LogAwarenessView::default();
        let mut first = LogAwarenessObserver::for_workspace(workspace.path());
        assert_eq!(
            observe(&mut first, &mut view, t0).await,
            LogAwareness::Unseen(3)
        );
        drop(first);
        // A restart before any visit must not suppress existing rows.
        let mut view = LogAwarenessView::default();
        let mut restarted = LogAwarenessObserver::for_workspace(workspace.path());
        assert_eq!(
            observe(&mut restarted, &mut view, t0).await,
            LogAwareness::Unseen(3),
            "a missing preference is seen_seq = 0, never 'start caught up'"
        );

        present(&mut restarted, &mut view, at(t0, 0), 3).await;
        assert_eq!(view.display, LogAwareness::Hidden);
        assert_eq!(
            stored(workspace.path()),
            SeenLoad::Valid(SeenBoundary {
                seen_seq: 3,
                reset_revision: 0
            }),
            "the real persisted seq"
        );
        drop(restarted);

        append_rows(workspace.path(), 1).await;
        let mut view = LogAwarenessView::default();
        let mut fresh = LogAwarenessObserver::for_workspace(workspace.path());
        assert_eq!(
            observe(&mut fresh, &mut view, t0).await,
            LogAwareness::Unseen(1),
            "old rows stay cleared across a fresh client; one later row counts one"
        );
    }

    #[tokio::test]
    async fn folded_row_counts_render_bounded_and_hidden_at_zero() {
        // K01 (fold half): real journal rows through the observer into the
        // real status bar; the widget half lives beside `status_bar::render`.
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let t0 = Instant::now();
        for (rows, expected) in [
            (0usize, None),
            (1, Some("log: 1")),
            (99, Some("log: 99")),
            (100, Some("log: 99+")),
            (150, Some("log: 99+")),
        ] {
            let workspace = tempfile::tempdir().unwrap();
            append_rows(workspace.path(), rows).await;
            let mut view = LogAwarenessView::default();
            let mut observer = LogAwarenessObserver::for_workspace(workspace.path());
            observe(&mut observer, &mut view, t0).await;

            let mut terminal = Terminal::new(TestBackend::new(80, 1)).unwrap();
            let theme = crate::adapters::tui::theme::Theme::dark();
            terminal
                .draw(|frame| {
                    crate::adapters::tui::widgets::status_bar::render(
                        frame,
                        frame.area(),
                        "(daemon)",
                        None,
                        &crate::domain::models::StatusState::Idle,
                        &theme,
                        0,
                        &[],
                        0,
                        20,
                        crate::domain::models::PermissionMode::Normal,
                        None,
                        0,
                        false,
                        None,
                        false,
                        true,
                        0,
                        None,
                        0,
                        None,
                        None,
                        None,
                        false,
                        None,
                        None,
                        crate::domain::models::visual::DensityMode::Focus,
                        false,
                        None,
                        view.display,
                    );
                })
                .unwrap();
            let row: String = (0..80)
                .map(|x| terminal.backend().buffer().cell((x, 0)).unwrap().symbol())
                .collect();
            match expected {
                Some(segment) => assert!(row.trim_end().ends_with(segment), "{rows}: {row}"),
                None => assert!(!row.contains("log:"), "{rows}: {row}"),
            }
        }
    }

    #[tokio::test]
    async fn closed_log_observes_a_later_durable_retract() {
        // K04 (observer half): started before the append, never opened, no
        // input and no daemon event — only the scheduled observation.
        let workspace = tempfile::tempdir().unwrap();
        let t0 = Instant::now();
        let mut view = LogAwarenessView::default();
        let mut observer = LogAwarenessObserver::for_workspace(workspace.path());
        assert_eq!(
            observe(&mut observer, &mut view, t0).await,
            LogAwareness::Hidden
        );

        let peer = PeerId::from_public_key(&[7; 32]).unwrap();
        NodeJournal::open_workspace(workspace.path())
            .await
            .unwrap()
            .append_room(RoomEvent::RecipientItemRetracted {
                address: ItemAddress::from_a2a_ingress(peer, ItemId::from_replay("ri_1")),
                retracted_at_ms: 1_700_000_000_000,
                principal_collapsed: false,
            })
            .await
            .unwrap();

        // Before the interval elapses nothing is observed…
        observer.tick_at(at(t0, 0), &mut view);
        assert!(!observer.in_flight());
        assert_eq!(view.display, LogAwareness::Hidden);
        // …and the next scheduled observation counts the retract row.
        assert_eq!(
            observe(&mut observer, &mut view, at(t0, 1)).await,
            LogAwareness::Unseen(1)
        );
    }

    #[tokio::test]
    async fn rows_not_sequence_distance_and_no_idle_refold() {
        // K05 / M05.
        let workspace = tempfile::tempdir().unwrap();
        let journal = NodeJournal::open_workspace(workspace.path()).await.unwrap();
        for alias in ["one", "two"] {
            journal
                .append(JournalRecord::AliasBound {
                    node: AgentId::new(),
                    alias: alias.to_owned(),
                })
                .await
                .unwrap();
        }
        journal
            .append_atomic_batch(vec![
                JournalRecord::Room(rejection(1)),
                JournalRecord::Room(rejection(2)),
            ])
            .await
            .unwrap();
        journal.append_room(rejection(3)).await.unwrap();
        // head 4, two invisible records, one batch of two rows sharing seq 3.

        let t0 = Instant::now();
        let mut view = LogAwarenessView::default();
        let mut observer = LogAwarenessObserver::for_workspace(workspace.path());
        assert_eq!(
            observe(&mut observer, &mut view, t0).await,
            LogAwareness::Unseen(3),
            "rows, not head distance (4) nor distinct seqs (2)"
        );
        assert_eq!(observer.full_reads(), 1);

        // Unchanged head AND stamp: polls read the preference, never refold.
        for second in 1..=3 {
            observe(&mut observer, &mut view, at(t0, second)).await;
        }
        assert_eq!(observer.jobs_started(), 4);
        assert_eq!(
            observer.full_reads(),
            1,
            "an idle journal is never refolded"
        );

        // A visit through the batch's seq acknowledges the batch together.
        present(&mut observer, &mut view, at(t0, 3), 3).await;
        assert_eq!(view.display, LogAwareness::Unseen(1));

        journal.append_room(rejection(4)).await.unwrap();
        assert_eq!(
            observe(&mut observer, &mut view, at(t0, 5)).await,
            LogAwareness::Unseen(2)
        );
        assert_eq!(
            observer.full_reads(),
            2,
            "a changed head performs a full read"
        );

        // Same last valid seq, one valid row edited in place: the stamp
        // (here its length) changes, so the journal is refolded.
        let path = workspace.path().join(".rustain/rooms").join(format!(
            "room-{}.jsonl",
            crate::infrastructure::paths::workspace_hash(workspace.path())
        ));
        let original = std::fs::read_to_string(&path).unwrap();
        std::fs::write(
            &path,
            original.replacen("\"detail\":\"policy\"", "\"detail\":\"policy, edited\"", 1),
        )
        .unwrap();
        observe(&mut observer, &mut view, at(t0, 7)).await;
        assert_eq!(observer.full_reads(), 3, "a same-head rewrite is refolded");
        assert_eq!(view.display, LogAwareness::Unseen(2));

        // An interior corruption that keeps the last valid seq: never a
        // current-looking count, the last good rows retained.
        let mut lines: Vec<String> = original.lines().map(str::to_owned).collect();
        lines[1] = "#".to_owned();
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        assert_eq!(
            observe(&mut observer, &mut view, at(t0, 9)).await,
            LogAwareness::Unavailable
        );
        std::fs::write(&path, original).unwrap();
        assert_eq!(
            observe(&mut observer, &mut view, at(t0, 11)).await,
            LogAwareness::Unseen(2),
            "recovery returns the count"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn one_observation_in_flight_and_a_held_read_never_blocks_the_tick() {
        // K05 single-flight half: a real writer lock holds the probe.
        use std::os::unix::io::AsRawFd as _;

        let workspace = tempfile::tempdir().unwrap();
        append_rows(workspace.path(), 2).await;
        let lock_path = workspace.path().join(".rustain/rooms").join(format!(
            "room-{}.lock",
            crate::infrastructure::paths::workspace_hash(workspace.path())
        ));
        let holder = std::fs::File::open(&lock_path).unwrap();
        // SAFETY: valid descriptor owned by `holder`.
        assert_eq!(unsafe { libc::flock(holder.as_raw_fd(), libc::LOCK_EX) }, 0);

        let t0 = Instant::now();
        let mut view = LogAwarenessView::default();
        let mut observer = LogAwarenessObserver::for_workspace(workspace.path());
        observer.tick_at(t0, &mut view);
        assert!(observer.in_flight());
        // Later ticks return immediately and start nothing new.
        for second in 1..=5 {
            observer.tick_at(at(t0, second), &mut view);
        }
        assert_eq!(observer.jobs_started(), 1, "no overlapping reads");
        assert_eq!(view.display, LogAwareness::Hidden);

        drop(holder);
        observer.settle(&mut view).await;
        assert_eq!(view.display, LogAwareness::Unseen(2));
        // No accumulated catch-up work: one more observation, not five.
        observe(&mut observer, &mut view, at(t0, 6)).await;
        assert_eq!(observer.jobs_started(), 2);
    }

    #[tokio::test]
    async fn unavailable_observation_never_means_caught_up() {
        // K09 / M09.
        let workspace = tempfile::tempdir().unwrap();
        let t0 = Instant::now();
        let mut view = LogAwarenessView::default();
        let mut observer = LogAwarenessObserver::for_workspace(workspace.path());
        for second in 0..3 {
            observe(&mut observer, &mut view, at(t0, second)).await;
        }
        assert!(
            !workspace.path().join(".rustain").exists(),
            "polling an unused workspace creates nothing"
        );

        append_rows(workspace.path(), 2).await;
        assert_eq!(
            observe(&mut observer, &mut view, at(t0, 3)).await,
            LogAwareness::Unseen(2)
        );

        // A journal read failure is not an empty successful report.
        let path = workspace.path().join(".rustain/rooms").join(format!(
            "room-{}.jsonl",
            crate::infrastructure::paths::workspace_hash(workspace.path())
        ));
        let moved = path.with_extension("moved");
        std::fs::rename(&path, &moved).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert_eq!(
            observe(&mut observer, &mut view, at(t0, 4)).await,
            LogAwareness::Unavailable
        );
        std::fs::remove_dir(&path).unwrap();
        std::fs::rename(&moved, &path).unwrap();
        assert_eq!(
            observe(&mut observer, &mut view, at(t0, 5)).await,
            LogAwareness::Unseen(2)
        );

        // A failed save keeps the prior committed boundary and shows `?`.
        let seen = crate::infrastructure::paths::transparency_seen_path(workspace.path());
        std::fs::create_dir(&seen).unwrap();
        present(&mut observer, &mut view, at(t0, 5), 2).await;
        assert_eq!(view.display, LogAwareness::Unavailable);
        assert!(matches!(stored(workspace.path()), SeenLoad::Untrusted(_)));
        std::fs::remove_dir(&seen).unwrap();
        // The retained visit is persisted by a later attempt.
        observe(&mut observer, &mut view, at(t0, 6)).await;
        observe(&mut observer, &mut view, at(t0, 7)).await;
        assert_eq!(view.display, LogAwareness::Hidden);
        assert_eq!(
            stored(workspace.path()),
            SeenLoad::Valid(SeenBoundary {
                seen_seq: 2,
                reset_revision: 0
            })
        );

        // A malformed preference counts from zero, never from the head.
        std::fs::write(&seen, b"{not json").unwrap();
        assert_eq!(
            observe(&mut observer, &mut view, at(t0, 8)).await,
            LogAwareness::Unavailable
        );
        let mut restarted_view = LogAwarenessView::default();
        let mut restarted = LogAwarenessObserver::for_workspace(workspace.path());
        observe(&mut restarted, &mut restarted_view, at(t0, 8)).await;
        assert_eq!(restarted_view.display, LogAwareness::Unavailable);
        // A successful visit write resolves it.
        present(&mut restarted, &mut restarted_view, at(t0, 8), 2).await;
        observe(&mut restarted, &mut restarted_view, at(t0, 9)).await;
        assert_eq!(restarted_view.display, LogAwareness::Hidden);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlinked_preference_is_neither_trusted_nor_followed() {
        // K09 / K11 symlink probes.
        let workspace = tempfile::tempdir().unwrap();
        append_rows(workspace.path(), 2).await;
        let target = workspace.path().join("outside.json");
        let room = format!(
            "room-{}",
            crate::infrastructure::paths::workspace_hash(workspace.path())
        );
        let forged = format!(
            "{{\"schema_version\":1,\"room\":\"{room}\",\"seen_seq\":2,\"reset_revision\":0}}"
        );
        std::fs::write(&target, &forged).unwrap();
        let seen = crate::infrastructure::paths::transparency_seen_path(workspace.path());
        std::os::unix::fs::symlink(&target, &seen).unwrap();

        let t0 = Instant::now();
        let mut view = LogAwarenessView::default();
        let mut observer = LogAwarenessObserver::for_workspace(workspace.path());
        assert_eq!(
            observe(&mut observer, &mut view, t0).await,
            LogAwareness::Unavailable,
            "a symlinked record is never trusted as 'caught up'"
        );
        present(&mut observer, &mut view, t0, 2).await;
        assert_eq!(view.display, LogAwareness::Unavailable);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), forged);
        assert!(
            std::fs::symlink_metadata(&seen)
                .unwrap()
                .file_type()
                .is_symlink()
        );

        // The lock path is refused too, and its target never truncated.
        std::fs::remove_file(&seen).unwrap();
        let lock_target = workspace.path().join("lock-target");
        std::fs::write(&lock_target, b"keep").unwrap();
        let lock = crate::infrastructure::paths::transparency_seen_lock_path(workspace.path());
        std::fs::remove_file(&lock).unwrap();
        std::os::unix::fs::symlink(&lock_target, &lock).unwrap();
        let store = SeenStore::for_workspace(workspace.path());
        assert!(
            store
                .commit_visits(
                    &[LogVisitCandidate {
                        seen_through: 1,
                        reset_revision: 0
                    }],
                    || Ok(2)
                )
                .is_err()
        );
        assert!(matches!(store.load(), SeenLoad::Untrusted(_)));
        assert_eq!(std::fs::read(&lock_target).unwrap(), b"keep");
        assert!(!seen.exists());
    }

    #[tokio::test]
    async fn workspace_and_concurrent_client_boundaries_are_safe() {
        // K11 / M11.
        let workspace = tempfile::tempdir().unwrap();
        append_rows(workspace.path(), 12).await;
        let head = || Ok(12);
        let newer = SeenStore::for_workspace(workspace.path());
        let stale = SeenStore::for_workspace(workspace.path());
        let visit = |seen_through| LogVisitCandidate {
            seen_through,
            reset_revision: 0,
        };
        assert_eq!(
            newer.commit_visits(&[visit(10)], head).unwrap(),
            VisitCommit::Committed(SeenBoundary {
                seen_seq: 10,
                reset_revision: 0
            })
        );
        assert_eq!(
            stale.commit_visits(&[visit(5)], head).unwrap(),
            VisitCommit::Committed(SeenBoundary {
                seen_seq: 10,
                reset_revision: 0
            }),
            "max-merge: an older visit never lowers a newer one"
        );
        let lock = crate::infrastructure::paths::transparency_seen_lock_path(workspace.path());
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
            let metadata = std::fs::metadata(&lock).unwrap();
            assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
            let inode = metadata.ino();
            newer.commit_visits(&[visit(11)], head).unwrap();
            assert_eq!(
                std::fs::metadata(&lock).unwrap().ino(),
                inode,
                "the lock inode is stable across preference replacements"
            );
        }

        // One client's visit reaches another client with no journal append.
        let t0 = Instant::now();
        let (mut view_a, mut view_b) = (LogAwarenessView::default(), LogAwarenessView::default());
        let mut a = LogAwarenessObserver::for_workspace(workspace.path());
        let mut b = LogAwarenessObserver::for_workspace(workspace.path());
        observe(&mut a, &mut view_a, t0).await;
        observe(&mut b, &mut view_b, t0).await;
        assert_eq!(view_b.display, LogAwareness::Unseen(1));
        present(&mut a, &mut view_a, t0, 12).await;
        assert_eq!(view_a.display, LogAwareness::Hidden);
        assert_eq!(
            observe(&mut b, &mut view_b, at(t0, 1)).await,
            LogAwareness::Hidden,
            "an unchanged head still re-reads the shared preference"
        );
        assert_eq!(b.full_reads(), 1);

        // Distinct workspaces do not share a boundary.
        let other = tempfile::tempdir().unwrap();
        append_rows(other.path(), 2).await;
        let mut view_c = LogAwarenessView::default();
        let mut c = LogAwarenessObserver::for_workspace(other.path());
        assert_eq!(
            observe(&mut c, &mut view_c, t0).await,
            LogAwareness::Unseen(2)
        );
        assert_eq!(stored(other.path()), SeenLoad::Missing);

        // A merely stale observation never resets a newer shared visit.
        assert_eq!(
            newer.confirm_reset(|| Ok(12)).unwrap(),
            ResetConfirmation::StaleObservation
        );
        assert_eq!(
            stored(workspace.path()),
            SeenLoad::Valid(SeenBoundary {
                seen_seq: 12,
                reset_revision: 0
            })
        );
    }

    #[tokio::test]
    async fn a_confirmed_lower_head_resets_and_rejects_pre_reset_candidates() {
        // K11 reset half / M11 (trust above head, omit reset/revision,
        // reset from an old cached head).
        let workspace = tempfile::tempdir().unwrap();
        append_rows(workspace.path(), 5).await;
        let t0 = Instant::now();
        let mut view = LogAwarenessView::default();
        let mut observer = LogAwarenessObserver::for_workspace(workspace.path());
        observe(&mut observer, &mut view, t0).await;
        present(&mut observer, &mut view, t0, 5).await;
        assert_eq!(view.display, LogAwareness::Hidden);

        // Replace the journal with a shorter history.
        let rooms = workspace.path().join(".rustain/rooms");
        for entry in std::fs::read_dir(&rooms).unwrap() {
            std::fs::remove_file(entry.unwrap().path()).unwrap();
        }
        append_rows(workspace.path(), 2).await;

        assert_eq!(
            observe(&mut observer, &mut view, at(t0, 1)).await,
            LogAwareness::Unavailable,
            "a head below the stored boundary is a suspected reset, not zero"
        );
        // A lower visit is not max-merged into the old boundary.
        let store = SeenStore::for_workspace(workspace.path());
        let durable = || Ok(2);
        assert_eq!(
            store
                .commit_visits(
                    &[LogVisitCandidate {
                        seen_through: 2,
                        reset_revision: 0
                    }],
                    durable
                )
                .unwrap(),
            VisitCommit::ResetSuspected
        );
        // The next due observation confirms under the lock and resets.
        observe(&mut observer, &mut view, at(t0, 2)).await;
        assert_eq!(
            stored(workspace.path()),
            SeenLoad::Valid(SeenBoundary {
                seen_seq: 0,
                reset_revision: 1
            })
        );
        assert_eq!(view.reset_revision, 1);
        assert_eq!(
            observe(&mut observer, &mut view, at(t0, 3)).await,
            LogAwareness::Unseen(2)
        );
        // Another client's pre-reset candidate is rejected even though its
        // number fits the new head.
        assert_eq!(
            store
                .commit_visits(
                    &[LogVisitCandidate {
                        seen_through: 2,
                        reset_revision: 0
                    }],
                    durable
                )
                .unwrap(),
            VisitCommit::Stale
        );
        // A lower-boundary visit under the new revision works and survives.
        present(&mut observer, &mut view, at(t0, 3), 2).await;
        drop(observer);
        let mut fresh_view = LogAwarenessView::default();
        let mut fresh = LogAwarenessObserver::for_workspace(workspace.path());
        assert_eq!(
            observe(&mut fresh, &mut fresh_view, t0).await,
            LogAwareness::Hidden
        );
        assert_eq!(fresh_view.reset_revision, 1);
    }

    #[tokio::test]
    async fn a_suspected_reset_is_confirmed_against_the_durable_head() {
        // K11 / M11 ("reset from an old cached head"): the journal is replaced
        // by a shorter history, then regrows past the stored boundary before
        // the serialized confirmation. The durable head — not the cached
        // observation that raised the suspicion — decides: a stale suspicion
        // reloads and never resets the shared visit.
        let workspace = tempfile::tempdir().unwrap();
        append_rows(workspace.path(), 5).await;
        let t0 = Instant::now();
        let mut view = LogAwarenessView::default();
        let mut observer = LogAwarenessObserver::for_workspace(workspace.path());
        observe(&mut observer, &mut view, t0).await;
        present(&mut observer, &mut view, t0, 5).await;

        let rooms = workspace.path().join(".rustain/rooms");
        for entry in std::fs::read_dir(&rooms).unwrap() {
            std::fs::remove_file(entry.unwrap().path()).unwrap();
        }
        append_rows(workspace.path(), 2).await;
        assert_eq!(
            observe(&mut observer, &mut view, at(t0, 1)).await,
            LogAwareness::Unavailable
        );
        append_rows(workspace.path(), 5).await;
        observe(&mut observer, &mut view, at(t0, 2)).await;
        assert_eq!(
            stored(workspace.path()),
            SeenLoad::Valid(SeenBoundary {
                seen_seq: 5,
                reset_revision: 0
            }),
            "no reset committed from a head the journal no longer has"
        );
        assert_eq!(
            observe(&mut observer, &mut view, at(t0, 3)).await,
            LogAwareness::Unseen(2)
        );
    }
}
