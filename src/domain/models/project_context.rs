use std::path::{Path, PathBuf};

/// Default budget for project context in characters (~25k tokens at ~4 chars/token).
pub const CONTEXT_BUDGET_CHARS: usize = 100_000;

/// Known file names to scan for project context.
#[allow(dead_code)]
pub const CONTEXT_FILE_NAMES: &[&str] = &["CLAUDE.md", ".cursorrules"];

/// Type of project context file discovered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextFileType {
    ClaudeMd,
    CursorRules,
    ClaudeIgnore,
}

/// A single discovered project context file.
#[derive(Debug, Clone)]
pub struct ProjectContextFile {
    pub path: PathBuf,
    pub content: String,
    pub priority: u8,
    #[allow(dead_code)]
    pub source_type: ContextFileType,
}

/// Assembled project context from all discovered files.
#[derive(Debug, Clone)]
pub struct ProjectContext {
    pub files: Vec<ProjectContextFile>,
    pub total_chars: usize,
    pub truncated: bool,
}

impl ProjectContext {
    /// Create an empty project context (no files discovered).
    pub fn empty() -> Self {
        Self {
            files: Vec::new(),
            total_chars: 0,
            truncated: false,
        }
    }

    /// Concatenate all files in priority order into a single prompt string.
    /// Enforces `CONTEXT_BUDGET_CHARS` budget, truncating lowest-priority files first.
    pub fn assembled_prompt(&self) -> String {
        if self.files.is_empty() {
            return String::new();
        }

        // Files should already be sorted by priority (lowest number = highest priority).
        let mut sorted: Vec<&ProjectContextFile> = self.files.iter().collect();
        sorted.sort_by_key(|f| f.priority);

        let mut result = String::new();
        let mut remaining_budget = CONTEXT_BUDGET_CHARS;
        let mut omitted_count = 0usize;

        for file in &sorted {
            let header = format!("--- FILE: {} ---\n", file.path.display());
            let entry_len = header.len() + file.content.len() + 1; // +1 for trailing newline

            if entry_len <= remaining_budget {
                result.push_str(&header);
                result.push_str(&file.content);
                result.push('\n');
                remaining_budget -= entry_len;
            } else if remaining_budget > header.len() + 50 {
                // Partial truncation: include what fits (byte-safe)
                let content_budget = remaining_budget - header.len() - 1;
                let mut safe_end = content_budget;
                while safe_end > 0 && !file.content.is_char_boundary(safe_end) {
                    safe_end -= 1;
                }
                result.push_str(&header);
                result.push_str(&file.content[..safe_end]);
                result.push('\n');
                remaining_budget = 0;
                omitted_count += 1;
            } else {
                omitted_count += 1;
            }
        }

        if omitted_count > 0 {
            result.push_str(&format!(
                "\n--- CONTEXT TRUNCATED: {} files truncated or omitted due to budget ---\n",
                omitted_count,
            ));
        }

        result
    }
}

/// Gitignore-style patterns parsed from `.claudeignore`.
#[derive(Debug, Clone, Default)]
pub struct IgnorePatterns {
    patterns: Vec<String>,
}

impl IgnorePatterns {
    /// Create from a list of pattern strings.
    pub fn new(patterns: Vec<String>) -> Self {
        Self { patterns }
    }

    /// Check if a path matches any ignore pattern.
    /// Uses simple glob matching: `*` matches any sequence, `/` suffix matches directories.
    pub fn matches(&self, path: &Path) -> bool {
        let path_str = path.to_string_lossy();
        for pattern in &self.patterns {
            if simple_glob_match(pattern, &path_str) {
                return true;
            }
        }
        false
    }

    /// Whether any patterns are loaded.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }
}

/// Simple glob matching supporting `*` (any chars) and trailing `/` (directory match).
/// Not a full gitignore implementation — MVP level.
fn simple_glob_match(pattern: &str, path: &str) -> bool {
    let pattern = pattern.trim_end_matches('/');

    // Check if any path component matches the pattern
    for component in path.split('/') {
        if glob_match_segment(pattern, component) {
            return true;
        }
    }

    // Also check the full path
    glob_match_segment(pattern, path)
}

