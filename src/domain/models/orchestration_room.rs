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

/// Durable outcome of an operator ticket. This is projected on the producing
/// node so a terminal task retains the human-visible reason that closed its
/// ticket (AC7), rather than collapsing every outcome into a generic node
/// state.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "outcome")]
pub enum TicketResolution {
    Answered,
    Cancelled,
    CancelUnconfirmed { reason: String },
    ExpiredUnanswered { reason: String },
    Failed { reason: String },
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

/// Which way an A2A interaction crossed this instance's boundary.
///
/// **Why this is persisted rather than derived.** For
/// [`RoomEvent::RemoteEnvelopeAccepted`] the node id still carries the
/// direction as a prefix (`a2a-in/` inbound vs `a2a/` outbound), but
/// [`RoomEvent::RemoteEnvelopeRejected`] carries `{ peer, reason }` and **no
/// node** — the direction is destroyed at the emit boundary and there is
/// nothing left to derive from at replay time. The field is therefore load
/// bearing, not stylistic: do not "optimise" it away in favour of the node
/// prefix. Tests may use the prefix as a cross-check, never as the source.
///
/// `Unknown` is the serde default for missing fields in journals written before
/// Story 18.2, and serde's future-value fallback for direction strings this
/// build does not yet recognise. Both cases render as an explicit unknown
/// rather than a fabricated direction.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// A peer asked this instance to do something.
    Inbound,
    /// This instance asked a peer to do something.
    Outbound,
    /// Journaled before direction was recorded, or a future direction this
    /// build does not understand. Rendered explicitly; never silently coerced.
    #[default]
    #[serde(other)]
    Unknown,
}

