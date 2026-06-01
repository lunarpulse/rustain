//! `LocalEmbeddingProvider` — local ONNX embeddings over `fastembed` (Story
//! 11.3a, AC1/AC5).
//!
//! Default model: `BAAI/bge-small-en-v1.5` (384-dim), downloaded on first use
//! and cached at `~/.config/rustain/models/` (via `dirs::config_dir()`). No
//! network is needed after that first setup (AC5). The model is NOT bundled in
//! the binary (NFR9) — `ort` fetches the ONNX runtime separately.
//!
//! ## Async discipline
//! `fastembed`'s `embed()` (and model construction, which performs the download)
//! is **synchronous and CPU-bound**, with no Tokio dependency. Every such call
//! is wrapped in [`tokio::task::spawn_blocking`] so the async runtime is never
//! blocked. The model is built once via a [`tokio::sync::OnceCell`].
//!
//! ## Progress (AC5, Q3)
//! `fastembed`'s built-in `with_show_download_progress(true)` writes to stderr,
//! which the TUI owns and hides. So the user-visible signal is a `SystemNotice`
//! emitted via `domain_tx` before the (potentially downloading) model build and
//! again when it is ready. A streaming percent bar would need a fastembed
//! callback it does not cleanly expose — deferred to a later story.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::{Mutex, OnceCell};

use crate::domain::events::AppEvent;
use crate::domain::models::NoticeLevel;

use super::{EmbeddingError, EmbeddingProvider, ProbeReport, ProviderKind};

/// Stable identifier for the default local model (also the index header's
/// `model_id`). No vendor SDK name leaks into the seam's types.
const MODEL_ID: &str = "BAAI/bge-small-en-v1.5";
/// `bge-small-en-v1.5` output dimension.
const MODEL_DIM: usize = 384;

/// The user-global model cache directory: `~/.config/rustain/models/` on Linux
/// (`dirs::config_dir()` + `rustain/models`). Falls back to the system temp dir
/// only if no config dir is resolvable (exotic platforms).
pub fn default_cache_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("rustain")
        .join("models")
}

/// Local embedding provider backed by a lazily-constructed `fastembed` model.
pub struct LocalEmbeddingProvider {
    cache_dir: PathBuf,
    /// Surfaces the download/ready notices in the TUI. `None` (headless/eval)
    /// stays silent.
    domain_tx: Option<UnboundedSender<AppEvent>>,
    /// Built (and the model downloaded, if needed) exactly once. `fastembed`'s
    /// `embed()` takes `&mut self`, so the model is behind a `tokio::sync::Mutex`
    /// (NOT `std::sync` — conformance) whose `OwnedMutexGuard` (Send + 'static)
    /// is moved into `spawn_blocking`. Calls are serialized, which is fine: the
    /// refresh issues ONE batched embed and search ONE query embed.
    model: OnceCell<Arc<Mutex<TextEmbedding>>>,
}

impl LocalEmbeddingProvider {
    /// Construct a provider that caches its model at `cache_dir`. Does NO I/O —
    /// the model is built (and downloaded on first use) on the first `embed`.
    pub fn new(cache_dir: PathBuf, domain_tx: Option<UnboundedSender<AppEvent>>) -> Self {
        Self {
            cache_dir,
            domain_tx,
            model: OnceCell::new(),
        }
    }

    /// Get-or-build the model handle. The build (which may download ~130MB) runs
    /// in `spawn_blocking`; a `SystemNotice` brackets it for the TUI (AC5, Q3).
    async fn model(&self) -> Result<Arc<Mutex<TextEmbedding>>, EmbeddingError> {
        self.model
            .get_or_try_init(|| async {
                self.notice(
                    NoticeLevel::Info,
                    format!("Loading embedding model ({MODEL_ID}); first run downloads ~130MB…"),
                );
                let cache_dir = self.cache_dir.clone();
                let built = tokio::task::spawn_blocking(move || {
                    TextEmbedding::try_new(
                        InitOptions::new(EmbeddingModel::BGESmallENV15)
                            .with_cache_dir(cache_dir)
                            .with_show_download_progress(true),
                    )
                })
                .await
                .map_err(|e| EmbeddingError::ModelUnavailable(format!("join error: {e}")))?
                .map_err(|e| EmbeddingError::ModelUnavailable(e.to_string()))?;
                self.notice(NoticeLevel::Info, "Embedding model ready.".to_string());
                Ok(Arc::new(Mutex::new(built)))
            })
            .await
            .map(Arc::clone)
    }

    /// Emit a `SystemNotice` if a channel is wired (mirrors `LongTermMemory`'s
    /// size-warning surface). The event is pre-built so the conformance scanner
    /// sees a bare `tx.send(event)`, and the line is tagged regardless.
    fn notice(&self, level: NoticeLevel, message: String) {
        if let Some(tx) = &self.domain_tx {
            let event = AppEvent::SystemNotice {
                conversation_id: None,
                level,
                message,
            };
            let _ = tx.send(event); // CONFORMANCE_EXCEPTION_EVENTBUS_BYPASS: 11-3a AC5 — model download/ready notice via adapter domain_tx (no event_bus access)
        }
    }
}

#[async_trait]
impl EmbeddingProvider for LocalEmbeddingProvider {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let model = self.model().await?;
        let owned: Vec<String> = texts.to_vec();
        // `embed()` takes `&mut self` + is sync/CPU-bound. Take an owned guard
        // (Send + 'static) and move it into a blocking task so the runtime is
        // never blocked and the borrow is mutable.
        let mut guard = model.lock_owned().await;
        tokio::task::spawn_blocking(move || guard.embed(owned, None))
            .await
            .map_err(|e| EmbeddingError::EmbedFailed(format!("join error: {e}")))?
            .map_err(|e| EmbeddingError::EmbedFailed(e.to_string()))
    }

    fn dimension(&self) -> usize {
        MODEL_DIM
    }

    fn model_id(&self) -> &str {
        MODEL_ID
    }

    fn kind(&self) -> ProviderKind {
        ProviderKind::Local
    }

    async fn probe(&self) -> Result<ProbeReport, EmbeddingError> {
        // Build (download if needed) + a one-token embed confirms the model
        // loads and reports its true dimension. 11.3b's remote impl overrides
        // this with a network health check (AC-11-3b-GATE).
        let v = self.embed(&["probe".to_string()]).await?;
        let dimension = v.first().map(|e| e.len()).unwrap_or(MODEL_DIM);
        Ok(ProbeReport {
            model_id: MODEL_ID.to_string(),
            dimension,
            kind: ProviderKind::Local,
            healthy: true,
            detail: None,
        })
    }
}
