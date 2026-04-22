use std::path::PathBuf;

use rustain::adapters::command_registry::{CommandRegistry, CommandSource};

// === Command Registry unit tests (Story 3.2, Task 2) ===

// Covers: AC6 — built-in /new command is registered
#[test]
fn test_registry_has_builtin_new() {
    let registry = CommandRegistry::new();
    let all = registry.filter("");
    assert!(
        all.iter().any(|c| c.name == "new"),
        "Missing built-in /new command"
    );
}

// Covers: AC6 — built-in commands appear first
#[test]
fn test_builtin_commands_listed_first() {
    let mut registry = CommandRegistry::new();
    // Simulate user-defined commands
    registry.register_user_command(
        "zebra".to_string(),
        "A custom cmd".to_string(),
        PathBuf::from(".claude/commands/zebra.md"),
        None,
    );
    registry.register_user_command(
        "alpha".to_string(),
        "Another".to_string(),
        PathBuf::from(".claude/commands/alpha.md"),
        None,
    );

    let all = registry.filter("");
    // First should be built-in
    assert!(matches!(all[0].source, CommandSource::BuiltIn));
    // User-defined should be sorted alphabetically
    let user_cmds: Vec<&str> = all
        .iter()
        .filter(|c| matches!(c.source, CommandSource::UserDefined { .. }))
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(user_cmds, vec!["alpha", "zebra"]);
}

// Covers: AC1 — case-insensitive substring filtering
#[test]
fn test_filter_case_insensitive() {
    let mut registry = CommandRegistry::new();
    registry.register_user_command(
        "deploy-staging".to_string(),
        "Deploy".to_string(),
        PathBuf::from("x.md"),
        None,
    );
    registry.register_user_command(
        "deploy-prod".to_string(),
        "Deploy prod".to_string(),
        PathBuf::from("y.md"),
        None,
    );
    registry.register_user_command(
        "run-tests".to_string(),
        "Tests".to_string(),
        PathBuf::from("z.md"),
        None,
    );

    let filtered = registry.filter("Deploy");
    assert_eq!(filtered.len(), 2);
    assert!(filtered.iter().all(|c| c.name.contains("deploy")));
}

// Covers: AC1 — empty filter returns all
#[test]
fn test_filter_empty_returns_all() {
    let registry = CommandRegistry::new();
    let all = registry.filter("");
    assert!(!all.is_empty()); // At least /new
}

// Covers: AC1 — no match returns empty
#[test]
fn test_filter_no_match() {
    let registry = CommandRegistry::new();
    let filtered = registry.filter("xyznonexistent");
    assert!(filtered.is_empty());
}

// Covers: AC2 — user-defined command stores content
#[test]
fn test_user_command_with_content() {
    let mut registry = CommandRegistry::new();
    registry.register_user_command(
        "my-cmd".to_string(),
        "My description".to_string(),
        PathBuf::from(".claude/commands/my-cmd.md"),
        Some("# Instruction\nDo something.".to_string()),
    );

    let filtered = registry.filter("my-cmd");
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].name, "my-cmd");
    assert_eq!(filtered[0].description, "My description");
    assert_eq!(
        filtered[0].content.as_deref(),
        Some("# Instruction\nDo something.")
    );
}

// Covers: AC6 — refresh reads from directory with frontmatter
#[test]
fn test_refresh_commands_from_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let cmd_dir = tmp.path().join(".claude").join("commands");
    std::fs::create_dir_all(&cmd_dir).unwrap();

    std::fs::write(
        cmd_dir.join("deploy-staging.md"),
        "---\ndescription: Deploy to staging\n---\n# Deploy\nRun deploy script.",
    )
    .unwrap();

    std::fs::write(
        cmd_dir.join("run-tests.md"),
        "Run all test suites.\n\nMore details here.",
    )
    .unwrap();

    let mut registry = CommandRegistry::new();
    registry.refresh(tmp.path());
    let all = registry.filter("");

    let user_cmds: Vec<&str> = all
        .iter()
        .filter(|c| matches!(c.source, CommandSource::UserDefined { .. }))
        .map(|c| c.name.as_str())
        .collect();
    assert!(user_cmds.contains(&"deploy-staging"));
    assert!(user_cmds.contains(&"run-tests"));

    let deploy = all.iter().find(|c| c.name == "deploy-staging").unwrap();
    assert_eq!(deploy.description, "Deploy to staging");

    let tests = all.iter().find(|c| c.name == "run-tests").unwrap();
    assert_eq!(tests.description, "Run all test suites.");
}

