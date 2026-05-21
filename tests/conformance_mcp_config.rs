//! Conformance tests for MCP configuration layer precedence (AC-1).
//!
//! Verifies that workspace `.claude/mcp.json` entries override profile
//! `[tools.config.mcp.*]` entries by server name, and distinct names are additive.

use rustain::domain::models::{McpServerSource, McpServerSpec, McpTransport};
use std::collections::BTreeMap;

#[test]
fn test_mcp_config_layer_precedence_workspace_wins() {
    let profile_spec = McpServerSpec {
        id: "postgres".into(),
        transport: McpTransport::Stdio,
        command: Some("profile-cmd".into()),
        args: vec![],
        env: BTreeMap::new(),
        url: None,
        persistent: false,
        source: McpServerSource::Profile {
            profile_name: "coding".into(),
        },
    };

    let workspace_spec = McpServerSpec {
        id: "postgres".into(),
        transport: McpTransport::Stdio,
        command: Some("workspace-cmd".into()),
        args: vec!["--ws".into()],
        env: BTreeMap::new(),
        url: None,
        persistent: false,
        source: McpServerSource::Workspace,
    };

    let merged = rustain::adapters::mcp::merge_mcp_specs(vec![workspace_spec], vec![profile_spec]);

    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].command.as_deref(), Some("workspace-cmd"));
    assert_eq!(merged[0].source, McpServerSource::Workspace);
}

#[test]
fn test_mcp_config_layer_precedence_additive() {
    let profile_spec = McpServerSpec {
        id: "postgres".into(),
        transport: McpTransport::Stdio,
        command: Some("pg-cmd".into()),
        args: vec![],
        env: BTreeMap::new(),
        url: None,
        persistent: false,
        source: McpServerSource::Profile {
            profile_name: "coding".into(),
        },
    };

    let workspace_spec = McpServerSpec {
        id: "git".into(),
        transport: McpTransport::Stdio,
        command: Some("git-cmd".into()),
        args: vec![],
        env: BTreeMap::new(),
        url: None,
        persistent: false,
        source: McpServerSource::Workspace,
    };

    let merged = rustain::adapters::mcp::merge_mcp_specs(vec![workspace_spec], vec![profile_spec]);

    assert_eq!(merged.len(), 2);
    let ids: Vec<_> = merged.iter().map(|s| s.id.as_str()).collect();
    assert!(ids.contains(&"postgres"));
    assert!(ids.contains(&"git"));
}

#[test]
fn test_mcp_config_layer_precedence_empty() {
    let merged = rustain::adapters::mcp::merge_mcp_specs(vec![], vec![]);
    assert!(merged.is_empty());
}
