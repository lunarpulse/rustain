use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Specification for an MCP server, parsed from workspace `.claude/mcp.json`
/// or per-profile `[tools.config.mcp.<server>]` TOML.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerSpec {
    /// Server name from the table key (e.g., "postgres").
    pub id: String,
    /// Transport protocol.
    pub transport: McpTransport,
    /// Command to spawn (required for stdio; ignored otherwise).
    pub command: Option<String>,
    /// Arguments for the command (default empty).
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment variables (default empty).
    /// Values support `$VAR` / `${VAR}` expansion at spawn time.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// URL for HTTP/SSE transports (ignored for stdio).
    pub url: Option<String>,
    /// If true, preserve across profile switches via warm-storage pool.
    #[serde(default)]
    pub persistent: bool,
    /// Where this spec originated (workspace JSON vs profile TOML).
    pub source: McpServerSource,
}

/// Transport protocols supported by MCP.
/// Story 9.1 implements stdio end-to-end; HTTP and SSE are accepted at parse
/// time but deferred / rejected with a user-facing notice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpTransport {
    Stdio,
    Http,
    Sse,
}

impl Default for McpTransport {
    fn default() -> Self {
        Self::Stdio
    }
}

/// Provenance of an `McpServerSpec` — used for merge precedence and UI labels.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum McpServerSource {
    /// Came from `.claude/mcp.json` in the workspace root.
    Workspace,
    /// Came from `~/.config/rustain/<profile>.toml`.
    Profile { profile_name: String },
}

/// Expand `$VAR` and `${VAR}` in a string using the current process environment.
/// Unknown variables are left as literal tokens and a warning flag is returned.
impl McpServerSpec {
    pub fn validate_id(&self) -> Result<(), String> {
        if self.id.trim().is_empty() {
            return Err("MCP server id must not be empty or whitespace".to_string());
        }
        if self.id.contains("__") {
            return Err(format!(
                "MCP server id {:?} must not contain double-underscore (__); it conflicts with the mcp__<server>__<tool> naming convention",
                self.id
            ));
        }
        Ok(())
    }
}

pub fn expand_env_vars(input: &str) -> (String, Vec<String>) {
    let mut output = String::with_capacity(input.len());
    let mut warnings = Vec::new();
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '$' {
            if let Some(&'{') = chars.peek() {
                // ${VAR} form
                chars.next(); // consume '{'
                let var_name: String = chars.by_ref().take_while(|c| *c != '}').collect();
                match std::env::var(&var_name) {
                    // CONFORMANCE_EXCEPTION: domain env-var expansion for MCP config interpolation
                    Ok(val) => output.push_str(&val),
                    Err(_) => {
                        warnings.push(format!("unknown env var: ${{{var_name}}}"));
                        output.push_str(&format!("${{{var_name}}}"));
                    }
                }
            } else {
                // $VAR form (alphanumeric + underscore)
                let mut var_name = String::new();
                while let Some(&c) = chars.peek() {
                    if c.is_alphanumeric() || c == '_' {
                        var_name.push(c);
                        chars.next();
                    } else {
                        break;
                    }
                }
                if var_name.is_empty() {
                    output.push('$');
                } else {
                    match std::env::var(&var_name) {
                        // CONFORMANCE_EXCEPTION: domain env-var expansion for MCP config interpolation
                        Ok(val) => output.push_str(&val),
                        Err(_) => {
                            warnings.push(format!("unknown env var: ${var_name}"));
                            output.push_str(&format!("${var_name}"));
                        }
                    }
                }
            }
        } else {
            output.push(ch);
        }
    }

    (output, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_expand_env_vars_dollar_brace() {
        unsafe { std::env::set_var("TEST_FOO", "hello") };
        let (s, w) = expand_env_vars("${TEST_FOO}");
        assert_eq!(s, "hello");
        assert!(w.is_empty());
    }

    #[test]
    fn test_expand_env_vars_dollar_plain() {
        unsafe { std::env::set_var("TEST_BAR", "world") };
        let (s, w) = expand_env_vars("$TEST_BAR");
        assert_eq!(s, "world");
        assert!(w.is_empty());
    }

    #[test]
    fn test_expand_env_vars_unknown_preserved() {
        let (s, w) = expand_env_vars("$UNKNOWN_VAR");
        assert_eq!(s, "$UNKNOWN_VAR");
        assert_eq!(w.len(), 1);
    }

    #[test]
    fn test_expand_env_vars_mixed() {
        unsafe { std::env::set_var("TEST_MIX", "val") };
        let (s, w) = expand_env_vars("pre-${TEST_MIX}-post");
        assert_eq!(s, "pre-val-post");
        assert!(w.is_empty());
    }

    #[test]
    fn test_mcp_transport_default_is_stdio() {
        assert_eq!(McpTransport::default(), McpTransport::Stdio);
    }
}
