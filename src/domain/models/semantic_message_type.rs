use serde::{Deserialize, Serialize};

/// The closed FR163 semantic message vocabulary.
///
/// This is the resolved domain value. Wire carriers retain the original scalar
/// token until their transport boundary has verified the message.

/// Deserialization contract, deliberate per the 19.15 review (2026-09-17):
/// `#[serde(other)]` degrades unknown string tokens to
/// [`SemanticMessageType::Unknown`], while a non-string JSON value in a
/// persisted record fails deserialization loudly. Malformed persisted data
/// fail-closes; only an unrecognised token degrades.
#[non_exhaustive]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum SemanticMessageType {
    Consultation,
    StoryAssignment,
    DesignUpdate,
    StatusRequest,
    BugReport,
    ScopeChange,
    ArchitectureUpdate,
    RetroRequest,
    #[default]
    #[serde(other)]
    Unknown,
}

impl SemanticMessageType {
    const KNOWN: [Self; 8] = [
        Self::Consultation,
        Self::StoryAssignment,
        Self::DesignUpdate,
        Self::StatusRequest,
        Self::BugReport,
        Self::ScopeChange,
        Self::ArchitectureUpdate,
        Self::RetroRequest,
    ];

    /// Resolve a byte-preserved transport token into the closed vocabulary.
    #[must_use]
    pub fn parse(token: &str) -> Self {
        Self::KNOWN
            .into_iter()
            .find(|kind| semantic_message_type_metadata(*kind).token == token)
            .unwrap_or(Self::Unknown)
    }

    /// Resolve an optional transport token. Absence and empty both mean
    /// [`Self::Unknown`], as does any value introduced by a newer peer.
    #[must_use]
    pub fn parse_optional(token: Option<&str>) -> Self {
        token.map_or(Self::Unknown, Self::parse)
    }
}

/// Whether a registry entry names one of FR163's eight recognised values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SemanticMessageRecognition {
    Known,
    Unknown,
}

/// Policy quantities a semantic type is permitted to select.
///
/// These are policy capabilities only. They grant no authority and deliberately
/// do not encode per-type action sets; action availability also depends on
/// ownership, response mode, and lifecycle state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SemanticMessagePolicyCapabilities {
    pub notification: bool,
    pub response_mode: bool,
    pub auto_response: bool,
}

/// Canonical metadata shared by parsing, policy, rendering, and explanation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SemanticMessageTypeMetadata {
    pub token: &'static str,
    /// Verbatim operator-facing noun. Semantic type identifiers are config keys,
    /// not humanised state labels.
    pub operator_noun: &'static str,
    pub recognition: SemanticMessageRecognition,
    pub policy: SemanticMessagePolicyCapabilities,
}

const KNOWN_POLICY: SemanticMessagePolicyCapabilities = SemanticMessagePolicyCapabilities {
    notification: true,
    response_mode: true,
    auto_response: true,
};

const UNKNOWN_POLICY: SemanticMessagePolicyCapabilities = SemanticMessagePolicyCapabilities {
    notification: false,
    response_mode: false,
    auto_response: false,
};

/// The single canonical semantic-message registry.
///
/// The exhaustive in-crate match is intentional: adding a variant must make
/// this function fail to compile until its metadata is supplied. Do not add a
/// wildcard arm.
#[must_use]
pub const fn semantic_message_type_metadata(
    kind: SemanticMessageType,
) -> &'static SemanticMessageTypeMetadata {
    match kind {
        SemanticMessageType::Consultation => &SemanticMessageTypeMetadata {
            token: "consultation",
            operator_noun: "consultation",
            recognition: SemanticMessageRecognition::Known,
            policy: KNOWN_POLICY,
        },
        SemanticMessageType::StoryAssignment => &SemanticMessageTypeMetadata {
            token: "story_assignment",
            operator_noun: "story_assignment",
            recognition: SemanticMessageRecognition::Known,
            policy: KNOWN_POLICY,
        },
        SemanticMessageType::DesignUpdate => &SemanticMessageTypeMetadata {
            token: "design_update",
            operator_noun: "design_update",
            recognition: SemanticMessageRecognition::Known,
            policy: KNOWN_POLICY,
        },
        SemanticMessageType::StatusRequest => &SemanticMessageTypeMetadata {
            token: "status_request",
            operator_noun: "status_request",
            recognition: SemanticMessageRecognition::Known,
            policy: KNOWN_POLICY,
        },
        SemanticMessageType::BugReport => &SemanticMessageTypeMetadata {
            token: "bug_report",
            operator_noun: "bug_report",
            recognition: SemanticMessageRecognition::Known,
            policy: KNOWN_POLICY,
        },
        SemanticMessageType::ScopeChange => &SemanticMessageTypeMetadata {
            token: "scope_change",
            operator_noun: "scope_change",
            recognition: SemanticMessageRecognition::Known,
            policy: KNOWN_POLICY,
        },
        SemanticMessageType::ArchitectureUpdate => &SemanticMessageTypeMetadata {
            token: "architecture_update",
            operator_noun: "architecture_update",
            recognition: SemanticMessageRecognition::Known,
            policy: KNOWN_POLICY,
        },
        SemanticMessageType::RetroRequest => &SemanticMessageTypeMetadata {
            token: "retro_request",
            operator_noun: "retro_request",
            recognition: SemanticMessageRecognition::Known,
            policy: KNOWN_POLICY,
        },
        SemanticMessageType::Unknown => &SemanticMessageTypeMetadata {
            token: "",
            operator_noun: "unknown type",
            recognition: SemanticMessageRecognition::Unknown,
            policy: UNKNOWN_POLICY,
        },
    }
}

/// Iterate the eight valid configuration and wire tokens in canonical order.
pub fn semantic_message_type_tokens() -> impl ExactSizeIterator<Item = &'static str> {
    SemanticMessageType::KNOWN
        .into_iter()
        .map(|kind| semantic_message_type_metadata(kind).token)
}
