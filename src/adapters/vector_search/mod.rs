//! `vector_search` — the local semantic-search memory adapter (Story 11.3a).
//!
//! This whole module is feature-gated behind `vector-search` (declared at the
//! `mod` site in `adapters/mod.rs`), so the default build pulls neither
//! `fastembed`/`ort` nor `bincode` and NFR9 (<30MB default binary) is untouched.
//!
//! ## What lives here (the local-only beachhead)
//! - [`EmbeddingProvider`] — the load-bearing **seam** Story 11.3b extends with
//!   remote/OpenAI-compatible providers *without changing the index or search
//!   code* (AC5). It sits INSIDE this adapter, NOT at the port boundary — per the
//!   2026-05-30 proposal Change 3: "`EmbeddingProvider` sits inside the
//!   vector-search memory adapter, not at the port boundary; `MemoryPort` is
//!   unaffected." No vendor names appear in any type (architecture.md:174).
//! - [`index`] — a flat brute-force cosine [`index::VectorIndex`] with binary
//!   (`bincode`) persistence (NOT HNSW — sub-10ms at the NFR56 10k bound).
//! - [`embedding_local`] — [`embedding_local::LocalEmbeddingProvider`] over
//!   `fastembed` (downloads + caches the model on first use → AC5).
//! - [`adapter`] — [`adapter::VectorSearchMemory`], the wrap-and-override
//!   `MemoryPort` composite (mirrors `ProjectScopedMemory`): delegates
//!   `store`/`remember_fact`/`recent` to its inner content source, overrides only
//!   `search` to do semantic top-k. The wrap is exactly why AC4's keyword
//!   fallback is natural — it is just `inner.search`.
//!
//! ## Out of scope (guarded — later stories)
//! - Remote providers, hybrid BM25+vector, temporal decay → Story 11.3b. The
//!   `probe()` hook on the trait already backs 11.3b's `AC-11-3b-GATE`.
//! - `ProvenancedEntry`/`RedactionRecord`/`Relevance`, context injection,
//!   redaction-from-index → Story 11.4. `search` returns plain `Vec<MemoryEntry>`.

pub mod adapter;
pub mod embedding_local;
pub mod embedding_remote;
pub mod fusion;
pub mod index;
pub mod redaction;

use async_trait::async_trait;
use serde::Deserialize;

pub use adapter::VectorSearchMemory;
pub use embedding_local::{LocalEmbeddingProvider, default_cache_dir};
pub use embedding_remote::RemoteEmbeddingProvider;

/// Whether an [`EmbeddingProvider`] runs locally (in-process ONNX, no network
/// after setup) or remotely (an HTTP API). The NFR56 `<200ms` latency assertion
/// attaches to [`ProviderKind::Local`] only; `embed()` stays `async + Result`
/// uniformly so the local and (future, 11.3b) remote impls share one trait —
/// there is deliberately no `local`/`remote` fork (architecture.md:174).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    Local,
    Remote,
}

/// Side-effect-free health/dimension report from [`EmbeddingProvider::probe`].
/// Shipped now; the remote impl that consumes it (and the `AC-11-3b-GATE`
/// OpenRouter probe) is Story 11.3b.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeReport {
    pub model_id: String,
    pub dimension: usize,
    pub kind: ProviderKind,
    pub healthy: bool,
    pub detail: Option<String>,
}

/// Errors from the embedding seam. No vendor names — local and remote share it.
#[derive(Debug, thiserror::Error)]
pub enum EmbeddingError {
    #[error("embedding model unavailable: {0}")]
    ModelUnavailable(String),
    #[error("embedding failed: {0}")]
    EmbedFailed(String),
    #[error("embedding provider not ready: {0}")]
    NotReady(String),
}

