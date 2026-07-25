//! Version-stable AgentCard discovery view.

use serde::{Deserialize, Deserializer, Serialize};

use super::error::A2aError;

use crate::domain::models::capability_registry::CapabilityRegistry;

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
    #[serde(
        rename = "x-rustain-ownership",
        skip_serializing_if = "Option::is_none"
    )]
    ownership: Option<ServedOwnership>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    signatures: Vec<ServedSignature>,
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

        Self {
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
            ownership: None,
            signatures: Vec::new(),
        }
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
