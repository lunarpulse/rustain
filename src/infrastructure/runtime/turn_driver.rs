//! # Turn-driving seam (Story 12.2a)
//!
//! This module factors the *turn-origination* side out of the TUI event loop.
//!
//! `event_loop::run` historically fused three concerns into one 11k-line
//! function: **producing** turns (assembling the API message list and spawning
//! `run_turn`), **consuming** their `AppEvent`s (`reduce` → view state), and
//! **rendering**. The consumer (`reduce`, `reducer.rs:269`) and the inner
//! producer (`run_turn`, `turn.rs:33`) were already clean; the fusion lived in
//! the 26-argument free function `start_turn` / `start_turn_inner`, called from
//! 18 sites inside `run()`.
//!
//! [`LocalTurnDriver`] is the producer half behind a stable seam:
//!
//! - **Local mode (today, and after this story):**
//!   `input → LocalTurnDriver::submit → run_turn → domain_tx → reduce → render`.
//! - **Attach mode (Story 12.2c, future):**
//!   `input → socket` … `[daemon: run_turn → AppEvent → socket]` …
//!   `client: socket recv → reduce → render`. Same consumer/renderer; a
//!   different driver (`SocketTurnDriver`) and a different event source.
//!
//! ## Design choice: concrete struct first, trait later (Rule of Three)
//!
//! No `TurnDriver` *trait* is introduced here. Per the project's
//! Rule-of-Three-after-the-seam discipline (`architecture.md:1191`), the trait
//! is extracted in Story 12.2b once `SocketTurnDriver` provides the second
//! impl. The seam today is the *single method* [`LocalTurnDriver::submit`] —
//! the one origination door (party-mode Q3).
//!
//! ## Q7 — the seam does not bake in eager-live-deps
//!
//! `LocalTurnDriver` happens to hold already-constructed live `Arc<dyn …>`
//! handles because the interactive TUI is always active. But the *contract* is
//! a single `async fn submit(...)` — it says nothing about *when* the agent
//! runtime is built. Story 12.2b's `SocketTurnDriver` can build its runtime
//! lazily on first `submit(...)` (a `TurnRuntimeFactory` + `OnceCell`) so an
//! idle daemon holds no live provider (NFR46). The seam is about *originating a
//! turn*, not *holding live handles*.

use std::path::PathBuf;
use std::sync::Arc;

use arc_swap::ArcSwap;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::adapters::filesystem::FileSystemStorage;
use crate::adapters::tui::handlers;
use crate::adapters::tui::state::TuiState;
use crate::domain::events::AppEvent;
use crate::domain::models::{
    ActiveAgent, AppConfig, ChatMessage, CompletionOptions, Conversation, ImageAttachment,
    MessageRole, PermissionMode, SessionManager, SkillActivationSet, StatusState, StreamingState,
    generate_conversation_id,
};
use crate::domain::ports::{
    ContextAssemblerPort, ContextPort, PersonaPort, SecurityPort, StoragePort, StreamingProvider,
    ToolSetPort, UsageLedgerPort,
};
use crate::domain::services::message_builder;
use crate::domain::services::plan_mode_injector::{DefaultPlanInjector, PlanModeInjector};
use crate::domain::services::tool_scheduler::ToolScheduler;
use crate::infrastructure::runtime::event_loop::{
    persist_image_attachments, rehydrate_historical_images,
};
use crate::infrastructure::runtime::turn;
use crate::infrastructure::telemetry::ActiveRatioWindow;

