use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::domain::models::project_context::{
    ContextFileType, IgnorePatterns, ProjectContext, ProjectContextFile,
};

/// Maximum number of parent directories to scan upward.
const MAX_PARENT_SCAN_DEPTH: usize = 10;

/// Discovers and loads project context files from the workspace.
pub struct ProjectContextLoader {
    workspace_path: PathBuf,
}

impl ProjectContextLoader {
    pub fn new(workspace_path: PathBuf) -> Self {
        Self { workspace_path }
    }

    /// Discover and load all project context files.
    ///
    /// Algorithm:
    /// 1. Scan workspace root for CLAUDE.md, .cursorrules, .claudeignore
    /// 2. Walk upward from workspace root looking for CLAUDE.md (up to 10 levels)
    /// 3. Scan immediate subdirectories for CLAUDE.md
    /// 4. Parse .claudeignore if found
    /// 5. Filter through ignore patterns
    /// 6. Assign priorities and build ProjectContext
    pub fn discover(&self) -> Result<ProjectContext> {
        let mut files: Vec<ProjectContextFile> = Vec::new();
        let ignore_patterns = Self::load_ignore_patterns(&self.workspace_path);

        // 1. Scan workspace root
        self.scan_workspace_root(&mut files, &ignore_patterns);

        // 2. Walk upward for CLAUDE.md in parent directories
        self.scan_parents(&mut files, &ignore_patterns);

        // 3. Scan immediate subdirectories for CLAUDE.md
        self.scan_subdirectories(&mut files, &ignore_patterns);

        let total_chars: usize = files.iter().map(|f| f.content.len()).sum();
        let truncated = total_chars > crate::domain::models::project_context::CONTEXT_BUDGET_CHARS;

        Ok(ProjectContext {
            files,
            total_chars,
            truncated,
        })
    }

    /// Read and parse `.claudeignore` from workspace root.
    pub fn load_ignore_patterns(workspace: &Path) -> IgnorePatterns {
        let ignore_path = workspace.join(".claudeignore");
        match std::fs::read_to_string(&ignore_path) {
            Ok(content) => {
                let patterns: Vec<String> = content
                    .lines()
                    .map(|l| l.trim())
                    .filter(|l| !l.is_empty() && !l.starts_with('#'))
                    .map(|l| l.to_string())
                    .collect();
                tracing::debug!(
                    "Loaded {} ignore patterns from .claudeignore",
                    patterns.len()
                );
                IgnorePatterns::new(patterns)
            }
            Err(_) => IgnorePatterns::default(),
        }
    }

    /// Scan workspace root for known context files.
    fn scan_workspace_root(&self, files: &mut Vec<ProjectContextFile>, ignore: &IgnorePatterns) {
        // CLAUDE.md at workspace root (highest priority)
        self.try_load_file(
            &self.workspace_path.join("CLAUDE.md"),
            0, // workspace root = highest priority
            ContextFileType::ClaudeMd,
            files,
            ignore,
        );

        // .cursorrules at workspace root
        self.try_load_file(
            &self.workspace_path.join(".cursorrules"),
            1, // just after CLAUDE.md
            ContextFileType::CursorRules,
            files,
            ignore,
        );
    }

    /// Walk upward from workspace root looking for CLAUDE.md in parent directories.
    fn scan_parents(&self, files: &mut Vec<ProjectContextFile>, ignore: &IgnorePatterns) {
        let mut current = self.workspace_path.clone();
        for depth in 1..=MAX_PARENT_SCAN_DEPTH {
            match current.parent() {
                Some(parent) => {
                    let claude_md = parent.join("CLAUDE.md");
                    self.try_load_file(
                        &claude_md,
                        (depth + 9) as u8, // parents start at 10, workspace root=0, .cursorrules=1
                        ContextFileType::ClaudeMd,
                        files,
                        ignore,
                    );
                    current = parent.to_path_buf();
                }
                None => break, // reached filesystem root
            }
        }
    }

