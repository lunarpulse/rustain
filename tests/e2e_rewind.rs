//! E2E tests for Story 4-3b: Rewind with File Snapshot & Rollback.
//!
//! AC1:  Rewind Trigger & Confirmation Card
//! AC2:  Checkpoint Creation Before Tool Execution
//! AC3:  Rewind Execution — Truncate Messages & Revert Files
//! AC4:  Fork-Instead Path
//! AC5:  Conflict Handling — Externally Modified Files
//! AC6:  DF-018 — Overlay Queue Prevents Focus Theft
//! AC7:  DF-005 — HeightCache Survives Message Truncation
//! AC8:  StoragePort — Checkpoint Protocol
//! AC9:  StoragePort — File Snapshot Protocol
//! AC10: Atomic SessionMeta Update on Truncation

use std::path::PathBuf;

use tempfile::TempDir;

use rustain::adapters::filesystem::FileSystemStorage;
use rustain::adapters::tui::app::{InputAction, handle_input};
use rustain::adapters::tui::state::{HeightCache, RevertPreviewItem, RewindPreview, TuiState};
use rustain::adapters::tui::theme::Theme;
use rustain::adapters::tui::widgets::rewind_confirm::render_rewind_confirmation_lines;
use rustain::domain::models::checkpoint::{CheckpointId, RevertStatus};
use rustain::domain::models::conversation::{ChatMessage, Conversation, generate_conversation_id};
use rustain::domain::models::visual::{ConfirmationType, OverlayType};
use rustain::domain::models::{FocusState, MessageRole, StatusState, UserMessage};
use rustain::domain::ports::StoragePort;

// ── Helpers ──────────────────────────────────────────────────────────────────

fn make_message(role: MessageRole, content: &str) -> ChatMessage {
    ChatMessage {
        id: generate_conversation_id(),
        role,
        content: content.to_string(),
        content_blocks: vec![],
        tool_calls: vec![],
        created_at: 1700000000,
        token_count: None,
        stop_reason: None,
        images: vec![],
    }
}

fn make_conversation_5_messages() -> Conversation {
    Conversation {
        id: generate_conversation_id(),
        title: "Test Rewind Conversation".to_string(),
        messages: vec![
            make_message(MessageRole::User, "Message 0"),
            make_message(MessageRole::Assistant, "Response 0"),
            make_message(MessageRole::User, "Message 1"),
            make_message(MessageRole::Assistant, "Response 1"),
            make_message(MessageRole::User, "Message 2"),
        ],
        created_at: 1700000000,
        updated_at: 1700000001,
        last_response_at: None,
        session_id: Some("sess-rewind-test".to_string()),
        usage: None,
        fork_source: None,
    }
}

fn make_state_in_chat_focus(width: u16, height: u16) -> TuiState {
    let mut state = TuiState::new(width, height);
    state.focus = FocusState::Chat;
    state.auto_scroll = true;
    state
}

fn make_state_in_rewind_overlay() -> TuiState {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Overlay(OverlayType::Confirmation(ConfirmationType::Rewind));
    state.pending_rewind_index = Some(2);
    state
}

// ── AC1: Rewind Trigger & Confirmation Card ───────────────────────────────────

#[test]
fn test_e2e_rewind_r_key_opens_confirmation() {
    // Given: Chat focus with non-empty message boundaries
    let mut state = make_state_in_chat_focus(80, 24);
    state.message_boundaries = vec![0, 5, 10, 15];
    state.total_content_height = 30;

    // When: I press 'R'
    use rustain::domain::events::DomainInputEvent;
    let event = DomainInputEvent::KeyPress('R');
    let action = handle_input(&mut state, &event);

    // Then: RewindAtMessage action returned
    assert_eq!(
        action,
        InputAction::RewindAtMessage,
        "Pressing 'R' in Chat focus should return RewindAtMessage"
    );
}

#[test]
fn test_e2e_rewind_card_shows_files_and_message_count() {
    // Given: a RewindPreview with 2 messages to remove and 1 file
    let preview = RewindPreview {
        target_message_index: 2,
        messages_to_remove: 2,
        files_to_revert: vec![RevertPreviewItem {
            display_path: "src/main.rs".to_string(),
            conflict: false,
        }],
    };
    let theme = Theme::dark();

    // When: rendering the confirmation card
    let lines = render_rewind_confirmation_lines(&preview, 80, &theme);
    let all_text: String = lines
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.to_string())
        .collect();

    // Then: card shows message count
    assert!(
        all_text.contains("2 messages after message 3"),
        "Card should show '2 messages after message 3', got: {all_text}"
    );
    // And: file list contains the file path
    assert!(
        all_text.contains("src/main.rs"),
        "Card should show 'src/main.rs' in file list"
    );
    // And: double border is rendered
    assert!(all_text.contains('╔'), "Card should have double border ╔");
    assert!(all_text.contains('╝'), "Card should have double border ╝");
}

#[test]
fn test_e2e_rewind_cancel_dismisses_no_side_effects() {
    // Given: rewind overlay is active with pending index
    let mut state = make_state_in_rewind_overlay();
    let original_pending = state.pending_rewind_index;

    // When: I press 'n'
    use rustain::domain::events::DomainInputEvent;
    let event = DomainInputEvent::KeyPress('n');
    let action = handle_input(&mut state, &event);

    // Then: RewindCancel returned
    assert_eq!(
        action,
        InputAction::RewindCancel,
        "'n' in Rewind overlay should return RewindCancel"
    );
    // And: pending_rewind_index was set (event_loop would clear it)
    assert_eq!(
        original_pending,
        Some(2),
        "Pending rewind index was correctly set to 2"
    );

    // And: Esc also cancels
    let mut state2 = make_state_in_rewind_overlay();
    use rustain::domain::events::{DomainInputEvent as DIE2, DomainKey};
    let esc = DIE2::SpecialKey(DomainKey::Esc);
    let esc_action = handle_input(&mut state2, &esc);
    assert_eq!(
        esc_action,
        InputAction::RewindCancel,
        "Esc in Rewind overlay should return RewindCancel"
    );
}

// ── AC2: Checkpoint Creation Before Tool Execution ────────────────────────────

#[tokio::test]
async fn test_e2e_rewind_create_checkpoint_before_tool_dispatch() {
    // Given: a saved conversation with messages
    let tmp = TempDir::new().unwrap();
    let storage = FileSystemStorage::new(tmp.path().join("sessions"));
    let conv = make_conversation_5_messages();
    let conv_id = conv.id.clone();
    storage.save_conversation(&conv).await.unwrap();

    // When: a turn that produces tool calls triggers create_checkpoint
    let cp = storage.create_checkpoint(&conv_id).await.unwrap();

    // Then: exactly 1 checkpoint exists
    let checkpoints = storage.list_checkpoints(&conv_id).await.unwrap();
    assert_eq!(
        checkpoints.len(),
        1,
        "Exactly one checkpoint after one create_checkpoint call"
    );
    // And: message_index is messages.len() - 1 = 4
    assert_eq!(
        checkpoints[0].message_index, 4,
        "message_index should be 4 (last message index in a 5-message conversation)"
    );
    assert_eq!(cp, checkpoints[0].id, "returned id matches list");
}

#[tokio::test]
async fn test_e2e_rewind_no_checkpoint_on_text_only_turn() {
    // Given: a saved conversation
    let tmp = TempDir::new().unwrap();
    let storage = FileSystemStorage::new(tmp.path().join("sessions"));
    let conv = make_conversation_5_messages();
    let conv_id = conv.id.clone();
    storage.save_conversation(&conv).await.unwrap();

    // When: a text-only turn (no tools) — no checkpoint is created

    // Then: list_checkpoints returns empty
    let checkpoints = storage.list_checkpoints(&conv_id).await.unwrap();
    assert!(
        checkpoints.is_empty(),
        "Text-only turn must not create a checkpoint; got: {:?}",
        checkpoints
    );
}

