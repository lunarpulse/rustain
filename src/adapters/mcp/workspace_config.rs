//! Parse workspace `.claude/mcp.json` into `Vec<McpServerSpec>`.
//!
//! Uses the existing `figment::providers::Json` (Story 8.1 7-layer chain)
//! but this module is a standalone helper that can be called with any path.

use crate::domain::models::{McpServerSource, McpServerSpec, McpTransport, expand_env_vars};
use std::collections::BTreeMap;

/// Top-level shape of `.claude/mcp.json` (Claude Code format).
#[derive(Debug, serde::Deserialize)]
struct McpJsonRoot {
    #[serde(rename = "mcpServers")]
    mcp_servers: BTreeMap<String, McpJsonServer>,
}

#[derive(Debug, serde::Deserialize)]
struct McpJsonServer {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    #[serde(rename = "type")]
    transport: Option<String>,
}

/// Parse a `.claude/mcp.json` file at the given path.
///
/// Returns `Ok(Vec<McpServerSpec>)` on success, or `Err` with a user-facing
/// message if the file is missing, malformed, or contains unsupported transports.
pub fn parse_workspace_mcp_config(path: &std::path::Path) -> Result<Vec<McpServerSpec>, String> {
    if !path.exists() {
        return Ok(Vec::new());
    }

    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;

    let root: McpJsonRoot = serde_json::from_str(&content)
        .map_err(|e| format!("Invalid JSON in {}: {}", path.display(), e))?;

    let mut specs = Vec::with_capacity(root.mcp_servers.len());
    let mut warnings = Vec::new();

    for (name, server) in root.mcp_servers {
        let transport = match server.transport.as_deref() {
            Some("http") => McpTransport::Http,
            Some("sse") => McpTransport::Sse,
            Some("stdio") | None => McpTransport::Stdio,
            Some(other) => {
                warnings.push(format!(
                    "MCP server '{name}': unknown transport '{other}', defaulting to stdio"
                ));
                McpTransport::Stdio
            }
        };

        // Expand env vars in command, args, and env values
        let (command, cmd_warnings) = expand_env_vars(&server.command);
        warnings.extend(cmd_warnings);

        let args: Vec<String> = server
            .args
            .iter()
            .map(|a| {
                let (expanded, ws) = expand_env_vars(a);
                warnings.extend(ws);
                expanded
            })
            .collect();

        let env: BTreeMap<String, String> = server
            .env
            .iter()
            .map(|(k, v)| {
                let (expanded, ws) = expand_env_vars(v);
                warnings.extend(ws);
                (k.clone(), expanded)
            })
            .collect();

        let spec = McpServerSpec {
            id: name,
            transport,
            command: Some(command),
            args,
            env,
            url: None,
            persistent: false,
            source: McpServerSource::Workspace,
        };
        if let Err(e) = spec.validate_id() {
            warnings.push(e);
            continue;
        }
        specs.push(spec);
    }

    for w in warnings {
        tracing::warn!("{w}");
    }

    Ok(specs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_json(content: &str) -> (tempfile::NamedTempFile, std::path::PathBuf) {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(content.as_bytes()).unwrap();
        let path = file.path().to_path_buf();
        (file, path)
    }

    #[test]
    fn test_parse_basic_stdio() {
        let (_f, path) = temp_json(
            r#"{
            "mcpServers": {
                "postgres": {
                    "command": "mcp-server-postgres",
                    "args": ["--connection-string", "$DATABASE_URL"]
                }
            }
        }"#,
        );

        let specs = parse_workspace_mcp_config(&path).unwrap();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].id, "postgres");
        assert_eq!(specs[0].transport, McpTransport::Stdio);
        assert_eq!(specs[0].command.as_deref(), Some("mcp-server-postgres"));
        assert_eq!(specs[0].args.len(), 2);
        assert_eq!(specs[0].source, McpServerSource::Workspace);
    }

    #[test]
    fn test_parse_missing_file_returns_empty() {
        let path = std::path::PathBuf::from("/nonexistent/mcp.json");
        let specs = parse_workspace_mcp_config(&path).unwrap();
        assert!(specs.is_empty());
    }

    #[test]
    fn test_parse_http_transport() {
        let (_f, path) = temp_json(
            r#"{
            "mcpServers": {
                "remote": {
                    "command": "ignored",
                    "type": "http"
                }
            }
        }"#,
        );

        let specs = parse_workspace_mcp_config(&path).unwrap();
        assert_eq!(specs[0].transport, McpTransport::Http);
    }
}
