//! Dual-channel EventBus: mpsc for the event loop, broadcast for observers.
//!
//! # Invariant
//!
//! Never write to `domain_tx` directly from outside `emit_domain`. All event
//! production MUST go through `EventBus::emit_domain` so that the broadcast
//! tail stays in sync with the mpsc stream. Raw subscribers (daemon, wire log,
//! metrics) observe the same event order as the event loop.
//!
//! Consumers of `raw_tx` use `subscribe_raw()` + `tokio::time::timeout`:
//!
//! ```ignore
//! let mut rx = event_bus.subscribe_raw();
//! loop {
//!     match tokio::time::timeout(Duration::from_secs(30), rx.recv()).await {
//!         Ok(Ok(raw)) => handle(raw),
//!         Ok(Err(broadcast::error::RecvError::Lagged(n))) => {
//!             tracing::warn!(missed = n, "raw event subscriber lagged");
//!             continue;
//!         }
//!         Ok(Err(broadcast::error::RecvError::Closed)) => break,
//!         Err(_elapsed) => continue,
//!     }
//! }
//! ```

use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc};

use crate::domain::events::AppEvent;
use crate::domain::models::{NoticeLevel, PermissionMode, StreamChunk, ToolCallTransition};

/// `Deserialize` (Story 12.2b): `RawEvent` is the single `AppEvent → wire`
/// projection (`from_app_event`). The daemon attach protocol reuses it verbatim
/// as [`crate::adapters::daemon::protocol::ClientEvent`] (a type alias) so the
/// client can deserialize the same events the broadcast bus serializes — one
/// projection, one place (AC2). Every nested payload already derived
/// `Deserialize`; only `RawEvent`/`RawEventKind` + `ApprovalRuntimeEvent` +
/// `CapabilityEvent`/`RegisteredCapability` needed it added.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RawEvent {
    pub conversation_id: Option<String>,
    pub timestamp_ms: i64,
    pub kind: RawEventKind,
}

#[non_exhaustive]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum RawEventKind {
    Provider(StreamChunk),
    /// Same permitted live display, with host-owned peer-history lineage.
    PeerProvider(StreamChunk),
    PeerSystemNotice {
        level: NoticeLevel,
        message: String,
    },
    ModeChanged(PermissionMode),
    SystemNotice {
        level: NoticeLevel,
        message: String,
    },
    Tool(ToolCallTransition),
    /// Durable-room event or transparency journal failure forwarded to attached
    /// clients. Unlike `AppEvent`, this payload has no runtime-only handles and
    /// is safe to serialize on the daemon IPC wire.
    DomainEvent(crate::domain::events::DomainEventPayload),
    Approval(crate::domain::services::approval_runtime::ApprovalRuntimeEvent),
    /// Story 8.1 AC-10 — config reload result for daemon/telemetry subscribers.
    ConfigReloaded {
        success: bool,
        error: Option<String>,
    },
    /// Story 9.1 — MCP server connection state changed.
    McpConnectionStateChanged {
        server_id: String,
        state: crate::domain::models::McpConnectionState,
        source_profile: Option<String>,
    },
    /// Story 9.2 — MCP tool catalog changed for a server.
    McpCatalogChanged {
        server_id: String,
        tool_count: usize,
    },
    /// Story 17.4a — A2A AgentCard inventory changed for one peer.
    #[cfg(feature = "a2a")]
    A2aCatalogChanged {
        peer_id: String,
        skill_count: usize,
    },
    /// Story 9.3a — Capability lifecycle event from the CapabilityRegistry.
    Capability(crate::domain::events::CapabilityEvent),
    /// Story 14.4 — attributed child→parent subagent event.
    Subagent(crate::domain::models::SubagentEnvelope),
}

pub struct EventBus {
    pub domain_tx: mpsc::UnboundedSender<AppEvent>,
    pub raw_tx: broadcast::Sender<RawEvent>,
}

impl crate::domain::ports::EventEmitter for EventBus {
    fn emit(&self, event: AppEvent) {
        // Story 14-4a (F10): emit_domain now surfaces the domain-send result;
        // ignore it here (trait contract returns ()).
        let _ = self.emit_domain(event);
    }
}