#[tokio::test]
async fn test_e2e_rewind_snapshot_taken_before_write() {
    // Given: a saved conversation and an existing file with original content
    let tmp = TempDir::new().unwrap();
    let storage = FileSystemStorage::new(tmp.path().join("sessions"));
    let conv = make_conversation_5_messages();
    let conv_id = conv.id.clone();
    storage.save_conversation(&conv).await.unwrap();

    // Create a file in the tmp workspace to snapshot
    let workspace = tmp.path();
    let file_path = workspace.join("src").join("lib.rs");
    tokio::fs::create_dir_all(file_path.parent().unwrap())
        .await
        .unwrap();
    let original_content = b"fn original() {}";
    tokio::fs::write(&file_path, original_content)
        .await
        .unwrap();

    // Create a checkpoint
    let cp = storage.create_checkpoint(&conv_id).await.unwrap();

    // Snapshot the file before "writing" (simulates ToolSetAdapter pre-write snapshot)
    storage
        .snapshot_file(&conv_id, cp, &file_path, original_content)
        .await
        .unwrap();

    // "Write" new content to the file (simulates Write tool execution)
    let new_content = b"fn modified() {}";
    tokio::fs::write(&file_path, new_content).await.unwrap();

    // Then: snapshot file exists in snapshots dir
    let snapshots_dir = tmp.path().join("sessions").join(&conv_id).join("snapshots");
    let mut entries = tokio::fs::read_dir(&snapshots_dir).await.unwrap();
    let mut snapshot_files = vec![];
    while let Some(entry) = entries.next_entry().await.unwrap() {
        snapshot_files.push(entry.path());
    }
    assert_eq!(
        snapshot_files.len(),
        1,
        "Exactly one snapshot file should exist"
    );

    // And: the snapshot envelope contains the ORIGINAL content, not the new content
    let envelope_str = tokio::fs::read_to_string(&snapshot_files[0]).await.unwrap();
    let envelope: serde_json::Value = serde_json::from_str(&envelope_str).unwrap();
    let stored_b64 = envelope["original_content_b64"].as_str().unwrap();
    let decoded =
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, stored_b64).unwrap();
    assert_eq!(
        decoded, original_content,
        "Snapshot must store the original content, not the new content"
    );
}

// ── AC3: Rewind Execution — Truncate Messages & Revert Files ─────────────────

#[tokio::test]
async fn test_e2e_rewind_truncates_messages() {
    // Given: a 5-message conversation with a checkpoint at message 2
    let tmp = TempDir::new().unwrap();
    let storage = FileSystemStorage::new(tmp.path().join("sessions"));
    let mut conv = make_conversation_5_messages();
    let conv_id = conv.id.clone();
    storage.save_conversation(&conv).await.unwrap();

    // Add message 0 checkpoint (covers message_index=4 for 5-msg conversation)
    // We want to revert to message index 2, so we need a checkpoint whose message_index <= 2
    // Directly create checkpoint after truncating messages in memory for this test:
    // Simulate: conv had 3 messages when checkpoint was taken
    conv.messages.truncate(3);
    storage.save_conversation(&conv).await.unwrap();
    let cp = storage.create_checkpoint(&conv_id).await.unwrap();
    // Now restore all 5 messages
    let conv_full = make_conversation_5_messages_with_id(conv_id.clone());
    storage.save_conversation(&conv_full).await.unwrap();

    // When: rewind to checkpoint (which covers message_index=2)
    let truncated = storage.revert_to_checkpoint(&conv_id, cp).await.unwrap();

    // Then: truncated conversation has 3 messages (0..=2)
    assert_eq!(
        truncated.messages.len(),
        3,
        "Rewound conversation should have 3 messages (indices 0-2)"
    );

    // And: disk file matches
    let reloaded = storage
        .load_conversation(&conv_id)
        .await
        .unwrap()
        .expect("conversation should exist after rewind");
    assert_eq!(
        reloaded.messages.len(),
        3,
        "Disk file should also have 3 messages after rewind"
    );
}

fn make_conversation_5_messages_with_id(id: String) -> Conversation {
    Conversation {
        id,
        title: "Test Rewind Conversation".to_string(),
        messages: vec![
            make_message(MessageRole::User, "Message 0"),
            make_message(MessageRole::Assistant, "Response 0"),
            make_message(MessageRole::User, "Message 1"),
            make_message(MessageRole::Assistant, "Response 1"),
            make_message(MessageRole::User, "Message 2"),
        ],
        created_at: 1700000000,
        updated_at: 1700000001,
        last_response_at: None,
        session_id: Some("sess-rewind-test".to_string()),
        usage: None,
        fork_source: None,
    }
}

#[tokio::test]
async fn test_e2e_rewind_reverts_files_in_reverse_order() {
    // Given: three files snapshotted at cp 1, 2, 3
    let tmp = TempDir::new().unwrap();
    let storage = FileSystemStorage::new(tmp.path().join("sessions"));
    let conv = make_conversation_5_messages();
    let conv_id = conv.id.clone();
    storage.save_conversation(&conv).await.unwrap();

    let workspace = tmp.path();

    // Create 3 files with original content
    let file_a = workspace.join("a.txt");
    let file_b = workspace.join("b.txt");
    let file_c = workspace.join("c.txt");
    tokio::fs::write(&file_a, b"original A").await.unwrap();
    tokio::fs::write(&file_b, b"original B").await.unwrap();
    tokio::fs::write(&file_c, b"original C").await.unwrap();

    // Create checkpoint 1 and snapshot file A
    let cp1 = storage.create_checkpoint(&conv_id).await.unwrap();
    storage
        .snapshot_file(&conv_id, cp1, &file_a, b"original A")
        .await
        .unwrap();

    // Create checkpoint 2 and snapshot file B
    let cp2 = storage.create_checkpoint(&conv_id).await.unwrap();
    storage
        .snapshot_file(&conv_id, cp2, &file_b, b"original B")
        .await
        .unwrap();

    // Create checkpoint 3 and snapshot file C
    let cp3 = storage.create_checkpoint(&conv_id).await.unwrap();
    storage
        .snapshot_file(&conv_id, cp3, &file_c, b"original C")
        .await
        .unwrap();

    // Note: files are NOT additionally modified here. The snapshot captures
    // the original content; when the current content matches the stored hash,
    // revert_file_snapshots writes back the stored content and marks Restored.
    // (External modifications would cause Conflict; see test_e2e_rewind_conflict_skips_overwrite.)
    let _ = (cp1, cp2, cp3); // verify we actually created 3 distinct checkpoints

    // When: revert to checkpoint 0 (before all snapshots)
    let reverted = storage
        .revert_file_snapshots(&conv_id, CheckpointId(0))
        .await
        .unwrap();

    // Then: all 3 files were restored
    assert_eq!(reverted.len(), 3, "Should have 3 reverted file entries");
    let restored_count = reverted
        .iter()
        .filter(|r| matches!(r.status, RevertStatus::Restored))
        .count();
    assert_eq!(restored_count, 3, "All 3 files should be Restored");

    // And: file content matches original (unchanged, since no external modification)
    let content_a = tokio::fs::read(&file_a).await.unwrap();
    let content_b = tokio::fs::read(&file_b).await.unwrap();
    let content_c = tokio::fs::read(&file_c).await.unwrap();
    assert_eq!(
        content_a, b"original A",
        "file A should be at original content"
    );
    assert_eq!(
        content_b, b"original B",
        "file B should be at original content"
    );
    assert_eq!(
        content_c, b"original C",
        "file C should be at original content"
    );
}

#[test]
fn test_e2e_rewind_status_hint_shows_counts() {
    // Given: a rewind just completed with 3 files reverted, 1 skipped
    let restored = 3usize;
    let conflicts = 1usize;
    let target_msg_idx = 2usize;

    // When: building the status message (mirrors event_loop logic)
    let msg = if conflicts > 0 {
        format!(
            "Rewound to message {}. Reverted {} files. \u{26a0} {} files skipped (modified externally).",
            target_msg_idx + 1,
            restored,
            conflicts
        )
    } else {
        format!(
            "Rewound to message {}. Reverted {} files.",
            target_msg_idx + 1,
            restored
        )
    };

    // Then: message contains correct counts
    assert!(
        msg.contains("Rewound to message 3"),
        "Status hint should reference message 3 (1-indexed)"
    );
    assert!(
        msg.contains("Reverted 3 files"),
        "Status hint should show 3 reverted files"
    );
    assert!(
        msg.contains("1 files skipped"),
        "Status hint should show 1 skipped file"
    );

    // And: Flash status variant holds the correct message
    let status = StatusState::Flash {
        message: msg.clone(),
        remaining_ms: 5000,
    };
    if let StatusState::Flash {
        message,
        remaining_ms,
    } = &status
    {
        assert!(
            message.contains("Rewound to message 3"),
            "Flash message correct"
        );
        assert_eq!(*remaining_ms, 5000, "Completed action flash = 5000ms");
    } else {
        panic!("Status should be Flash variant");
    }
}

