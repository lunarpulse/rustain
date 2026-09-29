use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashSet};
use std::path::PathBuf;

use super::ToolPolicy;
use crate::domain::services::skill_tool_pattern::{
    allowed_item_matches_tool, parse_allowed_tool_pattern,
};

pub const MAX_AGENT_FILE_SIZE: u64 = 1_048_576;
pub const MAX_AGENT_SCAN_FILES: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDef {
    pub name: String,
    pub description: String,
    pub file: PathBuf,
    pub allowed_tools: Option<Vec<String>>,
    pub exclude_tools: Option<Vec<String>>,
    pub model: Option<String>,
    /// Story 14.5 — run this agent's delegated children in an isolated scratch-dir
    /// clone. `false` by default; set `isolated: true` in an agent file's
    /// frontmatter to opt a specific agent into isolation (the R1 trigger).
    pub isolated: bool,
}

/// The active agent's dispatch-time restriction, including the declared-item
/// representation required when a child inherits from this turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentToolRestriction {
    pub agent_name: String,
    pub policy: ToolPolicy,
    pub declared_items: BTreeSet<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolRestrictionOrigin {
    Skill,
    Agent,
}

/// Per-origin allowlist carve-outs — the single source both enforcers read.
///
/// ⚠ These are **allowlist** carve-outs: they exempt a tool from an allowlist
/// it was never named in. They do NOT override an explicit `exclude-tools`
/// entry — see `permission_chain::restriction_excludes_by_name` (Story 19.28
/// code review, P2).
///
/// ⚠ This table is not the whole set of tools that bypass the restriction
/// gates. `exit_plan_mode` short-circuits ahead of BOTH gates in
/// `permission_chain::check_with_source_and_provenance_and_restriction`
/// (Story 19.28 A5: it is a mode control, not a workspace tool), and it
/// deliberately lives outside this table because it is origin-independent.
/// Adding an entry here will not affect it, and removing one will not reach it.
const SKILL_CARVE_OUTS: &[&str] = &["activate_skill"];
const AGENT_CARVE_OUTS: &[&str] = &["activate_skill", "task"];

/// Single source for allowlist carve-outs. The chain reads one origin; the
/// offer filter reads the union.
pub fn allowlist_carve_outs(origin: ToolRestrictionOrigin) -> &'static [&'static str] {
    match origin {
        ToolRestrictionOrigin::Skill => SKILL_CARVE_OUTS,
        ToolRestrictionOrigin::Agent => AGENT_CARVE_OUTS,
    }
}

pub fn is_allowlist_carve_out(origin: ToolRestrictionOrigin, tool_name: &str) -> bool {
    allowlist_carve_outs(origin).contains(&tool_name)
}

