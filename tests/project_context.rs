use std::fs;
use std::path::PathBuf;

use tempfile::TempDir;

use rustain::adapters::persona_adapter::PersonaAdapter;
use rustain::adapters::project_context_loader::ProjectContextLoader;
use rustain::domain::models::project_context::{
    CONTEXT_BUDGET_CHARS, ContextFileType, IgnorePatterns, ProjectContext, ProjectContextFile,
};
use rustain::domain::ports::PersonaPort;

// ── Unit tests: ProjectContext domain types (Task 9) ────────────────────────

// Covers: FR115 (project context)
#[test]
fn test_assembled_prompt_concatenates_in_priority_order() {
    let ctx = ProjectContext {
        files: vec![
            ProjectContextFile {
                path: PathBuf::from("sub/CLAUDE.md"),
                content: "sub rules".to_string(),
                priority: 100,
                source_type: ContextFileType::ClaudeMd,
            },
            ProjectContextFile {
                path: PathBuf::from("CLAUDE.md"),
                content: "root rules".to_string(),
                priority: 0,
                source_type: ContextFileType::ClaudeMd,
            },
            ProjectContextFile {
                path: PathBuf::from("../CLAUDE.md"),
                content: "parent rules".to_string(),
                priority: 1,
                source_type: ContextFileType::ClaudeMd,
            },
        ],
        total_chars: 31,
        truncated: false,
    };

    let prompt = ctx.assembled_prompt();
    let root_pos = prompt.find("root rules").unwrap();
    let parent_pos = prompt.find("parent rules").unwrap();
    let sub_pos = prompt.find("sub rules").unwrap();

    assert!(root_pos < parent_pos);
    assert!(parent_pos < sub_pos);
}

// Covers: FR115 (project context)
#[test]
fn test_assembled_prompt_truncation_removes_lowest_priority() {
    let large = "x".repeat(CONTEXT_BUDGET_CHARS);
    let ctx = ProjectContext {
        files: vec![
            ProjectContextFile {
                path: PathBuf::from("CLAUDE.md"),
                content: "important rules".to_string(),
                priority: 0,
                source_type: ContextFileType::ClaudeMd,
            },
            ProjectContextFile {
                path: PathBuf::from("low_priority.md"),
                content: large,
                priority: 200,
                source_type: ContextFileType::CursorRules,
            },
        ],
        total_chars: CONTEXT_BUDGET_CHARS + 100,
        truncated: true,
    };

    let prompt = ctx.assembled_prompt();
    assert!(prompt.contains("important rules"));
    assert!(prompt.contains("CONTEXT TRUNCATED"));
}

// Covers: FR115 (project context)
#[test]
fn test_assembled_prompt_empty_returns_empty_string() {
    let ctx = ProjectContext::empty();
    assert_eq!(ctx.assembled_prompt(), "");
}

// Covers: FR115 (project context)
#[test]
fn test_ignore_patterns_glob_matching() {
    let patterns = IgnorePatterns::new(vec![
        "node_modules/".to_string(),
        "*.log".to_string(),
        ".env".to_string(),
        "target/".to_string(),
    ]);

    // Should match
    assert!(patterns.matches(std::path::Path::new("node_modules/foo.js")));
    assert!(patterns.matches(std::path::Path::new("error.log")));
    assert!(patterns.matches(std::path::Path::new(".env")));
    assert!(patterns.matches(std::path::Path::new("target/debug")));

    // Should not match
    assert!(!patterns.matches(std::path::Path::new("src/main.rs")));
    assert!(!patterns.matches(std::path::Path::new("CLAUDE.md")));
    assert!(!patterns.matches(std::path::Path::new("README.md")));
}

// ── Unit tests: ProjectContextLoader (Task 10) ─────────────────────────────

// Covers: FR115 (project context)
#[test]
fn test_loader_finds_claude_md_at_workspace_root() {
    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join("CLAUDE.md"), "# My project rules").unwrap();

    let loader = ProjectContextLoader::new(tmp.path().to_path_buf());
    let ctx = loader.discover().unwrap();

    assert_eq!(ctx.files.len(), 1);
    assert_eq!(ctx.files[0].content, "# My project rules");
    assert_eq!(ctx.files[0].priority, 0);
}

// Covers: FR115 (project context)
#[test]
fn test_loader_walks_upward_for_parent_claude_md() {
    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join("CLAUDE.md"), "parent context").unwrap();

    let workspace = tmp.path().join("project").join("subdir");
    fs::create_dir_all(&workspace).unwrap();
    fs::write(workspace.join("CLAUDE.md"), "workspace context").unwrap();

    let loader = ProjectContextLoader::new(workspace);
    let ctx = loader.discover().unwrap();

    // Should find both
    assert!(
        ctx.files
            .iter()
            .any(|f| f.content == "workspace context" && f.priority == 0)
    );
    assert!(
        ctx.files
            .iter()
            .any(|f| f.content == "parent context" && f.priority > 0)
    );
}

// Covers: FR115 (project context)
#[test]
fn test_loader_finds_cursorrules_alongside_claude_md() {
    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join("CLAUDE.md"), "claude rules").unwrap();
    fs::write(tmp.path().join(".cursorrules"), "cursor rules").unwrap();

    let loader = ProjectContextLoader::new(tmp.path().to_path_buf());
    let ctx = loader.discover().unwrap();

    assert_eq!(ctx.files.len(), 2);
    assert!(
        ctx.files
            .iter()
            .any(|f| f.source_type == ContextFileType::ClaudeMd)
    );
    assert!(
        ctx.files
            .iter()
            .any(|f| f.source_type == ContextFileType::CursorRules)
    );
}