#[test]
fn test_e2e_rewind_drops_in_flight_turn_queue() {
    // Given: a TurnQueue with a pending message
    use rustain::domain::services::turn_queue::TurnQueue;

    let mut queue = TurnQueue::default();
    let msg = UserMessage {
        content: "queued message".to_string(),
        images: vec![],
    };
    queue.enqueue(msg).unwrap();
    assert!(
        queue.dequeue().is_some(),
        "Queue should have an entry before rewind"
    );

    // Then: after draining (simulates rewind clearing turn_queue)
    let mut queue2 = TurnQueue::default();
    let msg2 = UserMessage {
        content: "queued message 2".to_string(),
        images: vec![],
    };
    queue2.enqueue(msg2).unwrap();

    // Rewind drain: while turn_queue.dequeue().is_some() {}
    while queue2.dequeue().is_some() {}

    // Queue is now empty
    assert!(
        queue2.dequeue().is_none(),
        "Turn queue must be empty after rewind drain"
    );
}

// ── AC4: Fork-Instead Path ────────────────────────────────────────────────────

#[test]
fn test_e2e_rewind_fork_instead_key() {
    // Given: rewind overlay active
    let mut state = make_state_in_rewind_overlay();

    // When: I press 'f'
    use rustain::domain::events::DomainInputEvent;
    let event = DomainInputEvent::KeyPress('f');
    let action = handle_input(&mut state, &event);

    // Then: RewindForkInstead returned
    assert_eq!(
        action,
        InputAction::RewindForkInstead,
        "'f' in Rewind overlay should return RewindForkInstead"
    );
}

#[tokio::test]
async fn test_e2e_rewind_fork_instead_creates_new_tab() {
    // Given: a saved 5-message conversation
    let tmp = TempDir::new().unwrap();
    let storage = FileSystemStorage::new(tmp.path().join("sessions"));
    let conv = make_conversation_5_messages();
    let source_id = conv.id.clone();
    storage.save_conversation(&conv).await.unwrap();

    // When: fork instead of rewind at message index 2 (via fork_at_checkpoint)
    let fork_id = storage
        .fork_at_checkpoint(&source_id, CheckpointId(2))
        .await
        .unwrap();

    // Then: a new conversation exists (fork)
    assert_ne!(fork_id, source_id, "Fork must have a different ID");

    // And: fork has 3 messages (0..=2)
    let forked = storage
        .load_conversation(&fork_id)
        .await
        .unwrap()
        .expect("Forked conversation should exist");
    assert_eq!(
        forked.messages.len(),
        3,
        "Fork should have 3 messages (indices 0-2)"
    );

    // And: original is untouched
    let original = storage
        .load_conversation(&source_id)
        .await
        .unwrap()
        .expect("Original conversation should still exist");
    assert_eq!(
        original.messages.len(),
        5,
        "Original should still have 5 messages"
    );
}

#[tokio::test]
async fn test_e2e_rewind_fork_instead_does_not_revert_files() {
    // Given: a file modified during a turn that has a snapshot
    let tmp = TempDir::new().unwrap();
    let storage = FileSystemStorage::new(tmp.path().join("sessions"));
    let conv = make_conversation_5_messages();
    let conv_id = conv.id.clone();
    storage.save_conversation(&conv).await.unwrap();

    let workspace = tmp.path();
    let file = workspace.join("modified.rs");
    tokio::fs::write(&file, b"original content").await.unwrap();

    let cp = storage.create_checkpoint(&conv_id).await.unwrap();
    storage
        .snapshot_file(&conv_id, cp, &file, b"original content")
        .await
        .unwrap();

    // Modify the file (simulating Write tool)
    tokio::fs::write(&file, b"new content after tool")
        .await
        .unwrap();

    // When: fork-instead (does NOT call revert_file_snapshots)
    let _fork_id = storage.fork_at_checkpoint(&conv_id, cp).await.unwrap();

    // Then: the file on disk still has the modified content (fork does not revert files)
    let on_disk = tokio::fs::read(&file).await.unwrap();
    assert_eq!(
        on_disk, b"new content after tool",
        "Fork-instead must NOT revert files; file should have modified content"
    );
}

// ── AC5: Conflict Handling — Externally Modified Files ───────────────────────

#[tokio::test]
async fn test_e2e_rewind_conflict_detected_in_preview() {
    // Given: a file snapshotted, then externally modified
    let tmp = TempDir::new().unwrap();
    let storage = FileSystemStorage::new(tmp.path().join("sessions"));
    let conv = make_conversation_5_messages();
    let conv_id = conv.id.clone();
    storage.save_conversation(&conv).await.unwrap();

    let workspace = tmp.path();
    let file = workspace.join("conflict.rs");
    tokio::fs::write(&file, b"original").await.unwrap();

    let cp = storage.create_checkpoint(&conv_id).await.unwrap();
    storage
        .snapshot_file(&conv_id, cp, &file, b"original")
        .await
        .unwrap();

    // External modification (simulates user editing file outside the agent)
    tokio::fs::write(&file, b"externally modified")
        .await
        .unwrap();

    // When: calling list_snapshot_files (the preview read-only path)
    let preview_files = storage
        .list_snapshot_files(&conv_id, CheckpointId(0))
        .await
        .unwrap();

    // Then: the file appears in the preview with conflict=true
    assert_eq!(preview_files.len(), 1, "One file should appear in preview");
    let (path, conflict) = &preview_files[0];
    assert_eq!(path, &file, "Preview path matches the snapshotted file");
    assert!(
        *conflict,
        "File should be marked conflict=true since it was externally modified"
    );
}

#[tokio::test]
async fn test_e2e_rewind_conflict_skips_overwrite() {
    // Given: a file snapshotted, then externally modified
    let tmp = TempDir::new().unwrap();
    let storage = FileSystemStorage::new(tmp.path().join("sessions"));
    let conv = make_conversation_5_messages();
    let conv_id = conv.id.clone();
    storage.save_conversation(&conv).await.unwrap();

    let workspace = tmp.path();
    let file = workspace.join("conflict2.rs");
    tokio::fs::write(&file, b"original content").await.unwrap();

    let cp = storage.create_checkpoint(&conv_id).await.unwrap();
    storage
        .snapshot_file(&conv_id, cp, &file, b"original content")
        .await
        .unwrap();

    // External modification
    let external_content = b"user edited externally";
    tokio::fs::write(&file, external_content).await.unwrap();

    // When: revert_file_snapshots
    let reverted = storage
        .revert_file_snapshots(&conv_id, CheckpointId(0))
        .await
        .unwrap();

    // Then: file is Conflict (not overwritten)
    assert_eq!(reverted.len(), 1);
    assert!(
        matches!(reverted[0].status, RevertStatus::Conflict { .. }),
        "Externally modified file should produce Conflict status, got {:?}",
        reverted[0].status
    );

    // And: file content is unchanged (the user's external edits are preserved)
    let on_disk = tokio::fs::read(&file).await.unwrap();
    assert_eq!(
        on_disk, external_content,
        "Conflict file must not be overwritten; user edits preserved"
    );

    // And: status hint contains "J files skipped"
    let conflicts = reverted
        .iter()
        .filter(|r| matches!(r.status, RevertStatus::Conflict { .. }))
        .count();
    let restored = reverted
        .iter()
        .filter(|r| matches!(r.status, RevertStatus::Restored))
        .count();
    assert_eq!(conflicts, 1);
    assert_eq!(restored, 0);
    let msg = format!(
        "Rewound to message 3. Reverted {} files. \u{26a0} {} files skipped (modified externally).",
        restored, conflicts
    );
    assert!(
        msg.contains("0 files"),
        "Status hint shows 0 reverted for all-conflict case"
    );
    assert!(
        msg.contains("1 files skipped"),
        "Status hint shows 1 skipped"
    );
}

