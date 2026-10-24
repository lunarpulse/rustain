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

    #[must_use]
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::HumanDecided => "human-decided",
            Self::Automatic => "automatic",
            Self::Unknown => "unknown",
        }
    }

    #[must_use]
    pub fn from_wire(token: &str) -> Self {
        match token {
            "human-decided" => Self::HumanDecided,
            "automatic" => Self::Automatic,
            _ => Self::Unknown,
        }
    }
}

/// Product-owned marks for independent, recorded provenance facts.
#[must_use]
pub const fn provenance_tags(
    authorship: Authorship,
    send_authorization: SendAuthorization,
) -> &'static str {
    match (authorship, send_authorization) {
        (Authorship::AgentComposed, SendAuthorization::Automatic) => {
            " [agent-composed] [auto-sent]"
        }
        (Authorship::AgentComposed, _) => " [agent-composed]",
        (_, SendAuthorization::Automatic) => " [auto-sent]",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_authorization_wire_tokens_round_trip_and_unknown_is_tolerant() {
        for value in [
            SendAuthorization::HumanDecided,
            SendAuthorization::Automatic,
            SendAuthorization::Unknown,
        ] {
            assert_eq!(SendAuthorization::from_wire(value.wire_name()), value);
        }
        for token in ["", "future-token", "Automatic", " human-decided"] {
            assert_eq!(
                SendAuthorization::from_wire(token),
                SendAuthorization::Unknown
            );
        }
    }

    #[test]
    fn provenance_tags_cover_independent_facts_without_inference() {
        for (authorship, authorization, expected) in [
            (
                Authorship::HumanWritten,
                SendAuthorization::HumanDecided,
                "",
            ),
            (Authorship::Unknown, SendAuthorization::Unknown, ""),
            (
                Authorship::AgentComposed,
                SendAuthorization::Unknown,
                " [agent-composed]",
            ),
            (
                Authorship::Unknown,
                SendAuthorization::Automatic,
                " [auto-sent]",
            ),
            (
                Authorship::AgentComposed,
                SendAuthorization::Automatic,
                " [agent-composed] [auto-sent]",
            ),
        ] {
            assert_eq!(provenance_tags(authorship, authorization), expected);
        }
    }
}
