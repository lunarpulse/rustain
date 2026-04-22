//! Custom slash commands: discovered from `.claude/commands/`, with `{{args}}` and `@{path}` interpolation.
//! See Story 5.3.

use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::domain::services::command_normalize;
use crate::domain::services::frontmatter;

const MAX_COMMAND_FILE_SIZE: u64 = 1_048_576;
const MAX_COMMAND_DESCRIPTION_BYTES: usize = 200;
const MAX_COMMAND_SCAN_FILES: usize = 200;
const COMMAND_SCAN_BUDGET_MS: u64 = 50;
const MAX_FILE_REF_CONTENT_BYTES: usize = 102_400;
const MAX_FILE_REF_DIR_ENTRIES: usize = 100;

#[derive(Debug, Clone)]
pub enum CommandSource {
    BuiltIn,
    UserDefined {
        #[allow(dead_code)]
        path: PathBuf,
    },
}

#[derive(Debug, Clone)]
pub struct SlashCommandDef {
    pub name: String,
    pub description: String,
    pub source: CommandSource,
    pub content: Option<String>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct FileRefError {
    pub raw_path: String,
    pub reason: String,
}

pub struct CommandRegistry {
    commands: Vec<SlashCommandDef>,
    discovered: bool,
}

impl CommandRegistry {
    pub fn new() -> Self {
        let commands = vec![
            SlashCommandDef {
                name: "new".to_string(),
                description: "Start a new conversation".to_string(),
                source: CommandSource::BuiltIn,
                content: None,
            },
            SlashCommandDef {
                name: "export".to_string(),
                description: "Export conversation to markdown (optional filename arg)".to_string(),
                source: CommandSource::BuiltIn,
                content: None,
            },
            SlashCommandDef {
                name: "deactivate".to_string(),
                description: "Deactivate all active skills in this conversation".to_string(),
                source: CommandSource::BuiltIn,
                content: None,
            },
            SlashCommandDef {
                name: "mode".to_string(),
                description: "Set permission mode (plan, normal, autoedit, yolo)".to_string(),
                source: CommandSource::BuiltIn,
                content: None,
            },
        ];
        Self {
            commands,
            discovered: false,
        }
    }

    pub fn is_discovered(&self) -> bool {
        self.discovered
    }

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

    pub fn refresh(&mut self, workspace: &Path) {
        self.commands
            .retain(|c| matches!(c.source, CommandSource::BuiltIn));

        let cmd_dir = workspace.join(".claude").join("commands");
        let user_commands = scan_commands_dir(&cmd_dir);

        let builtin_names: HashSet<&str> = self.commands.iter().map(|c| c.name.as_str()).collect();
        for cmd in &user_commands {
            if builtin_names.contains(cmd.name.as_str()) {
                tracing::warn!(
                    "User command '{}' shadowed by built-in — rename the custom file",
                    cmd.name
                );
            }
        }

        self.commands.extend(user_commands);
        self.discovered = true;
    }

    pub fn warn_skill_shadows(
        &self,
        skill_registry: &crate::adapters::skill_registry::SkillRegistry,
    ) {
        for cmd in &self.commands {
            if matches!(cmd.source, CommandSource::UserDefined { .. }) {
                if skill_registry.find(&cmd.name).is_some() {
                    tracing::warn!(
                        "User command '{}' shadowed by skill '{}' — rename one to disambiguate",
                        cmd.name,
                        cmd.name
                    );
                }
            }
        }
    }

    #[deprecated(note = "Use refresh() instead; discover_into is kept for call-site compatibility")]
    #[allow(dead_code)]
    pub fn discover_into(&mut self, workspace: &Path) {
        self.refresh(workspace);
    }

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

        user_defined.sort_by(|a, b| a.name.cmp(&b.name));
        builtins.extend(user_defined);
        builtins
    }

    pub fn find(&self, name: &str) -> Option<&SlashCommandDef> {
        self.commands.iter().find(|c| c.name == name)
    }
}

impl Default for CommandRegistry {
    fn default() -> Self {
        Self::new()
    }
}