/// Match a single glob pattern segment against a string.
/// Supports `*` as wildcard for any sequence of characters.
fn glob_match_segment(pattern: &str, text: &str) -> bool {
    let mut p_star: Option<usize> = None;
    let mut t_star: Option<usize> = None;

    let p_vec: Vec<char> = pattern.chars().collect();
    let t_vec: Vec<char> = text.chars().collect();
    let mut pi = 0;
    let mut ti = 0;

    while ti < t_vec.len() {
        if pi < p_vec.len() && (p_vec[pi] == t_vec[ti] || p_vec[pi] == '?') {
            pi += 1;
            ti += 1;
        } else if pi < p_vec.len() && p_vec[pi] == '*' {
            p_star = Some(pi);
            t_star = Some(ti);
            pi += 1;
        } else if let Some(ps) = p_star {
            pi = ps + 1;
            let ts = t_star.unwrap() + 1;
            t_star = Some(ts);
            ti = ts;
        } else {
            return false;
        }
    }

    while pi < p_vec.len() && p_vec[pi] == '*' {
        pi += 1;
    }

    pi == p_vec.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_assembled_prompt_priority_order() {
        let ctx = ProjectContext {
            files: vec![
                ProjectContextFile {
                    path: PathBuf::from("sub/CLAUDE.md"),
                    content: "sub content".to_string(),
                    priority: 100,
                    source_type: ContextFileType::ClaudeMd,
                },
                ProjectContextFile {
                    path: PathBuf::from("CLAUDE.md"),
                    content: "root content".to_string(),
                    priority: 0,
                    source_type: ContextFileType::ClaudeMd,
                },
            ],
            total_chars: 22,
            truncated: false,
        };

        let prompt = ctx.assembled_prompt();
        let root_pos = prompt.find("root content").unwrap();
        let sub_pos = prompt.find("sub content").unwrap();
        assert!(
            root_pos < sub_pos,
            "Root (priority 0) should come before sub (priority 100)"
        );
    }

    #[test]
    fn test_assembled_prompt_truncation() {
        // Create context that exceeds budget
        let large_content = "x".repeat(CONTEXT_BUDGET_CHARS);
        let ctx = ProjectContext {
            files: vec![
                ProjectContextFile {
                    path: PathBuf::from("CLAUDE.md"),
                    content: "important".to_string(),
                    priority: 0,
                    source_type: ContextFileType::ClaudeMd,
                },
                ProjectContextFile {
                    path: PathBuf::from("extra.md"),
                    content: large_content,
                    priority: 100,
                    source_type: ContextFileType::CursorRules,
                },
            ],
            total_chars: CONTEXT_BUDGET_CHARS + 100,
            truncated: true,
        };

        let prompt = ctx.assembled_prompt();
        assert!(prompt.contains("important"));
        assert!(prompt.contains("CONTEXT TRUNCATED"));
    }

    #[test]
    fn test_assembled_prompt_empty() {
        let ctx = ProjectContext::empty();
        assert_eq!(ctx.assembled_prompt(), "");
    }

    #[test]
    fn test_ignore_patterns_matches() {
        let patterns = IgnorePatterns::new(vec![
            "node_modules/".to_string(),
            "*.log".to_string(),
            ".env".to_string(),
            "target/".to_string(),
        ]);

        assert!(patterns.matches(Path::new("node_modules/foo.js")));
        assert!(patterns.matches(Path::new("app.log")));
        assert!(patterns.matches(Path::new(".env")));
        assert!(patterns.matches(Path::new("target/debug")));
        assert!(!patterns.matches(Path::new("src/main.rs")));
        assert!(!patterns.matches(Path::new("CLAUDE.md")));
    }

    #[test]
    fn test_ignore_patterns_empty() {
        let patterns = IgnorePatterns::default();
        assert!(!patterns.matches(Path::new("anything.rs")));
        assert!(patterns.is_empty());
    }

    #[test]
    fn test_glob_wildcard() {
        assert!(glob_match_segment("*.log", "app.log"));
        assert!(glob_match_segment("*.log", "debug.log"));
        assert!(!glob_match_segment("*.log", "app.txt"));
        assert!(glob_match_segment("test*", "testing"));
        assert!(glob_match_segment("*", "anything"));
    }
}
