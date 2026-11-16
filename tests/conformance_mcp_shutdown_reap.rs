#![cfg(all(target_os = "linux", feature = "test-fake-mcp"))]
//! Story C-3 (C3-01 / R2-01) — daemon shutdown stops real MCP children even when
//! the graceful path cannot finish: a disconnect that outlives its bound, and a
//! connect aborted while the server is provably inside `tools/list`.
//!
//! Real `fake-mcp-server` children. "Terminated" means `/proc/<pid>` is gone or
//! the process is a zombie awaiting its owner; reaping (by the owning `Child` /
//! Tokio) is asserted separately within a bound.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustain::adapters::composite_toolset_adapter::CompositeToolsetAdapter;
use rustain::adapters::mcp::client::McpClientAdapter;
use rustain::adapters::noop::NoOpToolSet;
use rustain::domain::models::{McpServerSource, McpServerSpec, McpTransport};

use crate::common;

fn spec(id: &str, knobs: &[(&str, String)]) -> McpServerSpec {
    let mut env = BTreeMap::new();
    for (key, value) in knobs {
        env.insert((*key).to_string(), value.clone());
    }
    McpServerSpec {
        id: id.to_string(),
        transport: McpTransport::Stdio,
        command: Some(common::fake_mcp_binary().to_string_lossy().into_owned()),
        args: vec![],
        env,
        url: None,
        persistent: false,
        source: McpServerSource::Workspace,
    }
}

fn composite(client: &Arc<McpClientAdapter>, spec: McpServerSpec) -> CompositeToolsetAdapter {
    CompositeToolsetAdapter::new(
        Arc::new(NoOpToolSet),
        vec![Arc::clone(client)],
        vec![spec],
        true,
        None,
        None,
        None,
    )
}

fn terminated(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        Ok(stat) => stat
            .rsplit(')')
            .next()
            .is_some_and(|rest| rest.trim_start().starts_with('Z')),
    }
}

async fn reaped_within(pid: u32, secs: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Path::new(&format!("/proc/{pid}")).exists() {
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    true
}

/// The graceful close waits up to 2 s for a child that ignores stdin EOF; with a
/// 100 ms bound the disconnect future is dropped, and the child must still have
/// exited when `shutdown_mcp_clients` returns (and be counted as killed).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_disconnect_past_the_bound_still_ends_with_the_child_stopped() {
    if !rustain::adapters::mcp::child_handle::pidfd_supported() {
        let _ = std::io::Write::write_all(
            &mut std::io::stderr(),
            b"SKIPPED a_disconnect_past_the_bound_still_ends_with_the_child_stopped: pidfd_open unavailable\n",
        );
        return;
    }
    let spec = spec("stubborn", &[("FAKE_MCP_IGNORE_STDIN_EOF", "1".into())]);
    let client = Arc::new(McpClientAdapter::new(spec.clone(), None));
    client.connect().await.expect("fake server connects");
    let pid = client
        .live_child_pids()
        .await
        .first()
        .copied()
        .expect("child registered");
    assert!(
        !terminated(pid),
        "control: the child is alive before shutdown"
    );
    let composite = composite(&client, spec);

    let (closed, killed) = composite
        .shutdown_mcp_clients(Duration::from_millis(100))
        .await;

    assert_eq!((closed, killed), (0, 1), "a forced stop counts as killed");
    assert!(
        terminated(pid),
        "the child must have exited when shutdown returns"
    );
    assert!(reaped_within(pid, 5).await, "and be reaped by its owner");
}

/// Aborting the connect supervisor while the server is provably INSIDE
/// `tools/list` (it signals entry and never answers) leaves its child to an
/// asynchronous close; shutdown must still end with it exited.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopping_inside_tools_list_leaves_no_running_child() {
    if !rustain::adapters::mcp::child_handle::pidfd_supported() {
        let _ = std::io::Write::write_all(
            &mut std::io::stderr(),
            b"SKIPPED stopping_inside_tools_list_leaves_no_running_child: pidfd_open unavailable\n",
        );
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let entered = tmp.path().join("list-entered");
    let spec = spec(
        "hanging",
        &[
            ("FAKE_MCP_HANG_TOOLS_LIST", "1".into()),
            (
                "FAKE_MCP_LIST_ENTERED_FILE",
                entered.to_string_lossy().into_owned(),
            ),
        ],
    );
    let client = Arc::new(McpClientAdapter::new(spec.clone(), None));
    let composite = composite(&client, spec);
    composite.start_mcp_connections();

    let deadline = Instant::now() + Duration::from_secs(15);
    while !entered.exists() {
        assert!(
            Instant::now() < deadline,
            "the server never entered tools/list"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let pid = client
        .live_child_pids()
        .await
        .first()
        .copied()
        .expect("child registered");
    assert!(!terminated(pid), "control: alive inside tools/list");
    assert!(
        !composite.is_mcp_settled(),
        "the connect is still in flight"
    );

    composite.abort_mcp_supervisors().await;
    composite.shutdown_mcp_clients(Duration::from_secs(3)).await;

    assert!(
        terminated(pid),
        "an aborted in-flight connect must not leave its child running"
    );
    assert!(reaped_within(pid, 5).await);
}

/// Pre-C-3 spawn shape is unchanged (Round 5): an MCP child stays in the parent's
/// process group (no `process_group(0)`) and inherits the parent's stderr (no
/// relay), for the daemon and the TUI alike (one shared spawn).
#[tokio::test]
async fn an_mcp_child_keeps_the_parents_process_group_and_stderr() {
    use rustain::adapters::mcp::task_transport::TaskGuardTransport;
    let mut cmd = tokio::process::Command::new("sleep");
    cmd.arg("30").kill_on_drop(true);
    let transport = TaskGuardTransport::spawn(&mut cmd).expect("spawn");
    let pid = transport.child_id().expect("pid");
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).expect("stat");
    let after_comm = stat.rsplit(')').next().expect("stat tail");
    // Fields after `)`: state ppid pgrp ...
    let pgrp: i32 = after_comm
        .split_whitespace()
        .nth(2)
        .expect("pgrp")
        .parse()
        .expect("int");
    // SAFETY: getpgrp has no preconditions.
    assert_eq!(pgrp, unsafe { libc::getpgrp() }, "no own process group");
    let child_err = std::fs::read_link(format!("/proc/{pid}/fd/2")).expect("child fd 2");
    let own_err = std::fs::read_link("/proc/self/fd/2").expect("own fd 2");
    assert_eq!(
        child_err, own_err,
        "stderr is inherited, not piped to a relay"
    );
    drop(transport);
}
