//! Extract `[tools.config.mcp.<server>]` tables from a `ResolvedProfile`.

use crate::domain::models::{McpServerSource, McpServerSpec, McpTransport, expand_env_vars};
use std::collections::BTreeMap;

/// Walk a profile's `[tools.config]` TOML value and extract any `[tools.config.mcp.*]`
/// subtables into `Vec<McpServerSpec>`.
pub fn extract_profile_mcp_servers(
    tools_config: Option<&toml::Value>,
    profile_name: &str,
) -> Vec<McpServerSpec> {
    let tools_config = match tools_config {
        Some(v) => v,
        None => return Vec::new(),
    };

    let mcp_table = match tools_config.get("mcp") {
        Some(toml::Value::Table(t)) => t,
        _ => return Vec::new(),
    };

    let mut specs = Vec::new();
    for (server_name, server_val) in mcp_table {
        let table = match server_val.as_table() {
            Some(t) => t,
            None => continue,
        };

        let transport = table
            .get("transport")
            .and_then(|v| v.as_str())
            .map(|s| match s {
                "http" => McpTransport::Http,
                "sse" => McpTransport::Sse,
                _ => McpTransport::Stdio,
            })
            .unwrap_or_default();

        let command = table
            .get("command")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let args: Vec<String> = table
            .get("args")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        let env: BTreeMap<String, String> = table
            .get("env")
            .and_then(|v| v.as_table())
            .map(|t| {
                t.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();

        let persistent = table
            .get("persistent")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // Expand env vars
        let command = command.map(|c| {
            let (expanded, warnings) = expand_env_vars(&c);
            for w in &warnings {
                tracing::warn!("MCP server '{server_name}': {w}");
            }
            expanded
        });
        let args: Vec<String> = args
            .into_iter()
            .map(|a| {
                let (expanded, warnings) = expand_env_vars(&a);
                for w in &warnings {
                    tracing::warn!("MCP server '{server_name}': {w}");
                }
                expanded
            })
            .collect();
        let env: BTreeMap<String, String> = env
            .into_iter()
            .map(|(k, v)| {
                let (expanded, warnings) = expand_env_vars(&v);
                for w in &warnings {
                    tracing::warn!("MCP server '{server_name}': {w}");
                }
                (k, expanded)
            })
            .collect();

        let spec = McpServerSpec {
            id: server_name.clone(),
            transport,
            command,
            args,
            env,
            url: None,
            persistent,
            source: McpServerSource::Profile {
                profile_name: profile_name.to_string(),
            },
        };
        if let Err(e) = spec.validate_id() {
            tracing::error!("{e} — skipping server");
            continue;
        }
        specs.push(spec);
    }

    specs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_no_config() {
        let specs = extract_profile_mcp_servers(None, "coding");
        assert!(specs.is_empty());
    }

    #[test]
    fn test_extract_single_server() {
        let toml_str = r#"
[mcp.postgres]
transport = "stdio"
command = "mcp-server-postgres"
args = ["--connection-string", "$DATABASE_URL"]
persistent = false
"#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();
        let specs = extract_profile_mcp_servers(Some(&value), "coding");
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].id, "postgres");
        assert_eq!(specs[0].transport, McpTransport::Stdio);
        assert!(!specs[0].persistent);
        assert!(matches!(specs[0].source, McpServerSource::Profile { .. }));
    }

    #[test]
    fn test_extract_default_transport() {
        let toml_str = r#"
[mcp.git]
command = "mcp-server-git"
"#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();
        let specs = extract_profile_mcp_servers(Some(&value), "coding");
        assert_eq!(specs[0].transport, McpTransport::Stdio);
    }
}
