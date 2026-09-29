//! Turn orchestrator with tool execution loop.
//! Sprint 1 pattern: stream → collect tool calls → execute → loop.

use std::sync::Arc;

use futures::StreamExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::domain::events::AppEvent;
use crate::domain::models::checkpoint::CheckpointId;
use crate::domain::models::{
    CompletionOptions, EscalationReason, Message, MessageRole, NotCapturedReason, NoticeLevel,
    ProvenanceTag, StepKind, StopReason, StreamChunk, TokenUsage, ToolCall, ToolCallInfo,
    ToolResultMessage, ToolUseMessage, TurnOrigin, UsageLedgerEntry, WriteDiffState,
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
const MAX_TOOL_ITERATIONS: usize = 256;

/// Story 19.1 A3 — the ONE infrastructure site that populates a completed
/// Write's display diff. Reads the pre-write snapshot back through
/// [`StoragePort::read_snapshot`] (the adapter snapshotted the original in
/// `execute_write`) and diffs it against the new content from the tool
/// *input*; the provider-facing `ToolResult.content` is never touched (A5 —
/// the model's bytes are identical).
///
/// Code review (Decision 1, Crew unanimous 4/4) kept this read-back rather
/// than computing in the adapter: `execute_write` returns `ToolResult`, which
/// is pinned to three fields, so it has no legitimate carrier for a UI-only
/// value. The repairs the review did require are all here:
///
/// - `path` is the ADAPTER-RESOLVED absolute path, not the raw model string.
///   The adapter snapshots `workspace_path.join(file_path)`, so re-deriving
///   from the raw path against the process CWD silently missed whenever the
///   two roots differ (correct before only because `startup.rs` happens to
///   set `workspace_path = current_dir()`).
/// - Every failure returns its OWN reason. Previously a snapshot read error
///   rendered "no active checkpoint", which was simply false.
/// - `already_written` names paths an earlier Write in this same batch
///   already snapshotted. Snapshots are first-write-wins per
///   `(checkpoint, path)`, so a second write's read-back would return the
///   pre-FIRST-write content and misattribute both writes to this one call.
/// - The diff is elided and capped HERE, so the journal and the persisted
///   conversation never carry an unbounded diff.
pub async fn write_display_diff(
    storage: &dyn StoragePort,
    conversation_id: &str,
    checkpoint: CheckpointId,
    request: &crate::domain::models::ToolCallRequest,
    workspace_path: Option<&std::path::Path>,
    already_written: &std::collections::HashSet<std::path::PathBuf>,
) -> WriteDiffState {
    if !matches!(request.tool_name.as_str(), "Write" | "write") {
        return WriteDiffState::NotAWrite;
    }
    let (Some(path), Some(new_content)) = (
        request.input.get("file_path").and_then(|v| v.as_str()),
        request.input.get("content").and_then(|v| v.as_str()),
    ) else {
        // A Write whose input we cannot read is not a Write we can render;
        // fall back to the result text rather than invent a diff.
        return WriteDiffState::NotAWrite;
    };
    let Some(resolved) = resolve_write_path(workspace_path, path) else {
        // Relative path and no known workspace root: the key cannot be
        // reproduced, and guessing against the CWD is the bug we just fixed.
        return WriteDiffState::NotCaptured {
            reason: NotCapturedReason::SnapshotUnavailable,
        };
    };
    if already_written.contains(&resolved) {
        return WriteDiffState::NotCaptured {
            reason: NotCapturedReason::SupersededInBatch,
        };
    }
    // CheckpointId(0) is the sentinel `run_turn` falls through with when
    // checkpoint creation failed — no snapshot can exist under it.
    if checkpoint.0 == 0 {
        return WriteDiffState::NotCaptured {
            reason: NotCapturedReason::NoActiveCheckpoint,
        };
    }
    match storage
        .read_snapshot(conversation_id, checkpoint, &resolved)
        .await
    {
        Ok(Some(original)) => WriteDiffState::from_original(&original, new_content),
        Ok(None) => WriteDiffState::NotCaptured {
            reason: NotCapturedReason::SnapshotUnavailable,
        },
        Err(e) => {
            tracing::warn!("read_snapshot failed for {}: {}", resolved.display(), e);
            WriteDiffState::NotCaptured {
                reason: NotCapturedReason::SnapshotReadFailed,
            }
        }
    }
}

/// Resolve a Write's `file_path` exactly as `execute_write` does, so the
/// snapshot key derived on the write path and the one derived on the read
/// path cannot disagree. `None` when the path is relative and no workspace
/// root is available — the caller must then decline rather than guess.
pub fn resolve_write_path(
    workspace_path: Option<&std::path::Path>,
    file_path: &str,
) -> Option<std::path::PathBuf> {
    let p = std::path::Path::new(file_path);
    if p.is_absolute() {
        Some(p.to_path_buf())
    } else {
        workspace_path.map(|ws| ws.join(p))
    }
}

#[cfg(any(test, feature = "test-instrumentation"))]
pub static RUN_TURN_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

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
    mut activation_set: Option<crate::domain::models::SkillActivationSet>,
    agent_restriction: Option<crate::domain::models::AgentToolRestriction>,
    turn_cancel: CancellationToken,
    ledger: Arc<dyn UsageLedgerPort>,
    resolved: ResolvedModel,
    step_kind: Option<StepKind>,
    parent_ctx_tokens: u32,
    parent_trace: Option<crate::domain::models::TraceContext>,
    session_id: String,
    turn_origin: TurnOrigin,
    // Whether this turn's assembled context already contains peer-origin
    // material (Story 18.4a, FR151).
    //
    // ⚑ THE BRIDGE THAT WAS MISSING. `TurnOrigin::provenance()` returns
    // `SelfOriginated` only for `RemotePeer`, and the sole mid-turn escalation
    // was a completed tool call whose name began `"a2a__"` — so a locally
    // initiated turn that assembled peer-sourced context dispatched destructive
    // tools as `UserOriginated` and the taint gate never fired. The bundle's
    // provenance and this turn's taint bit were not connected by anything.
    //
    // ⛔ Derived by the caller from `ContextBundle::has_peer_origin()`, which
    // recomputes from each entry's `ContextSource`. There is no field a peer
    // can set to clear it (17.1b's Vex rule).
    context_tainted: bool,
    // Live activation state is needed only when a model activates a skill
    // during this turn. The initial snapshot still owns prompt composition;
    // this handle refreshes scheduler enforcement between tool calls.
    skill_activator: Option<Arc<crate::adapters::skill_activation::SkillActivator>>,
) {
    #[cfg(any(test, feature = "test-instrumentation"))]
    RUN_TURN_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
    // Once remote content enters this turn's context, every later destructive
    // dispatch is self-originated even when the turn itself began interactively.
    //
    // ⚑ Two sources, ⛔ never one (Story 18.4a): the turn's own route, and the
    // assembled context it was handed. Before this story the comment above
    // claimed the broader property while only the route half was wired, so a
    // locally initiated turn carrying peer context dispatched as
    // `UserOriginated` and FR151's *"peer context is read, never a silent
    // driver of a destructive action"* was false.
    let mut context_tainted =
        context_tainted || turn_origin.provenance() == ProvenanceTag::SelfOriginated;
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
                let mut accumulated_thinking = String::new();

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
                        StreamChunk::Thinking { content, .. } => {
                            accumulated_thinking.push_str(content);
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
                            // Story 7.5 AC8 — cache/reasoning attribution.
                            // DF-S71c-1 carries forward: only the LAST Usage chunk per
                            // iteration is retained; Anthropic emits Usage exactly once
                            // per call so this is correct for the current adapter set.
                            cache_creation_tokens: u.cache_creation_input_tokens,
                            cache_read_tokens: u.cache_read_input_tokens,
                            reasoning_tokens: u.reasoning_tokens,
                        },
                        None => TokenUsage {
                            tokens_in: 0,
                            tokens_out: 0,
                            parent_ctx: parent_ctx_tokens,
                            cache_creation_tokens: None,
                            cache_read_tokens: None,
                            reasoning_tokens: None,
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
                            reasoning_content: if accumulated_thinking.is_empty() {
                                None
                            } else {
                                Some(std::mem::take(&mut accumulated_thinking))
                            },
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
                        tools
                            .set_parent_context(
                                parent_ctx_tokens,
                                parent_trace.clone(),
                                agent_restriction.clone(),
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
                                            diff: crate::domain::models::WriteDiffState::NotAWrite,
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
                                    // AskUserQuestion answer — not a file write.
                                    diff: crate::domain::models::WriteDiffState::NotAWrite,
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
                            let source = turn_origin.approval_source(&conversation_id);
                            let requests: Vec<crate::domain::models::ToolCallRequest> =
                                batch_with_idx.iter().map(|(_, req)| req.clone()).collect();
                            let provenance = if context_tainted {
                                ProvenanceTag::SelfOriginated
                            } else {
                                ProvenanceTag::UserOriginated
                            };
                            // Activation changes policy immediately. A provider may emit
                            // `activate_skill` beside another call in one response, so run
                            // that batch in wire order and refresh the enforcement snapshot
                            // after each successful activation. Ordinary batches retain the
                            // scheduler's parallel-safe fast path.
                            let terminal = if requests
                                .iter()
                                .any(|request| request.tool_name == "activate_skill")
                            {
                                let mut terminal = Vec::with_capacity(requests.len());
                                for request in requests {
                                    let mut one = {
                                        let active_skills =
                                            activation_set.as_ref().map(|s| s.active_skills());
                                        tool_scheduler
                                            .clone()
                                            .schedule_with_provenance_and_restriction(
                                                source.clone(),
                                                vec![request],
                                                turn_cancel.clone(),
                                                active_skills,
                                                agent_restriction.as_ref(),
                                                provenance,
                                            )
                                            .await
                                    };
                                    let activated = one.iter().any(|call| {
                                        matches!(
                                            call,
                                            ToolCall::Success {
                                                request,
                                                result,
                                                ..
                                            } if request.tool_name == "activate_skill"
                                                && !result.is_error
                                        )
                                    });
                                    terminal.append(&mut one);
                                    if activated {
                                        if let Some(activator) = &skill_activator {
                                            activation_set =
                                                activator.snapshot_for_turn(&conversation_id).await;
                                        }
                                    }
                                }
                                terminal
                            } else {
                                let active_skills =
                                    activation_set.as_ref().map(|s| s.active_skills());
                                tool_scheduler
                                    .clone()
                                    .schedule_with_provenance_and_restriction(
                                        source,
                                        requests,
                                        turn_cancel.clone(),
                                        active_skills,
                                        agent_restriction.as_ref(),
                                        provenance,
                                    )
                                    .await
                            };
                            if terminal.iter().any(|call| {
                                matches!(
                                    call,
                                    ToolCall::Success {
                                        request,
                                        result,
                                        ..
                                    } if request.tool_name.starts_with("a2a__") && !result.is_error
                                )
                            }) {
                                context_tainted = true;
                            }
                            // Paths a Write in THIS batch has already
                            // snapshotted. Snapshots are first-write-wins per
                            // (checkpoint, path), so a later write to the same
                            // path must not read the earlier write's original
                            // and present both changes as its own.
                            let mut written_paths: std::collections::HashSet<std::path::PathBuf> =
                                std::collections::HashSet::new();
                            let workspace_root = tools.workspace_root();
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
                                // Story 19.1 A3: display diff for completed,
                                // non-error Writes (snapshot read-back).
                                // Provider-facing bytes are untouched (A5).
                                let diff = if is_error {
                                    WriteDiffState::NotAWrite
                                } else {
                                    write_display_diff(
                                        storage.as_ref(),
                                        &conversation_id,
                                        checkpoint,
                                        &batch_with_idx[i].1,
                                        workspace_root.as_deref(),
                                        &written_paths,
                                    )
                                    .await
                                };
                                if matches!(
                                    batch_with_idx[i].1.tool_name.as_str(),
                                    "Write" | "write"
                                ) {
                                    if let Some(p) = batch_with_idx[i]
                                        .1
                                        .input
                                        .get("file_path")
                                        .and_then(|v| v.as_str())
                                        .and_then(|p| {
                                            resolve_write_path(workspace_root.as_deref(), p)
                                        })
                                    {
                                        written_paths.insert(p);
                                    }
                                }
                                let _ = event_tx.send(AppEvent::ProviderChunk {
                                    conversation_id: conversation_id.clone(),
                                    chunk: StreamChunk::ToolResult {
                                        id: id.clone(),
                                        content: content.clone(),
                                        is_error,
                                        diff,
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
                            reasoning_content: None,
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
                        // Failed call — no usage data to attribute (Story 7.5 AC8).
                        cache_creation_tokens: None,
                        cache_read_tokens: None,
                        reasoning_tokens: None,
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
