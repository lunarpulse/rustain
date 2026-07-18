//! Version-stable AgentCard discovery view.

use serde::{Deserialize, Deserializer};

use super::error::A2aError;

#[derive(Debug, Clone, PartialEq)]
pub struct AgentCardView {
    pub name: String,
    pub description: Option<String>,
    pub version: Option<String>,
    pub capabilities: Option<serde_json::Value>,
    pub default_input_modes: Option<Vec<String>>,
    pub default_output_modes: Option<Vec<String>>,
    pub skills: Vec<AgentSkillView>,
    missing_name: bool,
    missing_skills: bool,
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
