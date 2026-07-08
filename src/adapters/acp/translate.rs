use std::collections::BTreeMap;
use std::path::Path;

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
fn tool_call_locations(
    tool_name: &str,
    input: &serde_json::Value,
    cwd: &Path,
) -> Vec<acp::ToolCallLocation> {
    let Some(path_str) = (match tool_name.to_ascii_lowercase().as_str() {
        "read" | "write" | "edit" | "apply_patch" | "view" | "read_file" | "write_file" => input
            .get("file_path")
            .or_else(|| input.get("path"))
            .and_then(serde_json::Value::as_str),
        _ => None,
    }) else {
        return Vec::new();
    };
    let path = Path::new(path_str);
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    vec![acp::ToolCallLocation::new(abs)]
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
        StreamChunk::ToolUse { id, name, input } => Some(acp::SessionUpdate::ToolCall(
            acp::ToolCall::new(id.clone(), name.clone())
                .status(acp::ToolCallStatus::Pending)
                .locations(tool_call_locations(name, input, cwd))
                .raw_input(input.clone()),
        )),
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
        updates.push(acp::SessionUpdate::ToolCall(
            acp::ToolCall::new(tc.id.clone(), tc.name.clone())
                .status(acp::ToolCallStatus::Completed)
                .locations(tool_call_locations(&tc.name, &tc.input, cwd))
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
}