pub fn is_any_allowlist_carve_out(tool_name: &str) -> bool {
    [ToolRestrictionOrigin::Skill, ToolRestrictionOrigin::Agent]
        .into_iter()
        .any(|origin| is_allowlist_carve_out(origin, tool_name))
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ActiveAgent {
    pub name: String,
    pub file: PathBuf,
    pub body: String,
    pub allowed_tools: Option<Vec<String>>,
    pub exclude_tools: Option<Vec<String>>,
    pub model: Option<String>,
}

#[allow(dead_code)]
impl ActiveAgent {
    pub fn effective_tool_filter(&self, all_tool_names: &[String]) -> Option<HashSet<String>> {
        if self.allowed_tools.is_none() && self.exclude_tools.is_none() {
            return None;
        }
        Some(
            all_tool_names
                .iter()
                .filter(|tool_name| {
                    let allowed = self.allowed_tools.as_ref().is_none_or(|items| {
                        items
                            .iter()
                            .any(|item| allowed_item_matches_tool(item, tool_name))
                    });
                    let excluded = self.exclude_tools.as_ref().is_some_and(|items| {
                        items
                            .iter()
                            .any(|item| excluded_item_names_tool(item, tool_name))
                    });
                    allowed && !excluded
                })
                .cloned()
                .collect(),
        )
    }

    pub fn tool_restriction(&self, all_tool_names: &[String]) -> Option<AgentToolRestriction> {
        let policy = tool_policy_from_lists(&self.allowed_tools, &self.exclude_tools);
        if policy == ToolPolicy::InheritFromParent {
            return None;
        }
        let declared_items = match &policy {
            ToolPolicy::Allowlist { tools } => tools.clone(),
            ToolPolicy::Denylist { tools } => all_tool_names
                .iter()
                .filter(|tool_name| {
                    !tools
                        .iter()
                        .any(|item| excluded_item_names_tool(item, tool_name))
                })
                .cloned()
                .collect(),
            ToolPolicy::InheritFromParent | ToolPolicy::ResolvedAgainstParent { .. } => {
                unreachable!("active agents are never pre-resolved")
            }
        };
        Some(AgentToolRestriction {
            agent_name: self.name.clone(),
            policy,
            declared_items,
        })
    }
}

impl AgentDef {
    pub fn tool_policy(&self) -> ToolPolicy {
        tool_policy_from_lists(&self.allowed_tools, &self.exclude_tools)
    }
}

fn tool_policy_from_lists(
    allowed_tools: &Option<Vec<String>>,
    exclude_tools: &Option<Vec<String>>,
) -> ToolPolicy {
    if let Some(allow) = allowed_tools.as_ref() {
        let tools = allow
            .iter()
            .filter(|allowed| {
                let tool_name = parse_allowed_tool_pattern(allowed)
                    .map(|pattern| pattern.tool_name)
                    .unwrap_or(allowed);
                !exclude_tools.as_ref().is_some_and(|excluded| {
                    excluded
                        .iter()
                        .any(|item| excluded_item_names_tool(item, tool_name))
                })
            })
            .cloned()
            .collect();
        return ToolPolicy::Allowlist { tools };
    }
    if let Some(tools) = exclude_tools.as_ref() {
        return ToolPolicy::Denylist {
            tools: tools.iter().cloned().collect(),
        };
    }
    ToolPolicy::InheritFromParent
}

pub(crate) fn excluded_item_names_tool(item: &str, tool_name: &str) -> bool {
    parse_allowed_tool_pattern(item)
        .map(|pattern| pattern.tool_name)
        .unwrap_or(item)
        == tool_name
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentValidationError {
    MissingName,
    InvalidName(String),
    MissingDescription,
    DescriptionTooLong(usize),
    NameMismatch { declared: String, expected: String },
}

impl std::fmt::Display for AgentValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AgentValidationError::MissingName => write!(f, "missing required field 'name'"),
            AgentValidationError::InvalidName(name) => {
                write!(
                    f,
                    "name '{}' does not match pattern ^[a-z0-9][a-z0-9-]{{0,63}}$",
                    name
                )
            }
            AgentValidationError::MissingDescription => {
                write!(f, "missing required field 'description'")
            }
            AgentValidationError::DescriptionTooLong(len) => {
                write!(f, "description too long ({} bytes, max 1024)", len)
            }
            AgentValidationError::NameMismatch { declared, expected } => {
                write!(
                    f,
                    "name '{}' does not match file stem '{}'",
                    declared, expected
                )
            }
        }
    }
}

impl std::error::Error for AgentValidationError {}

impl AgentDef {
    /// Story 10.7 — synthetic default worker agent definition.
    pub fn default_worker() -> Self {
        Self {
            name: "default".to_string(),
            description: "Default worker agent".to_string(),
            file: PathBuf::new(),
            allowed_tools: None,
            exclude_tools: None,
            model: None,
            isolated: false,
        }
    }
}

