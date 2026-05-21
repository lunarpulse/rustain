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
