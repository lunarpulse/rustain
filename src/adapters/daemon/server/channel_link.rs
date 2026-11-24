//! Story C-12 AC4 (FR68, NFR60) — the daemon channel adapter's redacted link state on the
//! existing attach surface.
//!
//! A lifecycle-owned task publishes the adapter's `ChannelPort::health_updates` watch onto the
//! daemon bus as `AppEvent::ChannelLinkChanged`: the current value first, then every change.
//! The forwarder projects it through the one `from_app_event` mapping and fans it out like any
//! other event. The audience fanout retains the latest link frame, so a TrustedLocal attachment
//! registered later receives it straight after its AttachAck (the late snapshot). Retention,
//! fanout and registration share the registry lock and the bus is FIFO, so an attachment never
//! ends on an older value than the adapter's last published one. Peers get neither the live
//! frames (`attach_audience::withheld_from_peer`) nor the snapshot.
//!
//! No new frame variant and no protocol change: the snapshot is the same `DaemonFrame::Event`,
//! under the same frame cap. The audience fanout moved here from `server.rs` together with the
//! retention (A-15 ratchet: `server.rs` never grows).

use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use super::{AttachServer, ConnRegistry, project_attach_frame};
use crate::adapters::daemon::attach_audience::withheld_from_peer;
use crate::adapters::daemon::protocol::{ConnectionTier, DaemonFrame};
use crate::domain::events::AppEvent;
use crate::domain::models::ChannelLinkState;
use crate::domain::ports::{ChannelPort, EventEmitter};
use crate::infrastructure::runtime::event_bus::{ChannelEmitter, RawEventKind};

fn is_channel_link(frame: &DaemonFrame) -> bool {
    matches!(
        frame,
        DaemonFrame::Event(event) if matches!(event.kind, RawEventKind::ChannelLinkChanged { .. })
    )
}

impl ConnRegistry {
    /// Project once per audience, preserving the existing slow/closed queue policy. A channel
    /// link frame is retained first: the late snapshot is never older than what an attachment
    /// registered before this fanout received.
    pub(super) fn fanout(&mut self, frame: &DaemonFrame) -> Vec<u64> {
        if is_channel_link(frame) {
            self.channel_link = Some(frame.clone());
        }
        let mut dead = Vec::new();
        for tier in [ConnectionTier::TrustedLocal, ConnectionTier::Peer] {
            // Story C-3 (AD-12): MCP connection status goes to TrustedLocal only; so does the
            // channel link state (Story C-12 AC4).
            if tier == ConnectionTier::Peer && withheld_from_peer(frame) {
                continue;
            }
            let mut connections = self
                .conns
                .iter()
                .filter(|conn| conn.tier == tier)
                .peekable();
            if connections.peek().is_none() {
                continue;
            }
            let projected = project_attach_frame(frame.clone(), tier);
            for conn in connections {
                match conn.tx.try_send(projected.clone()) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_))
                    | Err(mpsc::error::TrySendError::Closed(_)) => dead.push(conn.id),
                }
            }
        }
        dead
    }

    /// The late-attach snapshot: queue the retained link frame to `conn_id` when it is a
    /// TrustedLocal attachment. Called under the registry lock that registered it, before its
    /// writer drains, so the frame lands right after the AttachAck and before any later fanout.
    pub(super) fn project_channel_link_to(&self, conn_id: u64) {
        let Some(frame) = &self.channel_link else {
            return;
        };
        if let Some(conn) = self
            .conns
            .iter()
            .find(|conn| conn.id == conn_id && conn.tier == ConnectionTier::TrustedLocal)
        {
            // A fresh queue has room; a closed one is the reader's disconnect to handle.
            let _ = conn.tx.try_send(project_attach_frame(
                frame.clone(),
                ConnectionTier::TrustedLocal,
            ));
        }
    }
}

impl AttachServer {
    /// Spawn the task that publishes `channel`'s redacted link state onto this daemon's bus as
    /// `adapter` (the profile's Channels catalog name, e.g. `"slack"`): the current value first,
    /// then every change, until `shutdown` fires or the adapter drops its sender. `None` when
    /// the adapter publishes no link state (terminal, Telegram): nothing is published for it.
    /// The daemon lifecycle owns, cancels and joins the returned task.
    pub fn spawn_channel_link_publisher(
        &self,
        adapter: &str,
        channel: &dyn ChannelPort,
        shutdown: CancellationToken,
    ) -> Option<tokio::task::JoinHandle<()>> {
        let updates = channel.health_updates()?;
        let emitter = ChannelEmitter::new(self.domain_tx.clone());
        Some(tokio::spawn(publish_link_states(
            adapter.to_owned(),
            updates,
            emitter,
            shutdown,
        )))
    }
}

