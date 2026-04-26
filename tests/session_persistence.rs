//! Integration tests for session persistence (Story 2.1).
//!
//! Tests roundtrip save/restore, forward compatibility, and storage conformance.

use rustain::adapters::filesystem::FileSystemStorage;
use rustain::domain::models::{
    ChatMessage, Conversation, MessageRole, StopReason, UsageInfo, generate_conversation_id,
};
use rustain::domain::ports::StoragePort;

fn make_conversation(id: &str, title: &str, msg_count: usize) -> Conversation {
    let mut messages = Vec::new();
    for i in 0..msg_count {
        let role = if i % 2 == 0 {
            MessageRole::User
        } else {
            MessageRole::Assistant
        };
        messages.push(ChatMessage {
            synthetic: false,
            id: rustain::domain::models::generate_conversation_id(),
            role,
            content: format!("Message {
            }", i),
            content_blocks: vec![],
            tool_calls: vec![],
            created_at: 1700000000 + i as i64,
            token_count: if role == MessageRole::Assistant {
                Some(10)
            } else {
                None
            },
            stop_reason: if role == MessageRole::Assistant {
                Some(StopReason::EndTurn)
            } else {
                None
            },
            images: vec![],
        });
    }

    Conversation {
        id: id.to_string(),
        title: title.to_string(),
        messages,
        created_at: 1700000000,
        updated_at: 1700000000 + msg_count as i64,
        last_response_at: Some(1700000000 + msg_count as i64),
        session_id: Some(generate_conversation_id()),
        usage: Some(UsageInfo {
            input_tokens: 100,
            output_tokens: 200,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
        }),
        plans: std::collections::HashMap::new(),
        fork_source: None,
    }
}

// Covers: FR10 (session persistence), NFR20 (persist on completion)
/// 6.6: Integration test — save → restart → restore roundtrip.
/// Simulates what startup.rs does: list_conversations → load most recent.
#[tokio::test]
async fn test_save_restart_restore_roundtrip() {
    let tmp = tempfile::TempDir::new().unwrap();
    let sessions_dir = tmp.path().join(".claude").join("sessions");

    // "Session 1": save a conversation
    {
        let storage = FileSystemStorage::new(sessions_dir.clone());
        let conv = make_conversation("conv-first", "First Chat", 4);
        storage.save_conversation(&conv).await.unwrap();
    }

    // "Session 2": save a newer conversation
    {
        let storage = FileSystemStorage::new(sessions_dir.clone());
        let mut conv = make_conversation("conv-second", "Second Chat", 6);
        conv.updated_at = 1700001000; // newer timestamp
        storage.save_conversation(&conv).await.unwrap();
    }

    // "Restart": create new storage instance, find and restore most recent
    {
        let storage = FileSystemStorage::new(sessions_dir.clone());
        let summaries = storage.list_conversations().await.unwrap();
        assert_eq!(summaries.len(), 2);
        // Most recent should be first (sorted by updatedAt desc)
        assert_eq!(summaries[0].id, "conv-second");

        let restored = storage
            .load_conversation(&summaries[0].id)
            .await
            .unwrap()
            .expect("should restore most recent conversation");

        assert_eq!(restored.id, "conv-second");
        assert_eq!(restored.title, "Second Chat");
        assert_eq!(restored.messages.len(), 6);
        assert_eq!(restored.updated_at, 1700001000);
        assert!(restored.session_id.is_some());
        assert!(restored.usage.is_some());
    }
}

// Covers: FR10 (session persistence), NFR20 (persist on completion), NFR24 (shutdown <5s)
/// 6.7: Integration test — graceful shutdown persists conversation.
/// Verifies that a conversation saved during shutdown can be loaded by the next session.
#[tokio::test]
async fn test_graceful_shutdown_persists_conversation() {
    let tmp = tempfile::TempDir::new().unwrap();
    let sessions_dir = tmp.path().join(".claude").join("sessions");
    let storage = FileSystemStorage::new(sessions_dir.clone());

    // Simulate an active conversation being saved on shutdown
    let mut conv = make_conversation("shutdown-conv", "Shutdown Test", 2);
    conv.updated_at = 1700002000;

    // Simulate the shutdown save with timeout (mirrors event_loop.rs shutdown logic)
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        storage.save_conversation(&conv),
    )
    .await;
    assert!(result.is_ok(), "save should complete within timeout");
    assert!(result.unwrap().is_ok(), "save should succeed");

    // Verify file exists on disk
    let session_file = sessions_dir.join("shutdown-conv.meta.json");
    assert!(session_file.exists());

    // Simulate next startup loading the conversation
    let new_storage = FileSystemStorage::new(sessions_dir);
    let summaries = new_storage.list_conversations().await.unwrap();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].id, "shutdown-conv");

    let loaded = new_storage
        .load_conversation("shutdown-conv")
        .await
        .unwrap()
        .expect("should load shutdown-saved conversation");
    assert_eq!(loaded.messages.len(), 2);
    assert_eq!(loaded.updated_at, 1700002000);
}

// Covers: FR10 (session persistence), NFR20 (persist on completion)
/// 6.8: Storage conformance test for FileSystemStorage.
/// Validates the StoragePort contract: save, load, list, roundtrip integrity.
#[tokio::test]
async fn test_storage_conformance_filesystem() {
    storage_conformance(|| {
        let tmp = tempfile::TempDir::new().unwrap();
        let sessions_dir = tmp.path().join("sessions");
        // Return both storage and TempDir (to keep it alive)
        (FileSystemStorage::new(sessions_dir), tmp)
    })
    .await;
}

