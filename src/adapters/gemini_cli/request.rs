//! Cloud Code Assist request body, adapted from oh-my-pi (ai/src/providers/google-gemini-cli.ts,
//! ai/src/providers/google-shared.ts, ai/src/providers/transform-messages.ts, ai/src/stream.ts,
//! ai/src/dialect/demotion.ts, ai/src/dialect/gemini.ts, catalog/src/model-thinking.ts).
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

use super::profile::{Effort, EffortRoutes, GeminiModelProfile, Thinking};
use crate::adapters::responses::request::NON_VISION_IMAGE_PLACEHOLDER;
use crate::domain::models::{CompletionOptions, ImageAttachment, Message, MessageRole};
use serde::ser::SerializeMap;
use serde::{Serialize, Serializer};
use serde_json::Value;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque};

/// omp `SKIP_THOUGHT_SIGNATURE` (G:143).
const SKIP_THOUGHT_SIGNATURE: &str = "skip_thought_signature_validator";
/// Synthetic result for a call that never got one (transform-messages.ts:1341).
const NO_RESULT: &str = "No result provided";
/// `tool-call-names.kdl` google-gemini-cli bound.
const MAX_TOOL_CALL_NAME_LENGTH: usize = 64;
/// omp `MIN_OUTPUT_TOKENS` (stream.ts:1526).
const MIN_OUTPUT_TOKENS: u32 = 1024;
static NO_ARGS: Value = Value::Null;
/// omp's image user-turn lead text for a non-multimodal model (G:384).
const TOOL_RESULT_IMAGE: &str = "Tool result image:";
/// omp's value of a result that has images but no text (G:354).
const SEE_ATTACHED_IMAGE: &str = "(see attached image)";

/// The POST body, encoded once per logical request.
pub(crate) fn encode(
    messages: &[Message],
    options: &CompletionOptions,
    profile: &GeminiModelProfile,
    project: &str,
) -> Result<Vec<u8>, serde_json::Error> {
    let (generation_config, route) = generation(options, profile);
    let system = options.system_prompt.as_str();
    let envelope = Envelope {
        project,
        model: route.unwrap_or(options.model.as_str()),
        request: Request {
            contents: contents(messages, options, profile),
            system_instruction: (!system.trim().is_empty()).then_some(SystemInstruction {
                parts: [SystemPart { text: system }],
            }),
            tools: (!options.tools.is_empty()).then(|| {
                [ToolGroup {
                    function_declarations: options
                        .tools
                        .iter()
                        .map(|tool| FunctionDeclaration {
                            name: &tool.name,
                            description: &tool.description,
                            parameters_json_schema: super::schema::normalize_for_google(
                                &tool.input_schema,
                            ),
                        })
                        .collect(),
                }]
            }),
            generation_config,
        },
    };
    serde_json::to_vec(&envelope)
}

/// omp isValidThoughtSignature (G:140-146).
pub(crate) fn valid_signature(signature: &str) -> bool {
    if signature.is_empty() || !signature.len().is_multiple_of(4) {
        return false;
    }
    let body = signature.trim_end_matches('=');
    signature.len() - body.len() <= 2
        && !body.is_empty()
        && body
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
}

// Wire shapes; field order is omp's key order (no `preserve_order` here).

#[derive(Serialize)]
struct Envelope<'a> {
    project: &'a str,
    model: &'a str,
    request: Request<'a>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Request<'a> {
    contents: Vec<Content<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system_instruction: Option<SystemInstruction<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<[ToolGroup<'a>; 1]>,
    /// Never empty: rustain always carries `max_tokens`.
    generation_config: GenerationConfig,
}

#[derive(Serialize)]
struct SystemInstruction<'a> {
    parts: [SystemPart<'a>; 1],
}

#[derive(Serialize)]
struct SystemPart<'a> {
    text: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ToolGroup<'a> {
    function_declarations: Vec<FunctionDeclaration<'a>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FunctionDeclaration<'a> {
    name: &'a str,
    description: &'a str,
    parameters_json_schema: Value,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GenerationConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    max_output_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking_config: Option<ThinkingConfig>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ThinkingConfig {
    include_thoughts: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking_level: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking_budget: Option<u32>,
}

#[derive(Serialize)]
struct Content<'a> {
    role: &'static str,
    parts: Vec<Part<'a>>,
}

/// One Gemini `Part`; exactly one payload field is set per part.
#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct Part<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    thought: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<Cow<'a, str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    inline_data: Option<InlineData<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    function_call: Option<FunctionCall<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    function_response: Option<FunctionResponse<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thought_signature: Option<&'a str>,
    /// Call ordinal of a function response (omp `FunctionCallOrder`).
    #[serde(skip)]
    order: Option<usize>,
}

impl<'a> Part<'a> {
    fn text(text: Cow<'a, str>) -> Self {
        Part {
            text: Some(text),
            ..Part::default()
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InlineData<'a> {
    mime_type: &'a str,
    data: &'a str,
}

#[derive(Serialize)]
struct FunctionCall<'a> {
    name: &'a str,
    #[serde(serialize_with = "args_or_empty")]
    args: &'a Value,
}

/// omp `block.arguments ?? {}`.
fn args_or_empty<S: Serializer>(args: &&Value, serializer: S) -> Result<S::Ok, S::Error> {
    match args {
        Value::Null => serializer.serialize_map(Some(0))?.end(),
        other => other.serialize(serializer),
    }
}

#[derive(Serialize)]
struct FunctionResponse<'a> {
    name: &'a str,
    response: ResponseValue<'a>,
    /// Result images nested for a multimodal-function-response model (G:368).
    #[serde(skip_serializing_if = "Option::is_none")]
    parts: Option<Vec<Part<'a>>>,
}

#[derive(Serialize)]
struct ResponseValue<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<Cow<'a, str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<Cow<'a, str>>,
}

// Default thinking: omp `streamSimple` without an effort (stream.ts:2113-2172).

/// The generation config and, when the profile routes it, the wire model id.
fn generation(
    options: &CompletionOptions,
    profile: &GeminiModelProfile,
) -> (GenerationConfig, Option<&'static str>) {
    let base = options.max_tokens;
    let routes = profile.routes;
    let route = |effort| routes.and_then(|routes| routes.route(effort));
    let config = |max_output_tokens, thinking_config| GenerationConfig {
        temperature: options.temperature,
        max_output_tokens,
        thinking_config,
    };
    if let Some(effort) = mandatory_effort(profile) {
        match profile.thinking {
            Thinking::Level { .. } => {
                let thinking = ThinkingConfig {
                    include_thoughts: true,
                    thinking_level: Some(thinking_level(effort, routes)),
                    thinking_budget: None,
                };
                return (
                    config(base.min(profile.max_output), Some(thinking)),
                    route(Some(effort)),
                );
            }
            Thinking::Budget { .. } => {
                let mut budget = google_budget(effort);
                let max_tokens = base.saturating_add(budget).min(profile.max_output);
                if max_tokens <= budget {
                    budget = max_tokens.saturating_sub(MIN_OUTPUT_TOKENS);
                }
                if budget > 0 {
                    let thinking = ThinkingConfig {
                        include_thoughts: true,
                        thinking_level: None,
                        thinking_budget: Some(budget),
                    };
                    return (config(max_tokens, Some(thinking)), route(Some(effort)));
                }
                // A zero budget falls through to thinking off.
            }
            Thinking::None => {}
        }
    }
    let suppress =
        (profile.reasoning && profile.suppress_when_off).then_some(match profile.thinking {
            Thinking::Level { .. } => ThinkingConfig {
                include_thoughts: false,
                thinking_level: Some("MINIMAL"),
                thinking_budget: None,
            },
            Thinking::Budget { .. } | Thinking::None => ThinkingConfig {
                include_thoughts: false,
                thinking_level: None,
                thinking_budget: Some(0),
            },
        });
    (config(base.min(profile.max_output), suppress), route(None))
}

