//! Live Codex model discovery, adapted from oh-my-pi's catalog Codex discovery
//! (`catalog/src/discovery/codex.ts`) and its provider compat rules.
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

use super::{Inner, PROVIDER_KIND, profile};
use crate::adapters::model_catalog_cache::request_scope;
use crate::adapters::model_filter::{compile_filter_patterns, matches_any};
use crate::domain::errors::ProviderError;
use crate::domain::models::provider::{DiscoveredModels, ModelCapability, ModelRecord};
use serde_json::Value;
use std::collections::HashSet;
use std::time::Duration;

/// GPT-5.6 luna/sol/terra serve a 1M window although the registry reports less.
const FLOORED: [&str; 3] = ["gpt-5.6-luna", "gpt-5.6-sol", "gpt-5.6-terra"];
const FLOOR_CONTEXT: u32 = 1_000_000;
const GPT_5_6_CONTEXT: u32 = 372_000;
const DEFAULT_CONTEXT: u32 = 272_000;
/// Worker-mode SKUs route like their plain bundled counterpart.
const WORKER_SUFFIX: &str = "-wm";
/// omp's sort key for rows without a finite `priority` (`Number.MAX_SAFE_INTEGER`).
const NO_PRIORITY: f64 = 9_007_199_254_740_991.0;

/// omp's window for an id every catalogue layer omits.
pub(super) fn default_context_window(id: &str) -> u32 {
    if FLOORED.contains(&id) {
        FLOOR_CONTEXT
    } else if id == "gpt-5.6" || id.starts_with("gpt-5.6-") {
        GPT_5_6_CONTEXT
    } else {
        DEFAULT_CONTEXT
    }
}

impl Inner {
    /// `GET <codex>/models`, then `<root>/models` after a non-auth failure. A
    /// stored row's `401` recovers once; any remaining `401`/`403` is final, with
    /// no fallback request. The scope is that of the credential actually sent.
    pub(super) async fn discover(
        &self,
        model_filter: &[String],
        scope_context: &str,
    ) -> Result<DiscoveredModels, ProviderError> {
        let mut resolved = self.handle.current().await?;
        let mut auth_retried = false;
        let codex = self.url.strip_suffix("/responses").unwrap_or(&self.url);
        let root = codex.strip_suffix("/codex").unwrap_or(codex);
        let mut failure = ProviderError::EndpointUnsupported(404);
        for base in [codex, root] {
            let url = format!("{base}/models?client_version={}", profile::CODEX_VERSION);
            let sent = loop {
                let mut headers = profile::auth_headers(&resolved.auth, false)?;
                headers.insert(
                    "accept",
                    reqwest::header::HeaderValue::from_static("application/json"),
                );
                let sent = self
                    .client
                    .get(&url)
                    .headers(headers)
                    .timeout(Duration::from_secs(5))
                    .send()
                    .await;
                if let Ok(response) = &sent
                    && response.status().as_u16() == 401
                    && self.recover(&mut resolved, &mut auth_retried).await?
                {
                    continue;
                }
                break sent;
            };
            let response = match sent {
                Ok(response) => response,
                Err(error) => {
                    failure = crate::adapters::provider::classify_reqwest_error(&error);
                    continue;
                }
            };
            let status = response.status().as_u16();
            match status {
                200..=299 => {}
                401 | 403 => return Err(ProviderError::AuthenticationFailed),
                404 | 405 => {
                    // Unsupported only when both paths say so: an earlier
                    // transport, status or parse failure keeps its report.
                    if matches!(failure, ProviderError::EndpointUnsupported(_)) {
                        failure = ProviderError::EndpointUnsupported(status);
                    }
                    continue;
                }
                _ => {
                    failure = ProviderError::Other(format!("Codex models HTTP {status}"));
                    continue;
                }
            }
            let parsed = match response.bytes().await {
                Ok(body) => serde_json::from_slice::<Value>(&body)
                    .ok()
                    .and_then(|payload| records(&payload, model_filter)),
                Err(error) => {
                    failure = crate::adapters::provider::classify_reqwest_error(&error);
                    continue;
                }
            };
            match parsed {
                Some(records) => {
                    return Ok(DiscoveredModels {
                        records,
                        credential_scope: request_scope(scope_context, &resolved),
                    });
                }
                None => {
                    failure = ProviderError::Other("Could not parse Codex models response".into());
                }
            }
        }
        Err(failure)
    }
}

