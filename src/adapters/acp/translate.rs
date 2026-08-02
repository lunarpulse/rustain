use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::adapters::apply_patch::{PatchHunk, parse_apply_patch, resolve_workspace_path};
use agent_client_protocol as acp;

use crate::domain::models::{
    ApprovalOutcome, ChatMessage, McpServerSource, McpServerSpec, McpTransport, MessageRole,
    StopReason, StreamChunk,
};
use crate::domain::services::approval_runtime::ApprovalRuntimeEvent;

pub const PERMISSION_ALLOW_ONCE: &str = "allow_once";
pub const PERMISSION_ALLOW_ALWAYS_TOOL: &str = "allow_always_tool";
pub const PERMISSION_REJECT_ONCE: &str = "reject_once";
pub const SKILL_TRUST_ALLOW: &str = "skill_trust_allow";
pub const SKILL_TRUST_REJECT: &str = "skill_trust_reject";
pub fn mcp_servers_from_acp(servers: Vec<acp::McpServer>) -> Vec<McpServerSpec> {
    let mut out = Vec::with_capacity(servers.len());
    // Track forwarded ids so two client names that collapse to the same id
    // (e.g. "a b" and "a_b", or a literal duplicate) don't both become specs —
    // that would mint two MCP clients with identical ids and make tool routing
    // nondeterministic (`CompositeToolsetAdapter` finds the first match).
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for server in servers {
        let stdio = match server {
            acp::McpServer::Stdio(stdio) => stdio,
            acp::McpServer::Http(http) => {
                tracing::warn!(
                    server = %http.name,
                    "Ignoring ACP MCP HTTP server because rustain supports stdio MCP only"
                );
                continue;
            }
            acp::McpServer::Sse(sse) => {
                tracing::warn!(
                    server = %sse.name,
                    "Ignoring ACP MCP SSE server because rustain supports stdio MCP only"
                );
                continue;
            }
            _ => {
                tracing::warn!("Ignoring unknown ACP MCP server variant");
                continue;
            }
        };
        // Normalize the client-supplied name into a safe MCP server id:
        //   1. whitespace runs → single `_` separators;
        //   2. collapse any remaining `__` to `_` so the id NEVER contains a
        //      double-underscore. `project_tool` enforces this with a
        //      release-build `assert!(!server_id.contains("__"))`
        //      (`tool_projection.rs`), and `__` is the `mcp__<server>__<tool>`
        //      wire-name separator — an id with `__` would either panic on the
        //      first prompt turn or produce ambiguous tool names. The server
        //      name is client-controlled (AC3's threat model), so this must be
        //      a sanitize, not an assert.
        let mut id = stdio.name.split_whitespace().collect::<Vec<_>>().join("_");
        while id.contains("__") {
            id = id.replace("__", "_");
        }
        if id.is_empty() {
            tracing::warn!(
                name = %stdio.name,
                "Ignoring ACP MCP stdio server with empty/whitespace-only name \
                 (it would yield an empty id, producing uncallable tools)"
            );
            continue;
        }
        if !seen.insert(id.clone()) {
            tracing::warn!(
                id = %id,
                "Ignoring duplicate ACP MCP stdio server (its name collapsed to \
                 an id already forwarded; keeping the first)"
            );
            continue;
        }
        // AC3 SECURITY: forwarded `env` values are passed through LITERALLY and
        // are NEVER run through `expand_env_vars` against rustain's process
        // environment — a client must not be able to exfiltrate a rustain
        // secret (e.g. `${ANTHROPIC_API_KEY}`) into the spawned child.
        let env = stdio
            .env
            .into_iter()
            .map(|var| (var.name, var.value))
            .collect::<BTreeMap<_, _>>();
        out.push(McpServerSpec {
            id,
            transport: McpTransport::Stdio,
            command: Some(stdio.command.display().to_string()),
            args: stdio.args,
            env,
            url: None,
            persistent: false,
            source: McpServerSource::Workspace,
        });
    }
    out
}

pub fn stop_reason_to_acp(reason: &StopReason) -> acp::StopReason {
    match reason {
        StopReason::EndTurn => acp::StopReason::EndTurn,
        StopReason::MaxTokens => acp::StopReason::MaxTokens,
        StopReason::Cancelled => acp::StopReason::Cancelled,
        _ => acp::StopReason::Refusal,
    }
}

