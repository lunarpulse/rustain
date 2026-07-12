//! PermissionChain — domain service that orchestrates permission checks.
//! Pure orchestration: calls port traits, no I/O itself.

use crate::domain::models::{
    ActiveSkill, ApprovalSource, FileOperation, PermissionMode, ProvenanceTag, TaintDecision,
    ToolRisk, risk_for_builtin,
};
use crate::domain::ports::{SecurityPort, ToolSetPort};
use std::collections::HashSet;

/// Result of a permission chain check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionDecision {
    /// Auto-allow (mode×risk or blocklist/workspace passed).
    Allow,
    /// Deny with reason string.
    Deny(String),
    /// Needs user approval — route to ApprovalRuntime::request.
    Prompt {
        server_id: Option<String>,
        path_hint: Option<String>,
    },
}

/// Format the user feedback string as it is presented to the LLM and to the chat stream.
/// AC5 canonical format: `Tool denied. User feedback: "<text>"` with embedded quotes
/// and backslashes escaped so the quoted display stays unambiguous when the user types
/// `"` or `\`. Pure; shared by the event loop (chat FeedbackBlock) and the turn
/// runtime (ToolResult sent back to the LLM) so both paths speak the same string.
pub fn format_feedback_message(feedback: &str) -> String {
    let escaped = feedback.replace('\\', "\\\\").replace('"', "\\\"");
    format!("Tool denied. User feedback: \"{}\"", escaped)
}

/// Parse the argument of `/mode <arg>` into a `PermissionMode`.
/// Trims and lowercases the argument. Returns `None` when the argument is
/// missing or does not match a known mode name. Used by the event loop to
/// dispatch `AppEvent::SetPermissionMode`; extracted as a pure function so
/// the parse rules can be unit-tested without spinning up the loop.
pub fn parse_mode_arg(arg: Option<&str>) -> Option<PermissionMode> {
    match arg.map(|s| s.trim().to_ascii_lowercase()) {
        Some(a) => match a.as_str() {
            "plan" => Some(PermissionMode::Plan),
            "normal" => Some(PermissionMode::Normal),
            "autoedit" | "auto" => Some(PermissionMode::AutoEdit),
            "yolo" => Some(PermissionMode::Yolo),
            _ => None,
        },
        None => None,
    }
}

/// Determine if mode × risk combination requires a prompt.
/// Returns `None` when mode fully determines outcome (no prompt needed).
/// Returns `Some(true)` when the combination allows (auto-allow).
/// Returns `Some(false)` when the combination denies (no prompt).
pub fn mode_risk_outcome(mode: PermissionMode, risk: ToolRisk) -> Option<bool> {
    match (mode, risk) {
        // Blocked always denies regardless of mode
        (_, ToolRisk::Blocked) => Some(false),
        // Safe always allows regardless of mode
        (_, ToolRisk::Safe) => Some(true),
        // Plan mode: Standard and Elevated are blocked (no prompt)
        (PermissionMode::Plan, ToolRisk::Standard) => Some(false),
        (PermissionMode::Plan, ToolRisk::Elevated) => Some(false),
        // Normal mode: Standard and Elevated require prompt
        (PermissionMode::Normal, ToolRisk::Standard) => None,
        (PermissionMode::Normal, ToolRisk::Elevated) => None,
        // AutoEdit mode: Standard auto-allowed, Elevated requires prompt
        (PermissionMode::AutoEdit, ToolRisk::Standard) => Some(true),
        (PermissionMode::AutoEdit, ToolRisk::Elevated) => None,
        // Yolo mode: everything auto-allowed
        (PermissionMode::Yolo, ToolRisk::Standard) => Some(true),
        (PermissionMode::Yolo, ToolRisk::Elevated) => Some(true),
    }
}

/// Check permission for a tool call through the full chain.
///
/// Steps:
/// 1. Tool restriction check (active skill allowed_tools)
/// 2. Blocklist check (Bash tool only)
/// 3. Workspace restriction (file tools — Read/Write)
/// 3.5 Mode × risk gating (AC1, AC7)
/// 4. request_permission dispatch (only when mode × risk = "prompt")
pub async fn check(
    security: &dyn SecurityPort,
    tool_name: &str,
    input: &serde_json::Value,
    active_skills: Option<&[ActiveSkill]>,
    plan_file: Option<&std::path::Path>,
    tools_port: &dyn ToolSetPort,
) -> PermissionDecision {
    check_with_source(
        security,
        tool_name,
        input,
        active_skills,
        plan_file,
        tools_port,
        None,
    )
    .await
}