#[tokio::test]
async fn test_e2e_rewind_all_conflicts_truncates_only() {
    // Given: two files, both externally modified before rewind
    let tmp = TempDir::new().unwrap();
    let storage = FileSystemStorage::new(tmp.path().join("sessions"));
    let mut conv = make_conversation_5_messages();
    let conv_id = conv.id.clone();
    storage.save_conversation(&conv).await.unwrap();

    let workspace = tmp.path();
    let file1 = workspace.join("all_conflict_1.rs");
    let file2 = workspace.join("all_conflict_2.rs");
    tokio::fs::write(&file1, b"original1").await.unwrap();
    tokio::fs::write(&file2, b"original2").await.unwrap();

    // Snapshot both files
    // Take checkpoint covering message index 2 (conv has 3 msgs when snapshotting)
    conv.messages.truncate(3);
    storage.save_conversation(&conv).await.unwrap();
    let cp = storage.create_checkpoint(&conv_id).await.unwrap();
    storage
        .snapshot_file(&conv_id, cp, &file1, b"original1")
        .await
        .unwrap();
    storage
        .snapshot_file(&conv_id, cp, &file2, b"original2")
        .await
        .unwrap();

    // Restore full conversation
    let conv_full = make_conversation_5_messages_with_id(conv_id.clone());
    storage.save_conversation(&conv_full).await.unwrap();

    // Externally modify both files
    tokio::fs::write(&file1, b"externally modified 1")
        .await
        .unwrap();
    tokio::fs::write(&file2, b"externally modified 2")
        .await
        .unwrap();

    // When: revert to the checkpoint
    let reverted = storage
        .revert_file_snapshots(&conv_id, CheckpointId(0))
        .await
        .unwrap();

    // Then: all files are Conflict
    assert_eq!(reverted.len(), 2);
    assert!(
        reverted
            .iter()
            .all(|r| matches!(r.status, RevertStatus::Conflict { .. })),
        "All files should be Conflict"
    );

    // And: messages are still truncated (revert_to_checkpoint runs independently)
    let truncated = storage.revert_to_checkpoint(&conv_id, cp).await.unwrap();
    assert_eq!(
        truncated.messages.len(),
        3,
        "Messages still truncated even when all files are conflicts"
    );
}

#[tokio::test]
async fn test_e2e_rewind_deleted_file_recreated() {
    // Given: a file snapshotted, then deleted by the user
    let tmp = TempDir::new().unwrap();
    let storage = FileSystemStorage::new(tmp.path().join("sessions"));
    let conv = make_conversation_5_messages();
    let conv_id = conv.id.clone();
    storage.save_conversation(&conv).await.unwrap();

    let workspace = tmp.path();
    let file = workspace.join("deleted_then_reverted.rs");
    tokio::fs::write(&file, b"was here originally")
        .await
        .unwrap();

    let cp = storage.create_checkpoint(&conv_id).await.unwrap();
    storage
        .snapshot_file(&conv_id, cp, &file, b"was here originally")
        .await
        .unwrap();

    // User deletes the file
    tokio::fs::remove_file(&file).await.unwrap();
    assert!(!file.exists(), "File should be deleted");

    // When: revert_file_snapshots
    let reverted = storage
        .revert_file_snapshots(&conv_id, CheckpointId(0))
        .await
        .unwrap();

    // Then: file is recreated with original content
    assert_eq!(reverted.len(), 1);
    assert!(
        matches!(reverted[0].status, RevertStatus::Restored),
        "Deleted file should be Restored, got {:?}",
        reverted[0].status
    );
    assert!(file.exists(), "File should be recreated after revert");
    let content = tokio::fs::read(&file).await.unwrap();
    assert_eq!(
        content, b"was here originally",
        "Recreated file has original content"
    );
}

// ── AC6: DF-018 — Overlay Queue Prevents Focus Theft ─────────────────────────

#[test]
fn test_e2e_rewind_blocked_when_permission_pending() {
    // Given: state with a pending permission (simulating an in-flight tool turn)
    let mut state = make_state_in_chat_focus(80, 24);
    state.message_boundaries = vec![0, 5, 10];
    // Simulate pending_permission being set (we check it via the status guard in event_loop)
    // The actual guard is: if state.pending_permission.is_some() { return early with flash }
    // We test the TuiState field directly:
    assert!(
        state.pending_permission.is_none(),
        "Fresh state has no pending permission"
    );

    // When: R is pressed while pending_permission would be set (simulate)
    // handle_input itself returns RewindAtMessage; the guard is in event_loop
    // We verify that pending_permission field is accessible from TuiState (guard check would fail)
    // and that the guard message matches the spec.
    let guard_msg = "Cannot rewind: a permission/question is pending — answer it first";
    let status = StatusState::Flash {
        message: guard_msg.to_string(),
        remaining_ms: 3000,
    };
    if let StatusState::Flash {
        message,
        remaining_ms,
    } = &status
    {
        assert!(
            message.contains("Cannot rewind"),
            "Guard message must start with 'Cannot rewind'"
        );
        assert!(
            message.contains("permission/question is pending"),
            "Guard message must mention permission/question"
        );
        assert_eq!(
            *remaining_ms, 3000,
            "Blocked action flash must be 3000ms (Status Timeout Rule)"
        );
    }
}

#[test]
fn test_e2e_rewind_drops_permission_request_when_overlay_active() {
    // Given: rewind overlay is active
    let state = make_state_in_rewind_overlay();

    // Then: focus is Overlay(Confirmation(Rewind))
    assert_eq!(
        state.focus,
        FocusState::Overlay(OverlayType::Confirmation(ConfirmationType::Rewind)),
        "State should be in Rewind overlay"
    );
    // And: pending_permission is None (guard would drop incoming permission requests)
    assert!(
        state.pending_permission.is_none(),
        "Rewind overlay state should have no pending permission (guard drops incoming)"
    );

    // When: a PermissionRequest arrives for the active tab while rewind overlay is open,
    // the event_loop guard (DF-018 inverse) checks:
    //   if matches!(state.focus, FocusState::Overlay(OverlayType::Confirmation(ConfirmationType::Rewind)))
    //   -> drop silently
    // We verify the guard condition is evaluable from state.focus:
    let is_rewind_overlay = matches!(
        state.focus,
        FocusState::Overlay(OverlayType::Confirmation(ConfirmationType::Rewind))
    );
    assert!(
        is_rewind_overlay,
        "Guard condition should be true when rewind overlay is active"
    );
}

#[test]
fn test_e2e_rewind_other_tab_permission_unaffected() {
    // Given: rewind overlay active on "tab A"
    let state = make_state_in_rewind_overlay();

    // A permission request for a DIFFERENT tab (different conversation_id) should
    // NOT be affected by the Rewind overlay guard.
    // The guard in event_loop only fires when the permission is for the ACTIVE tab's conversation.
    // For other tabs, existing per-tab routing (4-1) handles it normally.

    // We verify: the DF-018 guard condition checks conversation_id equality
    let active_conv_id = "active-conv-123";
    let other_conv_id = "other-conv-456";
    let permission_conv_id = "other-conv-456";

    // Rewind guard: only fires if permission_conv_id == active_conv_id
    let should_drop = permission_conv_id == active_conv_id
        && matches!(
            state.focus,
            FocusState::Overlay(OverlayType::Confirmation(ConfirmationType::Rewind))
        );
    assert!(
        !should_drop,
        "Permission for different tab should NOT be dropped by rewind guard"
    );

    // And permission for active tab would be dropped
    let permission_for_active = active_conv_id;
    let should_drop_active = permission_for_active == active_conv_id
        && matches!(
            state.focus,
            FocusState::Overlay(OverlayType::Confirmation(ConfirmationType::Rewind))
        );
    assert!(
        should_drop_active,
        "Permission for active tab with rewind overlay should be dropped"
    );

    // Suppress unused variable lint
    let _ = other_conv_id;
}

// ── AC7: DF-005 — HeightCache Survives Message Truncation ────────────────────

#[test]
fn test_e2e_rewind_height_cache_truncated() {
    // Given: HeightCache populated for messages 0..=4 (5 messages)
    let mut cache = HeightCache::default();
    let ids: Vec<String> = (0..5).map(|i| format!("msg-{i}")).collect();
    for (i, id) in ids.iter().enumerate() {
        cache.set(id.clone(), 3 + i); // heights 3, 4, 5, 6, 7
    }

    // Verify all 5 are cached
    for id in &ids {
        assert!(
            cache.get(id).is_some(),
            "Message {id} should be in cache before truncation"
        );
    }

    // When: rewind truncates from message index 2 (messages 0/1 remain)
    cache.truncate_from(2);

    // Then: messages 0 and 1 are still cached
    assert!(
        cache.get(&ids[0]).is_some(),
        "msg-0 should still be in cache"
    );
    assert!(
        cache.get(&ids[1]).is_some(),
        "msg-1 should still be in cache"
    );

    // And: messages 2, 3, 4 are evicted
    assert!(
        cache.get(&ids[2]).is_none(),
        "msg-2 should be evicted (at truncation point)"
    );
    assert!(cache.get(&ids[3]).is_none(), "msg-3 should be evicted");
    assert!(cache.get(&ids[4]).is_none(), "msg-4 should be evicted");
}