/// [`EventEmitter`](crate::domain::ports::EventEmitter) over a bare domain
/// sender, for composition sites that hold the channel rather than the bus —
/// the daemon's A2A wiring passes `domain_tx` down, not `Arc<EventBus>`.
///
/// Lives in `event_bus.rs` deliberately: this file is the canonical channel
/// and the one place a direct `domain_tx.send` is sanctioned. Anywhere else it
/// would be an `emit_domain` bypass.
pub struct ChannelEmitter(mpsc::UnboundedSender<AppEvent>);

impl ChannelEmitter {
    #[must_use]
    pub fn new(domain_tx: mpsc::UnboundedSender<AppEvent>) -> Self {
        Self(domain_tx)
    }
}

impl crate::domain::ports::EventEmitter for ChannelEmitter {
    fn emit(&self, event: AppEvent) {
        let _ = self.0.send(event);
    }
}

impl EventBus {
    pub fn new(raw_capacity: usize) -> (Self, mpsc::UnboundedReceiver<AppEvent>) {
        assert!(raw_capacity > 0, "raw_capacity must be > 0");
        let (domain_tx, domain_rx) = mpsc::unbounded_channel();
        let (raw_tx, _) = broadcast::channel(raw_capacity);
        (Self { domain_tx, raw_tx }, domain_rx)
    }

    #[allow(clippy::result_large_err)]
    pub fn emit_domain(&self, event: AppEvent) -> Result<(), mpsc::error::SendError<AppEvent>> {
        if let Some(raw) = RawEvent::from_app_event(&event) {
            let _ = self.raw_tx.send(raw);
        }
        // Story 14-4a (F10) — surface the domain-channel send result so that
        // receipt emitters can log at warn when the event-loop receiver is
        // gone. Without this, a sender silently believes its receipt landed
        // (the recipient never learns of a refusal). The internal trace log is
        // kept for continuity; callers that care branch on the Result.
        let result = self.domain_tx.send(event);
        if result.is_err() {
            tracing::trace!("event loop receiver dropped — emit_domain discarded");
        }
        result
    }

    #[allow(dead_code)]
    pub fn subscribe_raw(&self) -> broadcast::Receiver<RawEvent> {
        self.raw_tx.subscribe()
    }
}

/// Keep the retained transcript intact while projecting only public tools.
pub(crate) fn public_transcript_message(
    message: &crate::domain::models::ChatMessage,
) -> crate::domain::models::ChatMessage {
    crate::domain::models::ChatMessage {
        id: message.id.clone(),
        role: message.role,
        content: message.content.clone(),
        content_blocks: message.content_blocks.clone(),
        tool_calls: message
            .tool_calls
            .iter()
            .filter(|tool| {
                tool.name != crate::domain::models::team_proposal::TEAM_PROPOSAL_TOOL_NAME
            })
            .cloned()
            .collect(),
        created_at: message.created_at,
        token_count: message.token_count,
        history_source: message.history_source,
        stop_reason: message.stop_reason.clone(),
        synthetic: message.synthetic,
        images: message.images.clone(),
        origin: message.origin,
        authorship: message.authorship,
        retracted_at_ms: message.retracted_at_ms,
    }
}

