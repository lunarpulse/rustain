use std::collections::HashSet;
use std::path::PathBuf;

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
        match (&self.allowed_tools, &self.exclude_tools) {
            (None, None) => None,
            (Some(allow), None) => {
                let set: HashSet<String> = allow.iter().cloned().collect();
                Some(set)
            }
            (None, Some(exclude)) => {
                let exclude_set: HashSet<String> = exclude.iter().cloned().collect();
                let set: HashSet<String> = all_tool_names
                    .iter()
                    .filter(|t| !exclude_set.contains(*t))
                    .cloned()
                    .collect();
                Some(set)
            }
            (Some(allow), Some(exclude)) => {
                let exclude_set: HashSet<String> = exclude.iter().cloned().collect();
                let set: HashSet<String> = allow
                    .iter()
                    .filter(|t| !exclude_set.contains(*t))
                    .cloned()
                    .collect();
                Some(set)
            }
        }
    }
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