// Covers: FR115 (project context)
#[test]
fn test_loader_claudeignore_excludes_matching_paths() {
    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join(".claudeignore"), "ignored/\n").unwrap();

    let ignored = tmp.path().join("ignored");
    fs::create_dir(&ignored).unwrap();
    fs::write(ignored.join("CLAUDE.md"), "should be excluded").unwrap();

    let kept = tmp.path().join("kept");
    fs::create_dir(&kept).unwrap();
    fs::write(kept.join("CLAUDE.md"), "should be included").unwrap();

    let loader = ProjectContextLoader::new(tmp.path().to_path_buf());
    let ctx = loader.discover().unwrap();

    assert!(!ctx.files.iter().any(|f| f.content == "should be excluded"));
    assert!(ctx.files.iter().any(|f| f.content == "should be included"));
}

// Covers: FR115 (project context)
#[test]
fn test_loader_empty_workspace_returns_empty_context() {
    let tmp = TempDir::new().unwrap();
    let loader = ProjectContextLoader::new(tmp.path().to_path_buf());
    let ctx = loader.discover().unwrap();

    assert!(ctx.files.is_empty());
    assert_eq!(ctx.total_chars, 0);
}

// Covers: FR115 (project context)
#[test]
fn test_loader_missing_files_gracefully_handled() {
    let tmp = TempDir::new().unwrap();
    // No files at all — should not error
    let loader = ProjectContextLoader::new(tmp.path().to_path_buf());
    let result = loader.discover();
    assert!(result.is_ok());
    assert!(result.unwrap().files.is_empty());
}

// ── Integration tests (Task 11) ────────────────────────────────────────────

// Covers: FR115 (project context)
#[test]
fn test_full_flow_workspace_with_claude_md() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("CLAUDE.md"),
        "You are a helpful assistant for the FooBar project.",
    )
    .unwrap();

    // Discover
    let loader = ProjectContextLoader::new(tmp.path().to_path_buf());
    let ctx = loader.discover().unwrap();
    assert_eq!(ctx.files.len(), 1);

    // Assemble prompt
    let prompt = ctx.assembled_prompt();
    assert!(prompt.contains("FooBar project"));

    // Wire into PersonaAdapter → system_prompt
    let adapter = PersonaAdapter::new(ctx);
    let system_prompt = adapter.system_prompt(tmp.path());
    assert!(system_prompt.contains("FooBar project"));
    assert!(adapter.has_context());
}

// Covers: FR115 (project context)
#[test]
fn test_full_flow_empty_workspace_no_crash() {
    let tmp = TempDir::new().unwrap();

    let loader = ProjectContextLoader::new(tmp.path().to_path_buf());
    let ctx = loader.discover().unwrap();
    assert!(ctx.files.is_empty());

    let adapter = PersonaAdapter::new(ctx);
    let system_prompt = adapter.system_prompt(tmp.path());
    assert!(system_prompt.is_empty());
    assert!(!adapter.has_context());
}

// Covers: FR115 (project context)
#[test]
fn test_full_flow_hierarchical_priority() {
    let tmp = TempDir::new().unwrap();
    // Parent CLAUDE.md
    fs::write(tmp.path().join("CLAUDE.md"), "parent instructions").unwrap();

    // Workspace nested inside parent
    let workspace = tmp.path().join("projects").join("myapp");
    fs::create_dir_all(&workspace).unwrap();
    fs::write(workspace.join("CLAUDE.md"), "workspace instructions").unwrap();

    let loader = ProjectContextLoader::new(workspace.clone());
    let ctx = loader.discover().unwrap();

    // Both should be included
    assert!(ctx.files.len() >= 2);

    // When assembled, workspace root (priority 0) comes first
    let adapter = PersonaAdapter::new(ctx);
    let prompt = adapter.system_prompt(&workspace);
    let ws_pos = prompt.find("workspace instructions").unwrap();
    let parent_pos = prompt.find("parent instructions").unwrap();
    assert!(
        ws_pos < parent_pos,
        "workspace root should have higher priority than parent"
    );
}

// Covers: FR115 (project context)
#[test]
fn test_truncation_with_multibyte_content_stays_within_budget() {
    // CJK characters are 3 bytes each. Budget tracking uses bytes.
    // Verify partial truncation doesn't exceed byte budget.
    let cjk_content = "你好世界".repeat(CONTEXT_BUDGET_CHARS / 12 + 100); // Each char is 3 bytes
    let ctx = ProjectContext {
        files: vec![
            ProjectContextFile {
                path: PathBuf::from("CLAUDE.md"),
                content: "important".to_string(),
                priority: 0,
                source_type: ContextFileType::ClaudeMd,
            },
            ProjectContextFile {
                path: PathBuf::from("large_cjk.md"),
                content: cjk_content,
                priority: 100,
                source_type: ContextFileType::CursorRules,
            },
        ],
        total_chars: CONTEXT_BUDGET_CHARS + 1000,
        truncated: true,
    };

    let prompt = ctx.assembled_prompt();
    // The assembled prompt must not exceed budget + reasonable overhead (header + footer)
    // Budget is 100_000 bytes. Headers + footer add ~120 bytes max.
    assert!(
        prompt.len() <= CONTEXT_BUDGET_CHARS + 200,
        "Assembled prompt ({} bytes) should not greatly exceed budget ({})",
        prompt.len(),
        CONTEXT_BUDGET_CHARS
    );
    assert!(prompt.contains("important"));
    assert!(prompt.contains("CONTEXT TRUNCATED"));
    // Verify the truncated CJK content is valid UTF-8 (implicit — String type guarantees this)
    assert!(prompt.contains("你好"));
}