fn scan_commands_dir(cmd_dir: &Path) -> Vec<SlashCommandDef> {
    if !cmd_dir.is_dir() {
        return Vec::new();
    }

    let mut results: Vec<SlashCommandDef> = Vec::new();
    let mut seen_keys: HashSet<String> = HashSet::new();
    let start = Instant::now();

    let mut entries: Vec<PathBuf> = Vec::new();
    collect_md_files(cmd_dir, &mut entries, 0);

    entries.sort_by(|a, b| {
        a.strip_prefix(cmd_dir)
            .unwrap_or(a)
            .cmp(b.strip_prefix(cmd_dir).unwrap_or(b))
    });

    for (file_count, path) in entries.into_iter().enumerate() {
        if file_count >= MAX_COMMAND_SCAN_FILES {
            tracing::warn!("Command scan truncated at {} files", MAX_COMMAND_SCAN_FILES);
            break;
        }
        if start.elapsed().as_millis() as u64 > COMMAND_SCAN_BUDGET_MS {
            tracing::warn!(
                "Command scan truncated after {}ms ({} files)",
                start.elapsed().as_millis(),
                file_count
            );
            break;
        }

        let key = match command_normalize::normalize_command_path(cmd_dir, &path) {
            Some(k) => k,
            None => {
                tracing::warn!(
                    "Skipping command file {:?}: empty name after normalization",
                    path
                );
                continue;
            }
        };

        if seen_keys.contains(&key) {
            tracing::warn!("Duplicate command key '{}' — skipping {:?}", key, path);
            continue;
        }

        if let Some(cmd) = parse_command_file(&path, &key) {
            seen_keys.insert(key);
            results.push(cmd);
        }
    }

    results.sort_by(|a, b| a.name.cmp(&b.name));
    results
}

fn collect_md_files(current: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    if depth > command_normalize::MAX_COMMAND_NAMESPACE_DEPTH as usize {
        return;
    }
    let Ok(entries) = std::fs::read_dir(current) else {
        return;
    };
    let mut sorted: Vec<_> = entries.flatten().collect();
    sorted.sort_by_key(|e| e.file_name());

    for entry in sorted {
        if out.len() >= MAX_COMMAND_SCAN_FILES {
            break;
        }
        if entry.file_type().is_ok_and(|ft| ft.is_symlink()) {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            collect_md_files(&path, out, depth + 1);
        } else if path.extension().is_some_and(|ext| ext == "md") {
            out.push(path);
        }
    }
}

fn parse_command_file(path: &Path, name: &str) -> Option<SlashCommandDef> {
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
    let (description, body) = parse_command_description_and_body(&content);

    Some(SlashCommandDef {
        name: name.to_string(),
        description,
        source: CommandSource::UserDefined {
            path: path.to_path_buf(),
        },
        content: Some(body),
    })
}

fn parse_command_description_and_body(content: &str) -> (String, String) {
    if let Some((frontmatter_text, body)) = frontmatter::parse_frontmatter(content) {
        let description =
            frontmatter::extract_field(frontmatter_text, "description").unwrap_or_default();
        if description.is_empty() {
            return (first_line_description(body), body.to_string());
        }
        return (description, body.to_string());
    }

    let description = first_line_description(content);
    (description, content.to_string())
}

fn strip_heading_prefix(line: &str) -> &str {
    let mut hash_count = 0;
    for ch in line.chars() {
        if ch == '#' && hash_count < 3 {
            hash_count += 1;
        } else {
            break;
        }
    }
    let after_hashes = &line[hash_count..];
    if hash_count > 0
        && after_hashes
            .chars()
            .next()
            .is_none_or(|c| c.is_whitespace())
    {
        after_hashes.trim_start()
    } else {
        line.trim_start()
    }
}

fn first_line_description(text: &str) -> String {
    let first_line = match text.lines().find(|line| !line.trim().is_empty()) {
        Some(line) => line.trim(),
        None => return "(no description)".to_string(),
    };

    let stripped = strip_heading_prefix(first_line);

    if stripped.is_empty() {
        return "(no description)".to_string();
    }

    if stripped.len() > MAX_COMMAND_DESCRIPTION_BYTES {
        #[allow(clippy::incompatible_msrv)]
        let boundary = stripped.floor_char_boundary(MAX_COMMAND_DESCRIPTION_BYTES);
        format!("{}…", &stripped[..boundary])
    } else {
        stripped.to_string()
    }
}

