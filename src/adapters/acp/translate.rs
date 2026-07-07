use agent_client_protocol as acp;

use crate::domain::models::{ApprovalOutcome, ChatMessage, MessageRole, StopReason, StreamChunk};
use crate::domain::services::approval_runtime::ApprovalRuntimeEvent;

pub const PERMISSION_ALLOW_ONCE: &str = "allow_once";
pub const PERMISSION_ALLOW_ALWAYS_TOOL: &str = "allow_always_tool";
pub const PERMISSION_REJECT_ONCE: &str = "reject_once";
pub const SKILL_TRUST_ALLOW: &str = "skill_trust_allow";
pub const SKILL_TRUST_REJECT: &str = "skill_trust_reject";

pub fn stop_reason_to_acp(reason: &StopReason) -> acp::StopReason {
    match reason {
        StopReason::EndTurn => acp::StopReason::EndTurn,
        StopReason::MaxTokens => acp::StopReason::MaxTokens,
        StopReason::Cancelled => acp::StopReason::Cancelled,
        _ => acp::StopReason::Refusal,
    }
}

pub fn stream_chunk_to_session_update(chunk: &StreamChunk) -> Option<acp::SessionUpdate> {
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
pub fn message_to_replay_updates(message: &ChatMessage) -> Vec<acp::SessionUpdate> {
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
