use std::collections::HashSet;
use std::sync::Arc;

use rand_core::RngCore;
use serde_json::Value;

use crate::domain::errors::ToolError;
use crate::domain::models::{
    ProposalRaiseOutcome, SemanticMessageType, TeamProposal, ToolDefinition, ToolInvocation,
    ToolResult, semantic_message_type_tokens,
};
use crate::domain::ports::TeamProposalSink;

use super::driver::A2aDelegationRuntime;
use super::send::{SendRequest, validate_send_request};

pub(crate) const TOOL_NAME: &str = "propose_team_message";
const MAX_BODY_BYTES: usize = 131_072;
const NO_SURFACE: &str =
    "Not proposed: no operator surface is attached to approve it. Nothing was sent.";
const ALREADY_PENDING: &str = "Not proposed: an earlier proposal is still awaiting the operator's decision. Nothing was sent.";

/// Model-facing proposal capability. It never sends to a recipient; the sink
/// only opens an operator review surface.
pub(crate) struct TeamProposalTool {
    runtime: Arc<A2aDelegationRuntime>,
    sink: Option<Arc<dyn TeamProposalSink>>,
}

impl TeamProposalTool {
    pub(crate) fn new(
        runtime: Arc<A2aDelegationRuntime>,
        sink: Option<Arc<dyn TeamProposalSink>>,
    ) -> Self {
        Self { runtime, sink }
    }

    pub(crate) fn definition() -> ToolDefinition {
        static DEFINITION: std::sync::LazyLock<ToolDefinition> =
            std::sync::LazyLock::new(|| ToolDefinition {
                name: TOOL_NAME.to_owned(),
                description: "Propose a team message for operator approval; this does not send it."
                    .to_owned(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "recipients": {
                            "type": "array",
                            "items": { "type": "string" },
                            "minItems": 1,
                            "description": "Exact configured team roster aliases"
                        },
                        "type": {
                            "type": "string",
                            "enum": semantic_message_type_tokens().collect::<Vec<_>>()
                        },
                        "body": {
                            "type": "string",
                            "minLength": 1,
                            "maxLength": MAX_BODY_BYTES
                        }
                    },
                    "required": ["recipients", "type", "body"],
                    "additionalProperties": false
                }),
                parallel_safe: false,
            });
        DEFINITION.clone()
    }

    pub(crate) async fn execute(
        &self,
        tool_use_id: &str,
        input: Value,
        invocation: &ToolInvocation<'_>,
    ) -> Result<ToolResult, ToolError> {
        let (recipients, message_type, body) = parse_input(input, &self.runtime)?;
        let Some(sink) = self.sink.as_ref() else {
            return Ok(result(tool_use_id, NO_SURFACE));
        };

        let proposal = Arc::new(TeamProposal {
            id: mint_proposal_id(),
            conversation_id: invocation.conversation_id.to_owned(),
            recipients,
            message_type,
            body,
        });
        match sink.raise(proposal.clone()).await {
            ProposalRaiseOutcome::Shown => Ok(result(
                tool_use_id,
                format!(
                    "Proposal shown to the operator. Nothing has been sent; the operator decides whether to send it to {}.",
                    proposal.recipients.join(", ")
                ),
            )),
            ProposalRaiseOutcome::Unavailable => Ok(result(tool_use_id, NO_SURFACE)),
            ProposalRaiseOutcome::Pending => Ok(result(tool_use_id, ALREADY_PENDING)),
        }
    }
}

fn result(tool_use_id: &str, content: impl Into<String>) -> ToolResult {
    ToolResult {
        tool_use_id: tool_use_id.to_owned(),
        content: content.into(),
        is_error: false,
    }
}

fn mint_proposal_id() -> String {
    // The id binds a human decision to one immutable snapshot across restarts.
    // A process-local sequence can authorize a different proposal after restart.
    let mut nonce = [0_u8; 16];
    rand_core::OsRng.fill_bytes(&mut nonce);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut id = String::with_capacity(35);
    id.push_str("tp_");
    for byte in nonce {
        id.push(char::from(HEX[usize::from(byte >> 4)]));
        id.push(char::from(HEX[usize::from(byte & 15)]));
    }
    id
}