pub fn resolve_file_refs(
    body: &str,
    workspace: &Path,
    security: &dyn crate::domain::ports::SecurityPort,
) -> (String, Vec<FileRefError>) {
    let mut result = String::with_capacity(body.len());
    let mut errors = Vec::new();
    let mut last_end = 0;

    let mut i = 0;
    let bytes = body.as_bytes();
    while i < body.len() {
        if bytes[i] == b'@' && i + 1 < body.len() && bytes[i + 1] == b'{' {
            let brace_start = i + 2;
            let mut j = brace_start;
            let mut path_chars = 0u32;
            let mut found_end = false;
            while j < body.len() {
                let c = body.as_bytes()[j];
                if c == b'}' {
                    found_end = true;
                    break;
                }
                if c.is_ascii_alphanumeric() || c == b'_' || c == b'-' || c == b'.' || c == b'/' {
                    path_chars += 1;
                    j += 1;
                } else {
                    break;
                }
            }

            if found_end && path_chars > 0 {
                let raw_path = &body[brace_start..j];
                result.push_str(&body[last_end..i]);

                let full_path = workspace.join(raw_path);
                let canonical = match full_path.canonicalize() {
                    Ok(p) => p,
                    Err(_) => {
                        result.push_str(&format!("[File not found: {raw_path}]"));
                        errors.push(FileRefError {
                            raw_path: raw_path.to_string(),
                            reason: format!("File not found: {raw_path}"),
                        });
                        last_end = j + 1;
                        i = j + 1;
                        continue;
                    }
                };

                use crate::domain::models::FileOperation;
                if security
                    .check_workspace_access(&canonical, FileOperation::Read)
                    .is_err()
                {
                    result.push_str(&format!("[File not found: {raw_path}]"));
                    errors.push(FileRefError {
                        raw_path: raw_path.to_string(),
                        reason: format!("File not found: {raw_path}"),
                    });
                    last_end = j + 1;
                    i = j + 1;
                    continue;
                }

                if canonical.is_dir() {
                    let mut entries: Vec<String> = Vec::new();
                    if let Ok(dir_entries) = std::fs::read_dir(&canonical) {
                        for entry in dir_entries.flatten() {
                            if entries.len() >= MAX_FILE_REF_DIR_ENTRIES {
                                entries.push("...".to_string());
                                break;
                            }
                            if let Some(name) = entry.file_name().to_str() {
                                entries.push(name.to_string());
                            }
                        }
                    }
                    result.push_str(&entries.join("\n"));
                } else {
                    // Bounded read to avoid OOM on multi-gigabyte files
                    let file = match std::fs::File::open(&canonical) {
                        Ok(f) => f,
                        Err(_) => {
                            result.push_str(&format!("[File not found: {raw_path}]"));
                            errors.push(FileRefError {
                                raw_path: raw_path.to_string(),
                                reason: format!("File not found: {raw_path}"),
                            });
                            last_end = j + 1;
                            i = j + 1;
                            continue;
                        }
                    };

                    const READ_SLACK: usize = 16;
                    let read_limit = MAX_FILE_REF_CONTENT_BYTES + READ_SLACK;
                    let mut buf = Vec::with_capacity(read_limit);
                    let mut reader = file.take(read_limit as u64);
                    if std::io::Read::read_to_end(&mut reader, &mut buf).is_err() {
                        result.push_str(&format!("[Binary file skipped: {raw_path}]"));
                        errors.push(FileRefError {
                            raw_path: raw_path.to_string(),
                            reason: format!("Binary file skipped: {raw_path}"),
                        });
                        last_end = j + 1;
                        i = j + 1;
                        continue;
                    }

                    // Check for null bytes in first 8 KiB (binary detection)
                    if buf.iter().take(8192).any(|&b| b == 0) {
                        result.push_str(&format!("[Binary file skipped: {raw_path}]"));
                        errors.push(FileRefError {
                            raw_path: raw_path.to_string(),
                            reason: format!("Binary file skipped: {raw_path}"),
                        });
                        last_end = j + 1;
                        i = j + 1;
                        continue;
                    }

                    // Decode as UTF-8
                    let content = match String::from_utf8(buf) {
                        Ok(s) => s,
                        Err(e) => {
                            let valid_up_to = e.utf8_error().valid_up_to();
                            if valid_up_to > MAX_FILE_REF_CONTENT_BYTES {
                                let mut buf = e.into_bytes();
                                buf.truncate(valid_up_to);
                                match String::from_utf8(buf) {
                                    Ok(s) => s,
                                    Err(_) => {
                                        result.push_str(&format!(
                                            "[Binary file skipped: {raw_path}]"
                                        ));
                                        errors.push(FileRefError {
                                            raw_path: raw_path.to_string(),
                                            reason: format!("Binary file skipped: {raw_path}"),
                                        });
                                        last_end = j + 1;
                                        i = j + 1;
                                        continue;
                                    }
                                }
                            } else {
                                result.push_str(&format!("[Binary file skipped: {raw_path}]"));
                                errors.push(FileRefError {
                                    raw_path: raw_path.to_string(),
                                    reason: format!("Binary file skipped: {raw_path}"),
                                });
                                last_end = j + 1;
                                i = j + 1;
                                continue;
                            }
                        }
                    };

                    if content.len() > MAX_FILE_REF_CONTENT_BYTES {
                        #[allow(clippy::incompatible_msrv)]
                        let boundary = content.floor_char_boundary(MAX_FILE_REF_CONTENT_BYTES);
                        result.push_str(&content[..boundary]);
                        result.push_str("\n...[truncated]");
                    } else {
                        result.push_str(&content);
                    }
                }

                last_end = j + 1;
                i = j + 1;
            } else {
                i += 1;
            }
        } else {
            i += 1;
        }
    }

    result.push_str(&body[last_end..]);
    (result, errors)
}
