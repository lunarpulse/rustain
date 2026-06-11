//! `RemoteEmbeddingProvider` — an OpenAI-compatible `/embeddings` HTTP client
//! (Story 11.3b, AC1 / AC-11-3b-GATE).
//!
//! The SECOND impl of the [`EmbeddingProvider`] seam (the first being
//! [`super::embedding_local::LocalEmbeddingProvider`]). The index and search
//! code depend ONLY on the trait, so wiring this in required ZERO change to the
//! vector math or the index codec — that is the whole point of the 11.3a seam.
//!
//! ## No vendor names in the type (architecture.md:174)
//! The type is `RemoteEmbeddingProvider`, NOT `OpenRouterEmbeddings`. Vendor
//! strings (`"openrouter"`, `"deepinfra"`, …) live ONLY in config and resolve to
//! a `base_url` (see [`super::provider_defaults`]). This one client talks to
//! OpenRouter, DeepInfra, Together, OpenAI, Voyage, or any OpenAI-compatible
//! `/embeddings` endpoint — switching host is a config change, not a code change
//! (AC-11-3b-GATE fallback requirement).
//!
//! ## Mirrors the existing OpenAI chat client (`adapters/openai/mod.rs`)
//! Construction, `Bearer` auth, control-char rejection, HTTP status mapping, and
//! the masked `Debug` impl all mirror the shipped OpenAI adapter — this is a
//! separate, simpler client (POSTs `/embeddings`, no streaming).
//!
//! ## Secrets discipline
//! The API key comes from an env var (resolved by the caller), is masked in
//! `Debug` as `"(***)"`, and is sent only as a `Bearer` header — it must never
//! reach logs, `SystemNotice`s, or `index.bin`.

use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::infrastructure::utils::normalize_base_url;

use super::{EmbeddingError, EmbeddingProvider, ProbeReport, ProviderKind};

/// Per-`embed` request timeout. Matches the OpenAI adapter's posture (a batched
/// embed of the whole corpus can be large; 30s is generous but bounded).
const EMBED_TIMEOUT: Duration = Duration::from_secs(30);
/// Per-`probe` request timeout. A truly dead host fails fast on connect; this
/// read budget must still accommodate a HEAVY model's cold start — an 8B
/// embedding model (e.g. `qwen/qwen3-embedding-8b`) can take >5s on its first
/// request, so a 5s probe budget produced false-negative GATE failures
/// (timeout surfaced as "error decoding response body"). 15s is bounded but
/// generous enough to verify heavy models; the hot `embed` path keeps 30s.
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// OpenAI-compatible remote embedding provider. One `reqwest::Client`, reused.
pub struct RemoteEmbeddingProvider {
    client: reqwest::Client,
    /// Normalized (no trailing slash); the request URL is `{base_url}/embeddings`.
    base_url: String,
    /// Bearer token. Empty means "send no `authorization` header" (some self-
    /// hosted OpenAI-compatible endpoints need none); a remote host will then
    /// reject with 401 → `NotReady`.
    api_key: String,
    model_id: String,
    /// The LOCKED output dimension persisted into the index header. For known
    /// models this is a built-in default; otherwise it must be configured. The
    /// AC-11-3b-GATE probe confirms the live value matches before the provider is
    /// marked "supported".
    dimension: usize,
}