/// Extract the file location a tool call touches, so the ACP client (Zed) can
/// implement its "follow" feature — navigating to the file the agent is
/// reading/writing in real time. Maps to `ToolCallLocation` on the emitted
/// `ToolCall`/`ToolCallUpdate` (see agentclientprotocol.com tool-calls#following-the-agent).
///
/// Only tools that KNOWN carry a file path contribute a location — guessing
/// risks pointing the editor at a wrong file. rustain's file tools (`read`/
/// `write`) key the path under `file_path`. MCP-forwarded and dynamic tools
/// have arbitrary inputs and advertise no extractable path, so they are
/// intentionally excluded (no follow target — the correct behavior). Relative
/// paths are resolved against the session `cwd` so the client always receives
/// an absolute target it can open.
fn resolve_tool_path(path_str: &str, cwd: &Path) -> PathBuf {
    let path = Path::new(path_str);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

fn tool_call_kind(tool_name: &str) -> acp::ToolKind {
    // Exact match on the canonical builtin names — the SAME set the risk
    // classifier and executor dispatch recognize. A case-variant like
    // `Apply_Patch` is NOT coerced to Edit: it stays Other here, Elevated in
    // `risk_for_builtin`, and NotFound in dispatch, consistently surfacing the
    // unrecognized name instead of rendering an Edit-kind call that cannot run.
    match tool_name {
        "Read" | "read" | "view" | "read_file" => acp::ToolKind::Read,
        "Edit" | "edit" | "apply_patch" => acp::ToolKind::Edit,
        _ => acp::ToolKind::Other,
    }
}

/// Build the ACP presentation `content` (hunk `Diff`s) and follow `locations`
/// for a tool call in a SINGLE pass, so an `apply_patch` body is parsed exactly
/// once per frame (it was previously parsed separately for content and for
/// locations). Shared by the live stream path and the `session/load` replay
/// path so reconnecting clients see the same kind/content/locations shape.
///
/// `apply_patch` paths are confined through `resolve_workspace_path` (the
/// executor's own gate): a hunk whose path escapes the workspace is omitted
/// from both content and locations, mirroring that the executor will reject it
/// — no misleading follow target or path leak. `edit`/read/write paths use
/// `resolve_tool_path` (matching what the executor actually touches).
fn tool_call_surface(
    name: &str,
    input: &serde_json::Value,
    cwd: &Path,
) -> (Vec<acp::ToolCallContent>, Vec<acp::ToolCallLocation>) {
    if name == "apply_patch" {
        let Some(patch) = input.get("patch").and_then(serde_json::Value::as_str) else {
            return (Vec::new(), Vec::new());
        };
        return match parse_apply_patch(patch) {
            Ok(parsed) => {
                let mut content = Vec::with_capacity(parsed.hunks.len());
                let mut locations = Vec::with_capacity(parsed.hunks.len());
                for hunk in parsed.hunks {
                    let (path, new_text, old_text_opt) = match hunk {
                        PatchHunk::AddFile { path, new_text } => (path, new_text, None),
                        PatchHunk::DeleteFile { path } => {
                            (path, String::new(), Some("[file deleted]".to_string()))
                        }
                        PatchHunk::UpdateFile {
                            path,
                            old_text,
                            new_text,
                        } => (path, new_text, Some(old_text)),
                    };
                    // Confine: omit hunks whose path escapes the workspace.
                    let Some(resolved) = resolve_workspace_path(cwd, &path).ok() else {
                        continue;
                    };
                    let mut diff = acp::Diff::new(resolved.clone(), new_text);
                    if let Some(old) = old_text_opt {
                        diff = diff.old_text(old);
                    }
                    content.push(acp::ToolCallContent::Diff(diff));
                    locations.push(acp::ToolCallLocation::new(resolved));
                }
                (content, locations)
            }
            Err(_) => (
                vec![acp::ToolCallContent::from(patch.to_string())],
                Vec::new(),
            ),
        };
    }
    if name == "edit" || name == "Edit" {
        let Some(path_str) = input
            .get("file_path")
            .or_else(|| input.get("path"))
            .and_then(serde_json::Value::as_str)
        else {
            return (Vec::new(), Vec::new());
        };
        let Some(old_string) = input.get("old_string").and_then(serde_json::Value::as_str) else {
            return (Vec::new(), Vec::new());
        };
        let Some(new_string) = input.get("new_string").and_then(serde_json::Value::as_str) else {
            return (Vec::new(), Vec::new());
        };
        let resolved = resolve_tool_path(path_str, cwd);
        let content = vec![acp::ToolCallContent::Diff(
            acp::Diff::new(resolved.clone(), new_string).old_text(old_string),
        )];
        let locations = vec![acp::ToolCallLocation::new(resolved)];
        return (content, locations);
    }
    // read/write/view/etc.: location only, no Diff content (ADR-14-10-02
    // read-follow; Write stays location-only by AC5).
    if let Some(path_str) = match name {
        "Read" | "read" | "Write" | "write" | "view" | "read_file" | "write_file" => input
            .get("file_path")
            .or_else(|| input.get("path"))
            .and_then(serde_json::Value::as_str),
        _ => None,
    } {
        let resolved = resolve_tool_path(path_str, cwd);
        return (Vec::new(), vec![acp::ToolCallLocation::new(resolved)]);
    }
    (Vec::new(), Vec::new())
}

pub fn stream_chunk_to_session_update(
    chunk: &StreamChunk,
    cwd: &Path,
) -> Option<acp::SessionUpdate> {
    match chunk {
        StreamChunk::Text { content, .. } => Some(acp::SessionUpdate::AgentMessageChunk(
            acp::ContentChunk::new(acp::ContentBlock::from(content.clone())),
        )),
        StreamChunk::Thinking { content, .. } => Some(acp::SessionUpdate::AgentThoughtChunk(
            acp::ContentChunk::new(acp::ContentBlock::from(content.clone())),
        )),
        StreamChunk::ToolUse { id, name, input } => {
            let (content, locations) = tool_call_surface(name, input, cwd);
            Some(acp::SessionUpdate::ToolCall(
                acp::ToolCall::new(id.clone(), name.clone())
                    .kind(tool_call_kind(name))
                    .status(acp::ToolCallStatus::Pending)
                    .content(content)
                    .locations(locations)
                    .raw_input(input.clone()),
            ))
        }
        StreamChunk::ToolResult {
            id,
            content,
            is_error,
        } => {
            let status = if *is_error {
                acp::ToolCallStatus::Failed
            } else {
                acp::ToolCallStatus::Completed
            };
            Some(acp::SessionUpdate::ToolCallUpdate(
                acp::ToolCallUpdate::new(
                    id.clone(),
                    acp::ToolCallUpdateFields::new()
                        .status(status)
                        .content(vec![acp::ToolCallContent::from(content.clone())]),
                ),
            ))
        }
        _ => None,
    }
}

/// Map a persisted conversation message to the `session/update` notifications
/// that replay it to a reconnecting client (AC2 `session/load` history replay).
///
/// Pure re-emission — no provider/model calls. Emits a `UserMessageChunk` /
/// `AgentMessageChunk` for the message text, plus one `ToolCall` (marked
/// `Completed`) per persisted tool call so the client reconstructs the prior
/// turn's tool invocations. Returns a `Vec` because an assistant message with
/// tool calls yields several updates. Meta/context items are intentionally
/// skipped (codex `replay_history` shape).
pub fn message_to_replay_updates(message: &ChatMessage, cwd: &Path) -> Vec<acp::SessionUpdate> {
    let mut updates = Vec::new();
    if !message.content.is_empty() {
        let chunk = acp::ContentChunk::new(acp::ContentBlock::from(message.content.clone()));
        updates.push(match message.role {
            MessageRole::User => acp::SessionUpdate::UserMessageChunk(chunk),
            _ => acp::SessionUpdate::AgentMessageChunk(chunk),
        });
    }

    for tc in &message.tool_calls {
        // P8: replay now carries kind + Diff content too, so a reconnecting
        // client renders prior edit/apply_patch calls the same as the live
        // stream (Edit-kind + hunk Diffs), not as generic Other/empty.
        let (content, locations) = tool_call_surface(&tc.name, &tc.input, cwd);
        updates.push(acp::SessionUpdate::ToolCall(
            acp::ToolCall::new(tc.id.clone(), tc.name.clone())
                .kind(tool_call_kind(&tc.name))
                .status(acp::ToolCallStatus::Completed)
                .content(content)
                .locations(locations)
                .raw_input(tc.input.clone()),
        ));
    }
    updates
}

pub fn approval_request_to_acp(
    session_id: acp::SessionId,
    event: &ApprovalRuntimeEvent,
) -> Option<acp::RequestPermissionRequest> {
    let ApprovalRuntimeEvent::Requested {
        id,
        tool,
        input_preview,
        ..
    } = event
    else {
        return None;
    };

    let tool_update = acp::ToolCallUpdate::new(
        id.0.clone(),
        acp::ToolCallUpdateFields::new()
            .title(format!("Approve {tool}"))
            .raw_input(serde_json::json!({ "preview": input_preview })),
    );

    Some(acp::RequestPermissionRequest::new(
        session_id,
        tool_update,
        vec![
            acp::PermissionOption::new(
                PERMISSION_ALLOW_ONCE,
                "Allow once",
                acp::PermissionOptionKind::AllowOnce,
            ),
            acp::PermissionOption::new(
                PERMISSION_ALLOW_ALWAYS_TOOL,
                "Always allow this tool",
                acp::PermissionOptionKind::AllowAlways,
            ),
            acp::PermissionOption::new(
                PERMISSION_REJECT_ONCE,
                "Reject",
                acp::PermissionOptionKind::RejectOnce,
            ),
        ],
    ))
}

pub fn skill_trust_request_to_acp(
    session_id: acp::SessionId,
    skill_name: &str,
    skill_source: &str,
    skill_file: &std::path::Path,
) -> acp::RequestPermissionRequest {
    let update = acp::ToolCallUpdate::new(
        format!("skill-trust:{skill_name}"),
        acp::ToolCallUpdateFields::new()
            .title(format!("Trust skill {skill_name}?"))
            .raw_input(serde_json::json!({
                "skill": skill_name,
                "source": skill_source,
                "path": skill_file.display().to_string(),
            })),
    );
    acp::RequestPermissionRequest::new(
        session_id,
        update,
        vec![
            acp::PermissionOption::new(
                SKILL_TRUST_ALLOW,
                "Allow skill",
                acp::PermissionOptionKind::AllowOnce,
            ),
            acp::PermissionOption::new(
                SKILL_TRUST_REJECT,
                "Reject skill",
                acp::PermissionOptionKind::RejectOnce,
            ),
        ],
    )
}

pub fn skill_trust_response_allows(response: acp::RequestPermissionResponse) -> bool {
    match response.outcome {
        acp::RequestPermissionOutcome::Selected(selected) => {
            selected.option_id.0.as_ref() == SKILL_TRUST_ALLOW
        }
        _ => false,
    }
}

pub fn permission_response_to_outcome(
    response: acp::RequestPermissionResponse,
    tool_name: &str,
) -> ApprovalOutcome {
    match response.outcome {
        acp::RequestPermissionOutcome::Cancelled => ApprovalOutcome::Cancel,
        acp::RequestPermissionOutcome::Selected(selected) => {
            let option = selected.option_id.0.as_ref();
            match option {
                PERMISSION_ALLOW_ONCE => ApprovalOutcome::Once,
                PERMISSION_ALLOW_ALWAYS_TOOL => ApprovalOutcome::AlwaysTool {
                    tool_name: tool_name.to_string(),
                },
                _ => ApprovalOutcome::Reject { feedback: None },
            }
        }
        _ => ApprovalOutcome::Reject { feedback: None },
    }
}

#[cfg(test)]
mod tests {
    use super::mcp_servers_from_acp;
    use crate::domain::models::{McpServerSource, McpTransport};
    use agent_client_protocol as acp;

    /// Build a `McpServer::Stdio` via the public crate builders (the structs are
    /// `#[non_exhaustive]`, so struct literals are forbidden outside the crate).
    fn stdio_server(name: &str, command: &str) -> acp::McpServer {
        acp::McpServer::Stdio(acp::McpServerStdio::new(name, command))
    }

    // ── stdio → McpServerSpec field-by-field mapping ─────────────────────

    /// A single `McpServer::Stdio` maps to one `McpServerSpec` whose every field
    /// is the forwarded stdio shape: id = name, transport = Stdio, command =
    /// `command.display()`, args forwarded, env as a BTreeMap, url = None,
    /// persistent = false, source = Workspace.
    ///
    /// Non-vacuity: every field is asserted. A mutant that drops `command`,
    /// stamps `persistent = true`, sets `source = Profile`, or invents a URL
    /// reddens a distinct assertion. The distinctive name + sentinel env value
    /// make false matches implausible.
    #[test]
    fn test_mcp_servers_from_acp_maps_stdio_server_field_by_field() {
        let server = acp::McpServer::Stdio(
            acp::McpServerStdio::new("echo-server", "/usr/bin/echo")
                .args(vec!["--foo".to_string(), "bar".to_string()])
                .env(vec![
                    acp::EnvVariable::new("API_KEY", "literal-token"),
                    acp::EnvVariable::new("DEBUG", "1"),
                ]),
        );

        let specs = mcp_servers_from_acp(vec![server]);

        assert_eq!(
            specs.len(),
            1,
            "exactly one stdio server must yield one spec"
        );
        let spec = &specs[0];
        assert_eq!(spec.id, "echo-server");
        assert_eq!(spec.transport, McpTransport::Stdio);
        assert_eq!(spec.command.as_deref(), Some("/usr/bin/echo"));
        assert_eq!(spec.args, vec!["--foo".to_string(), "bar".to_string()]);
        assert_eq!(
            spec.env.get("API_KEY").map(String::as_str),
            Some("literal-token"),
            "env values map into the spec env by name"
        );
        assert_eq!(spec.env.get("DEBUG").map(String::as_str), Some("1"));
        assert!(
            spec.url.is_none(),
            "stdio forwarding must never invent a url"
        );
        assert!(
            !spec.persistent,
            "forwarded stdio servers are never persistent"
        );
        assert!(
            matches!(spec.source, McpServerSource::Workspace),
            "a client-forwarded server originates as Workspace, not a profile"
        );
    }

    // ── whitespace → `_` normalization in the id ─────────────────────────

    /// The id normalizes ALL whitespace runs (spaces, tabs, leading/trailing)
    /// to single `_` separators via `split_whitespace().join("_")`. This avoids
    /// the `mcp__<server>__<tool>` wire-name collision and the `__` id validator.
    #[test]
    fn test_mcp_servers_from_acp_normalizes_whitespace_in_id() {
        let cases: &[(&str, &str)] = &[
            // (input name, expected normalized id)
            ("single", "single"),
            ("alpha beta", "alpha_beta"),
            ("double  space", "double_space"), // runs collapse to one `_`
            ("alpha\tbeta gamma", "alpha_beta_gamma"), // tab + space mix
            ("  leading", "leading"),          // leading whitespace trimmed
            ("trailing  ", "trailing"),        // trailing whitespace trimmed
        ];

        for (name, expected) in cases {
            let specs = mcp_servers_from_acp(vec![stdio_server(name, "/cmd")]);
            assert_eq!(specs.len(), 1, "name {name:?} should yield one stdio spec");
            assert_eq!(
                specs[0].id, *expected,
                "whitespace in name {name:?} must normalize to {expected:?}"
            );
        }
    }

    // ── AC3 SECURITY: forwarded env is LITERAL, never expanded ───────────

    /// A forwarded `env` value like `"${PROBE}"` is preserved LITERALLY — the
    /// mapping never calls `expand_env_vars` against rustain's process
    /// environment, so a client cannot exfiltrate a rustain secret into the
    /// spawned child (the AC3 / Vex exfiltration sleeper).
    ///
    /// Non-vacuity: `PROBE` is SET to a sentinel in the process env. If a mutant
    /// threads the value through `expand_env_vars`, `${PROBE}` expands to the
    /// sentinel and the literal-token assertion reddens. (With `PROBE` unset,
    /// `expand_env_vars` leaves unknown vars literal — the bug would pass — so
    /// the var MUST be set for the test to have teeth.)
    ///
    /// Hermeticity: `PROBE_VAR` is unique to this test and restored on scope
    /// exit; the function under test never reads the process env, so the
    /// assertion is deterministic.
    #[test]
    fn test_mcp_servers_from_acp_preserves_env_literally_without_expansion() {
        const PROBE_VAR: &str = "RUSTAIN_TEST_ACP_MCP_EXFIL_PROBE_UNIT_14_10";
        const SENTINEL: &str = "EXFIL-SENTINEL-UNIT-14-10";
        const CHILD_KEY: &str = "DOWNSTREAM_TOKEN";

        // SAFETY: `PROBE_VAR` is unique to this test; no other test reads it,
        // and the function under test does not read the process env. Restored
        // on scope exit via the guard so the sentinel never leaks.
        unsafe { std::env::set_var(PROBE_VAR, SENTINEL) };
        let _guard = scopeguard::guard((), |_| {
            // SAFETY: same uniqueness rationale.
            unsafe { std::env::remove_var(PROBE_VAR) };
        });

        let literal_value = format!("${{{PROBE_VAR}}}");
        let server =
            acp::McpServer::Stdio(acp::McpServerStdio::new("env-probe", "/cmd").env(vec![
                acp::EnvVariable::new(CHILD_KEY, literal_value.clone()),
            ]));

        let specs = mcp_servers_from_acp(vec![server]);
        assert_eq!(specs.len(), 1);
        let forwarded = specs[0].env.get(CHILD_KEY).unwrap_or_else(|| {
            panic!(
                "child env key `{CHILD_KEY}` not forwarded: {:?}",
                specs[0].env
            );
        });
        assert_eq!(
            forwarded, &literal_value,
            "forwarded env must be the LITERAL token `{literal_value}`, not the \
             expanded process value `{SENTINEL}` — expanding client env against \
             rustain's process environment would exfiltrate rustain secrets into \
             the spawned child (AC3 exfiltration guard)"
        );
    }

    // ── Http / Sse are dropped; only stdio survives ──────────────────────

    /// `McpServer::Http` / `McpServer::Sse` are DROPPED (rustain's
    /// `McpClientAdapter` connects stdio only). A mixed list reaches the seam as
    /// ONLY the stdio specs, in input order; http-only and sse-only lists yield
    /// nothing.
    ///
    /// Non-vacuity: three servers go in (http + sse + stdio), only the stdio may
    /// come out — a mutant that forwards all transports reddens `len`; a mutant
    /// that forwards nothing also reddens `len`. Distinctive names make the
    /// survivor identity assertions unambiguous.
    #[test]
    fn test_mcp_servers_from_acp_drops_http_and_sse_keeps_stdio() {
        let mixed = vec![
            acp::McpServer::Http(acp::McpServerHttp::new(
                "dropped-http",
                "https://example.invalid/mcp",
            )),
            acp::McpServer::Sse(acp::McpServerSse::new(
                "dropped-sse",
                "https://example.invalid/sse",
            )),
            acp::McpServer::Stdio(
                acp::McpServerStdio::new("stdio-keeper", "/cmd").args(vec!["--x".to_string()]),
            ),
        ];

        let specs = mcp_servers_from_acp(mixed);
        assert_eq!(
            specs.len(),
            1,
            "only the stdio server survives; http/sse must be dropped, got {specs:?}"
        );
        assert_eq!(specs[0].id, "stdio-keeper");
        assert_eq!(specs[0].transport, McpTransport::Stdio);
        assert_eq!(specs[0].args, vec!["--x".to_string()]);

        // http-only and sse-only: nothing is forwarded (the all-dropped fast
        // path — distinct from an empty input, which also yields empty).
        let http_only = vec![acp::McpServer::Http(acp::McpServerHttp::new(
            "solo-http",
            "https://example.invalid/mcp",
        ))];
        assert!(
            mcp_servers_from_acp(http_only).is_empty(),
            "an http-only list must forward nothing"
        );
        let sse_only = vec![acp::McpServer::Sse(acp::McpServerSse::new(
            "solo-sse",
            "https://example.invalid/sse",
        ))];
        assert!(
            mcp_servers_from_acp(sse_only).is_empty(),
            "an sse-only list must forward nothing"
        );

        // empty input → empty output (the builtin-full fast path's input shape).
        assert!(
            mcp_servers_from_acp(Vec::new()).is_empty(),
            "an empty input must forward an empty slice"
        );
    }
    // ── `__` collapse: a literal double-underscore in the name must not reach the id ──

    /// A client-supplied name containing `__` (no whitespace) is normalized so
    /// the resulting id never contains a double-underscore. Without this, the id
    /// is forwarded verbatim and `project_tool`'s release-build
    /// `assert!(!server_id.contains("__"))` panics on the first prompt turn
    /// (server names are client-controlled — AC3's threat model). This is the
    /// case the whitespace-normalization test above claims to cover but didn't.
    #[test]
    fn test_mcp_servers_from_acp_collapses_double_underscore_in_id() {
        let cases: &[(&str, &str)] = &[
            ("my__server", "my_server"),
            ("a____b", "a_b"), // runs of `_` collapse fully
            ("keep__one__two", "keep_one_two"),
            ("under_score", "under_score"), // single `_` is preserved
        ];
        for (name, expected) in cases {
            let specs = mcp_servers_from_acp(vec![stdio_server(name, "/cmd")]);
            assert_eq!(specs.len(), 1, "name {name:?} should yield one stdio spec");
            assert_eq!(
                specs[0].id, *expected,
                "double-underscore in name {name:?} must collapse to {expected:?}"
            );
            assert!(
                !specs[0].id.contains("__"),
                "normalized id must never contain `__`: {:?}",
                specs[0].id
            );
        }
    }

    // ── empty / whitespace-only name is dropped, not forwarded ──────────

    /// An empty or all-whitespace name yields an empty id, which would produce
    /// tools that are advertised but uncallable (`parse_mcp_tool_name` returns
    /// `None` for an empty server). Such a server is dropped with a warn rather
    /// than forwarded as a broken spec.
    #[test]
    fn test_mcp_servers_from_acp_drops_empty_name() {
        for name in ["", "   ", "\t"] {
            let specs = mcp_servers_from_acp(vec![stdio_server(name, "/cmd")]);
            assert!(
                specs.is_empty(),
                "empty/whitespace name {name:?} must be dropped, got {specs:?}"
            );
        }
        // A valid server after a dropped-empty one still forwards.
        let specs =
            mcp_servers_from_acp(vec![stdio_server("", "/cmd"), stdio_server("good", "/cmd")]);
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].id, "good");
    }

    // ── duplicate / colliding ids: first wins, rest dropped ─────────────

    /// Two names that collapse to the same id (a literal duplicate, or names
    /// that normalize together like "a b" and "a_b") must not both become specs
    /// — that would mint two MCP clients with identical ids and make tool
    /// dispatch nondeterministic. The first is kept; later collisions are
    /// dropped with a warn.
    #[test]
    fn test_mcp_servers_from_acp_dedups_colliding_ids() {
        let specs = mcp_servers_from_acp(vec![
            stdio_server("alpha beta", "/cmd"),
            stdio_server("alpha_beta", "/cmd"), // collapses to the same id
            stdio_server("gamma", "/cmd"),
            stdio_server("gamma", "/cmd"), // literal duplicate
        ]);
        assert_eq!(
            specs.len(),
            2,
            "colliding/duplicate ids must be deduped, got {specs:?}"
        );
        assert_eq!(specs[0].id, "alpha_beta");
        assert_eq!(specs[1].id, "gamma");
    }
    // ── ACP "follow" feature: ToolCall carries ToolCallLocation ─────────

    use super::{message_to_replay_updates, stream_chunk_to_session_update};
    use crate::domain::models::{ChatMessage, MessageRole, StreamChunk, ToolCallInfo};
    use std::path::Path;
    /// A `read` tool call emits a `ToolCall` whose `locations` point Zed's
    /// "follow" feature at the file, with a relative path resolved against the
    /// session `cwd`. Red-on-mutant: dropping the `.locations(...)` call in
    /// `stream_chunk_to_session_update` reddens the `len()` assertion.
    #[test]
    fn read_tool_use_emits_tool_call_with_cwd_resolved_location() {
        let chunk = StreamChunk::ToolUse {
            id: "tc-1".into(),
            name: "read".into(),
            input: serde_json::json!({ "file_path": "src/lib.rs" }),
        };
        let update = stream_chunk_to_session_update(&chunk, Path::new("/repo"))
            .expect("read ToolUse must map to a ToolCall");
        let acp::SessionUpdate::ToolCall(call) = update else {
            panic!("expected ToolCall, got {update:?}");
        };
        assert_eq!(
            call.locations.len(),
            1,
            "a read must advertise exactly one follow location"
        );
        assert_eq!(
            call.locations[0].path,
            std::path::PathBuf::from("/repo/src/lib.rs"),
            "relative file_path must be resolved against cwd"
        );
        assert!(
            call.locations[0].line.is_none(),
            "no line hint is available for a plain read"
        );
        assert_eq!(
            call.kind,
            acp::ToolKind::Read,
            "a read must advertise kind=Read so Zed treats it as a file-open follow"
        );
    }

    /// Absolute paths pass through verbatim (never re-anchored to cwd); the
    /// tool name match is case-insensitive (`Read` == `read`).
    #[test]
    fn read_tool_use_keeps_absolute_path_verbatim() {
        let chunk = StreamChunk::ToolUse {
            id: "tc-2".into(),
            name: "Read".into(),
            input: serde_json::json!({ "file_path": "/etc/hostname" }),
        };
        let update = stream_chunk_to_session_update(&chunk, Path::new("/repo")).unwrap();
        let acp::SessionUpdate::ToolCall(call) = update else {
            panic!("expected ToolCall");
        };
        assert_eq!(
            call.locations[0].path,
            std::path::PathBuf::from("/etc/hostname")
        );
    }

    /// A non-file tool (`bash`) advertises NO location — never point the editor
    /// at a guessed file. Guards against a mutant that attaches a bogus path to
    /// every tool call.
    #[test]
    fn non_file_tool_emits_no_location() {
        let chunk = StreamChunk::ToolUse {
            id: "tc-3".into(),
            name: "bash".into(),
            input: serde_json::json!({ "command": "ls -la" }),
        };
        let update = stream_chunk_to_session_update(&chunk, Path::new("/repo")).unwrap();
        let acp::SessionUpdate::ToolCall(call) = update else {
            panic!("expected ToolCall");
        };
        assert!(
            call.locations.is_empty(),
            "bash must not advertise a follow location"
        );
        assert_eq!(
            call.kind,
            acp::ToolKind::Other,
            "a non-file tool must carry no kind (the unset default), so Zed does \
             not render it as a file-open or inline-edit follow"
        );
    }

    // ── Story 14-11 Task 2 — `edit` surfaces as an Edit-kind hunk Diff ────
    //
    // The Diff is built from the input at ToolUse time (old_string/new_string
    // are already present — no execution needed), mirroring codex-acp's shape.
    // These are RED today: `stream_chunk_to_session_update` never sets `.kind`
    // (defaults to `Other`) nor `.content` (empty), so the kind/Diff assertions
    // fail until Task 2 lands.

    /// An `edit` (search-and-replace) tool call surfaces as an Edit-kind
    /// `ToolCall` carrying exactly ONE hunk `Diff` whose path/old_text/new_text
    /// mirror the input, plus exactly one cwd-resolved follow location. This is
    /// the codex-parity surface Zed matches to follow + render the edited region.
    ///
    /// Red-on-mutant: drop `.kind(Edit)` → `Other` RED; drop `.content([Diff])`
    /// → `len()==0` RED; swap old/new → field-equalities RED; mis-resolve path →
    /// path/location assertions RED.
    #[test]
    fn edit_tool_use_emits_edit_kind_with_hunk_diff() {
        let chunk = StreamChunk::ToolUse {
            id: "tc-edit".into(),
            name: "edit".into(),
            input: serde_json::json!({
                "file_path": "src/lib.rs",
                "old_string": "fn old() {}",
                "new_string": "fn new() {}",
            }),
        };
        let update = stream_chunk_to_session_update(&chunk, Path::new("/repo"))
            .expect("an edit ToolUse must map to a ToolCall");
        let acp::SessionUpdate::ToolCall(call) = update else {
            panic!("expected ToolCall, got {update:?}");
        };

        // kind = Edit (the Zed rendering discriminant for an inline buffer diff).
        assert_eq!(
            call.kind,
            acp::ToolKind::Edit,
            "an edit must advertise kind=Edit so Zed renders an inline buffer diff"
        );

        // Exactly one follow location; relative path resolved against cwd.
        assert_eq!(
            call.locations.len(),
            1,
            "an edit advertises exactly one follow location (its file)"
        );
        let resolved = std::path::PathBuf::from("/repo/src/lib.rs");
        assert_eq!(
            call.locations[0].path, resolved,
            "the relative file_path is resolved against cwd"
        );

        // Exactly ONE hunk Diff, hunk-scoped (not whole-file): path/old_text/
        // new_text mirror the input directly.
        assert_eq!(
            call.content.len(),
            1,
            "an edit carries exactly one hunk Diff"
        );
        let diff = match &call.content[0] {
            acp::ToolCallContent::Diff(d) => d,
            other => panic!("expected ToolCallContent::Diff, got {other:?}"),
        };
        assert_eq!(
            diff.path, resolved,
            "Diff path is the cwd-resolved file path (same target as the location)"
        );
        assert_eq!(
            diff.old_text.as_deref(),
            Some("fn old() {}"),
            "Diff.old_text is the input old_string (hunk-scoped, not whole-file)"
        );
        assert_eq!(
            diff.new_text.as_str(),
            "fn new() {}",
            "Diff.new_text is the input new_string"
        );
    }

    /// The canonical capitalized `Edit` surfaces identically to the lowercase
    /// `edit` alias: same Edit kind, one location, one hunk Diff mirroring the
    /// input. An absolute file_path passes through verbatim (never re-anchored).
    /// Guards against a mapper that classifies only one spelling.
    #[test]
    fn edit_tool_use_canonical_capitalized_alias_surfaces_identically() {
        let chunk = StreamChunk::ToolUse {
            id: "tc-edit-cap".into(),
            name: "Edit".into(),
            input: serde_json::json!({
                "file_path": "/abs/Cargo.toml",
                "old_string": "name = \"old\"",
                "new_string": "name = \"new\"",
            }),
        };
        let update = stream_chunk_to_session_update(&chunk, Path::new("/repo"))
            .expect("capitalized Edit ToolUse must map to a ToolCall");
        let acp::SessionUpdate::ToolCall(call) = update else {
            panic!("expected ToolCall, got {update:?}");
        };

        assert_eq!(
            call.kind,
            acp::ToolKind::Edit,
            "the capitalized Edit alias must also be kind=Edit"
        );
        assert_eq!(call.locations.len(), 1);
        assert_eq!(
            call.locations[0].path,
            std::path::PathBuf::from("/abs/Cargo.toml"),
            "an absolute file_path passes through verbatim"
        );
        assert_eq!(
            call.content.len(),
            1,
            "the Edit alias carries one hunk Diff"
        );
        let diff = match &call.content[0] {
            acp::ToolCallContent::Diff(d) => d,
            other => panic!("expected Diff, got {other:?}"),
        };
        assert_eq!(diff.path, std::path::PathBuf::from("/abs/Cargo.toml"));
        assert_eq!(diff.old_text.as_deref(), Some("name = \"old\""));
        assert_eq!(diff.new_text.as_str(), "name = \"new\"");
    }

    /// A persisted read tool call is replayed with its location on
    /// `session/load`, so a reconnecting Zed client can still follow prior turns.
    #[test]
    fn replay_emits_completed_tool_call_with_location() {
        let message = ChatMessage {
            role: MessageRole::Assistant,
            tool_calls: vec![ToolCallInfo {
                id: "tc-replay".into(),
                name: "read".into(),
                input: serde_json::json!({ "file_path": "a/b.rs" }),
                result: None,
                started_at_ms: None,
                completed_at_ms: None,
                status: None,
            }],
            authorship: Default::default(),
            retracted_at_ms: None,
            ..Default::default()
        };
        let updates = message_to_replay_updates(&message, Path::new("/repo"));
        let tool_calls: Vec<_> = updates
            .into_iter()
            .filter_map(|u| match u {
                acp::SessionUpdate::ToolCall(c) => Some(c),
                _ => None,
            })
            .collect();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(
            tool_calls[0].status,
            acp::ToolCallStatus::Completed,
            "replayed tool calls are already complete"
        );
        assert_eq!(
            tool_calls[0].locations[0].path,
            std::path::PathBuf::from("/repo/a/b.rs"),
            "replay must carry the follow location"
        );
    }

    // ── Story 14-11 Task 4 — `apply_patch` surfaces as multi-hunk Diffs ───
    //
    // The mapper parses the input `patch` at ToolUse time into one
    // `ToolCallContent::Diff` per hunk + one `ToolCallLocation` per file,
    // `kind = Edit` (codex-parity). RED today: `edit_tool_call_content`
    // returns an empty Vec for `apply_patch` (it only handles `edit`), and
    // `tool_call_locations` reads a top-level `file_path` (apply_patch carries
    // `patch` — paths live inside it), so both the Diff-content and the
    // multi-location assertions fail. `kind=Edit` for apply_patch is already
    // wired in `tool_call_kind`, so that assertion passes and guards it.

    /// A valid `apply_patch` with ONE Update File hunk + ONE Add File hunk
    /// surfaces as a single Edit-kind `ToolCall` with exactly TWO locations
    /// (one per file, cwd-resolved) and TWO `ToolCallContent::Diff` entries:
    /// the Update hunk hunk-scoped old/new text, the Add hunk new-text-only.
    #[test]
    fn apply_patch_tool_use_emits_edit_kind_with_one_diff_per_hunk() {
        let patch = "*** Begin Patch\n*** Update File: src/existing.rs\n@@\n-old fn\n+new fn\n*** Add File: src/created.rs\n+fresh line\n*** End Patch\n";
        let chunk = StreamChunk::ToolUse {
            id: "tc-patch".into(),
            name: "apply_patch".into(),
            input: serde_json::json!({ "patch": patch }),
        };
        let update = stream_chunk_to_session_update(&chunk, Path::new("/repo"))
            .expect("an apply_patch ToolUse must map to a ToolCall");
        let acp::SessionUpdate::ToolCall(call) = update else {
            panic!("expected ToolCall, got {update:?}");
        };

        assert_eq!(
            call.kind,
            acp::ToolKind::Edit,
            "apply_patch must advertise kind=Edit so Zed renders inline buffer diffs"
        );

        // One follow location per file touched, cwd-resolved, in patch order.
        assert_eq!(call.locations.len(), 2, "one location per file");
        assert_eq!(
            call.locations[0].path,
            std::path::PathBuf::from("/repo/src/existing.rs"),
            "the Update hunk's relative path resolves against cwd"
        );
        assert_eq!(
            call.locations[1].path,
            std::path::PathBuf::from("/repo/src/created.rs"),
            "the Add hunk's relative path resolves against cwd"
        );

        // One Diff per hunk, in patch order.
        assert_eq!(call.content.len(), 2, "one hunk Diff per file");

        // Hunk 1 — Update File: hunk-scoped old/new text.
        let update_diff = match &call.content[0] {
            acp::ToolCallContent::Diff(d) => d,
            other => panic!("content[0] must be a Diff, got {other:?}"),
        };
        assert_eq!(
            update_diff.path,
            std::path::PathBuf::from("/repo/src/existing.rs")
        );
        assert_eq!(
            update_diff.old_text.as_deref(),
            Some("old fn"),
            "Update hunk old_text is the joined `-` lines (hunk-scoped)"
        );
        assert_eq!(update_diff.new_text.as_str(), "new fn");

        // Hunk 2 — Add File: new content only, no old_text.
        let add_diff = match &call.content[1] {
            acp::ToolCallContent::Diff(d) => d,
            other => panic!("content[1] must be a Diff, got {other:?}"),
        };
        assert_eq!(
            add_diff.path,
            std::path::PathBuf::from("/repo/src/created.rs")
        );
        assert_eq!(
            add_diff.old_text, None,
            "an Add File hunk has no prior content — old_text must be absent"
        );
        assert_eq!(add_diff.new_text.as_str(), "fresh line");
    }

    /// A MALFORMED `apply_patch` (no `*** End Patch` terminator) must NOT be
    /// dropped: the `ToolCall` is still emitted with `kind=Edit`, and content
    /// falls back to raw text rather than the empty Vec a parse failure leaves.
    #[test]
    fn apply_patch_malformed_preserves_tool_call_with_text_fallback() {
        let malformed = "*** Begin Patch\n*** Update File: src/existing.rs\n@@\n-old fn\n+new fn\n";
        let chunk = StreamChunk::ToolUse {
            id: "tc-patch-bad".into(),
            name: "apply_patch".into(),
            input: serde_json::json!({ "patch": malformed }),
        };
        let update = stream_chunk_to_session_update(&chunk, Path::new("/repo"))
            .expect("a malformed apply_patch must still emit a ToolCall");
        let acp::SessionUpdate::ToolCall(call) = update else {
            panic!("expected ToolCall, got {update:?}");
        };

        assert_eq!(
            call.kind,
            acp::ToolKind::Edit,
            "kind=Edit is independent of parse success"
        );
        assert!(
            !call.content.is_empty(),
            "a malformed patch must fall back to raw text content, not an empty Vec that \
             drops the edit from the client's view"
        );
        assert!(
            !matches!(&call.content[0], acp::ToolCallContent::Diff(_)),
            "the parse-failure fallback is raw text, not a spurious half-parsed Diff"
        );
    }
}
