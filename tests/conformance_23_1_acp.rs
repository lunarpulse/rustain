//! Story 23.1 review round 1, ACP: a `[sandbox]` refusal reaches the client
//! (CR231-07), and a terminal interrupt stops the MCP servers' process groups
//! (CR231-03).

use std::sync::Arc;

use rustain::domain::models::AppConfig;
use rustain::infrastructure::sandbox::{HostPaths, LandlockProbe, SandboxProbe};

/// CR231-07: with `adapter = "landlock"` and no usable Landlock, `session/new`
/// fails with the shared refusal text in the protocol error's data.
#[test]
fn cr231_07_acp_session_refusal_carries_the_sandbox_notice() {
    let workspace = tempfile::tempdir().unwrap();
    let mut config = AppConfig::default();
    config.sandbox.adapter = "landlock".into();
    let probe = SandboxProbe {
        landlock: LandlockProbe::Unavailable {
            reason: "Landlock is disabled at boot".into(),
        },
        host: HostPaths::default(),
        inject_not_enforced: false,
    };
    let factory = rustain::adapters::acp::run::acp_core_factory(
        config,
        Arc::new(rustain::adapters::noop::NoOpPersona),
        Some(probe),
    );
    let error = match factory(workspace.path(), &[]) {
        Ok(_) => panic!("a required sandbox that is unavailable must refuse the session"),
        Err(error) => error,
    };
    let error = serde_json::to_value(&error).unwrap();
    let notice = error["data"]["sandbox"]
        .as_str()
        .unwrap_or_else(|| panic!("the refusal text travels in the error data: {error}"));
    assert!(
        notice.starts_with(
            "[sandbox].adapter = \"landlock\" requires Landlock, but Landlock is disabled at boot."
        ),
        "{notice}"
    );
}

/// CR231-03: SIGINT to `rustain acp` ends its input, so the session cleanup
/// stops the MCP server's process group; the server's background descendant
/// (and the server, which ignores stdin EOF) do not outlive rustain.
#[cfg(all(target_os = "linux", feature = "test-fake-mcp"))]
#[tokio::test]
async fn cr231_03_sigint_stops_the_mcp_process_groups() {
    use std::process::Stdio;
    use std::time::{Duration, Instant};
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

    fn gone(pid: i32) -> bool {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Err(_) => true,
            Ok(stat) => stat
                .rsplit(')')
                .next()
                .is_some_and(|rest| rest.trim_start().starts_with('Z')),
        }
    }

    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let cfg = tempfile::tempdir().unwrap();
    let pid_file = workspace.path().join("descendant.pid");
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_rustain"))
        .arg("acp")
        .current_dir(workspace.path())
        .env("RUSTAIN_DATA_DIR", data.path())
        .env("RUSTAIN_CONFIG_DIR", cfg.path())
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn `rustain acp`");
    let rustain_pid = child.id().expect("pid") as i32;
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let send = |body: serde_json::Value| format!("{body}\n");
    stdin
        .write_all(
            send(serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": { "protocolVersion": 1, "clientCapabilities": {} }
            }))
            .as_bytes(),
        )
        .await
        .unwrap();
    let script = format!(
        "sleep 300 & echo $! > '{}'; exec \"$FAKE_MCP\"",
        pid_file.display()
    );
    stdin
        .write_all(
            send(serde_json::json!({
                "jsonrpc": "2.0", "id": 2, "method": "session/new",
                "params": {
                    "cwd": workspace.path(),
                    "mcpServers": [{
                        "name": "group",
                        "command": "sh",
                        "args": ["-c", script],
                        "env": [
                            { "name": "FAKE_MCP", "value": env!("CARGO_BIN_EXE_fake-mcp-server") },
                            { "name": "FAKE_MCP_IGNORE_STDIN_EOF", "value": "1" }
                        ]
                    }]
                }
            }))
            .as_bytes(),
        )
        .await
        .unwrap();
    stdin.flush().await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut session_ready = false;
    while Instant::now() < deadline && !session_ready {
        if let Ok(Ok(Some(raw))) =
            tokio::time::timeout(Duration::from_millis(200), lines.next_line()).await
        {
            session_ready = serde_json::from_str::<serde_json::Value>(&raw)
                .is_ok_and(|v| v["id"] == 2 && v.get("result").is_some());
        }
    }
    assert!(session_ready, "session/new answered");
    while !pid_file.exists() {
        assert!(Instant::now() < deadline, "the MCP server never started");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let descendant: i32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // Let the handshake settle so the server is the running service.
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(!gone(descendant), "control: the descendant runs");

    // SAFETY: signalling our own child process.
    assert_eq!(unsafe { libc::kill(rustain_pid, libc::SIGINT) }, 0);
    let exited = tokio::time::timeout(Duration::from_secs(15), child.wait()).await;
    assert!(exited.is_ok(), "rustain acp exits after SIGINT");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !gone(descendant) {
        assert!(
            Instant::now() < deadline,
            "the MCP server's descendant {descendant} outlived `rustain acp`"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
