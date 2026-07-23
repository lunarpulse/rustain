//! Crash recovery fold for durable node checkpoints.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::domain::models::{
    AgentId, CorrelationId, HostBinding, JournalRecord, NodeCheckpoint, NodeState, RoomEvent,
    WaveId, WaveOutcome,
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
    /// Waves that journaled `WaveStarted` before the crash but never reached
    /// `WaveCompleted`; recovery closes each with `WaveOutcome::Failed`.
    pub interrupted_waves: Vec<WaveId>,
    /// Story 17.2d-b (AC-b1/b3): fork-join spokes still durably parked at
    /// recovery time (`Parked − Unparked` fold). Consumed by
    /// `resume_fork_join_run` at the composition root — `reconcile` itself
    /// stays state-only (layering: `subagent` never calls the orchestrator).
    pub parked: Vec<RecoveredPark>,
}

/// Story 17.2d-b (AC-b1): one durably parked fork-join spoke recovered from
/// the journal — the relaunch plan (`spec`) plus its readiness edges.
/// `node` is the full nonce-qualified tree-node id (the wave nonce is
/// embedded losslessly in the identity).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveredPark {
    pub node: AgentId,
    pub producers: Vec<AgentId>,
    pub spec: crate::domain::models::orchestration::SpokeSpec,
    pub concurrency: usize,
}