/// Check permission with an explicit approval source (for recursion guard).
pub async fn check_with_source(
    security: &dyn SecurityPort,
    tool_name: &str,
    input: &serde_json::Value,
    active_skills: Option<&[ActiveSkill]>,
    plan_file: Option<&std::path::Path>,
    tools_port: &dyn ToolSetPort,
    source: Option<&crate::domain::models::tool_call::ApprovalSource>,
) -> PermissionDecision {
    check_with_source_and_provenance(
        security,
        tool_name,
        input,
        active_skills,
        plan_file,
        tools_port,
        source,
        ProvenanceTag::UserOriginated,
    )
    .await
}

/// Permission check with the node-derived provenance tag.
pub async fn check_with_source_and_provenance(
    security: &dyn SecurityPort,
    tool_name: &str,
    input: &serde_json::Value,
    active_skills: Option<&[ActiveSkill]>,
    plan_file: Option<&std::path::Path>,
    tools_port: &dyn ToolSetPort,
    source: Option<&crate::domain::models::tool_call::ApprovalSource>,
    provenance: ProvenanceTag,
) -> PermissionDecision {
    // Step 0: exit_plan_mode short-circuit
    if tool_name == "exit_plan_mode" {
        return match security.current_mode() {
            PermissionMode::Plan => PermissionDecision::Allow,
            _ => PermissionDecision::Deny(
                "exit_plan_mode is only available in Plan mode".to_string(),
            ),
        };
    }

    // Step 0.5: Recursion guard — subagents may not invoke the task tool
    if tool_name.eq_ignore_ascii_case("task") {
        if let Some(crate::domain::models::tool_call::ApprovalSource::ForegroundSubagent {
            ..
        }) = source
        {
            return PermissionDecision::Deny(
                "recursion guard: subagent is not allowed to invoke the 'task' tool".to_string(),
            );
        }
    }

    // Step 0.6: Provenance-taint gate. Taint is an additional approval
    // requirement, never a bypass around the hard-deny checks below.
    let path_hint = derive_path_hint(tool_name, input);
    let server_id = derive_server_id(tool_name);
    let taint_requires_approval = match taint_gate_with_risk(
        tool_name,
        input,
        source,
        path_hint.as_deref(),
        server_id.as_deref(),
        provenance,
        risk_for_tool(tool_name, tools_port),
    ) {
        TaintDecision::Allow => false,
        TaintDecision::RequireApproval { .. } => true,
        #[cfg(any(test, feature = "test-instrumentation"))]
        TaintDecision::Deny => {
            return PermissionDecision::Deny(
                "provenance-taint gate denied tool dispatch".to_string(),
            );
        }
    };

    // Step 1: Tool restriction (active skill allowed_tools)
    // activate_skill is always allowed (carve-out for skill chaining)
    if tool_name != "activate_skill" {
        if let Some(deny_reason) = check_allowed_tools(tool_name, active_skills) {
            return PermissionDecision::Deny(deny_reason);
        }
    }

    // Step 2: Blocklist check (Bash tool only)
    if tool_name == "Bash" {
        match input.get("command").and_then(|v| v.as_str()) {
            Some(command) => {
                if let Err(e) = security.check_blocklist(command) {
                    return PermissionDecision::Deny(e.to_string());
                }
            }
            None => {
                return PermissionDecision::Deny(
                    "Bash tool missing required 'command' field".to_string(),
                );
            }
        }
    }

    let plan_file_exception = is_plan_file_write(tool_name, input, plan_file);

    // Step 3: Workspace restriction (file tools)
    if let Some((path_str, op)) = extract_file_path(tool_name, input) {
        if !plan_file_exception
            && let Err(e) = security.check_workspace_access(std::path::Path::new(&path_str), op)
        {
            return PermissionDecision::Deny(e.to_string());
        }
    } else if tool_name == "Read" {
        if input.get("file_path").and_then(|v| v.as_str()).is_none() {
            return PermissionDecision::Deny(
                "Read tool missing required 'file_path' field".to_string(),
            );
        }
    } else if matches!(tool_name, "Write" | "Edit")
        && input
            .get("file_path")
            .and_then(|value| value.as_str())
            .is_none()
    {
        return PermissionDecision::Deny(format!(
            "{} tool missing required 'file_path' field",
            tool_name
        ));
    }

    // Step 3.5: Mode × risk gating (AC1, AC7)
    let risk = risk_for_tool(tool_name, tools_port);
    let mode = security.current_mode();
    mode_risk_decision(
        mode,
        risk,
        plan_file_exception,
        taint_requires_approval,
        tool_name,
        server_id,
        path_hint,
    )
}

/// Whether this is the permitted Write/Edit operation for the active plan file.
fn is_plan_file_write(
    tool_name: &str,
    input: &serde_json::Value,
    plan_file: Option<&std::path::Path>,
) -> bool {
    plan_file.is_some_and(|plan| {
        matches!(tool_name, "Write" | "Edit")
            && input
                .get("file_path")
                .and_then(|value| value.as_str())
                .is_some_and(|path| std::path::Path::new(path) == plan)
    })
}

