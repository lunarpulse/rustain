//! Live Cloud Code Assist (Gemini CLI) model discovery, adapted from oh-my-pi
//! (catalog/src/provider-models/google.ts `googleGeminiCliModelManagerOptions`,
//! catalog/src/discovery/antigravity.ts, catalog/src/discovery/gemini-cli.ts,
//! catalog/src/model-manager.ts `mergeDynamicModels`, catalog/src/wire/gemini-headers.ts,
//! catalog/src/compat/taxonomy.ts and catalog/src/compat/revision.ts).
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

// Not replicated: Antigravity version-manifest discovery and the
// `PI_AI_ANTIGRAVITY_*` overrides; the user agent is pinned at omp's fallback
// version. Antigravity per-account `accountAccess` tagging belongs to the
// `google-antigravity` provider.

use super::collapse::{self, Spec};
use super::{Inner, LOAD_CODE_ASSIST_BODY, bearer, headers, profile, project};
use crate::adapters::auth::{gemini_cli_project::DEFAULT_HEADER_MODEL, policies};
use crate::adapters::model_catalog_cache::{CachedModelEntry, request_scope};
use crate::adapters::model_filter::{compile_filter_patterns, matches_any};
use crate::adapters::provider::classify_reqwest_error;
use crate::adapters::responses::transport::recover_once;
use crate::domain::errors::ProviderError;
use crate::domain::models::provider::{DiscoveredModels, ModelCapability, ModelRecord};
use crate::infrastructure::key_resolver::Resolved;
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::{Map, Value};
use std::collections::HashSet;
use std::time::Duration;

/// AG:8-10 `DEFAULT_ANTIGRAVITY_DISCOVERY_ENDPOINTS`, tried in order; fixed.
pub(super) const ANTIGRAVITY_ENDPOINTS: [&str; 2] = [
    "https://daily-cloudcode-pa.googleapis.com",
    "https://daily-cloudcode-pa.sandbox.googleapis.com",
];
const FETCH_AVAILABLE_MODELS: &str = "/v1internal:fetchAvailableModels";
const RETRIEVE_USER_QUOTA: &str = "/v1internal:retrieveUserQuota";
const LOAD_CODE_ASSIST: &str = "/v1internal:loadCodeAssist";
/// gemini-headers.ts:110-120 with `DEFAULT_ANTIGRAVITY_VERSION` (pinned).
pub(super) const ANTIGRAVITY_USER_AGENT: &str =
    "antigravity/hub/2.19.1 (aidev_client; os_type=darwin; arch=arm64; cl=963137146)";
/// AG:15.
const DENYLIST: [&str; 3] = ["chat_20706", "chat_23310", "gemini-2.5-pro"];
/// AG:13 `DEFAULT_CONTEXT_WINDOW`.
const ANTIGRAVITY_CONTEXT: u32 = 200_000;
/// D:18 `DEFAULT_CONTEXT_WINDOW`.
const QUOTA_CONTEXT: u32 = 1_048_576;
/// D:22 `REASONING_MIN_VERSION`.
const REASONING_MIN_REVISION: (u32, u32, u32) = (2, 5, 0);
/// model-manager.ts:770 keeps a larger bundled limit over a discovered 4096.
const SUSPECT_LIMIT: u32 = 4096;
/// Per request, on the adapter's no-redirect client.
const TIMEOUT: Duration = Duration::from_secs(10);
const QUOTA_FAILED: &str = "Code Assist quota discovery failed";
const QUOTA_INVALID: &str = "Code Assist quota discovery returned invalid JSON";

/// Which discovery answered; only Antigravity rows can update bundled fields.
enum Roster {
    Antigravity(Vec<Spec>),
    Quota(Vec<Spec>),
}

impl Inner {
    /// PM:162-187: Antigravity's roster, else (any failure, 401/403 included) the
    /// account's quota buckets. Bearer and project come from one resolved grant;
    /// the scope is that of the grant the answering request sent.
    pub(super) async fn discover(
        &self,
        model_filter: &[String],
        scope_context: &str,
    ) -> Result<DiscoveredModels, ProviderError> {
        let mut resolved = self.handle.current().await?;
        let roster = match self.antigravity(&resolved).await? {
            Some(specs) => Roster::Antigravity(specs),
            None => Roster::Quota(self.quota(&mut resolved).await?),
        };
        Ok(DiscoveredModels {
            records: records(self.seed(), roster, model_filter),
            credential_scope: request_scope(scope_context, &resolved),
        })
    }

