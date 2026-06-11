use std::path::Path;

use rustain::adapters::command_registry::{CommandRegistry, CommandSource, resolve_file_refs};
use rustain::adapters::noop::NoOpSecurity;
use rustain::adapters::tui::app::{InputAction, handle_input};
use rustain::adapters::tui::state::TuiState;
use rustain::domain::events::{DomainInputEvent, DomainKey};
use rustain::domain::models::FocusState;
use rustain::domain::services::command_interpolation::substitute_command_args;

fn write_command(workspace: &Path, rel_path: &str, content: &str) {
    let full = workspace.join(".claude").join("commands").join(rel_path);
    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
    std::fs::write(&full, content).unwrap();
}

fn setup_state_with_command(rel_path: &str, body: &str) -> (TuiState, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    write_command(tmp.path(), rel_path, body);
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Input;
    (state, tmp)
}

fn type_text(state: &mut TuiState, text: &str) {
    for ch in text.chars() {
        handle_input(state, &DomainInputEvent::KeyPress(ch));
    }
    if state.autocomplete.active {
        state.autocomplete.dismiss();
    }
}

fn press_enter(state: &mut TuiState) -> InputAction {
    handle_input(state, &DomainInputEvent::SpecialKey(DomainKey::Enter))
}

#[test]
fn test_submit_custom_command_with_args_produces_submit_with_context() {
    let (mut state, _tmp) = setup_state_with_command("review.md", "Review code.");
    type_text(&mut state, "/review src/main.rs");
    let action = press_enter(&mut state);
    match action {
        InputAction::SubmitWithContext {
            text,
            command,
            command_args,
        } => {
            assert_eq!(text, "");
            assert_eq!(command, Some("review".to_string()));
            assert_eq!(command_args, Some("src/main.rs".to_string()));
        }
        _ => panic!("Expected SubmitWithContext, got {:?}", action),
    }
}

#[test]
fn test_submit_custom_command_no_args() {
    let (mut state, _tmp) = setup_state_with_command("review.md", "Review code.");
    type_text(&mut state, "/review");
    let action = press_enter(&mut state);
    match action {
        InputAction::SubmitWithContext {
            text,
            command,
            command_args,
        } => {
            assert_eq!(text, "");
            assert_eq!(command, Some("review".to_string()));
            assert_eq!(command_args, None);
        }
        _ => panic!("Expected SubmitWithContext, got {:?}", action),
    }
}

#[test]
fn test_submit_custom_command_whitespace_args_normalized_to_none() {
    let (mut state, _tmp) = setup_state_with_command("review.md", "Review code.");
    type_text(&mut state, "/review   ");
    let action = press_enter(&mut state);
    match action {
        InputAction::SubmitWithContext { command_args, .. } => {
            assert_eq!(command_args, None);
        }
        _ => panic!("Expected SubmitWithContext, got {:?}", action),
    }
}

#[test]
fn test_submit_namespaced_command() {
    let (mut state, _tmp) = setup_state_with_command("deploy/staging.md", "Deploy staging.");
    type_text(&mut state, "/deploy:staging prod-v2");
    let action = press_enter(&mut state);
    match action {
        InputAction::SubmitWithContext {
            text,
            command,
            command_args,
        } => {
            assert_eq!(text, "");
            assert_eq!(command, Some("deploy:staging".to_string()));
            assert_eq!(command_args, Some("prod-v2".to_string()));
        }
        _ => panic!("Expected SubmitWithContext, got {:?}", action),
    }
}

#[test]
fn test_args_interpolation() {
    assert_eq!(
        substitute_command_args("Review {{args}} now.", "src/main.rs"),
        "Review src/main.rs now."
    );
}

#[test]
fn test_args_no_placeholder_appends() {
    assert_eq!(
        substitute_command_args("Review code.", "src/main.rs"),
        "Review code.\n\nsrc/main.rs"
    );
}

