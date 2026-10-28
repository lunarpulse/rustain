//! Responses request shaping, adapted from oh-my-pi's OpenAI/Codex providers
//! (openai-codex-responses.ts; generic dialect: openai-responses.ts buildParams,
//! openai-shared.ts input conversion, vision-guard.ts).
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

use super::{Dialect, SystemRole, WireProfile, repair, schema};
use crate::domain::models::{CompletionOptions, Message, MessageRole};
use serde::Serialize;
use serde_json::{Value, json};

/// omp `NON_VISION_IMAGE_PLACEHOLDER`, sent once per message in place of its images.
pub const NON_VISION_IMAGE_PLACEHOLDER: &str = "[image omitted: model does not support vision]";
const ENCRYPTED_REASONING: [&str; 1] = ["reasoning.encrypted_content"];

/// One request body; the profile's dialect selects the field set and wire order.
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub struct ResponsesRequest<'a> {
    body: Body<'a>,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum Body<'a> {
    Codex(CodexBody<'a>),
    Generic(GenericBody<'a>),
}

/// Declaration order is wire order. Sampling, limits, reasoning and parallel
/// tool controls deliberately have no fields: the Codex backend rejects them.
#[derive(Debug, Serialize)]
struct CodexBody<'a> {
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

/// Declaration order is omp `buildParams` order for generic backends: no
/// `client_metadata`, `reasoning`, `tool_choice` or retention fields.
#[derive(Debug, Serialize)]
struct GenericBody<'a> {
    model: &'a str,
    input: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_key: Option<&'a str>,
    store: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<FunctionTool<'a>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    include: Option<[&'static str; 1]>,
}

#[derive(Debug, Serialize)]
struct FunctionTool<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    name: &'a str,
    description: &'a str,
    parameters: Value,
}

/// History converted once; each dialect consumes the parts it sends.
#[derive(Default)]
struct Converted {
    /// Codex: one developer item per history `System` message.
    developer: Vec<Value>,
    input: Vec<Value>,
    /// Codex: the last non-blank history system text.
    last_developer: Option<String>,
    /// Generic: non-blank history system texts, in order.
    system: Vec<String>,
}

impl<'a> ResponsesRequest<'a> {
    pub fn new(
        messages: &[Message],
        options: &'a CompletionOptions,
        profile: &'a dyn WireProfile,
    ) -> Self {
        let dialect = profile.dialect();
        let converted = convert(messages, options, profile, dialect);
        let body = match dialect {
            Dialect::Codex => Body::Codex(CodexBody::new(converted, options, profile)),
            Dialect::Generic => Body::Generic(GenericBody::new(converted, options, profile)),
        };
        Self { body }
    }
}

impl<'a> CodexBody<'a> {
    fn new(
        converted: Converted,
        options: &'a CompletionOptions,
        profile: &'a dyn WireProfile,
    ) -> Self {
        let Converted {
            mut developer,
            input,
            last_developer,
            ..
        } = converted;
        let mut input = repair::repair(input, Dialect::Codex);
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
            include: ENCRYPTED_REASONING,
            client_metadata: profile.client_metadata(),
        }
    }
}

impl<'a> GenericBody<'a> {
    fn new(
        converted: Converted,
        options: &'a CompletionOptions,
        profile: &'a dyn WireProfile,
    ) -> Self {
        // omp repairs orphan outputs, then orphan calls, then hoists wedged messages.
        let mut input = repair::hoist(repair::repair(converted.input, Dialect::Generic));
        let texts = std::iter::once(options.system_prompt.as_str())
            .chain(converted.system.iter().map(String::as_str))
            .filter(|text| !text.trim().is_empty());
        let mut instructions = String::new();
        match profile.system_role() {
            // The system prompt first, then history system texts: one `instructions`.
            SystemRole::Instructions => {
                for text in texts {
                    if !instructions.is_empty() {
                        instructions.push_str("\n\n");
                    }
                    instructions.push_str(text);
                }
            }
            // omp unshifts each system text as a developer item, in order.
            SystemRole::Developer => {
                let developer: Vec<Value> = texts
                    .map(|text| json!({"role":"developer","content":text}))
                    .collect();
                input.splice(0..0, developer);
            }
        }
        let reject = profile.reject_root_object_union();
        let tools = (!options.tools.is_empty()).then(|| {
            options
                .tools
                .iter()
                .filter_map(|tool| {
                    let mut parameters = schema::sanitize(tool.input_schema.clone());
                    if reject {
                        parameters = schema::flatten_exclusive_required_root_union(parameters);
                    }
                    // One provider-rejected schema would fail the whole turn: omit that tool only.
                    if let Some(path) = schema::strict_violation(&parameters, reject) {
                        tracing::warn!(
                            tool = %tool.name,
                            path = %path,
                            "tool omitted from the Responses request: its parameter schema is invalid for this provider"
                        );
                        return None;
                    }
                    Some(FunctionTool {
                        kind: "function",
                        name: &tool.name,
                        description: &tool.description,
                        parameters,
                    })
                })
                .collect()
        });
        Self {
            model: &options.model,
            input,
            instructions: (!instructions.is_empty()).then_some(instructions),
            stream: true,
            prompt_cache_key: profile.prompt_cache_key(),
            store: false,
            max_output_tokens: profile.max_output_tokens(),
            temperature: profile.temperature(),
            tools,
            include: profile.reasoning().then_some(ENCRYPTED_REASONING),
        }
    }
}

fn convert(
    messages: &[Message],
    options: &CompletionOptions,
    profile: &dyn WireProfile,
    dialect: Dialect,
) -> Converted {
    let vision = dialect == Dialect::Codex || profile.vision();
    let mut converted = Converted::default();
    for message in messages {
        let text = combined_text(message);
        if message.role == MessageRole::System {
            match dialect {
                Dialect::Codex => {
                    if !text.trim().is_empty() {
                        converted.last_developer = Some(text.clone());
                    }
                    converted.developer.push(json!({"type":"message","role":"developer","content":[{"type":"input_text","text":text}]}));
                }
                Dialect::Generic => {
                    if !text.trim().is_empty() {
                        converted.system.push(text);
                    }
                }
            }
            continue;
        }
        let input = &mut converted.input;
        // Results precede any new user text, retaining rustain's ordering.
        for result in &message.tool_results {
            let output = if result.is_error {
                format!("Error: {}", result.content)
            } else {
                result.content.clone()
            };
            input.push(
                json!({"type":"function_call_output","call_id":result.tool_use_id,"output":output}),
            );
        }
        match message.role {
            MessageRole::User => {
                let has_text = !text.trim().is_empty();
                let omitted = !vision && !message.images.is_empty();
                let images = if vision { message.images.len() } else { 0 };
                let mut content =
                    Vec::with_capacity(usize::from(has_text) + images + usize::from(omitted));
                if has_text {
                    content.push(json!({"type":"input_text","text":text}));
                }
                if vision {
                    for image in &message.images {
                        content.push(json!({"type":"input_image","detail":"auto","image_url":format!("data:{};base64,{}",image.media_type,image.data)}));
                    }
                } else if omitted {
                    content.push(json!({"type":"input_text","text":NON_VISION_IMAGE_PLACEHOLDER}));
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
                            .filter(|item| item.provider == provider && item.model == options.model)
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
    converted
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
