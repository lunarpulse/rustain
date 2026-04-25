//! Runtime application state — session-level fields that span the event loop.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::infrastructure::runtime::event_bus::EventBus;

pub struct AppState {
    pub session_cancel: CancellationToken,
    pub event_bus: Arc<EventBus>,
}

impl AppState {
    pub fn new(
        raw_capacity: usize,
    ) -> (
        Self,
        tokio::sync::mpsc::UnboundedReceiver<crate::domain::events::AppEvent>,
    ) {
        let (event_bus, domain_rx) = EventBus::new(raw_capacity);
        (
            Self {
                session_cancel: CancellationToken::new(),
                event_bus: Arc::new(event_bus),
            },
            domain_rx,
        )
    }
}