async fn publish_link_states(
    adapter: String,
    mut updates: watch::Receiver<ChannelLinkState>,
    emitter: ChannelEmitter,
    shutdown: CancellationToken,
) {
    loop {
        let state = *updates.borrow_and_update();
        emitter.emit(AppEvent::ChannelLinkChanged {
            adapter: adapter.clone(),
            state,
        });
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            changed = updates.changed() => {
                if changed.is_err() {
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::Conn;
    use super::*;
    use crate::adapters::daemon::protocol::AttachMode;
    use crate::infrastructure::runtime::event_bus::RawEvent;

    fn link(state: ChannelLinkState) -> DaemonFrame {
        DaemonFrame::Event(RawEvent {
            conversation_id: None,
            timestamp_ms: 0,
            kind: RawEventKind::ChannelLinkChanged {
                adapter: "slack".into(),
                state,
            },
        })
    }

    fn register(
        registry: &mut ConnRegistry,
        id: u64,
        tier: ConnectionTier,
    ) -> mpsc::Receiver<DaemonFrame> {
        let (tx, rx) = mpsc::channel(8);
        registry.conns.push(Conn {
            id,
            tx,
            terminal_tx: watch::channel(None).0,
            mode: AttachMode::ReadOnly,
            tier,
            approval_ready: true,
        });
        rx
    }

    fn received_state(rx: &mut mpsc::Receiver<DaemonFrame>) -> Option<ChannelLinkState> {
        match rx.try_recv().ok()? {
            DaemonFrame::Event(RawEvent {
                kind: RawEventKind::ChannelLinkChanged { adapter, state },
                ..
            }) => {
                assert_eq!(adapter, "slack");
                Some(state)
            }
            other => panic!("unexpected frame {other:?}"),
        }
    }

    #[test]
    fn the_late_snapshot_is_the_latest_link_frame_for_trusted_local_only() {
        let mut registry = ConnRegistry::default();
        // Nothing published yet: a late attach gets nothing.
        let mut early = register(&mut registry, 1, ConnectionTier::TrustedLocal);
        registry.project_channel_link_to(1);
        assert_eq!(received_state(&mut early), None);
        for state in [ChannelLinkState::Connecting, ChannelLinkState::Connected] {
            assert!(registry.fanout(&link(state)).is_empty());
        }
        assert_eq!(
            received_state(&mut early),
            Some(ChannelLinkState::Connecting)
        );
        assert_eq!(
            received_state(&mut early),
            Some(ChannelLinkState::Connected)
        );

        let mut local = register(&mut registry, 2, ConnectionTier::TrustedLocal);
        registry.project_channel_link_to(2);
        assert_eq!(
            received_state(&mut local),
            Some(ChannelLinkState::Connected)
        );
        assert_eq!(received_state(&mut local), None, "exactly one snapshot");

        let mut peer = register(&mut registry, 3, ConnectionTier::Peer);
        registry.project_channel_link_to(3);
        assert!(registry.fanout(&link(ChannelLinkState::Offline)).is_empty());
        assert!(
            peer.try_recv().is_err(),
            "a peer gets no snapshot and no update"
        );
        assert_eq!(received_state(&mut local), Some(ChannelLinkState::Offline));

        // The retained value follows the last fanout: a stale value is replaced.
        let mut late = register(&mut registry, 4, ConnectionTier::TrustedLocal);
        registry.project_channel_link_to(4);
        assert_eq!(received_state(&mut late), Some(ChannelLinkState::Offline));
    }

    #[test]
    fn other_frames_never_replace_the_retained_link_frame() {
        let mut registry = ConnRegistry::default();
        registry.fanout(&link(ChannelLinkState::Reconnecting));
        registry.fanout(&DaemonFrame::Event(RawEvent {
            conversation_id: None,
            timestamp_ms: 0,
            kind: RawEventKind::SystemNotice {
                level: crate::domain::models::NoticeLevel::Info,
                message: "note".into(),
            },
        }));
        let mut local = register(&mut registry, 1, ConnectionTier::TrustedLocal);
        registry.project_channel_link_to(1);
        assert_eq!(
            received_state(&mut local),
            Some(ChannelLinkState::Reconnecting)
        );
    }
}
