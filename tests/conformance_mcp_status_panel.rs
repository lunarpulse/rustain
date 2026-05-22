//! Conformance tests for MCP status panel rendering — Story 9.1 AC-5.
//!
//! Verifies that MCP sub-rows appear in the adapter status panel output
//! when a CompositeToolsetAdapter is loaded into the tools port.

use std::collections::BTreeMap;
use std::sync::Arc;

use rustain::adapters::composite_toolset_adapter::CompositeToolsetAdapter;
use rustain::adapters::mcp::client::McpClientAdapter;
use rustain::adapters::noop::NoOpToolSet;
use rustain::domain::events::AppEvent;
use rustain::domain::models::{McpServerSource, McpServerSpec, McpTransport};
use rustain::domain::ports::ToolSetPort;

fn make_spec(id: &str) -> McpServerSpec {
    McpServerSpec {
        id: id.to_string(),
        transport: McpTransport::Stdio,
        command: Some("echo".into()),
        args: vec![],
        env: BTreeMap::new(),
        url: None,
        persistent: false,
        source: McpServerSource::Workspace,
    }
}

/// AC-5: MCP health sub-rows are produced for each MCP client.
/// This tests the `mcp_health_rows()` method directly (used by
/// the adapter status panel's `get_mcp_health_rows`).
#[test]
fn test_mcp_sub_rows_appear_in_health_output() {
    let builtin: Arc<dyn ToolSetPort> = Arc::new(NoOpToolSet);
    let spec_a = make_spec("test-server-a");
    let spec_b = make_spec("test-server-b");

    let (tx_a, _rx_a) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
    let (tx_b, _rx_b) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();

    let client_a = Arc::new(McpClientAdapter::new(spec_a.clone(), Some(tx_a)));
    let client_b = Arc::new(McpClientAdapter::new(spec_b.clone(), Some(tx_b)));

    let composite = CompositeToolsetAdapter::new(
        builtin,
        vec![client_a, client_b],
        vec![spec_a, spec_b],
        true,
        None,
        None,
    );

    let rows = composite.mcp_health_rows();
    assert_eq!(rows.len(), 2, "should have 2 MCP health sub-rows");

    let names: Vec<&str> = rows.iter().map(|r| r.server_name.as_str()).collect();
    assert!(
        names.contains(&"test-server-a"),
        "should contain test-server-a"
    );
    assert!(
        names.contains(&"test-server-b"),
        "should contain test-server-b"
    );

    for row in &rows {
        assert!(
            !row.transport.is_empty(),
            "transport should be populated for {}",
            row.server_name
        );
        assert!(
            !row.metric.is_empty(),
            "metric should be populated for {}",
            row.server_name
        );
    }
}
