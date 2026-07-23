use serde::{Deserialize, Serialize};
use std::fmt;

/// Unique identifier for a capability across all protocols.
///
/// # Namespace
///
/// Format: `{protocol}::{server}::{tool}` when server is non-empty,
/// else `{protocol}::{tool}` (for providers with no server, e.g. built-in).
///
/// The `::` (double-colon) separator is chosen because:
/// 1. `:` is used heavily in URIs, ports, and shell prompts — too noisy in logs
/// 2. `__` is reserved for the LLM-wire convention per ADR-06-08 + Story 9.2
/// 3. `::` matches Rust path syntax, visually distinguishing registry ids from wire names
///
/// # Relationship to LLM-wire `mcp__<server>__<tool>` shape (Story 9.2)
///
/// The `mcp__` double-underscore shape is the LLM/wire contract (kept verbatim).
/// The `mcp::` double-colon shape is the domain/registry contract (this struct).
/// Bridge methods `from_mcp_wire_name` / `to_mcp_wire_name` convert between them.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
pub struct CapabilityId {
    /// Protocol identifier: `"mcp"`, `"builtin"`, `"skill"`, `"a2a"`, `"subagent"`.
    pub protocol: String,
    /// Provider instance identifier. For MCP: the server-id.
    /// For built-ins/skills: empty string `""` (single global provider per type).
    pub server: String,
    /// Bare capability name. For MCP: tool name from `rmcp::Tool.name`.
    /// For built-ins: `"Bash"` / `"Read"` / etc. For skills: the skill name.
    pub tool: String,
}

impl CapabilityId {
    /// Format: `{protocol}::{server}::{tool}` when server is non-empty,
    /// else `{protocol}::{tool}`.
    pub fn as_string(&self) -> String {
        if self.server.is_empty() {
            format!("{}::{}", self.protocol, self.tool)
        } else {
            format!("{}::{}::{}", self.protocol, self.server, self.tool)
        }
    }

    /// Parse a `{protocol}::{server}::{tool}` or `{protocol}::{tool}` string.
    /// Returns `None` for malformed input (no `::`, empty parts, etc.).
    pub fn parse(s: &str) -> Option<Self> {
        let parts: Vec<&str> = s.split("::").collect();
        match parts.len() {
            2 => {
                let protocol = parts[0];
                let tool = parts[1];
                if protocol.is_empty() || tool.is_empty() {
                    return None;
                }
                Some(Self {
                    protocol: protocol.to_string(),
                    server: String::new(),
                    tool: tool.to_string(),
                })
            }
            3 => {
                let protocol = parts[0];
                let server = parts[1];
                let tool = parts[2];
                if protocol.is_empty() || server.is_empty() || tool.is_empty() {
                    return None;
                }
                Some(Self {
                    protocol: protocol.to_string(),
                    server: server.to_string(),
                    tool: tool.to_string(),
                })
            }
            _ => None,
        }
    }

    /// Bridge from the LLM-wire `mcp__<server>__<tool>` shape (Story 9.2).
    pub fn from_mcp_wire_name(name: &str) -> Option<Self> {
        let without_prefix = name.strip_prefix("mcp__")?;
        let (server, tool) = without_prefix.split_once("__")?;
        if server.is_empty() || tool.is_empty() {
            return None;
        }
        Some(Self {
            protocol: "mcp".to_string(),
            server: server.to_string(),
            tool: tool.to_string(),
        })
    }

    /// Bridge to the LLM-wire `mcp__<server>__<tool>` shape (Story 9.2).
    /// Returns `None` for non-MCP protocols or empty server.
    pub fn to_mcp_wire_name(&self) -> Option<String> {
        if self.protocol == "mcp" && !self.server.is_empty() {
            Some(format!("mcp__{}__{}", self.server, self.tool))
        } else {
            None
        }
    }

    /// Bridge from the LLM-wire `a2a__<peer>__<skill>` shape (Story 17.4b).
    /// The `__` separator is reserved (`A2aPeerSpec::validate_id` rejects peer
    /// ids containing `__`) so the peer/skill split is unambiguous.
    pub fn from_a2a_wire_name(name: &str) -> Option<Self> {
        let without_prefix = name.strip_prefix("a2a__")?;
        let (server, tool) = without_prefix.split_once("__")?;
        if server.is_empty() || tool.is_empty() {
            return None;
        }
        Some(Self {
            protocol: "a2a".to_string(),
            server: server.to_string(),
            tool: tool.to_string(),
        })
    }

