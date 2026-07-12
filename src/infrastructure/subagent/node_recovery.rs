//! Crash recovery fold for durable node checkpoints.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::domain::models::{
    AgentId, CorrelationId, HostBinding, JournalRecord, NodeCheckpoint, NodeState, RoomEvent,
};
use crate::infrastructure::subagent::{NodeJournal, NodeTree, RecoveryError};
pub fn current_host_id(_workspace: &Path) -> String {
    if let Some(value) = crate::infrastructure::utils::env_var_trimmed("RUSTAIN_HOST_ID") {
        return value;
    }
    if let Ok(value) = std::fs::read_to_string("/etc/machine-id") {
        let value = value.trim();
        if !value.is_empty() {
            return value.to_string();
        }
    }
    if let Some(value) = crate::infrastructure::utils::env_var_trimmed("HOSTNAME") {
        return value;
    }
    // Last resort: a per-machine random id persisted under the user data dir.
    // A workspace-derived id would alias two DISTINCT machines that mount the
    // same workspace, letting a foreign `Running` be recovered as local and
    // given a fabricated live handle (violates ADR-17-CC-03 host-bound
    // honesty). A machine-global persisted id never collides across hosts.
    persisted_machine_host_id()
}

fn persisted_machine_host_id() -> String {
    let Ok(dir) = crate::infrastructure::paths::data_dir() else {
        // Data dir unavailable: a process-unique id is still safer than a
        // workspace-derived one (it can never alias a foreign host).
        return format!("host-{}", nanoid::nanoid!(12));
    };
    let path = dir.join("host-id");
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let existing = existing.trim();
        if !existing.is_empty() {
            return existing.to_string();
        }
    }
    let id = format!("host-{}", nanoid::nanoid!(12));
    let _ = std::fs::write(&path, &id);
    id
}

pub fn current_host_binding(workspace: &Path) -> HostBinding {
    HostBinding::new(
        current_host_id(workspace),
        crate::infrastructure::paths::workspace_hash(workspace),
    )
}

/// OS-backed ownership proof held for the daemon's entire foreground lifetime.
/// Recovery accepts this value by reference, so it cannot run before singleton
/// acquisition or race a second live daemon.
pub struct DaemonSingletonLock {
    file: std::fs::File,
    path: PathBuf,
}

impl DaemonSingletonLock {
    pub async fn try_acquire(workspace: &Path) -> Result<Self, RecoveryError> {
        let path = workspace.join(".rustain").join("daemon.lock");
        tokio::task::spawn_blocking(move || acquire_lock(path))
            .await
            .map_err(|error| {
                RecoveryError::Restore(format!("singleton lock task failed: {error}"))
            })?
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(unix)]
fn acquire_lock(path: PathBuf) -> Result<DaemonSingletonLock, RecoveryError> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(RecoveryError::LockIo)?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(&path)
        .map_err(RecoveryError::LockIo)?;
    // SAFETY: `file` owns a valid descriptor for the lifetime of the guard.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::WouldBlock {
            return Err(RecoveryError::SingletonBusy);
        }
        return Err(RecoveryError::LockIo(error));
    }
    Ok(DaemonSingletonLock { file, path })
}

#[cfg(not(unix))]
fn acquire_lock(path: PathBuf) -> Result<DaemonSingletonLock, RecoveryError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(RecoveryError::LockIo)?;
    }
    let file = std::fs::OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                RecoveryError::SingletonBusy
            } else {
                RecoveryError::LockIo(error)
            }
        })?;
    Ok(DaemonSingletonLock { file, path })
}

impl Drop for DaemonSingletonLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            // SAFETY: the descriptor remains valid until this drop completes.
            unsafe {
                libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
            }
        }
        #[cfg(not(unix))]
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    pub restored: Vec<AgentId>,
    pub suspended: Vec<AgentId>,
    pub failed: Vec<AgentId>,
    pub host_bound_unavailable: Vec<AgentId>,
    pub hazards: Vec<AgentId>,
}

enum AliasLink {
    Bound {
        node: AgentId,
        alias: String,
    },
    Successor {
        predecessor: AgentId,
        successor: AgentId,
        alias: String,
    },
}

pub struct NodeRecovery;