fn parse_input(
    mut input: Value,
    runtime: &A2aDelegationRuntime,
) -> Result<(Vec<String>, SemanticMessageType, String), ToolError> {
    let object = input
        .as_object_mut()
        .ok_or_else(|| invalid("expected an object"))?;
    if let Some(field) = object
        .keys()
        .find(|field| !matches!(field.as_str(), "recipients" | "type" | "body"))
    {
        return Err(invalid(&format!(
            "unexpected field {field}; only recipients, type, and body are permitted"
        )));
    }

    let recipients_value = object
        .remove("recipients")
        .ok_or_else(|| invalid("missing recipients"))?;
    let Value::Array(recipient_values) = recipients_value else {
        return Err(invalid("recipients must be an array"));
    };
    if recipient_values.is_empty() {
        return Err(invalid("recipients must contain at least one alias"));
    }
    let recipients: Vec<String> = recipient_values
        .into_iter()
        .map(|value| match value {
            Value::String(alias) => Ok(alias),
            _ => Err(invalid("each recipient must be a string alias")),
        })
        .collect::<Result<_, _>>()?;
    let mut seen = HashSet::with_capacity(recipients.len());
    for alias in &recipients {
        if alias.is_empty() {
            return Err(invalid("recipient aliases must be nonempty"));
        }
        if !seen.insert(alias.as_str()) {
            return Err(invalid(&format!("duplicate recipient alias: {alias}")));
        }
    }

    let type_value = object.remove("type").ok_or_else(invalid_message_type)?;
    let token = type_value.as_str().ok_or_else(invalid_message_type)?;
    let message_type = SemanticMessageType::parse(token);
    if message_type.is_unknown() {
        return Err(invalid_message_type());
    }

    let body_value = object
        .remove("body")
        .ok_or_else(|| invalid("missing body"))?;
    let Value::String(body) = body_value else {
        return Err(invalid("body must be a string"));
    };
    if body.is_empty() {
        return Err(invalid("body must be nonempty"));
    }
    if body.len() > MAX_BODY_BYTES {
        return Err(invalid(&format!(
            "body is {} bytes; the limit is 131072",
            body.len()
        )));
    }
    validate_body_characters(&body)?;

    let request = SendRequest {
        recipients: &recipients,
        text: &body,
        message_type: Some(message_type),
        reply_to: None,
        subject: None,
        authorship: crate::domain::models::ReplyAuthorship::AgentComposed,
    };
    validate_send_request(runtime, &request)
        .map_err(|error| invalid(&format!("invalid recipients: {error}")))?;
    Ok((recipients, message_type, body))
}

fn validate_body_characters(body: &str) -> Result<(), ToolError> {
    for (offset, character) in body.char_indices() {
        let codepoint = character as u32;
        let forbidden = (codepoint <= 0x1f && character != '\n' && character != '\t')
            || (0x7f..=0x9f).contains(&codepoint)
            || (0x202a..=0x202e).contains(&codepoint)
            || (0x2066..=0x2069).contains(&codepoint);
        if forbidden {
            return Err(invalid(&format!(
                "body contains a forbidden control character at byte offset {offset}"
            )));
        }
    }
    Ok(())
}

fn invalid(message: &str) -> ToolError {
    ToolError::InvalidInput(message.to_owned())
}

fn invalid_message_type() -> ToolError {
    invalid(&format!(
        "required type must be one of: {}",
        semantic_message_type_tokens()
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

/// Direct invocation has no call-scoped identity, so it must not manufacture
/// a proposal with a binding from mutable composite state.
pub(crate) fn missing_invocation(tool_use_id: &str) -> Result<ToolResult, ToolError> {
    Err(ToolError::InvalidInput(format!(
        "{TOOL_NAME} requires call-scoped invocation context (tool use {tool_use_id})"
    )))
}
