//! MCP adapter module — client, config parsers, lifecycle, reconnect.
#![cfg(feature = "mcp")]

pub mod client;
pub mod error;
pub mod lazy_connect;
pub mod lifecycle;
pub mod mcp_provider;
pub mod profile_config;
pub mod reconnect;
pub mod tool_projection;
pub mod warm_pool;
pub mod workspace_config;

use crate::domain::models::{McpServerSpec, McpTransport};

/// Merge workspace and profile MCP server specs.
///
/// **Precedence rule:** workspace entries override profile entries by server name;
/// distinct names are additive. This matches the figment layer precedence
/// (workspace = layer 3, profile = layer 6 per Story 8.1).
///
/// Emits transport warnings for non-stdio servers (AC-1 P-5).
pub fn merge_mcp_specs(
    workspace: Vec<McpServerSpec>,
    profile: Vec<McpServerSpec>,
) -> Vec<McpServerSpec> {
    let mut merged: std::collections::BTreeMap<String, McpServerSpec> =
        std::collections::BTreeMap::new();

    // Profile entries first (lower precedence)
    for spec in profile {
        merged.insert(spec.id.clone(), spec);
    }

    // Workspace entries override (higher precedence)
    for spec in workspace {
        merged.insert(spec.id.clone(), spec);
    }

    merged.into_values().collect()
}

/// Emit startup SystemNotice warnings for non-stdio transports per AC-1.
pub fn emit_transport_warnings(specs: &[McpServerSpec]) {
    for spec in specs {
        match spec.transport {
            McpTransport::Http => {
                tracing::warn!(
                    "MCP server '{}': http transport deferred to a later Epic 9 story; skipping",
                    spec.id
                );
            }
            McpTransport::Sse => {
                tracing::warn!(
                    "MCP server '{}': SSE transport is not supported (deprecated by MCP spec 2025-03-26 per ADR-06-08). Use a proxy like mcp-proxy, or update the server to Streamable HTTP.",
                    spec.id
                );
            }
            McpTransport::Stdio => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::{McpServerSource, McpTransport};

    fn dummy_spec(id: &str, source: McpServerSource) -> McpServerSpec {
        McpServerSpec {
            id: id.to_string(),
            transport: McpTransport::Stdio,
            command: Some("cmd".into()),
            args: vec![],
            env: std::collections::BTreeMap::new(),
            url: None,
            persistent: false,
            source,
        }
    }

    #[test]
    fn test_merge_workspace_wins() {
        let profile = vec![dummy_spec(
            "pg",
            McpServerSource::Profile {
                profile_name: "coding".into(),
            },
        )];
        let mut workspace = vec![dummy_spec("pg", McpServerSource::Workspace)];
        workspace[0].command = Some("workspace-cmd".into());

        let merged = merge_mcp_specs(workspace, profile);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].command.as_deref(), Some("workspace-cmd"));
        assert_eq!(merged[0].source, McpServerSource::Workspace);
    }

    #[test]
    fn test_merge_additive() {
        let profile = vec![dummy_spec(
            "pg",
            McpServerSource::Profile {
                profile_name: "coding".into(),
            },
        )];
        let workspace = vec![dummy_spec("git", McpServerSource::Workspace)];

        let merged = merge_mcp_specs(workspace, profile);
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn test_merge_empty() {
        let merged = merge_mcp_specs(vec![], vec![]);
        assert!(merged.is_empty());
    }
}
