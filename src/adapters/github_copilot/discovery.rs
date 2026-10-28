//! Copilot live model discovery: the `/models` mapping, long-context siblings
//! and the re-probed root, adapted from oh-my-pi
//! (catalog/src/provider-models/openai-compat.ts
//! `githubCopilotModelManagerOptions`).
//!
//! MIT License
//!
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

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use serde_json::Value;

use super::{
    Inner, LiveRow, Shared,
    catalog::{self, Route},
    headers::{self, CopilotGrant},
    listing_headers, listing_status, routing,
};
use crate::adapters::model_catalog_cache::request_scope;
use crate::adapters::model_filter::{compile_filter_patterns, matches_any};
use crate::adapters::responses::transport;
use crate::domain::errors::ProviderError;
use crate::domain::models::{
    ModelCapability,
    provider::{DiscoveredModels, ModelRecord},
};

const TIMEOUT: Duration = Duration::from_secs(10);
/// omp's defaults for an id no reference describes.
const DEFAULT_CONTEXT: u32 = 128_000;
const DEFAULT_MAX_OUTPUT: u32 = 8_192;
const ENVELOPE_KEYS: [&str; 4] = ["data", "models", "result", "items"];
const LONG_CONTEXT_ID_SUFFIX: &str = "-1m";
const LONG_CONTEXT_NAME_SUFFIX: &str = " (1M)";

impl Inner {
    /// `GET <root>/models`, re-probing the account root when the request root
    /// is the canonical one. A stored row's 401 recovers once; the scope is
    /// that of the credential actually sent.
    pub(super) async fn discover(
        &self,
        model_filter: &[String],
        scope_context: &str,
    ) -> Result<DiscoveredModels, ProviderError> {
        let mut resolved = self.handle.current().await?;
        let mut recovered = false;
        let (response, root) = loop {
            let grant = CopilotGrant::of(&resolved);
            let mut root =
                headers::copilot_root(self.shared.configured_base.as_deref(), &grant, None);
            if root == super::canonical_root() {
                let learned = routing::probe(&self.client, &grant.token).await;
                if let Some(endpoint) = &learned {
                    root = headers::derived(endpoint);
                }
                let scope = request_scope(scope_context, &resolved);
                self.shared.learn(scope_context, &scope, learned);
            }
            let response = self
                .client
                .get(format!("{root}/models"))
                .headers(listing_headers(&resolved.auth)?)
                .timeout(TIMEOUT)
                .send()
                .await
                .map_err(|e| crate::adapters::provider::classify_reqwest_error(&e))?;
            if response.status().as_u16() == 401
                && transport::recover_once(&self.handle, &mut resolved, &mut recovered).await?
            {
                continue;
            }
            break (response, root);
        };
        listing_status(response.status().as_u16())?;
        let body = response
            .bytes()
            .await
            .map_err(|e| crate::adapters::provider::classify_reqwest_error(&e))?;
        let payload: Value = serde_json::from_slice(&body).map_err(|_| unparseable())?;
        let canonical = root == super::canonical_root();
        let (records, live) = records(&payload, canonical, model_filter).ok_or_else(unparseable)?;
        // The listing belongs to the credential actually sent.
        let scope = request_scope(scope_context, &resolved);
        self.shared.listed(scope_context, &scope, live);
        Ok(DiscoveredModels {
            records,
            credential_scope: scope,
        })
    }
}

fn unparseable() -> ProviderError {
    ProviderError::Other("Could not parse GitHub Copilot models response".into())
}

/// omp `toNumber` kept when positive; a fraction below one token is not a limit.
fn positive(value: Option<&Value>) -> Option<u32> {
    let number = match value? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) if !s.trim().is_empty() => s.trim().parse::<f64>().ok(),
        _ => None,
    }?;
    (number.is_finite() && number >= 1.0).then(|| number.min(f64::from(u32::MAX)) as u32)
}

/// An array of rows, or the first usable `data`/`models`/`result`/`items`
/// member (recursively).
fn live_rows(node: &Value) -> Option<Vec<&serde_json::Map<String, Value>>> {
    match node {
        Value::Array(rows) => Some(
            rows.iter()
                .filter_map(Value::as_object)
                .filter(|row| {
                    row.get("id")
                        .and_then(Value::as_str)
                        .is_some_and(|id| !id.is_empty())
                })
                .collect(),
        ),
        Value::Object(object) => ENVELOPE_KEYS
            .iter()
            .filter_map(|key| object.get(*key))
            .find_map(live_rows),
        _ => None,
    }
}

