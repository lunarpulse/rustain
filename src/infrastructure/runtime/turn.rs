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
    ToolResultMessage, ToolUseMessage, TurnOrigin, UsageInfo, UsageLedgerEntry, UsageProvenance,
    WriteDiffState,
};
use crate::domain::ports::{
    SecurityPort, StoragePort, StreamingProvider, ToolSetPort, UsageLedgerPort,
};
use crate::domain::services::model_router::ResolvedModel;
use crate::domain::services::tool_scheduler::{ToolExecutionBound, ToolScheduler};

use super::ask_limits::{AskExecution, deadline_elapsed, interruptible};

/// Execute a turn: stream completion, execute tools, loop until EndTurn.
///
/// The agentic loop:
/// 1. Call stream_completion with current messages
/// 2. Forward all chunks as AppEvent::ProviderChunk
/// 3. On TurnComplete(ToolUse): execute tools, append results, loop to 1
/// 4. On TurnComplete(EndTurn): done
/// Maximum number of tool execution loop iterations before forcing termination.
const MAX_TOOL_ITERATIONS: usize = 256;

fn checked_add_attempt_usage(a: &UsageInfo, b: &UsageInfo) -> Option<UsageInfo> {
    fn add_optional(a: Option<u32>, b: Option<u32>) -> Option<Option<u32>> {
        match (a, b) {
            (Some(a), Some(b)) => a.checked_add(b).map(Some),
            _ => Some(a.or(b)),
        }
    }

    Some(UsageInfo {
        input_tokens: a.input_tokens.checked_add(b.input_tokens)?,
        output_tokens: a.output_tokens.checked_add(b.output_tokens)?,
        cache_creation_input_tokens: add_optional(
            a.cache_creation_input_tokens,
            b.cache_creation_input_tokens,
        )?,
        cache_read_input_tokens: add_optional(
            a.cache_read_input_tokens,
            b.cache_read_input_tokens,
        )?,
        reasoning_tokens: add_optional(a.reasoning_tokens, b.reasoning_tokens)?,
        counters_observed: true,
    })
}

fn omitted_retry_spend(usage: &UsageInfo) -> bool {
    usage.input_tokens != 0
        || usage.output_tokens != 0
        || usage.cache_creation_input_tokens.unwrap_or(0) != 0
        || usage.cache_read_input_tokens.unwrap_or(0) != 0
        || usage.reasoning_tokens.unwrap_or(0) != 0
}

enum AttemptProof {
    Unproved,
    Display,
    Snapshot(UsageInfo),
}

fn same_usage_amount(a: &UsageInfo, b: &UsageInfo) -> bool {
    a.input_tokens == b.input_tokens
        && a.output_tokens == b.output_tokens
        && a.cache_creation_input_tokens == b.cache_creation_input_tokens
        && a.cache_read_input_tokens == b.cache_read_input_tokens
        && a.reasoning_tokens == b.reasoning_tokens
}

#[cfg(feature = "session-visit")]
fn strip_provider_task_frame(tool_name: &str, content: &mut String) {
    if tool_name == "task"
        && let Some((_, original)) = crate::domain::models::worker_report::split(content)
    {
        let header_len = content.len() - original.len();
        drop(content.drain(..header_len));
    }
}

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

/// A checkpoint failure that means the captured history is stale or its store is not in a
/// state to rest tool effects on (as opposed to an ordinary I/O failure).
fn checkpoint_refused(error: &crate::domain::errors::StorageError) -> bool {
    use crate::domain::errors::StorageError;
    matches!(
        error,
        StorageError::Stale(_) | StorageError::Indeterminate(_) | StorageError::PartialCommit(_)
    )
}

