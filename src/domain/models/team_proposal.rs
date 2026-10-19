use crate::domain::models::SemanticMessageType;

pub const TEAM_PROPOSAL_TOOL_NAME: &str = "propose_team_message";

/// An immutable message snapshot awaiting the operator's decision.
#[derive(Debug, PartialEq, Eq)]
pub struct TeamProposal {
    pub id: String,
    pub conversation_id: String,
    pub recipients: Vec<String>,
    pub message_type: SemanticMessageType,
    pub body: String,
}

/// The operator's decision on a proposal.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TeamProposalDecision {
    Send,
    Dismiss,
}

/// Whether a proposal was displayed, unavailable, or could not be displayed
/// because another proposal is already pending.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProposalRaiseOutcome {
    Shown,
    Unavailable,
    Pending,
}