/// Resolve the mode and risk gate after all hard-deny checks have passed.
fn mode_risk_decision(
    mode: PermissionMode,
    risk: ToolRisk,
    plan_file_exception: bool,
    taint_requires_approval: bool,
    tool_name: &str,
    server_id: Option<String>,
    path_hint: Option<String>,
) -> PermissionDecision {
    match mode_risk_outcome(mode, risk) {
        Some(true) if taint_requires_approval => PermissionDecision::Prompt {
            server_id,
            path_hint,
        },
        Some(true) => PermissionDecision::Allow,
        Some(false) if plan_file_exception && risk != ToolRisk::Blocked => {
            if taint_requires_approval {
                PermissionDecision::Prompt {
                    server_id,
                    path_hint,
                }
            } else {
                PermissionDecision::Allow
            }
        }
        Some(false) => {
            let reason = match (mode, risk) {
                (_, ToolRisk::Blocked) => format!("Tool '{}' is blocked", tool_name),
                (PermissionMode::Plan, _) => {
                    "Plan mode is active; you cannot modify state. Revise the plan or call exit_plan_mode.".to_string()
                }
                _ => format!("Mode {:?}: tool disallowed (risk: {:?})", mode, risk),
            };
            PermissionDecision::Deny(reason)
        }
        None => PermissionDecision::Prompt {
            server_id,
            path_hint,
        },
    }
}

/// Provenance-taint gate (Story 17.1b, AC8).
///
/// Inspects the provenance driving a tool call. Self-originated data remains
/// silent for safe reads but requires explicit approval for destructive or
/// elevated sinks. The resulting approval requirement is applied only after
/// active-skill, blocklist, workspace, and mode hard-deny checks.
///
/// The standalone gate uses builtin risk classification. The dispatch path
/// calls [`taint_gate_with_risk`] with the composed tool registry's risk. Both
/// paths share the same deterministic policy.
pub fn taint_gate(
    tool: &str,
    input: &serde_json::Value,
    source: Option<&ApprovalSource>,
    path_hint: Option<&str>,
    server_id: Option<&str>,
    provenance: ProvenanceTag,
) -> TaintDecision {
    taint_gate_with_risk(
        tool,
        input,
        source,
        path_hint,
        server_id,
        provenance,
        crate::domain::models::risk_for_builtin(tool),
    )
}

/// Model-D narrow policy: only self-originated data driving a destructive or
/// elevated sink requires approval. Reads and benign/user data remain silent.
pub fn taint_gate_with_risk(
    tool: &str,
    input: &serde_json::Value,
    source: Option<&ApprovalSource>,
    path_hint: Option<&str>,
    server_id: Option<&str>,
    provenance: ProvenanceTag,
    risk: ToolRisk,
) -> TaintDecision {
    #[cfg(any(test, feature = "test-instrumentation"))]
    TAINT_GATE_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    #[cfg(any(test, feature = "test-instrumentation"))]
    if TAINT_GATE_FORCE_DENY.load(std::sync::atomic::Ordering::Relaxed) {
        return TaintDecision::Deny;
    }
    let _ = (tool, input, source, path_hint, server_id);
    if provenance == ProvenanceTag::SelfOriginated
        && matches!(risk, ToolRisk::Standard | ToolRisk::Elevated)
    {
        return TaintDecision::RequireApproval {
            reason: "tainted context requires approval for destructive tool".to_string(),
        };
    }
    TaintDecision::Allow
}

/// Wired + load-bearing counter (DD4, Murat — proof (a)): every real
/// dispatch through `check_with_source` increments this exactly once. A
/// mutant deleting the `taint_gate` call site drives `count < dispatches`,
/// caught by `ac2_taint_gate_counter_matches_dispatch_count`.
#[cfg(any(test, feature = "test-instrumentation"))]
pub static TAINT_GATE_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Test-only override forcing `taint_gate` to return [`TaintDecision::Deny`].
/// The real scheduler test proves a deny verdict blocks dispatch rather than
/// acting as an inert hook. Production builds cannot read or set this flag.
#[cfg(any(test, feature = "test-instrumentation"))]
pub static TAINT_GATE_FORCE_DENY: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Derive risk for a tool, considering both built-in and MCP tools.
///
/// For built-in names (no `mcp__` prefix): delegates to `risk_for_builtin`.
/// For MCP names (`mcp__<server>__<tool>`): reads `parallel_safe` from the
/// tool catalog; Safe when `parallel_safe == true`, Elevated otherwise.
/// Unknown names (including unresolvable MCP names) default to Elevated.
pub fn risk_for_tool(tool_name: &str, tools_port: &dyn ToolSetPort) -> ToolRisk {
    if tool_name.starts_with("mcp__") {
        // P-21: Verify tool actually exists in catalog before treating as MCP
        let available = tools_port.available_tools();
        let found = available.iter().find(|t| t.name == tool_name);
        if let Some(tool) = found {
            return if tool.parallel_safe {
                ToolRisk::Safe
            } else {
                ToolRisk::Elevated
            };
        }
        // Unknown MCP tool — fail-safe to Elevated
        return ToolRisk::Elevated;
    }
    risk_for_builtin(tool_name)
}