pub async fn run_turn(
    provider: Arc<dyn StreamingProvider>,
    mut messages: Vec<Message>,
    mut options: CompletionOptions,
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
    mut ask_execution: Option<AskExecution>,
) {
    if options.session_id.as_deref() != Some(conversation_id.as_str()) {
        options.session_id = Some(conversation_id.clone());
    }
    #[cfg(any(test, feature = "test-instrumentation"))]
    RUN_TURN_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // Persist the conversation before the first API call so that
    // `create_checkpoint` (called when the API returns tool_use) can load it
    // from storage. Without this, the first tool call in a new conversation
    // fails checkpoint creation — snapshots get the sentinel CheckpointId(0)
    // and are never revertible.
    let pre_turn_save = storage.save_conversation(&conversation_snapshot);
    let saved = if let Some(execution) = &mut ask_execution {
        if execution.stopped_before_work() {
            return;
        }
        match interruptible(execution.deadline(), Some(&turn_cancel), pre_turn_save).await {
            Ok(result) => result,
            Err(_) => {
                execution.stopped_before_work();
                return;
            }
        }
    } else {
        pre_turn_save.await
    };
    if let Err(e) = saved {
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
        // Story 21.2 A4 (integrator-authorized): a cancelled turn must not start
        // another provider call. A tool that returns an error on cancellation
        // otherwise lets the loop continue and finish a turn reported Interrupted.
        // Like every other early exit, it closes the turn with a Cancelled
        // terminal (no await before `return`, so an abort cannot double it).
        if ask_execution.is_none() && iteration > 0 && turn_cancel.is_cancelled() {
            let _ = event_tx.send(AppEvent::ProviderChunk {
                conversation_id: conversation_id.clone(),
                chunk: StreamChunk::TurnComplete {
                    stop_reason: StopReason::Cancelled,
                },
            });
            return;
        }
        iteration += 1;
        if iteration > MAX_TOOL_ITERATIONS {
            if let Some(execution) = &mut ask_execution {
                if execution.stopped_before_work() || turn_cancel.is_cancelled() {
                    return;
                }
            }
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
        if let Some(execution) = &mut ask_execution {
            if !execution.may_admit(ledger.as_ref(), &turn_cancel).await {
                return;
            }
        }
        let limited = ask_execution.is_some();
        let deadline = ask_execution.as_ref().and_then(AskExecution::deadline);
        let mut admitted = false;
        let mut received_turn_complete = false;
        let mut forwarded_error = false;
        let mut stop_reason = StopReason::EndTurn;
        let mut tool_calls: Vec<ToolCallInfo> = Vec::new();
        let mut accumulated_text = String::new();
        let mut accumulated_thinking = String::new();
        // Story 20.1d1b: provider replay items ride the in-turn assistant
        // message; the stream emits them only after a terminal event.
        let mut accumulated_replay = Vec::new();
        let mut display_usage: Option<UsageInfo> = None;
        let mut attempt_proof = AttemptProof::Unproved;
        // For limited calls, hold the exact sum of abandoned attempts. For
        // other callers the old amount omits retries; only known zero fits.
        // Err means incomplete, unrepresentable, or omitted spend.
        let mut prior_attempt_usage: Result<Option<UsageInfo>, ()> = Ok(None);
        let call_messages = messages.clone();
        let call_options = options.clone();
        let provider_call = async {
            if let Some(execution) = &mut ask_execution {
                execution.mark_admitted();
            }
            admitted = true;
            let stream = provider
                .stream_completion(call_messages, call_options)
                .await?;
            futures::pin_mut!(stream);
            loop {
                // A continuously ready stream need not yield to the outer
                // select; keep checking at the per-chunk boundary, too.
                if limited && (deadline_elapsed(deadline) || turn_cancel.is_cancelled()) {
                    return Ok(false);
                }
                let Some(chunk) = stream.next().await else {
                    return Ok::<_, crate::domain::errors::ProviderError>(true);
                };
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
                            provider_id: None,
                        });
                    }
                    StreamChunk::Text { content, .. } => {
                        accumulated_text.push_str(content);
                    }
                    StreamChunk::Thinking { content, .. } => {
                        accumulated_thinking.push_str(content);
                    }
                    StreamChunk::ProviderReplay { item } => {
                        accumulated_replay.push(item.clone());
                    }
                    StreamChunk::Usage { usage, .. } => {
                        if usage.counters_observed {
                            attempt_proof = AttemptProof::Display;
                        } else if matches!(attempt_proof, AttemptProof::Display)
                            && let Some(previous) = display_usage.take()
                        {
                            attempt_proof = AttemptProof::Snapshot(previous);
                        }
                        display_usage = Some(usage.clone());
                    }
                    StreamChunk::AccountingUsage { usage } => {
                        if usage.counters_observed {
                            attempt_proof = AttemptProof::Snapshot(usage.clone());
                        }
                        continue; // Internal transport accounting is never an AppEvent.
                    }
                    StreamChunk::AccountingRetry { usage } => {
                        attempt_proof = AttemptProof::Unproved;
                        if limited {
                            display_usage = None;
                        }
                        let total = std::mem::replace(&mut prior_attempt_usage, Ok(None));
                        prior_attempt_usage = total.and_then(|total| {
                            let previous =
                                usage.as_ref().filter(|u| u.counters_observed).ok_or(())?;
                            if limited {
                                match total {
                                    Some(total) => checked_add_attempt_usage(&total, previous)
                                        .map(Some)
                                        .ok_or(()),
                                    None => Ok(Some(previous.clone())),
                                }
                            } else if omitted_retry_spend(previous) {
                                Err(())
                            } else {
                                Ok(None)
                            }
                        });
                        continue;
                    }
                    StreamChunk::Error { .. } => forwarded_error = true,
                    _ => {}
                }
                let _ = event_tx.send(AppEvent::ProviderChunk {
                    conversation_id: conversation_id.clone(),
                    chunk,
                });
            }
        };
        let call = if limited {
            interruptible(deadline, Some(&turn_cancel), provider_call).await
        } else {
            Ok(provider_call.await)
        };
        if !admitted {
            if let Some(execution) = &mut ask_execution {
                execution.stopped_before_work();
            }
            return;
        }

        let stream_closed = matches!(&call, Ok(Ok(true)));
        let open_failed = matches!(&call, Ok(Err(_)));
        let interrupted = !open_failed
            && (!stream_closed
                || !received_turn_complete
                || forwarded_error
                || stop_reason == StopReason::Cancelled);
        let amount = |usage: &UsageInfo| TokenUsage::from_usage(usage, parent_ctx_tokens);
        let (usage, observed) = if limited {
            let current = match &attempt_proof {
                AttemptProof::Unproved => None,
                AttemptProof::Display => display_usage.as_ref(),
                AttemptProof::Snapshot(usage) => Some(usage),
            };
            // Only the retry marker settles the previous attempt. The latest
            // proved current snapshot contributes once, even if also displayed.
            let proved = current.and_then(|current| match prior_attempt_usage {
                Ok(Some(prior)) => checked_add_attempt_usage(&prior, current)
                    .as_ref()
                    .map(amount),
                Ok(None) => Some(amount(current)),
                Err(()) => None,
            });
            let observed = proved.is_some();
            // A completed call can keep its unproved display amount for
            // diagnostics. Strict pricing never uses an unknown row.
            let usage = proved.or_else(|| {
                if interrupted {
                    None
                } else {
                    display_usage.as_ref().map(amount)
                }
            });
            (usage, observed)
        } else {
            // Keep the old amount. An internal snapshot can prove it but
            // cannot substitute a different amount on this path.
            let observed = prior_attempt_usage.is_ok()
                && match &attempt_proof {
                    AttemptProof::Unproved => false,
                    AttemptProof::Display => true,
                    AttemptProof::Snapshot(proof) => display_usage
                        .as_ref()
                        .is_some_and(|display| same_usage_amount(display, proof)),
                };
            (display_usage.as_ref().map(amount), observed)
        };
        let mut accounting = Ok(());
        if !interrupted
            || !limited
            || usage.is_some()
            || ask_execution
                .as_ref()
                .is_some_and(AskExecution::money_configured)
        {
            let ledger_entry = UsageLedgerEntry {
                timestamp_ms: chrono::Utc::now().timestamp_millis(),
                session_id: session_id.clone(),
                conversation_id: conversation_id.clone(),
                provider_id: provider.provider_id(),
                model: options.model.clone(),
                tier: resolved.tier,
                step_kind,
                escalation_reason: resolved.escalation_reason,
                usage: usage.unwrap_or(TokenUsage {
                    tokens_in: 0,
                    tokens_out: 0,
                    parent_ctx: parent_ctx_tokens,
                    cache_creation_tokens: None,
                    cache_read_tokens: None,
                    reasoning_tokens: None,
                }),
                provenance: Some(if observed {
                    UsageProvenance::Observed
                } else {
                    UsageProvenance::Unknown
                }),
            };
            if let Some(execution) = &mut ask_execution {
                accounting = execution.price_settled_call(&ledger_entry);
            }
            // Don't race the append against cancellation: once started, its
            // outcome must be known, and nothing else may be admitted first.
            if let Err(e) = ledger.append(ledger_entry).await {
                if open_failed {
                    tracing::warn!("Usage ledger append failed on error path: {e}");
                } else {
                    tracing::warn!("Usage ledger append failed: {e}");
                }
                accounting = Err("could not append the provider call's usage receipt");
            }
        }
        if let Some(execution) = &mut ask_execution {
            let continuation = stream_closed
                && received_turn_complete
                && stop_reason == StopReason::ToolUse
                && !tool_calls.is_empty()
                && !forwarded_error
                && !turn_cancel.is_cancelled();
            if execution
                .stopped_after_settlement(ledger.as_ref(), accounting, continuation)
                .await
                || turn_cancel.is_cancelled()
                || forwarded_error
            {
                return;
            }
        }

        match call {
            Ok(Ok(true)) => {
                // Safety: synthesize TurnComplete if stream ended without one. A
                // provider error already forwarded says what happened; the
                // generic disconnect text would only repeat it.
                if !received_turn_complete {
                    tracing::warn!("Provider stream ended without TurnComplete — synthesizing end");
                    if !forwarded_error {
                        let event = if matches!(&turn_origin, TurnOrigin::Cron) {
                            AppEvent::CronStreamDisconnected
                        } else {
                            AppEvent::ProviderChunk {
                                conversation_id: conversation_id.clone(),
                                chunk: StreamChunk::Error {
                                    content: "Stream disconnected unexpectedly".to_string(),
                                },
                            }
                        };
                        let _ = event_tx.send(event);
                    }
                    let _ = event_tx.send(AppEvent::ProviderChunk {
                        conversation_id: conversation_id.clone(),
                        chunk: StreamChunk::TurnComplete {
                            stop_reason: StopReason::Cancelled,
                        },
                    });
                    return;
                }

                if stop_reason == StopReason::ToolUse {
                    let tool_bound = limited.then_some(ToolExecutionBound { deadline });
                    let scoped_cancel = limited.then(|| turn_cancel.child_token());
                    let tool_cancel = scoped_cancel.as_ref().unwrap_or(&turn_cancel);
                    let tool_interrupted =
                        || limited && (deadline_elapsed(deadline) || tool_cancel.is_cancelled());
                    let mut scheduled_tools = false;
                    let tool_work = async {
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
                            return false;
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
                            provider_replay: std::mem::take(&mut accumulated_replay),
                        });

                        // Create a checkpoint BEFORE executing any tools in this turn (AC2, Story 4-3b).
                        // The checkpoint captures the conversation state just before the assistant's
                        // tool-executing turn. If creation fails, we fall through with a sentinel
                        // CheckpointId(0) — tools run but rewind to this point will be impossible.
                        let checkpoint = match storage
                            .create_checkpoint_for(&conversation_snapshot)
                            .await
                        {
                            Ok(cp) => cp,
                            Err(e) if checkpoint_refused(&e) => {
                                // The history this turn started from is no longer the accepted
                                // one: no checkpoint, snapshot or tool effect may rest on it.
                                tracing::error!("Checkpoint refused before tool execution: {}", e);
                                for chunk in [
                                    StreamChunk::Error {
                                        content: "This conversation changed elsewhere while the turn was starting, so its tools were not run. Try again.".to_string(),
                                    },
                                    StreamChunk::TurnComplete {
                                        stop_reason: StopReason::Cancelled,
                                    },
                                ] {
                                    let event = AppEvent::ProviderChunk {
                                        conversation_id: conversation_id.clone(),
                                        chunk,
                                    };
                                    let _ = event_tx.send(event);
                                }
                                return false;
                            }
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
                        // Story Q-6 AC3: the parent context travels with each call (the
                        // scheduler stamps it on the call's `ToolInvocation`), never
                        // through a slot the adapter shares between turns.
                        let task_context = crate::domain::models::TaskCallContext {
                            parent_ctx_tokens,
                            parent_trace: parent_trace.clone(),
                        };

                        let indexed: Vec<(usize, ToolCallInfo)> =
                            tool_calls.drain(..).enumerate().collect();
                        let (asks, regular): (Vec<_>, Vec<_>) = indexed
                            .into_iter()
                            .partition(|(_, tc)| tc.name == "AskUserQuestion");
                        let mut indexed_results: Vec<(usize, ToolResultMessage)> = Vec::new();

                        for (orig_idx, tc) in asks {
                            if tool_interrupted() {
                                return false;
                            }
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
                                        if tool_interrupted() {
                                            return false;
                                        }
                                        let _ = event_tx.send(AppEvent::ProviderChunk {
                                            conversation_id: conversation_id.clone(),
                                            chunk: StreamChunk::TurnComplete {
                                                stop_reason: StopReason::Cancelled,
                                            },
                                        });
                                        return false;
                                    }
                                },
                                _ = tool_cancel.cancelled() => {
                                    if tool_interrupted() {
                                        return false;
                                    }
                                    let _ = event_tx.send(AppEvent::ProviderChunk {
                                        conversation_id: conversation_id.clone(),
                                        chunk: StreamChunk::ToolResult {
                                            id: tc.id.clone(),
                                            content: "Tool execution cancelled".to_string(),
                                            is_error: true,
                                            operator_private: false,
                                            diff: crate::domain::models::WriteDiffState::NotAWrite,
                                            images: vec![],
                                        },
                                    });
                                    let _ = event_tx.send(AppEvent::ProviderChunk {
                                        conversation_id: conversation_id.clone(),
                                        chunk: StreamChunk::TurnComplete {
                                            stop_reason: StopReason::Cancelled,
                                        },
                                    });
                                    return false;
                                }
                            };
                            let result = crate::domain::models::ToolResult {
                                tool_use_id: tc.id.clone(),
                                content: answer.clone(),
                                is_error: false,
                                images: vec![],
                            };
                            let _ = event_tx.send(AppEvent::ProviderChunk {
                                conversation_id: conversation_id.clone(),
                                chunk: StreamChunk::ToolResult {
                                    id: result.tool_use_id.clone(),
                                    content: result.content.clone(),
                                    is_error: result.is_error,
                                    operator_private: false,
                                    // AskUserQuestion answer — not a file write.
                                    diff: crate::domain::models::WriteDiffState::NotAWrite,
                                    images: vec![],
                                },
                            });
                            indexed_results.push((
                                orig_idx,
                                ToolResultMessage {
                                    tool_use_id: result.tool_use_id,
                                    content: result.content,
                                    is_error: result.is_error,
                                    images: result.images,
                                },
                            ));
                        }

                        if !regular.is_empty() {
                            if tool_interrupted() {
                                return false;
                            }
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
                                    if tool_interrupted() {
                                        return false;
                                    }
                                    scheduled_tools = true;
                                    let mut one = {
                                        let skill_restriction =
                                            activation_set.as_ref().and_then(|s| {
                                                crate::domain::models::SkillToolRestriction::from_active_skills(
                                                    s.active_skills(),
                                                )
                                            });
                                        tool_scheduler
                                            .clone()
                                            .schedule_with_provenance_and_restriction_until(
                                                source.clone(),
                                                vec![request],
                                                tool_cancel.clone(),
                                                skill_restriction.as_ref(),
                                                agent_restriction.as_ref(),
                                                provenance,
                                                Some(turn_origin.route()),
                                                tool_bound,
                                                task_context.clone(),
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
                                let skill_restriction = activation_set.as_ref().and_then(|s| {
                                    crate::domain::models::SkillToolRestriction::from_active_skills(
                                        s.active_skills(),
                                    )
                                });
                                scheduled_tools = true;
                                tool_scheduler
                                    .clone()
                                    .schedule_with_provenance_and_restriction_until(
                                        source,
                                        requests,
                                        tool_cancel.clone(),
                                        skill_restriction.as_ref(),
                                        agent_restriction.as_ref(),
                                        provenance,
                                        Some(turn_origin.route()),
                                        tool_bound,
                                        task_context.clone(),
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
                                let (id, content, is_error, was_cancelled, images) = match call {
                                    ToolCall::Success { id, result, .. } => {
                                        (id, result.output, result.is_error, false, result.images)
                                    }
                                    ToolCall::Error { id, error, .. } => {
                                        (id, error, true, false, Vec::new())
                                    }
                                    ToolCall::Cancelled { id, reason, .. } => (
                                        id,
                                        format!("Tool execution cancelled: {}", reason),
                                        true,
                                        true,
                                        Vec::new(),
                                    ),
                                    _ => (
                                        batch_with_idx[i].1.id.clone(),
                                        "Internal scheduler error: unexpected non-terminal state"
                                            .to_string(),
                                        true,
                                        false,
                                        Vec::new(),
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
                                        operator_private: batch_with_idx[i].1.tool_name
                                            == crate::domain::models::team_proposal::TEAM_PROPOSAL_TOOL_NAME,
                                        diff,
                                        images: images.clone(),
                                    },
                                });
                                // Keep the authenticated handoff on the local event, never
                                // in the tool result sent back to the provider.
                                #[cfg(feature = "session-visit")]
                                let content = {
                                    let mut content = content;
                                    strip_provider_task_frame(
                                        &batch_with_idx[i].1.tool_name,
                                        &mut content,
                                    );
                                    content
                                };
                                indexed_results.push((
                                    batch_with_idx[i].0,
                                    ToolResultMessage {
                                        tool_use_id: id,
                                        content,
                                        is_error,
                                        images,
                                    },
                                ));
                                if was_cancelled {
                                    if tool_interrupted() {
                                        return false;
                                    }
                                    let _ = event_tx.send(AppEvent::ProviderChunk {
                                        conversation_id: conversation_id.clone(),
                                        chunk: StreamChunk::TurnComplete {
                                            stop_reason: StopReason::Cancelled,
                                        },
                                    });
                                    return false;
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
                            provider_replay: vec![],
                        });

                        true
                    };
                    let result = if limited {
                        interruptible(deadline, Some(&turn_cancel), tool_work).await
                    } else {
                        Ok(tool_work.await)
                    };
                    if let Some(execution) = &mut ask_execution {
                        if result.is_err() || tool_interrupted() {
                            tool_cancel.cancel();
                            if scheduled_tools {
                                tool_scheduler
                                    .cancel_ask_approvals(
                                        &turn_origin.approval_source(&conversation_id),
                                    )
                                    .await;
                            }
                            execution.stopped_before_work();
                            return;
                        }
                    }
                    if !result.unwrap_or(false) {
                        return;
                    }
                } else {
                    return;
                }
            }
            Ok(Err(e)) => {
                let _ = event_tx.send(AppEvent::SystemNotice {
                    conversation_id: Some(conversation_id.clone()),
                    level: NoticeLevel::Error,
                    message: format!("{e}"),
                });
                return;
            }
            Ok(Ok(false)) | Err(_) => return,
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

#[cfg(all(test, feature = "session-visit"))]
mod worker_report_boundary_tests {
    use super::strip_provider_task_frame;
    use crate::domain::models::worker_report::{ReportCapture, frame};

    #[test]
    fn provider_receives_only_original_authenticated_task_output() {
        for original in ["", "first\nsecond\n", "échec"] {
            let mut content = frame(ReportCapture::Unavailable, original);
            strip_provider_task_frame("task", &mut content);
            assert_eq!(content, original);
        }
    }

    #[test]
    fn provider_keeps_other_tools_and_unauthenticated_text_literal() {
        let framed = frame(ReportCapture::Unstructured, "original");
        let mut other_tool = framed.clone();
        strip_provider_task_frame("other", &mut other_tool);
        assert_eq!(other_tool, framed);

        let mut forged = format!("{framed}tampered");
        let original = forged.clone();
        strip_provider_task_frame("task", &mut forged);
        assert_eq!(forged, original);
    }
}