/// Story 17.2d-b (AC-b1/b3): pure `Parked − Unparked` fold over a journal
/// entry stream (the `ObligationAccepted/Discharged` precedent). Latest
/// `Parked` per node wins; an `Unparked` clears it. Shared by `reconcile`
/// (populating `RecoveryReport.parked`) and the composition-root resume scan,
/// so both read the SAME recovered parked set.
pub fn fold_parked_records(
    entries: &[crate::domain::models::JournalEntry],
) -> std::collections::BTreeMap<AgentId, RecoveredPark> {
    let mut parked = std::collections::BTreeMap::<AgentId, RecoveredPark>::new();
    for entry in entries {
        match &entry.record {
            JournalRecord::Parked {
                node,
                producers,
                spec,
                concurrency,
            } => {
                parked.insert(
                    node.clone(),
                    RecoveredPark {
                        node: node.clone(),
                        producers: producers.clone(),
                        spec: spec.clone(),
                        concurrency: *concurrency,
                    },
                );
            }
            JournalRecord::Unparked { node } => {
                parked.remove(node);
            }
            _ => {}
        }
    }
    parked
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
        // 17.2d-b (AC-b1): durable fork-join parked set (`Parked − Unparked`).
        let mut parked_set = BTreeMap::<AgentId, RecoveredPark>::new();
        // Track in-flight waves so recovery can close any wave that started but
        // never completed (host-loss / crash mid-flight) — see the fold below.
        let mut started_waves = Vec::<WaveId>::new();
        let mut completed_waves = std::collections::HashSet::<WaveId>::new();

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
                JournalRecord::Room(RoomEvent::WaveStarted { wave, .. }) => {
                    if !started_waves.contains(&wave) {
                        started_waves.push(wave);
                    }
                }
                JournalRecord::Room(RoomEvent::WaveCompleted { wave, .. }) => {
                    completed_waves.insert(wave);
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
                // Durable fork-join park records fold into the recovered
                // parked set (`Parked − Unparked`); node STATE is recovered by
                // the checkpoint loop, which already restores the parked
                // `Suspended` node + its `wait_reason` side-table.
                JournalRecord::Parked {
                    node,
                    producers,
                    spec,
                    concurrency,
                } => {
                    parked_set.insert(
                        node.clone(),
                        RecoveredPark {
                            node,
                            producers,
                            spec,
                            concurrency,
                        },
                    );
                }
                JournalRecord::Unparked { node } => {
                    parked_set.remove(&node);
                }
                JournalRecord::ParkClaimed { .. } | JournalRecord::ParkClaimReleased { .. } => {}
                // Ledger conservation head is recovered separately by
                // `AuthorityLedger::recover_from_journal` (17-2c D4); this fold
                // rebuilds node/room state only.
                JournalRecord::LedgerConservation(_) => {}
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
                let unavailable_event =
                    (!unavailable.contains(&node)).then(|| RoomEvent::HostBoundUnavailable {
                        node: node.clone(),
                        host,
                    });
                if checkpoint.state == NodeState::Running || checkpoint.state == NodeState::Waiting
                {
                    // 17.5b (AC4): a `Waiting` node is a phantom across
                    // restart — recover to exactly `Suspended`. C3: capture
                    // the hazard against the pre-fold `Waiting` checkpoint
                    // BEFORE rewriting the state, or restart-time escalation
                    // is silently lost (`waiting_hazard()` gates on `Waiting`).
                    if checkpoint.state == NodeState::Waiting
                        && tree
                            .raise_hazard_for_checkpoint(
                                &checkpoint,
                                crate::domain::models::WAITING_HAZARD_THRESHOLD_MS,
                            )
                            .await
                    {
                        report.hazards.push(node.clone());
                    }
                    let folded_from = checkpoint.state;
                    checkpoint.state = NodeState::Suspended;
                    let state_event = RoomEvent::NodeStateChanged {
                        node: node.clone(),
                        from: folded_from,
                        to: NodeState::Suspended,
                    };
                    let mut records = vec![
                        JournalRecord::Checkpoint(checkpoint),
                        JournalRecord::Room(state_event.clone()),
                    ];
                    if let Some(event) = &unavailable_event {
                        records.push(JournalRecord::Room(event.clone()));
                    }
                    journal.append_batch(records).await?;
                    tree.emit_room_event(state_event);
                    report.suspended.push(node.clone());
                    if let Some(event) = unavailable_event {
                        tree.emit_room_event(event);
                        report.host_bound_unavailable.push(node);
                    }
                } else if let Some(event) = unavailable_event {
                    journal.append_room(event.clone()).await?;
                    tree.emit_room_event(event);
                    report.host_bound_unavailable.push(node);
                }
                continue;
            }

            if checkpoint.state == NodeState::Running || checkpoint.state == NodeState::Waiting {
                // 17.5b (AC4): fold a phantom `Waiting` node to `Suspended`
                // alongside `Running`. C3: capture the hazard against the
                // pre-fold checkpoint first.
                if checkpoint.state == NodeState::Waiting
                    && tree
                        .raise_hazard_for_checkpoint(
                            &checkpoint,
                            crate::domain::models::WAITING_HAZARD_THRESHOLD_MS,
                        )
                        .await
                {
                    report.hazards.push(node.clone());
                }
                let folded_from = checkpoint.state;
                checkpoint.state = NodeState::Suspended;
                let event = RoomEvent::NodeStateChanged {
                    node: node.clone(),
                    from: folded_from,
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

        // Incomplete-wave fold (RC-7): a wave that journaled `WaveStarted` but
        // never reached `WaveCompleted` was interrupted by the crash/host loss.
        // Close it by appending the EXISTING `RoomEvent::WaveCompleted` with a
        // `Failed` outcome — reusing the variant, adding no new `NodeState`
        // variant and no portable runtime state — so the in-flight nested wave
        // is visibly projected as interrupted instead of phantom in-progress
        // (outcome `None`). Idempotent: a re-run observes the appended
        // completion and skips.
        for wave in started_waves {
            if completed_waves.contains(&wave) {
                continue;
            }
            let event = RoomEvent::WaveCompleted {
                wave: wave.clone(),
                outcome: WaveOutcome::Failed,
            };
            journal.append_room(event.clone()).await?;
            tree.emit_room_event(event);
            report.interrupted_waves.push(wave);
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

        // 17.2d-b (AC-b1/b3): expose the durable parked set (`Parked −
        // Unparked`) for the composition-root resume consumer. `reconcile`
        // stays state-only — it never dispatches (layering).
        report.parked = parked_set.into_values().collect();

        // 17.5b (C3): `Waiting` hazards were captured against the pre-fold
        // checkpoint above (folding to `Suspended` first would make
        // `waiting_hazard()` return `None` and silently delete restart-time
        // escalation). This residual pass catches any non-folded `Waiting`
        // node restored to the tree and merges — it does NOT replace the
        // captured set.
        let residual = tree
            .raise_due_hazards(crate::domain::models::WAITING_HAZARD_THRESHOLD_MS)
            .await;
        for id in residual {
            if !report.hazards.contains(&id) {
                report.hazards.push(id);
            }
        }

        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::{JournalEntry, JournalRecord};

    fn parked_spec(
        id: &AgentId,
        waits_for: Vec<AgentId>,
    ) -> crate::domain::models::orchestration::SpokeSpec {
        crate::domain::models::orchestration::SpokeSpec {
            id: id.clone(),
            label: "spoke".into(),
            prompt: "p".into(),
            effective_model: "m".into(),
            tier: crate::domain::models::ModelTier::Flagship,
            tools_allow: crate::domain::models::ToolPolicy::InheritFromParent,
            waits_for,
            role: crate::domain::models::orchestration::SpokeRole::Leaf,
        }
    }

    /// Story 17.2d-b AC-b1/b3: the recovered parked set is `Parked − Unparked`
    /// (latest-wins, idempotent replay) — the ObligationAccepted/Discharged
    /// precedent.
    #[test]
    fn parked_fold_is_set_difference_and_idempotent() {
        let node_a = AgentId::new();
        let node_b = AgentId::new();
        let spec_a = parked_spec(&AgentId::new(), vec![AgentId::new()]);
        let spec_b = parked_spec(&AgentId::new(), vec![]);
        let entries = vec![
            JournalEntry::new(
                1,
                JournalRecord::Parked {
                    node: node_a.clone(),
                    producers: vec![AgentId::new()],
                    spec: spec_a.clone(),
                    concurrency: 2,
                },
            ),
            JournalEntry::new(
                2,
                JournalRecord::Parked {
                    node: node_b.clone(),
                    producers: vec![],
                    spec: spec_b.clone(),
                    concurrency: 1,
                },
            ),
            JournalEntry::new(
                3,
                JournalRecord::Unparked {
                    node: node_a.clone(),
                },
            ),
            // Duplicate Unparked is a no-op (idempotent replay).
            JournalEntry::new(
                4,
                JournalRecord::Unparked {
                    node: node_a.clone(),
                },
            ),
        ];
        let folded = fold_parked_records(&entries);
        assert!(!folded.contains_key(&node_a), "unparked node folds out");
        assert_eq!(folded.len(), 1);
        assert_eq!(folded[&node_b].spec, spec_b);
        // Replaying the same stream yields the identical set.
        let replayed = fold_parked_records(&entries);
        assert_eq!(folded, replayed);
    }
}
