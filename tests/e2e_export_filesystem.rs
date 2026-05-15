//! Filesystem-level E2E tests for /export (Story 4-4 AC11, AC12).
//!
//! These tests exercise `apply_export_command` with a real tempdir
//! workspace so the full path-resolution + atomic-write + overlay flow
//! is verified end-to-end.
//!
//! Required by the second-audit Fix 10 — the existing e2e_export.rs file
//! tests only the pure `render_conversation_markdown` serializer and
//! explicitly admits it does not cover the event-loop filesystem path.
//! This file closes that gap for the security-critical scenarios:
//!
//!   - AC11 auto-number collision walks
//!   - AC11 explicit-path write to an existing-parent directory
//!   - AC12 overwrite-confirmation y / n flows
//!   - AC12 no-arg-never-prompts regression guard
//!   - Path-traversal rejection (absolute, `..`, canonical escape)

use std::path::PathBuf;
use tempfile::TempDir;

use rustain::adapters::tui::state::TuiState;
use rustain::domain::models::conversation::{ChatMessage, Conversation};
use rustain::domain::models::visual::{ConfirmationType, OverlayType};
use rustain::domain::models::{FocusState, MessageRole, SessionMeta, StatusState};
use rustain::infrastructure::runtime::event_loop::{
    apply_cancel_export_overwrite, apply_confirm_export_overwrite, apply_export_command,
};

// ── Fixtures ───────────────────────────────────────────────────────────────

fn msg(role: MessageRole, content: &str) -> ChatMessage {
    ChatMessage {
        synthetic: false,
        id: "m".to_string(),
        role,
        content: content.to_string(),
        content_blocks: vec![],
        tool_calls: vec![],
        created_at: 1_700_000_000,
        token_count: None,
        stop_reason: None,
        images: vec![],
    }
}

fn conv(title: &str) -> Conversation {
    Conversation {
        id: "conv-ffeeddcc-test".to_string(),
        title: title.to_string(),
        messages: vec![
            msg(MessageRole::User, "hello world"),
            msg(MessageRole::Assistant, "hi there"),
        ],
        turns: Vec::new(),
        created_at: 1_700_000_000,
        updated_at: 1_700_000_060,
        last_response_at: None,
        session_id: None,
        usage: None,
        plans: std::collections::HashMap::new(),
        fork_source: None,
        compaction: None,
    }
}

fn meta() -> SessionMeta {
    SessionMeta {
        version: 1,
        title: "Test".to_string(),
        created_at: 1_700_000_000,
        updated_at: 1_700_000_060,
        message_count: 2,
        bookmarks: vec![],
        fork_source: None,
        imported_from: None,
        extra: serde_json::Map::new(),
        plan_slug: None,
    }
}

fn state() -> TuiState {
    let mut s = TuiState::new(80, 24);
    s.focus = FocusState::Chat;
    s
}

/// Drain a Flash message from state if present.
fn flash_message(s: &TuiState) -> Option<&str> {
    if let StatusState::Flash { message, .. } = &s.status {
        Some(message.as_str())
    } else {
        None
    }
}

// ── AC11: auto-number on collision ─────────────────────────────────────────

#[tokio::test]
async fn test_e2e_export_auto_number_on_collision() {
    let tmp = TempDir::new().unwrap();
    let workspace = tmp.path();
    let exports_dir = workspace.join(".rustain").join("exports");

    let c = conv("My Chat");
    let m = meta();
    let mut s = state();

    // First export — writes my-chat.md
    apply_export_command(None, &c, &m, workspace, &mut s).await;
    let first = exports_dir.join("my-chat.md");
    assert!(first.exists(), "first auto-number export must exist");
    assert!(
        flash_message(&s).unwrap_or("").contains("Exported to"),
        "expected success flash, got: {:?}",
        flash_message(&s)
    );

    // Second export — writes my-chat-2.md (auto-number on collision)
    apply_export_command(None, &c, &m, workspace, &mut s).await;
    let second = exports_dir.join("my-chat-2.md");
    assert!(second.exists(), "second auto-number export must exist");

    // Third export — writes my-chat-3.md
    apply_export_command(None, &c, &m, workspace, &mut s).await;
    let third = exports_dir.join("my-chat-3.md");
    assert!(third.exists(), "third auto-number export must exist");
}

#[tokio::test]
async fn test_e2e_export_no_arg_never_prompts() {
    // AC12 regression guard: auto-number mode must NEVER open the confirmation
    // overlay, even after multiple collisions. Party-mode second-audit Fix 10.
    let tmp = TempDir::new().unwrap();
    let workspace = tmp.path();

    let c = conv("Never Prompt");
    let m = meta();
    let mut s = state();

    for _ in 0..5 {
        apply_export_command(None, &c, &m, workspace, &mut s).await;
        assert!(
            s.pending_export.is_none(),
            "auto-number mode must not set pending_export"
        );
        assert_ne!(
            s.focus,
            FocusState::Overlay(OverlayType::Confirmation(
                ConfirmationType::ExportOverwrite(PathBuf::from("dummy"))
            )),
            "auto-number mode must not open overwrite confirmation overlay"
        );
    }
}

