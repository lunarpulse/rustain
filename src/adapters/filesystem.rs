//! FileSystemStorage adapter — persists conversations to `{workspace}/.claude/sessions/`.
//!
//! Implements `StoragePort` using async file I/O via `tokio::fs`.
//! Session files use CC-compatible camelCase JSON format (`.meta.json`).

use std::path::PathBuf;

use async_trait::async_trait;

use crate::domain::errors::StorageError;
use crate::domain::models::{Conversation, ConversationSummary};
use crate::domain::ports::StoragePort;

/// Filesystem-backed storage for conversations.
///
/// Stores each conversation as `{sessions_dir}/{id}.meta.json`.
#[derive(Debug)]
pub struct FileSystemStorage {
    sessions_dir: PathBuf,
}

impl FileSystemStorage {
    /// Create a new `FileSystemStorage` targeting the given sessions directory.
    pub fn new(sessions_dir: PathBuf) -> Self {
        Self { sessions_dir }
    }

    /// Ensure the sessions directory exists.
    pub async fn ensure_dir(&self) -> Result<(), StorageError> {
        tokio::fs::create_dir_all(&self.sessions_dir)
            .await
            .map_err(|e| StorageError::IoError(format!("Failed to create sessions dir: {}", e)))
    }

    /// Build the file path for a given conversation ID.
    ///
    /// Validates that the ID contains only safe characters (alphanumeric, `-`, `_`)
    /// to prevent path traversal attacks from crafted session files.
    fn session_path(&self, id: &str) -> PathBuf {
        self.sessions_dir
            .join(format!("{}.meta.json", Self::sanitize_id(id)))
    }

    /// Sanitize a conversation ID to prevent path traversal.
    /// Delegates to shared utility; returns "invalid" on failure for backward compatibility.
    fn sanitize_id(id: &str) -> &str {
        crate::infrastructure::utils::sanitize_id(id).unwrap_or("invalid")
    }

    /// Save a conversation with the `clean_exit` flag set (used for graceful shutdown).
    pub async fn save_conversation_with_exit(
        &self,
        conv: &Conversation,
        clean_exit: bool,
    ) -> Result<(), StorageError> {
        self.ensure_dir().await?;

        let persisted = PersistedConversation::from_conversation_with_exit(conv, clean_exit);
        let json = serde_json::to_string_pretty(&persisted)
            .map_err(|e| StorageError::SerializationError(e.to_string()))?;

        let sanitized = Self::sanitize_id(&conv.id);
        let path = self.session_path(&conv.id);
        let tmp_path = self
            .sessions_dir
            .join(format!("{}.meta.json.tmp", sanitized));
        tokio::fs::write(&tmp_path, json.as_bytes())
            .await
            .map_err(|e| StorageError::IoError(format!("Failed to write session file: {}", e)))?;
        if let Err(e) = tokio::fs::rename(&tmp_path, &path).await {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return Err(StorageError::IoError(format!(
                "Failed to rename session file: {}",
                e
            )));
        }

        Ok(())
    }

    /// Load a conversation and return its `clean_exit` flag for crash detection.
    pub async fn load_conversation_with_exit(
        &self,
        id: &str,
    ) -> Result<Option<(Conversation, bool)>, StorageError> {
        let path = self.session_path(id);

        let content = match tokio::fs::read_to_string(&path).await {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(StorageError::IoError(format!(
                    "Failed to read session file: {}",
                    e
                )));
            }
        };

        let persisted: PersistedConversation = serde_json::from_str(&content)
            .map_err(|e| StorageError::SerializationError(e.to_string()))?;

        let clean_exit = persisted.clean_exit;
        Ok(Some((persisted.to_conversation(), clean_exit)))
    }
}

#[async_trait]
impl StoragePort for FileSystemStorage {
    async fn save_conversation(&self, conv: &Conversation) -> Result<(), StorageError> {
        self.ensure_dir().await?;

        let persisted = PersistedConversation::from_conversation(conv);
        let json = serde_json::to_string_pretty(&persisted)
            .map_err(|e| StorageError::SerializationError(e.to_string()))?;

        let path = self.session_path(&conv.id);

        // Write atomically: write to temp file, then rename
        let sanitized = Self::sanitize_id(&conv.id);
        let tmp_path = self
            .sessions_dir
            .join(format!("{}.meta.json.tmp", sanitized));
        tokio::fs::write(&tmp_path, json.as_bytes())
            .await
            .map_err(|e| StorageError::IoError(format!("Failed to write session file: {}", e)))?;
        if let Err(e) = tokio::fs::rename(&tmp_path, &path).await {
            // Clean up temp file on rename failure
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return Err(StorageError::IoError(format!(
                "Failed to rename session file: {}",
                e
            )));
        }

        Ok(())
    }

    async fn load_conversation(&self, id: &str) -> Result<Option<Conversation>, StorageError> {
        let path = self.session_path(id);

        let content = match tokio::fs::read_to_string(&path).await {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(StorageError::IoError(format!(
                    "Failed to read session file: {}",
                    e
                )));
            }
        };

