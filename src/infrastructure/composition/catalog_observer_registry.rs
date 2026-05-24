//! Phase B `CatalogObserverRegistry` per ADR-09-01 v2.2 §W3 + ADR-09-02 §Phase B.
//!
//! Owns TWO `tokio::sync::broadcast::Sender` channels — one per kind — and
//! the 250ms owned-task debounce. Subscribers receive `CatalogDelta` /
//! `SkillCatalogDelta` events and reindex BM25 in the background.
//!
//! # Why broadcast (not direct call)
//!
//! The `MetaSearchEngine` is a singleton shared by both `MetaSearchExposure`
//! adapters (tool + skill sides). When a catalog mutates, the reindex must
//! happen ONCE per merged corpus — not twice (once per port). Broadcast
//! semantics with a single subscriber (the reindex owned task) is the
//! cleanest expression. The contract: senders fire-and-forget; receivers
//! either receive or RecvError::Lagged (in which case the receiver issues a
//! full-rebuild request — a slow path acceptable for the 1-in-1000 catalog
//! churn case).
//!
//! # 250ms debounce contract (Winston Risk 4)
//!
//! Catalog mutations come in bursts (community-profile install → 10 skills
//! added in 500ms; MCP server reconnect → list_changed + 60 tool updates).
//! Reindexing on every event would thrash the BM25 corpus. The owned task
//! drains the receiver in a loop with `tokio::select! { _ = sleep(250ms) =>
//! reindex_now(), Some(delta) = recv() => coalesce_into_pending(delta) }`.

use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use tokio::sync::{broadcast, Mutex};
use tokio_util::sync::CancellationToken;

use crate::domain::models::catalog_delta::CatalogDelta;
use crate::domain::models::skill_catalog_delta::SkillCatalogDelta;
use crate::infrastructure::search::merged_index::MergedIndex;

const DEBOUNCE_MS: u64 = 250;
const CHANNEL_CAPACITY: usize = 128;

type RebuildFn = Arc<dyn Fn() -> Arc<MergedIndex> + Send + Sync>;

pub struct CatalogObserverRegistry {
    pub tool_sender: broadcast::Sender<CatalogDelta>,
    pub skill_sender: broadcast::Sender<SkillCatalogDelta>,
    pub merged_index: Arc<ArcSwap<MergedIndex>>,
    rebuild_fn: RebuildFn,
    tool_rx: Mutex<Option<broadcast::Receiver<CatalogDelta>>>,
    skill_rx: Mutex<Option<broadcast::Receiver<SkillCatalogDelta>>>,
}

impl CatalogObserverRegistry {
    /// Create a new registry sharing the given `ArcSwap<MergedIndex>` with the engine.
    /// Subscribers are created eagerly to avoid race window where initial
    /// `populate_registry` delta is sent before `spawn_reindex_task` subscribes.
    pub fn new(
        shared_index: Arc<ArcSwap<MergedIndex>>,
        rebuild_fn: RebuildFn,
    ) -> Arc<Self> {
        let (tool_tx, tool_rx) = broadcast::channel(CHANNEL_CAPACITY);
        let (skill_tx, skill_rx) = broadcast::channel(CHANNEL_CAPACITY);
        Arc::new(Self {
            tool_sender: tool_tx,
            skill_sender: skill_tx,
            merged_index: shared_index,
            rebuild_fn,
            tool_rx: Mutex::new(Some(tool_rx)),
            skill_rx: Mutex::new(Some(skill_rx)),
        })
    }

    /// Trigger an immediate reindex synchronously (for initial population).
    /// Calls the rebuild_fn and stores the result into the shared ArcSwap.
    pub fn rebuild_now(&self) {
        let new_index = (self.rebuild_fn)();
        self.merged_index.store(new_index);
    }

    pub async fn spawn_reindex_task(
        self: Arc<Self>,
        cancel: CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        let tool_rx = self.tool_rx.lock().await.take().expect("spawn_reindex_task called twice");
        let skill_rx = self.skill_rx.lock().await.take().expect("spawn_reindex_task called twice");
        tokio::spawn(async move {
            let mut tool_rx = tool_rx;
            let mut skill_rx = skill_rx;
            let mut tool_closed = false;
            let mut skill_closed = false;
            let mut pending = false;
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    delta = tool_rx.recv(), if !tool_closed => {
                        match delta {
                            Ok(_d) => { pending = true; }
                            Err(broadcast::error::RecvError::Lagged(_)) => { pending = true; }
                            Err(broadcast::error::RecvError::Closed) => {
                                tool_closed = true;
                                if tool_closed && skill_closed { break; }
                            }
                        }
                    }
                    delta = skill_rx.recv(), if !skill_closed => {
                        match delta {
                            Ok(_d) => { pending = true; }
                            Err(broadcast::error::RecvError::Lagged(_)) => { pending = true; }
                            Err(broadcast::error::RecvError::Closed) => {
                                skill_closed = true;
                                if tool_closed && skill_closed { break; }
                            }
                        }
                    }
                    _ = tokio::time::sleep(Duration::from_millis(DEBOUNCE_MS)), if pending => {
                        let new_index = (self.rebuild_fn)();
                        self.merged_index.store(new_index);
                        pending = false;
                        tracing::debug!("MergedIndex reindex completed (Phase B)");
                    }
                }
            }
        })
    }
}