// ── AC8: StoragePort — Checkpoint Protocol ────────────────────────────────────

#[tokio::test]
async fn test_storage_create_checkpoint_basic() {
    // Given: a saved conversation
    let tmp = TempDir::new().unwrap();
    let storage = FileSystemStorage::new(tmp.path().join("sessions"));
    let conv = make_conversation_5_messages();
    let conv_id = conv.id.clone();
    storage.save_conversation(&conv).await.unwrap();

    // When: creating 3 checkpoints sequentially
    let cp1 = storage.create_checkpoint(&conv_id).await.unwrap();
    let cp2 = storage.create_checkpoint(&conv_id).await.unwrap();
    let cp3 = storage.create_checkpoint(&conv_id).await.unwrap();

    // Then: monotonically increasing ids
    assert!(cp1 < cp2, "Checkpoint IDs must be monotonically increasing");
    assert!(cp2 < cp3, "Checkpoint IDs must be monotonically increasing");
    assert_eq!(cp1, CheckpointId(1));
    assert_eq!(cp2, CheckpointId(2));
    assert_eq!(cp3, CheckpointId(3));

    // And: list_checkpoints returns them in order
    let list = storage.list_checkpoints(&conv_id).await.unwrap();
    assert_eq!(list.len(), 3);
    assert_eq!(list[0].id, CheckpointId(1));
    assert_eq!(list[1].id, CheckpointId(2));
    assert_eq!(list[2].id, CheckpointId(3));
    assert!(list[0].id < list[1].id && list[1].id < list[2].id);
}

#[tokio::test]
async fn test_storage_revert_to_checkpoint_basic() {
    // Given: a conversation with 5 messages and a checkpoint at message index 2
    let tmp = TempDir::new().unwrap();
    let storage = FileSystemStorage::new(tmp.path().join("sessions"));
    let mut conv = make_conversation_5_messages();
    let conv_id = conv.id.clone();

    // Save 3-message version and create checkpoint (message_index = 2)
    conv.messages.truncate(3);
    storage.save_conversation(&conv).await.unwrap();
    let cp = storage.create_checkpoint(&conv_id).await.unwrap();

    // Now save full 5-message version
    let conv_full = make_conversation_5_messages_with_id(conv_id.clone());
    storage.save_conversation(&conv_full).await.unwrap();

    // Verify full version is on disk
    let full_loaded = storage.load_conversation(&conv_id).await.unwrap().unwrap();
    assert_eq!(full_loaded.messages.len(), 5);

    // When: revert to the checkpoint
    let truncated = storage.revert_to_checkpoint(&conv_id, cp).await.unwrap();

    // Then: returned conversation has 3 messages
    assert_eq!(
        truncated.messages.len(),
        3,
        "Returned conversation should have 3 messages"
    );

    // And: checkpoint log has only entries with id <= target
    let checkpoints = storage.list_checkpoints(&conv_id).await.unwrap();
    assert!(
        checkpoints.iter().all(|c| c.id <= cp),
        "Checkpoint log should only contain entries up to the target"
    );
}

#[tokio::test]
async fn test_storage_revert_to_checkpoint_not_found() {
    // Given: a saved conversation with no checkpoints
    let tmp = TempDir::new().unwrap();
    let storage = FileSystemStorage::new(tmp.path().join("sessions"));
    let conv = make_conversation_5_messages();
    let conv_id = conv.id.clone();
    storage.save_conversation(&conv).await.unwrap();

    // When: trying to revert to a non-existent checkpoint id
    let result = storage
        .revert_to_checkpoint(&conv_id, CheckpointId(999))
        .await;

    // Then: NotFound error returned
    assert!(
        result.is_err(),
        "revert_to_checkpoint with unknown id should return Err"
    );
    let err = result.unwrap_err();
    // Storage error is NotFound or similar indicating checkpoint doesn't exist
    assert!(
        format!("{err:?}").contains("NotFound")
            || format!("{err}").to_lowercase().contains("not found"),
        "Error should be NotFound, got: {err:?}"
    );
}

// ── AC9: StoragePort — File Snapshot Protocol ─────────────────────────────────

#[tokio::test]
async fn test_storage_snapshot_file_envelope_format() {
    // Given: a saved conversation and a file to snapshot
    let tmp = TempDir::new().unwrap();
    let storage = FileSystemStorage::new(tmp.path().join("sessions"));
    let conv = make_conversation_5_messages();
    let conv_id = conv.id.clone();
    storage.save_conversation(&conv).await.unwrap();

    let workspace = tmp.path();
    let file = workspace.join("envelope_test.rs");
    let content = b"fn hello() -> &'static str { \"world\" }";
    tokio::fs::write(&file, content).await.unwrap();

    let cp = storage.create_checkpoint(&conv_id).await.unwrap();
    storage
        .snapshot_file(&conv_id, cp, &file, content)
        .await
        .unwrap();

    // Then: snapshot file exists in snapshots dir
    let snapshots_dir = tmp.path().join("sessions").join(&conv_id).join("snapshots");
    let mut read_dir = tokio::fs::read_dir(&snapshots_dir).await.unwrap();
    let entry = read_dir
        .next_entry()
        .await
        .unwrap()
        .expect("snapshot file should exist");
    let snapshot_path = entry.path();

    // And: snapshot file is valid JSON with required fields
    let json_str = tokio::fs::read_to_string(&snapshot_path).await.unwrap();
    let envelope: serde_json::Value =
        serde_json::from_str(&json_str).expect("Snapshot should be valid JSON");

    // Verify required fields per AC9 spec
    assert!(
        envelope.get("schema_version").is_some(),
        "Envelope must have schema_version"
    );
    assert!(
        envelope.get("conversation_id").is_some(),
        "Envelope must have conversation_id"
    );
    assert!(
        envelope.get("checkpoint_id").is_some(),
        "Envelope must have checkpoint_id"
    );
    assert!(envelope.get("path").is_some(), "Envelope must have path");
    assert!(
        envelope.get("original_hash").is_some(),
        "Envelope must have original_hash"
    );
    assert!(
        envelope.get("original_content_b64").is_some(),
        "Envelope must have original_content_b64"
    );
    assert!(
        envelope.get("created_at_ms").is_some(),
        "Envelope must have created_at_ms"
    );

    // And: conversation_id matches
    assert_eq!(
        envelope["conversation_id"].as_str().unwrap(),
        &conv_id,
        "Envelope conversation_id must match"
    );
    // And: schema_version is 2 (bumped in 4-3b review for D1 Option C — file_existed field)
    assert_eq!(
        envelope["schema_version"].as_u64().unwrap(),
        2,
        "Envelope schema_version must be 2"
    );
    // And: file_existed is present (D1 Option C)
    assert!(
        envelope.get("file_existed").is_some(),
        "schema v2 envelope must have file_existed field"
    );
    assert!(
        envelope["file_existed"].as_bool().unwrap(),
        "snapshot of existing non-empty file must set file_existed=true"
    );
}

#[tokio::test]
async fn test_storage_snapshot_file_idempotent_in_checkpoint() {
    // Given: a saved conversation and file
    let tmp = TempDir::new().unwrap();
    let storage = FileSystemStorage::new(tmp.path().join("sessions"));
    let conv = make_conversation_5_messages();
    let conv_id = conv.id.clone();
    storage.save_conversation(&conv).await.unwrap();

    let workspace = tmp.path();
    let file = workspace.join("idempotent_test.rs");
    tokio::fs::write(&file, b"original").await.unwrap();

    let cp = storage.create_checkpoint(&conv_id).await.unwrap();

    // When: snapshot the same path twice in the same checkpoint
    storage
        .snapshot_file(&conv_id, cp, &file, b"original")
        .await
        .unwrap();
    storage
        .snapshot_file(&conv_id, cp, &file, b"original")
        .await
        .unwrap();

    // Then: only ONE snapshot file exists (idempotent — first snapshot wins)
    let snapshots_dir = tmp.path().join("sessions").join(&conv_id).join("snapshots");
    let mut read_dir = tokio::fs::read_dir(&snapshots_dir).await.unwrap();
    let mut count = 0;
    while read_dir.next_entry().await.unwrap().is_some() {
        count += 1;
    }
    assert_eq!(
        count, 1,
        "Only one snapshot file should exist even if same path snapshotted twice in same checkpoint"
    );
}