    /// Scan immediate subdirectories (1 level deep) for CLAUDE.md.
    fn scan_subdirectories(&self, files: &mut Vec<ProjectContextFile>, ignore: &IgnorePatterns) {
        let entries = match std::fs::read_dir(&self.workspace_path) {
            Ok(entries) => entries,
            Err(e) => {
                tracing::warn!("Failed to read workspace directory: {}", e);
                return;
            }
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let claude_md = path.join("CLAUDE.md");
                let depth = 100; // subdirectory base priority
                self.try_load_file(&claude_md, depth, ContextFileType::ClaudeMd, files, ignore);
            }
        }
    }

    /// Try to load a single file if it exists and isn't ignored.
    fn try_load_file(
        &self,
        path: &Path,
        priority: u8,
        source_type: ContextFileType,
        files: &mut Vec<ProjectContextFile>,
        ignore: &IgnorePatterns,
    ) {
        // Skip .claudeignore files themselves — they're patterns, not context
        if source_type == ContextFileType::ClaudeIgnore {
            return;
        }

        if !path.exists() {
            return;
        }

        // Check ignore patterns against relative path
        if let Ok(relative) = path.strip_prefix(&self.workspace_path) {
            if ignore.matches(relative) {
                tracing::debug!(
                    "Ignoring context file (matched .claudeignore): {}",
                    path.display()
                );
                return;
            }
        }

        match std::fs::read_to_string(path) {
            Ok(content) => {
                tracing::info!(
                    "Loaded project context file: {} ({} chars)",
                    path.display(),
                    content.len()
                );
                files.push(ProjectContextFile {
                    path: path.to_path_buf(),
                    content,
                    priority,
                    source_type,
                });
            }
            Err(e) => {
                tracing::warn!("Failed to read context file {}: {}", path.display(), e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_discover_claude_md_at_root() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("CLAUDE.md"), "# Project rules").unwrap();

        let loader = ProjectContextLoader::new(tmp.path().to_path_buf());
        let ctx = loader.discover().unwrap();

        assert_eq!(ctx.files.len(), 1);
        assert_eq!(ctx.files[0].content, "# Project rules");
        assert_eq!(ctx.files[0].priority, 0);
        assert_eq!(ctx.files[0].source_type, ContextFileType::ClaudeMd);
    }

    #[test]
    fn test_discover_walks_upward() {
        let tmp = TempDir::new().unwrap();
        // Create parent CLAUDE.md
        fs::write(tmp.path().join("CLAUDE.md"), "parent rules").unwrap();

        // Create workspace subdirectory
        let workspace = tmp.path().join("projects").join("myapp");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(workspace.join("CLAUDE.md"), "workspace rules").unwrap();

        let loader = ProjectContextLoader::new(workspace);
        let ctx = loader.discover().unwrap();

        // Should find both workspace root and parent
        assert!(ctx.files.len() >= 2);
        // Workspace root should have priority 0
        assert!(
            ctx.files
                .iter()
                .any(|f| f.priority == 0 && f.content == "workspace rules")
        );
        // Parent should have higher priority number (lower precedence)
        assert!(
            ctx.files
                .iter()
                .any(|f| f.priority > 0 && f.content == "parent rules")
        );
    }

    #[test]
    fn test_discover_cursorrules() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("CLAUDE.md"), "claude rules").unwrap();
        fs::write(tmp.path().join(".cursorrules"), "cursor rules").unwrap();

        let loader = ProjectContextLoader::new(tmp.path().to_path_buf());
        let ctx = loader.discover().unwrap();

        assert_eq!(ctx.files.len(), 2);
        assert!(
            ctx.files
                .iter()
                .any(|f| f.source_type == ContextFileType::CursorRules)
        );
    }

    #[test]
    fn test_claudeignore_excludes_paths() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join(".claudeignore"), "ignored/\n").unwrap();

        // Create subdirectory with CLAUDE.md that should be ignored
        let ignored_dir = tmp.path().join("ignored");
        fs::create_dir(&ignored_dir).unwrap();
        fs::write(ignored_dir.join("CLAUDE.md"), "should be ignored").unwrap();

        // Create another subdirectory that should NOT be ignored
        let kept_dir = tmp.path().join("kept");
        fs::create_dir(&kept_dir).unwrap();
        fs::write(kept_dir.join("CLAUDE.md"), "should be kept").unwrap();

        let loader = ProjectContextLoader::new(tmp.path().to_path_buf());
        let ctx = loader.discover().unwrap();

        // Should NOT contain the ignored file
        assert!(!ctx.files.iter().any(|f| f.content == "should be ignored"));
        // Should contain the kept file
        assert!(ctx.files.iter().any(|f| f.content == "should be kept"));
    }

    #[test]
    fn test_empty_workspace_graceful() {
        let tmp = TempDir::new().unwrap();
        let loader = ProjectContextLoader::new(tmp.path().to_path_buf());
        let ctx = loader.discover().unwrap();

        assert!(ctx.files.is_empty());
        assert_eq!(ctx.total_chars, 0);
    }

    #[test]
    fn test_unreadable_files_skipped() {
        let tmp = TempDir::new().unwrap();
        // Just create an empty workspace — no files to read
        let loader = ProjectContextLoader::new(tmp.path().to_path_buf());
        let ctx = loader.discover().unwrap();
        assert!(ctx.files.is_empty());
    }
}