#[test]
fn test_file_ref_interpolation_existing_file() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("main.rs"), "fn main() {}").unwrap();
    let security = NoOpSecurity;
    let (result, errors) = resolve_file_refs("@{main.rs}", tmp.path(), &security);
    assert_eq!(result, "fn main() {}");
    assert!(errors.is_empty());
}

#[test]
fn test_file_ref_interpolation_missing_file() {
    let tmp = tempfile::tempdir().unwrap();
    let security = NoOpSecurity;
    let (result, errors) = resolve_file_refs("@{missing.rs}", tmp.path(), &security);
    assert!(result.contains("[File not found: missing.rs]"));
    assert_eq!(errors.len(), 1);
}

#[test]
fn test_user_command_shadowed_by_builtin_dispatches_to_builtin() {
    let (mut state, _tmp) = setup_state_with_command("new.md", "Custom new.");
    type_text(&mut state, "/new");
    let action = press_enter(&mut state);
    match action {
        InputAction::ExecuteCommand { name, .. } => {
            assert_eq!(name, "new");
        }
        _ => panic!(
            "Expected ExecuteCommand for built-in 'new', got {:?}",
            action
        ),
    }
}

#[test]
fn test_builtin_mode_command_unaffected() {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Input;
    type_text(&mut state, "/mode plan");
    let action = press_enter(&mut state);
    match action {
        InputAction::ExecuteCommand { name, args } => {
            assert_eq!(name, "mode");
            assert_eq!(args, Some("plan".to_string()));
        }
        _ => panic!(
            "Expected ExecuteCommand for built-in 'mode', got {:?}",
            action
        ),
    }
}

#[test]
fn test_refresh_adds_newly_created_file() {
    let tmp = tempfile::tempdir().unwrap();
    let mut registry = CommandRegistry::new();
    registry.refresh(tmp.path());
    assert!(registry.find("review").is_none());

    write_command(tmp.path(), "review.md", "Review code.");
    registry.refresh(tmp.path());
    assert!(registry.find("review").is_some());
}

#[test]
fn test_refresh_removes_deleted_file() {
    let tmp = tempfile::tempdir().unwrap();
    write_command(tmp.path(), "review.md", "Review code.");
    let mut registry = CommandRegistry::new();
    registry.refresh(tmp.path());
    assert!(registry.find("review").is_some());

    std::fs::remove_file(tmp.path().join(".claude/commands/review.md")).unwrap();
    registry.refresh(tmp.path());
    assert!(registry.find("review").is_none());
}

#[test]
fn test_refresh_no_commands_dir_is_noop() {
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
fn test_file_ref_absolute_path_resolved() {
    let tmp = tempfile::tempdir().unwrap();
    let security = NoOpSecurity;
    let (result, errors) = resolve_file_refs("@{/etc/hostname}", tmp.path(), &security);
    let content = std::fs::read_to_string("/etc/hostname").unwrap_or_default();
    if content.is_empty() {
        // /etc/hostname exists but is empty on this system
        assert!(result.is_empty(), "Expected empty result for empty file");
        assert!(
            errors.is_empty(),
            "Expected no errors for existing empty file"
        );
    } else {
        assert!(result.contains(&content), "Expected file content in result");
        assert!(errors.is_empty(), "Expected no errors for readable file");
    }
}

#[test]
fn test_file_ref_directory_listing() {
    let tmp = tempfile::tempdir().unwrap();
    let subdir = tmp.path().join("subdir");
    std::fs::create_dir_all(&subdir).unwrap();
    std::fs::write(subdir.join("a.txt"), "aaa").unwrap();
    std::fs::write(subdir.join("b.txt"), "bbb").unwrap();
    let security = NoOpSecurity;
    let (result, errors) = resolve_file_refs("@{subdir/}", tmp.path(), &security);
    assert!(result.contains("a.txt"));
    assert!(result.contains("b.txt"));
    assert!(errors.is_empty());
}
