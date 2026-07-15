//! Performance test: subagent spawn latency, cancellation propagation, RSS, and spool write.
//!
//! Run: `cargo test --test subagent_spawn -- --ignored --nocapture`

use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use rustain::adapters::filesystem::FileSystemStorage;
use rustain::adapters::noop::{NoOpApprovalPersistence, NoOpProvider};
use rustain::adapters::sandbox::NoOpSandbox;
use rustain::adapters::security_adapter::SecurityAdapter;
use rustain::adapters::subagent::InProcessSubagentRunner;
use rustain::adapters::toolset_adapter::ToolSetAdapter;
use rustain::domain::models::{AgentLaunchSpec, ModelTier, SandboxPolicy, ToolPolicy};
use rustain::domain::ports::SubagentRunner;
use rustain::domain::services::approval_runtime::ApprovalRuntime;
use rustain::domain::services::tool_scheduler::ToolScheduler;
use rustain::infrastructure::runtime::event_bus::EventBus;
use rustain::infrastructure::subagent::{NodeTree, SubagentSpool};
use serde_json::json;
use tokio_util::sync::CancellationToken;

const BASELINE_P99_MICROS: u64 = 1_000; // 1 ms = 30× headroom on NFR8's 30 ms ceiling

fn write_json_sidecar(results: &serde_json::Value) {
    let dir = std::path::Path::new("target/perf-results");
    let _ = std::fs::create_dir_all(dir);
    let path = dir.join("subagent_spawn.json");
    if let Ok(json_str) = serde_json::to_string_pretty(results) {
        let _ = std::fs::write(path, json_str);
    }
}

fn percentile(sorted: &[Duration], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p / 100.0) as usize;
    sorted[idx.min(sorted.len() - 1)].as_micros() as u64
}

async fn make_runner(tmp: &std::path::Path) -> InProcessSubagentRunner {
    let provider = Arc::new(NoOpProvider) as Arc<dyn rustain::domain::ports::StreamingProvider>;
    let storage = Arc::new(FileSystemStorage::new(tmp.to_path_buf()))
        as Arc<dyn rustain::domain::ports::StoragePort>;
    let security = Arc::new(SecurityAdapter::new(tmp.to_path_buf()))
        as Arc<dyn rustain::domain::ports::SecurityPort>;
    let sandbox = Arc::new(ArcSwap::from_pointee(
        Arc::new(NoOpSandbox) as Arc<dyn rustain::domain::ports::SandboxManager>
    ));
    let tools = Arc::new(ToolSetAdapter::new(
        tmp.to_path_buf(),
        storage.clone(),
        sandbox,
        Arc::new(tokio::sync::RwLock::new(SandboxPolicy::Permissive)),
    )) as Arc<dyn rustain::domain::ports::ToolSetPort>;
    let approval = ApprovalRuntime::new(1024, Arc::new(NoOpApprovalPersistence));
    let scheduler = ToolScheduler::new(security.clone(), tools.clone(), approval.clone(), 1024);
    let (event_bus, _rx) = EventBus::new(1024);
    let registry = Arc::new(NodeTree::new());
    let parent_sandbox = Arc::new(tokio::sync::RwLock::new(SandboxPolicy::Permissive));
    let spool = Arc::new(SubagentSpool::new(tmp.join("spool")).await.unwrap());
    let root_authority =
        rustain::domain::models::CapabilityToken::r1_root(rustain::domain::models::AgentId::root());
    let authority_ledger = Arc::new(
        rustain::domain::services::authority_ledger::AuthorityLedger::new(
            root_authority.clone(),
            std::sync::Arc::new(rustain::domain::clock::SystemClock::default()),
        ),
    );
    let authority =
        Arc::new(rustain::adapters::authority::InProcessAuthorityProvider::new(authority_ledger))
            as Arc<dyn rustain::domain::ports::AuthorityProvider>;

    InProcessSubagentRunner::new(
        provider,
        storage,
        security,
        tools,
        approval,
        scheduler,
        Arc::new(event_bus),
        registry,
        parent_sandbox,
        spool,
        authority,
        root_authority,
    )
}

