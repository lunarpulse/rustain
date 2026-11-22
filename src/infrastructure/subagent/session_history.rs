//! Host-local custody for exact, bounded worker visits.
//! The journal is trusted local storage, not a cryptographic authorization log.
//! Journal revisions are indexed without hydrating any worker history.
//! Artifacts are bounded at 16 KiB of metadata and 128 KiB per body.
//! Related-link validation covers only the immediate exact siblings.
//! Repaint leases only probe the selected attempt's bounded metadata paths.

use std::collections::{BTreeMap, HashMap, HashSet};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::NodeJournal;
use super::node_journal::MAX_VISIT_JOURNAL_BYTES;
use crate::adapters::artifact::FileSystemArtifactStore;
use crate::domain::models::AgentId;
use crate::domain::models::artifact::{
    ArtifactKind, ContentHash, EvidenceArtifact, EvidenceArtifactDraft,
};
use crate::domain::models::capability_token::CapabilityTokenId;
use crate::domain::models::conversation::{Conversation, HistorySource};
#[cfg(test)]
use crate::domain::models::node_journal::JournalRecord;
use crate::domain::models::orchestration_room::{HostBinding, RoomEvent, VisitReport};
use crate::domain::models::turn::TurnPart;
use crate::domain::ports::ArtifactStore;
use tokio::io::AsyncReadExt;

pub use crate::domain::models::orchestration_room::MainVisitKey;