/// The single embedding seam. Local (11.3a) and remote (11.3b) impls both
/// satisfy it; the index and search code depend ONLY on this trait, so 11.3b
/// adds providers without touching them (AC5).
#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    /// Embed a batch of texts into dense vectors (one per input, same order).
    /// Batched by contract so callers amortize per-call overhead (AC6).
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError>;

    /// The fixed output dimension of this provider's model (e.g. 384 for
    /// bge-small). Persisted into the index header so a provider switch is
    /// detected on load (a `Vec<f32>` dim mismatch the type system can't catch).
    fn dimension(&self) -> usize;

    /// Stable model identifier (e.g. `"BAAI/bge-small-en-v1.5"`). Persisted into
    /// the index header alongside `dimension`.
    fn model_id(&self) -> &str;

    /// Local vs remote — see [`ProviderKind`].
    fn kind(&self) -> ProviderKind;

    /// A side-effect-free health/dimension check. The hook Story 11.3b's
    /// `AC-11-3b-GATE` will use; ship the method now, the remote impl is 11.3b.
    async fn probe(&self) -> Result<ProbeReport, EmbeddingError>;
}

/// Per-adapter `[memory]` config for the `vector-search` adapter (Story 11.3b,
/// AC1). Deserialized from the profile's per-dimension `config` table (the
/// `AdapterRef._config` seam, profile.rs:78-81) by the composition layer. Lives
/// adapter-local (Q3) — "config in infrastructure/config, never in the domain
/// port" (architecture.md:174). `#[serde(default)]` means every field is
/// optional; an absent `[memory] config` table leaves the offline-default local
/// provider untouched (NFR9).
///
/// Vendor strings are config VALUES here, never type names (architecture.md:174).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct VectorSearchConfig {
    /// `"local"` (default) selects the in-process ONNX model; any other value is
    /// resolved through [`provider_defaults`] to an OpenAI-compatible host.
    pub provider: String,
    /// Embedding model id sent verbatim to the remote `/embeddings` endpoint.
    /// Falls back to the provider's default model when omitted.
    pub model: Option<String>,
    /// Override the OpenAI-compatible base URL (must include any `/v1` path).
    /// Required for `provider = "openai-compatible"`; otherwise defaults per
    /// provider. This single field is what makes a host swap a config change,
    /// not a code change (AC-11-3b-GATE fallback).
    pub base_url: Option<String>,
    /// Name of the environment variable holding the API key. Defaults per
    /// provider (e.g. `OPENROUTER_API_KEY`). The key value itself is NEVER
    /// stored in config — only the env var's NAME is.
    pub api_key_env: Option<String>,
    /// The LOCKED embedding dimension. Optional when the model is in
    /// [`known_dimension`]; required (set from the AC-11-3b-GATE probe) otherwise.
    pub dimension: Option<usize>,
}

impl Default for VectorSearchConfig {
    fn default() -> Self {
        Self {
            provider: "local".to_string(),
            model: None,
            base_url: None,
            api_key_env: None,
            dimension: None,
        }
    }
}

/// Built-in defaults for a known remote provider STRING (Q7). Vendor names map
/// to a `base_url` + the conventional API-key env var + a safe default model.
/// `"openai-compatible"` is the escape hatch: it has NO base_url default (the
/// user must supply one). Returns `None` for an unrecognized provider string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderDefaults {
    /// `None` → the user MUST configure `base_url` (the generic case).
    pub base_url: Option<&'static str>,
    pub api_key_env: &'static str,
    /// `None` → the user MUST configure `model` (dimension can't be guessed).
    pub default_model: Option<&'static str>,
}

/// Resolve a provider config STRING to its built-in defaults (Q7). The string is
/// matched case-insensitively. `"local"` is handled by the caller and is not a
/// remote provider, so it returns `None` here too.
pub fn provider_defaults(provider: &str) -> Option<ProviderDefaults> {
    match provider.to_ascii_lowercase().as_str() {
        "openai" => Some(ProviderDefaults {
            base_url: Some("https://api.openai.com/v1"),
            api_key_env: "OPENAI_API_KEY",
            default_model: Some("text-embedding-3-small"),
        }),
        "voyage" => Some(ProviderDefaults {
            base_url: Some("https://api.voyageai.com/v1"),
            api_key_env: "VOYAGE_API_KEY",
            // Voyage model dims vary; require an explicit model + dimension.
            default_model: None,
        }),
        "openrouter" => Some(ProviderDefaults {
            base_url: Some("https://openrouter.ai/api/v1"),
            api_key_env: "OPENROUTER_API_KEY",
            // Default to the fixed-dim model (bge-m3, 1024) over the MRL-variable
            // qwen3-embedding-8b (4096) — safer until the GATE probe locks a dim.
            default_model: Some("baai/bge-m3"),
        }),
        "deepinfra" => Some(ProviderDefaults {
            base_url: Some("https://api.deepinfra.com/v1/openai"),
            api_key_env: "DEEPINFRA_API_KEY",
            default_model: Some("BAAI/bge-m3"),
        }),
        "together" => Some(ProviderDefaults {
            base_url: Some("https://api.together.xyz/v1"),
            api_key_env: "TOGETHER_API_KEY",
            default_model: None,
        }),
        "openai-compatible" => Some(ProviderDefaults {
            base_url: None, // must be explicit
            api_key_env: "RUSTAIN_EMBEDDING_API_KEY",
            default_model: None,
        }),
        _ => None,
    }
}

