use crate::domain::models::{ChatMessage, MessageRole};

/// Build a context prefix from conversation history for session expiry rebuild.
///
/// Pure function: no I/O, no async.
///
/// Truncation rules:
/// - Summarize each message to first 200 chars of content
/// - Strip XML-like tags (`<file_context>...</file_context>`, `<file_content>...</file_content>`)
/// - Include only the last `ImageAttachment` reference (as text placeholder)
/// - Prefix with `"Previous conversation context (N messages):\n"`
/// - Format: `"[User]: {summary}\n[Assistant]: {summary}\n..."` for each message
pub fn build_history_context(messages: &[ChatMessage]) -> String {
    if messages.is_empty() {
        return String::new();
    }

    let mut lines = Vec::new();

    // Track the last image reference across all messages
    let mut last_image_ref: Option<String> = None;

    for msg in messages {
        // Strip XML tags from content
        let stripped = strip_xml_tags(&msg.content);

        // Summarize to first 200 chars
        let summary = truncate_chars(&stripped, 200);

        let role_label = match msg.role {
            MessageRole::User => "[User]",
            MessageRole::Assistant => "[Assistant]",
            MessageRole::System => "[System]",
        };

        lines.push(format!("{}: {}", role_label, summary));

        // Track image attachments (we only include the last one across all messages)
        // Images are in ChatMessage content_blocks, but for context rebuild we scan content
        // for image references. Since ChatMessage doesn't store ImageAttachment directly,
        // check for image-like content patterns.
        if msg.content.contains("[Image:") {
            // Extract last image reference from content
            if let Some(pos) = msg.content.rfind("[Image:") {
                if let Some(end) = msg.content[pos..].find(']') {
                    last_image_ref = Some(msg.content[pos..pos + end + 1].to_string());
                }
            }
        }
    }

    let mut result = format!(
        "Previous conversation context ({} messages):\n",
        messages.len()
    );
    result.push_str(&lines.join("\n"));

    if let Some(img_ref) = last_image_ref {
        result.push('\n');
        result.push_str(&img_ref);
    }

    result
}

/// Strip XML-like tags from content (file_context, file_content).
fn strip_xml_tags(content: &str) -> String {
    let mut result = content.to_string();
    // Strip <file_context>...</file_context> tags and their content
    while let Some(start) = result.find("<file_context>") {
        if let Some(relative_end) = result[start..].find("</file_context>") {
            let end = start + relative_end + "</file_context>".len();
            result.replace_range(start..end, "");
        } else {
            break;
        }
    }
    // Strip <file_content>...</file_content> tags and their content
    while let Some(start) = result.find("<file_content>") {
        if let Some(relative_end) = result[start..].find("</file_content>") {
            let end = start + relative_end + "</file_content>".len();
            result.replace_range(start..end, "");
        } else {
            break;
        }
    }
    result.trim().to_string()
}

/// Truncate a string to `max_chars` characters.
fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let mut truncated: String = s.chars().take(max_chars).collect();
        truncated.push_str("...");
        truncated
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::MessageRole;

    fn make_message(role: MessageRole, content: &str) -> ChatMessage {
        ChatMessage {
            id: crate::domain::models::generate_message_id(),
            role,
            content: content.to_string(),
            content_blocks: vec![],
            tool_calls: vec![],
            created_at: 1700000000,
            token_count: None,
            stop_reason: None,
            synthetic: false,
            images: vec![],
        }
    }

    #[test]
    fn test_empty_messages_returns_empty() {
        assert_eq!(build_history_context(&[]), "");
    }

    #[test]
    fn test_basic_context_rebuild() {
        let messages = vec![
            make_message(MessageRole::User, "Hello"),
            make_message(MessageRole::Assistant, "Hi there!"),
        ];
        let result = build_history_context(&messages);
        assert!(result.starts_with("Previous conversation context (2 messages):"));
        assert!(result.contains("[User]: Hello"));
        assert!(result.contains("[Assistant]: Hi there!"));
    }

    #[test]
    fn test_strips_xml_tags() {
        let messages = vec![make_message(
            MessageRole::User,
            "Check this <file_context>some xml data</file_context> please",
        )];
        let result = build_history_context(&messages);
        assert!(result.contains("[User]: Check this  please"));
        assert!(!result.contains("file_context"));
    }

    #[test]
    fn test_strips_file_content_tags() {
        let messages = vec![make_message(
            MessageRole::User,
            "Review <file_content>fn main() {}</file_content> this code",
        )];
        let result = build_history_context(&messages);
        assert!(result.contains("[User]: Review  this code"));
        assert!(!result.contains("file_content"));
    }

    #[test]
    fn test_truncates_long_messages() {
        let long_msg = "A".repeat(300);
        let messages = vec![make_message(MessageRole::User, &long_msg)];
        let result = build_history_context(&messages);
        // Should contain truncated version (200 chars + "...")
        let expected_prefix: String = "A".repeat(200);
        assert!(result.contains(&format!("[User]: {}...", expected_prefix)));
    }

    #[test]
    fn test_includes_last_image_reference() {
        let messages = vec![
            make_message(MessageRole::User, "Look at [Image: photo1.png]"),
            make_message(MessageRole::User, "And also [Image: photo2.png]"),
        ];
        let result = build_history_context(&messages);
        // Should only include the last image reference
        assert!(result.contains("[Image: photo2.png]"));
    }
}
