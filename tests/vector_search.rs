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
        let count_1k = mem_1k.indexed_entry_count().await;
        assert!(
            count_1k >= 1000,
            "indexed at least 1000 entries (got {count_1k})"
        );
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
        let count_10k = mem_10k.indexed_entry_count().await;
        assert!(
            count_10k >= 10_000,
            "indexed at least 10000 entries (got {count_10k})"
        );

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

// ──────────────────────────────────────────────────────────────────────────
// Story 11.3b — RemoteEmbeddingProvider HTTP client (mockito; NO real network).
// The OpenAI-compatible `/embeddings` client is exercised against a local mock
// server: request shape, Bearer auth, response parsing, `index` re-ordering,
// and HTTP status → EmbeddingError mapping. Determinism > realism (project
// memory `feedback_mcp_llm_test_prompts.md`) — these run in `--features
// vector-search` CI with no network.
// ──────────────────────────────────────────────────────────────────────────
#[cfg(feature = "vector-search")]
mod remote_http {
    use rustain::adapters::vector_search::{
        EmbeddingError, EmbeddingProvider, ProviderKind, RemoteEmbeddingProvider,
    };

    fn provider(base_url: String, dimension: usize) -> RemoteEmbeddingProvider {
        RemoteEmbeddingProvider::new(
            base_url,
            "sk-test-key".into(),
            "baai/bge-m3".into(),
            dimension,
        )
        .expect("remote provider builds")
    }

    #[tokio::test]
    async fn embed_posts_correct_shape_and_parses_vectors() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/embeddings")
            .match_header("content-type", "application/json")
            .match_header("authorization", mockito::Matcher::Regex("Bearer .+".into()))
            .match_body(mockito::Matcher::PartialJsonString(
                r#"{"model":"baai/bge-m3","encoding_format":"float"}"#.into(),
            ))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"data":[{"embedding":[0.1,0.2,0.3,0.4],"index":0}],"usage":{"prompt_tokens":3}}"#,
            )
            .create_async()
            .await;

        let p = provider(server.url(), 4);
        let vectors = p.embed(&["hello world".to_string()]).await.unwrap();
        mock.assert_async().await; // request matched method+path+auth+body shape

        assert_eq!(vectors.len(), 1);
        assert_eq!(vectors[0], vec![0.1, 0.2, 0.3, 0.4]);
        assert_eq!(p.kind(), ProviderKind::Remote);
        assert_eq!(p.dimension(), 4);
    }

    #[tokio::test]
    async fn embed_reorders_response_by_index() {
        let mut server = mockito::Server::new_async().await;
        // Two inputs; response returned OUT of order (index 1 before index 0).
        // `embed()` must restore input order so refresh()/search() zip correctly.
        let _mock = server
            .mock("POST", "/embeddings")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"data":[{"embedding":[9.0,9.0],"index":1},{"embedding":[1.0,1.0],"index":0}]}"#,
            )
            .create_async()
            .await;

        let p = provider(server.url(), 2);
        let vectors = p
            .embed(&["first".to_string(), "second".to_string()])
            .await
            .unwrap();
        assert_eq!(
            vectors[0],
            vec![1.0, 1.0],
            "input 0 → embedding with index 0"
        );
        assert_eq!(
            vectors[1],
            vec![9.0, 9.0],
            "input 1 → embedding with index 1"
        );
    }

    #[tokio::test]
    async fn embed_count_mismatch_is_an_error() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("POST", "/embeddings")
            .with_status(200)
            .with_body(r#"{"data":[{"embedding":[1.0,1.0],"index":0}]}"#)
            .create_async()
            .await;
        // Two inputs, one vector returned → mismatch.
        let p = provider(server.url(), 2);
        let err = p
            .embed(&["a".to_string(), "b".to_string()])
            .await
            .unwrap_err();
        assert!(matches!(err, EmbeddingError::EmbedFailed(_)));
    }

    #[tokio::test]
    async fn status_401_maps_to_not_ready() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("POST", "/embeddings")
            .with_status(401)
            .with_body(r#"{"error":"invalid api key"}"#)
            .create_async()
            .await;
        let p = provider(server.url(), 4);
        let err = p.embed(&["x".to_string()]).await.unwrap_err();
        assert!(
            matches!(err, EmbeddingError::NotReady(_)),
            "401 → NotReady, got {err:?}"
        );
    }

    #[tokio::test]
    async fn status_404_maps_to_model_unavailable() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("POST", "/embeddings")
            .with_status(404)
            .with_body(r#"{"error":"model withdrawn"}"#)
            .create_async()
            .await;
        let p = provider(server.url(), 4);
        let err = p.embed(&["x".to_string()]).await.unwrap_err();
        assert!(
            matches!(err, EmbeddingError::ModelUnavailable(_)),
            "404 (model withdrawn — the GATE fallback trigger) → ModelUnavailable, got {err:?}"
        );
    }

    #[tokio::test]
    async fn status_429_and_5xx_map_to_embed_failed() {
        for code in [429u16, 500, 503] {
            let mut server = mockito::Server::new_async().await;
            let _mock = server
                .mock("POST", "/embeddings")
                .with_status(code as usize)
                .with_header("retry-after", "12")
                .with_body("server says no")
                .create_async()
                .await;
            let p = provider(server.url(), 4);
            let err = p.embed(&["x".to_string()]).await.unwrap_err();
            assert!(
                matches!(err, EmbeddingError::EmbedFailed(_)),
                "HTTP {code} → EmbedFailed, got {err:?}"
            );
        }
    }

    #[tokio::test]
    async fn probe_reports_healthy_with_live_dimension() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("POST", "/embeddings")
            .with_status(200)
            .with_body(r#"{"data":[{"embedding":[0.0,0.1,0.2],"index":0}]}"#)
            .create_async()
            .await;
        // Configured dimension matches the live 3-dim response → healthy, no detail.
        let p = provider(server.url(), 3);
        let report = p.probe().await.unwrap();
        assert!(report.healthy);
        assert_eq!(report.dimension, 3);
        assert_eq!(report.kind, ProviderKind::Remote);
        assert!(report.detail.is_none());
    }

    #[tokio::test]
    async fn probe_unreachable_host_is_unhealthy_with_detail() {
        // Point at an unroutable address; probe must NOT error — it returns an
        // unhealthy report carrying the failure detail (so the GATE row records it).
        let p = provider("http://127.0.0.1:1/v1".into(), 1024);
        let report = p.probe().await.unwrap();
        assert!(!report.healthy);
        assert!(report.detail.is_some());
        assert_eq!(report.kind, ProviderKind::Remote);
    }
}