/// omp `normalizeMandatoryReasoningOptions` + `minimumSupportedEffort`. The
/// routed-default branch of `defaultSupportedEffort` needs a `requestModelId`,
/// which no Gemini CLI row has.
fn mandatory_effort(profile: &GeminiModelProfile) -> Option<Effort> {
    if !profile.reasoning || !profile.thinking.requires_effort() || profile.suppress_when_off {
        return None;
    }
    let efforts = profile.thinking.efforts();
    Effort::CANONICAL
        .into_iter()
        .find(|effort| efforts.contains(effort))
        .or_else(|| efforts.first().copied())
}

/// omp `mapEffortToGoogleThinkingLevel` (model-thinking.ts:97-117).
fn thinking_level(effort: Effort, routes: Option<EffortRoutes>) -> &'static str {
    match effort {
        Effort::Minimal => match routes {
            Some(routes) if routes.minimal.is_some() && routes.minimal == routes.low => "LOW",
            _ => "MINIMAL",
        },
        Effort::Low => "LOW",
        Effort::Medium => "MEDIUM",
        Effort::High => "HIGH",
    }
}

/// omp `GOOGLE_THINKING` (stream.ts:1549-1556).
fn google_budget(effort: Effort) -> u32 {
    match effort {
        Effort::Minimal => 1024,
        Effort::Low => 4096,
        Effort::Medium => 8192,
        Effort::High => 16384,
    }
}

// History: rustain messages → omp message sequence → pair repair → contents.

/// A call occurrence: its raw id plus the duplicate index omp's `_dupN` rewrite assigns.
type Key<'a> = (&'a str, usize);

#[derive(Clone, Copy)]
struct Call<'a> {
    /// Index in `Message::tool_uses`.
    slot: usize,
    id: &'a str,
    name: &'a str,
    key: Key<'a>,
}

/// omp's `user`/`developer`, `assistant` and `toolResult` messages.
enum Entry<'a> {
    User {
        text: Cow<'a, str>,
        images: &'a [ImageAttachment],
    },
    Assistant {
        message: &'a Message,
        calls: Vec<Call<'a>>,
    },
    Result {
        key: Key<'a>,
        /// Name of the paired call; set by the pairing pass.
        name: &'a str,
        content: &'a str,
        is_error: bool,
        images: &'a [ImageAttachment],
    },
}

fn contents<'a>(
    messages: &'a [Message],
    options: &CompletionOptions,
    profile: &GeminiModelProfile,
) -> Vec<Content<'a>> {
    let entries = pair(sanitize_malformed(flatten(messages)));
    let mut order = CallOrder::default();
    let mut contents: Vec<Content<'a>> = Vec::with_capacity(entries.len());
    // G:215-225: image turns of a non-multimodal model wait until the merged
    // functionResponse turn is complete, each result's images in call order.
    let mut pending_images: Vec<(Option<usize>, Vec<Part<'a>>)> = Vec::new();
    for entry in entries {
        if !matches!(entry, Entry::Result { .. }) {
            flush_tool_images(&mut pending_images, &mut contents);
        }
        match entry {
            Entry::User { text, images } => {
                let parts = user_parts(text, images, profile.vision);
                if !parts.is_empty() {
                    contents.push(Content {
                        role: "user",
                        parts,
                    });
                }
            }
            Entry::Assistant { message, calls } => {
                let parts = model_parts(message, &calls, options, profile, &mut order);
                if !parts.is_empty() {
                    contents.push(Content {
                        role: "model",
                        parts,
                    });
                }
            }
            Entry::Result {
                key,
                name,
                content,
                is_error,
                images,
            } => {
                let ordinal = order.ordinals.get(&key).copied();
                let (value, attached) = result_value(content, images, profile.vision);
                let nested = profile.multimodal_function_response && !attached.is_empty();
                let response = if is_error {
                    ResponseValue {
                        output: None,
                        error: Some(value),
                    }
                } else {
                    ResponseValue {
                        output: Some(value),
                        error: None,
                    }
                };
                let part = Part {
                    function_response: Some(FunctionResponse {
                        name,
                        response,
                        parts: nested.then(|| image_parts(attached).collect()),
                    }),
                    order: ordinal,
                    ..Part::default()
                };
                // Cloud Code Assist wants every function response in one user turn.
                match contents.last_mut() {
                    Some(last)
                        if last.role == "user"
                            && last.parts.iter().any(|p| p.function_response.is_some()) =>
                    {
                        last.parts.push(part);
                    }
                    _ => contents.push(Content {
                        role: "user",
                        parts: vec![part],
                    }),
                }
                if !nested && !attached.is_empty() {
                    let mut parts = Vec::with_capacity(1 + attached.len());
                    parts.push(Part::text(Cow::Borrowed(TOOL_RESULT_IMAGE)));
                    parts.extend(image_parts(attached));
                    pending_images.push((ordinal, parts));
                }
            }
        }
    }
    flush_tool_images(&mut pending_images, &mut contents);
    for content in &mut contents {
        sort_known_in_place(&mut content.parts);
    }
    contents
}

/// G:338-360: the result value and the images the target receives. rustain's
/// `content` keeps the projector's `[image: <mime>]` markers (a recorded
/// deviation), so `(see attached image)` applies only to an empty result.
fn result_value<'a>(
    content: &'a str,
    images: &'a [ImageAttachment],
    vision: bool,
) -> (Cow<'a, str>, &'a [ImageAttachment]) {
    if images.is_empty() {
        return (Cow::Borrowed(content), images);
    }
    if !vision {
        let value = if content.is_empty() {
            Cow::Borrowed(NON_VISION_IMAGE_PLACEHOLDER)
        } else {
            Cow::Owned(format!("{content}\n{NON_VISION_IMAGE_PLACEHOLDER}"))
        };
        return (value, &[]);
    }
    let value = if content.is_empty() {
        Cow::Borrowed(SEE_ATTACHED_IMAGE)
    } else {
        Cow::Borrowed(content)
    };
    (value, images)
}

/// omp `convertGoogleImagePart` for inline bytes.
fn image_parts<'a>(images: &'a [ImageAttachment]) -> impl Iterator<Item = Part<'a>> {
    images.iter().map(|image| Part {
        inline_data: Some(InlineData {
            mime_type: &image.media_type,
            data: &image.data,
        }),
        ..Part::default()
    })
}

/// omp `flushPendingToolImages` (G:220-225): one user turn, groups in call order.
fn flush_tool_images<'a>(
    pending: &mut Vec<(Option<usize>, Vec<Part<'a>>)>,
    contents: &mut Vec<Content<'a>>,
) {
    if pending.is_empty() {
        return;
    }
    sort_known(pending, |group| group.0);
    contents.push(Content {
        role: "user",
        parts: pending.drain(..).flat_map(|(_, parts)| parts).collect(),
    });
}