impl NodeRecovery {
    /// Reconcile the latest checkpoint for each node while the daemon singleton
    /// is held. A durable `Running` state becomes `Suspended` exactly once.
    pub async fn reconcile(
        journal: &Arc<NodeJournal>,
        tree: &NodeTree,
        _singleton: &DaemonSingletonLock,
        current_host_id: &str,
    ) -> Result<RecoveryReport, RecoveryError> {
        let entries = journal.load().await?;
        let mut checkpoints = BTreeMap::<AgentId, NodeCheckpoint>::new();
        let mut hosts = BTreeMap::<AgentId, HostBinding>::new();
        let mut unavailable = BTreeSet::<AgentId>::new();
        let mut alias_links = Vec::<AliasLink>::new();
        // Rebuild MustReport obligations from the durable record stream so a
        // crash before a node reached terminal does not lose them. pending =
        // accepted − discharged − violated.
        let mut accepted = BTreeMap::<AgentId, std::collections::HashSet<CorrelationId>>::new();
        let mut resolved = BTreeMap::<AgentId, std::collections::HashSet<CorrelationId>>::new();

        for entry in entries {
            match entry.record {
                JournalRecord::Checkpoint(checkpoint) => {
                    checkpoints.insert(checkpoint.id.clone(), checkpoint);
                }
                JournalRecord::Room(RoomEvent::NodeRegistered { node, host, .. }) => {
                    hosts.insert(node, host);
                }
                JournalRecord::Room(RoomEvent::HostBoundUnavailable { node, .. }) => {
                    unavailable.insert(node);
                }
                JournalRecord::Room(_) => {}
                JournalRecord::AliasBound { node, alias } => {
                    alias_links.push(AliasLink::Bound { node, alias });
                }
                JournalRecord::Successor {
                    predecessor,
                    successor,
                    alias,
                } => alias_links.push(AliasLink::Successor {
                    predecessor,
                    successor,
                    alias,
                }),
                JournalRecord::ObligationAccepted {
                    node,
                    correlation_id,
                } => {
                    accepted.entry(node).or_default().insert(correlation_id);
                }
                JournalRecord::ObligationDischarged {
                    node,
                    correlation_id,
                }
                | JournalRecord::ObligationViolation {
                    node,
                    correlation_id,
                } => {
                    resolved.entry(node).or_default().insert(correlation_id);
                }
                JournalRecord::HazardRaised { .. } => {}
                // `load()` flattens atomic batches into individual records, so
                // a `Batch` never reaches this fold; the arm is defensive.
                JournalRecord::Batch(_) => {}
            }
        }

        let mut ordered = checkpoints.into_values().collect::<Vec<_>>();
        ordered.sort_by(|left, right| {
            left.depth
                .cmp(&right.depth)
                .then_with(|| left.id.cmp(&right.id))
        });
        let mut report = RecoveryReport::default();

        for mut checkpoint in ordered {
            let node = checkpoint.id.clone();
            if let Some(host) = hosts
                .get(&node)
                .filter(|host| host.host_id != current_host_id)
                .cloned()
            {
                if !unavailable.contains(&node) {
                    let event = RoomEvent::HostBoundUnavailable {
                        node: node.clone(),
                        host,
                    };
                    journal.append_room(event.clone()).await?;
                    tree.emit_room_event(event);
                    report.host_bound_unavailable.push(node);
                }
                continue;
            }

            if checkpoint.state == NodeState::Running {
                checkpoint.state = NodeState::Suspended;
                let event = RoomEvent::NodeStateChanged {
                    node: node.clone(),
                    from: NodeState::Running,
                    to: NodeState::Suspended,
                };
                journal
                    .append_batch(vec![
                        JournalRecord::Checkpoint(checkpoint.clone()),
                        JournalRecord::Room(event.clone()),
                    ])
                    .await?;
                tree.emit_room_event(event);
                report.suspended.push(node.clone());
            } else if checkpoint.state == NodeState::Failed {
                report.failed.push(node.clone());
            }

            if tree
                .restore_checkpoint(checkpoint)
                .await
                .map_err(|error| RecoveryError::Restore(error.to_string()))?
            {
                report.restored.push(node);
            }
        }

        for link in alias_links {
            match link {
                AliasLink::Bound { node, alias } => {
                    tree.restore_alias_link(node, alias).await;
                }
                AliasLink::Successor {
                    predecessor,
                    successor,
                    alias,
                } => {
                    tree.restore_successor_link(predecessor, successor, alias)
                        .await;
                }
            }
        }

        for (node, accepted_ids) in accepted {
            let resolved_ids = resolved.remove(&node).unwrap_or_default();
            let pending = accepted_ids
                .into_iter()
                .filter(|id| !resolved_ids.contains(id))
                .collect::<Vec<_>>();
            tree.restore_pending_obligations(node, pending).await;
        }

        // A node that was `Waiting` across the crash keeps its persisted
        // `waiting_since`; evaluate dwell against the injected wall clock now so
        // a long-waiting node escalates immediately on restart (R5).
        report.hazards = tree
            .raise_due_hazards(crate::domain::models::WAITING_HAZARD_THRESHOLD_MS)
            .await;

        Ok(report)
    }
}