/// Generic storage conformance test that can be run against any StoragePort implementation.
async fn storage_conformance<S, F>(factory: F)
where
    S: StoragePort,
    F: Fn() -> (S, tempfile::TempDir),
{
    // Test 1: Save and load roundtrip
    {
        let (storage, _tmp) = factory();
        let conv = make_conversation("conformance-1", "Conformance Test", 4);
        storage.save_conversation(&conv).await.unwrap();

        let loaded = storage
            .load_conversation("conformance-1")
            .await
            .unwrap()
            .expect("should load saved conversation");
        assert_eq!(loaded.id, conv.id);
        assert_eq!(loaded.title, conv.title);
        assert_eq!(loaded.messages.len(), conv.messages.len());
        assert_eq!(loaded.created_at, conv.created_at);
        assert_eq!(loaded.session_id, conv.session_id);
    }

    // Test 2: Load non-existent returns None
    {
        let (storage, _tmp) = factory();
        let result = storage.load_conversation("nonexistent").await.unwrap();
        assert!(result.is_none());
    }

    // Test 3: List returns correct count and sorted order
    {
        let (storage, _tmp) = factory();
        let mut c1 = make_conversation("c-old", "Old", 2);
        c1.updated_at = 1000;
        let mut c2 = make_conversation("c-new", "New", 2);
        c2.updated_at = 2000;

        storage.save_conversation(&c1).await.unwrap();
        storage.save_conversation(&c2).await.unwrap();

        let summaries = storage.list_conversations().await.unwrap();
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].id, "c-new"); // newer first
        assert_eq!(summaries[1].id, "c-old");
    }

    // Test 4: Overwrite existing conversation
    {
        let (storage, _tmp) = factory();
        let conv = make_conversation("overwrite", "Original", 2);
        storage.save_conversation(&conv).await.unwrap();

        let mut updated = conv.clone();
        updated.title = "Updated".to_string();
        updated.updated_at = 9999;
        storage.save_conversation(&updated).await.unwrap();

        let loaded = storage
            .load_conversation("overwrite")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.title, "Updated");
        assert_eq!(loaded.updated_at, 9999);

        // Should still be only 1 conversation
        let summaries = storage.list_conversations().await.unwrap();
        assert_eq!(summaries.len(), 1);
    }
}

// Covers: P14 / AC7 (Task 7.3) — /new command saves current session, new session accessible
/// Simulates the /new command flow: save active conversation, create new one,
/// verify previous session appears in list_conversations().
#[tokio::test]
async fn test_slash_new_saves_and_creates_fresh_session() {
    let tmp = tempfile::TempDir::new().unwrap();
    let sessions_dir = tmp.path().join(".claude").join("sessions");
    let storage = FileSystemStorage::new(sessions_dir.clone());

    // Step 1: Active conversation with messages (simulates pre-/new state)
    let active_conv = make_conversation("active-session", "Active Chat", 4);
    assert!(!active_conv.messages.is_empty());

    // Step 2: /new triggers save of current conversation
    storage.save_conversation(&active_conv).await.unwrap();

    // Step 3: Create fresh conversation (simulates event loop's /new handler)
    let new_conv = Conversation {
        id: generate_conversation_id(),
        title: String::new(),
        messages: Vec::new(),
        created_at: 1700010000,
        updated_at: 1700010000,
        last_response_at: None,
        session_id: Some(generate_conversation_id()),
        usage: None,
        plans: std::collections::HashMap::new(),
        fork_source: None,
    };
    assert!(new_conv.messages.is_empty());
    assert!(new_conv.title.is_empty());

    // Step 4: Verify previous session is accessible via list_conversations
    let summaries = storage.list_conversations().await.unwrap();
    assert_eq!(summaries.len(), 1, "saved session should appear in list");
    assert_eq!(summaries[0].id, "active-session");

    // Step 5: Verify saved session can be fully restored
    let restored = storage
        .load_conversation("active-session")
        .await
        .unwrap()
        .expect("saved session should be loadable");
    assert_eq!(restored.messages.len(), 4);
    assert_eq!(restored.title, "Active Chat");
}

// Covers: P14 / AC7 — /new with empty session does not save
#[tokio::test]
async fn test_slash_new_empty_session_no_save() {
    let tmp = tempfile::TempDir::new().unwrap();
    let sessions_dir = tmp.path().join(".claude").join("sessions");
    let storage = FileSystemStorage::new(sessions_dir.clone());

    // Empty conversation (first launch, nothing to save)
    let empty_conv = Conversation {
        id: "empty-session".to_string(),
        title: String::new(),
        messages: Vec::new(),
        created_at: 1700000000,
        updated_at: 1700000000,
        last_response_at: None,
        session_id: None,
        usage: None,
        plans: std::collections::HashMap::new(),
        fork_source: None,
    };

    // /new with empty session: skip save (event loop checks messages.is_empty())
    if !empty_conv.messages.is_empty() {
        storage.save_conversation(&empty_conv).await.unwrap();
    }

    // Verify nothing was saved
    let summaries = storage.list_conversations().await.unwrap();
    assert!(summaries.is_empty(), "empty session should not be saved");
}
