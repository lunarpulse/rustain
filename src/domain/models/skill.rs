use std::path::PathBuf;

pub const MAX_SKILL_FILE_SIZE: u64 = 1_048_576;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillDef {
    pub name: String,
    pub description: String,
    pub file: PathBuf,
    pub directory: PathBuf,
    pub source: SkillSource,
    pub allowed_tools: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SkillSource {
    WorkspaceAgents,
    WorkspaceRustain,
    WorkspaceClaude,
    GlobalAgents,
}

impl SkillSource {
    pub fn priority(&self) -> u8 {
        match self {
            SkillSource::WorkspaceAgents => 0,
            SkillSource::WorkspaceRustain => 1,
            SkillSource::WorkspaceClaude => 2,
            SkillSource::GlobalAgents => 3,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub enum SkillValidationError {
    MissingFrontmatter,
    MissingField(String),
    NameTooLong(usize),
    NameInvalid(String),
    DescriptionTooLong(usize),
    NameDirectoryMismatch { name: String, dir: String },
    IoError(String),
}

impl std::fmt::Display for SkillValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SkillValidationError::MissingFrontmatter => write!(f, "missing YAML frontmatter"),
            SkillValidationError::MissingField(field) => {
                write!(f, "missing required field '{}'", field)
            }
            SkillValidationError::NameTooLong(len) => {
                write!(f, "name too long ({} chars, max 64)", len)
            }
            SkillValidationError::NameInvalid(name) => {
                write!(
                    f,
                    "name '{}' does not match pattern ^[a-z0-9][a-z0-9-]*$",
                    name
                )
            }
            SkillValidationError::DescriptionTooLong(len) => {
                write!(f, "description too long ({} chars, max 1024)", len)
            }
            SkillValidationError::NameDirectoryMismatch { name, dir } => {
                write!(f, "name '{}' does not match directory '{}'", name, dir)
            }
            SkillValidationError::IoError(msg) => write!(f, "I/O error: {}", msg),
        }
    }
}

#[allow(dead_code)]
pub fn validate_skill_frontmatter(
    name: &str,
    description: &str,
    expected_name: &str,
) -> Result<(), SkillValidationError> {
    if name.is_empty() {
        return Err(SkillValidationError::MissingField("name".to_string()));
    }
    if name.len() > 64 {
        return Err(SkillValidationError::NameTooLong(name.len()));
    }
    let valid_name_pattern = name.chars().enumerate().all(|(i, c)| {
        if i == 0 {
            c.is_ascii_lowercase() || c.is_ascii_digit()
        } else {
            c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'
        }
    });
    if !valid_name_pattern {
        return Err(SkillValidationError::NameInvalid(name.to_string()));
    }
    if description.is_empty() {
        return Err(SkillValidationError::MissingField(
            "description".to_string(),
        ));
    }
    if description.len() > 1024 {
        return Err(SkillValidationError::DescriptionTooLong(description.len()));
    }
    if name != expected_name {
        return Err(SkillValidationError::NameDirectoryMismatch {
            name: name.to_string(),
            dir: expected_name.to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_frontmatter() {
        assert!(validate_skill_frontmatter("review-code", "Reviews code", "review-code").is_ok());
    }

    #[test]
    fn empty_name_rejected() {
        assert!(matches!(
            validate_skill_frontmatter("", "desc", "foo"),
            Err(SkillValidationError::MissingField(_))
        ));
    }

    #[test]
    fn name_too_long() {
        let long_name = "a".repeat(65);
        assert!(matches!(
            validate_skill_frontmatter(&long_name, "desc", &long_name),
            Err(SkillValidationError::NameTooLong(65))
        ));
    }

    #[test]
    fn uppercase_name_rejected() {
        assert!(matches!(
            validate_skill_frontmatter("Foo", "desc", "Foo"),
            Err(SkillValidationError::NameInvalid(_))
        ));
    }

    #[test]
    fn name_with_spaces_rejected() {
        assert!(matches!(
            validate_skill_frontmatter("foo bar", "desc", "foo bar"),
            Err(SkillValidationError::NameInvalid(_))
        ));
    }

    #[test]
    fn description_too_long() {
        let long_desc = "x".repeat(1025);
        assert!(matches!(
            validate_skill_frontmatter("foo", &long_desc, "foo"),
            Err(SkillValidationError::DescriptionTooLong(1025))
        ));
    }

    #[test]
    fn name_directory_mismatch() {
        assert!(matches!(
            validate_skill_frontmatter("bar", "desc", "foo"),
            Err(SkillValidationError::NameDirectoryMismatch { .. })
        ));
    }

    #[test]
    fn empty_description_rejected() {
        assert!(matches!(
            validate_skill_frontmatter("foo", "", "foo"),
            Err(SkillValidationError::MissingField(_))
        ));
    }

    #[test]
    fn skill_source_priority_order() {
        assert!(SkillSource::WorkspaceAgents.priority() < SkillSource::WorkspaceRustain.priority());
        assert!(SkillSource::WorkspaceRustain.priority() < SkillSource::WorkspaceClaude.priority());
        assert!(SkillSource::WorkspaceClaude.priority() < SkillSource::GlobalAgents.priority());
    }

    #[test]
    fn name_64_chars_accepted() {
        let name = "a".repeat(64);
        assert!(validate_skill_frontmatter(&name, "desc", &name).is_ok());
    }

    #[test]
    fn description_1024_chars_accepted() {
        let desc = "x".repeat(1024);
        assert!(validate_skill_frontmatter("foo", &desc, "foo").is_ok());
    }

    #[test]
    fn name_with_hyphens_accepted() {
        assert!(validate_skill_frontmatter("my-skill-123", "desc", "my-skill-123").is_ok());
    }

    #[test]
    fn name_starting_with_hyphen_rejected() {
        assert!(matches!(
            validate_skill_frontmatter("-foo", "desc", "-foo"),
            Err(SkillValidationError::NameInvalid(_))
        ));
    }
}
