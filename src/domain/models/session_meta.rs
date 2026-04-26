use serde::{Deserialize, Serialize};

use super::conversation::ForkSource;

/// Import provenance — set during `rustain migrate --from <source>`.
/// `None` for native rustain sessions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ImportSource {
    /// Source tool identifier (e.g., "claude-code").
    pub source: String,
    /// Original session ID in the source tool.
    pub original_session_id: String,
    /// Unix timestamp of the import operation.
    pub imported_at: i64,
}

/// Lightweight session metadata for sidebar display (sidecar file).
/// Stored as `{id}.session.json` alongside the full `{id}.meta.json` conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMeta {
    /// Schema version for forward compatibility.
    pub version: u32,
    /// Conversation title (auto-generated or user-edited).
    pub title: String,
    /// Unix timestamp (seconds) when conversation was created.
    pub created_at: i64,
    /// Unix timestamp (seconds) of last activity (message added, title updated).
    pub updated_at: i64,
    /// Number of messages in the conversation.
    pub message_count: usize,
    /// Bookmarked message indices (for Story 4.4).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bookmarks: Vec<usize>,
    /// Mirror of `Conversation.fork_source` for fast sidebar rendering (Story
    /// 4-3a.1 / DF-095). The sidebar render path must not touch disk, so the
    /// fork indicator is read from SessionSummary (built from this field).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork_source: Option<ForkSource>,
    /// Import provenance — set during `rustain migrate --from <source>`.
    /// `None` for native rustain sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imported_from: Option<ImportSource>,
    /// Plan mode slug — stable identifier for the plan file of this session.
    /// Generated on first Plan-mode entry, persisted across restarts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_slug: Option<String>,
    /// Lossless round-trip of unknown fields (DF-088). Any serde fields we
    /// don't know about are captured here and written back on re-save so
    /// cross-version saves never silently drop data.
    #[serde(flatten, default)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl SessionMeta {
    /// Create a new SessionMeta with current timestamp.
    #[allow(dead_code)]
    pub fn new(title: String) -> Self {
        let now = now_unix();
        Self {
            version: 1,
            title,
            created_at: now,
            updated_at: now,
            message_count: 0,
            bookmarks: Vec::new(),
            fork_source: None,
            imported_from: None,
            plan_slug: None,
            extra: serde_json::Map::new(),
        }
    }

    /// Create from conversation data.
    pub fn from_conversation(conv: &super::conversation::Conversation) -> Self {
        Self {
            version: 1,
            title: conv.title.clone(),
            created_at: conv.created_at,
            updated_at: conv.updated_at,
            message_count: conv.messages.len(),
            bookmarks: Vec::new(),
            fork_source: conv.fork_source.clone(),
            imported_from: None,
            plan_slug: None,
            extra: serde_json::Map::new(),
        }
    }

    /// Update metadata after a new message is added.
    #[allow(dead_code)]
    pub fn touch(&mut self, message_count: usize) {
        self.updated_at = now_unix();
        self.message_count = message_count;
    }

    /// Update the title.
    #[allow(dead_code)]
    pub fn set_title(&mut self, title: String) {
        self.title = title;
        self.updated_at = now_unix();
    }
}

/// Shorten text to max_chars, preserving word boundaries.
/// Uses char_indices for UTF-8 safe truncation.
#[allow(dead_code)]
pub fn shorten_text(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }

    // Find the byte index at the max_chars boundary
    let limit_byte_idx = text
        .char_indices()
        .nth(max_chars)
        .map(|(b, _)| b)
        .unwrap_or(text.len());

    // Try to find a word boundary (whitespace) within the last 10 chars
    let break_point = find_word_boundary(text, limit_byte_idx);
    format!("{}...", &text[..break_point])
}

/// Find a good word boundary before the given byte index.
/// Returns byte index (not char index) for safe UTF-8 slicing.
fn find_word_boundary(text: &str, before_byte_idx: usize) -> usize {
    let end = before_byte_idx.min(text.len());
    let slice = &text[..end];

    // Look for whitespace within the last 10 characters
    let search_start_char = slice.chars().count().saturating_sub(10);
    let search_start_byte = slice
        .char_indices()
        .nth(search_start_char)
        .map(|(b, _)| b)
        .unwrap_or(0);

    // Search for whitespace in that range
    if let Some(pos) = slice[search_start_byte..].rfind(|c: char| c.is_whitespace()) {
        return search_start_byte + pos;
    }
    // No word boundary found, truncate at the limit
    end
}

/// Extract a title from the first user message.
/// Returns the first line, shortened to max 50 chars.
#[allow(dead_code)]
pub fn extract_title_from_first_message(content: &str) -> String {
    // Get the first line
    let first_line = content.lines().next().unwrap_or("");
    // Trim whitespace
    let trimmed = first_line.trim();
    // Shorten if needed
    shorten_text(trimmed, 50)
}

pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shorten_text_no_truncation_needed() {
        assert_eq!(shorten_text("Hello", 10), "Hello");
        assert_eq!(shorten_text("", 10), "");
    }

    #[test]
    fn test_shorten_text_exact_boundary() {
        let text = "12345";
        assert_eq!(shorten_text(text, 5), "12345");
    }

    #[test]
    fn test_shorten_text_truncation_with_word_boundary() {
        let text = "This is a very long title that needs truncation";
        let result = shorten_text(text, 20);
        assert!(result.ends_with("..."));
        assert!(result.len() <= 23); // 20 + "..."
        // Should break at word boundary
        assert!(!result.contains(" titl")); // Partial word
    }

    #[test]
    fn test_shorten_text_multibyte_chars() {
        let text = "こんにちは世界これは長いテキストです";
        let result = shorten_text(text, 10);
        assert!(result.ends_with("..."));
        // Check it's valid UTF-8 (no panic)
        assert!(result.chars().count() <= 13);
    }

    #[test]
    fn test_shorten_text_no_word_boundary() {
        // A very long word with no spaces
        let text = "supercalifragilisticexpialidocious";
        let result = shorten_text(text, 10);
        assert!(result.ends_with("..."));
        assert_eq!(result.len(), 13); // 10 + "..."
    }

    #[test]
    fn test_extract_title_from_first_message() {
        assert_eq!(
            extract_title_from_first_message("Hello world"),
            "Hello world"
        );
        assert_eq!(extract_title_from_first_message("  Trimmed  "), "Trimmed");
    }

    #[test]
    fn test_extract_title_multiline() {
        let content = "First line\nSecond line\nThird line";
        assert_eq!(extract_title_from_first_message(content), "First line");
    }

    #[test]
    fn test_extract_title_long_first_line() {
        let content = "This is a very very very very very very very very very very very very very very long first line";
        let result = extract_title_from_first_message(content);
        assert!(result.ends_with("..."));
        assert!(result.len() <= 53); // 50 + "..."
    }

    #[test]
    fn test_session_meta_serialization_roundtrip() {
        let meta = SessionMeta {
            version: 1,
            title: "Test Session".to_string(),
            created_at: 1700000000,
            updated_at: 1700000100,
            message_count: 5,
            bookmarks: vec![1, 3],
            fork_source: None,
            imported_from: None,
            plan_slug: None,
            extra: serde_json::Map::new(),
        };

        let json = serde_json::to_string(&meta).unwrap();
        let deserialized: SessionMeta = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.version, meta.version);
        assert_eq!(deserialized.title, meta.title);
        assert_eq!(deserialized.created_at, meta.created_at);
        assert_eq!(deserialized.updated_at, meta.updated_at);
        assert_eq!(deserialized.message_count, meta.message_count);
        assert_eq!(deserialized.bookmarks, meta.bookmarks);
    }

    #[test]
    fn test_session_meta_from_conversation() {
        use super::super::conversation::Conversation;

        let conv = Conversation {
            id: "test-id".to_string(),
            title: "Test Title".to_string(),
            messages: vec![],
            created_at: 1700000000,
            updated_at: 1700000100,
            last_response_at: None,
            session_id: None,
            usage: None,
            fork_source: None,
        };

        let meta = SessionMeta::from_conversation(&conv);
        assert_eq!(meta.title, "Test Title");
        assert_eq!(meta.created_at, 1700000000);
        assert_eq!(meta.updated_at, 1700000100);
        assert_eq!(meta.message_count, 0);
    }

    #[test]
    fn test_session_meta_touch() {
        let mut meta = SessionMeta::new("Test".to_string());
        // Manually set the timestamp so we can test the change
        meta.updated_at = 1700000000;
        let original_updated = meta.updated_at;

        meta.touch(10);

        assert_eq!(meta.message_count, 10);
        assert!(meta.updated_at >= original_updated);
    }

    #[test]
    fn test_session_meta_set_title() {
        let mut meta = SessionMeta::new("Old Title".to_string());
        // Manually set the timestamp so we can test the change
        meta.updated_at = 1700000000;
        let original_updated = meta.updated_at;

        meta.set_title("New Title".to_string());

        assert_eq!(meta.title, "New Title");
        assert!(meta.updated_at >= original_updated);
    }

    #[test]
    fn test_forward_compatibility_unknown_fields() {
        // Simulate a future version with new fields
        let json = r#"{
            "version": 2,
            "title": "Future Session",
            "createdAt": 1700000000,
            "updatedAt": 1700000100,
            "messageCount": 3,
            "bookmarks": [],
            "futureField": "unknown value",
            "anotherNewField": 42
        }"#;

        let meta: SessionMeta = serde_json::from_str(json).unwrap();
        assert_eq!(meta.version, 2);
        assert_eq!(meta.title, "Future Session");
    }

    #[test]
    fn test_session_meta_with_imported_from_roundtrip() {
        let meta = SessionMeta {
            version: 1,
            title: "Imported Session".to_string(),
            created_at: 1700000000,
            updated_at: 1700000100,
            message_count: 10,
            bookmarks: vec![],
            fork_source: None,
            imported_from: Some(ImportSource {
                source: "claude-code".to_string(),
                original_session_id: "abc-123-def".to_string(),
                imported_at: 1700000200,
            }),
            plan_slug: None,
            extra: serde_json::Map::new(),
        };

        let json = serde_json::to_string(&meta).unwrap();
        let deserialized: SessionMeta = serde_json::from_str(&json).unwrap();

        let imported = deserialized.imported_from.as_ref().unwrap();
        assert_eq!(imported.source, "claude-code");
        assert_eq!(imported.original_session_id, "abc-123-def");
        assert_eq!(imported.imported_at, 1700000200);
    }

    #[test]
    fn test_session_meta_without_imported_from_omits_field() {
        let meta = SessionMeta {
            version: 1,
            title: "Native Session".to_string(),
            created_at: 1700000000,
            updated_at: 1700000100,
            message_count: 3,
            bookmarks: vec![],
            fork_source: None,
            imported_from: None,
            plan_slug: None,
            extra: serde_json::Map::new(),
        };

        let json = serde_json::to_string(&meta).unwrap();
        assert!(
            !json.contains("importedFrom"),
            "importedFrom must not appear when None: {}",
            json
        );
    }

    #[test]
    fn test_session_meta_legacy_json_without_imported_from_deserializes() {
        let json = r#"{
            "version": 1,
            "title": "Legacy Session",
            "createdAt": 1700000000,
            "updatedAt": 1700000100,
            "messageCount": 3
        }"#;

        let meta: SessionMeta = serde_json::from_str(json).unwrap();
        assert!(
            meta.imported_from.is_none(),
            "legacy JSON without importedFrom must deserialize with None"
        );
    }

    #[test]
    fn test_session_meta_imported_from_preserves_extra_fields() {
        let json = r#"{
            "version": 1,
            "title": "Extended Session",
            "createdAt": 1700000000,
            "updatedAt": 1700000100,
            "messageCount": 5,
            "importedFrom": {
                "source": "claude-code",
                "originalSessionId": "uuid-xxx",
                "importedAt": 1700000300
            },
            "futureField": "x"
        }"#;

        let meta: SessionMeta = serde_json::from_str(json).unwrap();
        assert!(meta.imported_from.is_some());
        assert_eq!(
            meta.extra.get("futureField").and_then(|v| v.as_str()),
            Some("x"),
            "futureField must survive in extra map"
        );

        // Re-serialize and verify both fields survive
        let resaved = serde_json::to_string(&meta).unwrap();
        assert!(
            resaved.contains("\"importedFrom\""),
            "importedFrom must survive re-save: {}",
            resaved
        );
        assert!(
            resaved.contains("\"futureField\""),
            "futureField must survive re-save: {}",
            resaved
        );
    }

    /// DF-088 regression: unknown fields written by a newer rustain must survive
    /// a round-trip through an older rustain's load→save cycle. Without the
    /// `extra` flatten-map, serde would silently drop them on re-save and
    /// forward-compat data would be lost.
    #[test]
    fn test_session_meta_preserves_unknown_fields_on_resave() {
        // Simulated on-disk JSON written by a future rustain that knows about
        // `futureField` and `nestedMeta`.
        let original_json = r#"{
            "version": 2,
            "title": "Future Session",
            "createdAt": 1700000000,
            "updatedAt": 1700000100,
            "messageCount": 3,
            "bookmarks": [1, 2],
            "futureField": "opaque string",
            "nestedMeta": {
                "reviewer": "alice",
                "score": 7
            }
        }"#;

        // Load → unknowns land in `extra`.
        let meta: SessionMeta = serde_json::from_str(original_json).unwrap();
        assert_eq!(
            meta.extra.get("futureField").and_then(|v| v.as_str()),
            Some("opaque string"),
            "futureField must be captured in extra map"
        );
        assert!(
            meta.extra.contains_key("nestedMeta"),
            "nestedMeta must be captured in extra map"
        );

        // Re-save → unknowns must appear back in the output JSON.
        let resaved = serde_json::to_string(&meta).unwrap();
        assert!(
            resaved.contains("\"futureField\""),
            "re-saved JSON must contain futureField: {}",
            resaved
        );
        assert!(
            resaved.contains("\"opaque string\""),
            "re-saved JSON must preserve futureField value: {}",
            resaved
        );
        assert!(
            resaved.contains("\"nestedMeta\""),
            "re-saved JSON must contain nestedMeta: {}",
            resaved
        );
        assert!(
            resaved.contains("\"reviewer\""),
            "re-saved JSON must preserve nested field: {}",
            resaved
        );

        // And a second round-trip must be stable (load → save → load equal).
        let meta2: SessionMeta = serde_json::from_str(&resaved).unwrap();
        assert_eq!(meta.extra, meta2.extra);
    }
}
