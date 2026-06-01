//! Story 11.3a — `vector-search` adapter: composition + (gated) real-model
//! integration tests.
//!
//! Split by feature so the DEFAULT test run never needs the model or the ONNX
//! runtime:
//! - **feature OFF** (default CI): the AC4 graceful not-compiled fallback —
//!   `build_memory("vector-search")` emits the exact notice and degrades to
//!   keyword-only `project-scoped` search, WITHOUT a hard error.
//! - **feature ON** (`--features vector-search`): the composition wires the real
//!   wrapper and `store`/`recent`/`remember_fact` delegate to inner unchanged
//!   (no model touched). The real-model round-trip + NFR56/57 timings are a
//!   separate `#[ignore]` test so `cargo test --features vector-search` stays
//!   offline by default.
//!
//! Pure index/cosine/diff/persistence logic is unit-tested inline in the module
//! (`adapters/vector_search/{index,adapter}.rs`), where the stub provider and
//! private fields are reachable — matching the daily-log/long-term convention.

use std::path::PathBuf;
use std::sync::Arc;

use rustain::domain::events::AppEvent;
use rustain::infrastructure::composition::ComposeContext;
use tokio::sync::mpsc::UnboundedSender;

/// Minimal `ComposeContext` rooted at `workspace`, parameterized on the event
/// channel. Mirrors `tests/composition_event_tx.rs::compose_ctx`.
fn compose_ctx(workspace: PathBuf, domain_tx: Option<UnboundedSender<AppEvent>>) -> ComposeContext {
    ComposeContext {
        workspace_path: workspace,
        project_context: rustain::domain::models::project_context::ProjectContext::empty(),
        storage: Arc::new(rustain::adapters::noop::NoOpStorage)
            as Arc<dyn rustain::domain::ports::StoragePort>,
        skill_activator: Arc::new(rustain::adapters::skill_activation::SkillActivator::new()),
        mcp_servers: Vec::new(),
        include_builtin_tools: true,
        domain_tx,
        tool_exposure: "static-full".into(),
        skill_exposure: "l1-metadata".into(),
        skill_cache: Arc::new(rustain::infrastructure::skill_cache::SkillCache::new_in_memory()),
        sandbox_adapter: "noop".into(),
        sandbox_startup_policy: rustain::domain::models::sandbox::SandboxPolicy::Permissive,
        sandbox_slot: Arc::new(arc_swap::ArcSwap::from_pointee(Arc::new(
            rustain::adapters::sandbox::NoOpSandbox,
        )
            as Arc<dyn rustain::domain::ports::SandboxManager>)),
        memory_slot: std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(std::sync::Arc::new(
            rustain::adapters::noop::NoOpMemory,
        )
            as std::sync::Arc<dyn rustain::domain::ports::MemoryPort>)),
        sandbox_policy: Arc::new(tokio::sync::RwLock::new(
            rustain::domain::models::sandbox::SandboxPolicy::Permissive,
        )),
        #[cfg(feature = "meta-search")]
        search_config: rustain::domain::models::SearchConfig::default(),
        #[cfg(feature = "meta-search")]
        meta_search_engine: None,
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Feature OFF — AC4 graceful not-compiled fallback (runs in default CI).
// ──────────────────────────────────────────────────────────────────────────
#[cfg(not(feature = "vector-search"))]
mod not_compiled {
    use super::*;
    use chrono::Local;
    use rustain::domain::models::{MemoryEntry, MemoryFact, NoticeLevel};
    use rustain::infrastructure::composition::build_memory;

    const EXPECTED_NOTICE: &str = "Adapter 'vector-search' not available. Install with: cargo install rustain --features vector-search";

    #[tokio::test]
    async fn falls_back_to_keyword_with_exact_notice() {
        let tmp = tempfile::tempdir().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let ctx = compose_ctx(tmp.path().to_path_buf(), Some(tx));

        // MUST NOT be a hard error — vector-search is known-but-not-compiled.
        let mem = build_memory("vector-search", None, &ctx)
            .expect("not-compiled vector-search composes via graceful fallback, never errors");

        // The exact AC4 message fired as a Warning notice.
        let mut found = None;
        while let Ok(ev) = rx.try_recv() {
            if let AppEvent::SystemNotice { level, message, .. } = ev {
                if message.contains("vector-search") {
                    found = Some((level, message));
                }
            }
        }
        let (level, message) = found.expect("AC4 not-available notice emitted");
        assert_eq!(level, NoticeLevel::Warning);
        assert_eq!(message, EXPECTED_NOTICE);

        // Behaves as project-scoped: store→daily, remember_fact→long-term,
        // search is keyword over both tiers.
        mem.store(MemoryEntry {
            timestamp: Local::now(),
            summary: "touched the parser".into(),
            context: None,
        })
        .await
        .unwrap();
        mem.remember_fact(MemoryFact {
            category: "Database".into(),
            fact: "the store is postgres".into(),
            detail: None,
        })
        .await
        .unwrap();

        let hits = mem.search("postgres", 10).await.unwrap();
        assert!(
            hits.iter().any(|e| e.summary == "the store is postgres"),
            "keyword search works via the project-scoped fallback (AC4)"
        );
    }

    #[tokio::test]
    async fn fallback_is_silent_without_domain_tx() {
        // Headless/eval path (domain_tx None) must remain valid: no panic, no
        // error, behaves as project-scoped.
        let tmp = tempfile::tempdir().unwrap();
        let ctx = compose_ctx(tmp.path().to_path_buf(), None);
        let mem =
            build_memory("vector-search", None, &ctx).expect("fallback composes without a channel");
        mem.store(MemoryEntry {
            timestamp: Local::now(),
            summary: "did a thing".into(),
            context: None,
        })
        .await
        .unwrap();
        assert!(
            mem.recent(10)
                .await
                .unwrap()
                .iter()
                .any(|e| e.summary == "did a thing"),
            "store/recent work on the fallback adapter"
        );
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Feature ON — composition wires the real wrapper; delegation is model-free.
// ──────────────────────────────────────────────────────────────────────────
#[cfg(feature = "vector-search")]
mod compiled {
    use super::*;
    use chrono::Local;
    use rustain::domain::models::{MemoryEntry, MemoryFact};
    use rustain::infrastructure::composition::build_memory;

    #[tokio::test]
    async fn composition_returns_wrapper_and_delegates_writes() {
        // Build through the REAL factory path with the feature on. Construction
        // does NO I/O and never touches the model — store/recent/remember_fact
        // delegate straight to the inner project-scoped adapter, so we can
        // assert delegation offline (no download).
        let tmp = tempfile::tempdir().unwrap();
        let ctx = compose_ctx(tmp.path().to_path_buf(), None);
        let mem = build_memory("vector-search", None, &ctx).expect("vector-search composes");

        mem.store(MemoryEntry {
            timestamp: Local::now(),
            summary: "operational note".into(),
            context: None,
        })
        .await
        .unwrap();
        mem.remember_fact(MemoryFact {
            category: "Preferences".into(),
            fact: "prefers snake_case".into(),
            detail: None,
        })
        .await
        .unwrap();

        let recent = mem.recent(10).await.unwrap();
        assert!(
            recent.iter().any(|e| e.summary == "operational note"),
            "store delegates to the daily-log inner"
        );
        assert!(
            recent.iter().any(|e| e.summary == "prefers snake_case"),
            "remember_fact delegates to the long-term inner"
        );

        // store wrote a daily-log dir; remember_fact wrote MEMORY.md — proving
        // the inner is project-scoped (the AC4 fallback target).
        assert!(tmp.path().join(".rustain").join("memory").is_dir());
        assert!(tmp.path().join(".rustain").join("MEMORY.md").exists());
    }

    /// Real-model round-trip + NFR timings. Gated behind `#[ignore]` so the
    /// default `--features vector-search` run never downloads the model (project
    /// memory: determinism > realism). Run with:
    /// `cargo test --features vector-search real_model -- --ignored`.
    #[tokio::test]
    #[ignore = "downloads the bge-small ONNX model on first run (network)"]
    async fn real_model_embeds_searches_and_meets_nfr_bounds() {
        use rustain::adapters::vector_search::{
            EmbeddingProvider, LocalEmbeddingProvider, ProviderKind, VectorSearchMemory,
            default_cache_dir,
        };
        use rustain::domain::ports::MemoryPort;
        use std::sync::Arc;
        use std::time::Instant;

        let provider = LocalEmbeddingProvider::new(default_cache_dir(), None);
        assert_eq!(provider.kind(), ProviderKind::Local);

        let vectors = provider
            .embed(&[
                "the database is postgres".into(),
                "the parser is pratt".into(),
            ])
            .await
            .expect("real model embeds");
        assert_eq!(vectors.len(), 2);
        assert_eq!(vectors[0].len(), 384, "bge-small is 384-dim");
        assert_eq!(provider.dimension(), 384);

        let report = provider.probe().await.expect("probe");
        assert_eq!(report.dimension, 384);
        assert_eq!(report.kind, ProviderKind::Local);

        let tmp = tempfile::tempdir().unwrap();
        let ctx = compose_ctx(tmp.path().to_path_buf(), None);
        let _ = &ctx;

        let inner: Arc<dyn rustain::domain::ports::MemoryPort> = Arc::new(
            rustain::adapters::daily_log_memory::DailyLogMemory::new(tmp.path()),
        );
        for s in [
            "migrated the database to postgres 16",
            "the pratt parser handles operator precedence",
            "ci pipeline now runs on the new runners",
        ] {
            inner
                .store(MemoryEntry {
                    timestamp: Local::now(),
                    summary: s.into(),
                    context: None,
                })
                .await
                .unwrap();
        }
        let mem = VectorSearchMemory::new(
            inner,
            Arc::new(LocalEmbeddingProvider::new(default_cache_dir(), None)),
            tmp.path().join(".rustain").join("memory").join("index.bin"),
        );
        mem.initialize().await.expect("index builds");

        let started = Instant::now();
        let hits = mem
            .search("which database did we move to?", 3)
            .await
            .unwrap();
        let elapsed = started.elapsed();
        assert!(!hits.is_empty());
        assert_eq!(
            hits[0].summary, "migrated the database to postgres 16",
            "semantic match (no shared keyword 'move'/'postgres' in the winner's favour is purely semantic)"
        );
        assert!(
            elapsed.as_millis() < 200,
            "NFR56: query < 200ms (was {}ms)",
            elapsed.as_millis()
        );
    }

    #[tokio::test]
    #[ignore = "downloads the bge-small ONNX model on first run (network)"]
    async fn real_model_nfr_at_scale_1k_and_10k() {
        use rustain::adapters::vector_search::{
            LocalEmbeddingProvider, VectorSearchMemory, default_cache_dir,
        };
        use rustain::domain::ports::MemoryPort;
        use std::sync::Arc;
        use std::time::Instant;

        let tmp = tempfile::tempdir().unwrap();
        let inner: Arc<dyn rustain::domain::ports::MemoryPort> = Arc::new(
            rustain::adapters::daily_log_memory::DailyLogMemory::new(tmp.path()),
        );

        let mut base_ts = chrono::Local::now();
        for i in 0..10_000 {
            inner
                .store(MemoryEntry {
                    timestamp: base_ts,
                    summary: format!("entry {i}: topic is about {i}"),
                    context: None,
                })
                .await
                .unwrap();
            base_ts += chrono::Duration::seconds(1);
        }

        let index_path = tmp.path().join(".rustain").join("memory").join("index.bin");

        // NFR57: indexing 1,000 entries must complete in <5s.
        let inner_1k: Arc<dyn rustain::domain::ports::MemoryPort> = Arc::new(
            rustain::adapters::daily_log_memory::DailyLogMemory::new(tmp.path()),
        );
        let mem_1k = VectorSearchMemory::new(
            inner_1k,
            Arc::new(LocalEmbeddingProvider::new(default_cache_dir(), None)),
            index_path.clone(),
        );
        let started = Instant::now();
        mem_1k.initialize().await.expect("1k index build");
        let build_1k = started.elapsed();
        let count_1k = { mem_1k.index.read().await.entries.len() };
        assert!(count_1k >= 1000, "indexed at least 1000 entries (got {count_1k})");
        assert!(
            build_1k.as_secs() < 5,
            "NFR57: 1k index build < 5s (was {:.1}s, {} entries)",
            build_1k.as_secs_f64(),
            count_1k,
        );

        // NFR56: query against 10k indexed entries must complete in <200ms.
        let inner_10k: Arc<dyn rustain::domain::ports::MemoryPort> = Arc::new(
            rustain::adapters::daily_log_memory::DailyLogMemory::new(tmp.path()),
        );
        let mem_10k = VectorSearchMemory::new(
            inner_10k,
            Arc::new(LocalEmbeddingProvider::new(default_cache_dir(), None)),
            index_path,
        );
        mem_10k.initialize().await.expect("10k index build");
        let count_10k = { mem_10k.index.read().await.entries.len() };
        assert!(count_10k >= 10_000, "indexed at least 10000 entries (got {count_10k})");

        let started = Instant::now();
        let hits = mem_10k.search("entry 5000", 10).await.expect("10k query");
        let query_10k = started.elapsed();
        assert!(!hits.is_empty(), "10k query returns results");
        assert!(
            query_10k.as_millis() < 200,
            "NFR56: 10k query < 200ms (was {}ms, {} entries)",
            query_10k.as_millis(),
            count_10k,
        );
    }
}