// ── AC12: overwrite confirmation overlay ───────────────────────────────────

#[tokio::test]
async fn test_e2e_export_explicit_path_overwrite_opens_confirmation_overlay() {
    let tmp = TempDir::new().unwrap();
    let workspace = tmp.path();
    let exports_dir = workspace.join(".rustain").join("exports");

    let c = conv("Overwrite Test");
    let m = meta();
    let mut s = state();

    // Pre-create the target file so the next export collides.
    std::fs::create_dir_all(&exports_dir).unwrap();
    let target = exports_dir.join("notes.md");
    std::fs::write(&target, "original").unwrap();

    apply_export_command(Some("notes.md"), &c, &m, workspace, &mut s).await;

    assert!(
        s.pending_export.is_some(),
        "explicit-path collision must stash pending_export"
    );
    let (stashed_path, stashed_content) = s.pending_export.as_ref().unwrap();
    assert_eq!(stashed_path, &target);
    assert!(!stashed_content.is_empty(), "content must be pre-rendered");
    assert!(
        matches!(
            s.focus,
            FocusState::Overlay(OverlayType::Confirmation(
                ConfirmationType::ExportOverwrite(_)
            ))
        ),
        "explicit-path collision must open ExportOverwrite overlay"
    );

    // Original file unchanged until user confirms.
    let content = std::fs::read_to_string(&target).unwrap();
    assert_eq!(content, "original");
}

#[tokio::test]
async fn test_e2e_export_explicit_path_no_collision_writes_immediately() {
    // AC12: when the target does NOT exist, explicit-path mode must write
    // atomically without opening an overlay.
    let tmp = TempDir::new().unwrap();
    let workspace = tmp.path();
    let exports_dir = workspace.join(".rustain").join("exports");

    let c = conv("Fresh Target");
    let m = meta();
    let mut s = state();

    apply_export_command(Some("fresh.md"), &c, &m, workspace, &mut s).await;

    let target = exports_dir.join("fresh.md");
    assert!(target.exists(), "fresh target must be written immediately");
    assert!(
        s.pending_export.is_none(),
        "no collision → no pending_export"
    );
    assert_eq!(
        s.focus,
        FocusState::Chat,
        "no overlay opened on fresh target"
    );
}

// ── Path traversal regression tests (Fix 27) ─────────────────────────────

#[tokio::test]
async fn test_e2e_export_rejects_absolute_path() {
    let tmp = TempDir::new().unwrap();
    let workspace = tmp.path();

    let c = conv("Traversal");
    let m = meta();
    let mut s = state();

    // /tmp/evil.md — absolute path, must be rejected.
    apply_export_command(Some("/tmp/evil.md"), &c, &m, workspace, &mut s).await;

    // The flash message must indicate rejection.
    let flash = flash_message(&s).unwrap_or("");
    assert!(
        flash.contains("absolute paths are not allowed"),
        "expected absolute-path rejection flash, got: {}",
        flash
    );
    assert!(
        !PathBuf::from("/tmp/evil.md").exists()
            || std::fs::read_to_string("/tmp/evil.md")
                .map(|c| !c.contains("hello world"))
                .unwrap_or(true),
        "absolute path must not be written"
    );
}

#[tokio::test]
async fn test_e2e_export_rejects_dotdot_components() {
    let tmp = TempDir::new().unwrap();
    let workspace = tmp.path();

    let c = conv("Traversal");
    let m = meta();
    let mut s = state();

    apply_export_command(Some("../outside.md"), &c, &m, workspace, &mut s).await;

    let flash = flash_message(&s).unwrap_or("");
    assert!(
        flash.contains("'..'") || flash.contains("not allowed"),
        "expected dotdot rejection flash, got: {}",
        flash
    );

    // Ensure nothing was written outside the exports dir.
    let parent_dir = workspace.join(".rustain");
    let outside = parent_dir.join("outside.md");
    assert!(!outside.exists(), "dotdot-escaped file must not be written");
}

#[tokio::test]
async fn test_e2e_export_rejects_nested_dotdot() {
    let tmp = TempDir::new().unwrap();
    let workspace = tmp.path();

    let c = conv("Traversal");
    let m = meta();
    let mut s = state();

    apply_export_command(Some("subdir/../../escape.md"), &c, &m, workspace, &mut s).await;

    let flash = flash_message(&s).unwrap_or("");
    assert!(
        flash.contains("'..'") || flash.contains("not allowed"),
        "expected nested-dotdot rejection flash, got: {}",
        flash
    );
}