/// The bundled row a live id is mapped against.
struct Reference {
    name: String,
    context: u32,
    max_output: u32,
    vision: bool,
    thinking: bool,
}

fn reference(id: &str) -> Option<Reference> {
    let descriptor = Shared::seed()
        .iter()
        .map(|entry| &entry.descriptor)
        .find(|m| m.model_id == id)?;
    Some(Reference {
        name: descriptor.display_name.clone(),
        context: descriptor.context_window,
        max_output: catalog::seed_row(id).map_or(DEFAULT_MAX_OUTPUT, |row| row.max_output),
        vision: descriptor.capabilities.contains(&ModelCapability::Vision),
        thinking: descriptor.capabilities.contains(&ModelCapability::Thinking),
    })
}

/// omp's lenient Claude classifier: the first numeric token after the
/// family, read as `major.minor`, is at least 3.7.
fn claude_thinks(id: &str) -> bool {
    let id = id.to_ascii_lowercase();
    let Some(rest) = [
        "claude-haiku-",
        "claude-sonnet-",
        "claude-opus-",
        "claude-fable-",
        "claude-mythos-",
    ]
    .iter()
    .find_map(|family| id.strip_prefix(family)) else {
        return false;
    };
    let mut tokens = rest.split('-');
    let Some(first) = tokens
        .next()
        .filter(|t| t.starts_with(|c: char| c.is_ascii_digit()))
    else {
        return false;
    };
    let (major, minor) = match first.split_once('.') {
        Some((major, minor)) => (major.parse::<u32>().ok(), minor.parse::<u32>().ok()),
        None => {
            let minor = tokens
                .next()
                .filter(|t| !t.is_empty() && t.len() <= 2 && t.bytes().all(|b| b.is_ascii_digit()))
                .and_then(|t| t.parse::<u32>().ok());
            (first.parse::<u32>().ok(), minor.or(Some(0)))
        }
    };
    matches!((major, minor), (Some(major), Some(minor)) if (major, minor) >= (3, 7))
}

/// `billing.token_prices.<tier>`: `max_prompt_tokens ?? context_max`.
fn tier_max(row: &serde_json::Map<String, Value>, tier: &str) -> Option<f64> {
    let tier = row.get("billing")?.get("token_prices")?.get(tier)?;
    if !tier.is_object() {
        return None;
    }
    let number = |key: &str| match tier.get(key)? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    };
    number("max_prompt_tokens").or_else(|| number("context_max"))
}

