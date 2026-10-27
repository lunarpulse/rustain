//! Responses request shaping, adapted from oh-my-pi's OpenAI/Codex providers.
//!
//! MIT License
//!
//! Copyright (c) 2025 Mario Zechner
//! Copyright (c) 2025-2026 Can Bölük
//! Copyright (c) 2026 Stencil Labs, Inc.
//!
//! Permission is hereby granted, free of charge, to any person obtaining a copy
//! of this software and associated documentation files (the "Software"), to deal
//! in the Software without restriction, including without limitation the rights
//! to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
//! copies of the Software, and to permit persons to whom the Software is
//! furnished to do so, subject to the following conditions:
//!
//! The above copyright notice and this permission notice shall be included in all
//! copies or substantial portions of the Software.
//!
//! THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
//! IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
//! FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
//! AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
//! LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
//! OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
//! SOFTWARE.

use super::{WireProfile, repair, schema};
use crate::domain::models::{CompletionOptions, Message, MessageRole};
use serde::Serialize;
use serde_json::{Value, json};

/// Declaration order is wire order. Sampling, limits, reasoning and parallel
/// tool controls deliberately have no fields: the Codex backend rejects them.
#[derive(Debug, Serialize)]
pub struct ResponsesRequest<'a> {
    model: &'a str,
    input: Vec<Value>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_key: Option<&'a str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<FunctionTool<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<&'a str>,
    store: bool,
    include: [&'static str; 1],
    #[serde(skip_serializing_if = "Option::is_none")]
    client_metadata: Option<&'a Value>,
}

#[derive(Debug, Serialize)]
struct FunctionTool<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    name: &'a str,
    description: &'a str,
    parameters: Value,
}

impl<'a> ResponsesRequest<'a> {
    pub fn new(
        messages: &[Message],
        options: &'a CompletionOptions,
        profile: &'a dyn WireProfile,
    ) -> Self {
        let mut developer = Vec::new();
        let mut input = Vec::new();
        let mut last_developer = None;
        for message in messages {
            let text = combined_text(message);
            if message.role == MessageRole::System {
                if !text.trim().is_empty() {
                    last_developer = Some(text.clone());
                }
                developer.push(json!({"type":"message","role":"developer","content":[{"type":"input_text","text":text}]}));
                continue;
            }
            // Results precede any new user text, retaining rustain's ordering.
            for result in &message.tool_results {
                let output = if result.is_error {
                    format!("Error: {}", result.content)
                } else {
                    result.content.clone()
                };
                input.push(json!({"type":"function_call_output","call_id":result.tool_use_id,"output":output}));
            }
            match message.role {
                MessageRole::User => {
                    let mut content = Vec::with_capacity(
                        usize::from(!text.trim().is_empty()) + message.images.len(),
                    );
                    if !text.trim().is_empty() {
                        content.push(json!({"type":"input_text","text":text}));
                    }
                    for image in &message.images {
                        content.push(json!({"type":"input_image","detail":"auto","image_url":format!("data:{};base64,{}",image.media_type,image.data)}));
                    }
                    if !content.is_empty() {
                        input.push(json!({"role":"user","content":content}));
                    }
                }
                MessageRole::Assistant => {
                    // omp replays native items only to the producing provider
                    // and model, ahead of that turn's output and calls.
                    if let Some(provider) = profile.replay_provider() {
                        input.extend(
                            message
                                .provider_replay
                                .iter()
                                .filter(|item| {
                                    item.provider == provider && item.model == options.model
                                })
                                .map(|item| item.payload.as_ref().clone()),
                        );
                    }
                    if !text.is_empty() {
                        input.push(json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":text,"annotations":[]}]}));
                    }
                    for call in &message.tool_uses {
                        input.push(json!({"type":"function_call","call_id":call.id,"name":call.name,"arguments":call.input.to_string()}));
                    }
                }
                MessageRole::System => unreachable!("system messages handled above"),
            }
        }
        let mut input = repair::repair(input);
        developer.append(&mut input);
        let instructions =
            (!options.system_prompt.trim().is_empty()).then_some(options.system_prompt.as_str());
        if developer.iter().all(|item| item["role"] == "developer") {
            if let Some(text) = last_developer.as_deref().or(instructions) {
                developer.push(json!({"type":"message","role":"user","content":[{"type":"input_text","text":text}]}));
            }
        }
        Self {
            model: &options.model,
            input: developer,
            stream: true,
            prompt_cache_key: profile.prompt_cache_key(),
            tools: options
                .tools
                .iter()
                .map(|tool| FunctionTool {
                    kind: "function",
                    name: &tool.name,
                    description: &tool.description,
                    parameters: schema::sanitize(tool.input_schema.clone()),
                })
                .collect(),
            instructions,
            store: false,
            include: ["reasoning.encrypted_content"],
            client_metadata: profile.client_metadata(),
        }
    }
}

fn combined_text(message: &Message) -> String {
    match &message.context_prefix {
        Some(prefix) if !prefix.is_empty() => {
            let mut text = String::with_capacity(prefix.len() + 2 + message.content.len());
            text.push_str(prefix);
            text.push_str("\n\n");
            text.push_str(&message.content);
            text
        }
        _ => message.content.clone(),
    }
}
