//! Story 9.1 — non-stdio transports surface `Unsupported` without spawning.
//!
//! ADR-06-08 rejects SSE in favor of stdio + Streamable HTTP. docs/mcp.md
//! tells users to install a proxy like mcp-proxy or update the server. The
//! contract: `McpClientAdapter::connect()` on an SSE-transport spec
//!   1. transitions state to `Unsupported { reason }`,
//!   2. returns `Err(McpError::Unsupported(_))`,
//!   3. **does not** fork a child process.
//!
//! Risk closed:
//!   * R14 — SSE transport surfaces `Unsupported` state without subprocess spawn
//!
//! Http is included as a sibling case because both Http and Sse currently land
//! in the same Unsupported branch (Story 9.1 deferred Streamable HTTP).
//! When Http support lands, this test should split into:
//!   * SSE → Unsupported (forever)
//!   * Http → Connected (post-Story-9.X)

#![cfg(feature = "mcp")]

use std::collections::BTreeMap;

use rustain::adapters::mcp::client::McpClientAdapter;
use rustain::adapters::mcp::error::McpError;
use rustain::domain::events::AppEvent;
use rustain::domain::models::{McpConnectionState, McpServerSource, McpServerSpec, McpTransport};

fn spec_with_transport(id: &str, transport: McpTransport) -> McpServerSpec {
    McpServerSpec {
        id: id.into(),
        transport,
        // `command` is required by serde for stdio specs but is irrelevant
        // for non-stdio — the connect() guard returns before it's read.
        // We deliberately use a path that would FAIL if anything tried to
        // spawn it, so a regression that ignores the transport guard would
        // either crash here or land in ConnectionFailed (not Unsupported).
        command: Some("/nonexistent/binary-must-not-spawn".into()),
        args: vec![],
        env: BTreeMap::new(),
        url: Some("http://localhost:9999/sse".into()),
        persistent: false,
        source: McpServerSource::Workspace,
    }
}

fn pgrep_alive(needle: &str) -> bool {
    let output = std::process::Command::new("pgrep")
        .args(["-f", needle])
        .output();
    matches!(output, Ok(out) if !out.stdout.is_empty())
}

#[tokio::test]
async fn r14_sse_transport_returns_unsupported_and_does_not_spawn() {
    let spec = spec_with_transport("r14-sse", McpTransport::Sse);
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
    let client = McpClientAdapter::new(spec, Some(tx));

    let err = client
        .connect()
        .await
        .expect_err("SSE connect must error, not succeed");
    assert!(
        matches!(err, McpError::Unsupported(_)),
        "expected McpError::Unsupported, got {:?}",
        err
    );

    match client.state() {
        McpConnectionState::Unsupported { reason } => {
            assert!(
                reason.to_lowercase().contains("sse"),
                "Unsupported reason should mention SSE; got: {reason}"
            );
        }
        other => panic!(
            "expected Unsupported state after SSE connect, got {:?}",
            other
        ),
    }

    // Belt-and-braces: the "would fail to spawn" binary path must never have
    // been forked. If the guard regressed, we'd either see a child OR a
    // ConnectionFailed state — both are caught above + here.
    assert!(
        !pgrep_alive("/nonexistent/binary-must-not-spawn"),
        "SSE connect must not invoke the spawn path — no child should exist"
    );
}

#[tokio::test]
async fn r14_http_transport_also_currently_unsupported() {
    // This test documents the *current* state of Story 9.1. When Streamable
    // HTTP lands, this test should be deleted (not retrofitted) — the new
    // behaviour is a successful connect, which is a different contract that
    // deserves its own dedicated test.
    let spec = spec_with_transport("r14-http", McpTransport::Http);
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
    let client = McpClientAdapter::new(spec, Some(tx));

    let err = client
        .connect()
        .await
        .expect_err("Http connect must currently error per Story 9.1 deferral");
    assert!(
        matches!(err, McpError::Unsupported(_)),
        "expected McpError::Unsupported for Http (pre-Streamable-HTTP), got {:?}",
        err
    );
    assert!(
        matches!(client.state(), McpConnectionState::Unsupported { .. }),
        "expected Unsupported state for Http"
    );
}
