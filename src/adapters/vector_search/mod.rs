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
pub mod index;

use async_trait::async_trait;

pub use adapter::VectorSearchMemory;
pub use embedding_local::{LocalEmbeddingProvider, default_cache_dir};

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