impl RemoteEmbeddingProvider {
    /// Build a provider against an already-resolved `base_url` / `api_key` /
    /// `model_id` / `dimension`. Does NO network I/O. Rejects credentials with
    /// control characters (mirrors openai/mod.rs:67-71) and fails if the HTTP
    /// client cannot be built.
    pub fn new(
        base_url: String,
        api_key: String,
        model_id: String,
        dimension: usize,
    ) -> Result<Self, EmbeddingError> {
        if api_key.bytes().any(|b| b < 0x20 || b == 0x7f) {
            return Err(EmbeddingError::NotReady(
                "API key contains control characters (newlines, tabs, etc.) — check the env var named by `api_key_env`".to_string(),
            ));
        }
        if dimension == 0 {
            return Err(EmbeddingError::NotReady(
                "remote embedding dimension is unknown — set `dimension` in the [memory] config or use a model with a known dimension".to_string(),
            ));
        }
        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| EmbeddingError::NotReady(format!("failed to build HTTP client: {e}")))?;
        Ok(Self {
            client,
            base_url: normalize_base_url(&base_url),
            api_key,
            model_id,
            dimension,
        })
    }

    /// POST `{base_url}/embeddings` with the OpenAI request schema and a per-call
    /// timeout. Returns vectors in INPUT ORDER (the response `data[]` is re-sorted
    /// by its `index` field, since the spec does not guarantee order and
    /// `refresh()`/`search()` zip vectors back to entries positionally).
    async fn embed_with_timeout(
        &self,
        texts: &[String],
        timeout: Duration,
    ) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let url = format!("{}/embeddings", self.base_url);
        let body = EmbeddingsRequest {
            model: &self.model_id,
            input: texts,
            encoding_format: "float",
        };

        let mut req = self
            .client
            .post(&url)
            .header("content-type", "application/json")
            .timeout(timeout)
            .json(&body);
        if !self.api_key.is_empty() {
            req = req.header("authorization", format!("Bearer {}", self.api_key));
        }

        let response = req.send().await.map_err(|e| {
            if e.is_timeout() {
                EmbeddingError::EmbedFailed(format!(
                    "request to {url} timed out after {}s (model may be slow/cold-starting — try a longer timeout or a lighter model)",
                    timeout.as_secs()
                ))
            } else {
                EmbeddingError::EmbedFailed(format!("request to {url} failed: {e}"))
            }
        })?;

        let status = response.status();
        if !status.is_success() {
            return Err(map_http_error(status, response).await);
        }

        let parsed: EmbeddingsResponse = response.json().await.map_err(|e| {
            // A read timeout during body download surfaces here as a decode
            // error; label it as a timeout so the GATE detail is not misleading.
            if e.is_timeout() {
                EmbeddingError::EmbedFailed(format!(
                    "response body read timed out after {}s (model may be slow/cold-starting)",
                    timeout.as_secs()
                ))
            } else {
                EmbeddingError::EmbedFailed(format!("failed to parse embeddings response: {e}"))
            }
        })?;

        let mut data = parsed.data;
        // Re-order by the response `index` so output matches `input` order.
        data.sort_by_key(|d| d.index);
        // Validate that every expected index (0..texts.len()) appears exactly
        // once — a duplicate or gap means the server misbehaved and vectors
        // would be mapped to the wrong inputs.
        let expected_indices: std::collections::HashSet<usize> = (0..texts.len()).collect();
        let actual_indices: std::collections::HashSet<usize> =
            data.iter().map(|d| d.index).collect();
        if actual_indices != expected_indices {
            return Err(EmbeddingError::EmbedFailed(format!(
                "embedding index mismatch: expected indices {:?}, got {:?}",
                expected_indices, actual_indices
            )));
        }
        let vectors: Vec<Vec<f32>> = data.into_iter().map(|d| d.embedding).collect();

        if vectors.len() != texts.len() {
            return Err(EmbeddingError::EmbedFailed(format!(
                "embedding count mismatch: {} inputs → {} vectors",
                texts.len(),
                vectors.len()
            )));
        }
        // Validate that every returned vector has the expected dimension — a
        // mismatch would corrupt the index.
        if let Some(v) = vectors.iter().find(|v| v.len() != self.dimension) {
            return Err(EmbeddingError::EmbedFailed(format!(
                "dimension mismatch: expected {}, got {}",
                self.dimension,
                v.len()
            )));
        }
        Ok(vectors)
    }
}

impl fmt::Debug for RemoteEmbeddingProvider {
    /// Masks `api_key` so the credential never reaches logs / notices
    /// (mirrors openai/mod.rs:140-148).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemoteEmbeddingProvider")
            .field("base_url", &self.base_url)
            .field("model_id", &self.model_id)
            .field("dimension", &self.dimension)
            .field("api_key", &"(***)")
            .finish()
    }
}

#[async_trait]
impl EmbeddingProvider for RemoteEmbeddingProvider {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        self.embed_with_timeout(texts, EMBED_TIMEOUT).await
    }

    fn dimension(&self) -> usize {
        self.dimension
    }

    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn kind(&self) -> ProviderKind {
        ProviderKind::Remote
    }

    /// The AC-11-3b-GATE hook: POST one short text and read
    /// `data[0].embedding.len()` as the live dimension. A network/HTTP failure is
    /// reported as `Ok(ProbeReport { healthy: false, detail: Some(err) })` rather
    /// than `Err` — the probe is a side-effect-free health check whose job is to
    /// FILL the Dev Record GATE row (chosen host / model / dimension / pass-fail),
    /// so the failure detail must survive into the report.
    async fn probe(&self) -> Result<ProbeReport, EmbeddingError> {
        match self
            .embed_with_timeout(&["probe".to_string()], PROBE_TIMEOUT)
            .await
        {
            Ok(vectors) => {
                let dimension = vectors.first().map(|v| v.len()).unwrap_or(0);
                let healthy = dimension > 0 && dimension == self.dimension;
                Ok(ProbeReport {
                    model_id: self.model_id.clone(),
                    dimension,
                    kind: ProviderKind::Remote,
                    healthy,
                    detail: (dimension != self.dimension).then(|| {
                        format!(
                            "live dimension {dimension} differs from configured {}",
                            self.dimension
                        )
                    }),
                })
            }
            Err(e) => Ok(ProbeReport {
                model_id: self.model_id.clone(),
                dimension: self.dimension,
                kind: ProviderKind::Remote,
                healthy: false,
                detail: Some(e.to_string()),
            }),
        }
    }
}