/// rustain shape → omp sequence: a message's results, then its own body.
fn flatten(messages: &[Message]) -> Vec<Entry<'_>> {
    let mut entries = Vec::with_capacity(messages.len());
    for message in messages {
        entries.extend(message.tool_results.iter().map(|result| Entry::Result {
            key: (result.tool_use_id.as_str(), 0),
            name: "",
            content: result.content.as_str(),
            is_error: result.is_error,
            images: &result.images,
        }));
        match message.role {
            MessageRole::User | MessageRole::System => {
                let text = combined_text(message);
                if !text.is_empty() || !message.images.is_empty() {
                    entries.push(Entry::User {
                        text,
                        images: &message.images,
                    });
                }
            }
            MessageRole::Assistant => entries.push(Entry::Assistant {
                message,
                calls: message
                    .tool_uses
                    .iter()
                    .enumerate()
                    .map(|(slot, call)| Call {
                        slot,
                        id: &call.id,
                        name: &call.name,
                        key: (call.id.as_str(), 0),
                    })
                    .collect(),
            }),
        }
    }
    entries
}

fn combined_text(message: &Message) -> Cow<'_, str> {
    match &message.context_prefix {
        Some(prefix) if !prefix.is_empty() => {
            let mut text = String::with_capacity(prefix.len() + 2 + message.content.len());
            text.push_str(prefix);
            text.push_str("\n\n");
            text.push_str(&message.content);
            Cow::Owned(text)
        }
        _ => Cow::Borrowed(message.content.as_str()),
    }
}

/// omp `isMalformedToolCall` with the google charset and the Gemini CLI bound.
fn is_malformed(id: &str, name: &str) -> bool {
    if id.trim().is_empty() || name.len() > MAX_TOOL_CALL_NAME_LENGTH {
        return true;
    }
    let mut bytes = name.bytes();
    let Some(first) = bytes.next() else {
        return true;
    };
    !(first.is_ascii_alphabetic() || first == b'_')
        || !bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':' | b'-'))
}

/// omp `sanitizeMalformedToolCalls` (transform-messages.ts:418-477).
fn sanitize_malformed(entries: Vec<Entry<'_>>) -> Vec<Entry<'_>> {
    let any_malformed = entries.iter().any(|entry| match entry {
        Entry::Assistant { calls, .. } => calls.iter().any(|call| is_malformed(call.id, call.name)),
        _ => false,
    });
    if !any_malformed {
        return entries;
    }
    let mut queues: HashMap<&str, VecDeque<bool>> = HashMap::new();
    let mut kept = Vec::with_capacity(entries.len());
    for entry in entries {
        let entry = match entry {
            Entry::Assistant { message, mut calls } => {
                queues.clear();
                calls.retain(|call| {
                    let malformed = is_malformed(call.id, call.name);
                    queues.entry(call.id).or_default().push_back(malformed);
                    !malformed
                });
                let has_blocks = !calls.is_empty()
                    || !message.content.is_empty()
                    || message
                        .reasoning_content
                        .as_deref()
                        .is_some_and(|r| !r.is_empty());
                if !has_blocks {
                    continue;
                }
                Entry::Assistant { message, calls }
            }
            Entry::Result {
                key,
                name,
                content,
                is_error,
                images,
            } => {
                let (malformed, drained) = match queues.get_mut(key.0) {
                    Some(queue) => (queue.pop_front(), queue.is_empty()),
                    None => (None, false),
                };
                if drained {
                    queues.remove(key.0);
                }
                if malformed == Some(true) {
                    continue;
                }
                Entry::Result {
                    key,
                    name,
                    content,
                    is_error,
                    images,
                }
            }
            Entry::User { text, images } => {
                queues.clear();
                Entry::User { text, images }
            }
        };
        kept.push(entry);
    }
    kept
}

struct IndexedResult<'a> {
    index: usize,
    content: &'a str,
    is_error: bool,
    images: &'a [ImageAttachment],
    consumed: bool,
}

/// omp `deduplicateToolCallIds` plus the second pass of `transformMessages`
/// (transform-messages.ts:225-347,1312-1517): every surviving call is followed
/// by exactly one result before the next non-result message.
fn pair(mut entries: Vec<Entry<'_>>) -> Vec<Entry<'_>> {
    let mut seen: HashMap<&str, usize> = HashMap::new();
    let mut rewrites: HashMap<&str, VecDeque<usize>> = HashMap::new();
    let mut valid: HashSet<Key<'_>> = HashSet::new();
    let mut results: HashMap<Key<'_>, Vec<IndexedResult<'_>>> = HashMap::new();
    for (index, entry) in entries.iter_mut().enumerate() {
        match entry {
            Entry::Result {
                key,
                content,
                is_error,
                images,
                ..
            } => {
                let id = key.0;
                let (duplicate, drained) = match rewrites.get_mut(id) {
                    Some(queue) => (queue.pop_front(), queue.is_empty()),
                    None => (None, false),
                };
                if drained {
                    rewrites.remove(id);
                }
                *key = (id, duplicate.unwrap_or(0));
                results.entry(*key).or_default().push(IndexedResult {
                    index,
                    content,
                    is_error: *is_error,
                    images,
                    consumed: false,
                });
            }
            Entry::Assistant { calls, .. } => {
                let mut touched: HashSet<&str> = HashSet::new();
                for call in calls.iter_mut() {
                    if touched.insert(call.id) {
                        rewrites.remove(call.id);
                    }
                    let count = seen.get(call.id).copied().unwrap_or(0);
                    seen.insert(call.id, count + 1);
                    call.key = (call.id, count);
                    rewrites.entry(call.id).or_default().push_back(count);
                    valid.insert(call.key);
                }
            }
            Entry::User { .. } => {}
        }
    }

    let mut out = Vec::with_capacity(entries.len());
    let mut pending: Vec<(Key<'_>, &str)> = Vec::new();
    let mut pending_start = 0;
    let mut status: HashSet<Key<'_>> = HashSet::new();
    for (index, entry) in entries.into_iter().enumerate() {
        match entry {
            Entry::Assistant { message, calls } => {
                flush(
                    &mut pending,
                    pending_start,
                    &mut status,
                    &mut results,
                    &mut out,
                );
                if !calls.is_empty() {
                    pending.extend(calls.iter().map(|call| (call.key, call.name)));
                    pending_start = index;
                }
                out.push(Entry::Assistant { message, calls });
            }
            Entry::Result {
                key,
                content,
                is_error,
                images,
                ..
            } => {
                if status.contains(&key) {
                    continue;
                }
                if let Some(&(_, name)) = pending.iter().find(|(open, _)| *open == key) {
                    status.insert(key);
                    out.push(Entry::Result {
                        key,
                        name,
                        content,
                        is_error,
                        images,
                    });
                    continue;
                }
                if valid.contains(&key) {
                    // Its call is elsewhere; the pending flush pulls it into place.
                    continue;
                }
                if pending.iter().any(|(open, _)| !status.contains(open)) {
                    continue;
                }
                if !content.trim().is_empty() {
                    let error = if is_error { " is-error=\"true\"" } else { "" };
                    out.push(Entry::User {
                        text: Cow::Owned(format!(
                            "<stale-tool-result tool=\"tool\" id=\"{}\"{error}>\n{content}\n</stale-tool-result>",
                            key.0
                        )),
                        images: &[],
                    });
                }
            }
            Entry::User { text, images } => {
                flush(
                    &mut pending,
                    pending_start,
                    &mut status,
                    &mut results,
                    &mut out,
                );
                out.push(Entry::User { text, images });
            }
        }
    }
    flush(
        &mut pending,
        pending_start,
        &mut status,
        &mut results,
        &mut out,
    );
    out
}

/// omp `flushPendingToolCalls`: the earliest unconsumed real result after the
/// call's turn, else the synthetic error result.
fn flush<'a>(
    pending: &mut Vec<(Key<'a>, &'a str)>,
    start: usize,
    status: &mut HashSet<Key<'a>>,
    results: &mut HashMap<Key<'a>, Vec<IndexedResult<'a>>>,
    out: &mut Vec<Entry<'a>>,
) {
    for (key, name) in pending.drain(..) {
        if !status.insert(key) {
            continue;
        }
        let real = results.get_mut(&key).and_then(|entries| {
            entries
                .iter_mut()
                .find(|entry| !entry.consumed && entry.index > start)
                .map(|entry| {
                    entry.consumed = true;
                    (entry.content, entry.is_error, entry.images)
                })
        });
        let (content, is_error, images) = real.unwrap_or((NO_RESULT, true, &[]));
        out.push(Entry::Result {
            key,
            name,
            content,
            is_error,
            images,
        });
    }
}

/// omp `FunctionCallOrder` call side.
#[derive(Default)]
struct CallOrder<'a> {
    ordinals: HashMap<Key<'a>, usize>,
    next: usize,
}