// ──────────────────────────────────────────────────────────────────────────
// AC-11-3b-GATE — the ONE real-network test. Gated by `#[ignore]` + the
// `OPENROUTER_API_KEY` env var so it NEVER runs in default or
// `--features vector-search` CI. Its job is to FILL the Dev Record GATE row
// (chosen host / model / returned dimension / latency / pass-fail), not to run
// in CI. Run with:
//   OPENROUTER_API_KEY=… cargo test --features vector-search gate_probe -- --ignored --nocapture
// ──────────────────────────────────────────────────────────────────────────
#[cfg(feature = "vector-search")]
mod gate_probe {
    use rustain::adapters::vector_search::{EmbeddingProvider, RemoteEmbeddingProvider};

    #[tokio::test]
    #[ignore = "AC-11-3b-GATE: real OpenRouter network probe; needs OPENROUTER_API_KEY"]
    async fn openrouter_embeddings_reachability_probe() {
        let api_key = match std::env::var("OPENROUTER_API_KEY") {
            Ok(k) if !k.trim().is_empty() => k,
            _ => {
                panic!("OPENROUTER_API_KEY must be set to run the GATE probe");
            }
        };
        // bge-m3 is the fixed-dim (1024) default; the probe confirms & LOCKS it.
        let model =
            std::env::var("RUSTAIN_GATE_MODEL").unwrap_or_else(|_| "baai/bge-m3".to_string());
        let expected_dim: usize = std::env::var("RUSTAIN_GATE_DIM")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1024);

        let provider = RemoteEmbeddingProvider::new(
            "https://openrouter.ai/api/v1".into(),
            api_key,
            model.clone(),
            expected_dim,
        )
        .expect("provider builds");

        let started = std::time::Instant::now();
        let report = provider.probe().await.expect("probe returns a report");
        let latency = started.elapsed();

        println!("── AC-11-3b-GATE probe result ──");
        println!("  host:       https://openrouter.ai/api/v1");
        println!("  model:      {model}");
        println!("  healthy:    {}", report.healthy);
        println!(
            "  dimension:  {} (expected {expected_dim})",
            report.dimension
        );
        println!("  latency:    {} ms", latency.as_millis());
        if let Some(detail) = &report.detail {
            println!("  detail:     {detail}");
        }
        println!("  → record the above in the story Dev Agent Record GATE table.");

        assert!(
            report.healthy,
            "probe must be healthy to mark the provider supported"
        );
        assert!(
            report.dimension > 0,
            "probe must return a positive dimension"
        );
    }
}
