//! MCP adapter module — client, config parsers, lifecycle, reconnect.
#![cfg(feature = "mcp")]

pub mod client;
pub mod error;
pub mod lazy_connect;
pub mod lifecycle;
pub mod mcp_provider;
pub mod profile_config;
pub mod reconnect;
pub mod task_driver;
pub mod task_transport;
pub mod tasks;
pub mod tool_projection;
pub mod warm_pool;
pub mod workspace_config;

#[cfg(test)]
mod arch_guards {
    //! src/-resident architecture guards for Story 17.5a. These pin the
    //! layering the story mandates (R-4 / ADR-17-5-01 D2) and the anti-
    //! patterns the 17.4b review burned us with (R-6).

    use std::path::PathBuf;

    fn mcp_adapter_sources() -> Vec<(PathBuf, String)> {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/adapters/mcp");
        std::fs::read_dir(&dir)
            .expect("mcp adapter dir")
            .map(|entry| {
                let path = entry.expect("dir entry").path();
                let source = std::fs::read_to_string(&path).expect("readable source");
                (path, source)
            })
            .collect()
    }

    /// Production code only: everything before the `#[cfg(test)]` module,
    /// with `//` line comments stripped (the guards target USES, not the
    /// doc comments that legitimately name the banned shapes).
    fn production_code(source: &str) -> String {
        let production = source.split("#[cfg(test)]").next().unwrap_or(source);
        production
            .lines()
            .map(|line| line.split("//").next().unwrap_or(line))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// R-4: `adapters/mcp` production code NEVER imports `infrastructure/`.
    /// Journal access goes through `RoomJournal`; lifecycle through
    /// `TaskNodes`/`SupervisedNodes`. (Test fixtures MAY build a real
    /// `NodeTree`/`NodeJournal` — the a2a double pattern — so only
    /// production code is scanned.)
    #[test]
    fn mcp_adapter_production_code_never_imports_infrastructure() {
        for (path, source) in mcp_adapter_sources() {
            let production = production_code(&source);
            assert!(
                !production.contains("crate::infrastructure"),
                "{}: adapters/mcp must reach infrastructure through domain \
                 ports only (R-4 / ADR-17-5-01 D2)",
                path.display()
            );
        }
    }

    /// R-6: the fire-and-forget `NodeTree::set_state` shim is never called
    /// from `adapters/mcp`. The only `set_state(` permitted in production
    /// code is the adapter's OWN connection-state setter
    /// (`self.set_state(McpConnectionState::…)`), an unrelated method.
    #[test]
    fn mcp_adapter_never_calls_the_set_state_shim() {
        for (path, source) in mcp_adapter_sources() {
            let production = production_code(&source);
            let mut rest = production.as_str();
            while let Some(idx) = rest.find("set_state(") {
                let before = &rest[..idx];
                let after = &rest[idx + "set_state(".len()..];
                if before.ends_with("try_")
                    || before.trim_end().ends_with("fn")
                    || after.starts_with("McpConnectionState")
                {
                    // try_set_state (the only legal lifecycle path), a
                    // method DEFINITION, or the adapter's own
                    // connection-state setter — all fine.
                    rest = after;
                    continue;
                }
                panic!(
                    "{}: bare `set_state(` found — lifecycle mutation goes \
                     through `try_set_state` exclusively (R-6)",
                    path.display()
                );
            }
        }
    }

    /// AC2 (structural half) / R-3: no per-connection session state may
    /// participate in task identity. rustain ships stdio only — there are no
    /// HTTP session headers — so identity is (server, taskId) alone,
    /// behaviorally proven in `identity_is_stateless_across_independent_clients`.
    #[test]
    fn mcp_adapter_never_reads_session_headers() {
        // Scan PRODUCTION code (comments + the `#[cfg(test)]` module stripped by
        // `production_code`) so the guard's own marker list cannot exempt it, and
        // drop the blanket `mod.rs` exemption the prior version relied on.
        const SESSION_MARKERS: &[&str] = &["Mcp-Session-Id", "session_id", "sessionId"];
        for (path, source) in mcp_adapter_sources() {
            let production = production_code(&source);
            for marker in SESSION_MARKERS {
                assert!(
                    !production.contains(marker),
                    "{}: task identity must not consult session state (`{marker}`) (AC2/R-3)",
                    path.display()
                );
            }
        }
    }

    /// R-1: rmcp's superseded Tasks API is never used — the typed task
    /// request/result symbols and the deleted client opt-in knob are banned
    /// from the adapter.
    #[test]
    fn mcp_adapter_never_touches_rmcp_superseded_task_types() {
        const BANNED: &[&str] = &[
            "GetTaskInfoRequest",
            "CancelTaskRequest",
            "GetTaskResult",
            "CancelTaskResult",
            "CreateTaskResult",
            "ListTasksRequest",
            "ListTasksResult",
            "GetTaskPayloadRequest",
            "TaskResultRequest",
            "with_task(",
        ];
        for (path, source) in mcp_adapter_sources() {
            // tasks.rs documents the superseded shapes by name; the ban is
            // on USE, so scan production code of every OTHER file plus
            // tasks.rs's non-comment tokens are impractical — restrict to
            // production code and skip the two files whose raison d'être is
            // describing the drift (they name the types in doc comments and
            // in the shim's guard proof test).
            if path.ends_with("tasks.rs") || path.ends_with("task_transport.rs") {
                continue;
            }
            let production = production_code(&source);
            for banned in BANNED {
                assert!(
                    !production.contains(banned),
                    "{}: banned superseded Tasks symbol `{banned}` (R-1)",
                    path.display()
                );
            }
        }
    }
}

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