impl<'a> CallOrder<'a> {
    fn call(&mut self, key: Key<'a>) {
        self.ordinals.insert(key, self.next);
        self.next += 1;
    }
}

/// omp `sortKnownInPlace`: ordered parts take the slots ordered parts occupy.
fn sort_known_in_place(parts: &mut [Part<'_>]) {
    sort_known(parts, |part| part.order);
}

/// omp `sortKnownInPlace` over any items: those with an order take, sorted,
/// the slots such items occupy; the rest stay put.
fn sort_known<T: Default>(items: &mut [T], order: impl Fn(&T) -> Option<usize>) {
    let slots: Vec<usize> = items
        .iter()
        .enumerate()
        .filter_map(|(slot, item)| order(item).map(|_| slot))
        .collect();
    if slots.len() < 2 {
        return;
    }
    let mut known: Vec<T> = slots
        .iter()
        .map(|&slot| std::mem::take(&mut items[slot]))
        .collect();
    known.sort_by_key(|item| order(item));
    for (slot, item) in slots.into_iter().zip(known) {
        items[slot] = item;
    }
}

fn user_parts<'a>(
    text: Cow<'a, str>,
    images: &'a [ImageAttachment],
    vision: bool,
) -> Vec<Part<'a>> {
    let mut parts = Vec::with_capacity(1 + images.len());
    if !text.trim().is_empty() {
        parts.push(Part::text(text));
    }
    if vision {
        parts.extend(images.iter().map(|image| Part {
            inline_data: Some(InlineData {
                mime_type: &image.media_type,
                data: &image.data,
            }),
            ..Part::default()
        }));
    } else if !images.is_empty() {
        parts.push(Part::text(Cow::Borrowed(NON_VISION_IMAGE_PLACEHOLDER)));
    }
    parts
}

/// One part of a `{"gemini_parts": [...]}` replay payload.
enum ReplayPart<'a> {
    Thinking {
        text: &'a str,
        signature: Option<&'a str>,
    },
    Text {
        text: &'a str,
        signature: Option<&'a str>,
    },
    Call {
        name: &'a str,
        args: &'a Value,
        signature: Option<&'a str>,
        call_id: &'a str,
    },
}

fn parse_replay(payload: &Value) -> Option<Vec<ReplayPart<'_>>> {
    payload
        .get(super::REPLAY_KEY)?
        .as_array()?
        .iter()
        .map(|part| {
            let signature = part.get("thoughtSignature").and_then(Value::as_str);
            if let Some(call) = part.get("functionCall") {
                return Some(ReplayPart::Call {
                    name: call.get("name")?.as_str()?,
                    args: call.get("args").unwrap_or(&NO_ARGS),
                    signature,
                    call_id: part.get("callId")?.as_str()?,
                });
            }
            let text = part.get("text")?.as_str()?;
            Some(
                if part.get("thought").and_then(Value::as_bool) == Some(true) {
                    ReplayPart::Thinking { text, signature }
                } else {
                    ReplayPart::Text { text, signature }
                },
            )
        })
        .collect()
}

/// The consistency guard: the item's calls and text are exactly this message's.
/// An unedited message carries one of two forms of the text: the visible text
/// alone (turn loop, `ask`, daemon), or the TUI history mirror's fold, where
/// each call-free run of parts contributes its visible text and then its
/// thinking (`reduce` flushes prose before reasoning, and
/// `Conversation::rebuild_messages_mirror` joins the parts). Any other text is
/// an edit, which refuses the item.
fn consistent(parts: &[ReplayPart<'_>], message: &Message) -> bool {
    let mut uses = message.tool_uses.iter();
    let mut visible = String::with_capacity(message.content.len());
    let mut folded = String::with_capacity(message.content.len());
    let mut run = 0;
    for (index, part) in parts.iter().enumerate() {
        match part {
            ReplayPart::Call {
                name,
                args,
                call_id,
                ..
            } => {
                match uses.next() {
                    Some(call)
                        if call.id == *call_id && call.name == *name && call.input == **args => {}
                    _ => return false,
                }
                fold_thinking(&parts[run..index], &mut folded);
                run = index + 1;
            }
            ReplayPart::Text { text, .. } => {
                visible.push_str(text);
                folded.push_str(text);
            }
            ReplayPart::Thinking { .. } => {}
        }
    }
    fold_thinking(&parts[run..], &mut folded);
    uses.next().is_none() && (visible == message.content || folded == message.content)
}

/// Appends the thinking text of one call-free run, as the TUI mirror does.
fn fold_thinking(run: &[ReplayPart<'_>], folded: &mut String) {
    for part in run {
        if let ReplayPart::Thinking { text, .. } = part {
            folded.push_str(text);
        }
    }
}

enum Replay<'a> {
    /// Same provider and model: native parts with valid signatures.
    Same(Vec<ReplayPart<'a>>),
    /// Another Gemini model: unsigned, thinking demoted.
    OtherModel(Vec<ReplayPart<'a>>),
}

fn select_replay<'a>(message: &'a Message, model: &str) -> Option<Replay<'a>> {
    let mut other = None;
    for item in &message.provider_replay {
        if item.provider != super::PROVIDER_KIND {
            continue;
        }
        let Some(parts) = parse_replay(item.payload.as_ref()) else {
            continue;
        };
        if !consistent(&parts, message) {
            continue;
        }
        if item.model == model {
            return Some(Replay::Same(parts));
        }
        if other.is_none() {
            other = Some(Replay::OtherModel(parts));
        }
    }
    other
}

/// omp `renderDemotedThinking` for a Gemini target (`gemini.ts:533-536`).
fn demoted_thinking(text: &str) -> Cow<'static, str> {
    Cow::Owned(format!("```thinking\n{text}\n```"))
}

/// Assistant parts in block order (G:261-329).
struct ModelTurn<'o, 'a> {
    parts: Vec<Part<'a>>,
    first_call: bool,
    sentinel: bool,
    order: &'o mut CallOrder<'a>,
}