const MAX_ATTEMPTS: usize = 65_536;
const MAX_LIVE: usize = 256;
const MAX_OFFERS: usize = 256;
const MAX_TURNS: u32 = 128;
const CAP_ORDINAL: u32 = MAX_TURNS;
const ARTIFACT_VERSION: u32 = 1;
const MAX_TEXT: usize = 16 * 1024;
const MAX_TOOLS: usize = 32;
const MAX_TOOL_NAME: usize = 128;
const MAX_ARTIFACT_BYTES: usize = 128 * 1024;
const MAX_METADATA_BYTES: usize = 16 * 1024;
// Only the immediate exact previous/next links may be inspected.
const RETENTION_MS: i64 = 30 * 24 * 60 * 60 * 1000;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VisitTurn {
    pub ordinal: u32,
    pub tool_names: Vec<String>,
    pub public_text: String,
}
impl std::fmt::Debug for VisitTurn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VisitTurn")
            .field("ordinal", &self.ordinal)
            .field("tool_count", &self.tool_names.len())
            .field("public_text", &"[redacted]")
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisitBody {
    pub attempt: AgentId,
    pub turns: Vec<VisitTurn>,
    /// Compatibility only: standalone Visit never hydrates historic report prose.
    pub report: Option<VisitReport>,
    pub previous: Option<MainVisitKey>,
    pub next: Option<MainVisitKey>,
    /// Absolute local retention deadline, if the durable record has a timestamp.
    pub expires_at_ms: Option<i64>,
    /// Metadata-only lease on the journal and the exact workspace artifacts.
    pub backing: VisitBacking,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VisitRead {
    Available(VisitBody),
    Missing,
    Denied,
    Expired,
    Changed,
    Partial(VisitBody),
}

/// A bounded set of file revisions captured around the verified explicit read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VisitBacking {
    files: Vec<(PathBuf, Option<FileRevision>, RevisionKind)>,
    relation_deadlines: Vec<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RevisionKind {
    Directory,
    Journal,
    Artifact,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FileRevision {
    len: u64,
    modified: Option<SystemTime>,
    #[cfg(unix)]
    identity: (u64, u64, u32),
    #[cfg(unix)]
    changed: (i64, i64),
}

fn metadata_revision(meta: &std::fs::Metadata) -> Option<FileRevision> {
    if meta.file_type().is_symlink() {
        return None;
    }
    #[cfg(unix)]
    if meta.mode() & 0o444 == 0 {
        return None;
    }
    Some(FileRevision {
        len: meta.len(),
        modified: meta.modified().ok(),
        #[cfg(unix)]
        identity: (meta.dev(), meta.ino(), meta.mode()),
        #[cfg(unix)]
        changed: (meta.ctime(), meta.ctime_nsec()),
    })
}

impl FileRevision {
    fn matches(&self, other: &Self, kind: RevisionKind) -> bool {
        #[cfg(unix)]
        if kind == RevisionKind::Directory {
            // A sibling entry can change directory timestamps during a read.
            // The retained descriptor's identity and permissions must not change.
            return self.identity == other.identity;
        }
        #[cfg(not(unix))]
        let _ = kind;
        self == other
    }
}

fn revision(path: &Path) -> Option<FileRevision> {
    metadata_revision(&std::fs::symlink_metadata(path).ok()?)
}

impl VisitBacking {
    #[cfg(test)]
    fn capture(&mut self, path: PathBuf, kind: RevisionKind) {
        if !self.files.iter().any(|(existing, _, _)| existing == &path) {
            let value = revision(&path);
            self.files.push((path, value, kind));
        }
    }

    fn retain(
        &mut self,
        path: PathBuf,
        value: Option<FileRevision>,
        kind: RevisionKind,
    ) -> Result<(), BodyError> {
        if let Some((_, expected, expected_kind)) =
            self.files.iter().find(|(existing, _, _)| existing == &path)
        {
            let matches = match (expected, &value) {
                (Some(before), Some(after)) => before.matches(after, kind),
                (None, None) => true,
                _ => false,
            };
            if *expected_kind != kind || !matches {
                return Err(BodyError::Changed);
            }
        } else {
            self.files.push((path, value, kind));
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum VisitError {
    #[error("visit scope denied or already consumed")]
    Denied,
    #[error("visit capacity exceeded")]
    Capacity,
    #[error("visit storage: {0}")]
    Storage(String),
}
#[derive(Clone, Copy, Debug)]
enum BodyError {
    Missing,
    Changed,
}

struct Scope {
    root: String,
    ordinals: HashSet<u32>,
    failed: bool,
}
struct Offer {
    root: String,
    revision: ContentHash,
    report: VisitReport,
    partial: bool,
}
#[derive(Default)]
struct State {
    live: HashMap<AgentId, Scope>,
    offers: HashMap<AgentId, Offer>,
    used: HashSet<AgentId>,
    offered: HashSet<AgentId>,
}

#[derive(Serialize, Deserialize)]
struct TurnArtifact {
    #[serde(default)]
    version: u32,
    attempt: AgentId,
    root: String,
    turn: VisitTurn,
}
#[derive(Serialize, Deserialize)]
struct LinkAuthorization {
    // Historic report prose is deliberately never deserialized.
    #[serde(default)]
    attempt: Option<AgentId>,
}

#[derive(Serialize, Deserialize)]
struct LinkArtifact {
    #[serde(default)]
    version: u32,
    attempt: AgentId,
    key: MainVisitKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    report: Option<LinkAuthorization>,
    #[serde(default)]
    partial: bool,
}

impl LinkArtifact {
    fn authenticates(&self, key: &MainVisitKey, attempt: &AgentId) -> bool {
        self.key == *key
            && self.attempt == *attempt
            && match self.version {
                0 => {
                    self.report
                        .as_ref()
                        .and_then(|report| report.attempt.as_ref())
                        == Some(attempt)
                }
                ARTIFACT_VERSION => self.report.is_none(),
                _ => false,
            }
    }
}

pub struct SessionHistory {
    journal: Arc<NodeJournal>,
    artifacts: Arc<FileSystemArtifactStore>,
    host: HostBinding,
    state: Mutex<State>,
    writes: tokio::sync::Mutex<()>,
}

pub fn hash_body(body: &str) -> ContentHash {
    ContentHash::from_bytes(Sha256::digest(body.as_bytes()).into())
}

fn truncate(value: &str, max: usize) -> (&str, bool) {
    if value.len() <= max {
        return (value, false);
    }
    let mut end = max;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    (&value[..end], true)
}

impl SessionHistory {
    pub fn new(
        journal: Arc<NodeJournal>,
        artifacts: Arc<FileSystemArtifactStore>,
        host: HostBinding,
    ) -> Self {
        Self {
            journal,
            artifacts,
            host,
            state: Mutex::new(State::default()),
            writes: tokio::sync::Mutex::new(()),
        }
    }

    pub fn begin(&self, root: &str, attempt: &AgentId) -> Result<(), VisitError> {
        let mut state = self.state.lock().unwrap();
        if root.is_empty()
            || !attempt.is_local()
            || *attempt == AgentId::root()
            || state.used.contains(attempt)
        {
            return Err(VisitError::Denied);
        }
        if state.live.len() >= MAX_LIVE || state.used.len() >= MAX_ATTEMPTS {
            return Err(VisitError::Capacity);
        }
        state.used.insert(attempt.clone());
        state.live.insert(
            attempt.clone(),
            Scope {
                root: root.to_owned(),
                ordinals: HashSet::new(),
                failed: false,
            },
        );
        Ok(())
    }
    pub fn admits(&self, attempt: &AgentId) -> bool {
        self.state.lock().unwrap().live.contains_key(attempt)
    }

    pub fn is_recording(&self, attempt: &AgentId) -> bool {
        self.admits(attempt)
    }

    pub fn forget(&self, attempt: &AgentId) {
        self.state.lock().unwrap().live.remove(attempt);
    }

    pub fn offer(
        &self,
        root: &str,
        attempt: &AgentId,
        original: &str,
        report: VisitReport,
    ) -> Result<(), VisitError> {
        let mut state = self.state.lock().unwrap();
        let partial = state
            .live
            .get(attempt)
            .map(|scope| scope.failed)
            .unwrap_or(true);
        if state.live.get(attempt).map(|s| s.root.as_str()) != Some(root)
            || report.attempt.as_ref() != Some(attempt)
            || state.offered.contains(attempt)
        {
            return Err(VisitError::Denied);
        }
        if state.offers.len() >= MAX_OFFERS {
            return Err(VisitError::Capacity);
        }
        state.offered.insert(attempt.clone());
        state.offers.insert(
            attempt.clone(),
            Offer {
                root: root.to_owned(),
                revision: hash_body(original),
                report,
                partial,
            },
        );
        Ok(())
    }

    /// Drop unconsumed offers for a cancelled Main, without affecting other roots.
    pub fn discard_offers(&self, root: &str) {
        let mut state = self.state.lock().unwrap();
        state.offers.retain(|_, offer| offer.root != root);
        state.live.retain(|_, scope| scope.root != root);
    }

    async fn put(&self, attempt: &AgentId, bytes: &[u8]) -> Result<EvidenceArtifact, VisitError> {
        self.validate_storage().map_err(|_| VisitError::Denied)?;
        self.artifacts
            .put(
                EvidenceArtifactDraft {
                    kind: ArtifactKind::Evidence,
                    producer: attempt.clone(),
                    authority: CapabilityTokenId::nil(),
                    provenance: Vec::new(),
                    depends_on: Vec::new(),
                    review: None,
                    host: self.host.clone(),
                },
                bytes,
            )
            .await
            .map_err(|e| VisitError::Storage(e.to_string()))
    }

    pub async fn record_turn(
        &self,
        attempt: &AgentId,
        actual_parent: &AgentId,
        ordinal: u32,
        tool_names: &[String],
        public_text: &str,
    ) -> Result<(), VisitError> {
        let _write = self.writes.lock().await;
        self.validate_storage().map_err(|_| VisitError::Denied)?;
        if *actual_parent != AgentId::root() || !Self::local_attempt(attempt) {
            return Err(VisitError::Denied);
        }
        if ordinal == 0 || ordinal > MAX_TURNS {
            if let Some(scope) = self.state.lock().unwrap().live.get_mut(attempt) {
                scope.failed = true;
            }
            return Err(VisitError::Capacity);
        }
        let root = {
            let mut state = self.state.lock().unwrap();
            let scope = state.live.get_mut(attempt).ok_or(VisitError::Denied)?;
            if !scope.ordinals.insert(ordinal) {
                return Err(VisitError::Denied);
            }
            scope.root.clone()
        };
        // The final slot is a durable cap marker, not a fabricated transcript turn.
        let (text, text_cut) = truncate(public_text, MAX_TEXT);
        let mut partial = text_cut || tool_names.len() > MAX_TOOLS || ordinal == CAP_ORDINAL;
        let mut tools = Vec::new();
        if ordinal < CAP_ORDINAL {
            for name in tool_names.iter().take(MAX_TOOLS) {
                let (name, cut) = truncate(name, MAX_TOOL_NAME);
                partial |= cut;
                tools.push(name.to_owned());
            }
        }
        let turn = VisitTurn {
            ordinal,
            tool_names: tools,
            public_text: if ordinal == CAP_ORDINAL {
                String::new()
            } else {
                text.to_owned()
            },
        };
        let result = async {
            let bytes = serde_json::to_vec(&TurnArtifact {
                version: ARTIFACT_VERSION,
                attempt: attempt.clone(),
                root: root.clone(),
                turn,
            })
            .map_err(|e| VisitError::Storage(e.to_string()))?;
            let artifact = self.put(attempt, &bytes).await?;
            self.journal
                .append_room_bounded(
                    RoomEvent::VisitTurnRecorded {
                        root,
                        attempt: attempt.clone(),
                        ordinal,
                        artifact,
                        partial,
                    },
                    MAX_VISIT_JOURNAL_BYTES,
                )
                .await
                .map_err(|e| VisitError::Storage(e.to_string()))?;
            Ok(())
        }
        .await;
        if partial || result.is_err() {
            if let Some(scope) = self.state.lock().unwrap().live.get_mut(attempt) {
                scope.failed = true;
            }
        }
        result
    }

    pub async fn commit_link(
        &self,
        key: MainVisitKey,
        attempt: AgentId,
    ) -> Result<VisitReport, VisitError> {
        let _write = self.writes.lock().await;
        self.validate_storage().map_err(|_| VisitError::Denied)?;
        if !Self::local_attempt(&attempt) {
            return Err(VisitError::Denied);
        }
        let offer = {
            let mut state = self.state.lock().unwrap();
            let offer = state.offers.get(&attempt).ok_or(VisitError::Denied)?;
            if offer.root != key.conversation_id || offer.revision != key.revision {
                return Err(VisitError::Denied);
            }
            state.offers.remove(&attempt).ok_or(VisitError::Denied)?
        };
        let bytes = serde_json::to_vec(&LinkArtifact {
            version: ARTIFACT_VERSION,
            attempt: attempt.clone(),
            key: key.clone(),
            report: None,
            partial: offer.partial,
        })
        .map_err(|e| VisitError::Storage(e.to_string()))?;
        let artifact = self.put(&attempt, &bytes).await?;
        let entry = self
            .journal
            .append_room_bounded(
                RoomEvent::VisitLinked {
                    key,
                    attempt,
                    artifact,
                },
                MAX_VISIT_JOURNAL_BYTES,
            )
            .await
            .map_err(|e| VisitError::Storage(e.to_string()))?;
        let mut report = offer.report;
        report.expires_at_ms = Self::deadline(entry.recorded_at_ms);
        if Self::check_expiry(report.expires_at_ms).is_some() {
            return Err(VisitError::Storage(
                "invalid or expired visit timestamp".into(),
            ));
        }
        Ok(report)
    }

    fn local_attempt(attempt: &AgentId) -> bool {
        attempt.is_local() && *attempt != AgentId::root()
    }

    fn authentic_artifact(&self, artifact: &EvidenceArtifact, attempt: &AgentId) -> bool {
        Self::local_attempt(attempt)
            && artifact.host == self.host
            && artifact.producer == *attempt
            && artifact.kind == ArtifactKind::Evidence
            && artifact.id.content_hash() == artifact.content_hash
    }

    fn validate_storage(&self) -> Result<(), BodyError> {
        let mut backing = VisitBacking::default();
        self.open_visit_journal(&mut backing)?;
        Self::check_backing(&backing).map_or(Ok(()), |_| Err(BodyError::Changed))
    }

    fn validate_storage_layout(&self) -> Result<(), BodyError> {
        let rooms = self.journal.path().parent().ok_or(BodyError::Changed)?;
        let root = rooms.parent().ok_or(BodyError::Changed)?;
        if rooms.file_name() != Some(std::ffi::OsStr::new("rooms"))
            || root.file_name() != Some(std::ffi::OsStr::new(".rustain"))
            || self.artifacts.root() != root.join("artifacts")
        {
            return Err(BodyError::Changed);
        }
        // An empty store may not exist until the first put; its parent must.
        match std::fs::symlink_metadata(self.artifacts.root()) {
            Ok(meta) if meta.is_dir() => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            _ => Err(BodyError::Changed),
        }
    }

    fn artifact_directory(&self, artifact: &EvidenceArtifact) -> Option<PathBuf> {
        let expected = self.journal.path().parent()?.parent()?.join("artifacts");
        if self.artifacts.root() != expected {
            return None;
        }
        Some(self.artifacts.root().join(artifact.id.as_str()))
    }

    /// Walk from the filesystem root with retained directory descriptors. Never
    /// follow a symlink in either the leaf or any parent component.
    #[cfg(unix)]
    fn open_nofollow(path: &Path, backing: &mut VisitBacking) -> Result<std::fs::File, BodyError> {
        use std::ffi::CString;
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::OpenOptionsExt;
        let path = if path.is_absolute() {
            path.to_owned()
        } else {
            std::env::current_dir()
                .map_err(|_| BodyError::Missing)?
                .join(path)
        };
        let mut components = path.components().peekable();
        if components.next() != Some(std::path::Component::RootDir) {
            return Err(BodyError::Changed);
        }
        let mut current = PathBuf::from("/");
        let mut directory = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&current)
            .map_err(|_| BodyError::Missing)?;
        loop {
            let meta = directory.metadata().map_err(|_| BodyError::Missing)?;
            backing.retain(
                current.clone(),
                Some(metadata_revision(&meta).ok_or(BodyError::Changed)?),
                RevisionKind::Directory,
            )?;
            let Some(std::path::Component::Normal(name)) = components.next() else {
                return Err(BodyError::Changed);
            };
            current.push(name);
            let leaf = components.peek().is_none();
            let name = CString::new(name.as_bytes()).map_err(|_| BodyError::Changed)?;
            let flags = libc::O_RDONLY
                | libc::O_NOFOLLOW
                | libc::O_CLOEXEC
                | libc::O_NONBLOCK
                | if leaf { 0 } else { libc::O_DIRECTORY };
            // SAFETY: directory owns a live fd; name is a terminated component.
            let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
            if fd < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::NotFound {
                    backing.retain(
                        current,
                        None,
                        if leaf {
                            RevisionKind::Artifact
                        } else {
                            RevisionKind::Directory
                        },
                    )?;
                    return Err(BodyError::Missing);
                }
                return Err(BodyError::Changed);
            }
            // SAFETY: openat returned a new owned descriptor.
            let file = unsafe { std::fs::File::from_raw_fd(fd) };
            if leaf {
                return Ok(file);
            }
            directory = file;
        }
    }

    #[cfg(not(unix))]
    fn open_nofollow(
        _path: &Path,
        _backing: &mut VisitBacking,
    ) -> Result<std::fs::File, BodyError> {
        // No portable descriptor-relative no-follow primitive: fail closed.
        Err(BodyError::Changed)
    }

    async fn read_limited(
        path: &Path,
        max: usize,
        kind: RevisionKind,
        backing: &mut VisitBacking,
    ) -> Result<Vec<u8>, BodyError> {
        let file = Self::open_nofollow(path, backing)?;
        let metadata = file.metadata().map_err(|_| BodyError::Missing)?;
        if !metadata.is_file() || metadata.len() > max as u64 {
            return Err(BodyError::Changed);
        }
        let expected = metadata_revision(&metadata).ok_or(BodyError::Changed)?;
        let mut file = tokio::fs::File::from_std(file);
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        (&mut file)
            .take(max as u64 + 1)
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| BodyError::Missing)?;
        if bytes.len() != metadata.len() as usize
            || metadata_revision(&file.metadata().await.map_err(|_| BodyError::Missing)?)
                != Some(expected.clone())
        {
            return Err(BodyError::Changed);
        }
        let path = if path.is_absolute() {
            path.to_owned()
        } else {
            std::env::current_dir()
                .map_err(|_| BodyError::Missing)?
                .join(path)
        };
        backing.retain(path, Some(expected), kind)?;
        Ok(bytes)
    }

    fn open_visit_journal(&self, backing: &mut VisitBacking) -> Result<std::fs::File, BodyError> {
        self.validate_storage_layout()?;
        let path = self.journal.path();
        let file = Self::open_nofollow(path, backing)?;
        let metadata = file.metadata().map_err(|_| BodyError::Missing)?;
        if !metadata.is_file() {
            return Err(BodyError::Changed);
        }
        let path = if path.is_absolute() {
            path.to_owned()
        } else {
            std::env::current_dir()
                .map_err(|_| BodyError::Missing)?
                .join(path)
        };
        backing.retain(
            path,
            Some(metadata_revision(&metadata).ok_or(BodyError::Changed)?),
            RevisionKind::Journal,
        )?;
        Ok(file)
    }

    async fn body<T: for<'de> Deserialize<'de>>(
        &self,
        artifact: &EvidenceArtifact,
        attempt: &AgentId,
        backing: &mut VisitBacking,
    ) -> Result<T, BodyError> {
        if !self.authentic_artifact(artifact, attempt) {
            return Err(BodyError::Changed);
        }
        let directory = self
            .artifact_directory(artifact)
            .ok_or(BodyError::Missing)?;
        let metadata = Self::read_limited(
            &directory.join("meta.json"),
            MAX_METADATA_BYTES,
            RevisionKind::Artifact,
            backing,
        )
        .await?;
        let retained: EvidenceArtifact =
            serde_json::from_slice(&metadata).map_err(|_| BodyError::Changed)?;
        if retained != *artifact {
            return Err(BodyError::Changed);
        }
        let bytes = Self::read_limited(
            &directory.join("body"),
            MAX_ARTIFACT_BYTES,
            RevisionKind::Artifact,
            backing,
        )
        .await?;
        if ContentHash::from_bytes(Sha256::digest(&bytes).into()) != artifact.content_hash {
            return Err(BodyError::Changed);
        }
        serde_json::from_slice(&bytes).map_err(|_| BodyError::Changed)
    }

    fn exact(key: &MainVisitKey, active: &Conversation) -> bool {
        if active.id != key.conversation_id {
            return false;
        }
        let mut turns = active.turns.iter().filter(|t| t.id == key.turn_id);
        let Some(turn) = turns.next() else {
            return false;
        };
        if turns.next().is_some()
            || turn.history_source != HistorySource::Local
            || turn.role != crate::domain::models::MessageRole::Assistant
            || key.invocation == key.result
        {
            return false;
        }
        let mut invocations = 0;
        let mut results = 0;
        let mut referenced_results = 0;
        for part in &turn.parts {
            let id = match part {
                TurnPart::Prose { id, .. }
                | TurnPart::Reasoning { id, .. }
                | TurnPart::ToolInvocation { id, .. }
                | TurnPart::ToolResult { id, .. }
                | TurnPart::ProviderReplay { id, .. } => *id,
            };
            if matches!(part, TurnPart::ToolResult { refs, .. } if *refs == key.invocation) {
                referenced_results += 1;
            }
            if id == key.invocation {
                invocations += 1;
                if !matches!(part, TurnPart::ToolInvocation { tool, .. } if tool == "task") {
                    return false;
                }
            }
            if id == key.result {
                results += 1;
                if !matches!(part, TurnPart::ToolResult { refs, output, .. } if *refs == key.invocation && !output.is_error && hash_body(&output.content) == key.revision)
                {
                    return false;
                }
            }
        }
        invocations == 1 && results == 1 && referenced_results == 1
    }

    /// Check the current local source without reading the journal or artifacts.
    pub fn check_source(
        &self,
        key: &MainVisitKey,
        active: &Conversation,
        local_allowed: bool,
    ) -> Option<VisitRead> {
        if !local_allowed || active.id != key.conversation_id {
            return Some(VisitRead::Denied);
        }
        if !Self::exact(key, active) {
            return Some(VisitRead::Changed);
        }
        None
    }

    /// Cache only availability deadlines from the bounded canonical journal.
    /// This is not proof of retained bodies: explicit resolution still verifies
    /// artifact metadata, hashes and authority before exposing any private text.
    pub async fn load_visit_deadlines(&self, conversation: &mut Conversation, local_allowed: bool) {
        for turn in &mut conversation.turns {
            turn.visit_deadlines.clear();
        }
        if !local_allowed {
            return;
        }
        let mut backing = VisitBacking::default();
        let Ok(file) = self.open_visit_journal(&mut backing) else {
            return;
        };
        let Ok(links) = self.journal.visit_links(file, conversation.id.clone()).await else {
            return;
        };
        let mut key_counts = HashMap::new();
        for (row, _) in &links {
            *key_counts.entry(&row.key).or_insert(0usize) += 1;
        }
        for (row, unique_attempt) in &links {
            let (key, attempt, artifact) = (&row.key, &row.attempt, &row.artifact);
            if key_counts.get(key) != Some(&1)
                || !unique_attempt
                || !self.authentic_artifact(artifact, attempt)
                || self
                    .check_source(key, conversation, local_allowed)
                    .is_some()
            {
                continue;
            }
            let expires_at_ms = Self::deadline(row.stamp_ms);
            if Self::check_expiry(expires_at_ms).is_some() {
                continue;
            }
            if let (Some(deadline), Some(turn)) = (
                expires_at_ms,
                conversation
                    .turns
                    .iter_mut()
                    .find(|turn| turn.id == key.turn_id),
            ) {
                turn.visit_deadlines.insert(key.result, deadline);
            }
        }
        if Self::check_backing(&backing).is_some() {
            for turn in &mut conversation.turns {
                turn.visit_deadlines.clear();
            }
        }
    }
    fn deadline(recorded_at_ms: i64) -> Option<i64> {
        let now = Self::now_ms()?;
        if recorded_at_ms <= 0 || recorded_at_ms > now {
            return None;
        }
        recorded_at_ms.checked_add(RETENTION_MS)
    }

    fn now_ms() -> Option<i64> {
        i64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .ok()?
                .as_millis(),
        )
        .ok()
    }

    /// Recheck cached retention without loading the journal or private bodies.
    pub fn check_expiry(expires_at_ms: Option<i64>) -> Option<VisitRead> {
        match (expires_at_ms, Self::now_ms()) {
            (Some(deadline), Some(now))
                if deadline > now
                    && deadline
                        .checked_sub(RETENTION_MS)
                        .is_some_and(|stamp| stamp > 0 && stamp <= now) =>
            {
                None
            }
            _ => Some(VisitRead::Expired),
        }
    }

    /// Compare file metadata only; explicit resolution still verifies every body.
    pub fn check_backing(backing: &VisitBacking) -> Option<VisitRead> {
        if backing
            .relation_deadlines
            .iter()
            .any(|deadline| Self::check_expiry(Some(*deadline)).is_some())
        {
            return Some(VisitRead::Changed);
        }
        if backing.files.iter().any(|(path, expected, kind)| {
            let current = revision(path);
            match (expected, &current) {
                (Some(before), Some(after)) => !before.matches(after, *kind),
                (None, None) => false,
                _ => true,
            }
        }) {
            Some(VisitRead::Changed)
        } else {
            None
        }
    }

    pub async fn resolve(
        &self,
        key: &MainVisitKey,
        active: &Conversation,
        local_allowed: bool,
    ) -> VisitRead {
        if let Some(failure) = self.check_source(key, active, local_allowed) {
            return failure;
        }
        let mut backing = VisitBacking::default();
        let Ok(file) = self.open_visit_journal(&mut backing) else {
            return VisitRead::Missing;
        };
        let rows = match self.journal.visit_rows(file, key.clone(), MAX_TURNS as usize).await {
            Ok(Some(rows)) => rows,
            Ok(None) => return VisitRead::Changed,
            Err(_) => return VisitRead::Missing,
        };
        if let Some(failure) = Self::check_backing(&backing) {
            return failure;
        }
        let links: Vec<_> = rows.links.iter()
            .filter(|(row, _)| Self::exact(&row.key, active))
            .collect();
        let mut selected = links.iter().enumerate().filter(|(_, (row, _))| &row.key == key);
        let Some((selected_index, (row, unique_attempt))) = selected.next() else {
            return VisitRead::Missing;
        };
        if selected.next().is_some() || !unique_attempt {
            return VisitRead::Changed;
        }
        let (attempt, artifact) = (&row.attempt, &row.artifact);
        if !self.authentic_artifact(artifact, attempt) {
            return VisitRead::Denied;
        }
        let expires_at_ms = Self::deadline(row.stamp_ms);
        if let Some(failure) = Self::check_expiry(expires_at_ms) {
            return failure;
        }
        let link: LinkArtifact = match self.body(artifact, attempt, &mut backing).await {
            Ok(link) => link,
            Err(BodyError::Missing) => return VisitRead::Missing,
            Err(BodyError::Changed) => return VisitRead::Changed,
        };
        if !link.authenticates(key, attempt) {
            return VisitRead::Changed;
        }
        let mut turns = BTreeMap::new();
        let mut seen = HashSet::new();
        let mut partial = link.partial;
        for row in &rows.turns {
            let (root, ordinal, artifact) = (&row.root, &row.ordinal, &row.artifact);
            if root != &key.conversation_id
                || !seen.insert(*ordinal)
                || *ordinal == 0
                || *ordinal > MAX_TURNS
            {
                return VisitRead::Changed;
            }
            let record: TurnArtifact = match self.body(artifact, attempt, &mut backing).await {
                Ok(record) => record,
                Err(BodyError::Missing) => {
                    partial = true;
                    continue;
                }
                Err(BodyError::Changed) => return VisitRead::Changed,
            };
            if !matches!(record.version, 0 | ARTIFACT_VERSION)
                || record.attempt != *attempt
                || record.root != *root
                || record.turn.ordinal != *ordinal
            {
                return VisitRead::Changed;
            }
            if record.turn.public_text.len() > MAX_TEXT
                || record.turn.tool_names.len() > MAX_TOOLS
                || record
                    .turn
                    .tool_names
                    .iter()
                    .any(|name| name.len() > MAX_TOOL_NAME)
                || (*ordinal == CAP_ORDINAL
                    && (!record.turn.public_text.is_empty() || !record.turn.tool_names.is_empty()))
            {
                return VisitRead::Changed;
            }
            partial |= row.partial || *ordinal == CAP_ORDINAL;
            if *ordinal < CAP_ORDINAL {
                turns.insert(*ordinal, record.turn);
            }
        }
        if seen.is_empty() {
            partial = true;
        }
        let mut neighbors = [None, None];
        let mut relation_deadlines = Vec::new();
        // Do not search past an unavailable immediate neighbor or touch unrelated
        // siblings. The journal and each exposed relation share the same lease.
        for (slot, index) in [selected_index.checked_sub(1), selected_index.checked_add(1)]
            .into_iter()
            .enumerate()
        {
            let Some((other, unique_attempt)) = index.and_then(|index| links.get(index)).copied() else {
                continue;
            };
            let (candidate, other_attempt, other_artifact) = (&other.key, &other.attempt, &other.artifact);
            let deadline = Self::deadline(other.stamp_ms);
            if Self::check_expiry(deadline).is_some()
                || links.iter().filter(|(row, _)| &row.key == candidate).take(2).count() != 1
                || !unique_attempt
                || !self.authentic_artifact(other_artifact, other_attempt)
            {
                continue;
            }
            let mut related_backing = backing.clone();
            if let Ok(other) = self
                .body::<LinkArtifact>(other_artifact, other_attempt, &mut related_backing)
                .await
            {
                if other.authenticates(candidate, other_attempt)
                    && Self::check_backing(&related_backing).is_none()
                    && Self::check_expiry(deadline).is_none()
                {
                    neighbors[slot] = Some(candidate.clone());
                    relation_deadlines.push(deadline.unwrap());
                    backing = related_backing;
                }
            }
        }
        backing.relation_deadlines = relation_deadlines;
        if let Some(failure) = Self::check_backing(&backing) {
            return failure;
        }
        if let Some(failure) = Self::check_expiry(expires_at_ms) {
            return failure;
        }
        let [previous, next] = neighbors;
        let body = VisitBody {
            attempt: attempt.clone(),
            turns: turns.into_values().collect(),
            report: None,
            previous,
            next,
            expires_at_ms,
            backing,
        };
        if partial {
            VisitRead::Partial(body)
        } else {
            VisitRead::Available(body)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::artifact::FileSystemArtifactStore;
    use crate::domain::models::turn::{InvocationStatus, ToolOutput, Turn, TurnPart};

    fn conversation() -> (Conversation, MainVisitKey) {
        let mut turn = Turn::new("test".into(), 0);
        let invocation = turn.push_part(|id| TurnPart::ToolInvocation {
            id,
            tool: "task".into(),
            args: serde_json::json!({}),
            status: InvocationStatus::Success,
            started_at: 0,
            ended_at: None,
        });
        let result = turn.push_part(|id| TurnPart::ToolResult {
            id,
            refs: invocation,
            output: ToolOutput {
                content: "original".into(),
                is_error: false,
                diff: Default::default(),
                images: Vec::new(),
            },
        });
        let key = MainVisitKey {
            conversation_id: "root-a".into(),
            turn_id: turn.id.clone(),
            invocation,
            result,
            revision: hash_body("original"),
        };
        (
            Conversation {
                id: "root-a".into(),
                turns: vec![turn],
                ..Default::default()
            },
            key,
        )
    }
    #[test]
    fn exact_source_rejects_competing_results_and_non_assistant_turns() {
        let (mut active, key) = conversation();
        assert!(SessionHistory::exact(&key, &active));
        active.turns[0].role = crate::domain::models::MessageRole::User;
        assert!(!SessionHistory::exact(&key, &active));
        active.turns[0].role = crate::domain::models::MessageRole::Assistant;
        if let TurnPart::ToolResult { output, .. } = &mut active.turns[0].parts[1] {
            output.is_error = true;
        }
        assert!(!SessionHistory::exact(&key, &active));
        if let TurnPart::ToolResult { output, .. } = &mut active.turns[0].parts[1] {
            output.is_error = false;
        }
        active.turns[0].push_part(|id| TurnPart::ToolResult {
            id,
            refs: key.invocation,
            output: ToolOutput {
                content: "other".into(),
                is_error: false,
                diff: Default::default(),
                images: Vec::new(),
            },
        });
        assert!(!SessionHistory::exact(&key, &active));
    }

    #[test]
    fn cached_expiration_and_debug_redaction() {
        assert!(matches!(
            SessionHistory::check_expiry(Some(1)),
            Some(VisitRead::Expired)
        ));
        assert!(matches!(
            SessionHistory::check_expiry(None),
            Some(VisitRead::Expired)
        ));
        assert_eq!(SessionHistory::deadline(0), None);
        assert_eq!(SessionHistory::deadline(-1), None);
        let turn = VisitTurn {
            ordinal: 1,
            tool_names: vec!["private tool".into()],
            public_text: "sensitive text".into(),
        };
        let debug = format!("{turn:?}");
        assert!(!debug.contains("sensitive text"));
        assert!(!debug.contains("private tool"));
    }

    #[tokio::test]
    async fn custody_survives_restart_and_revalidates_local_owner() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(NodeJournal::open_workspace(dir.path()).await.unwrap());
        let store = Arc::new(FileSystemArtifactStore::new(dir.path()));
        let host = HostBinding::new("host", "workspace");
        let history = SessionHistory::new(journal.clone(), store.clone(), host.clone());
        let (mut active, key) = conversation();
        let attempt = AgentId::new();
        let report = VisitReport {
            report: None,
            unstructured: true,
            attempt: Some(attempt.clone()),
            expires_at_ms: None,
        };
        history.begin("root-a", &attempt).unwrap();
        assert!(
            history
                .record_turn(&attempt, &AgentId::new(), 1, &[], "DENIED_PRIVATE_MARKER")
                .await
                .is_err()
        );
        history
            .record_turn(
                &attempt,
                &AgentId::root(),
                1,
                &["Read: ok".into()],
                "PUBLIC_ONLY_MARKER",
            )
            .await
            .unwrap();
        assert!(
            history
                .record_turn(&attempt, &AgentId::root(), 1, &[], "duplicate")
                .await
                .is_err()
        );
        assert!(
            history
                .offer("root-b", &attempt, "original", report.clone())
                .is_err()
        );
        history
            .offer("root-a", &attempt, "original", report.clone())
            .unwrap();
        history.forget(&attempt);
        let mut wrong = key.clone();
        wrong.revision = hash_body("not original");
        assert!(history.commit_link(wrong, attempt.clone()).await.is_err());
        let report = history
            .commit_link(key.clone(), attempt.clone())
            .await
            .unwrap();
        let stamp = journal
            .load()
            .await
            .unwrap()
            .into_iter()
            .find_map(|entry| {
                matches!(
                    entry.record,
                    JournalRecord::Room(RoomEvent::VisitLinked { .. })
                )
                .then_some(entry.recorded_at_ms)
            })
            .unwrap();
        assert_eq!(report.expires_at_ms, SessionHistory::deadline(stamp));
        assert!(
            history
                .commit_link(key.clone(), attempt.clone())
                .await
                .is_err()
        );
        let reopened = SessionHistory::new(journal.clone(), store, host);
        assert!(matches!(
            reopened.resolve(&key, &active, false).await,
            VisitRead::Denied
        ));
        let body = match reopened.resolve(&key, &active, true).await {
            VisitRead::Available(body) => body,
            status => panic!("expected retained visit after restart, got {status:?}"),
        };
        assert_eq!(body.turns.len(), 1);
        assert_eq!(body.turns[0].public_text, "PUBLIC_ONLY_MARKER");
        assert!(body.report.is_none());
        let journal_text = std::fs::read_to_string(journal.path()).unwrap();
        assert!(!journal_text.contains("PUBLIC_ONLY_MARKER"));
        assert!(!journal_text.contains("DENIED_PRIVATE_MARKER"));
        let serialized = serde_json::to_string(&active).unwrap();
        assert!(!serialized.contains("visitReports"));
        let mut foreign = active.clone();
        foreign.id = "root-b".into();
        assert!(matches!(
            reopened.resolve(&key, &foreign, true).await,
            VisitRead::Denied
        ));
        if let TurnPart::ToolResult { output, .. } = &mut active.turns[0].parts[1] {
            output.content = "changed".into();
        }
        assert!(matches!(
            reopened.resolve(&key, &active, true).await,
            VisitRead::Changed
        ));
        if let TurnPart::ToolResult { output, .. } = &mut active.turns[0].parts[1] {
            output.content = "original".into();
        }
        let duplicate = journal
            .load()
            .await
            .unwrap()
            .into_iter()
            .find_map(|entry| match entry.record {
                JournalRecord::Room(event @ RoomEvent::VisitLinked { .. }) => Some(event),
                _ => None,
            })
            .unwrap();
        journal.append_room(duplicate).await.unwrap();
        assert!(matches!(
            reopened.resolve(&key, &active, true).await,
            VisitRead::Changed
        ));
    }

    #[tokio::test]
    async fn availability_has_no_preview_or_body_dependency_and_clears_denied_sources() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(NodeJournal::open_workspace(dir.path()).await.unwrap());
        let history = SessionHistory::new(
            journal.clone(),
            Arc::new(FileSystemArtifactStore::new(dir.path())),
            HostBinding::new("host", "workspace"),
        );
        let (mut active, key) = conversation();
        let original_main = serde_json::to_vec(&active).unwrap();
        let attempt = AgentId::new();
        history.begin("root-a", &attempt).unwrap();
        history
            .record_turn(&attempt, &AgentId::root(), 1, &[], "PRIVATE_NEVER_PREVIEW")
            .await
            .unwrap();
        history
            .offer(
                "root-a",
                &attempt,
                "original",
                VisitReport {
                    report: None,
                    unstructured: true,
                    attempt: Some(attempt.clone()),
                    expires_at_ms: None,
                },
            )
            .unwrap();
        let report = history
            .commit_link(key.clone(), attempt.clone())
            .await
            .unwrap();
        let entries = journal.load().await.unwrap();
        for entry in entries {
            if let JournalRecord::Room(
                RoomEvent::VisitLinked { artifact, .. }
                | RoomEvent::VisitTurnRecorded { artifact, .. },
            ) = entry.record
            {
                // Even invalid metadata and absent bodies are resolved only on
                // explicit open; availability must not hydrate either of them.
                let directory = history.artifact_directory(&artifact).unwrap();
                std::fs::remove_file(directory.join("body")).unwrap();
                std::fs::write(directory.join("meta.json"), b"not a report").unwrap();
            }
        }
        history.load_visit_deadlines(&mut active, true).await;
        assert_eq!(
            active.turns[0].visit_deadlines.get(&key.result),
            report.expires_at_ms.as_ref()
        );
        assert_eq!(serde_json::to_vec(&active).unwrap(), original_main);
        // Corrupt metadata takes precedence over the absent body: it is changed
        // evidence, not merely unretained history.
        assert_eq!(
            history.resolve(&key, &active, true).await,
            VisitRead::Changed
        );
        history.load_visit_deadlines(&mut active, false).await;
        assert!(active.turns[0].visit_deadlines.is_empty());
        history.load_visit_deadlines(&mut active, true).await;
        active.turns[0].history_source = HistorySource::PeerDisplay;
        history.load_visit_deadlines(&mut active, true).await;
        assert!(active.turns[0].visit_deadlines.is_empty());
        active.turns[0].history_source = HistorySource::Local;
        history.load_visit_deadlines(&mut active, true).await;
        if let TurnPart::ToolResult { output, .. } = &mut active.turns[0].parts[1] {
            output.content = "changed".into();
        }
        history.load_visit_deadlines(&mut active, true).await;
        assert!(active.turns[0].visit_deadlines.is_empty());
    }

    #[tokio::test]
    async fn availability_requires_unique_keys_and_attempts_across_roots() {
        for duplicate_key in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let journal = Arc::new(NodeJournal::open_workspace(dir.path()).await.unwrap());
            let history = SessionHistory::new(
                journal.clone(),
                Arc::new(FileSystemArtifactStore::new(dir.path())),
                HostBinding::new("host", "workspace"),
            );
            let (mut active, key) = conversation();
            let attempt = AgentId::new();
            history.begin("root-a", &attempt).unwrap();
            history
                .offer(
                    "root-a",
                    &attempt,
                    "original",
                    VisitReport {
                        report: None,
                        unstructured: true,
                        attempt: Some(attempt.clone()),
                        expires_at_ms: None,
                    },
                )
                .unwrap();
            history
                .commit_link(key.clone(), attempt.clone())
                .await
                .unwrap();
            history.load_visit_deadlines(&mut active, true).await;
            assert!(active.turns[0].visit_deadlines.contains_key(&key.result));
            let mut duplicate = journal
                .load()
                .await
                .unwrap()
                .into_iter()
                .find_map(|entry| match entry.record {
                    JournalRecord::Room(event @ RoomEvent::VisitLinked { .. }) => Some(event),
                    _ => None,
                })
                .unwrap();
            if let RoomEvent::VisitLinked { key, attempt, .. } = &mut duplicate {
                if duplicate_key {
                    *attempt = AgentId::new();
                } else {
                    key.conversation_id = "other-root".into();
                }
            }
            journal.append_room(duplicate).await.unwrap();
            history.load_visit_deadlines(&mut active, true).await;
            assert!(active.turns[0].visit_deadlines.is_empty());
        }
    }

    #[tokio::test]
    async fn availability_rejects_mismatched_typed_link_metadata() {
        for invalid in 0..6 {
            let dir = tempfile::tempdir().unwrap();
            let journal = Arc::new(NodeJournal::open_workspace(dir.path()).await.unwrap());
            let history = SessionHistory::new(
                journal.clone(),
                Arc::new(FileSystemArtifactStore::new(dir.path())),
                HostBinding::new("host", "workspace"),
            );
            let (mut active, key) = conversation();
            let mut attempt = AgentId::new();
            let mut artifact = history
                .put(&attempt, b"PRIVATE_INVALID_REPORT_BODY")
                .await
                .unwrap();
            match invalid {
                0 => artifact.host = HostBinding::new("other-host", "workspace"),
                1 => artifact.producer = AgentId::new(),
                2 => artifact.kind = ArtifactKind::Patch,
                3 => artifact.content_hash = hash_body("different body"),
                4 => {
                    attempt = AgentId::from_peer_path("peer/worker").unwrap();
                    artifact.producer = attempt.clone();
                }
                _ => {
                    attempt = AgentId::root();
                    artifact.producer = attempt.clone();
                }
            }
            journal
                .append_room(RoomEvent::VisitLinked {
                    key: key.clone(),
                    attempt,
                    artifact,
                })
                .await
                .unwrap();
            active.turns[0].visit_deadlines.insert(key.result, i64::MAX);
            history.load_visit_deadlines(&mut active, true).await;
            assert!(
                active.turns[0].visit_deadlines.is_empty(),
                "invalid case {invalid}"
            );
        }
    }

    #[tokio::test]
    async fn bounded_records_and_mismatched_evidence_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(NodeJournal::open_workspace(dir.path()).await.unwrap());
        let store = Arc::new(FileSystemArtifactStore::new(dir.path()));
        let history = SessionHistory::new(
            journal.clone(),
            store,
            HostBinding::new("host", "workspace"),
        );
        let (active, key) = conversation();
        let attempt = AgentId::new();
        history.begin("root-a", &attempt).unwrap();
        history
            .record_turn(
                &attempt,
                &AgentId::root(),
                1,
                &[],
                &"x".repeat(MAX_TEXT + 1),
            )
            .await
            .unwrap();
        history
            .offer(
                "root-a",
                &attempt,
                "original",
                VisitReport {
                    report: None,
                    unstructured: true,
                    attempt: Some(attempt.clone()),
                    expires_at_ms: None,
                },
            )
            .unwrap();
        history
            .commit_link(key.clone(), attempt.clone())
            .await
            .unwrap();
        let body = match history.resolve(&key, &active, true).await {
            VisitRead::Partial(body) => body,
            status => panic!("expected bounded partial visit, got {status:?}"),
        };
        assert_eq!(body.turns.len(), 1);
        assert_eq!(body.turns[0].public_text.len(), MAX_TEXT);
        assert!(body.report.is_none());
        let entries = journal.load().await.unwrap();
        let artifact = entries
            .iter()
            .find_map(|entry| match &entry.record {
                JournalRecord::Room(RoomEvent::VisitTurnRecorded { artifact, .. }) => {
                    Some(artifact.clone())
                }
                _ => None,
            })
            .unwrap();
        journal
            .append_room(RoomEvent::VisitTurnRecorded {
                root: "root-b".into(),
                attempt,
                ordinal: 2,
                artifact,
                partial: false,
            })
            .await
            .unwrap();
        assert!(matches!(
            history.resolve(&key, &active, true).await,
            VisitRead::Changed
        ));
    }

    #[tokio::test]
    async fn mismatched_and_duplicate_journal_evidence_never_resolves() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(NodeJournal::open_workspace(dir.path()).await.unwrap());
        let store = Arc::new(FileSystemArtifactStore::new(dir.path()));
        let history = SessionHistory::new(
            journal.clone(),
            store,
            HostBinding::new("host", "workspace"),
        );
        let (active, key) = conversation();
        let attempt = AgentId::new();
        history.begin("root-a", &attempt).unwrap();
        history
            .record_turn(&attempt, &AgentId::root(), 1, &[], "same body")
            .await
            .unwrap();
        history
            .offer(
                "root-a",
                &attempt,
                "original",
                VisitReport {
                    report: None,
                    unstructured: true,
                    attempt: Some(attempt.clone()),
                    expires_at_ms: None,
                },
            )
            .unwrap();
        history
            .commit_link(key.clone(), attempt.clone())
            .await
            .unwrap();
        let entries = journal.load().await.unwrap();
        let artifact = entries
            .iter()
            .find_map(|entry| match &entry.record {
                JournalRecord::Room(RoomEvent::VisitTurnRecorded { artifact, .. }) => {
                    Some(artifact.clone())
                }
                _ => None,
            })
            .unwrap();
        // An artifact for ordinal one cannot substantiate a different ordinal.
        journal
            .append_room(RoomEvent::VisitTurnRecorded {
                root: "root-a".into(),
                attempt: attempt.clone(),
                ordinal: 2,
                artifact: artifact.clone(),
                partial: false,
            })
            .await
            .unwrap();
        assert!(matches!(
            history.resolve(&key, &active, true).await,
            VisitRead::Changed
        ));
        // Nor can the same ordinal occur twice, even with the identical handle.
        journal
            .append_room(RoomEvent::VisitTurnRecorded {
                root: "root-a".into(),
                attempt,
                ordinal: 1,
                artifact,
                partial: false,
            })
            .await
            .unwrap();
        assert!(matches!(
            history.resolve(&key, &active, true).await,
            VisitRead::Changed
        ));
    }

    #[tokio::test]
    async fn zero_stamp_link_is_never_published() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(
            NodeJournal::open_workspace(dir.path())
                .await
                .unwrap()
                .with_clock(Arc::new(crate::domain::clock::MockClock::at_wall_ms(0))),
        );
        let store = Arc::new(FileSystemArtifactStore::new(dir.path()));
        let history = SessionHistory::new(journal, store, HostBinding::new("host", "workspace"));
        let (active, key) = conversation();
        let attempt = AgentId::new();
        history.begin("root-a", &attempt).unwrap();
        history
            .offer(
                "root-a",
                &attempt,
                "original",
                VisitReport {
                    report: None,
                    unstructured: true,
                    attempt: Some(attempt.clone()),
                    expires_at_ms: None,
                },
            )
            .unwrap();
        assert!(history.commit_link(key.clone(), attempt).await.is_err());
        assert!(matches!(
            history.resolve(&key, &active, true).await,
            VisitRead::Expired
        ));
    }

    #[tokio::test]
    async fn failed_writer_survives_restart_as_partial() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(NodeJournal::open_workspace(dir.path()).await.unwrap());
        let store = Arc::new(FileSystemArtifactStore::new(dir.path()).with_max_body_bytes(1024));
        let host = HostBinding::new("host", "workspace");
        let history = SessionHistory::new(journal.clone(), store.clone(), host.clone());
        let (active, key) = conversation();
        let attempt = AgentId::new();
        history.begin("root-a", &attempt).unwrap();
        history
            .record_turn(&attempt, &AgentId::root(), 1, &[], "kept")
            .await
            .unwrap();
        assert!(
            history
                .record_turn(&attempt, &AgentId::root(), 2, &[], &"x".repeat(2048))
                .await
                .is_err()
        );
        history
            .offer(
                "root-a",
                &attempt,
                "original",
                VisitReport {
                    report: None,
                    unstructured: true,
                    attempt: Some(attempt.clone()),
                    expires_at_ms: None,
                },
            )
            .unwrap();
        history.commit_link(key.clone(), attempt).await.unwrap();
        let reopened = SessionHistory::new(journal, store, host);
        let body = match reopened.resolve(&key, &active, true).await {
            VisitRead::Partial(body) => body,
            status => panic!("expected durable writer-failure marker, got {status:?}"),
        };
        assert_eq!(body.turns.len(), 1);
        assert_eq!(body.turns[0].public_text, "kept");
        assert!(body.report.is_none());
    }

    #[tokio::test]
    async fn link_without_committed_turn_is_partial() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(NodeJournal::open_workspace(dir.path()).await.unwrap());
        let history = SessionHistory::new(
            journal,
            Arc::new(FileSystemArtifactStore::new(dir.path())),
            HostBinding::new("host", "workspace"),
        );
        let (active, key) = conversation();
        let attempt = AgentId::new();
        history.begin("root-a", &attempt).unwrap();
        history
            .offer(
                "root-a",
                &attempt,
                "original",
                VisitReport {
                    report: None,
                    unstructured: true,
                    attempt: Some(attempt.clone()),
                    expires_at_ms: None,
                },
            )
            .unwrap();
        history.commit_link(key.clone(), attempt).await.unwrap();
        assert!(
            matches!(history.resolve(&key, &active, true).await, VisitRead::Partial(body) if body.turns.is_empty())
        );
    }

    #[tokio::test]
    async fn backing_detects_replacement_and_journal_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(NodeJournal::open_workspace(dir.path()).await.unwrap());
        let store = Arc::new(FileSystemArtifactStore::new(dir.path()));
        let history = SessionHistory::new(
            journal.clone(),
            store,
            HostBinding::new("host", "workspace"),
        );
        let (active, key) = conversation();
        let attempt = AgentId::new();
        history.begin("root-a", &attempt).unwrap();
        history
            .record_turn(&attempt, &AgentId::root(), 1, &[], "text")
            .await
            .unwrap();
        history
            .offer(
                "root-a",
                &attempt,
                "original",
                VisitReport {
                    report: None,
                    unstructured: true,
                    attempt: Some(attempt.clone()),
                    expires_at_ms: None,
                },
            )
            .unwrap();
        history.commit_link(key.clone(), attempt).await.unwrap();
        let body = match history.resolve(&key, &active, true).await {
            VisitRead::Available(body) => body,
            status => panic!("expected retained visit before replacement, got {status:?}"),
        };
        assert_eq!(SessionHistory::check_backing(&body.backing), None);
        let other = AgentId::new();
        history.begin("root-b", &other).unwrap();
        history
            .record_turn(&other, &AgentId::root(), 1, &[], "unrelated")
            .await
            .unwrap();
        assert!(matches!(
            SessionHistory::check_backing(&body.backing),
            Some(VisitRead::Changed)
        ));
        let turn_artifact = journal
            .load()
            .await
            .unwrap()
            .into_iter()
            .find_map(|entry| match entry.record {
                JournalRecord::Room(RoomEvent::VisitTurnRecorded { artifact, .. }) => {
                    Some(artifact)
                }
                _ => None,
            })
            .unwrap();
        let path = dir
            .path()
            .join(".rustain/artifacts")
            .join(turn_artifact.id.as_str())
            .join("body");
        let replacement = path.with_extension("replacement");
        std::fs::write(&replacement, std::fs::read(&path).unwrap()).unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        assert!(matches!(
            SessionHistory::check_backing(&body.backing),
            Some(VisitRead::Changed)
        ));
        let mut journal_backing = VisitBacking::default();
        journal_backing.capture(journal.path().to_owned(), RevisionKind::Journal);
        std::fs::write(journal.path(), b"").unwrap();
        assert!(matches!(
            SessionHistory::check_backing(&journal_backing),
            Some(VisitRead::Changed)
        ));
    }

    #[tokio::test]
    async fn discarding_one_root_rejects_late_offers_without_touching_another() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(NodeJournal::open_workspace(dir.path()).await.unwrap());
        let history = SessionHistory::new(
            journal,
            Arc::new(FileSystemArtifactStore::new(dir.path())),
            HostBinding::new("host", "workspace"),
        );
        let a = AgentId::new();
        let b = AgentId::new();
        history.begin("root-a", &a).unwrap();
        history.begin("root-b", &b).unwrap();
        let report = |attempt: &AgentId| VisitReport {
            report: None,
            unstructured: true,
            attempt: Some(attempt.clone()),
            expires_at_ms: None,
        };
        history.offer("root-b", &b, "original", report(&b)).unwrap();
        history.discard_offers("root-a");
        assert!(history.offer("root-a", &a, "original", report(&a)).is_err());
        assert!(history.state.lock().unwrap().offers.contains_key(&b));
    }

    #[tokio::test]
    async fn missing_link_and_withdrawn_body_never_substitute_history() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(NodeJournal::open_workspace(dir.path()).await.unwrap());
        let store = Arc::new(FileSystemArtifactStore::new(dir.path()));
        let history = SessionHistory::new(
            journal.clone(),
            store,
            HostBinding::new("host", "workspace"),
        );
        let (active, key) = conversation();
        assert!(matches!(
            history.resolve(&key, &active, true).await,
            VisitRead::Missing
        ));
        let attempt = AgentId::new();
        history.begin("root-a", &attempt).unwrap();
        history
            .record_turn(&attempt, &AgentId::root(), 1, &[], "PUBLIC_ONLY_MARKER")
            .await
            .unwrap();
        history
            .offer(
                "root-a",
                &attempt,
                "original",
                VisitReport {
                    report: None,
                    unstructured: true,
                    attempt: Some(attempt.clone()),
                    expires_at_ms: None,
                },
            )
            .unwrap();
        history.commit_link(key.clone(), attempt).await.unwrap();
        let entries = journal.load().await.unwrap();
        let turn_artifact = entries
            .iter()
            .find_map(|entry| match &entry.record {
                JournalRecord::Room(RoomEvent::VisitTurnRecorded { artifact, .. }) => {
                    Some(artifact)
                }
                _ => None,
            })
            .unwrap();
        let turn_path = history
            .artifact_directory(turn_artifact)
            .unwrap()
            .join("body");
        std::fs::remove_file(turn_path).unwrap();
        assert!(
            matches!(history.resolve(&key, &active, true).await, VisitRead::Partial(body) if body.turns.is_empty())
        );
        let link_artifact = entries
            .iter()
            .find_map(|entry| match &entry.record {
                JournalRecord::Room(RoomEvent::VisitLinked { artifact, .. }) => Some(artifact),
                _ => None,
            })
            .unwrap();
        std::fs::remove_file(
            history
                .artifact_directory(link_artifact)
                .unwrap()
                .join("body"),
        )
        .unwrap();
        assert!(matches!(
            history.resolve(&key, &active, true).await,
            VisitRead::Missing
        ));
    }

    #[tokio::test]
    async fn expired_retention_is_not_revived_by_restart() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(
            NodeJournal::open_workspace(dir.path())
                .await
                .unwrap()
                .with_clock(Arc::new(crate::domain::clock::MockClock::at_wall_ms(1))),
        );
        let store = Arc::new(FileSystemArtifactStore::new(dir.path()));
        let host = HostBinding::new("host", "workspace");
        let history = SessionHistory::new(journal.clone(), store.clone(), host.clone());
        let (mut active, key) = conversation();
        let attempt = AgentId::new();
        history.begin("root-a", &attempt).unwrap();
        history
            .record_turn(&attempt, &AgentId::root(), 1, &[], "EXPIRED_PUBLIC_MARKER")
            .await
            .unwrap();
        history
            .offer(
                "root-a",
                &attempt,
                "original",
                VisitReport {
                    report: None,
                    unstructured: true,
                    attempt: Some(attempt.clone()),
                    expires_at_ms: None,
                },
            )
            .unwrap();
        assert!(history.commit_link(key.clone(), attempt).await.is_err());
        let reopened = SessionHistory::new(journal, store, host);
        assert!(matches!(
            reopened.resolve(&key, &active, true).await,
            VisitRead::Expired
        ));
        active.turns[0].visit_deadlines.insert(key.result, i64::MAX);
        reopened.load_visit_deadlines(&mut active, true).await;
        assert!(active.turns[0].visit_deadlines.is_empty());
    }

    #[tokio::test]
    async fn oversized_journal_fails_closed_before_loading_history() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(NodeJournal::open_workspace(dir.path()).await.unwrap());
        let history = SessionHistory::new(
            journal.clone(),
            Arc::new(FileSystemArtifactStore::new(dir.path())),
            HostBinding::new("host", "workspace"),
        );
        let (mut active, key) = conversation();
        let original_main = serde_json::to_vec(&active).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(journal.path())
            .unwrap()
            .set_len(MAX_VISIT_JOURNAL_BYTES as u64 + 1)
            .unwrap();
        assert!(matches!(
            history.resolve(&key, &active, true).await,
            VisitRead::Missing
        ));
        active.turns[0].visit_deadlines.insert(key.result, i64::MAX);
        history.load_visit_deadlines(&mut active, true).await;
        assert!(active.turns[0].visit_deadlines.is_empty());
        assert_eq!(serde_json::to_vec(&active).unwrap(), original_main);
    }

    #[tokio::test]
    async fn exact_store_binding_and_oversized_link_body_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let foreign_dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(NodeJournal::open_workspace(dir.path()).await.unwrap());
        let store = Arc::new(FileSystemArtifactStore::new(dir.path()));
        let host = HostBinding::new("host", "workspace");
        let history = SessionHistory::new(journal.clone(), store, host.clone());
        let (active, key) = conversation();
        let attempt = AgentId::new();
        history.begin("root-a", &attempt).unwrap();
        history
            .record_turn(&attempt, &AgentId::root(), 1, &[], "PUBLIC_ONLY_MARKER")
            .await
            .unwrap();
        history
            .offer(
                "root-a",
                &attempt,
                "original",
                VisitReport {
                    report: None,
                    unstructured: true,
                    attempt: Some(attempt.clone()),
                    expires_at_ms: None,
                },
            )
            .unwrap();
        history.commit_link(key.clone(), attempt).await.unwrap();
        let misbound = SessionHistory::new(
            journal.clone(),
            Arc::new(FileSystemArtifactStore::new(foreign_dir.path())),
            host,
        );
        assert!(matches!(
            misbound.resolve(&key, &active, true).await,
            VisitRead::Missing
        ));
        let artifact = journal
            .load()
            .await
            .unwrap()
            .into_iter()
            .find_map(|entry| match entry.record {
                JournalRecord::Room(RoomEvent::VisitLinked { artifact, .. }) => Some(artifact),
                _ => None,
            })
            .unwrap();
        let path = history.artifact_directory(&artifact).unwrap().join("body");
        let original = std::fs::read(&path).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(MAX_ARTIFACT_BYTES as u64 + 1)
            .unwrap();
        assert!(matches!(
            history.resolve(&key, &active, true).await,
            VisitRead::Changed
        ));
        std::fs::write(&path, original).unwrap();
        let metadata_path = history
            .artifact_directory(&artifact)
            .unwrap()
            .join("meta.json");
        std::fs::OpenOptions::new()
            .write(true)
            .open(metadata_path)
            .unwrap()
            .set_len(MAX_METADATA_BYTES as u64 + 1)
            .unwrap();
        assert!(matches!(
            history.resolve(&key, &active, true).await,
            VisitRead::Changed
        ));
    }

    #[test]
    fn legacy_link_without_partial_flag_still_decodes() {
        let (_, key) = conversation();
        let attempt = AgentId::new();
        let legacy = serde_json::json!({
            "attempt": attempt,
            "key": key,
            "report": { "report": null, "unstructured": true, "attempt": attempt }
        });
        let link: LinkArtifact = serde_json::from_value(legacy).unwrap();
        assert!(!link.partial);
        assert_eq!(link.version, 0);
        assert!(link.authenticates(&key, &attempt));
    }

    #[test]
    fn artifact_versions_and_legacy_authorization_are_explicit() {
        let (_, key) = conversation();
        let attempt = AgentId::new();
        let legacy = serde_json::json!({
            "attempt": attempt, "key": key,
            "report": { "attempt": attempt, "report": "ignored historic prose" }
        });
        let link: LinkArtifact = serde_json::from_value(legacy.clone()).unwrap();
        assert!(link.authenticates(&key, &attempt));
        let encoded = serde_json::to_value(link).unwrap();
        assert!(encoded["report"].get("report").is_none());
        for version in [ARTIFACT_VERSION, ARTIFACT_VERSION + 1] {
            let mut value = legacy.clone();
            value["version"] = version.into();
            let link: LinkArtifact = serde_json::from_value(value).unwrap();
            assert!(!link.authenticates(&key, &attempt));
        }
        let link = LinkArtifact {
            version: ARTIFACT_VERSION,
            attempt: attempt.clone(),
            key: key.clone(),
            report: None,
            partial: false,
        };
        assert!(link.authenticates(&key, &attempt));
        let mut unknown = link;
        unknown.version += 1;
        assert!(!unknown.authenticates(&key, &attempt));
    }

    #[test]
    fn retention_rejects_overflow_future_and_expired_relations() {
        assert_eq!(SessionHistory::deadline(i64::MAX), None);
        assert_eq!(
            SessionHistory::deadline(SessionHistory::now_ms().unwrap() + 60_000),
            None
        );
        assert!(SessionHistory::check_expiry(Some(i64::MAX)).is_some());
        let backing = VisitBacking {
            files: Vec::new(),
            relation_deadlines: vec![1],
        };
        assert_eq!(
            SessionHistory::check_backing(&backing),
            Some(VisitRead::Changed)
        );
    }

    #[tokio::test]
    async fn final_turn_slot_is_a_durable_cap_marker() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(NodeJournal::open_workspace(dir.path()).await.unwrap());
        let history = SessionHistory::new(
            journal,
            Arc::new(FileSystemArtifactStore::new(dir.path())),
            HostBinding::new("host", "workspace"),
        );
        let (active, key) = conversation();
        let attempt = AgentId::new();
        history.begin("root-a", &attempt).unwrap();
        history
            .record_turn(&attempt, &AgentId::root(), 1, &[], "kept")
            .await
            .unwrap();
        history
            .record_turn(
                &attempt,
                &AgentId::root(),
                CAP_ORDINAL,
                &["omitted".into()],
                "omitted",
            )
            .await
            .unwrap();
        assert!(matches!(
            history
                .record_turn(&attempt, &AgentId::root(), MAX_TURNS + 1, &[], "omitted")
                .await,
            Err(VisitError::Capacity)
        ));
        history
            .offer(
                "root-a",
                &attempt,
                "original",
                VisitReport {
                    report: None,
                    unstructured: true,
                    attempt: Some(attempt.clone()),
                    expires_at_ms: None,
                },
            )
            .unwrap();
        history.commit_link(key.clone(), attempt).await.unwrap();
        assert!(
            matches!(history.resolve(&key, &active, true).await, VisitRead::Partial(body) if body.turns.len() == 1 && body.turns[0].public_text == "kept" && body.report.is_none())
        );
    }

    #[tokio::test]
    async fn bounded_read_lease_uses_the_opened_file_revision() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("body");
        std::fs::write(&path, b"bounded").unwrap();
        let mut backing = VisitBacking::default();
        assert_eq!(
            SessionHistory::read_limited(&path, 7, RevisionKind::Artifact, &mut backing)
                .await
                .unwrap(),
            b"bounded"
        );
        assert_eq!(SessionHistory::check_backing(&backing), None);
        // Concurrent visits can add siblings in any shared ancestor. Reopening
        // the same directory must use the same identity rule as lease checks.
        std::fs::write(dir.path().join("sibling"), b"unrelated").unwrap();
        std::fs::File::open(dir.path())
            .unwrap()
            .set_modified(UNIX_EPOCH)
            .unwrap();
        assert_eq!(
            SessionHistory::read_limited(&path, 7, RevisionKind::Artifact, &mut backing)
                .await
                .unwrap(),
            b"bounded"
        );
        assert_eq!(SessionHistory::check_backing(&backing), None);
        std::fs::write(&path, b"growing body").unwrap();
        assert_eq!(
            SessionHistory::check_backing(&backing),
            Some(VisitRead::Changed)
        );
        assert!(matches!(
            SessionHistory::read_limited(
                &path,
                7,
                RevisionKind::Artifact,
                &mut VisitBacking::default()
            )
            .await,
            Err(BodyError::Changed)
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bounded_read_distinguishes_absence_from_symlink_leaves_and_parents() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("retained");
        std::fs::create_dir(&parent).unwrap();
        let body = parent.join("body");
        std::fs::write(&body, b"retained").unwrap();
        for path in [parent.join("missing"), dir.path().join("missing/body")] {
            assert!(matches!(
                SessionHistory::read_limited(
                    &path,
                    8,
                    RevisionKind::Artifact,
                    &mut VisitBacking::default()
                )
                .await,
                Err(BodyError::Missing)
            ));
        }

        let leaf_alias = parent.join("alias");
        symlink(&body, &leaf_alias).unwrap();
        let parent_alias = dir.path().join("alias");
        symlink(&parent, &parent_alias).unwrap();
        for path in [leaf_alias, parent_alias.join("body")] {
            assert!(matches!(
                SessionHistory::read_limited(
                    &path,
                    8,
                    RevisionKind::Artifact,
                    &mut VisitBacking::default()
                )
                .await,
                Err(BodyError::Changed)
            ));
        }

        let mut backing = VisitBacking::default();
        SessionHistory::read_limited(&body, 8, RevisionKind::Artifact, &mut backing)
            .await
            .unwrap();
        let moved = dir.path().join("moved");
        std::fs::rename(&parent, &moved).unwrap();
        symlink(&moved, &parent).unwrap();
        assert_eq!(
            SessionHistory::check_backing(&backing),
            Some(VisitRead::Changed)
        );
        assert!(matches!(
            SessionHistory::read_limited(&body, 8, RevisionKind::Artifact, &mut backing).await,
            Err(BodyError::Changed)
        ));
    }

    #[tokio::test]
    async fn unavailable_immediate_relation_does_not_scan_past_it() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(NodeJournal::open_workspace(dir.path()).await.unwrap());
        let history = SessionHistory::new(
            journal.clone(),
            Arc::new(FileSystemArtifactStore::new(dir.path())),
            HostBinding::new("host", "workspace"),
        );
        let mut active = Conversation {
            id: "root-a".into(),
            ..Default::default()
        };
        let mut keys = Vec::new();
        for _ in 0..4 {
            let (source, key) = conversation();
            active.turns.extend(source.turns);
            let attempt = AgentId::new();
            history.begin("root-a", &attempt).unwrap();
            history
                .record_turn(&attempt, &AgentId::root(), 1, &[], "kept")
                .await
                .unwrap();
            history
                .offer(
                    "root-a",
                    &attempt,
                    "original",
                    VisitReport {
                        report: None,
                        unstructured: true,
                        attempt: Some(attempt.clone()),
                        expires_at_ms: None,
                    },
                )
                .unwrap();
            history.commit_link(key.clone(), attempt).await.unwrap();
            keys.push(key);
        }
        let links: Vec<_> = journal
            .load()
            .await
            .unwrap()
            .into_iter()
            .filter_map(|entry| match entry.record {
                JournalRecord::Room(RoomEvent::VisitLinked { artifact, .. }) => Some(artifact),
                _ => None,
            })
            .collect();
        std::fs::remove_file(history.artifact_directory(&links[2]).unwrap().join("body")).unwrap();
        let VisitRead::Available(body) = history.resolve(&keys[1], &active, true).await else {
            panic!("selected visit unavailable")
        };
        assert_eq!(body.previous, Some(keys[0].clone()));
        assert_eq!(body.next, None);
        assert_eq!(SessionHistory::check_backing(&body.backing), None);
        // A retained relation's backing participates in the selected lease.
        std::fs::remove_file(history.artifact_directory(&links[0]).unwrap().join("body")).unwrap();
        assert_eq!(
            SessionHistory::check_backing(&body.backing),
            Some(VisitRead::Changed)
        );
    }

    #[tokio::test]
    async fn visit_admission_bounds_preserve_ordinary_journal_and_live_slots() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(NodeJournal::open_workspace(dir.path()).await.unwrap());
        let history = SessionHistory::new(
            journal.clone(),
            Arc::new(FileSystemArtifactStore::new(dir.path())),
            HostBinding::new("host", "workspace"),
        );
        let attempt_at = |index| AgentId::try_from(format!("rh1-{index:08}")).unwrap();
        let mut attempts = Vec::with_capacity(MAX_LIVE);
        for i in 0..MAX_LIVE {
            let attempt = attempt_at(i);
            history.begin("root-a", &attempt).unwrap();
            attempts.push(attempt);
        }
        let behind_limit = attempt_at(MAX_LIVE);
        assert!(matches!(history.begin("root-a", &behind_limit), Err(VisitError::Capacity)));

        fn offer(history: &SessionHistory, attempt: &AgentId) -> Result<(), VisitError> {
            history.offer("root-a", attempt, "original", VisitReport {
                report: None, unstructured: true, attempt: Some(attempt.clone()), expires_at_ms: None,
            })
        }

        for attempt in &attempts {
            offer(&history, attempt).unwrap();
        }
        history.forget(&attempts[0]);
        history.begin("root-a", &behind_limit).unwrap();
        assert!(matches!(offer(&history, &behind_limit), Err(VisitError::Capacity)));
        history.discard_offers("root-a");

        for i in MAX_LIVE + 1..MAX_ATTEMPTS {
            let attempt = attempt_at(i);
            history.begin("root-a", &attempt).unwrap();
            history.forget(&attempt);
        }
        assert!(matches!(history.begin("root-a", &attempt_at(MAX_ATTEMPTS)), Err(VisitError::Capacity)));
        assert!(journal.load().await.unwrap().is_empty());
        let row = journal.append_alias(behind_limit, "ordinary".into()).await.unwrap();
        assert_eq!(row.seq, 1);
    }
}