struct Entry<'a> {
    slug: &'a str,
    name: &'a str,
    context: Option<u32>,
    capabilities: Option<HashSet<ModelCapability>>,
    priority: f64,
}

fn non_empty(value: &Value) -> Option<&str> {
    value.as_str().map(str::trim).filter(|s| !s.is_empty())
}

fn positive_int(value: &Value) -> Option<u32> {
    value
        .as_f64()
        .filter(|n| n.is_finite() && *n > 0.0)
        .map(|n| n.trunc().min(f64::from(u32::MAX)) as u32)
}

/// omp's `supportsReasoning`: a non-`none` default or supported effort.
fn reasoning(entry: &Value) -> bool {
    if non_empty(&entry["default_reasoning_level"])
        .is_some_and(|level| !level.eq_ignore_ascii_case("none"))
    {
        return true;
    }
    entry["supported_reasoning_levels"]
        .as_array()
        .is_some_and(|levels| {
            levels.iter().any(|level| {
                non_empty(&level["effort"]).is_some_and(|e| !e.eq_ignore_ascii_case("none"))
            })
        })
}

/// Capabilities only when the live row declares modalities or reasoning, so the
/// catalogue fold keeps the seed's set otherwise. Codex rows always take tools.
fn declared_capabilities(entry: &Value) -> Option<HashSet<ModelCapability>> {
    let modalities = entry["input_modalities"].as_array();
    let declares_reasoning = entry.get("default_reasoning_level").is_some()
        || entry.get("supported_reasoning_levels").is_some();
    if modalities.is_none() && !declares_reasoning {
        return None;
    }
    let mut capabilities = HashSet::from([ModelCapability::ToolUse]);
    // omp reads a missing modality list as text + image.
    if modalities.is_none_or(|list| {
        list.iter()
            .filter_map(non_empty)
            .any(|m| m.eq_ignore_ascii_case("image"))
    }) {
        capabilities.insert(ModelCapability::Vision);
    }
    if reasoning(entry) {
        capabilities.insert(ModelCapability::Thinking);
    }
    Some(capabilities)
}

fn parse(entry: &Value) -> Option<Entry<'_>> {
    if !entry.is_object() {
        return None;
    }
    let slug = non_empty(&entry["slug"]).or_else(|| non_empty(&entry["id"]))?;
    if non_empty(&entry["visibility"])
        .is_some_and(|v| v.eq_ignore_ascii_case("hide") || v.eq_ignore_ascii_case("hidden"))
    {
        return None;
    }
    Some(Entry {
        slug,
        name: non_empty(&entry["display_name"]).unwrap_or(slug),
        context: positive_int(&entry["context_window"]),
        capabilities: declared_capabilities(entry),
        priority: entry["priority"]
            .as_f64()
            .filter(|p| p.is_finite())
            .unwrap_or(NO_PRIORITY),
    })
}

