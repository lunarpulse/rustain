//! Turn orchestrator with tool execution loop.
//! Sprint 1 pattern: stream → collect tool calls → execute → loop.

use std::sync::Arc;

use futures::StreamExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::domain::events::AppEvent;
use crate::domain::models::checkpoint::CheckpointId;
use crate::domain::models::{
    CompletionOptions, EscalationReason, Message, MessageRole, NoticeLevel, StepKind, StopReason,
    StreamChunk, TokenUsage, ToolCall, ToolCallInfo, ToolResultMessage, ToolUseMessage,
    UsageLedgerEntry,
};
use crate::domain::ports::{
    SecurityPort, StoragePort, StreamingProvider, ToolSetPort, UsageLedgerPort,
};
use crate::domain::services::model_router::ResolvedModel;
use crate::domain::services::tool_scheduler::ToolScheduler;

/// Execute a turn: stream completion, execute tools, loop until EndTurn.
///
/// The agentic loop:
/// 1. Call stream_completion with current messages
/// 2. Forward all chunks as AppEvent::ProviderChunk
/// 3. On TurnComplete(ToolUse): execute tools, append results, loop to 1
/// 4. On TurnComplete(EndTurn): done
/// Maximum number of tool execution loop iterations before forcing termination.
const MAX_TOOL_ITERATIONS: usize = 25;