/// One user (or synthetic) turn submission — the per-call inputs that vary
/// between the 18 origination sites. Everything *stable* (the agent-side
/// dependencies) is owned by [`LocalTurnDriver`] instead of threaded here.
pub struct UserSubmission {
    /// The user-visible turn text.
    pub text: String,
    /// Fresh image attachments for this turn (empty for synthetic/auto turns).
    pub images: Vec<ImageAttachment>,
    /// `true` for system-originated turns (plan approval/rejection, agent
    /// hand-off) whose user `ChatMessage` is marked `synthetic`.
    pub synthetic: bool,
    /// Per-turn skill activation snapshot (`None` outside an activation flow).
    pub activation_set: Option<SkillActivationSet>,
    /// Per-turn active-agent snapshot (`None`, the live snapshot, or an
    /// explicit fallback agent depending on the call site).
    pub agent_snapshot: Option<ActiveAgent>,
    /// Cancellation token for this turn (minted by the tab manager).
    pub turn_cancel: CancellationToken,
}

/// The TUI view-state bundle the driver mutates while originating a turn.
///
/// Held as one `&mut` group so the call sites stay short and so the
/// view/driver boundary is explicit. Passed **by value** (the call site
/// reborrows its locals into it) — this keeps the relocated body byte-identical
/// (`conversation`, `streaming`, `state`, … unrenamed) and sidesteps the nested
/// `&mut &mut` reborrow gymnastics a `&mut TurnViewState` parameter would force.
///
/// See the per-mutation classification table in the Story 12.2a Dev Notes for
/// which of these mutations a remote (attach) driver replicates locally vs.
/// receives over the wire.
pub struct TurnViewState<'a> {
    pub conversation: &'a mut Conversation,
    pub streaming: &'a mut StreamingState,
    pub state: &'a mut TuiState,
    pub active_turn: &'a mut Option<tokio::task::JoinHandle<()>>,
    pub session_manager: &'a mut SessionManager,
}

/// Local turn driver: owns every agent-side dependency and originates a turn
/// via [`submit`](Self::submit). This is the producer half of the event loop,
/// extracted from the former `start_turn` / `start_turn_inner` free functions.
pub struct LocalTurnDriver {
    provider: Arc<dyn StreamingProvider>,
    /// Held as the live `ArcSwap` slot (not a snapshot) and read with
    /// `load_full()` at submit time, mirroring the `context` slot pattern, so a
    /// `/config reload` between turns is seen. Config only changes on explicit
    /// reload, which has no in-flight turn — byte-identical to passing the
    /// `run()`-held snapshot.
    app_config: Arc<ArcSwap<AppConfig>>,
    domain_tx: mpsc::UnboundedSender<AppEvent>,
    security: Arc<dyn SecurityPort>,
    tools: Arc<dyn ToolSetPort>,
    tool_scheduler: Arc<ToolScheduler>,
    persona: Arc<dyn PersonaPort>,
    context: Arc<ArcSwap<Arc<dyn ContextPort>>>,
    context_assembler: Arc<ArcSwap<Option<Arc<dyn ContextAssemblerPort>>>>,
    workspace_path: PathBuf,
    fs_storage: Arc<FileSystemStorage>,
    storage: Arc<dyn StoragePort>,
    plan_injector: Arc<DefaultPlanInjector>,
    usage_ledger: Arc<dyn UsageLedgerPort>,
    telemetry: Arc<ActiveRatioWindow>,
}

