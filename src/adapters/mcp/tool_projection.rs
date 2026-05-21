//! Pure projection helpers for MCP tool naming, invocation routing, and
//! result mapping (Story 9.2). Zero `.await`, zero I/O — synchronously
//! transform rmcp types into rustain domain types.

use crate::domain::models::{McpConnectionState, ToolDefinition, ToolResult};
use rmcp::model::{CallToolResult, Content, RawContent, Tool};

/// Project a single rmcp `Tool` into a rustain `ToolDefinition` with
/// canonical `mcp__<server>__<tool>` naming per ADR-06-08.
///
/// - `name` = `mcp__{server_id}__{tool.name}` — verbatim concat, no escaping.
/// - `description` = `tool.description.clone().unwrap_or_default()`.
/// - `input_schema` = `tool.input_schema` verbatim (server is the source of truth).
/// - `parallel_safe` = `tool.annotations.read_only_hint.unwrap_or(false)`.
pub fn project_tool(server_id: &str, tool: &Tool) -> ToolDefinition {
    // P-26: Sanitize server_id and tool.name — reject double-underscore
    // to prevent ambiguous canonical names like mcp__bad__server__tool
    assert!(
        !server_id.contains("__"),
        "MCP server_id must not contain double-underscore: {:?}",
        server_id
    );
    assert!(
        !tool.name.contains("__"),
        "MCP tool name must not contain double-underscore: {:?}",
        tool.name
    );

    let parallel_safe = tool
        .annotations
        .as_ref()
        .and_then(|a| a.read_only_hint)
        .unwrap_or(false);

    ToolDefinition {
        name: format!("mcp__{}__{}", server_id, tool.name),
        description: tool
            .description
            .clone()
            .map(|s| s.into_owned())
            .unwrap_or_default(),
        input_schema: serde_json::Value::Object((*tool.input_schema).clone()),
        parallel_safe,
    }
}

/// Parse a canonical `mcp__<server>__<tool>` name into `(server_id, tool_name)`.
///
/// Returns `None` if the name doesn't match the expected pattern. The parser
/// uses `__` (double-underscore) as the separator; a single `_` in server_id
/// or tool_name does not collide.
pub fn parse_mcp_tool_name(name: &str) -> Option<(&str, &str)> {
    let rest = name.strip_prefix("mcp__")?;
    let (server, tool) = rest.split_once("__")?;
    if server.is_empty() || tool.is_empty() {
        return None;
    }
    Some((server, tool))
}

/// Project an rmcp `CallToolResult` into a rustain `ToolResult`.
///
/// - Text content blocks are concatenated with `"\n"`.
/// - Non-text blocks (image, resource, audio) become bracket placeholders.
/// - `is_error` comes from `result.is_error.unwrap_or(false)`.
pub fn project_rmcp_result(result: CallToolResult, tool_use_id: String) -> ToolResult {
    let mut text_parts: Vec<String> = Vec::new();

    for content in &result.content {
        match &content.raw {
            RawContent::Text(ct) => {
                text_parts.push(ct.text.clone());
            }
            RawContent::Image(ci) => {
                text_parts.push(format!("[image: {}]", ci.mime_type));
            }
            RawContent::Resource(cr) => {
                let label = match &cr.resource {
                    rmcp::model::ResourceContents::TextResourceContents { uri, .. } => {
                        format!("[resource: {}]", uri)
                    }
                    rmcp::model::ResourceContents::BlobResourceContents { uri, .. } => {
                        format!("[resource: {}]", uri)
                    }
                };
                text_parts.push(label);
            }
            RawContent::Audio(ca) => {
                // MCP spec doesn't define duration on Audio, so use a placeholder
                text_parts.push("[audio]".to_string());
                let _ = ca; // suppress unused warning
            }
            RawContent::ResourceLink(rl) => {
                text_parts.push(format!("[resource: {}]", rl.uri));
            }
        }
    }

    let content = text_parts.join("\n");
    let is_error = result.is_error.unwrap_or(false);

    ToolResult {
        tool_use_id,
        content,
        is_error,
    }
}

