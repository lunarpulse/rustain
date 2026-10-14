pub const TEAM_SEND_DELIVERED_LABEL: &str = "delivered";
pub const TEAM_SEND_DECLINED_LABEL: &str = "declined";
pub const TEAM_SEND_UNREACHABLE_LABEL: &str = "unreachable";
pub const TEAM_SEND_ACCEPTED_WITHOUT_ITEM_LABEL: &str = "accepted — this peer keeps no item id";
pub const TEAM_SEND_NOT_SENT_LABEL: &str = "not sent — this host could not record the attempt";

/// Provenance for a recipient send failure: either locally ratified guidance or
/// text influenced by the peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendCause {
    Ratified(String),
    PeerInfluenced(String),
}

impl SendCause {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Ratified(text) | Self::PeerInfluenced(text) => text,
        }
    }
}

/// The typed first-answer outcome for one addressed recipient.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RecipientOutcome {
    Delivered,
    Declined,
    AwaitingApproval,
    AcceptedWithoutItem,
    AskedQuestion { task_id: String },
    Unreachable { cause: Option<SendCause> },
    NoUsableAnswer,
    NotSent,
}
