//! Live xAI model discovery, adapted from oh-my-pi's generic OpenAI-compatible
//! catalog fetcher (`catalog/src/discovery/openai-compatible.ts`), its bundled
//! reference merge and the xAI OAuth curation rules.
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

use super::{Inner, profile};
use crate::adapters::model_catalog_cache::request_scope;
use crate::adapters::model_filter::{compile_filter_patterns, matches_any};
use crate::domain::errors::ProviderError;
use crate::domain::models::provider::{DiscoveredModels, ModelRecord};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

/// omp's window for an id no catalogue layer describes.
const DEFAULT_CONTEXT: u32 = 128_000;
/// Image, speech and voice models are never chat models, for either kind.
const EXCLUDED: [&str; 4] = [
    "grok-imagine-image",
    "grok-imagine-image-quality",
    "grok-imagine-image-2.0",
    "grok-tts",
];
const EXCLUDED_PREFIXES: [&str; 3] = ["grok-imagine-video", "grok-stt-", "grok-voice-"];
/// Envelope keys in omp's lookup order.
const ENVELOPE_KEYS: [&str; 4] = ["data", "models", "result", "items"];

pub(super) fn default_context_window(_id: &str) -> u32 {
    DEFAULT_CONTEXT
}

/// omp `isExcludedModel` matches the lowercased id.
fn excluded(id: &str) -> bool {
    let id = id.to_ascii_lowercase();
    EXCLUDED.contains(&id.as_str()) || EXCLUDED_PREFIXES.iter().any(|p| id.starts_with(p))
}

impl Inner {
    /// `GET <base>/models`. A stored row's `401` recovers once; any remaining
    /// `401`/`403` is final. The scope is that of the credential actually sent.
    pub(super) async fn discover(
        &self,
        model_filter: &[String],
        scope_context: &str,
    ) -> Result<DiscoveredModels, ProviderError> {
        let mut resolved = self.handle.current().await?;
        let mut auth_retried = false;
        let url = format!("{}/models", self.base());
        let response = loop {
            let mut headers = profile::auth_headers(&resolved.auth)?;
            headers.insert(
                reqwest::header::ACCEPT,
                reqwest::header::HeaderValue::from_static("application/json"),
            );
            let response = self
                .client
                .get(&url)
                .headers(headers)
                .timeout(Duration::from_secs(10))
                .send()
                .await
                .map_err(|e| crate::adapters::provider::classify_reqwest_error(&e))?;
            if response.status().as_u16() == 401
                && self.recover(&mut resolved, &mut auth_retried).await?
            {
                continue;
            }
            break response;
        };
        let status = response.status().as_u16();
        match status {
            200..=299 => {}
            401 | 403 => return Err(ProviderError::AuthenticationFailed),
            404 | 405 => return Err(ProviderError::EndpointUnsupported(status)),
            _ => return Err(ProviderError::Other(format!("xAI models HTTP {status}"))),
        }
        let body = response
            .bytes()
            .await
            .map_err(|e| crate::adapters::provider::classify_reqwest_error(&e))?;
        let records = serde_json::from_slice::<Value>(&body)
            .ok()
            .and_then(|payload| records(self.kind.id(), &payload, model_filter))
            .ok_or_else(|| ProviderError::Other("Could not parse xAI models response".into()))?;
        Ok(DiscoveredModels {
            records,
            credential_scope: request_scope(scope_context, &resolved),
        })
    }
}

struct Live<'a> {
    name: Option<&'a str>,
    context: Option<u32>,
}

/// omp's `toNumber` (finite number or numeric string), kept when positive.
fn positive_int(value: &Value) -> Option<u32> {
    let number = match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) if !s.trim().is_empty() => s.trim().parse::<f64>().ok(),
        _ => None,
    }?;
    (number.is_finite() && number >= 1.0).then(|| number.trunc().min(f64::from(u32::MAX)) as u32)
}

/// One `{id, name?}` row: a non-empty string id and a string or null name.
fn live_row(row: &Value) -> Option<(&str, Live<'_>)> {
    let object = row.as_object()?;
    let id = object.get("id")?.as_str().filter(|id| !id.is_empty())?;
    let name = match object.get("name") {
        None | Some(Value::Null) => None,
        Some(Value::String(name)) => Some(name.trim()).filter(|name| !name.is_empty()),
        Some(_) => return None,
    };
    let context = object.get("context_length").and_then(positive_int);
    Some((id, Live { name, context }))
}

/// An array of rows, or an object whose first usable `data`/`models`/`result`/
/// `items` member (recursively) holds them; anything else is unparseable.
fn live_rows(node: &Value) -> Option<Vec<(&str, Live<'_>)>> {
    match node {
        Value::Array(rows) => Some(rows.iter().filter_map(live_row).collect()),
        Value::Object(object) => ENVELOPE_KEYS
            .iter()
            .filter_map(|key| object.get(*key))
            .find_map(live_rows),
        _ => None,
    }
}

