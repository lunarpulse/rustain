//! Parse workspace `.claude/mcp.json` into `Vec<McpServerSpec>`.
//!
//! Uses the existing `figment::providers::Json` (Story 8.1 7-layer chain)
//! but this module is a standalone helper that can be called with any path.

use crate::domain::models::{McpServerSource, McpServerSpec, McpTransport, expand_env_vars};
use std::collections::BTreeMap;

/// Keep each entry as raw JSON until the per-entry loop. Deserializing the
/// entire map into `McpJsonServer` lets one field-type error erase every healthy
/// sibling before degradation can run.
#[derive(Debug, serde::Deserialize)]
struct McpJsonRoot {
    #[serde(rename = "mcpServers")]
    mcp_servers: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, serde::Deserialize)]
struct McpJsonServer {
    /// 🔴 Story 9.9 (ruling A1, MEASURED): this was a **required, non-`Option`
    /// `String`**, and because the whole file is one `serde_json::from_str`, the
    /// natural Claude Code HTTP entry — `{"type":"http","url":"…"}` with no
    /// `command` — failed the WHOLE FILE with ``missing field `command` `` and
    /// `toml_resolver.rs` swallowed that into a log line. The operator lost
    /// **every** MCP server in `.claude/mcp.json`, healthy stdio ones included,
    /// and was told nothing. ⛔ Do not make this required again.
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    /// Story 9.9 (AC2): the URL an `http` entry needs. Parsed and env-expanded
    /// like `command`/`args`/`env`; previously read and dropped on the floor.
    #[serde(default)]
    url: Option<String>,
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

    for (name, raw_server) in root.mcp_servers {
        let server = match serde_json::from_value::<McpJsonServer>(raw_server.clone()) {
            Ok(server) => server,
            Err(error) => {
                warnings.push(format!("MCP server '{name}': invalid entry: {error}"));
                let transport =
                    transport_from_name(raw_server.get("type").and_then(serde_json::Value::as_str));
                specs.push(McpServerSpec {
                    id: name,
                    transport,
                    command: None,
                    args: Vec::new(),
                    env: BTreeMap::new(),
                    url: None,
                    persistent: false,
                    source: McpServerSource::Workspace,
                });
                continue;
            }
        };
        let transport = transport_from_name(server.transport.as_deref());

        // Expand env vars in command, args, env values — and now `url` too, via
        // the SAME helper with the same unset-variable semantics (the literal is
        // preserved plus a warning; never an error).
        let command = server.command.as_deref().map(|raw| {
            let (expanded, cmd_warnings) = expand_env_vars(raw);
            warnings.extend(cmd_warnings);
            expanded
        });

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

        let url = server.url.as_deref().map(|raw| {
            let (expanded, ws) = expand_env_vars(raw);
            warnings.extend(ws);
            crate::domain::models::redacted_url::RedactedUrl::new(expanded)
        });

        let spec = McpServerSpec {
            id: name,
            transport,
            command,
            args,
            env,
            url,
            persistent: false,
            source: McpServerSource::Workspace,
        };
        if let Err(e) = spec.validate_id() {
            warnings.push(e);
            continue;
        }
        // 9.9 AC2: a transport ↔ field fault degrades PER ENTRY. The entry stays
        // in the list so its healthy siblings survive (ruling A1) and so the
        // fault reaches the operator through `McpClientAdapter`'s `event_tx` as
        // `ConnectionFailed { last_error }` in the status panel — a config
        // mistake that silently removes a row is how this story started.
        if let Err(e) = spec.validate_transport_fields() {
            warnings.push(e);
        }
        specs.push(spec);
    }

    for w in warnings {
        tracing::warn!("{w}");
    }

    Ok(specs)
}

fn transport_from_name(name: Option<&str>) -> McpTransport {
    match name {
        Some("http") => McpTransport::Http,
        Some("sse") => McpTransport::Sse,
        Some("stdio") | None => McpTransport::Stdio,
        Some(other) => {
            tracing::warn!("unknown MCP transport '{other}', defaulting to stdio");
            McpTransport::Stdio
        }
    }
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

    #[test]
    fn malformed_entry_does_not_discard_healthy_siblings() {
        let (_f, path) = temp_json(
            r#"{
                "mcpServers": {
                    "good": {"command": "healthy"},
                    "bad": {"type": "http", "url": 423}
                }
            }"#,
        );

        let specs = parse_workspace_mcp_config(&path).expect("file shape remains valid");
        assert_eq!(specs.len(), 2);
        let good = specs.iter().find(|spec| spec.id == "good").unwrap();
        assert_eq!(good.command.as_deref(), Some("healthy"));
        let bad = specs.iter().find(|spec| spec.id == "bad").unwrap();
        assert_eq!(bad.transport, McpTransport::Http);
        assert!(
            bad.url.is_none(),
            "invalid entry must fail closed at connect"
        );
    }
}
