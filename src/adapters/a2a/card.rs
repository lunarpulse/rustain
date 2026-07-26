//! Version-stable AgentCard discovery view.

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize};

use super::error::A2aError;

use crate::domain::models::capability_registry::CapabilityRegistry;

/// The most skills a public card discloses (Story 18.1b, AC7b / R2).
///
/// Deterministic prefix of the id-sorted inventory. Bounds the *card*, never
/// withholds it.
pub const MAX_DISCLOSED_SKILLS: usize = 64;

/// The byte budget for a complete signed card.
///
/// Off-host, `GET /.well-known/agent-card.json` is the one surface reachable
/// **before** authentication, and serving it costs a registry snapshot, a
/// projection, a sort, two JCS canonicalizations and an Ed25519 signature. The
/// budget bounds the final bytes we emit and, with the signed-card cache, the
/// work an unauthenticated caller can make us repeat.
pub const MAX_CARD_BYTES: usize = 96 * 1024;

/// The largest JWS `signatures` member `sign_card` can append to a JCS card.
///
/// A canonical [`crate::domain::models::PeerId`] is 68 ASCII hex bytes, making
/// the protected `{"alg":"EdDSA","kid":"…"}` header 92 bytes and its unpadded
/// base64url form 123 bytes. Together with the fixed 86-byte Ed25519 signature
/// and JSON member syntax, the appended member is exactly 256 bytes. Reserve it
/// before signing so a near-cap unsigned card cannot become an over-cap served
/// card. Changes to the peer-id or JWS encoding must update this bound.
const MAX_SIGNATURE_OVERHEAD_BYTES: usize = 256;
const MAX_UNSIGNED_CARD_BYTES: usize = MAX_CARD_BYTES - MAX_SIGNATURE_OVERHEAD_BYTES;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServedAgentCard {
    name: String,
    description: String,
    version: String,
    capabilities: ServedCapabilities,
    default_input_modes: Vec<String>,
    default_output_modes: Vec<String>,
    skills: Vec<ServedAgentSkill>,
    supported_interfaces: Vec<ServedInterface>,
    /// Story 18.1b, AC7b (R9/D1): **declare what you enforce.**
    ///
    /// Empirically this is where auth belongs — 53 of the 141 live cards in
    /// `tests/fixtures/a2a/CORPUS_141_live_cards_2026-07-17.json` publish
    /// `securitySchemes` in the *public* card (28 `apiKey`, 26 `http`).
    /// Key-gating the card would gate the document that explains how to get past
    /// the gate; without these fields a conformant client fetches the card, sees
    /// no requirement, sends unauthenticated, and is rejected with no
    /// discoverable reason.
    ///
    /// A `BTreeMap` so `DF-18-1-MTLS` and `DF-18-1-OAUTH2` are extra entries in
    /// a map that already exists, not a card-shape migration — and so JCS
    /// canonicalization sees a deterministic order.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    security_schemes: BTreeMap<String, serde_json::Value>,
    /// Which of the declared schemes a caller must satisfy. A2A models this as
    /// an array of alternatives; we enforce exactly one.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    security: Vec<BTreeMap<String, Vec<String>>>,
    /// Advertised honestly: this story does not implement the spec's
    /// authenticated extended card (`DF-18-1-EXTCARD`), so it says so rather
    /// than advertising an endpoint that does not exist.
    supports_authenticated_extended_card: bool,
    #[serde(
        rename = "x-rustain-ownership",
        skip_serializing_if = "Option::is_none"
    )]
    ownership: Option<ServedOwnership>,
    /// Present only when the deterministic capability cap actually bit.
    ///
    /// A silent truncation is a lie with good intentions: a peer that reads this
    /// card and concludes "that agent has 32 skills" must be able to find out
    /// that it has 900 and we disclosed a bounded prefix.
    #[serde(
        rename = "x-rustain-truncated",
        skip_serializing_if = "Option::is_none"
    )]
    truncated: Option<ServedTruncation>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    signatures: Vec<ServedSignature>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ServedTruncation {
    disclosed_skills: usize,
    total_skills: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ServedCapabilities {
    streaming: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ServedOwnership {
    kind: &'static str,
    peer_id: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ServedAgentSkill {
    id: String,
    name: String,
    description: String,
    tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ServedInterface {
    url: String,
    protocol_binding: &'static str,
    protocol_version: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ServedSignature {
    pub(crate) protected: String,
    pub(crate) signature: String,
}

impl ServedAgentCard {
    /// Project the live local capability registry into the intentionally narrow
    /// A2A disclosure policy. AgentCard `skills` describe agent skills, not raw
    /// built-in/MCP tools or recursively discovered remote A2A capabilities.
    ///
    /// Async because a signed card must never be built from a fabricated
    /// catalog: `snapshot_consistent` awaits the read lock, whereas the
    /// best-effort `snapshot()` yields an EMPTY vec under contention and would
    /// have us sign "this agent has no skills" during a concurrent register.
    ///
    /// `description` is projected verbatim: capability text is by-design
    /// disclosure (AC3a). The opacity boundary is *instance* state — workspace
    /// root, input schemas, tool argv, system prompt — none of which appear in
    /// any field of this struct.
    pub async fn from_registry(registry: &CapabilityRegistry, endpoint_url: &str) -> Self {
        let mut skills = registry
            .snapshot_consistent()
            .await
            .into_iter()
            .filter(|capability| capability.protocol == "skill")
            .map(|capability| ServedAgentSkill {
                id: capability.name.clone(),
                name: capability.name,
                description: capability.description,
                tags: vec!["rustain".to_owned(), "skill".to_owned()],
            })
            .collect::<Vec<_>>();
        skills.sort_by(|left, right| left.id.cmp(&right.id));

        let total_skills = skills.len();
        // R2 (repaired) — BOUND the card, never withhold it. Failing closed on
        // the whole document would make a large skill inventory render the agent
        // undiscoverable *by its own defence*, triggered by success. Discovery is
        // mandatory, so we serve a valid card at a deterministic cap instead.
        skills.truncate(MAX_DISCLOSED_SKILLS);

        let mut card = Self {
            name: "rustain".to_owned(),
            description: "Terminal-native AI coding agent".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            capabilities: ServedCapabilities { streaming: false },
            default_input_modes: vec!["text/plain".to_owned()],
            default_output_modes: vec!["text/plain".to_owned()],
            skills,
            supported_interfaces: vec![ServedInterface {
                url: endpoint_url.to_owned(),
                protocol_binding: "JSONRPC",
                protocol_version: "1.0",
            }],
            security_schemes: BTreeMap::new(),
            security: Vec::new(),
            supports_authenticated_extended_card: false,
            ownership: None,
            truncated: None,
            signatures: Vec::new(),
        };
        if card.skills.len() < total_skills {
            card.truncated = Some(ServedTruncation {
                disclosed_skills: card.skills.len(),
                total_skills,
            });
        }
        card.enforce_byte_budget();
        card
    }

    /// Shed skills, largest-index first, until the unsigned JCS card leaves
    /// room for its final detached JWS signature.
    ///
    /// Deterministic: `skills` is already sorted by id, so repeated builds over
    /// the same registry drop the same entries. The required fields are never
    /// candidates — an over-budget card is still a *valid* card. A prospective
    /// truncation marker participates in the measurement so adding that marker
    /// cannot itself break the budget.
    fn enforce_byte_budget(&mut self) {
        let total_skills = self
            .truncated
            .as_ref()
            .map_or(self.skills.len(), |truncation| truncation.total_skills);

        if self.skills.len() == total_skills {
            self.truncated = Some(ServedTruncation {
                disclosed_skills: self.skills.len(),
                total_skills,
            });
        }

        while !self.skills.is_empty() && self.serialized_len() > MAX_UNSIGNED_CARD_BYTES {
            self.skills.pop();
        }

        self.truncated = (self.skills.len() < total_skills).then_some(ServedTruncation {
            disclosed_skills: self.skills.len(),
            total_skills,
        });
    }

    fn serialized_len(&self) -> usize {
        serde_jcs::to_vec(self)
            .map(|bytes| bytes.len())
            .unwrap_or(usize::MAX)
    }

    /// Declare the authentication this server actually enforces.
    ///
    /// Takes the scheme from [`super::auth::A2aServerAuth::declared_scheme`] —
    /// the *same* function the request middleware is built from — so a card that
    /// advertises `apiKey` while the server accepts anything (or the reverse) is
    /// not expressible (AC7b/R9, mutants a and b).
    pub(crate) fn with_declared_auth(mut self, auth: Option<&super::auth::A2aServerAuth>) -> Self {
        let Some(auth) = auth else {
            return self;
        };
        let (name, scheme) = auth.declared_scheme();
        self.security_schemes.insert(name.to_owned(), scheme);
        self.security
            .push(BTreeMap::from([(name.to_owned(), Vec::new())]));
        self
    }

    pub(crate) fn with_signature(mut self, protected: String, signature: String) -> Self {
        self.signatures.push(ServedSignature {
            protected,
            signature,
        });
        self
    }

    pub(crate) fn with_ownership(mut self, peer_id: String) -> Self {
        self.ownership = Some(ServedOwnership {
            kind: "self",
            peer_id,
        });
        self
    }

    /// Re-apply the cap after every unsigned extension has been added.
    ///
    /// `from_registry` reserves the signature already, so direct signers are
    /// bounded too. The served path calls this after authentication and
    /// ownership fields are present, ensuring their actual serialized cost is
    /// included before `sign_card` appends the reserved signature member.
    pub(crate) fn with_signature_budget_reserved(mut self) -> Self {
        self.enforce_byte_budget();
        self
    }

    /// Skills disclosed by this card (test/observability accessor).
    #[must_use]
    pub fn disclosed_skill_count(&self) -> usize {
        self.skills.len()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentCardView {
    pub name: String,
    pub description: Option<String>,
    pub version: Option<String>,
    pub capabilities: Option<serde_json::Value>,
    pub default_input_modes: Option<Vec<String>>,
    pub default_output_modes: Option<Vec<String>>,
    pub skills: Vec<AgentSkillView>,
    /// Top-level v0.3 endpoint (`url`). Kept raw; resolution validates it.
    pub url: Option<String>,
    /// v0.3 `preferredTransport`. Open string per ADR-17-4a-01 R11.
    pub preferred_transport: Option<String>,
    /// v1.0 `supportedInterfaces[]` block.
    pub supported_interfaces: Option<Vec<SupportedInterface>>,
    /// v0.3 `additionalInterfaces[]` block.
    pub additional_interfaces: Option<Vec<AdditionalInterface>>,
    missing_name: bool,
    missing_skills: bool,
}

/// A v1.0 `supportedInterfaces[]` entry. `protocolBinding` is an open string.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SupportedInterface {
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub protocol_binding: Option<String>,
    #[serde(default)]
    pub protocol_version: Option<String>,
}

/// A v0.3 `additionalInterfaces[]` entry. `transport` is an open string.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdditionalInterface {
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub transport: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentSkillView {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub tags: Option<Vec<String>>,
    pub examples: Option<Vec<String>>,
    missing_id: bool,
    missing_name: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentCardInput {
    name: Option<String>,
    description: Option<String>,
    version: Option<String>,
    capabilities: Option<serde_json::Value>,
    default_input_modes: Option<Vec<String>>,
    default_output_modes: Option<Vec<String>>,
    skills: Option<Vec<AgentSkillInput>>,
    url: Option<String>,
    preferred_transport: Option<String>,
    supported_interfaces: Option<Vec<SupportedInterface>>,
    additional_interfaces: Option<Vec<AdditionalInterface>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentSkillInput {
    id: Option<String>,
    name: Option<String>,
    description: Option<String>,
    tags: Option<Vec<String>>,
    examples: Option<Vec<String>>,
}

impl<'de> Deserialize<'de> for AgentCardView {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let input = AgentCardInput::deserialize(deserializer)?;
        let missing_name = input.name.is_none();
        let missing_skills = input.skills.is_none();
        let skills = input
            .skills
            .unwrap_or_default()
            .into_iter()
            .map(AgentSkillView::from)
            .collect();

        Ok(Self {
            name: input.name.unwrap_or_default(),
            description: input.description,
            version: input.version,
            capabilities: input.capabilities,
            default_input_modes: input.default_input_modes,
            default_output_modes: input.default_output_modes,
            skills,
            url: input.url,
            preferred_transport: input.preferred_transport,
            supported_interfaces: input.supported_interfaces,
            additional_interfaces: input.additional_interfaces,
            missing_name,
            missing_skills,
        })
    }
}

impl From<AgentSkillInput> for AgentSkillView {
    fn from(input: AgentSkillInput) -> Self {
        let missing_id = input.id.is_none();
        let missing_name = input.name.is_none();
        Self {
            id: input.id.unwrap_or_default(),
            name: input.name.unwrap_or_default(),
            description: input.description,
            tags: input.tags,
            examples: input.examples,
            missing_id,
            missing_name,
        }
    }
}

pub fn decode_and_validate(raw: &str) -> Result<AgentCardView, A2aError> {
    let card: AgentCardView = serde_json::from_str(raw)?;
    validate_required(&card)?;
    Ok(card)
}

pub fn validate_required(card: &AgentCardView) -> Result<(), A2aError> {
    if card.missing_name || card.name.trim().is_empty() {
        return Err(malformed("name"));
    }
    if card.missing_skills {
        return Err(malformed("skills"));
    }

    for (index, skill) in card.skills.iter().enumerate() {
        if skill.missing_id || skill.id.trim().is_empty() {
            return Err(malformed(format!("skills[{index}].id")));
        }
        if skill.missing_name || skill.name.trim().is_empty() {
            return Err(malformed(format!("skills[{index}].name")));
        }
    }
    Ok(())
}

fn malformed(field: impl Into<String>) -> A2aError {
    A2aError::MalformedCard {
        field: field.into(),
    }
}