/// omp's non-authoritative union expressed as an authoritative roster: every
/// seed id in seed order (fields left to the seed), then the live ids the seed
/// lacks, deduplicated (last row wins) and sorted by id, media ids excluded.
fn records(kind: &str, payload: &Value, model_filter: &[String]) -> Option<Vec<ModelRecord>> {
    let live: BTreeMap<&str, Live<'_>> = live_rows(payload)?.into_iter().collect();
    let seed: Vec<&str> = crate::adapters::model_catalog_cache::load_embedded_seed()
        .and_then(|seed| seed.providers.get(kind))
        .map(|entry| {
            entry
                .models
                .iter()
                .map(|m| m.descriptor.model_id.as_str())
                .collect()
        })
        .unwrap_or_default();
    let seeded: HashSet<&str> = seed.iter().copied().collect();
    let filter = compile_filter_patterns(model_filter);
    let bundled = seed.iter().map(|id| ModelRecord {
        model_id: (*id).to_owned(),
        ..Default::default()
    });
    let discovered = live
        .into_iter()
        .filter(|(id, _)| !seeded.contains(id) && !excluded(id))
        .map(|(id, row)| ModelRecord {
            model_id: id.to_owned(),
            display_name: row.name.map(str::to_owned),
            context_window: row.context,
            capabilities: None,
            pricing_tier: None,
            wire_model_id: None,
        });
    Some(
        bundled
            .chain(discovered)
            .filter(|record| matches_any(&filter, &record.model_id))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ids(records: &[ModelRecord]) -> Vec<&str> {
        records.iter().map(|r| r.model_id.as_str()).collect()
    }

    fn seed(kind: &str) -> Vec<String> {
        crate::adapters::model_catalog_cache::load_embedded_seed()
            .and_then(|seed| seed.providers.get(kind))
            .map(|entry| {
                entry
                    .models
                    .iter()
                    .map(|m| m.descriptor.model_id.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn unknown_ids_take_omp_default_window() {
        assert_eq!(default_context_window("grok-9"), 128_000);
    }

    #[test]
    fn exclusions_cover_media_and_voice_ids_only() {
        for id in [
            "grok-imagine-image",
            "grok-imagine-image-quality",
            "grok-imagine-image-2.0",
            "grok-tts",
            "grok-imagine-video",
            "grok-imagine-video-1",
            "grok-stt-1",
            "grok-voice-mini",
            "GROK-TTS",
            "Grok-Imagine-Video-1",
        ] {
            assert!(excluded(id), "{id}");
        }
        for id in ["grok-4.6", "grok-imagine-image-3", "grok-tts-2", "grok-stt"] {
            assert!(!excluded(id), "{id}");
        }
    }

    #[test]
    fn union_is_seed_order_then_sorted_new_ids() {
        for kind in ["xai", "xai-oauth"] {
            let payload = json!({"data":[
                {"id":"grok-zz","name":"  Grok ZZ ","context_length":"256000"},
                {"id":"grok-4.6","name":"Live Name","context_length":1},
                {"id":"grok-imagine-video-1"},
                {"id":"grok-stt-1"},
                {"id":"grok-tts"},
                {"id":"grok-9","name":null},
                {"id":"grok-9","name":"Grok 9","context_length":0},
                {"id":"","name":"blank"},
                {"id":"grok-bad-name","name":7},
                "not-a-row"
            ]});
            let records = records(kind, &payload, &[]).unwrap();
            let seed = seed(kind);
            assert!(!seed.is_empty(), "{kind} seed rows");
            let mut expected: Vec<&str> = seed.iter().map(String::as_str).collect();
            expected.extend(["grok-9", "grok-zz"]);
            assert_eq!(ids(&records), expected, "{kind}");
            let bundled = &records[..seed.len()];
            assert!(bundled.iter().all(|r| r.display_name.is_none()
                && r.context_window.is_none()
                && r.capabilities.is_none()
                && r.pricing_tier.is_none()));
            let grok9 = &records[seed.len()];
            assert_eq!(grok9.display_name.as_deref(), Some("Grok 9"));
            assert_eq!(grok9.context_window, None);
            let zz = &records[seed.len() + 1];
            assert_eq!(zz.display_name.as_deref(), Some("Grok ZZ"));
            assert_eq!(zz.context_window, Some(256_000));
            assert!(zz.capabilities.is_none());
        }
    }

    #[test]
    fn envelopes_follow_omp_lookup() {
        let seeded = seed("xai").len();
        let count = |payload: Value| records("xai", &payload, &[]).map(|r| r.len() - seeded);
        assert_eq!(count(json!([{"id":"grok-a"}])), Some(1));
        assert_eq!(count(json!({"models":[{"id":"grok-a"}]})), Some(1));
        assert_eq!(
            count(json!({"result":{"items":[{"id":"grok-a"}]}})),
            Some(1)
        );
        // An unusable member falls through to the next key.
        assert_eq!(
            count(json!({"data":"x","items":[{"id":"grok-a"},{"id":"grok-b"}]})),
            Some(2)
        );
        assert_eq!(count(json!({"data":[]})), Some(0));
        assert_eq!(count(json!({})), None);
        assert_eq!(count(json!({"data":null})), None);
        assert_eq!(count(json!("grok-a")), None);
        assert_eq!(count(json!(null)), None);
    }

    #[test]
    fn model_filter_applies_to_the_whole_roster() {
        let payload = json!([{"id":"grok-9"},{"id":"other"}]);
        let records =
            records("xai-oauth", &payload, &["grok-4.6".into(), "grok-9".into()]).unwrap();
        assert_eq!(ids(&records), ["grok-4.6", "grok-9"]);
    }
}