#[tokio::test]
#[ignore = "performance test — run manually with `cargo test --test subagent_spawn -- --ignored --nocapture`"]
async fn subagent_spawn_latency() {
    let tmp = tempfile::tempdir().unwrap();
    let runner = make_runner(tmp.path()).await;
    let spec = AgentLaunchSpec {
        prompt: String::from("bench"),
        effective_model: String::from("noop"),
        tier: ModelTier::CheapAgentic,
        tools_allow: ToolPolicy::InheritFromParent,
        parent_ctx_tokens: 0,
        sandbox_override: None,
        parent_trace: None,
        isolated: false,
        delegation: rustain::domain::models::launch_spec::DelegationProfile::Child,
    };

    let iterations = 1_000;
    let mut latencies = Vec::with_capacity(iterations);

    for _ in 0..iterations {
        let cancel = CancellationToken::new();
        let start = Instant::now();
        let handle = runner
            .launch(spec.clone(), cancel.clone(), None)
            .await
            .unwrap();
        let elapsed = start.elapsed();
        latencies.push(elapsed);
        handle.cancel.cancel();
        // Deregister to avoid hitting NFR15 children limit
        runner.deregister(&handle.agent_id).await;
        // Small yield to let the child task exit
        tokio::task::yield_now().await;
    }

    latencies.sort();
    let p50 = percentile(&latencies, 50.0);
    let p95 = percentile(&latencies, 95.0);
    let p99 = percentile(&latencies, 99.0);

    println!("spawn p50={}µs p95={}µs p99={}µs", p50, p95, p99);
    assert!(
        p99 <= BASELINE_P99_MICROS,
        "spawn p99 {}µs exceeds baseline {}µs",
        p99,
        BASELINE_P99_MICROS
    );

    write_json_sidecar(&json!({
        "spawn_latency": {
            "p50_us": p50,
            "p95_us": p95,
            "p99_us": p99,
            "iterations": iterations,
            "baseline_p99_us": BASELINE_P99_MICROS,
        }
    }));
}

#[tokio::test]
#[ignore = "performance test — run manually with `cargo test --test subagent_spawn -- --ignored --nocapture`"]
async fn cancellation_propagation_latency() {
    let tmp = tempfile::tempdir().unwrap();
    let runner = make_runner(tmp.path()).await;
    let spec = AgentLaunchSpec {
        prompt: String::from("bench"),
        effective_model: String::from("noop"),
        tier: ModelTier::CheapAgentic,
        tools_allow: ToolPolicy::InheritFromParent,
        parent_ctx_tokens: 0,
        sandbox_override: None,
        parent_trace: None,
        isolated: false,
        delegation: rustain::domain::models::launch_spec::DelegationProfile::Child,
    };

    let iterations = 100;
    let mut latencies = Vec::with_capacity(iterations);

    for _ in 0..iterations {
        let cancel = CancellationToken::new();
        let handle = runner
            .launch(spec.clone(), cancel.clone(), None)
            .await
            .unwrap();
        let start = Instant::now();
        cancel.cancel();
        // Wait for child to observe cancellation via status channel
        let mut rx = handle.status_rx;
        let _ = tokio::time::timeout(Duration::from_millis(100), rx.recv()).await;
        let elapsed = start.elapsed();
        latencies.push(elapsed);
        runner.deregister(&handle.agent_id).await;
    }

    latencies.sort();
    let p95 = percentile(&latencies, 95.0);
    println!("cancellation-propagation p95={}µs", p95);
}

#[tokio::test]
#[ignore = "performance test — run manually with `cargo test --test subagent_spawn -- --ignored --nocapture`"]
async fn memory_rss_per_agent() {
    let tmp = tempfile::tempdir().unwrap();
    let runner = make_runner(tmp.path()).await;
    let spec = AgentLaunchSpec {
        prompt: String::from("bench"),
        effective_model: String::from("noop"),
        tier: ModelTier::CheapAgentic,
        tools_allow: ToolPolicy::InheritFromParent,
        parent_ctx_tokens: 0,
        sandbox_override: None,
        parent_trace: None,
        isolated: false,
        delegation: rustain::domain::models::launch_spec::DelegationProfile::Child,
    };

    let before = procfs::process::Process::myself()
        .unwrap()
        .statm()
        .unwrap()
        .resident;
    let mut handles = Vec::new();
    for _ in 0..10 {
        let cancel = CancellationToken::new();
        let h = runner.launch(spec.clone(), cancel, None).await.unwrap();
        handles.push(h);
    }
    let after = procfs::process::Process::myself()
        .unwrap()
        .statm()
        .unwrap()
        .resident;
    let page_size_kb = unsafe { libc::sysconf(libc::_SC_PAGESIZE) as u64 / 1024 };
    let delta_kb = (after - before) * page_size_kb;
    println!(
        "RSS delta for 10 agents: {} KB ({} KB/agent)",
        delta_kb,
        delta_kb / 10
    );

    for h in handles {
        h.cancel.cancel();
    }
}

#[tokio::test]
#[ignore = "performance test — run manually with `cargo test --test subagent_spawn -- --ignored --nocapture`"]
async fn spool_write_8k_p95() {
    let tmp = tempfile::tempdir().unwrap();
    let spool = Arc::new(SubagentSpool::new(tmp.path().join("spool")).await.unwrap());

    let chunk = vec![0u8; 8192];
    let iterations = 10_000;
    let mut latencies = Vec::with_capacity(iterations);

    for i in 0..iterations {
        let start = Instant::now();
        spool.append(&format!("task-{}", i), &chunk).await.unwrap();
        latencies.push(start.elapsed());
    }

    latencies.sort();
    let p95 = percentile(&latencies, 95.0);
    println!("spool-write-8k p95={}µs", p95);
}