/// Map a non-2xx HTTP response to an [`EmbeddingError`], mirroring the OpenAI
/// chat client's status handling (openai/mod.rs:197-232) but onto the embedding
/// seam's three variants.
async fn map_http_error(
    status: reqwest::StatusCode,
    response: reqwest::Response,
) -> EmbeddingError {
    match status.as_u16() {
        // Bad/missing credentials — the provider is not ready to serve.
        401 | 403 => EmbeddingError::NotReady(format!("authentication failed (HTTP {status})")),
        // Model withdrawn / wrong id — the AC-11-3b-GATE "model withdrawn" trigger.
        404 => EmbeddingError::ModelUnavailable(format!("model not found (HTTP {status})")),
        429 => {
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());
            match retry_after {
                Some(secs) => EmbeddingError::EmbedFailed(format!(
                    "rate limited (HTTP 429, retry-after {secs}s)"
                )),
                None => EmbeddingError::EmbedFailed("rate limited (HTTP 429)".to_string()),
            }
        }
        code if code >= 500 => {
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "unknown".to_string());
            EmbeddingError::EmbedFailed(format!("server error (HTTP {code}): {body}"))
        }
        _ => {
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "unknown".to_string());
            EmbeddingError::EmbedFailed(format!("HTTP {status}: {body}"))
        }
    }
}

// ── Private OpenAI `/embeddings` wire DTOs (keep the domain serde-free) ──

#[derive(Serialize)]
struct EmbeddingsRequest<'a> {
    model: &'a str,
    input: &'a [String],
    encoding_format: &'a str,
}

#[derive(Deserialize)]
struct EmbeddingsResponse {
    data: Vec<EmbeddingData>,
}

#[derive(Deserialize)]
struct EmbeddingData {
    embedding: Vec<f32>,
    /// Position in the input batch. Defaulted in case a provider omits it (then
    /// the stable sort is a no-op and response order is preserved).
    #[serde(default)]
    index: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_control_chars_in_api_key() {
        let err = RemoteEmbeddingProvider::new(
            "https://example.com/v1".into(),
            "sk-bad\nkey".into(),
            "model".into(),
            1024,
        )
        .unwrap_err();
        assert!(matches!(err, EmbeddingError::NotReady(_)));
    }

    #[test]
    fn rejects_zero_dimension() {
        let err = RemoteEmbeddingProvider::new(
            "https://example.com/v1".into(),
            "sk-key".into(),
            "model".into(),
            0,
        )
        .unwrap_err();
        assert!(matches!(err, EmbeddingError::NotReady(_)));
    }

    #[test]
    fn debug_masks_api_key() {
        let p = RemoteEmbeddingProvider::new(
            "https://example.com/v1/".into(),
            "sk-super-secret-12345".into(),
            "baai/bge-m3".into(),
            1024,
        )
        .unwrap();
        let dbg = format!("{p:?}");
        assert!(
            !dbg.contains("sk-super-secret-12345"),
            "api key leaked in Debug"
        );
        assert!(dbg.contains("(***)"));
        assert!(dbg.contains("baai/bge-m3"));
        // base_url is normalized (trailing slash trimmed).
        assert!(dbg.contains("https://example.com/v1"));
        assert!(!dbg.contains("/v1/\""));
    }

    #[test]
    fn trait_accessors() {
        let p = RemoteEmbeddingProvider::new(
            "https://example.com/v1".into(),
            String::new(),
            "voyage-3".into(),
            1024,
        )
        .unwrap();
        assert_eq!(p.dimension(), 1024);
        assert_eq!(p.model_id(), "voyage-3");
        assert_eq!(p.kind(), ProviderKind::Remote);
    }
}