    /// AG:181-235,257-310: `POST {}` to each Antigravity host in order; the first
    /// parsed payload wins. A network error, any non-2xx and an unusable payload
    /// move on; `None` takes the quota fallback. A 401/403 here is never a
    /// credential verdict.
    async fn antigravity(&self, resolved: &Resolved) -> Result<Option<Vec<Spec>>, ProviderError> {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", bearer(&resolved.auth)?);
        headers.insert("content-type", HeaderValue::from_static("application/json"));
        headers.insert(
            "user-agent",
            HeaderValue::from_static(ANTIGRAVITY_USER_AGENT),
        );
        for endpoint in ANTIGRAVITY_ENDPOINTS {
            let url = policies::endpoint(&format!("{endpoint}{FETCH_AVAILABLE_MODELS}"));
            let sent = self
                .client
                .post(&url)
                .headers(headers.clone())
                .body("{}")
                .timeout(TIMEOUT)
                .send()
                .await;
            let Ok(response) = sent else { continue };
            if !response.status().is_success() {
                continue;
            }
            let Ok(body) = response.bytes().await else {
                continue;
            };
            if let Some(specs) = serde_json::from_slice::<Value>(&body)
                .ok()
                .as_ref()
                .and_then(antigravity_specs)
            {
                return Ok(Some(specs));
            }
        }
        Ok(None)
    }

    /// D:83-166: `POST <base>/v1internal:retrieveUserQuota` with the Gemini CLI
    /// headers and the sent grant's project. A stored row's 401 recovers once and
    /// resends with the refreshed grant (bearer and project); a remaining 401/403
    /// is final.
    async fn quota(&self, resolved: &mut Resolved) -> Result<Vec<Spec>, ProviderError> {
        let url = format!("{}{RETRIEVE_USER_QUOTA}", self.base);
        let mut retried = false;
        loop {
            let headers = headers(&resolved.auth, DEFAULT_HEADER_MODEL, false)?;
            let project = match project(resolved) {
                Ok(project) => Some(project.to_owned()),
                Err(_) => self.load_project(&headers).await,
            };
            let response = self
                .client
                .post(&url)
                .headers(headers)
                .body(quota_body(project.as_deref()))
                .timeout(TIMEOUT)
                .send()
                .await
                .map_err(|e| classify_reqwest_error(&e))?;
            let status = response.status().as_u16();
            if status == 401 && recover_once(&self.handle, resolved, &mut retried).await? {
                continue;
            }
            match status {
                200..=299 => {}
                401 | 403 => return Err(ProviderError::AuthenticationFailed),
                _ => return Err(ProviderError::Other(format!("{QUOTA_FAILED}: {status}"))),
            }
            let body = response
                .bytes()
                .await
                .map_err(|e| classify_reqwest_error(&e))?;
            return serde_json::from_slice::<Value>(&body)
                .ok()
                .as_ref()
                .and_then(|payload| quota_specs(payload, self.seed()))
                .ok_or_else(|| ProviderError::Other(QUOTA_INVALID.into()));
        }
    }

    /// D:168-201: a project-less `loadCodeAssist` supplies the project of a grant
    /// without one; any failure leaves the quota body empty.
    async fn load_project(&self, headers: &HeaderMap) -> Option<String> {
        let response = self
            .client
            .post(format!("{}{LOAD_CODE_ASSIST}", self.base))
            .headers(headers.clone())
            .body(LOAD_CODE_ASSIST_BODY)
            .timeout(TIMEOUT)
            .send()
            .await
            .ok()
            .filter(|response| response.status().is_success())?;
        let payload: Value = serde_json::from_slice(&response.bytes().await.ok()?).ok()?;
        match payload.as_object()?.get("cloudaicompanionProject")? {
            Value::String(project) => Some(project.clone()),
            Value::Object(project) => project.get("id")?.as_str().map(str::to_owned),
            _ => None,
        }
    }
}

/// `JSON.stringify(projectId ? { project: projectId } : {})`.
fn quota_body(project: Option<&str>) -> String {
    match project.filter(|project| !project.is_empty()) {
        Some(project) => serde_json::json!({ "project": project }).to_string(),
        None => "{}".to_owned(),
    }
}

