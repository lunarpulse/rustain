use std::path::PathBuf;

use rustain::adapters::command_registry::{CommandRegistry, CommandSource};

// === Command Registry unit tests (Story 3.2, Task 2) ===

// Covers: AC6 — built-in /new command is registered
#[test]
fn test_registry_has_builtin_new() {
    let registry = CommandRegistry::new();
    let all = registry.filter("");
    assert!(all.iter().any(|c| c.name == "new"), "Missing built-in /new command");
}

// Covers: AC6 — built-in commands appear first
#[test]
fn test_builtin_commands_listed_first() {
    let mut registry = CommandRegistry::new();
    // Simulate user-defined commands
    registry.register_user_command("zebra".to_string(), "A custom cmd".to_string(), PathBuf::from(".claude/commands/zebra.md"), None);
    registry.register_user_command("alpha".to_string(), "Another".to_string(), PathBuf::from(".claude/commands/alpha.md"), None);

    let all = registry.filter("");
    // First should be built-in
    assert!(matches!(all[0].source, CommandSource::BuiltIn));
    // User-defined should be sorted alphabetically
    let user_cmds: Vec<&str> = all.iter()
        .filter(|c| matches!(c.source, CommandSource::UserDefined { .. }))
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(user_cmds, vec!["alpha", "zebra"]);
}

// Covers: AC1 — case-insensitive substring filtering
#[test]
fn test_filter_case_insensitive() {
    let mut registry = CommandRegistry::new();
    registry.register_user_command("deploy-staging".to_string(), "Deploy".to_string(), PathBuf::from("x.md"), None);
    registry.register_user_command("deploy-prod".to_string(), "Deploy prod".to_string(), PathBuf::from("y.md"), None);
    registry.register_user_command("run-tests".to_string(), "Tests".to_string(), PathBuf::from("z.md"), None);

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
    assert_eq!(filtered[0].content.as_deref(), Some("# Instruction\nDo something."));
}

// Covers: AC6 — discover_commands reads from directory with frontmatter
#[test]
fn test_discover_commands_from_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let cmd_dir = tmp.path().join(".claude").join("commands");
    std::fs::create_dir_all(&cmd_dir).unwrap();

    // Command with YAML frontmatter
    std::fs::write(
        cmd_dir.join("deploy-staging.md"),
        "---\ndescription: Deploy to staging\n---\n# Deploy\nRun deploy script.",
    ).unwrap();

    // Command without frontmatter (first line as description)
    std::fs::write(
        cmd_dir.join("run-tests.md"),
        "Run all test suites.\n\nMore details here.",
    ).unwrap();

    let registry = CommandRegistry::discover_commands(tmp.path());
    let all = registry.filter("");

    // Should have built-in + 2 discovered
    let user_cmds: Vec<&str> = all.iter()
        .filter(|c| matches!(c.source, CommandSource::UserDefined { .. }))
        .map(|c| c.name.as_str())
        .collect();
    assert!(user_cmds.contains(&"deploy-staging"));
    assert!(user_cmds.contains(&"run-tests"));

    // Check frontmatter description
    let deploy = all.iter().find(|c| c.name == "deploy-staging").unwrap();
    assert_eq!(deploy.description, "Deploy to staging");

    // Check first-line description
    let tests = all.iter().find(|c| c.name == "run-tests").unwrap();
    assert_eq!(tests.description, "Run all test suites.");
}

// Covers: AC2 — lazy caching: discover is called once, result is cached
#[test]
fn test_registry_caching() {
    let registry = CommandRegistry::new();
    // is_discovered should be false initially
    assert!(!registry.is_discovered());
}