/// One live row as records: the row itself and, with a long-context tier, its
/// `-1m` sibling. `None` drops a non-chat row.
fn map_row(
    row: &serde_json::Map<String, Value>,
    canonical: bool,
) -> Option<(ModelRecord, u32, Option<(ModelRecord, u32)>)> {
    let id = row.get("id")?.as_str()?.to_owned();
    let capabilities = row.get("capabilities").filter(|c| c.is_object());
    if capabilities
        .and_then(|c| c.get("type"))
        .and_then(Value::as_str)
        .is_some_and(|kind| kind != "chat")
    {
        return None;
    }
    let limits = capabilities
        .and_then(|c| c.get("limits"))
        .filter(|l| l.is_object());
    let limit = |key: &str| positive(limits.and_then(|l| l.get(key)));
    let reference = reference(&id);
    let context = limit("max_context_window_tokens")
        .or_else(|| positive(row.get("context_length")))
        .or_else(|| limit("max_prompt_tokens"))
        .unwrap_or_else(|| reference.as_ref().map_or(DEFAULT_CONTEXT, |r| r.context));
    let max_output = limit("max_output_tokens")
        .or_else(|| positive(row.get("max_completion_tokens")))
        .or_else(|| limit("max_non_streaming_output_tokens"))
        .unwrap_or_else(|| {
            reference
                .as_ref()
                .map_or(DEFAULT_MAX_OUTPUT, |r| r.max_output)
        });
    let live_name = row
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.trim().is_empty());
    let vision = match capabilities
        .and_then(|c| c.get("supports"))
        .filter(|s| s.is_object())
        .and_then(|s| s.get("vision"))
        .and_then(Value::as_bool)
    {
        Some(vision) => vision,
        None if canonical => reference.as_ref().is_some_and(|r| r.vision),
        None => false,
    };
    let thinking = match &reference {
        Some(reference) => reference.thinking,
        None => catalog::route(&id) == Route::Messages && claude_thinks(&id),
    };
    // The served window is the long-context ceiling; the base row keeps the
    // default tier, the long tier is the opt-in `-1m` sibling.
    let long = tier_max(row, "long_context").filter(|max| *max > 0.0);
    let billed = tier_max(row, "default");
    let tiered = if long.is_some() {
        limit("max_prompt_tokens").map_or(0.0, f64::from)
    } else {
        0.0
    };
    let default_max =
        if tiered > 0.0 && billed.is_none_or(|billed| billed <= 0.0 || tiered < billed) {
            Some(tiered)
        } else {
            billed
        };
    let window = |max: f64| (f64::from(context)).min(max + f64::from(max_output)) as u32;
    let base_window = default_max.filter(|max| *max > 0.0).map_or(context, window);
    let mut caps = HashSet::from([ModelCapability::ToolUse]);
    if vision {
        caps.insert(ModelCapability::Vision);
    }
    if thinking {
        caps.insert(ModelCapability::Thinking);
    }
    let sibling = long
        .map(window)
        .filter(|long_window| *long_window > base_window)
        .map(|long_window| {
            let name = live_name
                .map(str::to_owned)
                .or_else(|| reference.as_ref().map(|r| r.name.clone()))
                .unwrap_or_else(|| id.clone());
            (
                ModelRecord {
                    model_id: format!("{id}{LONG_CONTEXT_ID_SUFFIX}"),
                    display_name: Some(format!("{name}{LONG_CONTEXT_NAME_SUFFIX}")),
                    context_window: Some(long_window),
                    capabilities: Some(caps.clone()),
                    pricing_tier: None,
                    wire_model_id: Some(id.clone()),
                },
                max_output,
            )
        });
    let (mut context, mut output) = (Some(base_window), Some(max_output));
    catalog::pin_limits(&id, &mut context, &mut output);
    Some((
        ModelRecord {
            model_id: id,
            display_name: live_name.map(str::to_owned),
            context_window: context,
            capabilities: Some(caps),
            pricing_tier: None,
            wire_model_id: None,
        },
        output.unwrap_or(max_output),
        sibling,
    ))
}