impl<'a> ModelTurn<'_, 'a> {
    fn text(&mut self, text: Cow<'a, str>, signature: Option<&'a str>) {
        if text.trim().is_empty() {
            return;
        }
        self.parts.push(Part {
            text: Some(text),
            thought_signature: signature,
            ..Part::default()
        });
    }

    fn call(&mut self, call: &Call<'a>, args: &'a Value, signature: Option<&'a str>) {
        self.order.call(call.key);
        let fallback = (self.first_call && self.sentinel).then_some(SKIP_THOUGHT_SIGNATURE);
        self.first_call = false;
        self.parts.push(Part {
            function_call: Some(FunctionCall {
                name: call.name,
                args,
            }),
            thought_signature: signature.or(fallback),
            ..Part::default()
        });
    }
}

fn model_parts<'a>(
    message: &'a Message,
    calls: &[Call<'a>],
    options: &CompletionOptions,
    profile: &GeminiModelProfile,
    order: &mut CallOrder<'a>,
) -> Vec<Part<'a>> {
    let mut turn = ModelTurn {
        parts: Vec::new(),
        first_call: true,
        sentinel: profile.first_call_sentinel,
        order,
    };
    let Some(replay) = select_replay(message, &options.model) else {
        if let Some(reasoning) = message.reasoning_content.as_deref()
            && !reasoning.trim().is_empty()
        {
            turn.text(demoted_thinking(reasoning), None);
        }
        turn.text(Cow::Borrowed(message.content.as_str()), None);
        for call in calls {
            let args = &message.tool_uses[call.slot].input;
            turn.call(call, args, None);
        }
        return turn.parts;
    };
    let (parts, same) = match replay {
        Replay::Same(parts) => (parts, true),
        Replay::OtherModel(parts) => (parts, false),
    };
    let signed = |signature: Option<&'a str>| signature.filter(|s| same && valid_signature(s));
    // The guard aligned call parts with `tool_uses`; dropped calls have no slot in `calls`.
    let mut slot = 0;
    let mut surviving = calls.iter().peekable();
    for part in parts {
        match part {
            ReplayPart::Thinking { text, signature } => {
                if !same {
                    if !text.trim().is_empty() {
                        turn.text(demoted_thinking(text), None);
                    }
                    continue;
                }
                let signature = signed(signature);
                if text.trim().is_empty() && signature.is_none() {
                    continue;
                }
                turn.parts.push(Part {
                    thought: Some(true),
                    text: Some(Cow::Borrowed(text)),
                    thought_signature: signature,
                    ..Part::default()
                });
            }
            ReplayPart::Text { text, signature } => {
                turn.text(Cow::Borrowed(text), signed(signature));
            }
            ReplayPart::Call {
                args, signature, ..
            } => {
                if let Some(call) = surviving.next_if(|call| call.slot == slot) {
                    turn.call(call, args, signed(signature));
                }
                slot += 1;
            }
        }
    }
    turn.parts
}

#[cfg(test)]
mod tests {
    use super::super::profile::for_model;
    use super::*;
    use crate::domain::models::{
        ProviderReplayItem, ToolDefinition, ToolResultMessage, ToolUseMessage,
    };
    use serde_json::json;
    use std::sync::Arc;

    fn options(model: &str) -> CompletionOptions {
        CompletionOptions {
            session_id: None,
            model: model.into(),
            max_tokens: 8192,
            system_prompt: String::new(),
            temperature: None,
            tools: Vec::new(),
        }
    }

    fn message(role: MessageRole, content: &str) -> Message {
        Message {
            role,
            content: content.into(),
            images: Vec::new(),
            tool_results: Vec::new(),
            tool_uses: Vec::new(),
            context_prefix: None,
            reasoning_content: None,
            provider_replay: Vec::new(),
        }
    }

    fn user(content: &str) -> Message {
        message(MessageRole::User, content)
    }

    fn assistant(content: &str, calls: &[(&str, &str, Value)]) -> Message {
        let mut message = message(MessageRole::Assistant, content);
        message.tool_uses = calls
            .iter()
            .map(|(id, name, input)| ToolUseMessage {
                id: (*id).into(),
                name: (*name).into(),
                input: input.clone(),
            })
            .collect();
        message
    }

    fn results(results: &[(&str, &str, bool)]) -> Message {
        let mut message = user("");
        message.tool_results = results
            .iter()
            .map(|(id, content, is_error)| ToolResultMessage {
                tool_use_id: (*id).into(),
                content: (*content).into(),
                is_error: *is_error,
                images: Vec::new(),
            })
            .collect();
        message
    }

    fn wire(messages: &[Message], model: &str) -> String {
        let options = options(model);
        let profile = for_model(model);
        serde_json::to_string(&contents(messages, &options, &profile)).expect("contents encode")
    }

    fn generation_json(options: &CompletionOptions, profile: &GeminiModelProfile) -> String {
        serde_json::to_string(&generation(options, profile).0).expect("config encode")
    }

    const PRO31: &str = "gemini-3.1-pro-preview";

