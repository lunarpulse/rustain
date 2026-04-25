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

use serde::Serialize;
use tokio::sync::{broadcast, mpsc};

use crate::domain::events::AppEvent;
use crate::domain::models::{NoticeLevel, PermissionMode, StreamChunk};

#[derive(Clone, Debug, Serialize)]
pub struct RawEvent {
    pub conversation_id: Option<String>,
    pub timestamp_ms: i64,
    pub kind: RawEventKind,
}

#[non_exhaustive]
#[derive(Clone, Debug, Serialize)]
pub enum RawEventKind {
    Provider(StreamChunk),
    ModeChanged(PermissionMode),
    SystemNotice { level: NoticeLevel, message: String },
}

pub struct EventBus {
    pub domain_tx: mpsc::UnboundedSender<AppEvent>,
    pub raw_tx: broadcast::Sender<RawEvent>,
}

impl EventBus {
    pub fn new(raw_capacity: usize) -> (Self, mpsc::UnboundedReceiver<AppEvent>) {
        assert!(raw_capacity > 0, "raw_capacity must be > 0");
        let (domain_tx, domain_rx) = mpsc::unbounded_channel();
        let (raw_tx, _) = broadcast::channel(raw_capacity);
        (Self { domain_tx, raw_tx }, domain_rx)
    }

    pub fn emit_domain(&self, event: AppEvent) {
        if let Some(raw) = RawEvent::from_app_event(&event) {
            let _ = self.raw_tx.send(raw);
        }
        if self.domain_tx.send(event).is_err() {
            tracing::trace!("event loop receiver dropped — emit_domain discarded");
        }
    }

    #[allow(dead_code)]
    pub fn subscribe_raw(&self) -> broadcast::Receiver<RawEvent> {
        self.raw_tx.subscribe()
    }
}

impl RawEvent {
    pub fn from_app_event(ev: &AppEvent) -> Option<Self> {
        let now = chrono::Utc::now().timestamp_millis();
        Some(match ev {
            AppEvent::ProviderChunk { conversation_id, chunk } => RawEvent {
                conversation_id: Some(conversation_id.clone()),
                timestamp_ms: now,
                kind: RawEventKind::Provider(chunk.clone()),
            },
            AppEvent::SetPermissionMode(mode) => RawEvent {
                conversation_id: None,
                timestamp_ms: now,
                kind: RawEventKind::ModeChanged(*mode),
            },
            AppEvent::SystemNotice { conversation_id, level, message } => RawEvent {
                conversation_id: conversation_id.clone(),
                timestamp_ms: now,
                kind: RawEventKind::SystemNotice { level: *level, message: message.clone() },
            },
            AppEvent::Tick
            | AppEvent::Resize(..)
            | AppEvent::InputEvent(..)
            | AppEvent::DomainEvent(..) => return None,
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

        bus.emit_domain(AppEvent::SystemNotice {
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

        bus.emit_domain(AppEvent::Tick);

        let ev = tokio::time::timeout(std::time::Duration::from_millis(50), domain_rx.recv())
            .await
            .expect("timed out")
            .expect("channel closed");
        assert!(matches!(ev, AppEvent::Tick));

        let result = tokio::time::timeout(std::time::Duration::from_millis(50), raw_rx.recv()).await;
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
        bus.emit_domain(notice);
        let notice2 = AppEvent::SystemNotice {
            conversation_id: None,
            level: NoticeLevel::Info,
            message: "b".to_string(),
        };
        bus.emit_domain(notice2);

        let mut raw_rx = bus.subscribe_raw();

        let notice3 = AppEvent::SystemNotice {
            conversation_id: None,
            level: NoticeLevel::Info,
            message: "c".to_string(),
        };
        bus.emit_domain(notice3);

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
        assert!(matches!(raw.kind, RawEventKind::ModeChanged(PermissionMode::Normal)));
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
}