fn flag(model: &Map<String, Value>, key: &str) -> bool {
    model.get(key) == Some(&Value::Bool(true))
}

/// omp `toPositiveNumber` over a finite number field, saturated to `u32`.
fn positive(value: Option<&Value>) -> Option<u32> {
    value
        .and_then(Value::as_f64)
        .filter(|number| number.is_finite() && *number > 0.0)
        .map(|number| number.min(f64::from(u32::MAX)) as u32)
}

/// taxonomy.ts `bounded "gemini"`: the bare lowercased id is `gemini` or starts
/// with it followed by `-`, `_`, `.`, `:` or a digit.
fn is_gemini_class(id: &str) -> bool {
    let lower = id.trim().to_ascii_lowercase();
    let bare = lower.rsplit('/').next().unwrap_or_default();
    bare.strip_prefix("gemini").is_some_and(|rest| {
        rest.bytes()
            .next()
            .is_none_or(|b| matches!(b, b'-' | b'_' | b'.' | b':' | b'0'..=b'9'))
    })
}

/// revision.ts `parseComponent`: non-empty digits, at most 255.
fn component(digits: &str) -> Option<u32> {
    (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| digits.parse().ok())
        .flatten()
        .filter(|value| *value <= 255)
}

/// revision.ts `parseRevisionPrefix`: up to three components separated by `.`
/// or by `-` before a digit; a component running into a letter is a size token.
fn revision_prefix(value: &str) -> Option<(u32, u32, u32)> {
    let bytes = value.as_bytes();
    let mut out = [0_u32; 3];
    let mut count = 0;
    let mut index = 0;
    while count < 3 {
        let start = index;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        let size_token = bytes.get(index).is_some_and(u8::is_ascii_alphabetic);
        let Some(parsed) = (!size_token)
            .then(|| component(&value[start..index]))
            .flatten()
        else {
            return (count > 0).then_some((out[0], out[1], out[2]));
        };
        out[count] = parsed;
        count += 1;
        let Some(&separator) = bytes.get(index) else {
            break;
        };
        if !matches!(separator, b'.' | b'-')
            || !bytes.get(index + 1).is_some_and(u8::is_ascii_digit)
        {
            break;
        }
        index += 1;
    }
    Some((out[0], out[1], out[2]))
}

/// D:143-152: `reasoning` of a quota id the bundle lacks — the gemini class
/// revision (taxonomy.ts `revision prefix="gemini-"`) is at least 2.5.
fn quota_reasoning(id: &str) -> bool {
    let lower = id.trim().to_ascii_lowercase();
    let bare = lower.rsplit('/').next().unwrap_or_default();
    bare.strip_prefix("gemini-")
        .and_then(|tail| {
            tail.find(|c: char| c.is_ascii_digit())
                .map(|at| &tail[at..])
        })
        .and_then(revision_prefix)
        .is_some_and(|revision| revision >= REASONING_MIN_REVISION)
}

/// AG:106-139,188-234 plus PM:180-181: rows of a `fetchAvailableModels` object,
/// denylisted, internal and disabled rows dropped, collapsed with the reviewed
/// table only, sorted by name then id, gemini class only. `None` when the
/// payload is not an object.
fn antigravity_specs(payload: &Value) -> Option<Vec<Spec>> {
    let object = payload.as_object()?;
    let mut specs = Vec::new();
    if let Some(Value::Object(models)) = object.get("models") {
        for (id, model) in models {
            let Value::Object(model) = model else {
                continue;
            };
            if DENYLIST.contains(&id.as_str())
                || flag(model, "isInternal")
                || flag(model, "disabled")
            {
                continue;
            }
            let name = model
                .get("displayName")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .unwrap_or(id);
            specs.push(Spec {
                id: id.clone(),
                name: name.to_owned(),
                reasoning: flag(model, "supportsThinking"),
                image: flag(model, "supportsImages"),
                context: positive(model.get("maxTokens")).unwrap_or(ANTIGRAVITY_CONTEXT),
                collapsed: false,
                wire: None,
            });
        }
    }
    let mut specs = collapse::reviewed_only(&specs);
    collapse::sort(&mut specs);
    specs.retain(|spec| is_gemini_class(&spec.id));
    Some(specs)
}