        let persisted: PersistedConversation = serde_json::from_str(&content)
            .map_err(|e| StorageError::SerializationError(e.to_string()))?;

        Ok(Some(persisted.to_conversation()))
    }

    async fn list_conversations(&self) -> Result<Vec<ConversationSummary>, StorageError> {
        let mut entries = match tokio::fs::read_dir(&self.sessions_dir).await {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => {
                return Err(StorageError::IoError(format!(
                    "Failed to read sessions dir: {}",
                    e
                )));
            }
        };

        let mut summaries = Vec::new();
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| StorageError::IoError(e.to_string()))?
        {
            let path = entry.path();
            if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(".meta.json") && !n.ends_with(".meta.json.tmp"))
            {
                match tokio::fs::read_to_string(&path).await {
                    Ok(content) => {
                        if let Ok(persisted) =
                            serde_json::from_str::<PersistedConversation>(&content)
                        {
                            summaries.push(ConversationSummary {
                                id: persisted.id,
                                title: persisted.title,
                                created_at: persisted.created_at,
                                updated_at: persisted.updated_at.unwrap_or(persisted.created_at),
                                message_count: persisted.messages.len(),
                            });
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Failed to read session file {}: {}", path.display(), e);
                    }
                }
            }
        }

        // Sort by updatedAt desc (most recent first)
        summaries.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Ok(summaries)
    }
}

// ── PersistedConversation serde wrapper ────────────────────────────

