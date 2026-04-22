use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub const MAX_SKILL_FILE_SIZE: u64 = 1_048_576;
pub const MAX_SKILL_ACTIVATION_DEPTH: u8 = 3;

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

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ActiveSkill {
    pub name: String,
    pub directory: PathBuf,
    pub allowed_tools: Option<Vec<String>>,
    pub body: String,
    pub arguments: String,
    pub activation_depth: u8,
    pub source: SkillSource,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct SkillActivationSet {
    active: Vec<ActiveSkill>,
    /// Trust decisions keyed on the skill file **path**, not its name. Prevents
    /// a workspace-tier skill from inheriting trust by sharing a name with a
    /// global-tier skill (or vice-versa). Canonicalization is the adapter's
    /// responsibility — domain is I/O-free.
    trusted_in_session: HashSet<PathBuf>,
}

#[allow(dead_code)]
impl SkillActivationSet {
    pub fn new() -> Self {
        Self {
            active: Vec::new(),
            trusted_in_session: HashSet::new(),
        }
    }

    pub fn push(&mut self, skill: ActiveSkill) {
        if let Some(existing) = self.active.iter_mut().find(|s| s.name == skill.name) {
            existing.body = skill.body;
            existing.arguments = skill.arguments;
        } else {
            self.active.push(skill);
        }
    }

    pub fn deactivate(&mut self, name: &str) -> Option<ActiveSkill> {
        let idx = self.active.iter().position(|s| s.name == name)?;
        Some(self.active.remove(idx))
    }

    pub fn deactivate_all(&mut self) -> Vec<ActiveSkill> {
        std::mem::take(&mut self.active)
    }

    pub fn effective_allowed_tools(&self) -> Option<HashSet<String>> {
        let constrained: Vec<&Vec<String>> = self
            .active
            .iter()
            .filter_map(|s| s.allowed_tools.as_ref())
            .collect();
        if constrained.is_empty() {
            return None;
        }
        let mut iter = constrained.iter();
        let Some(first) = iter.next() else {
            return Some(HashSet::new());
        };
        let mut result: HashSet<String> = first.iter().cloned().collect();
        for set in iter {
            let other: HashSet<String> = set.iter().cloned().collect();
            result = result.intersection(&other).cloned().collect();
        }
        Some(result)
    }

    #[allow(dead_code)]
    pub fn active_names(&self) -> Vec<String> {
        self.active.iter().map(|s| s.name.clone()).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.active.is_empty()
    }

    pub fn active_skills(&self) -> &[ActiveSkill] {
        &self.active
    }

    pub fn is_trusted(&self, path: &Path) -> bool {
        self.trusted_in_session.contains(path)
    }

    pub fn mark_trusted(&mut self, path: PathBuf) {
        self.trusted_in_session.insert(path);
    }

    pub fn active_count(&self) -> usize {
        self.active.len()
    }
}

impl Default for SkillActivationSet {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub enum SkillActivationError {
    NotFound(String),
    Disabled(String),
    DepthExceeded {
        depth: u8,
        max: u8,
    },
    /// Skill file was discovered but has since been removed. Carries the skill
    /// `name` so the canonical AC1 message (`✗ Skill '{name}' file no longer exists at '{path}'`)
    /// can be rendered without requiring the adapter to look up the name separately.
    FileMissing {
        name: String,
        path: PathBuf,
    },
    /// Skill file exceeds `MAX_SKILL_FILE_SIZE`. Carries the skill `name` so the
    /// canonical AC1 message (`✗ Skill '{name}' exceeds 1 MiB — refusing to load`)
    /// can be rendered by the adapter/UI layer.
    FileTooLarge {
        name: String,
        size: u64,
    },
    BodyReadFailed(String),
}

/// Outcome of a trust-gated activation (Decision 4, Group 1).
///
/// `activate_by_name` returns `Result<SkillActivationOutcome, SkillActivationError>`:
/// - `Ok(Activated)` on success.
/// - `Ok(TrustDeclined)` when the user (or a dropped oneshot) declines trust —
///   this is not a *failure*, it is a user choice, so the adapter surfaces it
///   as an Info feedback per AC4 rather than an Error.
/// - `Err(_)` for mechanical errors (missing file, disabled, depth exceeded…).
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum SkillActivationOutcome {
    Activated(ActiveSkill),
    TrustDeclined(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillTrustResponse {
    Accepted,
    Declined,
}

impl std::fmt::Display for SkillActivationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SkillActivationError::NotFound(name) => {
                write!(f, "Skill not found: {}", name)
            }
            SkillActivationError::Disabled(name) => {
                write!(f, "Skill '{}' is disabled by config", name)
            }
            SkillActivationError::DepthExceeded { max, .. } => {
                write!(
                    f,
                    "Skill activation depth limit ({}) exceeded. Deactivate some skills before activating more.",
                    max
                )
            }
            SkillActivationError::FileMissing { name, path } => {
                write!(
                    f,
                    "✗ Skill '{}' file no longer exists at '{}'",
                    name,
                    path.display()
                )
            }
            SkillActivationError::FileTooLarge { name, .. } => {
                write!(f, "✗ Skill '{}' exceeds 1 MiB — refusing to load", name)
            }
            SkillActivationError::BodyReadFailed(msg) => {
                write!(f, "Failed to read skill body: {}", msg)
            }
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

    #[test]
    fn activation_depth_constant() {
        assert_eq!(MAX_SKILL_ACTIVATION_DEPTH, 3);
    }

    #[test]
    fn effective_allowed_tools_none_plus_none() {
        let set = SkillActivationSet::new();
        assert!(set.effective_allowed_tools().is_none());
    }

    #[test]
    fn effective_allowed_tools_some_plus_none() {
        let mut set = SkillActivationSet::new();
        set.push(ActiveSkill {
            name: "a".to_string(),
            directory: PathBuf::from("/tmp/a"),
            allowed_tools: Some(vec!["Read".to_string()]),
            body: String::new(),
            arguments: String::new(),
            activation_depth: 1,
            source: SkillSource::GlobalAgents,
        });
        set.push(ActiveSkill {
            name: "b".to_string(),
            directory: PathBuf::from("/tmp/b"),
            allowed_tools: None,
            body: String::new(),
            arguments: String::new(),
            activation_depth: 2,
            source: SkillSource::GlobalAgents,
        });
        let effective = set.effective_allowed_tools().unwrap();
        assert!(effective.contains("Read"));
    }

    #[test]
    fn effective_allowed_tools_intersection() {
        let mut set = SkillActivationSet::new();
        set.push(ActiveSkill {
            name: "a".to_string(),
            directory: PathBuf::from("/tmp/a"),
            allowed_tools: Some(vec!["Read".to_string(), "Grep".to_string()]),
            body: String::new(),
            arguments: String::new(),
            activation_depth: 1,
            source: SkillSource::GlobalAgents,
        });
        set.push(ActiveSkill {
            name: "b".to_string(),
            directory: PathBuf::from("/tmp/b"),
            allowed_tools: Some(vec!["Read".to_string(), "Write".to_string()]),
            body: String::new(),
            arguments: String::new(),
            activation_depth: 2,
            source: SkillSource::GlobalAgents,
        });
        let effective = set.effective_allowed_tools().unwrap();
        assert!(effective.contains("Read"));
        assert!(!effective.contains("Grep"));
        assert!(!effective.contains("Write"));
    }

    #[test]
    fn effective_allowed_tools_empty_collapses() {
        let mut set = SkillActivationSet::new();
        set.push(ActiveSkill {
            name: "a".to_string(),
            directory: PathBuf::from("/tmp/a"),
            allowed_tools: Some(vec![]),
            body: String::new(),
            arguments: String::new(),
            activation_depth: 1,
            source: SkillSource::GlobalAgents,
        });
        set.push(ActiveSkill {
            name: "b".to_string(),
            directory: PathBuf::from("/tmp/b"),
            allowed_tools: Some(vec!["Read".to_string()]),
            body: String::new(),
            arguments: String::new(),
            activation_depth: 2,
            source: SkillSource::GlobalAgents,
        });
        let effective = set.effective_allowed_tools().unwrap();
        assert!(effective.is_empty());
    }

    #[test]
    fn effective_allowed_tools_disjoint_yields_empty() {
        let mut set = SkillActivationSet::new();
        set.push(ActiveSkill {
            name: "a".to_string(),
            directory: PathBuf::from("/tmp/a"),
            allowed_tools: Some(vec!["Read".to_string()]),
            body: String::new(),
            arguments: String::new(),
            activation_depth: 1,
            source: SkillSource::GlobalAgents,
        });
        set.push(ActiveSkill {
            name: "b".to_string(),
            directory: PathBuf::from("/tmp/b"),
            allowed_tools: Some(vec!["Bash".to_string()]),
            body: String::new(),
            arguments: String::new(),
            activation_depth: 2,
            source: SkillSource::GlobalAgents,
        });
        let effective = set.effective_allowed_tools().unwrap();
        assert!(effective.is_empty());
    }

    #[test]
    fn deactivate_removes_skill() {
        let mut set = SkillActivationSet::new();
        set.push(ActiveSkill {
            name: "a".to_string(),
            directory: PathBuf::from("/tmp/a"),
            allowed_tools: None,
            body: String::new(),
            arguments: String::new(),
            activation_depth: 1,
            source: SkillSource::GlobalAgents,
        });
        let removed = set.deactivate("a").unwrap();
        assert_eq!(removed.name, "a");
        assert!(set.is_empty());
    }

    #[test]
    fn deactivate_unknown_returns_none() {
        let mut set = SkillActivationSet::new();
        assert!(set.deactivate("nope").is_none());
    }

    #[test]
    fn deactivate_all_returns_all() {
        let mut set = SkillActivationSet::new();
        set.push(ActiveSkill {
            name: "a".to_string(),
            directory: PathBuf::from("/tmp/a"),
            allowed_tools: None,
            body: String::new(),
            arguments: String::new(),
            activation_depth: 1,
            source: SkillSource::GlobalAgents,
        });
        set.push(ActiveSkill {
            name: "b".to_string(),
            directory: PathBuf::from("/tmp/b"),
            allowed_tools: None,
            body: String::new(),
            arguments: String::new(),
            activation_depth: 2,
            source: SkillSource::GlobalAgents,
        });
        let all = set.deactivate_all();
        assert_eq!(all.len(), 2);
        assert!(set.is_empty());
    }

    #[test]
    fn trust_session_sticky_by_path_not_name() {
        // Decision 1 (Group 1): trust is keyed on skill file path so that
        // a workspace-tier skill named `review` cannot inherit trust granted
        // to a global-tier skill named `review`.
        let mut set = SkillActivationSet::new();
        let global = PathBuf::from("/home/u/.claude/skills/review/SKILL.md");
        let workspace = PathBuf::from("/repo/.agents/skills/review/SKILL.md");
        assert!(!set.is_trusted(&global));
        assert!(!set.is_trusted(&workspace));
        set.mark_trusted(global.clone());
        assert!(set.is_trusted(&global));
        // Same name, different path — must NOT be trusted.
        assert!(!set.is_trusted(&workspace));
    }

    #[test]
    fn push_refreshes_body_and_args_on_reactivation() {
        let mut set = SkillActivationSet::new();
        set.push(ActiveSkill {
            name: "a".to_string(),
            directory: PathBuf::from("/tmp/a"),
            allowed_tools: None,
            body: "v1".to_string(),
            arguments: "--old".to_string(),
            activation_depth: 1,
            source: SkillSource::GlobalAgents,
        });
        set.push(ActiveSkill {
            name: "a".to_string(),
            directory: PathBuf::from("/tmp/a"),
            allowed_tools: None,
            body: "v2".to_string(),
            arguments: "--new".to_string(),
            activation_depth: 3,
            source: SkillSource::GlobalAgents,
        });
        assert_eq!(set.active_count(), 1);
        assert_eq!(set.active_skills()[0].body, "v2");
        assert_eq!(set.active_skills()[0].arguments, "--new");
        assert_eq!(
            set.active_skills()[0].activation_depth,
            1,
            "depth preserved from original activation per AC10"
        );
    }

    #[test]
    fn activation_error_display_formats() {
        assert_eq!(
            SkillActivationError::NotFound("foo".to_string()).to_string(),
            "Skill not found: foo"
        );
        assert!(
            SkillActivationError::DepthExceeded { depth: 4, max: 3 }
                .to_string()
                .contains("depth limit (3)")
        );
        // TrustDeclined was moved to `SkillActivationOutcome::TrustDeclined`
        // (Decision 4) — decline is a user choice, not an error, so no Display
        // assertion here.

        // AC1 canonical message: `✗ Skill '{name}' exceeds 1 MiB — refusing to load`
        assert_eq!(
            SkillActivationError::FileTooLarge {
                name: "big".to_string(),
                size: 2_000_000,
            }
            .to_string(),
            "✗ Skill 'big' exceeds 1 MiB — refusing to load"
        );

        // AC1 canonical message: `✗ Skill '{name}' file no longer exists at '{path}'`
        let missing = SkillActivationError::FileMissing {
            name: "ghost".to_string(),
            path: PathBuf::from("/tmp/ghost/SKILL.md"),
        }
        .to_string();
        assert!(missing.starts_with("✗ Skill 'ghost' file no longer exists at '"));
        assert!(missing.ends_with("/tmp/ghost/SKILL.md'"));
    }
}