/// Derive server_id from tool_name (MCP pattern `mcp__<server>__<tool>`).
/// Returns `Some("mcp__<server>")` for MCP tools or `None` for built-ins.
/// Uses the `mcp__` prefix to prevent future skill/builtin name collisions.
fn derive_server_id(tool_name: &str) -> Option<String> {
    if let Some(rest) = tool_name.strip_prefix("mcp__") {
        if let Some((server, _tool)) = rest.split_once("__") {
            return Some(format!("mcp__{server}"));
        }
    }
    None
}

/// Derive path_hint from tool_name and input (for Read/Write/Edit).
fn derive_path_hint(tool_name: &str, input: &serde_json::Value) -> Option<String> {
    match tool_name {
        "Read" | "Write" | "Edit" | "edit" => input
            .get("file_path")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        _ => None,
    }
}

/// Extract file_path and infer FileOperation from tool name.
/// Returns None for tools that don't operate on file paths.
fn extract_file_path(
    tool_name: &str,
    input: &serde_json::Value,
) -> Option<(String, FileOperation)> {
    let op = match tool_name {
        "Read" => FileOperation::Read,
        "Write" | "Edit" | "edit" => FileOperation::Write,
        _ => return None,
    };
    let path = input.get("file_path").and_then(|v| v.as_str())?;
    Some((path.to_string(), op))
}

/// Check if the tool is allowed by the active skills' `allowed_tools`.
/// Returns `Some(deny_reason)` if denied, `None` if allowed or no constraints.
fn check_allowed_tools(tool_name: &str, active_skills: Option<&[ActiveSkill]>) -> Option<String> {
    let skills = active_skills?;
    let constrained: Vec<&Vec<String>> = skills
        .iter()
        .filter_map(|s| s.allowed_tools.as_ref())
        .collect();
    if constrained.is_empty() {
        return None;
    }
    let mut iter = constrained.iter();
    let first = iter.next()?;
    let mut effective: HashSet<String> = first.iter().cloned().collect();
    for set in iter {
        let other: HashSet<String> = set.iter().cloned().collect();
        effective = effective.intersection(&other).cloned().collect();
    }
    if effective.contains(tool_name) {
        return None;
    }
    let mut names: Vec<String> = effective.into_iter().collect();
    names.sort();
    let constrained_skill_names: Vec<&str> = skills
        .iter()
        .filter(|s| s.allowed_tools.is_some())
        .map(|s| s.name.as_str())
        .collect();
    let noun = if constrained_skill_names.len() == 1 {
        "skill"
    } else {
        "skills"
    };
    Some(format!(
        "Tool '{}' not allowed by {} '{}'. Allowed: [{}]",
        tool_name,
        noun,
        constrained_skill_names.join(", "),
        names.join(", ")
    ))
}

// Tests moved to tests/security.rs to satisfy domain purity conformance test.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_file_write_decision_table_preserves_taint_and_blocked_denials() {
        let plan_path = std::path::Path::new("/tmp/rustain-plan.md");
        let input = serde_json::json!({
            "file_path": plan_path.to_str().unwrap(),
            "content": "updated plan",
        });
        let plan_file_exception = is_plan_file_write("Write", &input, Some(plan_path));
        assert!(
            plan_file_exception,
            "the test input must target the plan file"
        );

        let cases = [
            (
                "untainted standard write",
                ToolRisk::Standard,
                false,
                PermissionDecision::Allow,
            ),
            (
                "tainted standard write",
                ToolRisk::Standard,
                true,
                PermissionDecision::Prompt {
                    server_id: None,
                    path_hint: Some(plan_path.display().to_string()),
                },
            ),
            (
                "blocked file tool",
                ToolRisk::Blocked,
                false,
                PermissionDecision::Deny("Tool 'Write' is blocked".to_string()),
            ),
        ];

        for (name, risk, taint_requires_approval, expected) in cases {
            assert_eq!(
                mode_risk_decision(
                    PermissionMode::Plan,
                    risk,
                    plan_file_exception,
                    taint_requires_approval,
                    "Write",
                    None,
                    Some(plan_path.display().to_string()),
                ),
                expected,
                "{name}"
            );
        }
    }
}