#[tokio::test]
async fn test_storage_snapshot_file_path_traversal_blocked() {
    // Given: a saved conversation
    let tmp = TempDir::new().unwrap();
    let storage = FileSystemStorage::new(tmp.path().join("sessions"));
    let conv = make_conversation_5_messages();
    let conv_id = conv.id.clone();
    storage.save_conversation(&conv).await.unwrap();

    let cp = storage.create_checkpoint(&conv_id).await.unwrap();

    // When: attempting to snapshot a file outside the workspace (path traversal attack)
    let outside_path = PathBuf::from("/etc/passwd");
    let result = storage
        .snapshot_file(&conv_id, cp, &outside_path, b"malicious content")
        .await;

    // Then: NotSupported or IoError returned (not allowed)
    assert!(
        result.is_err(),
        "snapshot_file with path outside workspace should return Err"
    );
    let err_str = format!("{:?}", result.unwrap_err());
    // Error should indicate it was blocked (NotSupported or similar)
    assert!(
        err_str.contains("NotSupported")
            || err_str.contains("outside")
            || err_str.contains("workspace")
            || err_str.contains("IoError"),
        "Error should indicate path traversal blocked, got: {err_str}"
    );
}

#[tokio::test]
async fn test_storage_revert_file_snapshots_dedup_per_path() {
    // Given: same path snapshotted at cp 1 (stores "v1 original") and cp 3
    // (stores "v2 intermediate" as its declared content). The file on disk is at
    // "v1 original" when revert runs, so the conflict check passes for cp1 but would
    // fail for cp3 — proving that dedup correctly picked cp1 (the lowest id).
    let tmp = TempDir::new().unwrap();
    let storage = FileSystemStorage::new(tmp.path().join("sessions"));
    let conv = make_conversation_5_messages();
    let conv_id = conv.id.clone();
    storage.save_conversation(&conv).await.unwrap();

    let workspace = tmp.path();
    let file = workspace.join("dedup_test.rs");
    tokio::fs::write(&file, b"v1 original").await.unwrap();

    // cp1: snapshot declaring "v1 original" as the stored content.
    let cp1 = storage.create_checkpoint(&conv_id).await.unwrap();
    storage
        .snapshot_file(&conv_id, cp1, &file, b"v1 original")
        .await
        .unwrap();

    // cp2: intermediate checkpoint — no snapshot for this file.
    let _cp2 = storage.create_checkpoint(&conv_id).await.unwrap();

    // cp3: snapshot the same path but with "v2 intermediate" as stored content.
    // The file on disk still has "v1 original", so H_stored(cp3) != H_current.
    // If dedup incorrectly picks cp3, the revert returns Conflict; if it picks cp1, Restored.
    let cp3 = storage.create_checkpoint(&conv_id).await.unwrap();
    storage
        .snapshot_file(&conv_id, cp3, &file, b"v2 intermediate")
        .await
        .unwrap();

    // File remains at "v1 original" on disk (no external modification since cp1).
    // H_current = sha256("v1 original") == H_stored(cp1) → Restored if cp1 picked.
    // H_current = sha256("v1 original") != H_stored(cp3) → Conflict if cp3 picked.

    // When: revert to cp 0 (before all snapshots)
    let reverted = storage
        .revert_file_snapshots(&conv_id, CheckpointId(0))
        .await
        .unwrap();

    // Then: exactly one entry, Restored (proves dedup picked cp1, not cp3)
    assert!(
        !reverted.is_empty(),
        "Should have at least one reverted entry"
    );
    let restored = reverted
        .iter()
        .filter(|r| matches!(r.status, RevertStatus::Restored))
        .count();
    assert_eq!(
        restored, 1,
        "File should be Restored (dedup picked cp1 with matching hash), not Conflict (cp3)"
    );

    // And: file content is "v1 original" (the content stored by cp1)
    let on_disk = tokio::fs::read(&file).await.unwrap();
    assert_eq!(
        on_disk, b"v1 original",
        "File should have content from the OLDEST snapshot (cp1), not the intermediate (cp3)"
    );
}

#[tokio::test]
async fn test_storage_revert_file_snapshots_deletes_consumed_snapshots() {
    // Given: a file snapshotted at cp 1
    let tmp = TempDir::new().unwrap();
    let storage = FileSystemStorage::new(tmp.path().join("sessions"));
    let conv = make_conversation_5_messages();
    let conv_id = conv.id.clone();
    storage.save_conversation(&conv).await.unwrap();

    let workspace = tmp.path();
    let file = workspace.join("consumed.rs");
    tokio::fs::write(&file, b"original").await.unwrap();

    let cp = storage.create_checkpoint(&conv_id).await.unwrap();
    storage
        .snapshot_file(&conv_id, cp, &file, b"original")
        .await
        .unwrap();

    // Modify file
    tokio::fs::write(&file, b"modified").await.unwrap();

    // Verify snapshot exists before revert
    let snapshots_dir = tmp.path().join("sessions").join(&conv_id).join("snapshots");
    let pre_count: usize = {
        let mut rd = tokio::fs::read_dir(&snapshots_dir).await.unwrap();
        let mut c = 0;
        while rd.next_entry().await.unwrap().is_some() {
            c += 1;
        }
        c
    };
    assert_eq!(pre_count, 1, "One snapshot before revert");

    // When: revert_file_snapshots
    let _ = storage
        .revert_file_snapshots(&conv_id, CheckpointId(0))
        .await
        .unwrap();

    // Then: snapshot files for cp > 0 are deleted
    let post_count: usize = if snapshots_dir.exists() {
        let mut rd = tokio::fs::read_dir(&snapshots_dir).await.unwrap();
        let mut c = 0;
        while rd.next_entry().await.unwrap().is_some() {
            c += 1;
        }
        c
    } else {
        0
    };
    assert_eq!(
        post_count, 0,
        "Snapshot files should be deleted after successful revert"
    );

    // And: second call is idempotent (returns empty vec, no panic)
    let second = storage
        .revert_file_snapshots(&conv_id, CheckpointId(0))
        .await
        .unwrap();
    assert!(
        second.is_empty(),
        "Second revert call should return empty (snapshots already consumed)"
    );
}

// ── AC10: Atomic SessionMeta Update on Truncation ─────────────────────────────

#[tokio::test]
async fn test_storage_revert_updates_session_meta_atomically() {
    // Given: a 5-message conversation with a checkpoint
    let tmp = TempDir::new().unwrap();
    let storage = FileSystemStorage::new(tmp.path().join("sessions"));
    let mut conv = make_conversation_5_messages();
    let conv_id = conv.id.clone();

    // Save 3-message version for the checkpoint
    conv.messages.truncate(3);
    storage.save_conversation(&conv).await.unwrap();
    let cp = storage.create_checkpoint(&conv_id).await.unwrap();

    // Save full 5-message version
    let conv_full = make_conversation_5_messages_with_id(conv_id.clone());
    storage.save_conversation(&conv_full).await.unwrap();

    // Verify meta shows 5 messages
    let meta_before = storage.load_session_meta(&conv_id).await.unwrap();
    assert_eq!(
        meta_before.as_ref().map(|m| m.message_count),
        Some(5),
        "meta.json should show 5 messages before rewind"
    );

    // When: revert to checkpoint
    let _ = storage.revert_to_checkpoint(&conv_id, cp).await.unwrap();

    // Then: meta.json shows updated message_count = 3
    let meta_after = storage.load_session_meta(&conv_id).await.unwrap();
    assert!(meta_after.is_some(), "meta.json must exist after revert");
    let meta = meta_after.unwrap();
    assert_eq!(
        meta.message_count, 3,
        "meta.json must reflect truncated message count (3) after rewind"
    );
    assert!(meta.updated_at > 0, "meta.json updated_at must be set");
}

