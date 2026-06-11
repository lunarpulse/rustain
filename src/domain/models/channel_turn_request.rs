//! Request from a non-socket channel into the daemon turn loop.

use tokio::sync::oneshot;

use crate::domain::models::ChannelKind;

/// One inbound channel message plus the response route back to that channel.
pub struct ChannelTurnRequest {
    pub text: String,
    pub origin: ChannelKind,
    /// Routing key for sending the assistant response back to the channel adapter.
    pub response_tx: oneshot::Sender<String>,
}