use crate::domain::models::conversation::PersistedConversation;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::{ChatMessage, MessageRole, StopReason, UsageInfo};
    use tempfile::TempDir;

    fn make_test_conversation() -> Conversation {
        Conversation {
            id: "test-conv-123".to_string(),
            title: "Test Conversation".to_string(),
            messages: vec![
                ChatMessage {
                    id: "msg-test-001".to_string(),
                    role: MessageRole::User,
                    content: "Hello".to_string(),
                    content_blocks: vec![],
                    tool_calls: vec![],
                    created_at: 1700000000,
                    token_count: None,
                    stop_reason: None,
                },
                ChatMessage {
                    id: "msg-test-002".to_string(),
                    role: MessageRole::Assistant,
                    content: "Hi there!".to_string(),
                    content_blocks: vec![],
                    tool_calls: vec![],
                    created_at: 1700000001,
                    token_count: Some(10),
                    stop_reason: Some(StopReason::EndTurn),
                },
            ],
            created_at: 1700000000,
            updated_at: 1700000001,
            last_response_at: Some(1700000001),
            session_id: Some("sess-abc".to_string()),
            usage: Some(UsageInfo {
                input_tokens: 5,
                output_tokens: 10,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
            }),
            fork_source: None,
        }
    }

    // 6.1: save_conversation creates valid JSON with camelCase keys
    #[tokio::test]
    async fn test_save_creates_camel_case_json() {
        let tmp = TempDir::new().unwrap();
        let storage = FileSystemStorage::new(tmp.path().join("sessions"));
        let conv = make_test_conversation();

        storage.save_conversation(&conv).await.unwrap();

        let path = tmp.path().join("sessions/test-conv-123.meta.json");
        assert!(path.exists());

        let content = std::fs::read_to_string(&path).unwrap();
        // Verify camelCase field names
        assert!(content.contains("\"createdAt\""));
        assert!(content.contains("\"updatedAt\""));
        assert!(content.contains("\"lastResponseAt\""));
        assert!(content.contains("\"sessionId\""));
        assert!(content.contains("\"contentBlocks\""));
        assert!(content.contains("\"toolCalls\""));
        // Verify it's valid JSON
        let _: serde_json::Value = serde_json::from_str(&content).unwrap();
    }

    // 6.2: load_conversation reads back saved conversation with all fields
    #[tokio::test]
    async fn test_roundtrip_save_load() {
        let tmp = TempDir::new().unwrap();
        let storage = FileSystemStorage::new(tmp.path().join("sessions"));
        let conv = make_test_conversation();

        storage.save_conversation(&conv).await.unwrap();
        let loaded = storage
            .load_conversation("test-conv-123")
            .await
            .unwrap()
            .expect("should load conversation");

        assert_eq!(loaded.id, conv.id);
        assert_eq!(loaded.title, conv.title);
        assert_eq!(loaded.messages.len(), 2);
        assert_eq!(loaded.created_at, conv.created_at);
        assert_eq!(loaded.updated_at, conv.updated_at);
        assert_eq!(loaded.last_response_at, conv.last_response_at);
        assert_eq!(loaded.session_id, conv.session_id);
        assert!(loaded.usage.is_some());
        assert!(loaded.fork_source.is_none());

        // Verify message content preserved
        assert_eq!(loaded.messages[0].content, "Hello");
        assert_eq!(loaded.messages[1].content, "Hi there!");
        assert_eq!(loaded.messages[1].stop_reason, Some(StopReason::EndTurn));
    }

    // 6.3: Forward compatibility — file with unknown fields loads without error
    #[tokio::test]
    async fn test_forward_compatibility_unknown_fields() {
        let tmp = TempDir::new().unwrap();
        let sessions_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        // Write a JSON file with extra unknown fields (simulating a future version)
        let json = r#"{
            "id": "future-conv",
            "title": "Future Session",
            "messages": [],
            "createdAt": 1700000000,
            "updatedAt": 1700000001,
            "unknownField": "some value",
            "anotherNewField": { "nested": true },
            "enabledMcpServers": ["server1"]
        }"#;
        std::fs::write(sessions_dir.join("future-conv.meta.json"), json).unwrap();

        let storage = FileSystemStorage::new(sessions_dir);
        let loaded = storage
            .load_conversation("future-conv")
            .await
            .unwrap()
            .expect("should load despite unknown fields");

        assert_eq!(loaded.id, "future-conv");
        assert_eq!(loaded.title, "Future Session");
        assert_eq!(loaded.updated_at, 1700000001);
    }

    // 6.4: list_conversations returns sorted summaries
    #[tokio::test]
    async fn test_list_conversations_sorted() {
        let tmp = TempDir::new().unwrap();
        let storage = FileSystemStorage::new(tmp.path().join("sessions"));

        // Save three conversations with different timestamps
        for (id, ts) in [("c1", 1000), ("c2", 3000), ("c3", 2000)] {
            let mut conv = make_test_conversation();
            conv.id = id.to_string();
            conv.title = format!("Conv {}", id);
            conv.updated_at = ts;
            storage.save_conversation(&conv).await.unwrap();
        }

        let summaries = storage.list_conversations().await.unwrap();
        assert_eq!(summaries.len(), 3);
        // Sorted by updatedAt desc
        assert_eq!(summaries[0].id, "c2");
        assert_eq!(summaries[1].id, "c3");
        assert_eq!(summaries[2].id, "c1");
        assert_eq!(summaries[0].message_count, 2);
    }

    // 6.5: Missing sessions directory is auto-created
    #[tokio::test]
    async fn test_auto_create_sessions_dir() {
        let tmp = TempDir::new().unwrap();
        let sessions_dir = tmp.path().join("deeply").join("nested").join("sessions");
        let storage = FileSystemStorage::new(sessions_dir.clone());

        assert!(!sessions_dir.exists());

        let conv = make_test_conversation();
        storage.save_conversation(&conv).await.unwrap();

        assert!(sessions_dir.exists());
        assert!(sessions_dir.join("test-conv-123.meta.json").exists());
    }

    // Load non-existent conversation returns None
    #[tokio::test]
    async fn test_load_nonexistent_returns_none() {
        let tmp = TempDir::new().unwrap();
        let storage = FileSystemStorage::new(tmp.path().join("sessions"));

        let result = storage.load_conversation("nonexistent").await.unwrap();
        assert!(result.is_none());
    }

    // List on non-existent directory returns empty
    #[tokio::test]
    async fn test_list_nonexistent_dir_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let storage = FileSystemStorage::new(tmp.path().join("no-such-dir"));

        let summaries = storage.list_conversations().await.unwrap();
        assert!(summaries.is_empty());
    }

    // 7.3: clean_exit flag defaults to false and is true after graceful save
    #[tokio::test]
    async fn test_clean_exit_false_by_default() {
        let tmp = TempDir::new().unwrap();
        let storage = FileSystemStorage::new(tmp.path().join("sessions"));
        let conv = make_test_conversation();

        // Normal save (via StoragePort) sets clean_exit = false
        storage.save_conversation(&conv).await.unwrap();

        let (_, clean_exit) = storage
            .load_conversation_with_exit("test-conv-123")
            .await
            .unwrap()
            .expect("should load");
        assert!(!clean_exit, "default save should have clean_exit = false");
    }

    #[tokio::test]
    async fn test_clean_exit_true_after_graceful_shutdown() {
        let tmp = TempDir::new().unwrap();
        let storage = FileSystemStorage::new(tmp.path().join("sessions"));
        let conv = make_test_conversation();

        // Graceful shutdown save sets clean_exit = true
        storage
            .save_conversation_with_exit(&conv, true)
            .await
            .unwrap();

        let (_, clean_exit) = storage
            .load_conversation_with_exit("test-conv-123")
            .await
            .unwrap()
            .expect("should load");
        assert!(clean_exit, "graceful save should have clean_exit = true");
    }

    #[tokio::test]
    async fn test_clean_exit_backward_compat() {
        // Old session files without clean_exit field should default to false
        let tmp = TempDir::new().unwrap();
        let sessions_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let json = r#"{
            "id": "old-session",
            "title": "Old Session",
            "messages": [],
            "createdAt": 1700000000
        }"#;
        std::fs::write(sessions_dir.join("old-session.meta.json"), json).unwrap();

        let storage = FileSystemStorage::new(sessions_dir);
        let (_, clean_exit) = storage
            .load_conversation_with_exit("old-session")
            .await
            .unwrap()
            .expect("should load old session");
        assert!(
            !clean_exit,
            "old sessions without clean_exit should default to false (trigger recovery)"
        );
    }
}
