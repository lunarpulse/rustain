use serde::{Deserialize, Serialize};

/// AD-1830 fact 1 — who composed the text. Recorded with the act, never inferred.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Authorship {
    HumanWritten,
    AgentComposed,
    #[serde(other)]
    Unknown,
}

impl Authorship {
    #[must_use]
    pub const fn human_written() -> Self {
        Self::HumanWritten
    }

    #[must_use]
    pub const fn unknown() -> Self {
        Self::Unknown
    }

    #[must_use]
    pub const fn is_human_written(&self) -> bool {
        matches!(self, Self::HumanWritten)
    }

    /// Whether this row carries no known authorship provenance.
    #[must_use]
    pub const fn is_unknown(&self) -> bool {
        matches!(self, Self::Unknown)
    }

    /// The wire token the thread read serves.
    #[must_use]
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::HumanWritten => "human-written",
            Self::AgentComposed => "agent-composed",
            Self::Unknown => "unknown",
        }
    }

    /// Parse a served token; anything unnamed is [`Self::Unknown`].
    #[must_use]
    pub fn from_wire(token: &str) -> Self {
        match token {
            "human-written" => Self::HumanWritten,
            "agent-composed" => Self::AgentComposed,
            _ => Self::Unknown,
        }
    }
}

/// AD-1830 fact 2 — whether a human decided this particular send.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SendAuthorization {
    HumanDecided,
    Automatic,
    #[serde(other)]
    Unknown,
}

impl SendAuthorization {
    #[must_use]
    pub const fn unknown() -> Self {
        Self::Unknown
    }

    #[must_use]
    pub const fn is_unknown(&self) -> bool {
        matches!(self, Self::Unknown)
    }

    #[must_use]
    pub const fn is_automatic(&self) -> bool {
        matches!(self, Self::Automatic)
    }
}