pub fn validate_agent_frontmatter(
    name: &str,
    description: &str,
    expected_name: &str,
) -> Result<(), AgentValidationError> {
    if name.is_empty() {
        return Err(AgentValidationError::MissingName);
    }
    if name.len() > 64 {
        return Err(AgentValidationError::InvalidName(name.to_string()));
    }
    let valid_name_pattern = name.chars().enumerate().all(|(i, c)| {
        if i == 0 {
            c.is_ascii_lowercase() || c.is_ascii_digit()
        } else {
            c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'
        }
    });
    if !valid_name_pattern {
        return Err(AgentValidationError::InvalidName(name.to_string()));
    }
    let description = description.trim();
    if description.is_empty() {
        return Err(AgentValidationError::MissingDescription);
    }
    if description.len() > 1024 {
        return Err(AgentValidationError::DescriptionTooLong(description.len()));
    }
    if name != expected_name {
        return Err(AgentValidationError::NameMismatch {
            declared: name.to_string(),
            expected: expected_name.to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_frontmatter() {
        assert!(
            validate_agent_frontmatter("code-reviewer", "Reviews code", "code-reviewer").is_ok()
        );
    }

    #[test]
    fn empty_name_rejected() {
        assert!(matches!(
            validate_agent_frontmatter("", "desc", "foo"),
            Err(AgentValidationError::MissingName)
        ));
    }

    #[test]
    fn name_too_long() {
        let long_name = "a".repeat(65);
        assert!(matches!(
            validate_agent_frontmatter(&long_name, "desc", &long_name),
            Err(AgentValidationError::InvalidName(_))
        ));
    }

    #[test]
    fn uppercase_name_rejected() {
        assert!(matches!(
            validate_agent_frontmatter("Foo", "desc", "Foo"),
            Err(AgentValidationError::InvalidName(_))
        ));
    }

    #[test]
    fn name_with_spaces_rejected() {
        assert!(matches!(
            validate_agent_frontmatter("foo bar", "desc", "foo bar"),
            Err(AgentValidationError::InvalidName(_))
        ));
    }

    #[test]
    fn name_starting_with_hyphen_rejected() {
        assert!(matches!(
            validate_agent_frontmatter("-foo", "desc", "-foo"),
            Err(AgentValidationError::InvalidName(_))
        ));
    }

    #[test]
    fn empty_description_rejected() {
        assert!(matches!(
            validate_agent_frontmatter("foo", "", "foo"),
            Err(AgentValidationError::MissingDescription)
        ));
    }

    #[test]
    fn description_too_long() {
        let long_desc = "x".repeat(1025);
        assert!(matches!(
            validate_agent_frontmatter("foo", &long_desc, "foo"),
            Err(AgentValidationError::DescriptionTooLong(1025))
        ));
    }

    #[test]
    fn name_mismatch_rejected() {
        assert!(matches!(
            validate_agent_frontmatter("bar", "desc", "foo"),
            Err(AgentValidationError::NameMismatch { .. })
        ));
    }

    #[test]
    fn name_64_chars_accepted() {
        let name = "a".repeat(64);
        assert!(validate_agent_frontmatter(&name, "desc", &name).is_ok());
    }

    #[test]
    fn description_1024_chars_accepted() {
        let desc = "x".repeat(1024);
        assert!(validate_agent_frontmatter("foo", &desc, "foo").is_ok());
    }

    #[test]
    fn name_with_hyphens_accepted() {
        assert!(validate_agent_frontmatter("my-agent-123", "desc", "my-agent-123").is_ok());
    }

    #[test]
    fn effective_tool_filter_allow_only() {
        let agent = ActiveAgent {
            name: "test".to_string(),
            file: PathBuf::from("/tmp/test.md"),
            body: String::new(),
            allowed_tools: Some(vec!["Read".to_string(), "Grep".to_string()]),
            exclude_tools: None,
            model: None,
        };
        let all = vec!["Read".to_string(), "Grep".to_string(), "Bash".to_string()];
        let filter = agent.effective_tool_filter(&all).unwrap();
        assert!(filter.contains("Read"));
        assert!(filter.contains("Grep"));
        assert!(!filter.contains("Bash"));
    }

    #[test]
    fn effective_tool_filter_exclude_only() {
        let agent = ActiveAgent {
            name: "test".to_string(),
            file: PathBuf::from("/tmp/test.md"),
            body: String::new(),
            allowed_tools: None,
            exclude_tools: Some(vec!["Bash".to_string()]),
            model: None,
        };
        let all = vec!["Read".to_string(), "Grep".to_string(), "Bash".to_string()];
        let filter = agent.effective_tool_filter(&all).unwrap();
        assert!(filter.contains("Read"));
        assert!(filter.contains("Grep"));
        assert!(!filter.contains("Bash"));
    }

    #[test]
    fn effective_tool_filter_both() {
        let agent = ActiveAgent {
            name: "test".to_string(),
            file: PathBuf::from("/tmp/test.md"),
            body: String::new(),
            allowed_tools: Some(vec!["Read".to_string(), "Bash".to_string()]),
            exclude_tools: Some(vec!["Bash".to_string()]),
            model: None,
        };
        let all = vec!["Read".to_string(), "Bash".to_string(), "Grep".to_string()];
        let filter = agent.effective_tool_filter(&all).unwrap();
        assert!(filter.contains("Read"));
        assert!(!filter.contains("Bash"));
        assert!(!filter.contains("Grep"));
    }

    #[test]
    fn effective_tool_filter_neither() {
        let agent = ActiveAgent {
            name: "test".to_string(),
            file: PathBuf::from("/tmp/test.md"),
            body: String::new(),
            allowed_tools: None,
            exclude_tools: None,
            model: None,
        };
        let all = vec!["Read".to_string()];
        assert!(agent.effective_tool_filter(&all).is_none());
    }

    #[test]
    fn constants_match_spec() {
        assert_eq!(MAX_AGENT_FILE_SIZE, 1_048_576);
        assert_eq!(MAX_AGENT_SCAN_FILES, 100);
    }

    #[test]
    fn error_display_formats() {
        assert_eq!(
            AgentValidationError::MissingName.to_string(),
            "missing required field 'name'"
        );
        assert!(
            AgentValidationError::InvalidName("Bad".to_string())
                .to_string()
                .contains("Bad")
        );
        assert!(
            AgentValidationError::NameMismatch {
                declared: "bar".to_string(),
                expected: "foo".to_string(),
            }
            .to_string()
            .contains("bar")
        );
    }
}