/// D:119-165: deduplicated trimmed gemini-class `buckets[].modelId`; a bundled
/// id is the bundled row unchanged, any other id gets Gemini CLI defaults; then
/// the reviewed table and derived pairs, sorted. `None` when the payload is not
/// an object.
fn quota_specs(payload: &Value, seed: &[CachedModelEntry]) -> Option<Vec<Spec>> {
    let buckets = match payload.as_object()?.get("buckets") {
        Some(Value::Array(buckets)) => buckets.as_slice(),
        _ => &[],
    };
    let mut seen = HashSet::new();
    let mut specs = Vec::new();
    for bucket in buckets {
        let Some(id) = bucket
            .as_object()
            .and_then(|bucket| bucket.get("modelId"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
        else {
            continue;
        };
        if seen.contains(id) || !is_gemini_class(id) {
            continue;
        }
        seen.insert(id);
        let spec = match seed.iter().find(|entry| entry.descriptor.model_id == id) {
            Some(entry) => {
                let bundled = &entry.descriptor;
                Spec {
                    id: id.to_owned(),
                    name: bundled.display_name.clone(),
                    reasoning: bundled.capabilities.contains(&ModelCapability::Thinking),
                    image: bundled.capabilities.contains(&ModelCapability::Vision),
                    context: bundled.context_window,
                    // The bundled `gemini-2.5-flash` row is itself a collapsed family.
                    collapsed: profile::for_model(id).routes.is_some(),
                    wire: None,
                }
            }
            None => Spec {
                id: id.to_owned(),
                name: id.to_owned(),
                reasoning: quota_reasoning(id),
                image: true,
                context: QUOTA_CONTEXT,
                collapsed: false,
                wire: None,
            },
        };
        specs.push(spec);
    }
    let mut specs = collapse::reviewed_and_pairs(&specs);
    collapse::sort(&mut specs);
    Some(specs)
}

fn capabilities(spec: &Spec) -> HashSet<ModelCapability> {
    let mut capabilities = HashSet::from([ModelCapability::ToolUse]);
    if spec.image {
        capabilities.insert(ModelCapability::Vision);
    }
    if spec.reasoning {
        capabilities.insert(ModelCapability::Thinking);
    }
    capabilities
}

/// omp's non-authoritative merge (model-manager.ts:605-632) as an authoritative
/// roster for 20-1c's merge: every bundled id in bundled order, then each new id
/// in discovery order. A bundled id carries only the fields omp's
/// `mergeDynamicModel` takes from an Antigravity row — the trimmed name unless
/// empty (equal-to-id names are resolved by the fold), the window unless a
/// suspect 4096 below the bundled one, and image/thinking added — and nothing
/// from a quota row, which is the bundled row itself. A new collapsed row names
/// its default wire id (omp `requestModelId`, or a derived pair's bare id), which
/// the profile reads back.
fn records(seed: &[CachedModelEntry], roster: Roster, model_filter: &[String]) -> Vec<ModelRecord> {
    let (specs, live_bundled_fields) = match roster {
        Roster::Antigravity(specs) => (specs, true),
        Roster::Quota(specs) => (specs, false),
    };
    let seeded: HashSet<&str> = seed
        .iter()
        .map(|entry| entry.descriptor.model_id.as_str())
        .collect();
    let bundled = seed.iter().map(|entry| {
        let bundled = &entry.descriptor;
        let live = specs
            .iter()
            .find(|spec| live_bundled_fields && spec.id == bundled.model_id);
        match live {
            None => ModelRecord {
                model_id: bundled.model_id.clone(),
                ..Default::default()
            },
            Some(spec) => ModelRecord {
                model_id: bundled.model_id.clone(),
                display_name: Some(spec.name.trim())
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned),
                context_window: (spec.context != SUSPECT_LIMIT
                    || bundled.context_window <= SUSPECT_LIMIT)
                    .then_some(spec.context),
                capabilities: Some(capabilities(spec)),
                pricing_tier: None,
                wire_model_id: None,
            },
        }
    });
    let discovered = specs
        .iter()
        .filter(|spec| !seeded.contains(spec.id.as_str()))
        .map(|spec| ModelRecord {
            model_id: spec.id.clone(),
            display_name: Some(spec.name.clone()),
            context_window: Some(spec.context),
            capabilities: Some(capabilities(spec)),
            pricing_tier: None,
            wire_model_id: spec.wire.clone(),
        });
    let filter = compile_filter_patterns(model_filter);
    bundled
        .chain(discovered)
        .filter(|record| matches_any(&filter, &record.model_id))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn seed() -> &'static [CachedModelEntry] {
        crate::adapters::model_catalog_cache::load_embedded_seed()
            .and_then(|seed| seed.providers.get(super::super::PROVIDER_KIND))
            .map(|entry| entry.models.as_slice())
            .unwrap_or_default()
    }

    fn ids(specs: &[Spec]) -> Vec<&str> {
        specs.iter().map(|s| s.id.as_str()).collect()
    }

    #[test]
    fn endpoints_and_user_agent_are_omps() {
        assert_eq!(
            ANTIGRAVITY_ENDPOINTS,
            [
                "https://daily-cloudcode-pa.googleapis.com",
                "https://daily-cloudcode-pa.sandbox.googleapis.com"
            ]
        );
        assert_eq!(
            ANTIGRAVITY_USER_AGENT,
            "antigravity/hub/2.19.1 (aidev_client; os_type=darwin; arch=arm64; cl=963137146)"
        );
        assert_eq!(quota_body(Some("p-1")), r#"{"project":"p-1"}"#);
        assert_eq!(quota_body(Some("")), "{}");
        assert_eq!(quota_body(None), "{}");
    }

    #[test]
    fn gemini_class_and_quota_reasoning() {
        for id in [
            "gemini",
            "gemini-x",
            "Gemini-2.5-pro",
            "models/gemini-3",
            "gemini2",
        ] {
            assert!(is_gemini_class(id), "{id}");
        }
        for id in ["geminix", "chat_20706", "gpt-oss-120b", "claude-gemini-x"] {
            assert!(!is_gemini_class(id), "{id}");
        }
        for (id, reasoning) in [
            ("gemini-2.5-flash-lite", true),
            ("gemini-2.4-pro", false),
            ("gemini-3-pro", true),
            ("gemini-exp-1206", false),
            ("gemini-2-5-pro", true),
            ("gemini-pro", false),
            ("gemini-70b-2.5", false),
        ] {
            assert_eq!(quota_reasoning(id), reasoning, "{id}");
        }
    }

    #[test]
    fn antigravity_rows_map_and_filter_like_omp() {
        let payload = json!({"models": {
            "gemini-new": {"displayName": "Gemini New", "supportsThinking": true, "supportsImages": true, "maxTokens": 500000, "maxOutputTokens": 1},
            "gemini-plain": {"displayName": "", "maxTokens": 0},
            "gemini-str": {"displayName": 7, "supportsImages": "yes", "maxTokens": "9"},
            "gemini-2.5-pro": {},
            "chat_20706": {},
            "gemini-internal": {"isInternal": true},
            "gemini-off": {"disabled": true},
            "claude-x": {"displayName": "A Claude"},
            "gemini-bad": 3,
        }});
        let specs = antigravity_specs(&payload).unwrap();
        assert_eq!(ids(&specs), ["gemini-new", "gemini-plain", "gemini-str"]);
        assert_eq!(
            specs[0],
            Spec {
                id: "gemini-new".into(),
                name: "Gemini New".into(),
                reasoning: true,
                image: true,
                context: 500_000,
                collapsed: false,
                wire: None,
            }
        );
        assert_eq!(
            (specs[1].name.as_str(), specs[1].context, specs[1].image),
            ("gemini-plain", 200_000, false)
        );
        assert_eq!(
            (specs[2].name.as_str(), specs[2].context, specs[2].image),
            ("gemini-str", 200_000, false)
        );
        assert!(
            antigravity_specs(&json!({"models": []}))
                .unwrap()
                .is_empty()
        );
        assert!(antigravity_specs(&json!({})).unwrap().is_empty());
        for unusable in [json!([]), json!("x"), json!(null)] {
            assert!(antigravity_specs(&unusable).is_none());
        }
    }

    #[test]
    fn quota_buckets_dedupe_trim_and_keep_bundled_rows() {
        let payload = json!({"buckets": [
            {"modelId": "  gemini-9-pro "},
            {"modelId": "gemini-9-pro"},
            {"modelId": "gemini-3-pro-preview"},
            {"modelId": "gemini-9-pro-thinking"},
            {"modelId": "gpt-oss-120b-medium"},
            {"modelId": 4},
            "gemini-not-an-object",
            {"modelId": ""},
        ]});
        let specs = quota_specs(&payload, seed()).unwrap();
        assert_eq!(ids(&specs), ["gemini-3-pro-preview", "gemini-9-pro"]);
        assert_eq!(specs[0].name, "Gemini 3 Pro Preview");
        assert_eq!(specs[0].context, 1_000_000);
        assert!(specs[1].reasoning && specs[1].image);
        assert_eq!(specs[1].wire.as_deref(), Some("gemini-9-pro"));
        assert_eq!(specs[1].context, QUOTA_CONTEXT);
        assert!(quota_specs(&json!({}), seed()).unwrap().is_empty());
        assert!(
            quota_specs(&json!({"buckets": "x"}), seed())
                .unwrap()
                .is_empty()
        );
        assert!(quota_specs(&json!([]), seed()).is_none());
    }

    #[test]
    fn records_put_the_seed_first_and_follow_omps_bundled_merge() {
        let seed = seed();
        let spec = |id: &str, name: &str, context: u32| Spec {
            id: id.into(),
            name: name.into(),
            reasoning: false,
            image: false,
            context,
            collapsed: false,
            wire: None,
        };
        let live = vec![
            spec("gemini-3-pro-preview", " Live Pro ", SUSPECT_LIMIT),
            spec("gemini-2.0-flash", "", 7),
            spec("gemini-new", "Gemini New", 9),
        ];
        let antigravity = records(seed, Roster::Antigravity(live.clone()), &[]);
        let seed_ids: Vec<&str> = seed
            .iter()
            .map(|e| e.descriptor.model_id.as_str())
            .collect();
        let ids: Vec<&str> = antigravity.iter().map(|r| r.model_id.as_str()).collect();
        assert_eq!(ids[..seed_ids.len()], seed_ids[..]);
        assert_eq!(ids[seed_ids.len()..], ["gemini-new"]);
        let pro = antigravity
            .iter()
            .find(|r| r.model_id == "gemini-3-pro-preview")
            .unwrap();
        assert_eq!(pro.display_name.as_deref(), Some("Live Pro"));
        assert_eq!(
            pro.context_window, None,
            "a suspect 4096 keeps the bundled window"
        );
        assert_eq!(
            pro.capabilities,
            Some(HashSet::from([ModelCapability::ToolUse]))
        );
        let flash = antigravity
            .iter()
            .find(|r| r.model_id == "gemini-2.0-flash")
            .unwrap();
        assert_eq!(
            (flash.display_name.as_deref(), flash.context_window),
            (None, Some(7))
        );
        let untouched = antigravity
            .iter()
            .find(|r| r.model_id == "gemini-2.5-pro")
            .unwrap();
        assert!(
            untouched.display_name.is_none()
                && untouched.context_window.is_none()
                && untouched.capabilities.is_none()
        );
        let fresh = antigravity.last().unwrap();
        assert_eq!(fresh.display_name.as_deref(), Some("Gemini New"));
        assert_eq!(fresh.context_window, Some(9));
        assert_eq!(fresh.wire_model_id, None);

        // A quota row of a bundled id is the bundled row: no field changes.
        let quota = records(seed, Roster::Quota(live), &["gemini-3*".into()]);
        assert!(quota.iter().all(|r| r.model_id.starts_with("gemini-3")));
        assert!(
            quota
                .iter()
                .all(|r| r.display_name.is_none() && r.capabilities.is_none())
        );

        let mut family = spec("gemini-3.1-pro", "Gemini 3.1 Pro", 1);
        family.wire = Some("gemini-pro-agent".into());
        let mut pair = spec("gemini-9-pro", "gemini-9-pro", 1);
        pair.wire = Some("gemini-9-pro".into());
        let records = records(seed, Roster::Quota(vec![family, pair]), &[]);
        let wires: Vec<_> = records[seed_ids.len()..]
            .iter()
            .map(|r| r.wire_model_id.as_deref())
            .collect();
        assert_eq!(wires, [Some("gemini-pro-agent"), Some("gemini-9-pro")]);
    }
}