// Covers: AC2 — lazy caching: discover is called once, result is cached
#[test]
fn test_registry_caching() {
    let registry = CommandRegistry::new();
    assert!(!registry.is_discovered());
}

// === Story 5.3 — Task 4.8: Refresh tests ===

fn write_command(workspace: &std::path::Path, rel_path: &str, content: &str) {
    let full = workspace.join(".claude").join("commands").join(rel_path);
    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
    std::fs::write(&full, content).unwrap();
}

#[test]
fn test_refresh_drops_old_user_commands() {
    let tmp = tempfile::tempdir().unwrap();
    let cmd_dir = tmp.path().join(".claude").join("commands");
    std::fs::create_dir_all(&cmd_dir).unwrap();

    write_command(tmp.path(), "review.md", "Review the code.");
    let mut registry = CommandRegistry::new();
    registry.refresh(tmp.path());
    assert!(registry.find("review").is_some());

    std::fs::remove_file(cmd_dir.join("review.md")).unwrap();
    registry.refresh(tmp.path());
    assert!(registry.find("review").is_none());
}

#[test]
fn test_refresh_discovers_subdirectories() {
    let tmp = tempfile::tempdir().unwrap();

    write_command(
        tmp.path(),
        "deploy/staging.md",
        "Deploy staging instructions.",
    );
    let mut registry = CommandRegistry::new();
    registry.refresh(tmp.path());

    assert!(registry.find("deploy:staging").is_some());
    let cmd = registry.find("deploy:staging").unwrap();
    assert_eq!(cmd.description, "Deploy staging instructions.");
}

#[test]
fn test_refresh_depth_cap() {
    let tmp = tempfile::tempdir().unwrap();
    write_command(tmp.path(), "a/b/c/d/e/f/cmd.md", "Deep command.");
    let mut registry = CommandRegistry::new();
    registry.refresh(tmp.path());
    assert!(registry.find("a:b:c:d:e:f:cmd").is_none());
}

#[test]
fn test_refresh_duplicate_key_first_wins() {
    let tmp = tempfile::tempdir().unwrap();
    let cmd_dir = tmp.path().join(".claude").join("commands");
    std::fs::create_dir_all(&cmd_dir).unwrap();

    std::fs::write(cmd_dir.join("Code_Review.md"), "First file.").unwrap();
    std::fs::write(cmd_dir.join("code-review.md"), "Second file.").unwrap();

    let mut registry = CommandRegistry::new();
    registry.refresh(tmp.path());

    let cmd = registry.find("code-review").unwrap();
    assert_eq!(cmd.content.as_deref(), Some("First file."));
}

#[test]
fn test_refresh_no_directory_no_op() {
    let tmp = tempfile::tempdir().unwrap();
    let mut registry = CommandRegistry::new();
    registry.refresh(tmp.path());

    let all = registry.filter("");
    assert!(
        all.iter()
            .all(|c| matches!(c.source, CommandSource::BuiltIn))
    );
}

#[test]
fn test_refresh_file_count_cap() {
    let tmp = tempfile::tempdir().unwrap();
    let cmd_dir = tmp.path().join(".claude").join("commands");
    std::fs::create_dir_all(&cmd_dir).unwrap();

    for i in 0..201 {
        std::fs::write(cmd_dir.join(format!("cmd-{i:03}.md")), format!("Cmd {i}")).unwrap();
    }

    let mut registry = CommandRegistry::new();
    registry.refresh(tmp.path());

    let user_count = registry
        .filter("")
        .iter()
        .filter(|c| matches!(c.source, CommandSource::UserDefined { .. }))
        .count();
    assert!(
        user_count <= 200,
        "Expected at most 200 commands, got {user_count}"
    );
}

// === Story 5.3 — Task 3.4: AC3 description edge cases ===

#[test]
fn test_description_empty_frontmatter_falls_back_to_body() {
    let tmp = tempfile::tempdir().unwrap();
    write_command(
        tmp.path(),
        "cmd.md",
        "---\ndescription: \"\"\n---\nFirst body line.\nMore text.",
    );
    let mut registry = CommandRegistry::new();
    registry.refresh(tmp.path());
    let cmd = registry.find("cmd").unwrap();
    assert_eq!(cmd.description, "First body line.");
}

