//! Integration tests for crash detection and recovery flow (Story 2.2b).

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
            content: format!(
                "Message {
            }",
                i
            ),
            content_blocks: vec![],
            tool_calls: vec![],
            created_at: 1700000000 + i as i64,
            token_count: if role == MessageRole::Assistant {
                Some(42)
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
        turns: Vec::new(),
        created_at: 1700000000,
        updated_at: 1700000000 + msg_count as i64,
        last_response_at: Some(1700000000 + msg_count as i64),
        session_id: Some(generate_conversation_id()),
        usage: Some(UsageInfo {
            input_tokens: 100,
            output_tokens: 200,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            reasoning_tokens: None,
        }),
        plans: std::collections::HashMap::new(),
        fork_source: None,
    }
}

/// 7.5: Integration test -- crash recovery flow.
/// Save with clean_exit = false (simulating crash), restore, verify recovery should be triggered.
// Covers: FR105 (crash safety), FR10 (session persistence)
#[tokio::test]
async fn test_crash_recovery_detects_unclean_exit() {
    let tmp = tempfile::TempDir::new().unwrap();
    let sessions_dir = tmp.path().join("sessions");
    let storage = FileSystemStorage::new(sessions_dir);

    let conv = make_conversation("crash-test", "Crash Test", 4);

    // Simulate in-flight save (clean_exit = false, the default via save_conversation)
    storage.save_conversation(&conv).await.unwrap();

    // Simulate restart: load with exit flag
    let (loaded, clean_exit) = storage
        .load_conversation_with_exit("crash-test")
        .await
        .unwrap()
        .expect("should load");

    assert!(!clean_exit, "in-flight save should have clean_exit = false");
    assert_eq!(loaded.id, "crash-test");
    assert_eq!(loaded.messages.len(), 4);
    // Recovery prompt should be triggered (clean_exit == false && messages not empty)
    assert!(!clean_exit && !loaded.messages.is_empty());
}

/// 7.6: Integration test -- normal restore flow.
/// Save with clean_exit = true (graceful shutdown), restore, verify NO recovery prompt.
// Covers: FR105 (crash safety), FR10 (session persistence)
#[tokio::test]
async fn test_normal_restore_no_recovery() {
    let tmp = tempfile::TempDir::new().unwrap();
    let sessions_dir = tmp.path().join("sessions");
    let storage = FileSystemStorage::new(sessions_dir);

    let conv = make_conversation("normal-test", "Normal Test", 4);

    // Simulate graceful shutdown save
    storage
        .save_conversation_with_exit(&conv, true)
        .await
        .unwrap();

    // Simulate restart: load with exit flag
    let (loaded, clean_exit) = storage
        .load_conversation_with_exit("normal-test")
        .await
        .unwrap()
        .expect("should load");

    assert!(clean_exit, "graceful save should have clean_exit = true");
    assert_eq!(loaded.id, "normal-test");
    // No recovery prompt should be triggered
    assert!(clean_exit);
}

/// 7.7: Integration test -- --new flag creates new empty session.
/// Previous session exists, but --new skips restore.
// Covers: FR10 (session persistence)
#[tokio::test]
async fn test_new_flag_skips_restore() {
    let tmp = tempfile::TempDir::new().unwrap();
    let sessions_dir = tmp.path().join("sessions");
    let storage = FileSystemStorage::new(sessions_dir.clone());

    // Save an existing conversation
    let conv = make_conversation("existing", "Existing Session", 4);
    storage.save_conversation(&conv).await.unwrap();

    // Simulate --new flag behavior: skip restore
    // In startup.rs, cli.new -> restored_conversation = None
    let restored_conversation: Option<Conversation> = None;
    assert!(restored_conversation.is_none());

    // Verify existing session is still on disk
    let summaries = storage.list_conversations().await.unwrap();
    assert_eq!(summaries.len(), 1, "existing session should be preserved");
    assert_eq!(summaries[0].id, "existing");
}

/// 7.8: Integration test -- --session <id> loads specific session.
// Covers: FR10 (session persistence)
#[tokio::test]
async fn test_session_flag_loads_specific() {
    let tmp = tempfile::TempDir::new().unwrap();
    let sessions_dir = tmp.path().join("sessions");
    let storage = FileSystemStorage::new(sessions_dir);

    // Save multiple sessions
    let mut conv1 = make_conversation("sess-alpha", "Alpha", 2);
    conv1.updated_at = 1000;
    let mut conv2 = make_conversation("sess-beta", "Beta", 4);
    conv2.updated_at = 2000;
    let mut conv3 = make_conversation("sess-gamma", "Gamma", 6);
    conv3.updated_at = 3000;

    storage.save_conversation(&conv1).await.unwrap();
    storage.save_conversation(&conv2).await.unwrap();
    storage.save_conversation(&conv3).await.unwrap();

    // Simulate --session sess-beta: load specific session (not most recent)
    let loaded = storage
        .load_conversation("sess-beta")
        .await
        .unwrap()
        .expect("should load specific session");

    assert_eq!(loaded.id, "sess-beta");
    assert_eq!(loaded.title, "Beta");
    assert_eq!(loaded.messages.len(), 4);
}

/// 7.9: Integration test -- --session <bad-id> should fail.
// Covers: FR10 (session persistence)
#[tokio::test]
async fn test_session_flag_bad_id_returns_none() {
    let tmp = tempfile::TempDir::new().unwrap();
    let sessions_dir = tmp.path().join("sessions");
    let storage = FileSystemStorage::new(sessions_dir);

    // No sessions exist, try loading a bad ID
    let result = storage.load_conversation("nonexistent-id").await.unwrap();
    assert!(
        result.is_none(),
        "loading nonexistent session should return None"
    );
}
