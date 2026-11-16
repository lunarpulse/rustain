//! Story C-3 AC8 — the Peer-tier attach audience. MCP connection status and
//! catalogue events carry connection error text (a spawn failure names the
//! configured command), so they reach TrustedLocal attachments only. A provider
//! text event is the positive control: both tiers receive it.
//!
//! Child module of `server.rs`, declared there with
//! `#[cfg(test)] mod attach_audience_tests;`, so it can reach the private
//! `ConnRegistry::fanout`.

use super::*;
use crate::domain::models::McpConnectionState;

fn event(kind: RawEventKind) -> DaemonFrame {
    DaemonFrame::Event(RawEvent {
        conversation_id: None,
        timestamp_ms: 0,
        kind,
    })
}

fn mcp_failed() -> DaemonFrame {
    event(RawEventKind::McpConnectionStateChanged {
        server_id: "c3-srv".into(),
        state: McpConnectionState::ConnectionFailed {
            attempts: 5,
            last_error: "C3-CANARY-ERR".into(),
        },
        source_profile: None,
    })
}

fn mcp_catalog() -> DaemonFrame {
    event(RawEventKind::McpCatalogChanged {
        server_id: "c3-srv".into(),
        tool_count: 2,
    })
}

fn provider_text() -> DaemonFrame {
    event(RawEventKind::Provider(StreamChunk::Text {
        content: "C3-PROVIDER-TEXT".into(),
        parent_tool_use_id: None,
    }))
}

struct Rig {
    registry: ConnRegistry,
    local_rx: mpsc::Receiver<DaemonFrame>,
    peer_rx: mpsc::Receiver<DaemonFrame>,
}

/// One TrustedLocal writer (id 1) and one Peer read-only mirror (id 2).
fn rig() -> Rig {
    let (local_tx, local_rx) = mpsc::channel(8);
    let (peer_tx, peer_rx) = mpsc::channel(8);
    let mut registry = ConnRegistry::default();
    for (id, tx, mode, tier) in [
        (
            1,
            local_tx,
            AttachMode::ReadWrite,
            ConnectionTier::TrustedLocal,
        ),
        (2, peer_tx, AttachMode::ReadOnly, ConnectionTier::Peer),
    ] {
        registry.conns.push(Conn {
            id,
            tx,
            terminal_tx: watch::channel(None).0,
            mode,
            tier,
            approval_ready: true,
        });
    }
    Rig {
        registry,
        local_rx,
        peer_rx,
    }
}

fn assert_reaches_local_only(frame: DaemonFrame, what: &str) {
    let mut rig = rig();
    let dead = rig.registry.fanout(&frame);
    assert!(dead.is_empty(), "{what}: no queue may be reported dead");
    let delivered = rig
        .local_rx
        .try_recv()
        .unwrap_or_else(|e| panic!("{what}: the TrustedLocal queue must receive it: {e:?}"));
    assert_eq!(
        serde_json::to_value(&delivered).unwrap(),
        serde_json::to_value(&frame).unwrap(),
        "{what}: TrustedLocal receives the frame unchanged"
    );
    assert!(
        rig.local_rx.try_recv().is_err(),
        "{what}: exactly one frame for TrustedLocal"
    );
    if let Ok(leaked) = rig.peer_rx.try_recv() {
        panic!("{what}: the Peer queue must stay empty, got {leaked:?}");
    }
}

#[test]
fn mcp_connection_state_reaches_trusted_local_only() {
    assert_reaches_local_only(mcp_failed(), "McpConnectionStateChanged");
}

#[test]
fn mcp_catalog_change_reaches_trusted_local_only() {
    assert_reaches_local_only(mcp_catalog(), "McpCatalogChanged");
}

#[test]
fn provider_text_reaches_both_tiers() {
    let mut rig = rig();
    let frame = provider_text();
    let dead = rig.registry.fanout(&frame);
    assert!(dead.is_empty());
    for (name, rx) in [
        ("TrustedLocal", &mut rig.local_rx),
        ("Peer", &mut rig.peer_rx),
    ] {
        let delivered = rx
            .try_recv()
            .unwrap_or_else(|e| panic!("{name} must receive provider text: {e:?}"));
        assert_eq!(
            serde_json::to_value(&delivered).unwrap(),
            serde_json::to_value(&frame).unwrap(),
            "{name} receives the provider event unchanged"
        );
    }
}