/// The live rows mapped, their `-1m` siblings (a real upstream id wins) and
/// the bundled ids live omits, sorted by id; plus each live id's output limit.
fn records(
    payload: &Value,
    canonical: bool,
    model_filter: &[String],
) -> Option<(Vec<ModelRecord>, HashMap<String, LiveRow>)> {
    let mut mapped: BTreeMap<String, (ModelRecord, u32)> = BTreeMap::new();
    let mut siblings = Vec::new();
    for row in live_rows(payload)? {
        if let Some((record, output, sibling)) = map_row(row, canonical) {
            mapped.insert(record.model_id.clone(), (record, output));
            siblings.extend(sibling);
        }
    }
    for (record, output) in siblings {
        mapped
            .entry(record.model_id.clone())
            .or_insert((record, output));
    }
    let live: HashMap<String, LiveRow> = mapped
        .iter()
        .map(|(id, (_, output))| {
            (
                id.clone(),
                LiveRow {
                    max_output: Some(*output),
                },
            )
        })
        .collect();
    for entry in Shared::seed() {
        let id = &entry.descriptor.model_id;
        if !mapped.contains_key(id) {
            mapped.insert(
                id.clone(),
                (
                    ModelRecord {
                        model_id: id.clone(),
                        ..Default::default()
                    },
                    0,
                ),
            );
        }
    }
    let filter = compile_filter_patterns(model_filter);
    let records = mapped
        .into_values()
        .map(|(record, _)| record)
        .filter(|record| matches_any(&filter, &record.model_id))
        .collect();
    Some((records, live))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ids(records: &[ModelRecord]) -> Vec<&str> {
        records.iter().map(|r| r.model_id.as_str()).collect()
    }

    #[test]
    fn mapping_drops_non_chat_rows_and_adds_long_context_siblings() {
        let payload = json!({"data":[
            {"id":"gpt-5.5","name":"GPT-5.5 live","capabilities":{"type":"chat","limits":{"max_context_window_tokens":1050000,"max_output_tokens":128000,"max_prompt_tokens":272000},"supports":{"vision":true}}},
            {"id":"claude-opus-5","capabilities":{"type":"chat","limits":{"max_context_window_tokens":1000000,"max_output_tokens":64000,"max_prompt_tokens":200000}},
             "billing":{"token_prices":{"default":{"max_prompt_tokens":200000},"long_context":{"max_prompt_tokens":936000}}}},
            {"id":"text-embedding-3","capabilities":{"type":"embedding"}},
            {"id":"new-model"}
        ]});
        let (records, live) = records(&payload, false, &[]).unwrap();
        let ids = ids(&records);
        assert!(!ids.contains(&"text-embedding-3"));
        let position = |id: &str| ids.iter().position(|x| *x == id).unwrap();
        assert_eq!(position("claude-opus-5") + 1, position("claude-opus-5-1m"));
        assert_eq!(ids.last(), Some(&"raptor-mini"));
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        assert_eq!(ids, sorted);
        let opus = &records[position("claude-opus-5")];
        assert_eq!(opus.context_window, Some(264_000));
        let long = &records[position("claude-opus-5-1m")];
        assert_eq!(long.wire_model_id.as_deref(), Some("claude-opus-5"));
        assert_eq!(long.display_name.as_deref(), Some("Claude Opus 5 (1M)"));
        assert_eq!(long.context_window, Some(1_000_000));
        let fresh = &records[position("new-model")];
        assert_eq!(fresh.context_window, Some(128_000));
        assert_eq!(
            fresh.capabilities,
            Some(HashSet::from([ModelCapability::ToolUse]))
        );
        assert_eq!(live["new-model"].max_output, Some(8_192));
        assert_eq!(live["claude-opus-5-1m"].max_output, Some(64_000));
        assert!(!live.contains_key("gpt-4o"));
        // Live ids keep their limits; bundled ids live omits are bare.
        let gpt4o = &records[position("gpt-4o")];
        assert!(gpt4o.context_window.is_none() && gpt4o.capabilities.is_none());
    }

    #[test]
    fn vision_follows_the_root_rule_and_pins_apply() {
        let payload = json!([
            {"id":"gpt-4o"},
            {"id":"claude-opus-4.6","capabilities":{"limits":{"max_context_window_tokens":200000,"max_output_tokens":64000}}}
        ]);
        let (canonical, _) = records(&payload, true, &[]).unwrap();
        let (tenant, live) = records(&payload, false, &[]).unwrap();
        let find = |records: &[ModelRecord], id: &str| {
            records.iter().find(|r| r.model_id == id).cloned().unwrap()
        };
        assert!(
            find(&canonical, "gpt-4o")
                .capabilities
                .unwrap()
                .contains(&ModelCapability::Vision)
        );
        assert!(
            !find(&tenant, "gpt-4o")
                .capabilities
                .unwrap()
                .contains(&ModelCapability::Vision)
        );
        assert_eq!(
            find(&tenant, "claude-opus-4.6").context_window,
            Some(168_000)
        );
        assert_eq!(live["claude-opus-4.6"].max_output, Some(32_000));
    }

    #[test]
    fn envelopes_and_unparseable_payloads() {
        assert!(records(&json!({"models":[{"id":"x"}]}), false, &[]).is_some());
        assert!(records(&json!({"result":{"items":[{"id":"x"}]}}), false, &[]).is_some());
        assert!(records(&json!({}), false, &[]).is_none());
        assert!(records(&json!("x"), false, &[]).is_none());
    }

    #[test]
    fn unreferenced_claude_thinks_from_revision_three_seven() {
        assert!(claude_thinks("claude-opus-5"));
        assert!(claude_thinks("claude-sonnet-3.7"));
        assert!(claude_thinks("claude-sonnet-3-7"));
        assert!(!claude_thinks("claude-haiku-3.5"));
        assert!(!claude_thinks("claude-haiku-3-5-20241022"));
        assert!(!claude_thinks("gpt-5"));
    }

    #[test]
    fn limits_below_one_token_fall_through() {
        assert_eq!(positive(Some(&json!(0.5))), None);
        assert_eq!(positive(Some(&json!(0))), None);
        assert_eq!(positive(Some(&json!(" 128000 "))), Some(128_000));
        assert_eq!(positive(Some(&json!(64000.9))), Some(64_000));
    }
}
