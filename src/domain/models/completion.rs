#![allow(dead_code)]
use serde::{Deserialize, Serialize};

use super::tools::ToolDefinition;

/// Options for a provider completion request.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompletionOptions {
    /// Stable conversation identity; independent of a UI/runtime session.
    #[serde(default)]
    pub session_id: Option<String>,
    pub model: String,
    pub max_tokens: u32,
    pub system_prompt: String,
    pub temperature: Option<f32>,
    pub tools: Vec<ToolDefinition>,
    // v0.5: pub thinking_budget: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn older_serialized_options_default_to_no_session() {
        let options: CompletionOptions = serde_json::from_value(serde_json::json!({
            "model": "model",
            "maxTokens": 16,
            "systemPrompt": "",
            "temperature": null,
            "tools": []
        }))
        .unwrap();
        assert!(options.session_id.is_none());
        let mut scoped = options;
        scoped.session_id = Some("conversation".into());
        let json = serde_json::to_value(&scoped).unwrap();
        assert_eq!(json["sessionId"], "conversation");
        let restored: CompletionOptions = serde_json::from_value(json).unwrap();
        assert_eq!(restored.session_id.as_deref(), Some("conversation"));
    }
}