    /// Bridge to the LLM-wire `a2a__<peer>__<skill>` shape (Story 17.4b, R-D).
    /// This is a SECURITY BOUNDARY: the raw peer-chosen skill *name* must never
    /// reach the LLM tool surface (a hostile peer naming its skill `Read` would
    /// otherwise be classified `ToolRisk::Safe`). The wire name is built from the
    /// namespaced `a2a__` prefix + peer id + skill id, so `risk_for_tool`'s
    /// `a2a__` arm always fires. Returns `None` for non-A2A protocols.
    pub fn to_a2a_wire_name(&self) -> Option<String> {
        if self.protocol == "a2a" && !self.server.is_empty() {
            Some(format!("a2a__{}__{}", self.server, self.tool))
        } else {
            None
        }
    }
}

impl fmt::Display for CapabilityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn test_round_trip_mcp_wire() {
        let id = CapabilityId::from_mcp_wire_name("mcp__postgres__query").unwrap();
        assert_eq!(id.protocol, "mcp");
        assert_eq!(id.server, "postgres");
        assert_eq!(id.tool, "query");
        assert_eq!(id.to_mcp_wire_name().unwrap(), "mcp__postgres__query");
    }

    #[test]
    fn test_round_trip_a2a_wire() {
        let id = CapabilityId::from_a2a_wire_name("a2a__planets__claim_planet").unwrap();
        assert_eq!(id.protocol, "a2a");
        assert_eq!(id.server, "planets");
        assert_eq!(id.tool, "claim_planet");
        assert_eq!(id.to_a2a_wire_name().unwrap(), "a2a__planets__claim_planet");
        // Non-A2A protocols never produce an a2a wire name.
        assert!(id.to_mcp_wire_name().is_none());
        let mcp = CapabilityId::from_mcp_wire_name("mcp__pg__query").unwrap();
        assert!(mcp.to_a2a_wire_name().is_none());
    }

    #[test]
    fn test_no_collision_across_protocols() {
        let mut map = BTreeMap::new();
        map.insert(
            CapabilityId {
                protocol: "mcp".into(),
                server: "postgres".into(),
                tool: "query".into(),
            },
            (),
        );
        map.insert(
            CapabilityId {
                protocol: "builtin".into(),
                server: String::new(),
                tool: "query".into(),
            },
            (),
        );
        map.insert(
            CapabilityId {
                protocol: "skill".into(),
                server: String::new(),
                tool: "query".into(),
            },
            (),
        );
        assert_eq!(map.len(), 3);
    }

    #[test]
    fn test_parse_rejects_malformed() {
        assert_eq!(CapabilityId::parse(""), None);
        assert_eq!(CapabilityId::parse("foo"), None); // no `::`
        assert_eq!(CapabilityId::parse("foo::"), None); // empty tool
        assert_eq!(CapabilityId::parse("::bar"), None); // empty protocol
        assert_eq!(CapabilityId::parse("foo::bar::baz::quux"), None); // too many parts
    }

    #[test]
    fn test_display_matches_as_string() {
        let id = CapabilityId {
            protocol: "mcp".into(),
            server: "postgres".into(),
            tool: "query".into(),
        };
        assert_eq!(format!("{}", id), "mcp::postgres::query");
    }

    #[test]
    fn test_serde_round_trip() {
        let id = CapabilityId {
            protocol: "mcp".into(),
            server: "postgres".into(),
            tool: "query".into(),
        };
        let json = serde_json::to_string(&id).unwrap();
        let parsed: CapabilityId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn test_parse_two_part() {
        let id = CapabilityId::parse("builtin::bash").unwrap();
        assert_eq!(id.protocol, "builtin");
        assert_eq!(id.server, "");
        assert_eq!(id.tool, "bash");
        assert_eq!(id.as_string(), "builtin::bash");
    }

    #[test]
    fn test_parse_three_part() {
        let id = CapabilityId::parse("mcp::postgres::query").unwrap();
        assert_eq!(id.protocol, "mcp");
        assert_eq!(id.server, "postgres");
        assert_eq!(id.tool, "query");
        assert_eq!(id.as_string(), "mcp::postgres::query");
    }
}
