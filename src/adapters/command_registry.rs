use std::path::{Path, PathBuf};

/// Source of a slash command — built-in or user-defined.
#[derive(Debug, Clone)]
pub enum CommandSource {
    BuiltIn,
    #[allow(dead_code)]
    UserDefined {
        path: PathBuf,
    },
}

/// Definition of a slash command.
#[derive(Debug, Clone)]
pub struct SlashCommandDef {
    pub name: String,
    pub description: String,
    pub source: CommandSource,
    /// Full content of the command file (for user-defined commands).
    pub content: Option<String>,
}

/// Registry of available slash commands (built-in + user-defined).
/// User-defined commands are discovered lazily on first `/` keystroke.
pub struct CommandRegistry {
    commands: Vec<SlashCommandDef>,
    discovered: bool,
}

impl CommandRegistry {
    /// Create a new registry with only built-in commands.
    pub fn new() -> Self {
        let commands = vec![
            // Built-in: /new
            SlashCommandDef {
                name: "new".to_string(),
                description: "Start a new conversation".to_string(),
                source: CommandSource::BuiltIn,
                content: None,
            },
        ];
        Self {
            commands,
            discovered: false,
        }
    }

    /// Whether user-defined commands have been discovered.
    pub fn is_discovered(&self) -> bool {
        self.discovered
    }

    /// Register a user-defined command manually.
    #[allow(dead_code)]
    pub fn register_user_command(
        &mut self,
        name: String,
        description: String,
        path: PathBuf,
        content: Option<String>,
    ) {
        self.commands.push(SlashCommandDef {
            name,
            description,
            source: CommandSource::UserDefined { path },
            content,
        });
    }

    /// Discover user-defined commands from `.claude/commands/` in the workspace.
    /// Returns a new registry with built-in + discovered commands.
    #[allow(dead_code)]
    pub fn discover_commands(workspace: &Path) -> Self {
        let mut registry = Self::new();
        let cmd_dir = workspace.join(".claude").join("commands");

        if cmd_dir.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&cmd_dir) {
                let mut user_commands: Vec<SlashCommandDef> = Vec::new();

                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().is_some_and(|ext| ext == "md") {
                        if let Some(cmd) = parse_command_file(&path) {
                            user_commands.push(cmd);
                        }
                    }
                }

                // Sort user-defined commands alphabetically
                user_commands.sort_by(|a, b| a.name.cmp(&b.name));
                registry.commands.extend(user_commands);
            }
        }

        registry.discovered = true;
        registry
    }

    /// Load user-defined commands into an existing registry (for lazy loading).
    pub fn discover_into(&mut self, workspace: &Path) {
        if self.discovered {
            return;
        }
        let cmd_dir = workspace.join(".claude").join("commands");

        if cmd_dir.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&cmd_dir) {
                let mut user_commands: Vec<SlashCommandDef> = Vec::new();

                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().is_some_and(|ext| ext == "md") {
                        if let Some(cmd) = parse_command_file(&path) {
                            user_commands.push(cmd);
                        }
                    }
                }

                // Sort user-defined commands alphabetically
                user_commands.sort_by(|a, b| a.name.cmp(&b.name));
                self.commands.extend(user_commands);
            }
        }

        self.discovered = true;
    }

    /// Filter commands by case-insensitive substring match on name.
    /// Returns built-in first, then user-defined alphabetically.
    pub fn filter(&self, query: &str) -> Vec<&SlashCommandDef> {
        let lower_query = query.to_lowercase();
        let mut builtins: Vec<&SlashCommandDef> = Vec::new();
        let mut user_defined: Vec<&SlashCommandDef> = Vec::new();

        for cmd in &self.commands {
            if lower_query.is_empty() || cmd.name.to_lowercase().contains(&lower_query) {
                match cmd.source {
                    CommandSource::BuiltIn => builtins.push(cmd),
                    CommandSource::UserDefined { .. } => user_defined.push(cmd),
                }
            }
        }

        // Sort user-defined alphabetically by name
        user_defined.sort_by(|a, b| a.name.cmp(&b.name));
        // Built-in first, then user-defined
        builtins.extend(user_defined);
        builtins
    }

    /// Find a command by exact name.
    pub fn find(&self, name: &str) -> Option<&SlashCommandDef> {
        self.commands.iter().find(|c| c.name == name)
    }
}

impl Default for CommandRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse a command `.md` file into a SlashCommandDef.
/// Maximum command file size (1 MB).
const MAX_COMMAND_FILE_SIZE: u64 = 1_048_576;

fn parse_command_file(path: &Path) -> Option<SlashCommandDef> {
    let name = path.file_stem()?.to_string_lossy().to_string();
    // Guard against oversized command files
    let metadata = std::fs::metadata(path).ok()?;
    if metadata.len() > MAX_COMMAND_FILE_SIZE {
        tracing::warn!(
            "Skipping command file {} ({} bytes exceeds {} limit)",
            path.display(),
            metadata.len(),
            MAX_COMMAND_FILE_SIZE
        );
        return None;
    }
    let content = std::fs::read_to_string(path).ok()?;

    let (description, body) = parse_frontmatter_and_content(&content);

    Some(SlashCommandDef {
        name,
        description,
        source: CommandSource::UserDefined {
            path: path.to_path_buf(),
        },
        content: Some(body),
    })
}

/// Parse optional YAML frontmatter for `description:` field.
/// Returns (description, full_content_after_frontmatter).
fn parse_frontmatter_and_content(content: &str) -> (String, String) {
    if let Some(after_first) = content
        .strip_prefix("---\r\n")
        .or_else(|| content.strip_prefix("---\n"))
    {
        if let Some(end_idx) = after_first.find("\n---") {
            let frontmatter = &after_first[..end_idx];
            let body_start = end_idx + 4; // skip "\n---"
            let body = after_first[body_start..]
                .trim_start_matches(['\n', '\r'])
                .to_string();

            // Extract description from frontmatter
            let description = frontmatter
                .lines()
                .find_map(|line| {
                    let trimmed = line.trim();
                    trimmed
                        .strip_prefix("description:")
                        .map(|rest| rest.trim().to_string())
                })
                .unwrap_or_default();

            return (description, body);
        }
    }

    // No frontmatter — use first non-empty line as description
    let description = content
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .to_string();

    (description, content.to_string())
}
