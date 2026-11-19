//! Public channel/cron reply accumulated across every response of a turn.
use std::fmt::Write;

use crate::domain::models::StreamChunk;

#[derive(Default)]
pub struct TurnReplyFold {
    text: String,
}

impl TurnReplyFold {
    pub fn push(&mut self, chunk: &StreamChunk) {
        if chunk.is_operator_private() {
            return;
        }
        match chunk {
            StreamChunk::Text { content, .. } => self.text.push_str(content),
            StreamChunk::ToolUse { id, name, .. } => {
                let _ = write!(self.text, "\n[tool use: {name} (id: {id})]\n");
            }
            _ => {}
        }
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }
    pub fn into_string(self) -> String {
        self.text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_only_public_text_and_tool_uses_in_order() {
        let mut fold = TurnReplyFold::default();
        fold.push(&StreamChunk::Text {
            content: "Checking.".into(),
            parent_tool_use_id: None,
        });
        fold.push(&StreamChunk::ToolUse {
            id: "t1".into(),
            name: "read_file".into(),
            input: serde_json::json!({}),
        });
        fold.push(&StreamChunk::ToolUse {
            id: "private".into(),
            name: crate::domain::models::team_proposal::TEAM_PROPOSAL_TOOL_NAME.into(),
            input: serde_json::json!({"secret":true}),
        });
        fold.push(&StreamChunk::Error {
            content: "secret-error".into(),
        });
        fold.push(&StreamChunk::Text {
            content: "Answer: 42".into(),
            parent_tool_use_id: None,
        });
        assert_eq!(
            fold.as_str(),
            "Checking.\n[tool use: read_file (id: t1)]\nAnswer: 42"
        );
    }
}