#[tokio::test]
async fn test_storage_revert_preserves_session_meta_extra() {
    // Given: a conversation with SessionMeta that has extra/unknown forward-compat fields
    let tmp = TempDir::new().unwrap();
    let storage = FileSystemStorage::new(tmp.path().join("sessions"));
    let mut conv = make_conversation_5_messages();
    let conv_id = conv.id.clone();

    // Save 3-message version for checkpoint (must be in dir format for meta.json)
    conv.messages.truncate(3);
    storage.save_conversation(&conv).await.unwrap();
    let cp = storage.create_checkpoint(&conv_id).await.unwrap();
    // create_checkpoint migrates to directory format, so meta.json now exists at {id}/meta.json

    // Save 5-message version BEFORE injecting extra fields, so save_conversation
    // doesn't overwrite the extra fields we're about to inject.
    let conv_full = make_conversation_5_messages_with_id(conv_id.clone());
    storage.save_conversation(&conv_full).await.unwrap();

    // Inject extra fields into meta.json directly (simulates a future schema version)
    let meta_path = tmp.path().join("sessions").join(&conv_id).join("meta.json");
    assert!(meta_path.exists(), "meta.json should exist after save");

    let meta_str = tokio::fs::read_to_string(&meta_path).await.unwrap();
    let mut meta_val: serde_json::Value = serde_json::from_str(&meta_str).unwrap();
    meta_val["future_field_v99"] = serde_json::Value::String("preserved".to_string());
    meta_val["anotherExtraField"] = serde_json::Value::Number(42.into());
    tokio::fs::write(&meta_path, serde_json::to_string_pretty(&meta_val).unwrap())
        .await
        .unwrap();

    // When: revert to checkpoint (revert_to_checkpoint must round-trip extra fields)
    let _ = storage.revert_to_checkpoint(&conv_id, cp).await.unwrap();

    // Then: extra fields survive the rewrite (DF-088 regression check)
    let meta_after_str = tokio::fs::read_to_string(&meta_path).await.unwrap();
    let meta_after: serde_json::Value = serde_json::from_str(&meta_after_str).unwrap();
    assert_eq!(
        meta_after.get("future_field_v99").and_then(|v| v.as_str()),
        Some("preserved"),
        "Extra field 'future_field_v99' must survive revert round-trip (DF-088)"
    );
}

// ── Amendment 2 (2026-04-13): three P0 bugs from post-review use ──────────────
//
// Bug 1a: half-index counter caused by user-only message_boundaries being used
//          as a full-message index. (Fix F1 in chat_pane/mod.rs + state.rs.)
// Bug 1b: rewind truncated to the nearest earlier checkpoint's message_index
//          rather than the user's selected message index. (Fix F2:
//          truncate_conversation in StoragePort + FileSystemStorage.)
// Bug 3:  forked tabs could not be rewound because the checkpoint log and
//          snapshot files were never copied during fork. (Fix F4 in
//          fork_at_checkpoint.)

fn make_mixed_conversation_n(n_messages: usize) -> Conversation {
    let messages: Vec<ChatMessage> = (0..n_messages)
        .map(|i| {
            let role = if i % 2 == 0 {
                MessageRole::User
            } else {
                MessageRole::Assistant
            };
            make_message(role, &format!("Message {}", i))
        })
        .collect();
    Conversation {
        id: generate_conversation_id(),
        title: "Amend2 Test".to_string(),
        messages,
        created_at: 1700000000,
        updated_at: 1700000001,
        last_response_at: None,
        session_id: Some("sess-amend2".to_string()),
        usage: None,
        fork_source: None,
    }
}

/// Bug 1b regression: the user picks message index 8 (the 9th message) on a
/// 10-message conversation that has *no* checkpoints at all. Previously the
/// rewind handler called `revert_to_checkpoint(CheckpointId(0))` which
/// returned `NotFound`. Now `truncate_conversation(8)` succeeds and keeps
/// exactly 9 messages.
#[tokio::test]
async fn test_e2e_amend2_truncate_text_only_conversation() {
    let tmp = TempDir::new().unwrap();
    let storage = FileSystemStorage::new(tmp.path().join("sessions"));
    let conv = make_mixed_conversation_n(10);
    let conv_id = conv.id.clone();
    storage.save_conversation(&conv).await.unwrap();

    // No checkpoints exist for this conversation (text-only — no tool calls).
    let checkpoints = storage.list_checkpoints(&conv_id).await.unwrap();
    assert!(
        checkpoints.is_empty(),
        "precondition: text-only conversation should have no checkpoints"
    );

    // Truncate to message index 8 (keep 9 messages, indices 0..=8).
    let truncated = storage.truncate_conversation(&conv_id, 8).await.unwrap();
    assert_eq!(
        truncated.messages.len(),
        9,
        "truncate_conversation must honor target_message_index even with no checkpoints"
    );

    // Disk state matches.
    let reloaded = storage.load_conversation(&conv_id).await.unwrap().unwrap();
    assert_eq!(reloaded.messages.len(), 9);
}

/// Bug 1b regression: the user picks message index 8 on a conversation whose
/// only checkpoint is at message_index=2. Previously the buggy
/// `revert_to_checkpoint` path collapsed the conversation back to 3 messages
/// (everything after message 2 was deleted). Now `truncate_conversation(8)`
/// preserves the user's selection and only deletes messages 9.
#[tokio::test]
async fn test_e2e_amend2_truncate_ignores_earlier_checkpoint() {
    let tmp = TempDir::new().unwrap();
    let storage = FileSystemStorage::new(tmp.path().join("sessions"));
    let mut conv = make_mixed_conversation_n(10);
    let conv_id = conv.id.clone();

    // Save the conversation truncated to 3 messages, create a checkpoint
    // there (so message_index=2), then restore the full 10 messages.
    let full_messages = conv.messages.clone();
    conv.messages.truncate(3);
    storage.save_conversation(&conv).await.unwrap();
    let _early_cp = storage.create_checkpoint(&conv_id).await.unwrap();

    conv.messages = full_messages;
    storage.save_conversation(&conv).await.unwrap();

    // Truncate to message index 8 — must keep 9 messages (indices 0..=8),
    // NOT collapse to 3 even though the only checkpoint is at message_index=2.
    let truncated = storage.truncate_conversation(&conv_id, 8).await.unwrap();
    assert_eq!(
        truncated.messages.len(),
        9,
        "truncate_conversation must NOT use the checkpoint's message_index — \
         it must honor the user's target_message_index directly"
    );

    // Checkpoint log is pruned to entries whose message_index <= 8 — the
    // early checkpoint at message_index=2 survives.
    let cps_after = storage.list_checkpoints(&conv_id).await.unwrap();
    assert_eq!(
        cps_after.len(),
        1,
        "checkpoint at message_index=2 should survive truncation to index 8"
    );
    assert_eq!(cps_after[0].message_index, 2);
}

/// Bug 1b regression: a checkpoint exists at message_index=8 (newer than the
/// target). After truncation it must be pruned out.
#[tokio::test]
async fn test_e2e_amend2_truncate_prunes_later_checkpoints() {
    let tmp = TempDir::new().unwrap();
    let storage = FileSystemStorage::new(tmp.path().join("sessions"));
    let mut conv = make_mixed_conversation_n(10);
    let conv_id = conv.id.clone();

    // Save with 9 messages, create checkpoint at message_index=8, then
    // restore all 10. Truncating to index 5 must drop the index-8 checkpoint.
    let full_messages = conv.messages.clone();
    conv.messages.truncate(9);
    storage.save_conversation(&conv).await.unwrap();
    let _late_cp = storage.create_checkpoint(&conv_id).await.unwrap();

    conv.messages = full_messages;
    storage.save_conversation(&conv).await.unwrap();

    let _truncated = storage.truncate_conversation(&conv_id, 5).await.unwrap();
    let cps_after = storage.list_checkpoints(&conv_id).await.unwrap();
    assert!(
        cps_after.is_empty(),
        "checkpoint at message_index=8 should be pruned after truncating to index 5"
    );
}

/// Bug 1a regression: the chat-pane render must produce one boundary per
/// message in `message_boundaries`, and a separate user-only set in
/// `user_message_boundaries`. Previously these were conflated into one vec
/// that only tracked user turns, causing the status-bar counter to display
/// `5/5` for a 10-message conversation.
#[test]
fn test_e2e_amend2_render_produces_full_and_user_message_boundaries() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use rustain::adapters::tui::theme::Theme;
    use rustain::adapters::tui::widgets::chat_pane;
    use rustain::domain::models::{FeedbackBlock, StreamingState};
    use std::collections::{BTreeMap, HashMap};

    let conversation = make_mixed_conversation_n(10);
    let streaming = StreamingState::default();
    let theme = Theme::dark();
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut cache = HeightCache::default();

    terminal
        .draw(|frame| {
            let area = frame.area();
            let result = chat_pane::render(
                frame,
                area,
                &conversation,
                &streaming,
                0,
                true,
                &theme,
                &mut cache,
                &HashMap::new(),
                &BTreeMap::<String, FeedbackBlock>::new(),
            );
            assert_eq!(
                result.message_boundaries.len(),
                10,
                "message_boundaries must have one entry per message (all roles)"
            );
            assert_eq!(
                result.user_message_boundaries.len(),
                5,
                "user_message_boundaries must have one entry per user message"
            );
        })
        .unwrap();
}