pub async fn run_turn(
    provider: Arc<dyn StreamingProvider>,
    mut messages: Vec<Message>,
    options: CompletionOptions,
    event_tx: mpsc::UnboundedSender<AppEvent>,
    _security: Arc<dyn SecurityPort>,
    tools: Arc<dyn ToolSetPort>,
    tool_scheduler: Arc<ToolScheduler>,
    conversation_id: String,
    storage: Arc<dyn StoragePort>,
    conversation_snapshot: crate::domain::models::Conversation,
    activation_set: Option<crate::domain::models::SkillActivationSet>,
    turn_cancel: CancellationToken,
    ledger: Arc<dyn UsageLedgerPort>,
    resolved: ResolvedModel,
    step_kind: Option<StepKind>,
    parent_ctx_tokens: u32,
    session_id: String,
) {
    // Persist the conversation before the first API call so that
    // `create_checkpoint` (called when the API returns tool_use) can load it
    // from storage. Without this, the first tool call in a new conversation
    // fails checkpoint creation — snapshots get the sentinel CheckpointId(0)
    // and are never revertible.
    if let Err(e) = storage.save_conversation(&conversation_snapshot).await {
        tracing::warn!(
            "Pre-turn conversation save failed — checkpoint creation may fail: {}",
            e
        );
    }
    let mut iteration = 0;
    loop {
        iteration += 1;
        if iteration > MAX_TOOL_ITERATIONS {
            tracing::warn!(
                "Tool execution loop exceeded {} iterations — terminating",
                MAX_TOOL_ITERATIONS
            );
            let _ = event_tx.send(AppEvent::ProviderChunk {
                conversation_id: conversation_id.clone(),
                chunk: StreamChunk::Error {
                    content: format!(
                        "Tool execution loop exceeded {} iterations",
                        MAX_TOOL_ITERATIONS
                    ),
                },
            });
            let _ = event_tx.send(AppEvent::ProviderChunk {
                conversation_id: conversation_id.clone(),
                chunk: StreamChunk::TurnComplete {
                    stop_reason: StopReason::Cancelled,
                },
            });
            return;
        }
        match provider
            .stream_completion(messages.clone(), options.clone())
            .await
        {
            Ok(stream) => {
                futures::pin_mut!(stream);
                let mut received_turn_complete = false;
                let mut stop_reason = StopReason::EndTurn;
                let mut tool_calls: Vec<ToolCallInfo> = Vec::new();
                let mut accumulated_text = String::new();

                let mut iteration_usage: Option<crate::domain::models::UsageInfo> = None;

                while let Some(chunk) = stream.next().await {
                    match &chunk {
                        StreamChunk::TurnComplete { stop_reason: sr } => {
                            received_turn_complete = true;
                            stop_reason = sr.clone();
                        }
                        StreamChunk::ToolUse { id, name, input } => {
                            tool_calls.push(ToolCallInfo {
                                id: id.clone(),
                                name: name.clone(),
                                input: input.clone(),
                                result: None,
                                started_at_ms: Some(now_ms()),
                                completed_at_ms: None,
                                status: None,
                            });
                        }
                        StreamChunk::Text { content, .. } => {
                            accumulated_text.push_str(content);
                        }
                        StreamChunk::Usage { usage, .. } => {
                            iteration_usage = Some(usage.clone());
                        }
                        _ => {}
                    }
                    let _ = event_tx.send(AppEvent::ProviderChunk {
                        conversation_id: conversation_id.clone(),
                        chunk,
                    });
                }

                // Write ledger entry for this provider call (success path)
                let ledger_entry = UsageLedgerEntry {
                    timestamp_ms: chrono::Utc::now().timestamp_millis(),
                    session_id: session_id.clone(),
                    conversation_id: conversation_id.clone(),
                    provider_id: provider.provider_id(),
                    model: options.model.clone(),
                    tier: resolved.tier,
                    step_kind,
                    escalation_reason: resolved.escalation_reason,
                    usage: match iteration_usage {
                        Some(ref u) => TokenUsage {
                            tokens_in: u.input_tokens,
                            tokens_out: u.output_tokens,
                            parent_ctx: parent_ctx_tokens,
                        },
                        None => TokenUsage {
                            tokens_in: 0,
                            tokens_out: 0,
                            parent_ctx: parent_ctx_tokens,
                        },
                    },
                };
                if let Err(e) = ledger.append(ledger_entry).await {
                    tracing::warn!("Usage ledger append failed: {}", e);
                }

                // Safety: synthesize TurnComplete if stream ended without one
                if !received_turn_complete {
                    tracing::warn!("Provider stream ended without TurnComplete — synthesizing end");
                    let _ = event_tx.send(AppEvent::ProviderChunk {
                        conversation_id: conversation_id.clone(),
                        chunk: StreamChunk::Error {
                            content: "Stream disconnected unexpectedly".to_string(),
                        },
                    });
                    let _ = event_tx.send(AppEvent::ProviderChunk {
                        conversation_id: conversation_id.clone(),
                        chunk: StreamChunk::TurnComplete {
                            stop_reason: StopReason::Cancelled,
                        },
                    });
                    return;
                }

                match stop_reason {
                    StopReason::ToolUse => {
                        // Execute tool calls and continue the loop
                        if tool_calls.is_empty() {
                            tracing::warn!(
                                "TurnComplete(ToolUse) but no tool calls collected — synthesizing EndTurn"
                            );
                            let _ = event_tx.send(AppEvent::ProviderChunk {
                                conversation_id: conversation_id.clone(),
                                chunk: StreamChunk::TurnComplete {
                                    stop_reason: StopReason::EndTurn,
                                },
                            });
                            return;
                        }

                        // Build the assistant message with accumulated text and tool_use blocks.
                        // The Anthropic API requires the assistant's tool_use blocks to be
                        // present in the message history for multi-turn tool conversations.
                        let tool_use_msgs: Vec<ToolUseMessage> = tool_calls
                            .iter()
                            .map(|tc| ToolUseMessage {
                                id: tc.id.clone(),
                                name: tc.name.clone(),
                                input: tc.input.clone(),
                            })
                            .collect();
                        messages.push(Message {
                            role: MessageRole::Assistant,
                            content: std::mem::take(&mut accumulated_text),
                            images: vec![],
                            tool_results: vec![],
                            tool_uses: tool_use_msgs,
                            context_prefix: None,
                        });

                        // Create a checkpoint BEFORE executing any tools in this turn (AC2, Story 4-3b).
                        // The checkpoint captures the conversation state just before the assistant's
                        // tool-executing turn. If creation fails, we fall through with a sentinel
                        // CheckpointId(0) — tools run but rewind to this point will be impossible.
                        let checkpoint = match storage.create_checkpoint(&conversation_id).await {
                            Ok(cp) => cp,
                            Err(e) => {
                                tracing::error!(
                                    "Failed to create checkpoint before tool execution: {}",
                                    e
                                );
                                CheckpointId(0)
                            }
                        };
                        // Caller depth = max depth of skills active in this conversation
                        // (0 if none). When the model invokes `activate_skill`, this is
                        // passed to `activate_by_name` so MAX_SKILL_ACTIVATION_DEPTH caps
                        // chains (Story 5-2 AC9).
                        let caller_depth = activation_set
                            .as_ref()
                            .map(|s| {
                                s.active_skills()
                                    .iter()
                                    .map(|a| a.activation_depth)
                                    .max()
                                    .unwrap_or(0)
                            })
                            .unwrap_or(0);
                        tools
                            .set_execution_context(
                                conversation_id.clone(),
                                checkpoint,
                                caller_depth,
                            )
                            .await;

                        let indexed: Vec<(usize, ToolCallInfo)> =
                            tool_calls.drain(..).enumerate().collect();
                        let (asks, regular): (Vec<_>, Vec<_>) = indexed
                            .into_iter()
                            .partition(|(_, tc)| tc.name == "AskUserQuestion");
                        let mut indexed_results: Vec<(usize, ToolResultMessage)> = Vec::new();

                        for (orig_idx, tc) in asks {
                            let question = tc
                                .input
                                .get("question")
                                .and_then(|v| v.as_str())
                                .unwrap_or("(no question text)")
                                .to_string();
                            let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
                            let _ = event_tx.send(AppEvent::AskUserQuestion {
                                conversation_id: conversation_id.clone(),
                                tool_use_id: tc.id.clone(),
                                question,
                                response_tx: resp_tx,
                            });
                            let answer = tokio::select! {
                                a = resp_rx => match a {
                                    Ok(a) => a,
                                    Err(_) => {
                                        let _ = event_tx.send(AppEvent::ProviderChunk {
                                            conversation_id: conversation_id.clone(),
                                            chunk: StreamChunk::TurnComplete {
                                                stop_reason: StopReason::Cancelled,
                                            },
                                        });
                                        return;
                                    }
                                },
                                _ = turn_cancel.cancelled() => {
                                    let _ = event_tx.send(AppEvent::ProviderChunk {
                                        conversation_id: conversation_id.clone(),
                                        chunk: StreamChunk::ToolResult {
                                            id: tc.id.clone(),
                                            content: "Tool execution cancelled".to_string(),
                                            is_error: true,
                                        },
                                    });
                                    let _ = event_tx.send(AppEvent::ProviderChunk {
                                        conversation_id: conversation_id.clone(),
                                        chunk: StreamChunk::TurnComplete {
                                            stop_reason: StopReason::Cancelled,
                                        },
                                    });
                                    return;
                                }
                            };
                            let result = crate::domain::models::ToolResult {
                                tool_use_id: tc.id.clone(),
                                content: answer.clone(),
                                is_error: false,
                            };
                            let _ = event_tx.send(AppEvent::ProviderChunk {
                                conversation_id: conversation_id.clone(),
                                chunk: StreamChunk::ToolResult {
                                    id: result.tool_use_id.clone(),
                                    content: result.content.clone(),
                                    is_error: result.is_error,
                                },
                            });
                            indexed_results.push((
                                orig_idx,
                                ToolResultMessage {
                                    tool_use_id: result.tool_use_id,
                                    content: result.content,
                                    is_error: result.is_error,
                                },
                            ));
                        }

                        if !regular.is_empty() {
                            let batch_with_idx: Vec<(
                                usize,
                                crate::domain::models::ToolCallRequest,
                            )> = regular
                                .iter()
                                .map(|(orig_idx, tc)| {
                                    (
                                        *orig_idx,
                                        crate::domain::models::ToolCallRequest {
                                            id: tc.id.clone(),
                                            tool_name: tc.name.clone(),
                                            input: tc.input.clone(),
                                        },
                                    )
                                })
                                .collect();
                            let source = crate::domain::models::ApprovalSource::ForegroundTurn {
                                conversation_id: conversation_id.clone(),
                            };
                            let active_skills = activation_set.as_ref().map(|s| s.active_skills());
                            let requests: Vec<crate::domain::models::ToolCallRequest> =
                                batch_with_idx.iter().map(|(_, req)| req.clone()).collect();
                            let terminal = tool_scheduler
                                .clone()
                                .schedule(source, requests, turn_cancel.clone(), active_skills)
                                .await;
                            for (i, call) in terminal.into_iter().enumerate() {
                                let (id, content, is_error, was_cancelled) = match call {
                                    ToolCall::Success { id, result, .. } => {
                                        (id, result.output, result.is_error, false)
                                    }
                                    ToolCall::Error { id, error, .. } => (id, error, true, false),
                                    ToolCall::Cancelled { id, reason, .. } => (
                                        id,
                                        format!("Tool execution cancelled: {}", reason),
                                        true,
                                        true,
                                    ),
                                    _ => (
                                        batch_with_idx[i].1.id.clone(),
                                        "Internal scheduler error: unexpected non-terminal state"
                                            .to_string(),
                                        true,
                                        false,
                                    ),
                                };
                                let _ = event_tx.send(AppEvent::ProviderChunk {
                                    conversation_id: conversation_id.clone(),
                                    chunk: StreamChunk::ToolResult {
                                        id: id.clone(),
                                        content: content.clone(),
                                        is_error,
                                    },
                                });
                                indexed_results.push((
                                    batch_with_idx[i].0,
                                    ToolResultMessage {
                                        tool_use_id: id,
                                        content,
                                        is_error,
                                    },
                                ));
                                if was_cancelled {
                                    let _ = event_tx.send(AppEvent::ProviderChunk {
                                        conversation_id: conversation_id.clone(),
                                        chunk: StreamChunk::TurnComplete {
                                            stop_reason: StopReason::Cancelled,
                                        },
                                    });
                                    return;
                                }
                            }
                        }

                        indexed_results.sort_by_key(|(idx, _)| *idx);
                        let tool_result_messages: Vec<ToolResultMessage> =
                            indexed_results.into_iter().map(|(_, msg)| msg).collect();

                        // Append tool results as a user message for the next completion
                        messages.push(Message {
                            role: MessageRole::User,
                            content: String::new(),
                            images: vec![],
                            tool_results: tool_result_messages,
                            tool_uses: vec![],
                            context_prefix: None,
                        });

                        // Loop back to stream_completion
                        continue;
                    }
                    StopReason::EndTurn | StopReason::MaxTokens | StopReason::Cancelled => {
                        // Turn is done
                        return;
                    }
                }
            }
            Err(e) => {
                // Write failure ledger entry before emitting notice
                let failure_entry = UsageLedgerEntry {
                    timestamp_ms: chrono::Utc::now().timestamp_millis(),
                    session_id: session_id.clone(),
                    conversation_id: conversation_id.clone(),
                    provider_id: provider.provider_id(),
                    model: options.model.clone(),
                    tier: resolved.tier,
                    step_kind,
                    escalation_reason: resolved.escalation_reason,
                    usage: TokenUsage {
                        tokens_in: 0,
                        tokens_out: 0,
                        parent_ctx: parent_ctx_tokens,
                    },
                };
                if let Err(le) = ledger.append(failure_entry).await {
                    tracing::warn!("Usage ledger append failed on error path: {}", le);
                }

                let _ = event_tx.send(AppEvent::SystemNotice {
                    conversation_id: Some(conversation_id.clone()),
                    level: NoticeLevel::Error,
                    message: format!("{e}"),
                });
                return;
            }
        }
    }
}

/// Get current unix timestamp in milliseconds.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
