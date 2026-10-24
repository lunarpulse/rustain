//! Decode refusals contain no client bytes, variants, or serde diagnostics.
use super::AttachServer;
use crate::adapters::daemon::protocol::{DaemonFrame, ProtocolError};

pub(super) const UNDECODABLE_CLIENT_FRAME: &str =
    "this daemon could not decode a frame from this client; the frame was ignored";
pub(super) const UNDECODABLE_FIRST_FRAME: &str =
    "the first frame could not be decoded; attach with a client of the same version";

impl AttachServer {
    /// JSON decoding consumed a complete frame, so the next length prefix is safe.
    pub(super) async fn answer_undecodable_frame(&self, conn_id: u64) {
        self.send_to(
            conn_id,
            DaemonFrame::Error(ProtocolError::Malformed(UNDECODABLE_CLIENT_FRAME.into())),
        )
        .await;
    }
}
