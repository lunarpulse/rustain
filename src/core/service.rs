use anyhow::Result;
use tokio::sync::mpsc;

use crate::types::event::{AppEvent, ApprovalDecision, PermissionRequest};
use crate::types::stream::TuiStreamEvent;

use super::provider::{CompletionOptions, Message, StreamingProvider, ToolCallResult};

/// The streaming service orchestrates the conversation loop:
/// send messages → stream response → execute tools → repeat until done.
///
/// This replaces rustycode's ProcessMessageUseCase with a streaming-first design.
/// The service is STATELESS — it takes a message snapshot in, emits events out.
/// All conversation mutation happens in AppState via events (unidirectional data flow).
pub struct StreamingService {
    provider: Box<dyn StreamingProvider>,
    event_tx: mpsc::UnboundedSender<AppEvent>,
    // TODO: tool_registry for executing tools (reuse rustycode's tool implementations)
}

impl StreamingService {
    pub fn new(
        provider: Box<dyn StreamingProvider>,
        event_tx: mpsc::UnboundedSender<AppEvent>,
    ) -> Self {
        Self {
            provider,
            event_tx,
        }
    }

    /// Send a message and stream the response.
    ///
    /// Runs in a background tokio task. Takes a SNAPSHOT of messages (not &mut Conversation).
    /// All state mutations flow back through AppEvent::Stream events to AppState.
    ///
    /// Flow: stream → tool_use stop → check permission → execute tool → append results → stream again
    pub async fn send_message(
        &self,
        mut api_messages: Vec<Message>,
        options: CompletionOptions,
    ) -> Result<()> {
        // Create stream event sender (wraps into AppEvent::Stream)
        let (stream_tx, mut stream_rx) = mpsc::unbounded_channel::<TuiStreamEvent>();

        // Forward stream events to the app event channel
        let event_tx = self.event_tx.clone();
        let forwarder = tokio::spawn(async move {
            while let Some(event) = stream_rx.recv().await {
                if event_tx.send(AppEvent::Stream(event)).is_err() {
                    break;
                }
            }
        });

        // Tool execution loop
        loop {
            let result = self
                .provider
                .stream_completion(&api_messages, &options, &stream_tx)
                .await?;

            if result.stop_reason == "tool_use" && !result.tool_calls.is_empty() {
                for tool_call in &result.tool_calls {
                    let approved = self.check_permission(tool_call).await?;

                    match approved {
                        ApprovalDecision::Allow | ApprovalDecision::AlwaysAllow => {
                            let tool_result = self.execute_tool(tool_call).await;

                            let (result_content, is_error) = match tool_result {
                                Ok(output) => (output, false),
                                Err(e) => (e.to_string(), true),
                            };

                            stream_tx.send(TuiStreamEvent::ToolResult {
                                id: tool_call.id.clone(),
                                content: result_content.clone(),
                                is_error,
                            })?;

                            // Append to messages for next API request
                            api_messages.push(Message {
                                role: "assistant".to_string(),
                                content: serde_json::json!([{
                                    "type": "tool_use",
                                    "id": tool_call.id,
                                    "name": tool_call.name,
                                    "input": tool_call.input,
                                }]),
                            });
                            api_messages.push(Message {
                                role: "user".to_string(),
                                content: serde_json::json!([{
                                    "type": "tool_result",
                                    "tool_use_id": tool_call.id,
                                    "content": result_content,
                                    "is_error": is_error,
                                }]),
                            });
                        }
                        ApprovalDecision::Deny => {
                            stream_tx.send(TuiStreamEvent::Blocked {
                                content: format!("Tool {} denied by user", tool_call.name),
                            })?;
                            break;
                        }
                        ApprovalDecision::Cancel => {
                            break;
                        }
                    }
                }
                continue;
            }

            // end_turn — done
            break;
        }

        stream_tx.send(TuiStreamEvent::Done)?;
        drop(stream_tx);
        forwarder.await?;

        Ok(())
    }

    /// Check permission for a tool call.
    /// Sends a PermissionRequest via AppEvent and blocks until the UI responds.
    async fn check_permission(&self, _tool_call: &ToolCallResult) -> Result<ApprovalDecision> {
        // TODO: Check permission mode (YOLO → auto-approve)
        // Full implementation:
        //
        // let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        // self.event_tx.send(AppEvent::Permission(PermissionRequest {
        //     tool_name: tool_call.name.clone(),
        //     tool_input: serde_json::to_string_pretty(&tool_call.input)?,
        //     tool_id: tool_call.id.clone(),
        //     response_tx,
        // }))?;
        // Ok(response_rx.await?)

        Ok(ApprovalDecision::Allow)
    }

    /// Execute a tool using rustycode's tool implementations.
    async fn execute_tool(&self, tool_call: &ToolCallResult) -> Result<String> {
        // TODO: Wire up rustycode's ToolRegistry
        // let result = self.tool_registry.execute(&tool_call.name, &tool_call.input).await?;
        Ok(format!("[Tool {} execution not yet wired]", tool_call.name))
    }
}
