use async_trait::async_trait;
use serde_json::Value;
use thiserror::Error;
use tokio::sync::broadcast;

use crate::domain::models::AgentEnvelope;

#[derive(Debug, Error)]
pub enum AgentTransportError {
    #[error("agent transport send failed: {0}")]
    Send(String),
    #[error("agent transport subscribe failed: {0}")]
    Subscribe(String),
}

/// R1 subset of the RAP transport surface.
///
/// `announce`/`discover`/`stream` remain Epic 17.4/Epic 18 work. Story 17.1a
/// only needs a shared signed-envelope boundary that later transports can reuse.
///
/// `RapTransport` is this port's broadcast adapter. `PeerTransport` is the
/// sibling cross-host port with remote addressing and connection lifecycle.
#[async_trait]
pub trait AgentTransport: Send + Sync {
    async fn send(&self, envelope: AgentEnvelope<Value>) -> Result<(), AgentTransportError>;
    fn subscribe(&self) -> Result<broadcast::Receiver<AgentEnvelope<Value>>, AgentTransportError>;
}
