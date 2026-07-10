use serde::{Deserialize, Serialize};

use crate::domain::models::{AgentId, CorrelationId, Ed25519Sig, MessageKind, PeerIdentity};
use thiserror::Error;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentEnvelopeHeader {
    pub sender: AgentId,
    pub recipient: AgentId,
    pub correlation_id: CorrelationId,
    pub kind: MessageKind,
    pub sequence: u64,
    pub not_after: i64,
    pub nonce: String,
    pub content_hash: Vec<u8>,
    /// Hash of this signer's immediately preceding accepted header. Empty only
    /// for the genesis entry of a single-session peer feed.
    pub prev_hash: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentEnvelope<T> {
    pub header: AgentEnvelopeHeader,
    pub body: T,
    pub signer: PeerIdentity,
    pub signature: Ed25519Sig,
}

impl<T> AgentEnvelope<T> {
    pub fn new(
        header: AgentEnvelopeHeader,
        body: T,
        signer: PeerIdentity,
        signature: Ed25519Sig,
    ) -> Self {
        Self {
            header,
            body,
            signer,
            signature,
        }
    }
}

#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RapTaskState {
    Submitted,
    Working,
    InputRequired,
    AuthRequired,
    Completed,
    Failed,
    Canceled,
    Rejected,
}

impl RapTaskState {
    pub const R1_STATES: [Self; 8] = [
        Self::Submitted,
        Self::Working,
        Self::InputRequired,
        Self::AuthRequired,
        Self::Completed,
        Self::Failed,
        Self::Canceled,
        Self::Rejected,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Submitted => "submitted",
            Self::Working => "working",
            Self::InputRequired => "inputRequired",
            Self::AuthRequired => "authRequired",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
            Self::Rejected => "rejected",
        }
    }

    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Canceled | Self::Rejected
        )
    }

    pub const fn can_transition_to(self, target: Self) -> bool {
        match self {
            Self::Submitted => matches!(target, Self::Working | Self::Canceled | Self::Rejected),
            Self::Working => matches!(
                target,
                Self::InputRequired
                    | Self::AuthRequired
                    | Self::Completed
                    | Self::Failed
                    | Self::Canceled
                    | Self::Rejected
            ),
            Self::InputRequired | Self::AuthRequired => matches!(
                target,
                Self::Working | Self::Failed | Self::Canceled | Self::Rejected
            ),
            Self::Completed | Self::Failed | Self::Canceled | Self::Rejected => false,
        }
    }

    pub fn transition_or_err(&mut self, target: Self) -> Result<(), RapTaskStateError> {
        if !self.can_transition_to(target) {
            return Err(RapTaskStateError::IllegalTransition {
                from: *self,
                to: target,
            });
        }
        *self = target;
        Ok(())
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum RapTaskStateError {
    #[error("illegal RAP task transition: {from:?} -> {to:?}")]
    IllegalTransition {
        from: RapTaskState,
        to: RapTaskState,
    },
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rap_task_state_r1_table_is_complete_and_stable() {
        let names: Vec<&str> = RapTaskState::R1_STATES.iter().map(|s| s.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "submitted",
                "working",
                "inputRequired",
                "authRequired",
                "completed",
                "failed",
                "canceled",
                "rejected",
            ]
        );
    }
}
