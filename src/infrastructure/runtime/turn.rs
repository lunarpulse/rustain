//! Turn orchestrator with tool execution loop.
//! Sprint 1 pattern: stream → collect tool calls → execute → loop.

use std::sync::Arc;

use futures::StreamExt;
use tokio::sync::mpsc;

use crate::domain::events::AppEvent;
use crate::domain::models::checkpoint::CheckpointId;
use crate::domain::models::{
    CompletionOptions, Message, MessageRole, NoticeLevel, StopReason, StreamChunk, ToolCallInfo,
    ToolResultMessage, ToolUseMessage,
};
use crate::domain::ports::{ProviderPort, SecurityPort, StoragePort, ToolSetPort};
use crate::domain::services::permission_chain::{self, PermissionDecision};

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
    provider: Arc<dyn ProviderPort>,
    mut messages: Vec<Message>,
    options: CompletionOptions,
    event_tx: mpsc::UnboundedSender<AppEvent>,
    security: Arc<dyn SecurityPort>,
    tools: Arc<dyn ToolSetPort>,
    conversation_id: String,
    storage: Arc<dyn StoragePort>,
) {
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
                            });
                        }
                        StreamChunk::Text { content, .. } => {
                            accumulated_text.push_str(content);
                        }
                        _ => {}
                    }
                    let _ = event_tx.send(AppEvent::ProviderChunk {
                        conversation_id: conversation_id.clone(),
                        chunk,
                    });
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
                        tools
                            .set_execution_context(conversation_id.clone(), checkpoint)
                            .await;

                        let mut tool_result_messages = Vec::new();

                        for tc in &tool_calls {
                            // Special handling for AskUserQuestion tool — it does not write files,
                            // so it is excluded from checkpoint dispatch (AC2, Story 4-3b note).
                            if tc.name == "AskUserQuestion" {
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
                                // Wait for user's answer
                                let answer = match resp_rx.await {
                                    Ok(a) => a,
                                    Err(_) => {
                                        // Channel dropped — user cancelled
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
                                tool_result_messages.push(ToolResultMessage {
                                    tool_use_id: result.tool_use_id,
                                    content: result.content,
                                    is_error: result.is_error,
                                });
                                continue;
                            }

                            let decision =
                                permission_chain::check(security.as_ref(), &tc.name, &tc.input)
                                    .await;

                            let result = match decision {
                                PermissionDecision::Allow | PermissionDecision::AlwaysAllow => {
                                    match tools.execute(&tc.name, tc.input.clone()).await {
                                        Ok(mut result) => {
                                            result.tool_use_id = tc.id.clone();
                                            result
                                        }
                                        Err(e) => crate::domain::models::ToolResult {
                                            tool_use_id: tc.id.clone(),
                                            content: format!("Tool execution failed: {}", e),
                                            is_error: true,
                                        },
                                    }
                                }
                                PermissionDecision::Deny(reason) => {
                                    crate::domain::models::ToolResult {
                                        tool_use_id: tc.id.clone(),
                                        content: format!("Permission denied: {}", reason),
                                        is_error: true,
                                    }
                                }
                                PermissionDecision::Cancel => {
                                    // User cancelled — stop the turn
                                    let _ = event_tx.send(AppEvent::ProviderChunk {
                                        conversation_id: conversation_id.clone(),
                                        chunk: StreamChunk::TurnComplete {
                                            stop_reason: StopReason::Cancelled,
                                        },
                                    });
                                    return;
                                }
                            };

                            // Send ToolResult chunk so apply_chunk processes it
                            let _ = event_tx.send(AppEvent::ProviderChunk {
                                conversation_id: conversation_id.clone(),
                                chunk: StreamChunk::ToolResult {
                                    id: result.tool_use_id.clone(),
                                    content: result.content.clone(),
                                    is_error: result.is_error,
                                },
                            });

                            tool_result_messages.push(ToolResultMessage {
                                tool_use_id: result.tool_use_id,
                                content: result.content,
                                is_error: result.is_error,
                            });
                        }

                        // Append tool results as a user message for the next completion
                        messages.push(Message {
                            role: MessageRole::User,
                            content: String::new(),
                            images: vec![],
                            tool_results: tool_result_messages,
                            tool_uses: vec![],
                            context_prefix: None,
                        });

                        // Clear tool_calls for next iteration
                        tool_calls.clear();

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
