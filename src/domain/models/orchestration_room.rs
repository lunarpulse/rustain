//! Pure read-model projection for durable orchestration rooms.
//!
//! `RoomEvent` is the canonical durable event. `OrchestrationRoom::project`
//! performs no I/O and exposes no mutation surface to observers.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::domain::models::NodeOrigin;
use crate::domain::models::agent_id::AgentId;
use crate::domain::models::approval::ApprovalOutcome;
use crate::domain::models::artifact::{
    ArtifactId, ArtifactKind, ArtifactRef, ContentHash, ReviewStatus,
};
use crate::domain::models::invocation_fingerprint::InvocationFingerprint;
use crate::domain::models::node_state::NodeState;
use crate::domain::models::peer_identity::PeerId;
use crate::domain::models::tool_call::ApprovalSource;

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OrchestrationRoomId(String);

impl OrchestrationRoomId {
    pub fn new() -> Self {
        Self(nanoid::nanoid!(12))
    }

    pub fn parse(value: impl Into<String>) -> Result<Self, RoomIdError> {
        let value = value.into();
        validate_id(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RoomIdError {
    #[error("room or wave id must not be empty")]
    Empty,
    #[error("room or wave id must not contain '/'")]
    EmbeddedSeparator,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WaveId(String);

impl WaveId {
    pub fn new() -> Self {
        Self(nanoid::nanoid!(12))
    }

    pub fn parse(value: impl Into<String>) -> Result<Self, RoomIdError> {
        let value = value.into();
        validate_id(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct HostBinding {
    pub host_id: String,
    pub workspace_id: String,
}

impl HostBinding {
    pub fn new(host_id: impl Into<String>, workspace_id: impl Into<String>) -> Self {
        Self {
            host_id: host_id.into(),
            workspace_id: workspace_id.into(),
        }
    }
}

#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaveOutcome {
    Completed,
    Failed,
    Cancelled,
}

#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewVerdict {
    Approved,
    ChangesRequested,
    Rejected,
}

#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "reason")]
pub enum RejectReason {
    InvalidSignature,
    Expired,
    Replay,
    Policy { detail: String },
    UnknownRecipient,
    Malformed,
}

#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "event")]
pub enum RoomEvent {
    NodeRegistered {
        node: AgentId,
        origin: NodeOrigin,
        host: HostBinding,
    },
    NodeStateChanged {
        node: AgentId,
        from: NodeState,
        to: NodeState,
    },
    WaveStarted {
        wave: WaveId,
        coordinator: AgentId,
        spokes: Vec<AgentId>,
    },
    WaveCompleted {
        wave: WaveId,
        outcome: WaveOutcome,
    },
    /// Host-local admission refusal/defer. This event is durable for operator
    /// observability but is never carried by a signed peer envelope.
    AdmissionDeferred {
        coordinator: AgentId,
        spoke: String,
        gate: String,
    },
    ArtifactCreated {
        artifact: ArtifactRef,
    },
    ApprovalRequested {
        node: AgentId,
        fingerprint: InvocationFingerprint,
    },
    ApprovalResolved {
        node: AgentId,
        fingerprint: InvocationFingerprint,
        source: ApprovalSource,
        outcome: ApprovalOutcome,
    },
    PatchCaptured {
        artifact: ArtifactId,
        producer: AgentId,
    },
    PatchReviewed {
        artifact: ArtifactId,
        reviewer: AgentId,
        verdict: ReviewVerdict,
    },
    /// Defined against 17.1a's `PeerId`; production emission is the sole
    /// 17.1b-gated room-event seam.
    RemoteEnvelopeAccepted {
        peer: PeerId,
        node: AgentId,
        content_hash: ContentHash,
    },
    /// Defined against 17.1a's `PeerId`; production emission is the sole
    /// 17.1b-gated room-event seam.
    RemoteEnvelopeRejected {
        peer: PeerId,
        reason: RejectReason,
    },
    HostBoundUnavailable {
        node: AgentId,
        host: HostBinding,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeView {
    pub id: AgentId,
    pub origin: NodeOrigin,
    pub state: NodeState,
    pub host: HostBinding,
    pub host_bound_unavailable: bool,
    pub last_remote_content: Option<ContentHash>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WaveView {
    pub id: WaveId,
    pub coordinator: AgentId,
    pub spokes: Vec<AgentId>,
    pub outcome: Option<WaveOutcome>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApprovalView {
    pub node: AgentId,
    pub fingerprint: InvocationFingerprint,
    pub source: Option<ApprovalSource>,
    pub outcome: Option<ApprovalOutcome>,
}

/// A durable record of a refused remote envelope (Story 17.4b, Ruling 6). Kept
/// so a rejection is inspectable in the room, not just durable-but-invisible.
/// `RemoteEnvelopeRejected` carries no node, so this is room-scoped rather than
/// projected onto a `NodeView`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteRejectionView {
    pub peer: PeerId,
    pub reason: RejectReason,
}

/// Immutable read model reconstructed from the canonical event stream.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OrchestrationRoom {
    id: OrchestrationRoomId,
    nodes: BTreeMap<AgentId, NodeView>,
    waves: Vec<WaveView>,
    artifacts: BTreeMap<ArtifactId, ArtifactRef>,
    approvals: Vec<ApprovalView>,
    remote_rejections: Vec<RemoteRejectionView>,
}

impl OrchestrationRoom {
    /// Fold an ordered stream into an immutable room projection.
    pub fn project(id: OrchestrationRoomId, events: impl IntoIterator<Item = RoomEvent>) -> Self {
        let mut room = Self {
            id,
            ..Self::default()
        };

        for event in events {
            room.apply(event);
        }
        room
    }

    /// Host-honest projection: fold, then derive each node's availability from
    /// its recorded host binding vs the current host. A node whose binding
    /// matches the current host is available even if a prior foreign-host
    /// replay left a persisted `HostBoundUnavailable` marker (the marker never
    /// sticks after the node returns home); a foreign binding renders
    /// unavailable with no live handle fabricated (ADR-17-CC-03).
    pub fn project_for_host(
        id: OrchestrationRoomId,
        events: impl IntoIterator<Item = RoomEvent>,
        current_host_id: &str,
    ) -> Self {
        let mut room = Self::project(id, events);
        for view in room.nodes.values_mut() {
            view.host_bound_unavailable = view.host.host_id != current_host_id;
        }
        room
    }

    /// Read-only accessors. The room is a projection, not a store: there is no
    /// public field and no `&mut` observer path to node/authority state (AC9).
    pub fn id(&self) -> &OrchestrationRoomId {
        &self.id
    }

    pub fn nodes(&self) -> &BTreeMap<AgentId, NodeView> {
        &self.nodes
    }

    pub fn waves(&self) -> &[WaveView] {
        &self.waves
    }

    pub fn artifacts(&self) -> &BTreeMap<ArtifactId, ArtifactRef> {
        &self.artifacts
    }

    pub fn approvals(&self) -> &[ApprovalView] {
        &self.approvals
    }

    /// Refused remote envelopes, in arrival order (Story 17.4b).
    pub fn remote_rejections(&self) -> &[RemoteRejectionView] {
        &self.remote_rejections
    }

    fn apply(&mut self, event: RoomEvent) {
        match event {
            RoomEvent::NodeRegistered { node, origin, host } => {
                self.nodes.insert(
                    node.clone(),
                    NodeView {
                        id: node,
                        origin,
                        state: NodeState::Created,
                        host,
                        host_bound_unavailable: false,
                        last_remote_content: None,
                    },
                );
            }
            RoomEvent::NodeStateChanged { node, from, to } => {
                if let Some(view) = self.nodes.get_mut(&node)
                    && view.state == from
                {
                    view.state = to;
                }
            }
            RoomEvent::WaveStarted {
                wave,
                coordinator,
                spokes,
            } => self.waves.push(WaveView {
                id: wave,
                coordinator,
                spokes,
                outcome: None,
            }),
            RoomEvent::WaveCompleted { wave, outcome } => {
                if let Some(view) = self.waves.iter_mut().rev().find(|view| view.id == wave) {
                    view.outcome = Some(outcome);
                }
            }
            RoomEvent::AdmissionDeferred { .. } => {}
            RoomEvent::ArtifactCreated { artifact } => {
                self.artifacts.insert(artifact.id.clone(), artifact);
            }
            RoomEvent::ApprovalRequested { node, fingerprint } => {
                self.approvals.push(ApprovalView {
                    node,
                    fingerprint,
                    source: None,
                    outcome: None,
                });
            }
            RoomEvent::ApprovalResolved {
                node,
                fingerprint,
                source,
                outcome,
            } => {
                if let Some(view) = self.approvals.iter_mut().rev().find(|view| {
                    view.node == node && view.fingerprint == fingerprint && view.outcome.is_none()
                }) {
                    view.source = Some(source);
                    view.outcome = Some(outcome);
                }
            }
            RoomEvent::PatchCaptured { artifact, producer } => {
                if let Some(view) = self.artifacts.get_mut(&artifact)
                    && view.producer == producer
                {
                    view.kind = ArtifactKind::Patch;
                    view.review = Some(ReviewStatus::Pending);
                }
            }
            RoomEvent::PatchReviewed {
                artifact,
                reviewer,
                verdict,
            } => {
                if let Some(view) = self.artifacts.get_mut(&artifact) {
                    view.review = Some(ReviewStatus::Reviewed { reviewer, verdict });
                }
            }
            RoomEvent::RemoteEnvelopeAccepted {
                node, content_hash, ..
            } => {
                if let Some(view) = self.nodes.get_mut(&node) {
                    view.last_remote_content = Some(content_hash);
                }
            }
            RoomEvent::RemoteEnvelopeRejected { peer, reason } => {
                self.remote_rejections
                    .push(RemoteRejectionView { peer, reason });
            }
            RoomEvent::HostBoundUnavailable { node, host } => {
                if let Some(view) = self.nodes.get_mut(&node) {
                    view.host = host;
                    view.host_bound_unavailable = true;
                }
            }
        }
    }
}

fn validate_id(value: &str) -> Result<(), RoomIdError> {
    if value.is_empty() {
        return Err(RoomIdError::Empty);
    }
    if value.contains('/') {
        return Err(RoomIdError::EmbeddedSeparator);
    }
    Ok(())
}
