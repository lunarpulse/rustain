//! NFR45 — memory overhead per loaded adapter < 5MB.
//! Gated by #[ignore] for local-only runs; RSS measurement is platform-specific.
//!
//! Run with: cargo test --test profile_composition_memory -- --ignored --nocapture

use std::sync::Arc;

use rustain::domain::models::profile::{AdapterRef, PortDimension, ProfileSelection};
use rustain::infrastructure::composition::ComposeContext;
use rustain::infrastructure::runtime::agent_core::AgentCore;
use std::collections::BTreeMap;

fn compose_ctx() -> ComposeContext {
    ComposeContext {
        workspace_path: std::path::PathBuf::from("/tmp/test-memory"),
        project_context: rustain::domain::models::project_context::ProjectContext::empty(),
        storage: Arc::new(rustain::adapters::noop::NoOpStorage::default())
            as Arc<dyn rustain::domain::ports::StoragePort>,
        skill_activator: Arc::new(rustain::adapters::skill_activation::SkillActivator::new()),
        mcp_servers: Vec::new(),
        include_builtin_tools: true,
        domain_tx: None,
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

fn coding_selection() -> ProfileSelection {
    let mut dims = BTreeMap::new();
    dims.insert(
        PortDimension::Persona,
        AdapterRef {
            adapter: "coding".into(),
            _config: None,
        },
    );
    dims.insert(
        PortDimension::Memory,
        AdapterRef {
            adapter: "noop".into(),
            _config: None,
        },
    );
    dims.insert(
        PortDimension::Session,
        AdapterRef {
            adapter: "basic".into(),
            _config: None,
        },
    );
    dims.insert(
        PortDimension::Tools,
        AdapterRef {
            adapter: "builtin-full".into(),
            _config: None,
        },
    );
    dims.insert(
        PortDimension::Channels,
        AdapterRef {
            adapter: "terminal".into(),
            _config: None,
        },
    );
    dims.insert(
        PortDimension::Scheduler,
        AdapterRef {
            adapter: "none".into(),
            _config: None,
        },
    );
    dims.insert(
        PortDimension::Context,
        AdapterRef {
            adapter: "default".into(),
            _config: None,
        },
    );
    ProfileSelection { dimensions: dims }
}

fn base_selection() -> ProfileSelection {
    let mut dims = BTreeMap::new();
    dims.insert(
        PortDimension::Persona,
        AdapterRef {
            adapter: "minimal".into(),
            _config: None,
        },
    );
    dims.insert(
        PortDimension::Memory,
        AdapterRef {
            adapter: "noop".into(),
            _config: None,
        },
    );
    dims.insert(
        PortDimension::Session,
        AdapterRef {
            adapter: "basic".into(),
            _config: None,
        },
    );
    dims.insert(
        PortDimension::Tools,
        AdapterRef {
            adapter: "builtin-only".into(),
            _config: None,
        },
    );
    dims.insert(
        PortDimension::Channels,
        AdapterRef {
            adapter: "terminal".into(),
            _config: None,
        },
    );
    dims.insert(
        PortDimension::Scheduler,
        AdapterRef {
            adapter: "none".into(),
            _config: None,
        },
    );
    dims.insert(
        PortDimension::Context,
        AdapterRef {
            adapter: "default".into(),
            _config: None,
        },
    );
    ProfileSelection { dimensions: dims }
}

/// Story 11.1 — the `personal-assistant` profile's resolved selection. The real
/// profile declares `channels = "telegram"` / `scheduler = "cron"` (preview,
/// feature-gated); the resolver rewrites those to the available `terminal`/`none`
/// adapters on a build without those features. Memory is the real `daily-log`.
fn personal_assistant_selection() -> ProfileSelection {
    let mut dims = BTreeMap::new();
    dims.insert(
        PortDimension::Persona,
        AdapterRef {
            adapter: "personal-assistant".into(),
            _config: None,
        },
    );
    dims.insert(
        PortDimension::Memory,
        AdapterRef {
            adapter: "daily-log".into(),
            _config: None,
        },
    );
    dims.insert(
        PortDimension::Session,
        AdapterRef {
            adapter: "basic".into(),
            _config: None,
        },
    );
    dims.insert(
        PortDimension::Tools,
        AdapterRef {
            adapter: "builtin-full".into(),
            _config: None,
        },
    );
    dims.insert(
        PortDimension::Channels,
        AdapterRef {
            adapter: "terminal".into(),
            _config: None,
        },
    );
    dims.insert(
        PortDimension::Scheduler,
        AdapterRef {
            adapter: "none".into(),
            _config: None,
        },
    );
    dims.insert(
        PortDimension::Context,
        AdapterRef {
            adapter: "daily".into(),
            _config: None,
        },
    );
    ProfileSelection { dimensions: dims }
}

/// AC1 + DoD + Task 5 end-to-end: the `personal-assistant` profile composes with
/// the real daily-log memory, and the risk-Safe `remember` builtin tool (wired at
/// the composition root) persists a notable entry through it. Drives the tool
/// (not `MemoryEntry` directly) so the test needs no `chrono` dev-dependency.
#[tokio::test]
async fn test_personal_assistant_remember_persists_via_daily_log() {
    use tokio_util::sync::CancellationToken;

    let tmp = tempfile::tempdir().unwrap();
    let mut ctx = compose_ctx();
    ctx.workspace_path = tmp.path().to_path_buf();

    let core = AgentCore::compose("personal-assistant", &personal_assistant_selection(), &ctx)
        .expect("personal-assistant profile composes with daily-log memory");

    // The `remember` tool routes through memory_slot → composed daily-log adapter.
    let tools = core.tools.load_full();
    let result = tools
        .execute(
            "remember",
            serde_json::json!({
                "summary": "epic 11 kickoff",
                "context": "first real MemoryPort adapter"
            }),
            CancellationToken::new(),
        )
        .await
        .expect("remember tool executes");
    assert!(!result.is_error, "remember is risk-Safe and succeeds");

    // The composed memory port reflects the stored entry (non-NoOp daily-log).
    let mem = core.memory.load_full();
    let recent = mem.recent(10).await.expect("recent");
    assert_eq!(
        recent.len(),
        1,
        "entry persisted via composed daily-log memory"
    );
    assert_eq!(recent[0].summary, "epic 11 kickoff");

    // AC1 path: the day file landed under {workspace}/.rustain/memory/.
    let memory_dir = tmp.path().join(".rustain").join("memory");
    assert!(
        memory_dir.is_dir(),
        "memory dir auto-created at canonical path"
    );
}

#[test]
#[ignore = "RSS-based; run with `cargo test --test profile_composition_memory -- --ignored --nocapture`"]
fn test_coding_profile_under_35mb() {
    let before = read_rss_kb();
    let _agent_core = AgentCore::compose("coding", &coding_selection(), &compose_ctx())
        .expect("composition should succeed");
    let after = read_rss_kb();
    let delta_kb = after.saturating_sub(before);
    let delta_mb = delta_kb / 1024;
    assert!(
        delta_mb < 35,
        "coding profile composition added {}MB (cap: 35MB)",
        delta_mb
    );
}

#[test]
#[ignore = "RSS-based; run with `cargo test --test profile_composition_memory -- --ignored --nocapture`"]
fn test_base_profile_under_5mb() {
    let before = read_rss_kb();
    let _agent_core = AgentCore::compose("base", &base_selection(), &compose_ctx())
        .expect("composition should succeed");
    let after = read_rss_kb();
    let delta_kb = after.saturating_sub(before);
    let delta_mb = delta_kb / 1024;
    assert!(
        delta_mb < 5,
        "base profile composition added {}MB (cap: 5MB)",
        delta_mb
    );
}

#[cfg(target_os = "linux")]
fn read_rss_kb() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let n: u64 = rest
                .trim()
                .split_whitespace()
                .next()
                .unwrap()
                .parse()
                .unwrap();
            return n;
        }
    }
    panic!("VmRSS not found in /proc/self/status");
}

#[cfg(not(target_os = "linux"))]
fn read_rss_kb() -> u64 {
    0
}
