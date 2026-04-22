use rustain::domain::services::command_normalize::{
    MAX_COMMAND_NAMESPACE_DEPTH, normalize_command_component, normalize_command_path,
};
use std::path::PathBuf;

#[test]
fn test_normalize_review() {
    assert_eq!(normalize_command_component("review"), Some("review".into()));
}

#[test]
fn test_normalize_code_review() {
    assert_eq!(
        normalize_command_component("Code Review"),
        Some("code-review".into())
    );
}

#[test]
fn test_normalize_underscore() {
    assert_eq!(
        normalize_command_component("deploy_prod"),
        Some("deploy-prod".into())
    );
}

#[test]
fn test_normalize_multiple_spaces() {
    assert_eq!(
        normalize_command_component("Run   Tests"),
        Some("run-tests".into())
    );
}

#[test]
fn test_normalize_double_underscore() {
    assert_eq!(
        normalize_command_component("deploy__staging"),
        Some("deploy-staging".into())
    );
}

#[test]
fn test_normalize_special_chars_stripped() {
    assert_eq!(
        normalize_command_component("Review!v2"),
        Some("reviewv2".into())
    );
}

#[test]
fn test_normalize_all_special_returns_none() {
    assert_eq!(normalize_command_component("___"), None);
    assert_eq!(normalize_command_component("!@#"), None);
}

#[test]
fn test_normalize_readme() {
    assert_eq!(normalize_command_component("README"), Some("readme".into()));
}

#[test]
fn test_normalize_hidden_file() {
    assert_eq!(
        normalize_command_component(".hidden"),
        Some("hidden".into())
    );
}

#[test]
fn test_normalize_depth_limit() {
    assert_eq!(MAX_COMMAND_NAMESPACE_DEPTH, 5);
}

#[test]
fn test_path_flat_file() {
    let root = PathBuf::from("/ws/.claude/commands");
    let file = PathBuf::from("/ws/.claude/commands/review.md");
    assert_eq!(normalize_command_path(&root, &file), Some("review".into()));
}

#[test]
fn test_path_one_level_namespace() {
    let root = PathBuf::from("/ws/.claude/commands");
    let file = PathBuf::from("/ws/.claude/commands/deploy/staging.md");
    assert_eq!(
        normalize_command_path(&root, &file),
        Some("deploy:staging".into())
    );
}

#[test]
fn test_path_exactly_at_depth_limit() {
    let root = PathBuf::from("/ws/.claude/commands");
    let file = PathBuf::from("/ws/.claude/commands/a/b/c/d/e/cmd.md");
    assert_eq!(
        normalize_command_path(&root, &file),
        Some("a:b:c:d:e:cmd".into())
    );
}

#[test]
fn test_path_exceeds_depth_limit() {
    let root = PathBuf::from("/ws/.claude/commands");
    let file = PathBuf::from("/ws/.claude/commands/a/b/c/d/e/f/cmd.md");
    assert_eq!(normalize_command_path(&root, &file), None);
}
