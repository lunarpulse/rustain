#![allow(dead_code)]
use serde::{Deserialize, Serialize};

/// Options for a provider completion request.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompletionOptions {
    pub model: String,
    pub max_tokens: u32,
    pub system_prompt: String,
    pub temperature: Option<f32>,
    // v0.5: pub tools: Vec<ToolDefinition>,
    // v0.5: pub thinking_budget: Option<u32>,
}
