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
    pub url: Option<crate::domain::models::redacted_url::RedactedUrl>,
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

    /// Whether resolution refused a stdio field (Q-6 AC1): the spec then carries
    /// [`REFUSED_REFERENCE`] in place of the value, never the reference itself.
    #[must_use]
    pub fn references_refused_app_credential(&self) -> bool {
        self.command.as_deref() == Some(REFUSED_REFERENCE)
            || self.args.iter().any(|arg| arg == REFUSED_REFERENCE)
            || self.env.values().any(|value| value == REFUSED_REFERENCE)
    }

    /// Story 9.9 (AC2) — transport ↔ field consistency. The **first** such
    /// validation in the tree: `validate_id` checks the id alone, and
    /// `RedactedUrl::new` is infallible and validates nothing, so an `http`
    /// entry's URL must be parsed here or nowhere.
    ///
    /// ⚑ ONE function, TWO consumers. The config parsers call it to warn **per
    /// entry** at load time — the offending entry stays in the list so its
    /// healthy siblings survive (ruling A1: a required `command` used to fail
    /// the WHOLE `.claude/mcp.json` and delete every server in it) — and
    /// `McpClientAdapter::connect` calls it as the fail-closed gate that turns
    /// the same fault into `ConnectionFailed { last_error }` in the adapter
    /// status panel. A second, drifting copy of this rule is the defect this
    /// shape exists to avoid.
    pub fn validate_transport_fields(&self) -> Result<(), String> {
        match self.transport {
            McpTransport::Stdio if self.references_refused_app_credential() => Err(format!(
                "MCP server '{}': a stdio `command`, `args` or `env` value references an \
                 app-owned credential; the reference was refused before any lookup and the server \
                 is not launched. Use a dedicated server-scoped variable instead",
                self.id
            )),
            McpTransport::Stdio => match self.command.as_deref() {
                None => Err(format!(
                    "MCP server '{}': transport = \"stdio\" requires a `command`, but none is configured",
                    self.id
                )),
                Some(command) if command.trim().is_empty() => Err(format!(
                    "MCP server '{}': transport = \"stdio\" has an empty `command`",
                    self.id
                )),
                Some(_) => Ok(()),
            },
            McpTransport::Http => {
                let Some(url) = self.url.as_ref() else {
                    return Err(format!(
                        "MCP server '{}': transport = \"http\" requires a `url`, but none is configured",
                        self.id
                    ));
                };
                // `parse_url` keeps the raw value inside the newtype — a
                // credential-bearing URL must still parse, and no string below
                // ever interpolates anything but the redacting `Display` form.
                let parsed = url.parse_url().map_err(|error| {
                    format!(
                        "MCP server '{}': `url` {url} is not a valid URL: {error}",
                        self.id
                    )
                })?;
                match parsed.scheme() {
                    "http" | "https" => Ok(()),
                    other => Err(format!(
                        "MCP server '{}': `url` {url} uses scheme {other:?}; Streamable HTTP requires http or https",
                        self.id
                    )),
                }
            }
            // SSE is refused at connect (ADR-06-08) — there is no field
            // contract worth checking for a transport that never dials.
            McpTransport::Sse => Ok(()),
        }
    }
}

pub fn expand_env_vars(input: &str) -> (String, Vec<String>) {
    let (output, warnings, _) = expand_impl(input, false);
    (output, warnings)
}

/// What a stdio `command`, `args` or `env` value became after resolution.
///
/// A value that references an app-owned credential variable (Story Q-6 AC1,
/// owner decision `mcp_app_credentials=deny`) is **refused before any lookup**:
/// `value` is then [`REFUSED_REFERENCE`] — the whole field, so neither the
/// protected name nor surrounding text survives in the spec — and `refused` is
/// set.
pub struct StdioExpansion {
    pub value: String,
    pub warnings: Vec<String>,
    pub refused: bool,
}

/// Placeholder stored instead of a refused stdio value. It names nothing, and
/// `McpServerSpec::validate_transport_fields` refuses a spec that carries it.
pub const REFUSED_REFERENCE: &str = "[refused: app-owned credential reference]";

/// Expand a stdio field with the app-credential denial applied. Every other
/// reference expands exactly as [`expand_env_vars`] does. HTTP `url` expansion
/// keeps using [`expand_env_vars`]: the denial is a stdio-field rule.
pub fn expand_stdio_env_vars(input: &str) -> StdioExpansion {
    let (value, warnings, refused) = expand_impl(input, true);
    StdioExpansion {
        value,
        warnings,
        refused,
    }
}