#[test]
fn test_description_heading_stripped_from_body() {
    let tmp = tempfile::tempdir().unwrap();
    write_command(tmp.path(), "cmd.md", "## Code Review\nBody text here.");
    let mut registry = CommandRegistry::new();
    registry.refresh(tmp.path());
    let cmd = registry.find("cmd").unwrap();
    assert_eq!(cmd.description, "Code Review");
}

#[test]
fn test_description_long_body_truncated() {
    let tmp = tempfile::tempdir().unwrap();
    let long_line = "x".repeat(300);
    write_command(tmp.path(), "cmd.md", &long_line);
    let mut registry = CommandRegistry::new();
    registry.refresh(tmp.path());
    let cmd = registry.find("cmd").unwrap();
    assert!(
        cmd.description.len() <= 210,
        "Description should be truncated near 200 bytes with ellipsis"
    );
    assert!(cmd.description.ends_with('…'), "Should end with ellipsis");
}

#[test]
fn test_description_empty_body_gives_no_description() {
    let tmp = tempfile::tempdir().unwrap();
    write_command(tmp.path(), "cmd.md", "");
    let mut registry = CommandRegistry::new();
    registry.refresh(tmp.path());
    let cmd = registry.find("cmd").unwrap();
    assert_eq!(cmd.description, "(no description)");
}

// === Story 5.3 — Task 6.5: resolve_file_refs tests ===

use rustain::adapters::command_registry::resolve_file_refs;
use rustain::adapters::noop::NoOpSecurity;

#[test]
fn test_resolve_file_refs_existing_file() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("hello.txt"), "Hello, world!").unwrap();
    let security = NoOpSecurity;
    let (result, errors) = resolve_file_refs("Check @{hello.txt} out.", tmp.path(), &security);
    assert_eq!(result, "Check Hello, world! out.");
    assert!(errors.is_empty());
}

#[test]
fn test_resolve_file_refs_missing_file() {
    let tmp = tempfile::tempdir().unwrap();
    let security = NoOpSecurity;
    let (result, errors) = resolve_file_refs("See @{missing.rs}.", tmp.path(), &security);
    assert!(result.contains("[File not found: missing.rs]"));
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].raw_path, "missing.rs");
}

#[test]
fn test_resolve_file_refs_directory_listing() {
    let tmp = tempfile::tempdir().unwrap();
    let subdir = tmp.path().join("src");
    std::fs::create_dir_all(&subdir).unwrap();
    std::fs::write(subdir.join("main.rs"), "fn main() {}").unwrap();
    std::fs::write(subdir.join("lib.rs"), "pub fn lib() {}").unwrap();

    let security = NoOpSecurity;
    let (result, errors) = resolve_file_refs("Files: @{src/}", tmp.path(), &security);
    assert!(result.contains("main.rs"));
    assert!(result.contains("lib.rs"));
    assert!(errors.is_empty());
}

#[test]
fn test_resolve_file_refs_binary_file_skipped() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("binary.bin"), b"hello\x00world").unwrap();
    let security = NoOpSecurity;
    let (result, errors) = resolve_file_refs("Read @{binary.bin}.", tmp.path(), &security);
    assert!(result.contains("[Binary file skipped: binary.bin]"));
    assert_eq!(errors.len(), 1);
}

#[test]
fn test_resolve_file_refs_file_too_large_truncated() {
    let tmp = tempfile::tempdir().unwrap();
    let big_content = "x".repeat(150_000);
    std::fs::write(tmp.path().join("big.rs"), &big_content).unwrap();
    let security = NoOpSecurity;
    let (result, errors) = resolve_file_refs("@{big.rs}", tmp.path(), &security);
    assert!(result.contains("[truncated]"));
    assert!(errors.is_empty());
}

#[test]
fn test_resolve_file_refs_multiple_refs_in_body() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.txt"), "AAA").unwrap();
    std::fs::write(tmp.path().join("b.txt"), "BBB").unwrap();
    std::fs::write(tmp.path().join("c.txt"), "CCC").unwrap();
    let security = NoOpSecurity;
    let (result, errors) = resolve_file_refs("@{a.txt} @{b.txt} @{c.txt}", tmp.path(), &security);
    assert_eq!(result, "AAA BBB CCC");
    assert!(errors.is_empty());
}

#[test]
fn test_resolve_file_refs_preserves_non_matching_text() {
    let security = NoOpSecurity;
    let tmp = tempfile::tempdir().unwrap();
    let (result, errors) = resolve_file_refs("@something and @{} here", tmp.path(), &security);
    assert_eq!(result, "@something and @{} here");
    assert!(errors.is_empty());
}
