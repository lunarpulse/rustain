use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::broadcast;

use crate::domain::models::AgentEnvelope;
use crate::domain::ports::{AgentTransport, AgentTransportError};

#[derive(Debug)]
pub struct RapTransport {
    tx: broadcast::Sender<AgentEnvelope<Value>>,
}

impl Default for RapTransport {
    fn default() -> Self {
        Self::new(128)
    }
}

impl RapTransport {
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity.max(1));
        Self { tx }
    }
}

#[async_trait]
impl AgentTransport for RapTransport {
    async fn send(&self, envelope: AgentEnvelope<Value>) -> Result<(), AgentTransportError> {
        self.tx
            .send(envelope)
            .map(|_| ())
            .map_err(|e| AgentTransportError::Send(e.to_string()))
    }

    fn subscribe(&self) -> Result<broadcast::Receiver<AgentEnvelope<Value>>, AgentTransportError> {
        Ok(self.tx.subscribe())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::rap::AgentSigner;
    use crate::domain::models::{AgentId, CorrelationId, MessageKind};

    #[tokio::test]
    async fn transport_broadcasts_signed_envelopes() {
        let transport = RapTransport::new(4);
        let mut rx = transport.subscribe().unwrap();
        let signer = AgentSigner::from_signing_key(ed25519_dalek::SigningKey::from_bytes(&[9; 32]));
        let sender_path = format!("{}/agent", signer.identity().peer_id.as_str());
        let envelope = signer
            .sign(
                AgentId::from_peer_path(&sender_path).unwrap(),
                AgentId::parse("recipient").unwrap(),
                CorrelationId::new("corr"),
                MessageKind::PeerMessage,
                1,
                2_000,
                "nonce".to_string(),
                Vec::new(),
                serde_json::json!({"ok":true}),
            )
            .unwrap();
        transport.send(envelope.clone()).await.unwrap();
        assert_eq!(
            rx.recv().await.unwrap().header.sequence,
            envelope.header.sequence
        );
    }
}