    #[test]
    fn default_generation_config_per_bundled_row() {
        let table = [
            ("gemini-2.0-flash", r#"{"maxOutputTokens":8192}"#),
            ("gemini-2.5-flash", r#"{"maxOutputTokens":8192}"#),
            (
                "gemini-2.5-pro",
                r#"{"maxOutputTokens":9216,"thinkingConfig":{"includeThoughts":true,"thinkingBudget":1024}}"#,
            ),
            (
                "gemini-3-flash-preview",
                r#"{"maxOutputTokens":8192,"thinkingConfig":{"includeThoughts":true,"thinkingLevel":"MINIMAL"}}"#,
            ),
            (
                "gemini-3-pro-preview",
                r#"{"maxOutputTokens":8192,"thinkingConfig":{"includeThoughts":true,"thinkingLevel":"LOW"}}"#,
            ),
            (
                "gemini-3.1-flash-lite-preview",
                r#"{"maxOutputTokens":8192,"thinkingConfig":{"includeThoughts":true,"thinkingLevel":"MINIMAL"}}"#,
            ),
            (
                "gemini-3.1-pro-preview",
                r#"{"maxOutputTokens":8192,"thinkingConfig":{"includeThoughts":true,"thinkingLevel":"LOW"}}"#,
            ),
        ];
        for (id, expected) in table {
            let options = options(id);
            let profile = for_model(id);
            assert_eq!(generation_json(&options, &profile), expected, "{id}");
            let (_, route) = generation(&options, &profile);
            assert_eq!(route.unwrap_or(id), id, "{id} wire model");
        }
        let (_, route) = generation(&options("gemini-2.5-flash"), &for_model("gemini-2.5-flash"));
        assert_eq!(route, Some("gemini-2.5-flash"));
    }

    #[test]
    fn temperature_leads_the_generation_config() {
        let mut options = options("gemini-2.0-flash");
        options.temperature = Some(0.5);
        assert_eq!(
            generation_json(&options, &for_model("gemini-2.0-flash")),
            r#"{"temperature":0.5,"maxOutputTokens":8192}"#
        );
    }

    #[test]
    fn budget_clamp_boundaries() {
        let pro = for_model("gemini-2.5-pro");
        let budget = |max: u32, budget: u32| {
            format!(
                r#"{{"maxOutputTokens":{max},"thinkingConfig":{{"includeThoughts":true,"thinkingBudget":{budget}}}}}"#
            )
        };
        let mut opts = options("gemini-2.5-pro");
        opts.max_tokens = 64_512;
        assert_eq!(generation_json(&opts, &pro), budget(65_536, 1024));
        opts.max_tokens = 65_000;
        assert_eq!(generation_json(&opts, &pro), budget(65_536, 1024));

        opts.max_tokens = 8192;
        let mut capped = pro;
        capped.max_output = 1025;
        assert_eq!(generation_json(&opts, &capped), budget(1025, 1024));
        // max ≤ budget → max(0, max − 1024) = 0 → thinking off with the base cap.
        capped.max_output = 1024;
        assert_eq!(
            generation_json(&opts, &capped),
            r#"{"maxOutputTokens":1024}"#
        );

        opts.max_tokens = 0;
        assert_eq!(generation_json(&opts, &pro), r#"{"maxOutputTokens":0}"#);
    }

    #[test]
    fn level_routes_and_suppression() {
        let opts = options("gemini-3-flash-preview");
        let mut flash = for_model("gemini-3-flash-preview");
        flash.routes = Some(EffortRoutes {
            off: Some("flash-off"),
            minimal: Some("flash-low"),
            low: Some("flash-low"),
            medium: None,
            high: None,
        });
        assert_eq!(
            generation_json(&opts, &flash),
            r#"{"maxOutputTokens":8192,"thinkingConfig":{"includeThoughts":true,"thinkingLevel":"LOW"}}"#
        );
        assert_eq!(generation(&opts, &flash).1, Some("flash-low"));

        flash.suppress_when_off = true;
        assert_eq!(
            generation_json(&opts, &flash),
            r#"{"maxOutputTokens":8192,"thinkingConfig":{"includeThoughts":false,"thinkingLevel":"MINIMAL"}}"#
        );
        assert_eq!(generation(&opts, &flash).1, Some("flash-off"));

        let mut pro = for_model("gemini-2.5-pro");
        pro.suppress_when_off = true;
        assert_eq!(
            generation_json(&options("gemini-2.5-pro"), &pro),
            r#"{"maxOutputTokens":8192,"thinkingConfig":{"includeThoughts":false,"thinkingBudget":0}}"#
        );
    }

    #[test]
    fn signature_validity() {
        for (signature, valid) in [
            ("U0lHMA==", true),
            ("U0lHMQ==", true),
            ("abcd", true),
            ("ab+/", true),
            ("abc", false),
            ("a_b-", false),
            ("", false),
            ("a===", false),
            ("====", false),
            ("ab=c", false),
        ] {
            assert_eq!(valid_signature(signature), valid, "{signature:?}");
        }
    }

    #[test]
    fn malformed_call_names() {
        assert!(!is_malformed("id", "ns:tool.v-1"));
        assert!(!is_malformed("id", "_x"));
        assert!(!is_malformed("id", &"a".repeat(64)));
        assert!(is_malformed("id", &"a".repeat(65)));
        assert!(is_malformed("id", "9x"));
        assert!(is_malformed("id", "bad name"));
        assert!(is_malformed("id", ""));
        assert!(is_malformed(" ", "read"));
    }

    #[test]
    fn full_conversation_request_bytes() {
        let mut first = user("Look");
        first.context_prefix = Some("ctx".into());
        first.images.push(ImageAttachment {
            media_type: "image/png".into(),
            data: "AAAA".into(),
        });
        let messages = [
            first,
            assistant(
                "Checking.",
                &[
                    ("call_a", "read", json!({"path": "a"})),
                    ("call_b", "grep", json!({"q": "x"})),
                    ("call_m", "bad name", json!({})),
                ],
            ),
            results(&[
                ("call_b", "b-out", false),
                ("call_a", "a-out", false),
                ("call_m", "m-out", true),
            ]),
            results(&[("ghost", "stale text", false)]),
            assistant("", &[("call_c", "run", json!({}))]),
            user("continue"),
        ];
        let mut options = options(PRO31);
        options.system_prompt = "Be terse.".into();
        let body = encode(&messages, &options, &for_model(PRO31), "proj-1").expect("encode");
        let expected = concat!(
            r#"{"project":"proj-1","model":"gemini-3.1-pro-preview","request":{"contents":["#,
            r#"{"role":"user","parts":[{"text":"ctx\n\nLook"},{"inlineData":{"mimeType":"image/png","data":"AAAA"}}]},"#,
            r#"{"role":"model","parts":[{"text":"Checking."},"#,
            r#"{"functionCall":{"name":"read","args":{"path":"a"}},"thoughtSignature":"skip_thought_signature_validator"},"#,
            r#"{"functionCall":{"name":"grep","args":{"q":"x"}}}]},"#,
            r#"{"role":"user","parts":[{"functionResponse":{"name":"read","response":{"output":"a-out"}}},"#,
            r#"{"functionResponse":{"name":"grep","response":{"output":"b-out"}}}]},"#,
            r#"{"role":"user","parts":[{"text":"<stale-tool-result tool=\"tool\" id=\"ghost\">\nstale text\n</stale-tool-result>"}]},"#,
            r#"{"role":"model","parts":[{"functionCall":{"name":"run","args":{}},"thoughtSignature":"skip_thought_signature_validator"}]},"#,
            r#"{"role":"user","parts":[{"functionResponse":{"name":"run","response":{"error":"No result provided"}}}]},"#,
            r#"{"role":"user","parts":[{"text":"continue"}]}],"#,
            r#""systemInstruction":{"parts":[{"text":"Be terse."}]},"#,
            r#""generationConfig":{"maxOutputTokens":8192,"thinkingConfig":{"includeThoughts":true,"thinkingLevel":"LOW"}}}}"#,
        );
        assert_eq!(String::from_utf8(body).expect("utf-8"), expected);
    }

    #[test]
    fn tools_declare_normalized_schemas_without_tool_config() {
        let schema = json!({"type": "object", "properties": {"path": {"type": "string"}}});
        let mut options = options(PRO31);
        options.system_prompt = "S".into();
        options.tools.push(ToolDefinition {
            name: "read".into(),
            description: String::new(),
            input_schema: schema.clone(),
            parallel_safe: false,
        });
        let body = String::from_utf8(
            encode(&[user("hi")], &options, &for_model(PRO31), "p").expect("encode"),
        )
        .expect("utf-8");
        let normalized =
            serde_json::to_string(&super::super::schema::normalize_for_google(&schema))
                .expect("schema encode");
        let tools = format!(
            r#""systemInstruction":{{"parts":[{{"text":"S"}}]}},"tools":[{{"functionDeclarations":[{{"name":"read","description":"","parametersJsonSchema":{normalized}}}]}}],"generationConfig":"#
        );
        assert!(body.contains(&tools), "{body}");
        assert!(!body.contains("toolConfig"));
        assert!(!body.contains(r#""id""#));
    }

    #[test]
    fn blank_system_prompt_and_blank_user_text_are_omitted() {
        let mut options = options("gemini-2.0-flash");
        options.system_prompt = "  \n".into();
        let body = String::from_utf8(
            encode(
                &[user("   "), user("x")],
                &options,
                &for_model("gemini-2.0-flash"),
                "p",
            )
            .expect("encode"),
        )
        .expect("utf-8");
        assert_eq!(
            body,
            r#"{"project":"p","model":"gemini-2.0-flash","request":{"contents":[{"role":"user","parts":[{"text":"x"}]}],"generationConfig":{"maxOutputTokens":8192}}}"#
        );
    }

    #[test]
    fn history_system_messages_become_user_content() {
        let messages = [message(MessageRole::System, "rules"), user("hi")];
        assert_eq!(
            wire(&messages, PRO31),
            r#"[{"role":"user","parts":[{"text":"rules"}]},{"role":"user","parts":[{"text":"hi"}]}]"#
        );
    }

    #[test]
    fn non_vision_profile_replaces_images_with_placeholder() {
        let mut message = user("see");
        message.images.push(ImageAttachment {
            media_type: "image/jpeg".into(),
            data: "BBBB".into(),
        });
        let mut profile = for_model(PRO31);
        profile.vision = false;
        let parts = contents(std::slice::from_ref(&message), &options(PRO31), &profile);
        assert_eq!(
            serde_json::to_string(&parts).expect("encode"),
            r#"[{"role":"user","parts":[{"text":"see"},{"text":"[image omitted: model does not support vision]"}]}]"#
        );
    }

    /// Story 20.1d4a T1: calls A then B; B's image-only result arrives first.
    fn image_results(model: &str, vision: bool) -> String {
        let image = |data: &str| ImageAttachment {
            media_type: "image/png".into(),
            data: data.into(),
        };
        let mut out = results(&[("b", "", false), ("a", "A text", false)]);
        out.tool_results[0].images = vec![image("BBBB")];
        out.tool_results[1].images = vec![image("AAAA")];
        let messages = [
            assistant("", &[("a", "read", json!({})), ("b", "grep", json!({}))]),
            out,
            user("next"),
        ];
        let mut profile = for_model(model);
        profile.vision = vision;
        serde_json::to_string(&contents(&messages, &options(model), &profile)).expect("encode")
    }

    #[test]
    fn multimodal_model_nests_result_images_in_the_call_ordered_result_turn() {
        let wire = image_results(PRO31, true);
        assert!(
            wire.contains(concat!(
                r#"{"role":"user","parts":[{"functionResponse":{"name":"read","response":{"output":"A text"},"parts":[{"inlineData":{"mimeType":"image/png","data":"AAAA"}}]}},"#,
                r#"{"functionResponse":{"name":"grep","response":{"output":"(see attached image)"},"parts":[{"inlineData":{"mimeType":"image/png","data":"BBBB"}}]}}]},"#,
                r#"{"role":"user","parts":[{"text":"next"}]}]"#
            )),
            "{wire}"
        );
    }

    #[test]
    fn older_model_gets_one_buffered_image_turn_after_the_result_turn_in_call_order() {
        assert_eq!(
            image_results("gemini-2.5-pro", true),
            concat!(
                r#"[{"role":"model","parts":[{"functionCall":{"name":"read","args":{}}},{"functionCall":{"name":"grep","args":{}}}]},"#,
                r#"{"role":"user","parts":[{"functionResponse":{"name":"read","response":{"output":"A text"}}},"#,
                r#"{"functionResponse":{"name":"grep","response":{"output":"(see attached image)"}}}]},"#,
                r#"{"role":"user","parts":[{"text":"Tool result image:"},{"inlineData":{"mimeType":"image/png","data":"AAAA"}},"#,
                r#"{"text":"Tool result image:"},{"inlineData":{"mimeType":"image/png","data":"BBBB"}}]},"#,
                r#"{"role":"user","parts":[{"text":"next"}]}]"#
            )
        );
    }

    #[test]
    fn non_vision_profile_sends_the_placeholder_value_and_no_result_image() {
        let wire = image_results(PRO31, false);
        assert!(!wire.contains("inlineData"), "{wire}");
        assert!(
            wire.contains(concat!(
                r#"{"functionResponse":{"name":"read","response":{"output":"A text\n[image omitted: model does not support vision]"}}},"#,
                r#"{"functionResponse":{"name":"grep","response":{"output":"[image omitted: model does not support vision]"}}}]}"#
            )),
            "{wire}"
        );
    }

    #[test]
    fn consecutive_result_messages_merge_into_one_turn() {
        let messages = [
            assistant("", &[("a", "read", json!({})), ("b", "grep", json!({}))]),
            results(&[("b", "B", false)]),
            results(&[("a", "A", true)]),
        ];
        assert_eq!(
            wire(&messages, "gemini-2.5-pro"),
            concat!(
                r#"[{"role":"model","parts":[{"functionCall":{"name":"read","args":{}}},{"functionCall":{"name":"grep","args":{}}}]},"#,
                r#"{"role":"user","parts":[{"functionResponse":{"name":"read","response":{"error":"A"}}},"#,
                r#"{"functionResponse":{"name":"grep","response":{"output":"B"}}}]}]"#
            )
        );
    }

    #[test]
    fn user_text_after_results_is_a_following_turn() {
        let mut answer = results(&[("a", "A", false)]);
        answer.content = "next".into();
        let messages = [assistant("", &[("a", "read", json!({"x": 1}))]), answer];
        assert_eq!(
            wire(&messages, "gemini-2.5-pro"),
            concat!(
                r#"[{"role":"model","parts":[{"functionCall":{"name":"read","args":{"x":1}}}]},"#,
                r#"{"role":"user","parts":[{"functionResponse":{"name":"read","response":{"output":"A"}}}]},"#,
                r#"{"role":"user","parts":[{"text":"next"}]}]"#
            )
        );
    }

    #[test]
    fn orphan_result_inside_an_open_window_is_dropped() {
        let messages = [
            assistant("", &[("a", "read", json!({}))]),
            results(&[("ghost", "lost", true), ("a", "A", false)]),
        ];
        let body = wire(&messages, "gemini-2.5-pro");
        assert!(!body.contains("ghost") && !body.contains("lost"), "{body}");
        let messages = [results(&[("ghost", "lost", true)])];
        assert_eq!(
            wire(&messages, "gemini-2.5-pro"),
            r#"[{"role":"user","parts":[{"text":"<stale-tool-result tool=\"tool\" id=\"ghost\" is-error=\"true\">\nlost\n</stale-tool-result>"}]}]"#
        );
    }

    #[test]
    fn reasoning_content_is_demoted_to_a_thinking_fence() {
        let mut turn = assistant("ok", &[]);
        turn.reasoning_content = Some("plan".into());
        let mut blank = assistant("done", &[]);
        blank.reasoning_content = Some("  ".into());
        assert_eq!(
            wire(&[turn, blank], PRO31),
            concat!(
                r#"[{"role":"model","parts":[{"text":"```thinking\nplan\n```"},{"text":"ok"}]},"#,
                r#"{"role":"model","parts":[{"text":"done"}]}]"#
            )
        );
    }

    fn replay_item(provider: &str, model: &str, call_a_signature: &str) -> ProviderReplayItem {
        ProviderReplayItem {
            provider: provider.into(),
            model: model.into(),
            payload: Arc::new(json!({"gemini_parts": [
                {"thought": true, "text": "T", "thoughtSignature": "U0lHMA=="},
                {"text": "V", "thoughtSignature": "U0lHMQ=="},
                {"functionCall": {"name": "read", "args": {"p": 1}}, "thoughtSignature": call_a_signature, "callId": "a"},
                {"functionCall": {"name": "grep", "args": {"q": 2}}, "callId": "b"},
            ]})),
        }
    }

    fn replayed_turn(item: ProviderReplayItem, content: &str) -> [Message; 2] {
        let mut turn = assistant(
            content,
            &[
                ("a", "read", json!({"p": 1})),
                ("b", "grep", json!({"q": 2})),
            ],
        );
        turn.reasoning_content = Some("T".into());
        turn.provider_replay.push(item);
        [turn, results(&[("b", "B", false), ("a", "A", false)])]
    }

    const RESULTS: &str = concat!(
        r#"{"role":"user","parts":[{"functionResponse":{"name":"read","response":{"output":"A"}}},"#,
        r#"{"functionResponse":{"name":"grep","response":{"output":"B"}}}]}]"#
    );

    #[test]
    fn matching_item_replays_signed_parts_without_call_ids() {
        let messages = replayed_turn(
            replay_item(super::super::PROVIDER_KIND, PRO31, "U0lHMg=="),
            "V",
        );
        let body = wire(&messages, PRO31);
        let expected = format!(
            "{}{RESULTS}",
            concat!(
                r#"[{"role":"model","parts":[{"thought":true,"text":"T","thoughtSignature":"U0lHMA=="},"#,
                r#"{"text":"V","thoughtSignature":"U0lHMQ=="},"#,
                r#"{"functionCall":{"name":"read","args":{"p":1}},"thoughtSignature":"U0lHMg=="},"#,
                r#"{"functionCall":{"name":"grep","args":{"q":2}}}]},"#
            )
        );
        assert_eq!(body, expected);
        assert!(!body.contains("callId"));
    }

    #[test]
    fn invalid_signature_is_dropped_and_the_first_call_gets_the_sentinel() {
        let messages = replayed_turn(replay_item(super::super::PROVIDER_KIND, PRO31, "abc"), "V");
        let expected = format!(
            "{}{RESULTS}",
            concat!(
                r#"[{"role":"model","parts":[{"thought":true,"text":"T","thoughtSignature":"U0lHMA=="},"#,
                r#"{"text":"V","thoughtSignature":"U0lHMQ=="},"#,
                r#"{"functionCall":{"name":"read","args":{"p":1}},"thoughtSignature":"skip_thought_signature_validator"},"#,
                r#"{"functionCall":{"name":"grep","args":{"q":2}}}]},"#
            )
        );
        assert_eq!(wire(&messages, PRO31), expected);
    }

    const UNSIGNED: &str = concat!(
        r#"{"functionCall":{"name":"read","args":{"p":1}},"thoughtSignature":"skip_thought_signature_validator"},"#,
        r#"{"functionCall":{"name":"grep","args":{"q":2}}}]},"#
    );

    #[test]
    fn edited_turn_falls_back_to_unsigned_reconstruction() {
        let messages = replayed_turn(
            replay_item(super::super::PROVIDER_KIND, PRO31, "U0lHMg=="),
            "V edited",
        );
        let body = wire(&messages, PRO31);
        assert_eq!(
            body,
            format!(
                r#"[{{"role":"model","parts":[{{"text":"```thinking\nT\n```"}},{{"text":"V edited"}},{UNSIGNED}{RESULTS}"#
            )
        );
        assert!(!body.contains("U0lH"));

        // Calls that differ from `tool_uses` fail the guard as well.
        let mut messages = replayed_turn(
            replay_item(super::super::PROVIDER_KIND, PRO31, "U0lHMg=="),
            "V",
        );
        messages[0].tool_uses[1].input = json!({"q": 3});
        assert!(!wire(&messages, PRO31).contains("U0lH"));
    }

    /// The TUI mirror's content for `[T, V, call a, call b]`: each call-free run
    /// folds as its visible text, then its thinking (`reduce` flushes prose
    /// before reasoning).
    #[test]
    fn unedited_tui_mirror_fold_keeps_the_signatures() {
        let signed = wire(
            &replayed_turn(
                replay_item(super::super::PROVIDER_KIND, PRO31, "U0lHMg=="),
                "V",
            ),
            PRO31,
        );
        let folded = wire(
            &replayed_turn(
                replay_item(super::super::PROVIDER_KIND, PRO31, "U0lHMg=="),
                "VT",
            ),
            PRO31,
        );
        assert_eq!(folded, signed);

        // Runs split at calls: `[T1, V1, call a, T2, V2]` folds as `V1T1V2T2`.
        let mut turn = assistant("V1T1V2T2", &[("a", "read", json!({"p": 1}))]);
        turn.provider_replay.push(ProviderReplayItem {
            provider: super::super::PROVIDER_KIND.into(),
            model: PRO31.into(),
            payload: Arc::new(json!({"gemini_parts": [
                {"thought": true, "text": "T1", "thoughtSignature": "U0lHMA=="},
                {"text": "V1"},
                {"functionCall": {"name": "read", "args": {"p": 1}}, "thoughtSignature": "U0lHMg==", "callId": "a"},
                {"thought": true, "text": "T2"},
                {"text": "V2", "thoughtSignature": "U0lHMQ=="},
            ]})),
        });
        let body = wire(&[turn, results(&[("a", "A", false)])], PRO31);
        assert!(
            body.contains(r#""thoughtSignature":"U0lHMA==""#)
                && body.contains(r#""thoughtSignature":"U0lHMg==""#)
                && body.contains(r#""thoughtSignature":"U0lHMQ==""#),
            "{body}"
        );
    }

    /// Only the exact visible text or the exact mirror fold keeps the item: an
    /// edited fold, a fold in an order the mirror never produces, or a fold
    /// missing or repeating its thinking all fall back to the unsigned rebuild.
    #[test]
    fn edited_tui_mirror_fold_drops_the_signatures() {
        for content in ["VT edited", "V editedT", "TV", "V T", "VTT", "T", ""] {
            let messages = replayed_turn(
                replay_item(super::super::PROVIDER_KIND, PRO31, "U0lHMg=="),
                content,
            );
            let body = wire(&messages, PRO31);
            assert!(!body.contains("U0lH"), "{content:?}: {body}");
        }
    }

    #[test]
    fn other_model_item_is_rebuilt_unsigned_with_demoted_thinking() {
        let messages = replayed_turn(
            replay_item(
                super::super::PROVIDER_KIND,
                "gemini-3-pro-preview",
                "U0lHMg==",
            ),
            "V",
        );
        let body = wire(&messages, PRO31);
        assert_eq!(
            body,
            format!(
                r#"[{{"role":"model","parts":[{{"text":"```thinking\nT\n```"}},{{"text":"V"}},{UNSIGNED}{RESULTS}"#
            )
        );
        assert!(!body.contains("U0lH"));
    }

    #[test]
    fn other_provider_item_is_never_sent() {
        let messages = replayed_turn(replay_item("openai-codex", PRO31, "U0lHMg=="), "V");
        let body = wire(&messages, PRO31);
        assert_eq!(
            body,
            format!(
                r#"[{{"role":"model","parts":[{{"text":"```thinking\nT\n```"}},{{"text":"V"}},{UNSIGNED}{RESULTS}"#
            )
        );
        assert!(!body.contains("U0lH") && !body.contains("gemini_parts"));
    }

    #[test]
    fn replayed_malformed_call_is_dropped_with_its_result() {
        let mut turn = assistant(
            "",
            &[("a", "read", json!({})), ("m", "bad name", json!({}))],
        );
        turn.provider_replay.push(ProviderReplayItem {
            provider: super::super::PROVIDER_KIND.into(),
            model: PRO31.into(),
            payload: Arc::new(json!({"gemini_parts": [
                {"functionCall": {"name": "read", "args": {}}, "callId": "a"},
                {"functionCall": {"name": "bad name", "args": {}}, "thoughtSignature": "U0lHMg==", "callId": "m"},
            ]})),
        });
        let messages = [turn, results(&[("a", "A", false), ("m", "M", true)])];
        assert_eq!(
            wire(&messages, PRO31),
            concat!(
                r#"[{"role":"model","parts":[{"functionCall":{"name":"read","args":{}},"thoughtSignature":"skip_thought_signature_validator"}]},"#,
                r#"{"role":"user","parts":[{"functionResponse":{"name":"read","response":{"output":"A"}}}]}]"#
            )
        );
    }
}