/// Records for `{models|data: [...]}`; an object with neither key is an empty
/// roster (omp's `models ?? data ?? []`), and `None` means a non-object payload
/// or a non-array `models`/`data`. Bundled ids keep their bundled names
/// (`keep-bundled-names`).
fn records(payload: &Value, model_filter: &[String]) -> Option<Vec<ModelRecord>> {
    let object = payload.as_object()?;
    if ["models", "data"]
        .iter()
        .any(|key| object.get(*key).is_some_and(|value| !value.is_array()))
    {
        return None;
    }
    let entries: Vec<Entry<'_>> = object
        .get("models")
        .or_else(|| object.get("data"))
        .and_then(Value::as_array)
        .map(|rows| rows.iter().filter_map(parse).collect())
        .unwrap_or_default();
    let bundled: HashSet<&str> = crate::adapters::model_catalog_cache::load_embedded_seed()
        .and_then(|seed| seed.providers.get(PROVIDER_KIND))
        .map(|entry| {
            entry
                .models
                .iter()
                .map(|m| m.descriptor.model_id.as_str())
                .collect()
        })
        .unwrap_or_default();
    let advertised: HashSet<&str> = entries.iter().map(|entry| entry.slug).collect();
    let record = |id: &str, canonical: &str, entry: &Entry<'_>| ModelRecord {
        model_id: id.to_owned(),
        display_name: (!bundled.contains(id)).then(|| entry.name.to_owned()),
        // Floors live in the record: a later positive context wins the fold.
        context_window: if FLOORED.contains(&canonical) {
            Some(
                entry
                    .context
                    .map_or(FLOOR_CONTEXT, |c| c.max(FLOOR_CONTEXT)),
            )
        } else {
            entry.context
        },
        capabilities: entry.capabilities.clone(),
        pricing_tier: None,
    };
    let mut ranked = Vec::with_capacity(entries.len());
    for entry in &entries {
        let canonical = entry
            .slug
            .strip_suffix(WORKER_SUFFIX)
            .filter(|plain| !plain.is_empty() && bundled.contains(plain))
            .unwrap_or(entry.slug);
        ranked.push((entry.priority, record(entry.slug, canonical, entry)));
        if canonical != entry.slug && !advertised.contains(canonical) {
            ranked.push((entry.priority, record(canonical, canonical, entry)));
        }
    }
    ranked.sort_by(|(a, left), (b, right)| {
        a.total_cmp(b)
            .then_with(|| left.model_id.cmp(&right.model_id))
    });
    let filter = compile_filter_patterns(model_filter);
    Some(
        ranked
            .into_iter()
            .map(|(_, record)| record)
            .filter(|record| matches_any(&filter, &record.model_id))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn omitted_contexts_follow_omp_defaults() {
        assert_eq!(default_context_window("gpt-5.6-luna"), 1_000_000);
        assert_eq!(default_context_window("gpt-5.6-mini"), 372_000);
        assert_eq!(default_context_window("gpt-5.6"), 372_000);
        assert_eq!(default_context_window("gpt-5.60"), 272_000);
        assert_eq!(default_context_window("gpt-7-anything"), 272_000);
    }

    #[test]
    fn rows_are_ranked_named_floored_and_worker_variants_synthesised() {
        let payload = json!({"models":[
            {"slug":"zz-new","display_name":"ZZ New","priority":3},
            {"slug":"gpt-5.6-luna","display_name":"GPT-5.6-Luna","context_window":272000,"priority":1},
            {"slug":"hidden-one","visibility":"Hidden","priority":0},
            {"slug":"gpt-6.1-sol-wm","display_name":"Worker","priority":2},
            {"id":"  ","slug":7},
            "not-an-object"
        ]});
        let records = records(&payload, &[]).unwrap();
        let ids: Vec<_> = records.iter().map(|r| r.model_id.as_str()).collect();
        assert_eq!(
            ids,
            ["gpt-5.6-luna", "gpt-6.1-sol", "gpt-6.1-sol-wm", "zz-new"]
        );
        assert_eq!(records[0].display_name, None, "bundled names are kept");
        assert_eq!(records[0].context_window, Some(1_000_000));
        assert_eq!(records[1].display_name, None);
        assert_eq!(records[2].display_name.as_deref(), Some("Worker"));
        assert_eq!(records[3].display_name.as_deref(), Some("ZZ New"));
        assert_eq!(records[3].context_window, None);
        assert!(
            records
                .iter()
                .all(|r| r.capabilities.is_none() && r.pricing_tier.is_none())
        );
    }

    #[test]
    fn declared_capabilities_and_filters_and_shapes() {
        let payload = json!({"data":[
            {"slug":"a","input_modalities":["text"],"default_reasoning_level":"none"},
            {"slug":"b","supported_reasoning_levels":[{"effort":"high"}]}
        ]});
        let records = records(&payload, &["a".into()]).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].capabilities,
            Some(HashSet::from([ModelCapability::ToolUse]))
        );
        let records = super::records(&payload, &[]).unwrap();
        assert_eq!(
            records[1].capabilities,
            Some(HashSet::from([
                ModelCapability::ToolUse,
                ModelCapability::Vision,
                ModelCapability::Thinking
            ]))
        );
        assert!(super::records(&json!({"models":{}}), &[]).is_none());
        assert!(super::records(&json!([]), &[]).is_none());
        assert_eq!(super::records(&json!({}), &[]).unwrap().len(), 0);
    }
}