impl Direction {
    /// Stable, monochrome-safe label. Paired with a glyph at every render
    /// surface (UX monochrome rule) — this is the text half.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Inbound => "inbound",
            Self::Outbound => "outbound",
            _ => "unknown",
        }
    }

    /// Glyph half of the monochrome pair.
    #[must_use]
    pub fn glyph(self) -> &'static str {
        match self {
            Self::Inbound => "←",
            Self::Outbound => "→",
            _ => "?",
        }
    }
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
        /// Story 18.2 (AC2). `#[serde(default)]` so pre-18.2 journals replay
        /// as [`Direction::Unknown`] instead of a fabricated direction.
        #[serde(default)]
        direction: Direction,
        /// Original remote task id, if the emitting protocol supplied one.
        /// `None` preserves replay of pre-18.2 journal entries.
        #[serde(default)]
        task: Option<String>,
    },
    /// Defined against 17.1a's `PeerId`; production emission is the sole
    /// 17.1b-gated room-event seam.
    RemoteEnvelopeRejected {
        peer: PeerId,
        reason: RejectReason,
        /// Story 18.2 (AC2). This variant carries no node, so direction is
        /// **unrecoverable** at replay time unless it is persisted here.
        #[serde(default)]
        direction: Direction,
        /// Original remote task id, if the emitting protocol supplied one.
        /// `None` preserves replay of pre-18.2 journal entries.
        #[serde(default)]
        task: Option<String>,
    },
    HostBoundUnavailable {
        node: AgentId,
        host: HostBinding,
    },
    /// An MCP long-running task was bound to a durable node (Story 17.5a).
    /// `task` is the server-chosen task id; identity is additionally
    /// recoverable from the node id itself (reversible mint), so this event
    /// is the room's display/audit record, not the source of truth.
    McpTaskBound {
        node: AgentId,
        server: String,
        task: String,
    },
    /// 17.5b — an MCP task filed a blocking input-request ticket to the
    /// operator (FR152's shape, adopted early per R-14). Carries the artifact
    /// handle produced for the elicitation. Ships WITHOUT a `to:` field (there
    /// is exactly one operator today; 18.3a adds `to: AgentPath`).
    ///
    /// **Replay contract (NFR70(d)):** when 18.3a adds `to:`, that field MUST
    /// be `#[serde(default)]` so a ticket journaled by 17.5b replays unchanged.
    TicketAssigned {
        node: AgentId,
        artifact: ArtifactId,
    },
    /// Resolve a previously assigned ticket. The artifact id is the stable
    /// idempotency key: duplicate replay must neither reopen the ticket nor
    /// overwrite its first durable outcome.
    TicketResolved {
        node: AgentId,
        artifact: ArtifactId,
        outcome: TicketResolution,
    },
    /// Story 18.3 (AC4) — FR95 "who saw what": content actually crossed to the
    /// peer. Distinct from the 18.2 status-query record, which only proves the
    /// peer ASKED. A `tasks/get` returning `working` discloses nothing and MUST
    /// NOT produce this event.
    PeerDisclosure {
        /// Authenticated remote principal that received the content.
        #[serde(default)]
        peer: Option<PeerId>,
        node: AgentId,
        /// Original remote task id when the protocol supplied one.
        #[serde(default)]
        task: Option<String>,
        /// Byte length of the content handed back. Never the content itself.
        #[serde(default)]
        disclosed_bytes: usize,
    },
    /// An `event` tag this build does not recognise.
    ///
    /// `RoomEvent` is `#[non_exhaustive]` and the journal is a durable
    /// forward-compatible format: a newer build may append a variant this one
    /// has never heard of. Without `#[serde(other)]` that line would fail
    /// `parse_entries` and take the **whole journal** with it. Catching it
    /// here is what lets UX-DR-ROOM-01 hold — an unrecognised record renders
    /// as an explicit unknown row instead of vanishing or failing the load.
    ///
    /// The payload is deliberately discarded: the journal is append-only and
    /// never rewritten, so nothing round-trips this variant back to disk.
    #[serde(other)]
    Unrecognized,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeView {
    pub id: AgentId,
    pub origin: NodeOrigin,
    pub state: NodeState,
    pub host: HostBinding,
    pub host_bound_unavailable: bool,
    pub last_remote_content: Option<ContentHash>,
    /// `(server, taskId)` when this node is a bound MCP task (17.5a).
    pub mcp_task: Option<(String, String)>,
    /// 17.5b — open input-request ticket(s) filed by this node to the
    /// operator. A `Waiting` MCP-task node carries at least one.
    pub open_tickets: Vec<ArtifactId>,
    /// Durable terminal outcomes keyed by the ticket artifact. Keeping the
    /// outcome on the node makes expiry and cancellation failures visible
    /// after replay and across restarts.
    pub resolved_tickets: BTreeMap<ArtifactId, TicketResolution>,
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
    /// Story 18.2 (AC2, P-3). Carried into the read model deliberately: a
    /// projection that drops a field it was just told to record is the exact
    /// drift this story exists to prevent.
    pub direction: Direction,
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
                        mcp_task: None,
                        open_tickets: Vec::new(),
                        resolved_tickets: BTreeMap::new(),
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
            RoomEvent::RemoteEnvelopeRejected {
                peer,
                reason,
                direction,
                ..
            } => {
                self.remote_rejections.push(RemoteRejectionView {
                    peer,
                    reason,
                    direction,
                });
            }
            RoomEvent::HostBoundUnavailable { node, host } => {
                if let Some(view) = self.nodes.get_mut(&node) {
                    view.host = host;
                    view.host_bound_unavailable = true;
                }
            }
            RoomEvent::McpTaskBound { node, server, task } => {
                if let Some(view) = self.nodes.get_mut(&node) {
                    view.mcp_task = Some((server, task));
                }
            }
            RoomEvent::TicketAssigned { node, artifact } => {
                if let Some(view) = self.nodes.get_mut(&node)
                    && !view.open_tickets.contains(&artifact)
                    && !view.resolved_tickets.contains_key(&artifact)
                {
                    view.open_tickets.push(artifact);
                }
            }
            RoomEvent::TicketResolved {
                node,
                artifact,
                outcome,
            } => {
                if let Some(view) = self.nodes.get_mut(&node) {
                    view.open_tickets.retain(|open| open != &artifact);
                    view.resolved_tickets.entry(artifact).or_insert(outcome);
                }
            }
            RoomEvent::PeerDisclosure { .. } => {}
            // The room read model has nothing to fold an unknown tag into.
            // The transparency projection renders it as an explicit unknown
            // row instead (UX-DR-ROOM-01); dropping it here is not a silent
            // loss, it is the absence of a node/wave/artifact to attach to.
            RoomEvent::Unrecognized => {}
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::{ArtifactId, ContentHash};

    fn sample_artifact_id() -> ArtifactId {
        ArtifactId::from(ContentHash::from_bytes([0x11; 32]))
    }

    #[test]
    fn direction_defaults_for_legacy_fields_and_absorbs_future_values() {
        assert_eq!(Direction::default(), Direction::Unknown);
        let future: Direction =
            serde_json::from_str(r#""sideways""#).expect("future direction parses");
        assert_eq!(future, Direction::Unknown);
    }

    /// NFR70(d) / AC3: replaying the journal twice yields the identical room,
    /// including the ticket. A `TicketAssigned` folded twice must not duplicate
    /// the artifact handle on the node's `open_tickets`.
    #[test]
    fn ticket_assigned_replays_idempotently() {
        let node = AgentId::from_validated("mcp/s-srv/t-task");
        let artifact = sample_artifact_id();
        let room_id = OrchestrationRoomId::new();
        let assigned = RoomEvent::TicketAssigned {
            node: node.clone(),
            artifact: artifact.clone(),
        };
        let events = vec![
            RoomEvent::NodeRegistered {
                node: node.clone(),
                origin: NodeOrigin::Remote,
                host: HostBinding::new("local", "h"),
            },
            assigned.clone(),
            assigned,
        ];
        let room = OrchestrationRoom::project(room_id, events);
        let view = room.nodes().get(&node).expect("node projected");
        assert_eq!(view.open_tickets, vec![artifact]);
    }

    #[test]
    fn ticket_resolution_closes_once_and_duplicate_assignment_cannot_reopen_it() {
        let node = AgentId::from_validated("mcp/s-srv/t-task");
        let artifact = sample_artifact_id();
        let assigned = RoomEvent::TicketAssigned {
            node: node.clone(),
            artifact: artifact.clone(),
        };
        let resolved = RoomEvent::TicketResolved {
            node: node.clone(),
            artifact: artifact.clone(),
            outcome: TicketResolution::ExpiredUnanswered {
                reason: "remote task TTL expired".into(),
            },
        };
        let room = OrchestrationRoom::project(
            OrchestrationRoomId::new(),
            vec![
                RoomEvent::NodeRegistered {
                    node: node.clone(),
                    origin: NodeOrigin::Remote,
                    host: HostBinding::new("local", "h"),
                },
                assigned.clone(),
                resolved.clone(),
                resolved,
                assigned,
            ],
        );
        let view = room.nodes().get(&node).expect("node projected");
        assert!(view.open_tickets.is_empty());
        assert_eq!(
            view.resolved_tickets.get(&artifact),
            Some(&TicketResolution::ExpiredUnanswered {
                reason: "remote task TTL expired".into(),
            })
        );
    }

    /// NFR70(d): a `TicketAssigned` journaled by 17.5b (no `to:` field) must
    /// round-trip through serde unchanged. When 18.3a adds `to: AgentPath`
    /// `#[serde(default)]`, this byte-identical event must still deserialize.
    #[test]
    fn ticket_assigned_serializes_without_a_to_field() {
        let node = AgentId::from_validated("mcp/s-srv/t-task");
        let artifact = sample_artifact_id();
        let event = RoomEvent::TicketAssigned {
            node: node.clone(),
            artifact: artifact.clone(),
        };
        let json = serde_json::to_string(&event).expect("serialize");
        // The 17.5b wire shape carries `node` + `artifact` + the `event` tag,
        // and NO `to:` field. A future defaulted `to:` must not break this.
        assert!(
            json.contains("\"event\":\"ticket_assigned\""),
            "json: {json}"
        );
        assert!(
            !json.contains("\"to\""),
            "17.5b tickets carry no `to:`: {json}"
        );
        let back: RoomEvent = serde_json::from_str(&json).expect("deserialize round-trip");
        assert_eq!(event, back);
    }
}