fn expand_impl(input: &str, deny_app_credentials: bool) -> (String, Vec<String>, bool) {
    let mut output = String::with_capacity(input.len());
    let mut warnings = Vec::new();
    let mut refused = false;
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch != '$' {
            output.push(ch);
            continue;
        }
        let (var_name, braced) = if let Some(&'{') = chars.peek() {
            chars.next(); // consume '{'
            (
                chars.by_ref().take_while(|c| *c != '}').collect::<String>(),
                true,
            )
        } else {
            let mut name = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_alphanumeric() || c == '_' {
                    name.push(c);
                    chars.next();
                } else {
                    break;
                }
            }
            (name, false)
        };
        if var_name.is_empty() && !braced {
            output.push('$');
            continue;
        }
        if deny_app_credentials
            && crate::domain::services::app_credentials::is_app_credential_name(&var_name)
        {
            // Refused by name, before any lookup: no `std::env::var` call, no
            // warning that would name the variable.
            refused = true;
            continue;
        }
        let literal = if braced {
            format!("${{{var_name}}}")
        } else {
            format!("${var_name}")
        };
        match std::env::var(&var_name) {
            // CONFORMANCE_EXCEPTION: domain env-var expansion for MCP config interpolation
            Ok(val) => output.push_str(&val),
            Err(_) => {
                warnings.push(format!("unknown env var: {literal}"));
                output.push_str(&literal);
            }
        }
    }

    if refused {
        return (REFUSED_REFERENCE.to_owned(), Vec::new(), true);
    }
    (output, warnings, false)
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
    fn stdio_expansion_refuses_an_app_credential_reference_before_any_lookup() {
        // A registered custom name whose value IS set: a lookup would substitute it.
        let name = "Q6_SPEC_CUSTOM_PROVIDER_KEY";
        crate::domain::services::app_credentials::register_configured_credential_names([name]);
        unsafe { std::env::set_var(name, "q6-canary") };
        for input in [
            format!("${name}"),
            format!("--k=${{{name}}}"),
            "pre-${OPENAI_API_KEY}-post".to_string(),
        ] {
            let expansion = expand_stdio_env_vars(&input);
            assert!(expansion.refused, "{input}");
            assert_eq!(expansion.value, REFUSED_REFERENCE);
            assert!(
                expansion.warnings.is_empty(),
                "no warning names the variable"
            );
        }
        // The shared helper (`url`, HTTP) is untouched: it still expands.
        assert_eq!(expand_env_vars(&format!("${name}")).0, "q6-canary");
        unsafe { std::env::remove_var(name) };
    }

    #[test]
    fn stdio_expansion_keeps_ordinary_references_and_empty_braces() {
        unsafe { std::env::set_var("Q6_SPEC_ORDINARY", "fine") };
        let expansion = expand_stdio_env_vars("a-${Q6_SPEC_ORDINARY}-$Q6_SPEC_ORDINARY");
        assert!(!expansion.refused);
        assert_eq!(expansion.value, "a-fine-fine");
        assert_eq!(expand_stdio_env_vars("cost $5 ${}").value, "cost $5 ${}");
        unsafe { std::env::remove_var("Q6_SPEC_ORDINARY") };
    }

    #[test]
    fn a_refused_stdio_field_fails_the_shared_transport_gate_without_naming_it() {
        let mut spec = McpServerSpec {
            id: "refused".into(),
            transport: McpTransport::Stdio,
            command: Some("srv".into()),
            args: vec![REFUSED_REFERENCE.into()],
            env: BTreeMap::new(),
            url: None,
            persistent: false,
            source: McpServerSource::Workspace,
        };
        let reason = spec.validate_transport_fields().unwrap_err();
        assert!(reason.contains("refused") && reason.contains("'refused'"));
        assert!(!reason.contains("KEY") && !reason.contains("TOKEN"));
        // An HTTP declaration is not a stdio field.
        spec.transport = McpTransport::Http;
        spec.url = Some(crate::domain::models::redacted_url::RedactedUrl::new(
            "https://example.com/mcp".into(),
        ));
        assert!(spec.validate_transport_fields().is_ok());
    }

    #[test]
    fn test_mcp_transport_default_is_stdio() {
        assert_eq!(McpTransport::default(), McpTransport::Stdio);
    }
}