impl LocalTurnDriver {
    /// Construct the driver from the agent-side dependencies `run()` already
    /// holds. All `Arc`s are clones (cheap refcount bumps); `workspace_path` is
    /// cloned once at startup.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        provider: Arc<dyn StreamingProvider>,
        app_config: Arc<ArcSwap<AppConfig>>,
        domain_tx: mpsc::UnboundedSender<AppEvent>,
        security: Arc<dyn SecurityPort>,
        tools: Arc<dyn ToolSetPort>,
        tool_scheduler: Arc<ToolScheduler>,
        persona: Arc<dyn PersonaPort>,
        context: Arc<ArcSwap<Arc<dyn ContextPort>>>,
        context_assembler: Arc<ArcSwap<Option<Arc<dyn ContextAssemblerPort>>>>,
        workspace_path: PathBuf,
        fs_storage: Arc<FileSystemStorage>,
        storage: Arc<dyn StoragePort>,
        plan_injector: Arc<DefaultPlanInjector>,
        usage_ledger: Arc<dyn UsageLedgerPort>,
        telemetry: Arc<ActiveRatioWindow>,
    ) -> Self {
        Self {
            provider,
            app_config,
            domain_tx,
            security,
            tools,
            tool_scheduler,
            persona,
            context,
            context_assembler,
            workspace_path,
            fs_storage,
            storage,
            plan_injector,
            usage_ledger,
            telemetry,
        }
    }

    /// Originate one turn: assemble the API message list, decorate it
    /// (history/plan/context-tier injection, image attach/rehydrate), spawn
    /// `run_turn`, and mark the view-state as streaming.
    ///
    /// This is the relocated body of the former `start_turn_inner`, preserved
    /// verbatim — `run_turn` (`turn.rs:33`) is called unchanged (AC6).
    pub async fn submit(&self, sub: UserSubmission, view: TurnViewState<'_>) {
        // Per-call inputs.
        let UserSubmission {
            text,
            images,
            synthetic,
            activation_set,
            agent_snapshot,
            turn_cancel,
        } = sub;

        // View-state &mut bundle — unrenamed so the relocated body stays verbatim.
        let TurnViewState {
            conversation,
            streaming,
            state,
            active_turn,
            session_manager,
        } = view;

        // Driver-owned dependencies, rebound to the original local names so the
        // relocated body below is byte-identical to the former `start_turn_inner`.
        let config_arc = self.app_config.load_full();
        let config: &AppConfig = &config_arc;
        let provider = &self.provider;
        let domain_tx = &self.domain_tx;
        let security = &self.security;
        let tools = &self.tools;
        let tool_scheduler = &self.tool_scheduler;
        let persona = &self.persona;
        let context = &self.context;
        let context_assembler = &self.context_assembler;
        let workspace_path: &std::path::Path = &self.workspace_path;
        let fs_storage: &FileSystemStorage = &self.fs_storage;
        let storage = &self.storage;
        let plan_injector = &self.plan_injector;
        let usage_ledger = self.usage_ledger.clone();
        let telemetry = self.telemetry.clone();

        // ----- relocated body (was `start_turn_inner`, event_loop.rs:8589-8918) -----
        tracing::debug!(
            "LocalTurnDriver::submit: synthetic={synthetic} text_len={}",
            text.len()
        );
        // Persist any image attachments and collect their references. These are
        // attached to the user ChatMessage so they survive a session reload
        // (Story 4-3a.1 AC3 / DF-067).
        let persisted_refs = if images.is_empty() {
            Vec::new()
        } else {
            persist_image_attachments(&conversation.id, fs_storage, &images)
        };

        // Add user ChatMessage to conversation
        conversation.messages.push(ChatMessage {
            id: generate_conversation_id(),
            role: MessageRole::User,
            content: text.clone(),
            content_blocks: vec![],
            tool_calls: vec![],
            created_at: crate::domain::models::session_meta::now_unix(),
            token_count: None,
            stop_reason: None,
            synthetic,
            images: persisted_refs,
        });

        // Build messages list for provider via the Story 11.0a Message-tier assembler
        // (ADR-10-4). `StaticPassthroughAssembler` is byte-identical to the legacy
        // inline `build_api_messages` and ignores the budget; Story 11.6's
        // `WindowingAssembler` trims to it. The budget is the model's FULL context
        // window (Story 11.6 Q2 — NOT the compaction `*7/10` headroom; windowing and
        // compaction are independent stages). `0`/unknown → `usize::MAX` (no trim).
        // `None` is the reserved eval/replay bypass → fall back to
        // `build_api_messages`. Everything below this line is post-assembly
        // decoration and stays inline.
        let assembly_budget = if state.active_context_window == 0 {
            usize::MAX
        } else {
            state.active_context_window as usize
        };
        let mut messages = match context_assembler.load().as_ref() {
            Some(assembler) => {
                let assembled = assembler.assemble(
                    conversation,
                    crate::domain::models::AssemblyBudget {
                        max_tokens: assembly_budget,
                    },
                );
                // Story 11.6 Task 7 — capture Message-tier diagnostics for `/context
                // show` group info (kept separate from the Content-tier bundle).
                state.last_assembler_diagnostics = Some(assembled.diagnostics);
                assembled.messages
            }
            None => message_builder::build_api_messages(conversation),
        };

        // Story 7.4 AC7: reshape messages when compaction is present
        crate::domain::services::compaction::shape_compacted_messages(conversation, &mut messages);

        // Plan mode reminder injection (Story 6-0d AC2/AC8)
        if security.current_mode() == PermissionMode::Plan {
            if let Some(ref plan_path) = state.plan_file_path {
                if let Some(reminder) = plan_injector.pre_turn(conversation, plan_path).await {
                    if let Some(first_user_msg) =
                        messages.iter_mut().find(|m| m.role == MessageRole::User)
                    {
                        first_user_msg.context_prefix =
                            Some(crate::domain::services::compaction::compose_context_prefix(
                                first_user_msg.context_prefix.take(),
                                reminder,
                            ));
                    }
                    let assistant_turns = conversation
                        .messages
                        .iter()
                        .filter(|m| m.role == MessageRole::Assistant)
                        .count() as u32;
                    state.pending_plan_reminder_at_turn = Some(assistant_turns);
                } else {
                    state.pending_plan_reminder_at_turn = None;
                }
            }
        }

        // Attach images to the last user message in the API request
        // Covers: FR112 (AC1, AC2)
        if !images.is_empty() {
            if let Some(last_user_msg) = messages
                .iter_mut()
                .rev()
                .find(|m| m.role == MessageRole::User && m.content == text)
            {
                last_user_msg.images = images;
            }
        }

        // Rehydrate historical images from disk so the provider sees the same
        // visual context on every subsequent turn (Story 4-3a.1 Addendum 2 /
        // multi-turn image rehydration). `build_api_messages` is a pure domain
        // function and cannot touch disk, so rehydration happens here — after
        // the fresh-turn attachment above, so the just-submitted message is
        // skipped (its `images` vec is already populated with the raw base64
        // and we don't need to re-read it from disk).
        rehydrate_historical_images(conversation, &mut messages, fs_storage);

        // If session manager indicates history rebuild needed, prepend context
        if session_manager.needs_history_rebuild() {
            use crate::domain::services::history_rebuild;
            let context = history_rebuild::build_history_context(
                &conversation.messages[..conversation.messages.len().saturating_sub(1)],
            );
            // Attach context_prefix to the actual user input message (matching `text`),
            // not to a synthetic tool-result message that build_api_messages may have appended.
            if let Some(target_msg) = messages
                .iter_mut()
                .rev()
                .find(|m| m.role == MessageRole::User && m.content == text)
            {
                target_msg.context_prefix =
                    Some(crate::domain::services::compaction::compose_context_prefix(
                        target_msg.context_prefix.take(),
                        context,
                    ));
            } else if let Some(last_msg) = messages.last_mut() {
                // Fallback: attach to last message if exact match not found
                last_msg.context_prefix =
                    Some(crate::domain::services::compaction::compose_context_prefix(
                        last_msg.context_prefix.take(),
                        context,
                    ));
            }
            let new_session_id = generate_conversation_id();
            conversation.session_id = Some(new_session_id.clone());
            session_manager.mark_active(new_session_id);
            let msg_count = conversation.messages.len().saturating_sub(1);
            domain_tx.send(AppEvent::SystemNotice {
                conversation_id: Some(conversation.id.clone()),
                level: crate::domain::models::NoticeLevel::Info,
                message: format!(
                    "\u{2139}\u{fe0f} Session restarted with your conversation history ({} messages).",
                    msg_count
                ),
            }).ok();
        }

        // Story 7.4 AC11: consume pending context carryover on first turn of new conversation
        if let Some(carry) = state.pending_context_carryover.take() {
            if let Some(first_user) = messages.iter_mut().find(|m| m.role == MessageRole::User) {
                first_user.context_prefix =
                    Some(crate::domain::services::compaction::compose_context_prefix(
                        first_user.context_prefix.take(),
                        format!("<conversation-summary>\n{}\n</conversation-summary>", carry),
                    ));
            }
        }

        // Story 11.4 — Content-tier memory/context injection (AC1, AC3). Logic lives
        // in the handler to respect the event_loop line budget; it short-circuits when
        // the session toggle is OFF (AC7).
        handlers::context_command::inject_assembled_context(state, context, &text, &mut messages)
            .await;

        let all_tool_defs = tools.available_tools();
        let persona_prompt = persona.system_prompt(workspace_path);
        let empty_set = crate::domain::models::SkillActivationSet::new();
        let activation = activation_set.as_ref().unwrap_or(&empty_set);
        let agent_body = agent_snapshot.as_ref().map(|a| a.body.as_str());
        let system_prompt =
            crate::domain::services::skill_context::assemble_system_prompt_with_agent(
                &persona_prompt,
                agent_body,
                activation,
                workspace_path,
            );
        let all_tool_names: Vec<String> = all_tool_defs.iter().map(|t| t.name.clone()).collect();
        let agent_filter = agent_snapshot
            .as_ref()
            .and_then(|a| a.effective_tool_filter(&all_tool_names));
        let skill_filter = activation.effective_allowed_tools();
        let combined: Option<std::collections::HashSet<String>> = match (agent_filter, skill_filter)
        {
            (None, None) => None,
            (Some(a), None) => Some(a),
            (None, Some(s)) => Some(s),
            (Some(a), Some(s)) => Some(a.intersection(&s).cloned().collect()),
        };
        if let Some(ref allowed) = combined {
            if allowed.is_empty() {
                domain_tx.send(AppEvent::SystemNotice {
                    conversation_id: Some(conversation.id.clone()),
                    level: crate::domain::models::NoticeLevel::Warning,
                    message: "Active agent and skill tool filters are disjoint — no tools available for this turn".to_string(),
                }).ok();
            }
        }
        let tool_defs = match combined {
            Some(allowed) => {
                let mut filtered: Vec<_> = all_tool_defs
                    .into_iter()
                    .filter(|t| allowed.contains(&t.name) || t.name == "activate_skill")
                    .collect();
                if !filtered.iter().any(|t| t.name == "activate_skill") {
                    let act_tool = crate::domain::models::ToolDefinition {
                        name: "activate_skill".to_string(),
                        description: "Activate an Agent Skill to gain its procedural instructions and tool restrictions. Arg: name of the skill to activate (must match a discovered skill).".to_string(),
                        input_schema: serde_json::json!({
                            "type": "object",
                            "properties": {
                                "name": { "type": "string", "description": "Skill name (exact match, case-sensitive)" },
                                "arguments": { "type": "string", "description": "Optional trailing arguments passed to the skill" }
                            },
                            "required": ["name"]
                        }),
                        parallel_safe: true,
                    };
                    filtered.push(act_tool);
                }
                filtered
            }
            None => all_tool_defs,
        };
        // Story 9.5 — emit telemetry after tool list is built (AC-9-5-7).
        {
            use crate::infrastructure::telemetry::{ProviderId, emit_tool_after_render};
            let provider_id = ProviderId::Anthropic;
            let catalog_len = tool_defs.len();
            let diagnostics = crate::adapters::tool_exposure::RenderDiagnostics::clean();
            emit_tool_after_render(provider_id, catalog_len, &diagnostics, &telemetry).await;
        }
        // --- Model resolution (Story 7.1c) ---
        let retry_count = state.retry_state.as_ref().map_or(0, |r| r.attempt as u32);
        let input_tokens = conversation.usage.as_ref().map_or(0, |u| u.input_tokens);
        let explicit_override = agent_snapshot
            .as_ref()
            .and_then(|a| {
                let m = a.model.as_ref()?;
                if m.is_empty() { None } else { Some(m.clone()) }
            })
            .or_else(|| state.selected_model.clone());
        let req = crate::domain::services::model_router::ModelResolutionRequest {
            explicit_override,
            tier_hint: None,
            step_kind: None,
            retry_count,
            input_tokens,
            fallback_model: config.model.clone(),
        };
        let resolved =
            crate::domain::services::model_router::resolve_effective_model(&req, &config.router);
        if resolved.escalation_reason != crate::domain::models::EscalationReason::None {
            tracing::info!(
                target: "router",
                "tier escalated: reason={:?} model={}",
                resolved.escalation_reason,
                resolved.model
            );
        }
        // Story 7.5 AC1.5 — capture resolved model for Turn.model stamping on the
        // first subsequent TurnComplete (the reducer leaves Turn.model empty).
        state.pending_resolved_model = Some(resolved.model.clone());
        let options = CompletionOptions {
            model: resolved.model.clone(),
            max_tokens: 8192,
            system_prompt: system_prompt.clone(),
            temperature: None,
            tools: tool_defs,
        };

        let session_id = conversation
            .session_id
            .clone()
            .unwrap_or_else(|| conversation.id.clone());

        // Clear any stale buffers from a previous turn (e.g. after TurnContinuing or SystemNotice)
        streaming.current_text_buffer.clear();
        streaming.current_blocks.clear();
        streaming.active_tool_calls.clear();
        streaming.is_streaming = true;
        streaming.phase = crate::domain::models::StreamingPhase::AccumulatingText;

        // Story 10.7 — rough parent context token estimate (1 token ≈ 4 chars)
        let parent_ctx_tokens: u32 = conversation
            .messages
            .iter()
            .map(|m| m.content.len() as u32 / 4)
            .sum();

        // Story 10.7 — generate W3C Trace Context for subagent propagation.
        // AI-11.5 (Epic 11 retro): the prior `ts*31`/`ts*17` form was a pure function of
        // the millisecond clock, so two turns minted in the same millisecond produced
        // identical trace/span ids — a W3C uniqueness violation. Mix a per-process
        // monotonic counter (+ pid) into the ids so collisions are impossible within a
        // process and improbable across processes. No new dependency.
        let parent_trace = {
            use std::sync::atomic::{AtomicU64, Ordering};
            static TRACE_SEQ: AtomicU64 = AtomicU64::new(0);
            let ts = chrono::Utc::now().timestamp_millis() as u64;
            let seq = TRACE_SEQ.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id() as u64;
            // SplitMix64 odd-constant multiply is bijective over u64, so distinct
            // (seq, pid) pairs map to distinct `mixed` values — guaranteeing per-process
            // uniqueness regardless of clock resolution.
            let mixed = (seq ^ (pid << 32)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
            let trace_id = format!("{:016x}{:016x}", ts.wrapping_mul(31), mixed);
            let span_id = format!("{:016x}", ts.wrapping_mul(17) ^ mixed);
            crate::domain::models::TraceContext::new(trace_id, span_id, 1).ok()
        };

        let handle = tokio::spawn(turn::run_turn(
            provider.clone(),
            messages,
            options,
            domain_tx.clone(),
            security.clone(),
            tools.clone(),
            tool_scheduler.clone(),
            conversation.id.clone(),
            storage.clone(),
            conversation.clone(),
            activation_set,
            turn_cancel,
            usage_ledger.clone(),
            resolved,
            None,
            parent_ctx_tokens,
            parent_trace,
            session_id,
        ));
        *active_turn = Some(handle);

        state.status = StatusState::Streaming;
        state.needs_redraw = true;
    }
}