impl RawEvent {
    pub fn from_app_event(ev: &AppEvent) -> Option<Self> {
        match ev {
            AppEvent::ProviderChunk { chunk, .. } | AppEvent::PeerProviderChunk { chunk, .. }
                if chunk.is_operator_private() =>
            {
                return None;
            }
            AppEvent::ToolCallTransitionBridged { transition, .. }
                if transition.call.request().tool_name
                    == crate::domain::models::team_proposal::TEAM_PROPOSAL_TOOL_NAME =>
            {
                return None;
            }
            _ => {}
        }
        let now = chrono::Utc::now().timestamp_millis();
        Some(match ev {
            AppEvent::ProviderChunk {
                conversation_id,
                chunk,
            } => RawEvent {
                conversation_id: Some(conversation_id.clone()),
                timestamp_ms: now,
                kind: RawEventKind::Provider(chunk.clone()),
            },
            AppEvent::PeerProviderChunk {
                conversation_id,
                chunk,
                ..
            } => RawEvent {
                conversation_id: Some(conversation_id.clone()),
                timestamp_ms: now,
                kind: RawEventKind::PeerProvider(chunk.clone()),
            },
            AppEvent::PeerTurnClosed {
                conversation_id, ..
            } => RawEvent {
                conversation_id: Some(conversation_id.clone()),
                timestamp_ms: now,
                kind: RawEventKind::PeerProvider(StreamChunk::TurnComplete {
                    stop_reason: crate::domain::models::StopReason::Cancelled,
                }),
            },
            AppEvent::PeerSystemNotice {
                conversation_id,
                level,
                message,
                ..
            } => RawEvent {
                conversation_id: conversation_id.clone(),
                timestamp_ms: now,
                kind: RawEventKind::PeerSystemNotice {
                    level: *level,
                    message: message.clone(),
                },
            },
            AppEvent::SetPermissionMode(mode) => RawEvent {
                conversation_id: None,
                timestamp_ms: now,
                kind: RawEventKind::ModeChanged(*mode),
            },
            AppEvent::SystemNotice {
                conversation_id,
                level,
                message,
            } => RawEvent {
                conversation_id: conversation_id.clone(),
                timestamp_ms: now,
                kind: RawEventKind::SystemNotice {
                    level: *level,
                    message: message.clone(),
                },
            },
            AppEvent::ToolCallTransitionBridged {
                conversation_id,
                transition,
            } => RawEvent {
                conversation_id: Some(conversation_id.clone()),
                timestamp_ms: now,
                kind: RawEventKind::Tool(transition.clone()),
            },
            AppEvent::ApprovalRuntimeEventBridged { .. } => return None,
            AppEvent::ConfigReloaded { success, error } => RawEvent {
                conversation_id: None,
                timestamp_ms: now,
                kind: RawEventKind::ConfigReloaded {
                    success: *success,
                    error: error.clone(),
                },
            },
            AppEvent::McpConnectionStateChanged {
                server_id,
                state,
                source_profile,
            } => RawEvent {
                conversation_id: None,
                timestamp_ms: now,
                kind: RawEventKind::McpConnectionStateChanged {
                    server_id: server_id.clone(),
                    state: state.clone(),
                    source_profile: source_profile.clone(),
                },
            },
            AppEvent::McpCatalogChanged {
                server_id,
                tool_count,
            } => RawEvent {
                conversation_id: None,
                timestamp_ms: now,
                kind: RawEventKind::McpCatalogChanged {
                    server_id: server_id.clone(),
                    tool_count: *tool_count,
                },
            },
            #[cfg(feature = "a2a")]
            AppEvent::A2aCatalogChanged {
                peer_id,
                skill_count,
            } => RawEvent {
                conversation_id: None,
                timestamp_ms: now,
                kind: RawEventKind::A2aCatalogChanged {
                    peer_id: peer_id.clone(),
                    skill_count: *skill_count,
                },
            },
            AppEvent::CapabilityEvent(event) => RawEvent {
                conversation_id: None,
                timestamp_ms: now,
                kind: RawEventKind::Capability(event.clone()),
            },
            AppEvent::Subagent(envelope) => RawEvent {
                conversation_id: None,
                timestamp_ms: now,
                kind: RawEventKind::Subagent(envelope.clone()),
            },
            AppEvent::DomainEvent(payload) => {
                if matches!(
                    payload,
                    crate::domain::events::DomainEventPayload::Room(
                        crate::domain::models::RoomEvent::TeamSendActionRecorded { .. }
                            | crate::domain::models::RoomEvent::TeamSendRecipientSettled { .. }
                    )
                ) {
                    return None;
                }
                let mut payload = payload.clone();
                if let crate::domain::events::DomainEventPayload::Room(
                    crate::domain::models::RoomEvent::RemoteEnvelopeDispatched {
                        action,
                        authorship,
                        ..
                    },
                ) = &mut payload
                {
                    *action = None;
                    *authorship = crate::domain::models::ReplyAuthorship::Unknown;
                }
                RawEvent {
                    conversation_id: None,
                    timestamp_ms: now,
                    kind: RawEventKind::DomainEvent(payload),
                }
            }
            #[cfg(feature = "a2a")]
            AppEvent::TeamProposalReady { .. } => return None,
            AppEvent::Tick
            | AppEvent::ConfigReload
            | AppEvent::Resize(..)
            | AppEvent::InputEvent(..) => return None,
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn cancellation_token_parent_cancels_child() {
        let parent = CancellationToken::new();
        let child = parent.child_token();
        assert!(!parent.is_cancelled());
        assert!(!child.is_cancelled());
        parent.cancel();
        assert!(parent.is_cancelled());
        assert!(child.is_cancelled());
    }

    #[test]
    fn cancellation_token_child_does_not_cancel_parent() {
        let parent = CancellationToken::new();
        let child = parent.child_token();
        child.cancel();
        assert!(!parent.is_cancelled());
        assert!(child.is_cancelled());
    }

    #[test]
    fn cancellation_token_siblings_independent() {
        let parent = CancellationToken::new();
        let child_a = parent.child_token();
        let child_b = parent.child_token();
        child_a.cancel();
        assert!(child_a.is_cancelled());
        assert!(!child_b.is_cancelled());
        assert!(!parent.is_cancelled());
    }

    #[tokio::test]
    async fn emit_domain_writes_both_channels() {
        let (bus, mut domain_rx) = EventBus::new(16);
        let mut raw_rx = bus.subscribe_raw();

        let _ = bus.emit_domain(AppEvent::SystemNotice {
            conversation_id: None,
            level: NoticeLevel::Info,
            message: "test".to_string(),
        });

        let ev = tokio::time::timeout(std::time::Duration::from_millis(100), domain_rx.recv())
            .await
            .expect("timed out")
            .expect("channel closed");
        assert!(matches!(ev, AppEvent::SystemNotice { .. }));

        let raw = tokio::time::timeout(std::time::Duration::from_millis(100), raw_rx.recv())
            .await
            .expect("timed out")
            .expect("channel closed");
        assert!(matches!(raw.kind, RawEventKind::SystemNotice { .. }));
    }

    #[tokio::test]
    async fn emit_domain_tick_not_broadcast() {
        let (bus, mut domain_rx) = EventBus::new(16);
        let mut raw_rx = bus.subscribe_raw();

        let _ = bus.emit_domain(AppEvent::Tick);

        let ev = tokio::time::timeout(std::time::Duration::from_millis(50), domain_rx.recv())
            .await
            .expect("timed out")
            .expect("channel closed");
        assert!(matches!(ev, AppEvent::Tick));

        let result =
            tokio::time::timeout(std::time::Duration::from_millis(50), raw_rx.recv()).await;
        assert!(result.is_err(), "Tick should not appear on raw channel");
    }

    #[tokio::test]
    async fn subscribe_raw_receives_from_tail() {
        let (bus, _) = EventBus::new(16);

        let notice = AppEvent::SystemNotice {
            conversation_id: None,
            level: NoticeLevel::Info,
            message: "a".to_string(),
        };
        let _ = bus.emit_domain(notice);
        let notice2 = AppEvent::SystemNotice {
            conversation_id: None,
            level: NoticeLevel::Info,
            message: "b".to_string(),
        };
        let _ = bus.emit_domain(notice2);

        let mut raw_rx = bus.subscribe_raw();

        let notice3 = AppEvent::SystemNotice {
            conversation_id: None,
            level: NoticeLevel::Info,
            message: "c".to_string(),
        };
        let _ = bus.emit_domain(notice3);

        let raw = tokio::time::timeout(std::time::Duration::from_millis(100), raw_rx.recv())
            .await
            .expect("timed out")
            .expect("channel closed");
        assert!(matches!(raw.kind, RawEventKind::SystemNotice { .. }));
    }

    #[test]
    fn from_app_event_provider_chunk() {
        let chunk = StreamChunk::Text {
            content: "hello".to_string(),
            parent_tool_use_id: None,
        };
        let ev = AppEvent::ProviderChunk {
            conversation_id: "conv-1".to_string(),
            chunk,
        };
        let raw = RawEvent::from_app_event(&ev).unwrap();
        assert_eq!(raw.conversation_id.as_deref(), Some("conv-1"));
        assert!(matches!(raw.kind, RawEventKind::Provider(_)));
    }

    #[test]
    fn from_app_event_set_permission_mode() {
        let ev = AppEvent::SetPermissionMode(PermissionMode::Normal);
        let raw = RawEvent::from_app_event(&ev).unwrap();
        assert!(raw.conversation_id.is_none());
        assert!(matches!(
            raw.kind,
            RawEventKind::ModeChanged(PermissionMode::Normal)
        ));
    }

    #[test]
    fn from_app_event_system_notice() {
        let ev = AppEvent::SystemNotice {
            conversation_id: Some("conv-2".to_string()),
            level: NoticeLevel::Warning,
            message: "disk low".to_string(),
        };
        let raw = RawEvent::from_app_event(&ev).unwrap();
        assert_eq!(raw.conversation_id.as_deref(), Some("conv-2"));
        match &raw.kind {
            RawEventKind::SystemNotice { level, message } => {
                assert_eq!(*level, NoticeLevel::Warning);
                assert_eq!(message, "disk low");
            }
            other => panic!("expected SystemNotice, got {:?}", other),
        }
    }

    #[test]
    fn domain_event_projects_to_serializable_raw_ipc() {
        let event = AppEvent::DomainEvent(
            crate::domain::events::DomainEventPayload::TransparencyJournalFailed {
                failures: 2,
                detail: "journal fsync failed".to_owned(),
            },
        );

        let raw = RawEvent::from_app_event(&event).expect("domain event is forwarded");
        let decoded: RawEvent =
            serde_json::from_str(&serde_json::to_string(&raw).expect("serialize raw event"))
                .expect("deserialize raw event");

        assert!(matches!(
            decoded.kind,
            RawEventKind::DomainEvent(
                crate::domain::events::DomainEventPayload::TransparencyJournalFailed {
                    failures: 2,
                    ref detail
                }
            ) if detail == "journal fsync failed"
        ));
    }

    #[test]
    fn from_app_event_tick_returns_none() {
        assert!(RawEvent::from_app_event(&AppEvent::Tick).is_none());
    }

    #[test]
    fn from_app_event_resize_returns_none() {
        assert!(RawEvent::from_app_event(&AppEvent::Resize(80, 24)).is_none());
    }

    #[test]
    fn from_app_event_unknown_returns_none() {
        let ev = AppEvent::ToolResult(crate::domain::events::ToolResultEvent {
            result: crate::domain::models::ToolResult {
                tool_use_id: String::new(),
                content: String::new(),
                is_error: false,
            },
        });
        assert!(RawEvent::from_app_event(&ev).is_none());
    }

    #[test]
    fn from_app_event_tool_call_transition() {
        use crate::domain::models::tool_call::{ToolCall, ToolCallRequest, ToolCallTransition};
        let transition = ToolCallTransition {
            conversation_id: "conv-3".to_string(),
            call: ToolCall::Validating {
                id: "tc-1".to_string(),
                request: ToolCallRequest {
                    id: "tc-1".to_string(),
                    tool_name: "Read".to_string(),
                    input: serde_json::json!({"file_path": "/tmp/x"}),
                },
                started_at: 1714000000,
            },
        };
        let ev = AppEvent::ToolCallTransitionBridged {
            conversation_id: "conv-3".to_string(),
            transition: transition.clone(),
        };
        let raw = RawEvent::from_app_event(&ev).unwrap();
        assert_eq!(raw.conversation_id.as_deref(), Some("conv-3"));
        assert!(matches!(raw.kind, RawEventKind::Tool(_)));
        if let RawEventKind::Tool(ref t) = raw.kind {
            assert_eq!(t.conversation_id, "conv-3");
            assert!(matches!(t.call, ToolCall::Validating { .. }));
        }
    }
    #[test]
    fn domain_dispatch_projection_preserves_legacy_fields_but_strips_action_reference() {
        let peer = crate::domain::models::PeerId::from_public_key(&[0x2a; 32]).unwrap();
        let action = crate::domain::models::SendActionId::from_replay("sa_private123");
        let event = AppEvent::DomainEvent(crate::domain::events::DomainEventPayload::Room(
            crate::domain::models::RoomEvent::RemoteEnvelopeDispatched {
                peer: peer.clone(),
                task: Some("legacy-task-1".to_owned()),
                bytes: 19,
                authorship: crate::domain::models::ReplyAuthorship::Unknown,
                act: crate::domain::models::DispatchAct::ItemRetract {
                    item: "peer-item-1".to_owned(),
                },
                content_hash: None,
                endpoint: Some("sha256:origin-digest".to_owned()),
                action: Some(crate::domain::models::SendActionRef { action, index: 3 }),
            },
        ));
        let raw = RawEvent::from_app_event(&event).expect("legacy dispatch remains projected");
        let RawEventKind::DomainEvent(crate::domain::events::DomainEventPayload::Room(
            crate::domain::models::RoomEvent::RemoteEnvelopeDispatched {
                peer: projected_peer,
                task,
                bytes,
                act,
                content_hash,
                endpoint,
                action,
                ..
            },
        )) = raw.kind
        else {
            panic!("expected legacy dispatch in raw event");
        };
        assert_eq!(projected_peer, peer);
        assert_eq!(task.as_deref(), Some("legacy-task-1"));
        assert_eq!(bytes, 19);
        assert_eq!(
            act,
            crate::domain::models::DispatchAct::ItemRetract {
                item: "peer-item-1".to_owned()
            }
        );
        assert_eq!(content_hash, None);
        assert_eq!(endpoint.as_deref(), Some("sha256:origin-digest"));
        assert_eq!(action, None, "sender-local action correlation is stripped");
    }

    #[tokio::test]
    async fn proposal_chunks_stay_on_domain_channel_only() {
        use crate::domain::models::{AgentId, WriteDiffState};
        let (bus, mut domain) = EventBus::new(16);
        let mut raw = bus.subscribe_raw();
        for chunk in [
            StreamChunk::ToolUse {
                id: "private-tool".into(),
                name: crate::domain::models::team_proposal::TEAM_PROPOSAL_TOOL_NAME.into(),
                input: serde_json::json!({"body":"secret","recipients":["private-alias"]}),
            },
            StreamChunk::ToolResult {
                id: "private-tool".into(),
                content: "Proposal for private-alias".into(),
                is_error: false,
                operator_private: true,
                diff: WriteDiffState::NotAWrite,
            },
        ] {
            for event in [
                AppEvent::ProviderChunk {
                    conversation_id: "conv".into(),
                    chunk: chunk.clone(),
                },
                AppEvent::PeerProviderChunk {
                    node: std::sync::Arc::new(AgentId::new()),
                    conversation_id: "conv".into(),
                    chunk: chunk.clone(),
                },
            ] {
                bus.emit_domain(event).unwrap();
                let retained = domain.try_recv().expect("private domain event retained");
                let retained_chunk = match retained {
                    AppEvent::ProviderChunk { chunk, .. }
                    | AppEvent::PeerProviderChunk { chunk, .. } => chunk,
                    _ => unreachable!(),
                };
                assert_eq!(
                    serde_json::to_value(retained_chunk).unwrap(),
                    serde_json::to_value(&chunk).unwrap(),
                    "domain payload must stay byte-equivalent"
                );
                assert!(matches!(
                    raw.try_recv(),
                    Err(broadcast::error::TryRecvError::Empty)
                ));
            }
        }
        let public = StreamChunk::ToolResult {
            id: "read-tool".into(),
            content: "ordinary tool result".into(),
            is_error: false,
            operator_private: false,
            diff: WriteDiffState::NotAWrite,
        };
        let encoded = serde_json::to_string(&public).unwrap();
        assert!(!encoded.contains("operator_private"));
        let decoded: StreamChunk = serde_json::from_str(&encoded).unwrap();
        assert!(
            !decoded.is_operator_private(),
            "historical results default public"
        );
        bus.emit_domain(AppEvent::ProviderChunk {
            conversation_id: "conv".into(),
            chunk: public,
        })
        .unwrap();
        assert!(
            serde_json::to_string(&raw.try_recv().unwrap())
                .unwrap()
                .contains("ordinary tool result"),
            "unrelated results remain observable"
        );
    }

    #[test]
    fn every_proposal_scheduler_transition_is_private() {
        use crate::domain::models::{RequestId, ToolCall, ToolCallRequest, ToolCallResult};
        let request = ToolCallRequest {
            id: "private-tool".into(),
            tool_name: crate::domain::models::team_proposal::TEAM_PROPOSAL_TOOL_NAME.into(),
            input: serde_json::json!({"body":"secret","recipients":["private-alias"]}),
        };
        for call in [
            ToolCall::Validating {
                id: request.id.clone(),
                request: request.clone(),
                started_at: 0,
            },
            ToolCall::Scheduled {
                id: request.id.clone(),
                request: request.clone(),
            },
            ToolCall::AwaitingApproval {
                id: request.id.clone(),
                request: request.clone(),
                approval_id: RequestId::new(),
            },
            ToolCall::Executing {
                id: request.id.clone(),
                request: request.clone(),
                started_at: 0,
            },
            ToolCall::Success {
                id: request.id.clone(),
                request: request.clone(),
                result: ToolCallResult {
                    output: "private-alias".into(),
                    is_error: false,
                    duration_ms: 0,
                },
            },
            ToolCall::Error {
                id: request.id.clone(),
                request: request.clone(),
                error: "private-alias".into(),
            },
            ToolCall::Cancelled {
                id: request.id.clone(),
                request: request.clone(),
                reason: "private-alias".into(),
            },
        ] {
            assert!(
                RawEvent::from_app_event(&AppEvent::ToolCallTransitionBridged {
                    conversation_id: "conv".into(),
                    transition: ToolCallTransition {
                        conversation_id: "conv".into(),
                        call
                    },
                })
                .is_none(),
                "no scheduler state may expose proposal arguments or output"
            );
        }
    }

    #[test]
    fn transcript_projection_removes_only_proposal_tools() {
        use crate::domain::models::{ChatMessage, ToolCallInfo, ToolResultInfo, WriteDiffState};
        let private = ToolCallInfo {
            id: "private-call".into(),
            name: crate::domain::models::team_proposal::TEAM_PROPOSAL_TOOL_NAME.into(),
            input: serde_json::json!({"body":"private-body","recipients":["private-alias"]}),
            result: Some(ToolResultInfo {
                content: "Proposal shown for private-alias".into(),
                is_error: false,
                diff: WriteDiffState::NotAWrite,
            }),
            started_at_ms: None,
            completed_at_ms: None,
            status: None,
        };
        let mut public = private.clone();
        public.name = "Read".into();
        public.input = serde_json::json!({"file_path":"public-path"});
        public.result.as_mut().unwrap().content = "public-result".into();
        let retained = ChatMessage {
            tool_calls: vec![private, public],
            content: "public prose".into(),
            ..Default::default()
        };
        let projected = public_transcript_message(&retained);
        let encoded = serde_json::to_string(&projected).unwrap();
        assert!(!encoded.contains("private-body") && !encoded.contains("private-alias"));
        assert!(
            encoded.contains("public-path")
                && encoded.contains("public-result")
                && encoded.contains("public prose")
        );
        assert_eq!(
            retained.tool_calls.len(),
            2,
            "A6 stored transcript is unchanged"
        );
        assert_eq!(projected.tool_calls.len(), 1);
    }
}