/// Built-in output dimension for a known embedding model (web-confirmed
/// 2026-06-01). Used to fill `dimension` when not explicitly configured; an
/// unknown model returns `None`, forcing an explicit `dimension` (which the
/// AC-11-3b-GATE probe establishes). Never guesses — a wrong dimension would
/// corrupt the index header.
pub fn known_dimension(model: &str) -> Option<usize> {
    match model.to_ascii_lowercase().as_str() {
        "baai/bge-m3" => Some(1024),
        "qwen/qwen3-embedding-8b" => Some(4096),
        "text-embedding-3-small" | "text-embedding-ada-002" => Some(1536),
        "text-embedding-3-large" => Some(3072),
        _ => None,
    }
}

#[cfg(test)]
mod config_tests {
    use super::*;

    fn parse(toml_src: &str) -> VectorSearchConfig {
        let value: toml::Value = toml::from_str(toml_src).unwrap();
        value.try_into().unwrap()
    }

    #[test]
    fn empty_config_defaults_to_local() {
        let cfg = VectorSearchConfig::default();
        assert_eq!(cfg.provider, "local");
        assert!(cfg.model.is_none() && cfg.base_url.is_none() && cfg.dimension.is_none());
    }

    #[test]
    fn deserializes_remote_provider_block() {
        let cfg = parse(
            r#"
            provider = "openrouter"
            model = "baai/bge-m3"
            api_key_env = "OPENROUTER_API_KEY"
            "#,
        );
        assert_eq!(cfg.provider, "openrouter");
        assert_eq!(cfg.model.as_deref(), Some("baai/bge-m3"));
        assert_eq!(cfg.api_key_env.as_deref(), Some("OPENROUTER_API_KEY"));
        assert!(cfg.dimension.is_none());
    }

    #[test]
    fn deserializes_partial_block_filling_rest_with_defaults() {
        let cfg = parse(r#"provider = "openai""#);
        assert_eq!(cfg.provider, "openai");
        assert!(cfg.model.is_none(), "model defaulted (resolved later)");
    }

    #[test]
    fn provider_defaults_cover_the_q7_set() {
        for p in [
            "openai",
            "voyage",
            "openrouter",
            "deepinfra",
            "together",
            "openai-compatible",
        ] {
            assert!(
                provider_defaults(p).is_some(),
                "{p} should be a known provider"
            );
        }
        // Case-insensitive.
        assert!(provider_defaults("OpenRouter").is_some());
        // "local" is not a remote provider; unknowns return None.
        assert!(provider_defaults("local").is_none());
        assert!(provider_defaults("does-not-exist").is_none());
    }

    #[test]
    fn openai_compatible_has_no_base_url_default() {
        let d = provider_defaults("openai-compatible").unwrap();
        assert!(
            d.base_url.is_none(),
            "generic provider must require explicit base_url"
        );
        assert!(d.default_model.is_none());
    }

    #[test]
    fn known_dimensions_match_web_confirmed_values() {
        assert_eq!(known_dimension("baai/bge-m3"), Some(1024));
        assert_eq!(known_dimension("BAAI/bge-m3"), Some(1024)); // case-insensitive
        assert_eq!(known_dimension("qwen/qwen3-embedding-8b"), Some(4096));
        assert_eq!(known_dimension("text-embedding-3-small"), Some(1536));
        assert_eq!(known_dimension("text-embedding-3-large"), Some(3072));
        assert_eq!(known_dimension("some-unknown-model"), None);
    }
}
