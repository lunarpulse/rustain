//! Story C-3 (AD-12) — who may see which attach frames.
//!
//! The daemon fans every bus event with a raw projection out to both audiences,
//! TrustedLocal and Peer. MCP connection status carries connection error text
//! (a spawn failure names the configured command), so those frames go to
//! TrustedLocal attachments only. The channel adapter's link state (Story C-12
//! AC4) is operator health, not peer business, so it is withheld the same way.
//! Lives in its own module, with the frame variant name helper moved out of
//! `server.rs`, so the A-15 ratchet (`server.rs` never grows) holds.

use super::protocol::DaemonFrame;
use crate::infrastructure::runtime::event_bus::RawEventKind;

/// Whether `frame` must NOT be sent to a Peer-tier attachment: exactly the
/// `McpConnectionStateChanged`, `McpCatalogChanged` and `ChannelLinkChanged`
/// events.
pub(super) fn withheld_from_peer(frame: &DaemonFrame) -> bool {
    matches!(
        frame,
        DaemonFrame::Event(event)
            if matches!(
                event.kind,
                RawEventKind::McpConnectionStateChanged { .. }
                    | RawEventKind::McpCatalogChanged { .. }
                    | RawEventKind::ChannelLinkChanged { .. }
            )
    )
}

pub(super) fn frame_variant_name(frame: &DaemonFrame) -> &'static str {
    match frame {
        DaemonFrame::AttachChallenge { .. } => "AttachChallenge",
        DaemonFrame::AttachAck { .. } => "AttachAck",
        DaemonFrame::Event(_) => "Event",
        DaemonFrame::ToolEventWithheld { .. } => "ToolEventWithheld",
        DaemonFrame::FrameTooLarge { .. } => "FrameTooLarge",
        DaemonFrame::PeerRowOperation(_) => "PeerRowOperation",
        DaemonFrame::History { .. } => "History",
        DaemonFrame::ApprovalRequest { .. } => "ApprovalRequest",
        DaemonFrame::ApprovalInputChunk { .. } => "ApprovalInputChunk",
        DaemonFrame::ApprovalInputComplete { .. } => "ApprovalInputComplete",
        DaemonFrame::ApprovalRefused { .. } => "ApprovalRefused",
        DaemonFrame::ApprovalClosed { .. } => "ApprovalClosed",
        DaemonFrame::ConsolidationProposed { .. } => "ConsolidationProposed",
        DaemonFrame::PeerAccepted { .. } => "PeerAccepted",
        DaemonFrame::PeerShareResult { .. } => "PeerShareResult",
        DaemonFrame::RecipientActionResult { .. } => "RecipientActionResult",
        DaemonFrame::RecipientContextState { .. } => "RecipientContextState",
        DaemonFrame::TeamProposal { .. } => "TeamProposal",
        DaemonFrame::TeamProposalClosed { .. } => "TeamProposalClosed",
        DaemonFrame::TeamSendStarted { .. } => "TeamSendStarted",
        DaemonFrame::TeamSendSettled { .. } => "TeamSendSettled",
        DaemonFrame::TeamViewChunk { .. } => "TeamViewChunk",
        DaemonFrame::TeamViewDone { .. } => "TeamViewDone",
        DaemonFrame::TeamRetractPreviewed { .. } => "TeamRetractPreviewed",
        DaemonFrame::TeamRetractAnswered { .. } => "TeamRetractAnswered",
        DaemonFrame::TeamStatusResult { .. } => "TeamStatusResult",
        DaemonFrame::SenderConsentRevoked { .. } => "SenderConsentRevoked",
        DaemonFrame::ActiveTurn { .. } => "ActiveTurn",
        DaemonFrame::InterruptResult { .. } => "InterruptResult",
        DaemonFrame::Detached => "Detached",
        DaemonFrame::Error(_) => "Error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::{ChannelLinkState, McpConnectionState, NoticeLevel, StreamChunk};
    use crate::infrastructure::runtime::event_bus::RawEvent;

    fn event(kind: RawEventKind) -> DaemonFrame {
        DaemonFrame::Event(RawEvent {
            conversation_id: None,
            timestamp_ms: 1,
            kind,
        })
    }

    #[test]
    fn operator_health_events_are_withheld_from_peers_and_nothing_else_is() {
        assert!(withheld_from_peer(&event(
            RawEventKind::McpConnectionStateChanged {
                server_id: "s".into(),
                state: McpConnectionState::NotConnected,
                source_profile: None,
            }
        )));
        assert!(withheld_from_peer(&event(
            RawEventKind::McpCatalogChanged {
                server_id: "s".into(),
                tool_count: 2,
            }
        )));
        for state in [
            ChannelLinkState::Connecting,
            ChannelLinkState::Connected,
            ChannelLinkState::Reconnecting,
            ChannelLinkState::Offline,
        ] {
            assert!(withheld_from_peer(&event(
                RawEventKind::ChannelLinkChanged {
                    adapter: "slack".into(),
                    state,
                }
            )));
        }
        assert!(!withheld_from_peer(&event(RawEventKind::Provider(
            StreamChunk::Text {
                content: "hi".into(),
                parent_tool_use_id: None,
            }
        ))));
        assert!(!withheld_from_peer(&event(RawEventKind::SystemNotice {
            level: NoticeLevel::Info,
            message: "note".into(),
        })));
        assert!(!withheld_from_peer(&DaemonFrame::Detached));
    }
}