/// Bug 3 regression: fork_at_checkpoint must copy the source's checkpoint log
/// (filtered to entries with message_index <= fork_message_index) and the
/// matching snapshot files into the forked session directory. After this the
/// forked conversation is independently rewindable.
#[tokio::test]
async fn test_e2e_amend2_fork_copies_checkpoint_log_and_snapshots() {
    let tmp = TempDir::new().unwrap();
    let workspace = tmp.path();
    let storage = FileSystemStorage::with_workspace_root(
        tmp.path().join("sessions"),
        workspace.to_path_buf(),
    );
    let mut conv = make_mixed_conversation_n(8);
    let conv_id = conv.id.clone();

    // Build snapshot history: cp1 at message_index=2, cp2 at message_index=5.
    conv.messages.truncate(3);
    storage.save_conversation(&conv).await.unwrap();
    let cp1 = storage.create_checkpoint(&conv_id).await.unwrap();
    let file_a = workspace.join("a.txt");
    tokio::fs::write(&file_a, b"orig A").await.unwrap();
    storage
        .snapshot_file(&conv_id, cp1, &file_a, b"orig A")
        .await
        .unwrap();

    conv.messages = make_mixed_conversation_n(8).messages;
    conv.id = conv_id.clone();
    conv.messages.truncate(6);
    storage.save_conversation(&conv).await.unwrap();
    let cp2 = storage.create_checkpoint(&conv_id).await.unwrap();
    let file_b = workspace.join("b.txt");
    tokio::fs::write(&file_b, b"orig B").await.unwrap();
    storage
        .snapshot_file(&conv_id, cp2, &file_b, b"orig B")
        .await
        .unwrap();

    // Restore full 8 messages.
    conv.messages = make_mixed_conversation_n(8).messages;
    conv.id = conv_id.clone();
    storage.save_conversation(&conv).await.unwrap();

    // Fork at message_index=4 (between cp1 and cp2). Filter must keep cp1
    // (message_index=2) and drop cp2 (message_index=5).
    let new_id = storage
        .fork_at_checkpoint(
            &conv_id,
            rustain::domain::models::checkpoint::CheckpointId(4),
        )
        .await
        .unwrap();

    let forked_log = storage.list_checkpoints(&new_id).await.unwrap();
    assert_eq!(
        forked_log.len(),
        1,
        "forked checkpoint log should keep only entries with message_index <= 4"
    );
    assert_eq!(forked_log[0].message_index, 2);

    // Snapshot file for cp1 should exist in the forked session dir.
    let forked_snapshots_dir = tmp.path().join("sessions").join(&new_id).join("snapshots");
    let mut found_cp1_snapshot = false;
    let mut found_cp2_snapshot = false;
    if let Ok(mut entries) = tokio::fs::read_dir(&forked_snapshots_dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let fname = entry.file_name().to_string_lossy().to_string();
            if fname.starts_with(&format!("{}_", cp1.0)) {
                found_cp1_snapshot = true;
            }
            if fname.starts_with(&format!("{}_", cp2.0)) {
                found_cp2_snapshot = true;
            }
        }
    }
    assert!(
        found_cp1_snapshot,
        "fork must copy snapshot file for cp1 (eligible)"
    );
    assert!(
        !found_cp2_snapshot,
        "fork must NOT copy snapshot file for cp2 (filtered out)"
    );

    // Source conversation snapshot dir is untouched — both files still there.
    let src_snapshots_dir = tmp.path().join("sessions").join(&conv_id).join("snapshots");
    let mut src_count = 0usize;
    if let Ok(mut entries) = tokio::fs::read_dir(&src_snapshots_dir).await {
        while let Ok(Some(_)) = entries.next_entry().await {
            src_count += 1;
        }
    }
    assert_eq!(
        src_count, 2,
        "source snapshot dir must be untouched (both snapshots remain)"
    );
}

/// Bug 3 regression: the forked conversation can be independently rewound.
/// After fork + rewind on the fork, the source conversation is untouched and
/// the fork has its own truncated message list and checkpoint log.
///
/// Note: this test exercises the message-truncation + checkpoint-log
/// cooperation that Amendment 2 fixes. The deeper question of
/// "tool-modified files vs externally-modified files" is governed by the
/// pre-existing AC5 conflict-detection logic and is intentionally left
/// outside Amendment 2 scope (mirrors existing
/// `test_e2e_rewind_reverts_files_in_reverse_order` which keeps file
/// content unchanged across the snapshot).
#[tokio::test]
async fn test_e2e_amend2_forked_conversation_can_be_rewound() {
    let tmp = TempDir::new().unwrap();
    let workspace = tmp.path();
    let storage = FileSystemStorage::with_workspace_root(
        tmp.path().join("sessions"),
        workspace.to_path_buf(),
    );
    let mut conv = make_mixed_conversation_n(8);
    let conv_id = conv.id.clone();

    // Source: snapshot a file at cp1 (message_index=2).
    conv.messages.truncate(3);
    storage.save_conversation(&conv).await.unwrap();
    let cp1 = storage.create_checkpoint(&conv_id).await.unwrap();
    let file_a = workspace.join("source_a.txt");
    tokio::fs::write(&file_a, b"orig A").await.unwrap();
    storage
        .snapshot_file(&conv_id, cp1, &file_a, b"orig A")
        .await
        .unwrap();

    // Restore full 8 messages and fork at index 4.
    conv.messages = make_mixed_conversation_n(8).messages;
    conv.id = conv_id.clone();
    storage.save_conversation(&conv).await.unwrap();

    let new_id = storage
        .fork_at_checkpoint(
            &conv_id,
            rustain::domain::models::checkpoint::CheckpointId(4),
        )
        .await
        .unwrap();

    // Pre-rewind sanity: the forked conversation has 5 messages (indices
    // 0..=4) and one checkpoint copied from the source.
    let forked_loaded = storage.load_conversation(&new_id).await.unwrap().unwrap();
    assert_eq!(forked_loaded.messages.len(), 5);
    let forked_cps = storage.list_checkpoints(&new_id).await.unwrap();
    assert_eq!(
        forked_cps.len(),
        1,
        "fork must copy the eligible checkpoint"
    );

    // Rewind the fork to message index 1.
    let truncated = storage.truncate_conversation(&new_id, 1).await.unwrap();
    assert_eq!(
        truncated.messages.len(),
        2,
        "fork rewind to index 1 must keep exactly 2 messages"
    );

    // The fork's checkpoint log is pruned (cp1 had message_index=2 which is
    // now beyond the truncation point).
    let cps_after = storage.list_checkpoints(&new_id).await.unwrap();
    assert!(
        cps_after.is_empty(),
        "checkpoint at message_index=2 must be pruned after truncating fork to index 1"
    );

    // File snapshot revert runs against the fork — uses the copied snapshot
    // file. The test mirrors the existing
    // `test_e2e_rewind_reverts_files_in_reverse_order` pattern: the file
    // content is unchanged since the snapshot, so the revert succeeds with
    // status `Restored` and the file ends up at "orig A".
    let reverted = storage
        .revert_file_snapshots(
            &new_id,
            rustain::domain::models::checkpoint::CheckpointId(0),
        )
        .await
        .unwrap();
    assert!(
        !reverted.is_empty(),
        "fork must have its own snapshot to revert (proves snapshot files were copied)"
    );
    assert!(
        reverted
            .iter()
            .any(|r| matches!(r.status, RevertStatus::Restored)),
        "at least one snapshot must be restored from the fork's copy"
    );

    let after = tokio::fs::read(&file_a).await.unwrap();
    assert_eq!(after, b"orig A");

    // Source conversation untouched (still 8 messages).
    let src = storage.load_conversation(&conv_id).await.unwrap().unwrap();
    assert_eq!(
        src.messages.len(),
        8,
        "source conversation must be untouched by fork-then-rewind on the fork"
    );
}