/// Collect MCP tool autocomplete suggestions from a composite adapter.
///
/// Iterates connected/degraded clients, projects each tool to `McpToolInfo`,
/// and optionally filters by a case-insensitive substring match on tool name
/// or description.
pub fn collect_mcp_autocomplete(
    clients: &[std::sync::Arc<crate::adapters::mcp::client::McpClientAdapter>],
    filter: Option<&str>,
) -> Vec<crate::domain::models::autocomplete::McpToolInfo> {
    let mut results = Vec::new();
    // P-24: Hoist filter lowercase outside loop
    let filter_lower = filter.map(|f| f.to_lowercase());

    for client in clients {
        let state = client.state();
        if !matches!(
            state,
            McpConnectionState::Connected { .. } | McpConnectionState::Degraded { .. }
        ) {
            continue;
        }

        let server_id = client.server_id().to_string();
        if let Some(tools) = client.cached_tools() {
            for tool in &tools {
                // Optionally filter
                if let Some(ref f_lower) = filter_lower {
                    if !tool.name.to_lowercase().contains(f_lower)
                        && !tool
                            .description
                            .as_deref()
                            .unwrap_or("")
                            .to_lowercase()
                            .contains(f_lower)
                    {
                        continue;
                    }
                }

                results.push(crate::domain::models::autocomplete::McpToolInfo {
                    server: server_id.clone(),
                    name: tool.name.to_string(),
                    description: tool
                        .description
                        .clone()
                        .map(|s| s.into_owned())
                        .unwrap_or_default(),
                });
            }
        }
    }

    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::{Tool as RmcpTool, ToolAnnotations};
    use std::sync::Arc;

    fn make_tool(name: &str, desc: Option<&str>, read_only: Option<bool>) -> RmcpTool {
        let annotations = read_only.map(|ro| {
            let mut ann = ToolAnnotations::default();
            ann.read_only_hint = Some(ro);
            ann
        });
        let mut tool: RmcpTool = Default::default();
        tool.name = std::borrow::Cow::Owned(name.to_string());
        tool.description = desc.map(|s| std::borrow::Cow::Owned(s.to_string()));
        tool.input_schema = Arc::new(serde_json::Map::new());
        tool.annotations = annotations;
        tool
    }

    #[test]
    fn test_project_tool_canonical_naming() {
        let tool = make_tool("query", Some("Run a query"), None);
        let def = project_tool("postgres", &tool);
        assert_eq!(def.name, "mcp__postgres__query");
        assert_eq!(def.description, "Run a query");
        assert!(def.input_schema.is_object());
    }

    #[test]
    fn test_project_tool_missing_description() {
        let tool = make_tool("list", None, None);
        let def = project_tool("git", &tool);
        assert_eq!(def.description, "");
    }

    #[test]
    fn test_project_tool_parallel_safe_from_read_only_hint() {
        let tool = make_tool("read", None, Some(true));
        let def = project_tool("fs", &tool);
        assert!(def.parallel_safe);

        let tool2 = make_tool("write", None, Some(false));
        let def2 = project_tool("fs", &tool2);
        assert!(!def2.parallel_safe);

        let tool3 = make_tool("op", None, None);
        let def3 = project_tool("fs", &tool3);
        assert!(!def3.parallel_safe);
    }

    #[test]
    fn test_parse_mcp_tool_name_roundtrip() {
        assert_eq!(
            parse_mcp_tool_name("mcp__postgres__query"),
            Some(("postgres", "query"))
        );
        assert_eq!(
            parse_mcp_tool_name("mcp__git__list_branches"),
            Some(("git", "list_branches"))
        );
        assert_eq!(
            parse_mcp_tool_name("mcp__server.with.dots__tool"),
            Some(("server.with.dots", "tool"))
        );
    }

    #[test]
    fn test_parse_mcp_tool_name_rejects_malformed() {
        assert_eq!(parse_mcp_tool_name("Bash"), None);
        assert_eq!(parse_mcp_tool_name("mcp_foo"), None);
        assert_eq!(parse_mcp_tool_name("mcp__"), None);
        assert_eq!(parse_mcp_tool_name("mcp__foo"), None);
        assert_eq!(parse_mcp_tool_name(""), None);
    }

    #[test]
    fn test_project_rmcp_result_text_concatenation() {
        let result = CallToolResult::success(vec![Content::text("hello"), Content::text("world")]);
        let tool_result = project_rmcp_result(result, "id-1".into());
        assert_eq!(tool_result.tool_use_id, "id-1");
        assert_eq!(tool_result.content, "hello\nworld");
        assert!(!tool_result.is_error);
    }

    #[test]
    fn test_project_rmcp_result_image_placeholder() {
        let mut result = CallToolResult::success(vec![
            Content::text("result: "),
            Content::image("base64data", "image/png"),
        ]);
        result.is_error = None;
        let tool_result = project_rmcp_result(result, "id-2".into());
        assert_eq!(tool_result.content, "result: \n[image: image/png]");
    }

    #[test]
    fn test_project_rmcp_result_error_flag() {
        let result = CallToolResult::error(vec![Content::text("not found")]);
        let tool_result = project_rmcp_result(result, "id-3".into());
        assert!(tool_result.is_error);
    }

    #[test]
    fn test_parse_mcp_name_with_underscore_in_server_id() {
        assert_eq!(
            parse_mcp_tool_name("mcp__my_server__get_data"),
            Some(("my_server", "get_data"))
        );
    }
}
