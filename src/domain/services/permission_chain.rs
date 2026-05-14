//! PermissionChain — domain service that orchestrates permission checks.
//! Pure orchestration: calls port traits, no I/O itself.

use crate::domain::models::{
    ActiveSkill, FileOperation, PermissionMode, ToolRisk, risk_for_builtin,
};
use crate::domain::ports::SecurityPort;
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

    // Step 3: Workspace restriction (file tools)
    if let Some((path_str, op)) = extract_file_path(tool_name, input) {
        // Plan-file write exception: allow Write/Edit to the plan file when in Plan mode.
        if let Some(plan) = plan_file {
            if matches!(tool_name, "Write" | "Edit") {
                if let Some(p) = input.get("file_path").and_then(|v| v.as_str()) {
                    if std::path::Path::new(p) == plan {
                        return PermissionDecision::Allow;
                    }
                }
            }
        }
        if let Err(e) = security.check_workspace_access(std::path::Path::new(&path_str), op) {
            return PermissionDecision::Deny(e.to_string());
        }
    } else if tool_name == "Read" {
        if input.get("file_path").and_then(|v| v.as_str()).is_none() {
            return PermissionDecision::Deny(
                "Read tool missing required 'file_path' field".to_string(),
            );
        }
    } else if matches!(tool_name, "Write" | "Edit") {
        if input.get("file_path").and_then(|v| v.as_str()).is_none() {
            return PermissionDecision::Deny(format!(
                "{} tool missing required 'file_path' field",
                tool_name
            ));
        }
    }

    // Step 3.5: Mode × risk gating (AC1, AC7)
    let risk = risk_for_builtin(tool_name);
    let mode = security.current_mode();
    match mode_risk_outcome(mode, risk) {
        Some(true) => return PermissionDecision::Allow,
        Some(false) => {
            let reason = match (mode, risk) {
                (_, ToolRisk::Blocked) => format!("Tool '{}' is blocked", tool_name),
                (PermissionMode::Plan, _) => {
                    "Plan mode is active; you cannot modify state. Revise the plan or call exit_plan_mode.".to_string()
                }
                _ => format!("Mode {:?}: tool disallowed (risk: {:?})", mode, risk),
            };
            return PermissionDecision::Deny(reason);
        }
        None => {
            // needs prompt — route to ApprovalRuntime
            return PermissionDecision::Prompt {
                server_id: derive_server_id(tool_name),
                path_hint: derive_path_hint(tool_name, input),
            };
        }
    }
}

/// Derive server_id from tool_name (MCP pattern `<server>.<tool>`).
/// Today returns None for all built-ins — full implementation lands in 9-2.
fn derive_server_id(tool_name: &str) -> Option<String> {
    tool_name.find('.').map(|dot| tool_name[..dot].to_string())
}

/// Derive path_hint from tool_name and input (for Read/Write/Edit).
fn derive_path_hint(tool_name: &str, input: &serde_json::Value) -> Option<String> {
    match tool_name {
        "Read" | "Write" | "Edit" => input
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
        "Write" | "Edit" => FileOperation::Write,
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