#[tokio::test]
async fn test_e2e_export_writes_to_rustain_exports_subfolder() {
    // AC11 regression guard: export always writes to {workspace}/.rustain/exports/
    // not the workspace root.
    let tmp = TempDir::new().unwrap();
    let workspace = tmp.path();

    let c = conv("Subfolder Test");
    let m = meta();
    let mut s = state();

    apply_export_command(None, &c, &m, workspace, &mut s).await;

    let exports_dir = workspace.join(".rustain").join("exports");
    assert!(
        exports_dir.exists(),
        ".rustain/exports/ must be auto-created"
    );
    let entries: Vec<_> = std::fs::read_dir(&exports_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    assert_eq!(
        entries.len(),
        1,
        "exactly one .md file should exist in .rustain/exports/"
    );
    let workspace_root_entries: Vec<_> = std::fs::read_dir(workspace)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map(|ext| ext == "md").unwrap_or(false))
        .collect();
    assert!(
        workspace_root_entries.is_empty(),
        "no .md file should exist at the workspace root"
    );
}

// ── AC12: y/n handler behavior (third-audit Fix R7) ─────────────────────

#[tokio::test]
async fn test_e2e_confirm_export_overwrite_writes_atomically() {
    // AC12: pressing `y` on the ExportOverwrite confirmation overlay must
    // atomically overwrite the existing file with the pre-staged content.
    //
    // Third-audit Fix R7: exercises `apply_confirm_export_overwrite`
    // directly — the handler was previously only reachable via the full
    // event loop dispatch and had zero automated coverage.
    let tmp = TempDir::new().unwrap();
    let target = tmp.path().join("notes.md");

    // Pre-create the target file with original content.
    std::fs::write(&target, "ORIGINAL CONTENT").unwrap();

    // Stash the pre-rendered new content in pending_export (mimicking
    // what apply_export_command does before opening the overlay).
    let mut s = state();
    s.pending_export = Some((target.clone(), "NEW CONTENT".to_string()));
    s.focus = FocusState::Overlay(OverlayType::Confirmation(
        ConfirmationType::ExportOverwrite(target.clone()),
    ));

    apply_confirm_export_overwrite(&mut s).await;

    // File now contains the new content — atomic overwrite succeeded.
    let on_disk = std::fs::read_to_string(&target).unwrap();
    assert_eq!(on_disk, "NEW CONTENT", "target file must be overwritten");

    // pending_export is cleared, focus restored, flash set.
    assert!(s.pending_export.is_none(), "pending_export must be cleared");
    assert_eq!(s.focus, FocusState::Chat, "focus must return to Chat");
    let flash = flash_message(&s).unwrap_or("");
    assert!(
        flash.contains("Overwrote"),
        "expected 'Overwrote' success flash, got: {}",
        flash
    );

    // No lingering .md.tmp file.
    let tmp_path = target.with_extension("md.tmp");
    assert!(
        !tmp_path.exists(),
        ".md.tmp sidecar must be renamed or cleaned up"
    );
}

#[tokio::test]
async fn test_e2e_cancel_export_overwrite_leaves_file_unchanged() {
    // AC12: pressing `n` (or Esc) on the ExportOverwrite confirmation
    // overlay must leave the existing file untouched and clear the
    // pre-staged pending_export.
    //
    // Third-audit Fix R7: exercises `apply_cancel_export_overwrite`
    // directly to close the no-coverage gap.
    let tmp = TempDir::new().unwrap();
    let target = tmp.path().join("notes.md");

    std::fs::write(&target, "UNTOUCHED ORIGINAL").unwrap();
    let mtime_before = std::fs::metadata(&target).unwrap().modified().unwrap();

    let mut s = state();
    s.pending_export = Some((target.clone(), "NEW CONTENT WE WILL DROP".to_string()));
    s.focus = FocusState::Overlay(OverlayType::Confirmation(
        ConfirmationType::ExportOverwrite(target.clone()),
    ));

    apply_cancel_export_overwrite(&mut s);

    // File is still the original content.
    let on_disk = std::fs::read_to_string(&target).unwrap();
    assert_eq!(
        on_disk, "UNTOUCHED ORIGINAL",
        "target file must NOT be modified on cancel"
    );

    // Modification time has not changed.
    let mtime_after = std::fs::metadata(&target).unwrap().modified().unwrap();
    assert_eq!(
        mtime_before, mtime_after,
        "mtime must not change when export is cancelled"
    );

    // pending_export is cleared, focus restored, cancelled flash set.
    assert!(
        s.pending_export.is_none(),
        "pending_export must be cleared on cancel"
    );
    assert_eq!(s.focus, FocusState::Chat);
    let flash = flash_message(&s).unwrap_or("");
    assert!(
        flash.contains("Export cancelled"),
        "expected 'Export cancelled' flash, got: {}",
        flash
    );
}

#[tokio::test]
async fn test_e2e_confirm_export_overwrite_with_no_pending_is_noop() {
    // Defensive: if `pending_export` is `None` (e.g. double-confirm), the
    // handler must not panic and must restore focus to Chat.
    let mut s = state();
    s.pending_export = None;
    s.focus = FocusState::Overlay(OverlayType::Confirmation(
        ConfirmationType::ExportOverwrite(PathBuf::from("nonexistent")),
    ));

    apply_confirm_export_overwrite(&mut s).await;

    assert!(s.pending_export.is_none());
    assert_eq!(s.focus, FocusState::Chat);
}
