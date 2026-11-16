//! Daemon turn runtime + lazy composition core (Story 12.2b AC1/AC1b/AC3).
//!
//! [`DaemonCore`] is what `build_daemon_core` returns: the **eager, cheap,
//! connection-free** parts (memory/storage/security/persona/config) plus a
//! `TurnRuntimeFactory` captured behind a single
//! [`tokio::sync::OnceCell`]`<Arc<`[`DaemonTurnRuntime`]`>>`. An idle daemon
//! therefore holds **no live provider connection** (NFR46 < 30 MB) — the
//! provider/tools/scheduler/context/approval are built by the factory on the
//! **first** activity and never again (build-once).
//!
//! [`DaemonTurnRuntime`] is the live bundle. It is **not** a [`TurnDriver`] impl
//! (the daemon is headless — it has no `TurnViewState`); it owns the per-process
//! conversation drive directly, reusing the shared origination *primitives*
//! ([`build_api_messages`](crate::domain::services::message_builder::build_api_messages)
//! + [`run_turn`](crate::infrastructure::runtime::turn::run_turn)) — the same
//! ones `LocalTurnDriver::submit` calls. See the seam-shape reconciliation in the
//! Story 12.2b Dev Notes (settled: factor-core / daemon-owns-runtime).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use arc_swap::ArcSwap;
use tokio::sync::{OnceCell, mpsc};

use super::mcp_admission::McpAdmission;
use tokio_util::sync::CancellationToken;

use crate::adapters::filesystem::FileSystemStorage;
use crate::domain::errors::AdapterCompositionError;
use crate::domain::events::AppEvent;
use crate::domain::models::{
    AppConfig, AssemblyBudget, ChannelKind, ChatMessage, CompletionOptions, ContextBudget,
    ContextBundle, ContextSource, Conversation, ItemAddress, Message, MessageRole,
    ProvenancedEntry, Relevance, RetrievalMethod, SkillActivationSet, StopReason, StreamChunk,
    TurnOrigin, generate_message_id,
};
use crate::domain::ports::{
    ContextAssemblerPort, ContextPort, MemoryPort, PersonaPort, SecurityPort, StoragePort,
    StreamingProvider, ToolSetPort, UsageLedgerPort,
};
use crate::domain::services::approval_runtime::ApprovalRuntime;
use crate::domain::services::message_builder;
use crate::domain::services::model_router::{ModelResolutionRequest, resolve_effective_model};
use crate::domain::services::plan_mode_injector::DefaultPlanInjector;
use crate::domain::services::tool_scheduler::ToolScheduler;
use crate::infrastructure::runtime::turn;
use crate::infrastructure::telemetry::ActiveRatioWindow;

/// Captures config + deps and builds a [`DaemonTurnRuntime`] on demand. Does NOT
/// touch the network when constructed — only when invoked (first activity).
pub type TurnRuntimeFactory =
    Box<dyn Fn() -> Result<Arc<DaemonTurnRuntime>, AdapterCompositionError> + Send + Sync>;

/// The daemon's composed core: eager light parts + a lazily-built live runtime.
///
/// Constructed by `composition::build_daemon_core`. The single
/// [`OnceCell`]`<Arc<DaemonTurnRuntime>>` is the laziness invariant (AC1) —
/// never `Option<T>` scattered across handles.
pub struct DaemonCore {
    pub workspace: PathBuf,
    pub config: Arc<ArcSwap<AppConfig>>,
    /// Eager — the memory sink (12.0 hardened `prepare_detach`→`flush` path).
    pub memory: Arc<dyn MemoryPort>,
    /// Eager — real session storage (the daemon persists its conversation, AC4).
    pub storage: Arc<dyn StoragePort>,
    /// Eager — security policy. Headless: never `Yolo` (AC6).
    pub security: Arc<dyn SecurityPort>,
    /// Eager — persona/system-prompt source.
    pub persona: Arc<dyn PersonaPort>,
    /// Story 18.4a — the replicated Topic log this daemon fills.
    ///
    /// ⚑ Eager and shared: the verified-peer delivery front door writes into
    /// it, and the `"composite"` context adapter — composed from the same
    /// `ComposeContext` — reads it. ⛔ One `Arc`, or a host's agent reads a
    /// different log than the one its transport writes.
    pub peer_topic_store: Arc<crate::adapters::rap::PeerTopicStore>,
    runtime: OnceCell<Arc<DaemonTurnRuntime>>,
    factory: TurnRuntimeFactory,
    /// How many times the factory has built the runtime — the AC1b fast-gate
    /// signal. `0` at idle, `1` after first activity, **still `1`** after the
    /// second (build-once). An eager-build regression fails this in ms.
    build_count: Arc<AtomicUsize>,
    /// Story C-3 (AD-3) — the gate between "start MCP children on first
    /// activity" and "shutdown reaps them"; shared with the factory.
    mcp_admission: Arc<McpAdmission>,
}

impl DaemonCore {
    /// Assemble a `DaemonCore` from its eager parts + a runtime factory.
    pub fn new(
        workspace: PathBuf,
        config: Arc<ArcSwap<AppConfig>>,
        memory: Arc<dyn MemoryPort>,
        storage: Arc<dyn StoragePort>,
        security: Arc<dyn SecurityPort>,
        persona: Arc<dyn PersonaPort>,
        peer_topic_store: Arc<crate::adapters::rap::PeerTopicStore>,
        factory: TurnRuntimeFactory,
    ) -> Self {
        Self {
            workspace,
            config,
            memory,
            storage,
            security,
            persona,
            peer_topic_store,
            runtime: OnceCell::new(),
            factory,
            build_count: Arc::new(AtomicUsize::new(0)),
            mcp_admission: Arc::new(McpAdmission::default()),
        }
    }

    /// Get the live runtime, building it via the factory on first call (and only
    /// the first — `OnceCell` build-once). This is the ONLY place the network is
    /// touched; calling it is "first activity".
    pub async fn ensure_runtime(&self) -> Result<Arc<DaemonTurnRuntime>, AdapterCompositionError> {
        let rt = self
            .runtime
            .get_or_try_init(|| {
                let count = self.build_count.clone();
                let factory = &self.factory;
                async move {
                    let result = factory()?;
                    count.fetch_add(1, Ordering::SeqCst);
                    Ok(result)
                }
            })
            .await?;
        Ok(rt.clone())
    }

    /// AC1b fast gate: has the live runtime been built yet? `false` at idle.
    pub fn is_runtime_initialized(&self) -> bool {
        self.runtime.get().is_some()
    }

    /// 18.9b-a review patch — peek at the already-built runtime WITHOUT
    /// building: the daemon forwarder's `A2aCatalogChanged` arm must not
    /// itself trigger first activity (`ensure_runtime` would).
    pub fn built_runtime(&self) -> Option<Arc<DaemonTurnRuntime>> {
        self.runtime.get().cloned()
    }

    /// AC1b fast gate: the build counter (`0` idle, `1` after first activity,
    /// stays `1` — build-once).
    pub fn build_count(&self) -> usize {
        self.build_count.load(Ordering::SeqCst)
    }

    /// Story C-3 (AD-3) — hand the core the admission the factory shares, so
    /// [`shutdown_mcp`](Self::shutdown_mcp) closes the same gate the factory's
    /// MCP start passes through. A core without it (28 `new` call sites in
    /// tests) keeps the default: open and empty.
    pub fn with_mcp_admission(mut self, admission: Arc<McpAdmission>) -> Self {
        self.mcp_admission = admission;
        self
    }

    /// Story C-3 (AD-11) — close MCP admission, then stop every MCP child.
    ///
    /// (1) closing the admission means a first activity still in progress can no
    /// longer start children; (2) the composite's connect/reconnect supervisors
    /// are aborted so no reconnect respawns a child; (3) every client is
    /// disconnected concurrently within `bound` (a healthy `disconnect` ends in
    /// the transport's bounded 2 s wait and kill; stdin close delivers no EOF,
    /// pre-existing); (4) every still-live spawn attempt of every client is then
    /// SIGKILLed through its pidfd (Linux 5.3+, direct child only; wrapper
    /// descendants are NOT stopped, DF-C-3-MCP-WRAPPER-DESCENDANTS-NOT-STOPPED),
    /// concurrently under ONE shared 1 s deadline, because dropping a `disconnect`
    /// future past its bound does not stop a child owned by the service task.
    /// Total worst case `bound` + 1 s. A child that cannot be stopped (no pidfd or
    /// a failed syscall) is reported and left tracked; PID-file removal then
    /// proceeds best-effort. A core whose runtime was never built (or whose
    /// composite has no MCP servers) returns at once.
    pub async fn shutdown_mcp(&self, bound: std::time::Duration) {
        let Some(tools) = self.mcp_admission.close_and_take() else {
            return;
        };
        let Some(composite) = tools
            .as_any()
            .downcast_ref::<crate::adapters::composite_toolset_adapter::CompositeToolsetAdapter>(
        ) else {
            return;
        };
        if composite.mcp_server_ids().is_empty() {
            return;
        }
        composite.abort_mcp_supervisors().await;
        let (closed, killed) = composite.shutdown_mcp_clients(bound).await;
        tracing::info!("daemon MCP shutdown: {closed} closed, {killed} killed");
    }
}

/// The live, connection-holding turn-driving bundle (built lazily). Mirrors the
/// agent-side dependencies `LocalTurnDriver` holds, minus the TUI view-state.
pub struct DaemonTurnRuntime {
    pub recipient_context: Arc<super::recipient_context::RecipientContextState>,
    pub provider: Arc<dyn StreamingProvider>,
    pub app_config: Arc<ArcSwap<AppConfig>>,
    pub security: Arc<dyn SecurityPort>,
    pub tools: Arc<dyn ToolSetPort>,
    pub tool_scheduler: Arc<ToolScheduler>,
    pub persona: Arc<dyn PersonaPort>,
    /// Message-tier context assembler, shared with the local driver.
    pub context_assembler: Arc<ArcSwap<Option<Arc<dyn ContextAssemblerPort>>>>,
    /// Bundle-tier context for the daemon's interactive operator turn (Story
    /// 18.4a, code-review D2). Composed from the **same** `ComposeContext` as
    /// the p2p delivery handler's `PeerTopicStore`, so the daemon is the one
    /// process that both fills and reads the log. Remote preloaded turns never
    /// touch it: their origin already taints them and FR151 is about the local
    /// operator reading teammates' context.
    pub context: Arc<ArcSwap<Arc<dyn ContextPort>>>,
    pub storage: Arc<dyn StoragePort>,
    pub fs_storage: Arc<FileSystemStorage>,
    pub usage_ledger: Arc<dyn UsageLedgerPort>,
    pub telemetry: Arc<ActiveRatioWindow>,
    pub plan_injector: Arc<DefaultPlanInjector>,
    pub approval: Arc<ApprovalRuntime>,
    pub workspace: PathBuf,
    /// 17.5b — the MCP task runtimes for this activation, so a
    /// `ClientFrame::InputResponse` (D2 inward answer surface) can route the
    /// operator's answer to the parked `Waiting` node via `submit_answer`.
    #[cfg(feature = "mcp")]
    pub mcp_task_runtimes: Vec<Arc<crate::adapters::mcp::task_driver::McpTaskRuntime>>,
}

/// Temporarily presents only local-lineage history to the synchronous assembler.
/// Removed elements remain owned here and are merged back by one linear pass on Drop.
struct FilteredConversation<'a> {
    conversation: &'a mut Conversation,
    hidden_messages: Vec<(usize, ChatMessage)>,
    hidden_turns: Vec<(usize, crate::domain::models::Turn)>,
}

impl<'a> FilteredConversation<'a> {
    fn new(conversation: &'a mut Conversation) -> Self {
        let hidden_messages = extract_hidden(&mut conversation.messages, |message| {
            message.is_peer_display()
        });
        // Exact mirror ids retain known legacy lineage even when old Turns
        // did not persist it. This is a join, never an id-prefix classifier.
        let peer_ids: std::collections::HashSet<&str> = hidden_messages
            .iter()
            .map(|(_, message)| message.id.as_str())
            .collect();
        let hidden_turns = extract_hidden(&mut conversation.turns, |turn| {
            turn.history_source == crate::domain::models::HistorySource::PeerDisplay
                || peer_ids.contains(turn.id.0.as_str())
        });
        drop(peer_ids);
        Self {
            conversation,
            hidden_messages,
            hidden_turns,
        }
    }
}

impl Drop for FilteredConversation<'_> {
    fn drop(&mut self) {
        restore_hidden(
            &mut self.conversation.messages,
            std::mem::take(&mut self.hidden_messages),
        );
        restore_hidden(
            &mut self.conversation.turns,
            std::mem::take(&mut self.hidden_turns),
        );
    }
}

fn extract_hidden<T>(items: &mut Vec<T>, mut hidden: impl FnMut(&T) -> bool) -> Vec<(usize, T)> {
    let index = std::cell::Cell::new(0);
    items
        .extract_if(.., |item| {
            index.set(index.get() + 1);
            hidden(item)
        })
        .map(|item| (index.get() - 1, item))
        .collect()
}

fn restore_hidden<T>(items: &mut Vec<T>, hidden: Vec<(usize, T)>) {
    if hidden.is_empty() {
        return;
    }
    let mut visible = std::mem::take(items).into_iter().peekable();
    let mut removed = hidden.into_iter().peekable();
    let length = visible.size_hint().0 + removed.size_hint().0;
    let mut restored = Vec::with_capacity(length);
    for position in 0..length {
        if removed.peek().is_some_and(|(index, _)| *index == position) {
            restored.push(removed.next().expect("peeked").1);
        } else {
            restored.push(
                visible
                    .next()
                    .expect("conversation view order remains intact"),
            );
        }
    }
    *items = restored;
}

fn recipient_item_data(body: &str, retracted: bool, max_bytes: usize) -> String {
    const OPEN: &str = "BEGIN RECEIVED ITEM DATA (untrusted)\n";
    const CLOSE: &str = "END RECEIVED ITEM DATA\n";
    let mark = if retracted {
        "[sender retracted this item; retained evidence]\n"
    } else {
        ""
    };
    let mut out = String::with_capacity(
        body.len()
            .min(max_bytes)
            .saturating_add(OPEN.len() + CLOSE.len() + mark.len()),
    );
    out.push_str(OPEN);
    out.push_str(mark);
    for line in body.lines() {
        let available = max_bytes.saturating_sub(out.len() + CLOSE.len());
        if available < 3 {
            break;
        }
        let mut end = line.len().min(available - 3);
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        out.push_str("| ");
        out.push_str(&line[..end]);
        out.push('\n');
        if end != line.len() {
            break;
        }
    }
    out.push_str(CLOSE);
    out
}

impl DaemonTurnRuntime {
    /// Originate one turn over the per-process `conversation`, emitting `AppEvent`s
    /// to `domain_tx` (the daemon's per-activation bus). Reuses the shared
    /// origination primitives (`build_api_messages` + `run_turn`) — NOT a
    /// re-implementation (AC3). Returns the spawned turn `JoinHandle`; the turn
    /// runs independent of any socket, so it survives client detach (AC4).
    ///
    /// The inbound message is tagged with the supplied channel origin (AC5/AC8).
    /// An interactive operator turn additionally assembles the daemon-owned
    /// bundle-tier context before spawning; channel and remote-peer turns do
    /// not (Story 18.4a, code-review D2).
    pub async fn drive_turn(
        &self,
        text: String,
        origin: ChannelKind,
        conversation: &mut Conversation,
        domain_tx: &mpsc::UnboundedSender<AppEvent>,
        turn_origin: TurnOrigin,
        turn_cancel: CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        let interactive_operator =
            matches!(&turn_origin, TurnOrigin::Interactive) && origin == ChannelKind::Terminal;
        let user_turn_id = generate_message_id();
        conversation.messages.push(ChatMessage {
            id: user_turn_id.clone(),
            role: MessageRole::User,
            content: text.clone(),
            content_blocks: vec![],
            tool_calls: vec![],
            created_at: crate::domain::models::session_meta::now_unix(),
            token_count: None,
            stop_reason: None,
            synthetic: false,
            images: vec![],
            origin,
            authorship: crate::domain::models::Authorship::HumanWritten,
            send_authorization: crate::domain::models::SendAuthorization::Unknown,
            retracted_at_ms: None,
            history_source: Default::default(),
        });

        let source_view = FilteredConversation::new(conversation);
        let mut messages = self.assemble_messages(source_view.conversation);
        drop(source_view);
        let context_tainted = if interactive_operator {
            self.inject_interactive_peer_context(&mut messages, &text, &user_turn_id)
                .await
        } else {
            false
        };

        self.spawn_turn(
            messages,
            conversation,
            domain_tx,
            turn_origin,
            turn_cancel,
            context_tainted,
        )
    }

    /// Drive a turn whose user message was already appended by a verified
    /// transport ingest. This keeps receipt/replay commit tied to local context
    /// mutation while still routing the resulting tools through the typed
    /// [`TurnOrigin::RemotePeer`] path.
    ///
    /// ⛔ Remote preloaded turns deliberately skip bundle-tier context
    /// injection: their `TurnOrigin::provenance()` already marks them
    /// `SelfOriginated`, and FR151 is the local operator reading peer context —
    /// never a peer driving a turn on this host.
    pub fn drive_preloaded_turn(
        &self,
        conversation: &mut Conversation,
        domain_tx: &mpsc::UnboundedSender<AppEvent>,
        turn_origin: TurnOrigin,
        turn_cancel: CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        let messages = self.assemble_messages(conversation);
        self.spawn_turn(
            messages,
            conversation,
            domain_tx,
            turn_origin,
            turn_cancel,
            false,
        )
    }

    /// Assemble the message tier shared by interactive and preloaded daemon
    /// paths, then shape compaction exactly once.
    fn assemble_messages(&self, conversation: &Conversation) -> Vec<Message> {
        // Assemble the API message list via the Message-tier assembler (same
        // seam as `LocalTurnDriver::submit`), falling back to
        // `build_api_messages`.
        let mut messages = match self.context_assembler.load().as_ref() {
            Some(assembler) => {
                assembler
                    .assemble(
                        conversation,
                        AssemblyBudget {
                            max_tokens: usize::MAX,
                        },
                    )
                    .messages
            }
            None => message_builder::build_api_messages(conversation),
        };
        crate::domain::services::compaction::shape_compacted_messages(conversation, &mut messages);
        messages
    }

    /// Assemble and attach the daemon-owned context bundle for one interactive
    /// operator turn, returning whether the attached bundle carried peer-origin
    /// material.
    ///
    /// Mirrors `inject_assembled_context`'s payload shape without importing the
    /// TUI handler: the daemon has no `TuiState` cache, so no stale-bundle
    /// toggle hazard exists here. The same `ContextBundle::has_peer_origin`
    /// derivation feeds `run_turn`; peer-supplied taint remains structurally
    /// impossible (FR151 / 17.1b Vex rule).
    async fn inject_interactive_peer_context(
        &self,
        messages: &mut [Message],
        text: &str,
        turn_id: &str,
    ) -> bool {
        const SHARED_CONTEXT_BUDGET: usize = 4096;
        let context = self.context.load_full();
        let mut bundle = match context
            .assemble(text, ContextBudget::new(SHARED_CONTEXT_BUDGET))
            .await
        {
            Ok(bundle) => bundle,
            Err(error) => {
                tracing::warn!(error = %error, "daemon interactive context assemble failed — selected items continue without local bundle");
                ContextBundle::empty()
            }
        };

        let mut prefix = bundle.to_prefix().unwrap_or_default();
        let selected = self.recipient_context.selected().await;
        let current =
            super::recipient_context::resolve_current_items(&self.workspace, &selected).await;
        for (item_id, item) in current {
            let Some(item) = item else {
                self.recipient_context.mark_unavailable(&item_id).await;
                continue;
            };
            let source = ContextSource::RecipientItem {
                principal: item.address.principal().clone(),
                item: item.address.item().clone(),
            };
            let attribution = source.attribution();
            let separator_bytes = usize::from(!prefix.is_empty());
            let remaining_bytes = SHARED_CONTEXT_BUDGET
                .saturating_mul(4)
                .saturating_sub(prefix.len() + separator_bytes + attribution.len() + 1);
            let empty_frame = recipient_item_data("", item.retracted, usize::MAX);
            if remaining_bytes <= empty_frame.len() {
                self.recipient_context
                    .record_use(
                        &item_id,
                        super::recipient_context::RecipientContextInclusion::Skipped,
                        0,
                        turn_id,
                    )
                    .await;
                continue;
            }
            let framed_bytes =
                empty_frame.len() + item.body.lines().map(|line| line.len() + 3).sum::<usize>();
            let inclusion = if framed_bytes <= remaining_bytes {
                super::recipient_context::RecipientContextInclusion::Full
            } else {
                super::recipient_context::RecipientContextInclusion::Partial
            };
            let data = recipient_item_data(&item.body, item.retracted, remaining_bytes);
            let cost = (separator_bytes + attribution.len() + 1 + data.len()) / 4;
            let entry = ProvenancedEntry {
                source,
                content: Arc::<str>::from(data),
                timestamp: 0,
                retrieval_method: RetrievalMethod::Structural,
                relevance: Relevance::Unscored,
            };
            if !prefix.is_empty() {
                prefix.push('\n');
            }
            prefix.push_str(&attribution);
            prefix.push(' ');
            prefix.push_str(&entry.content);
            bundle.entries.push(entry);
            self.recipient_context
                .record_use(&item_id, inclusion, cost, turn_id)
                .await;
        }
        if !prefix.is_empty() {
            if let Some(target) = messages
                .iter_mut()
                .rev()
                .find(|message| message.role == MessageRole::User)
            {
                target.context_prefix =
                    Some(crate::domain::services::compaction::compose_context_prefix(
                        target.context_prefix.take(),
                        prefix,
                    ));
            } else {
                tracing::warn!(
                    "daemon interactive context prefix target not found — no user message in batch"
                );
            }
        }
        bundle.has_peer_origin()
    }

    /// Resolve the model and spawn the common `run_turn` engine.
    fn spawn_turn(
        &self,
        messages: Vec<Message>,
        conversation: &Conversation,
        domain_tx: &mpsc::UnboundedSender<AppEvent>,
        turn_origin: TurnOrigin,
        turn_cancel: CancellationToken,
        context_tainted: bool,
    ) -> tokio::task::JoinHandle<()> {
        // System prompt (persona; no per-turn skill activation headless).
        // `None` catalog: a daemon turn has no skill source (DF-FR41-DAEMON-ASK-NO-SKILL-SOURCE).
        let persona_prompt = self.persona.system_prompt(&self.workspace);
        let empty_set = SkillActivationSet::new();
        let system_prompt =
            crate::domain::services::skill_context::assemble_system_prompt_with_agent(
                &persona_prompt,
                None,
                None,
                &empty_set,
                &self.workspace,
            );

        // Model resolution (defaults — no TUI retry/selected-model state).
        let config = self.app_config.load_full();
        let req = ModelResolutionRequest {
            explicit_override: None,
            tier_hint: None,
            step_kind: None,
            retry_count: 0,
            input_tokens: 0,
            fallback_model: config.model.clone(),
        };
        let resolved = resolve_effective_model(&req, &config.router);
        // Story C-3 (AD-4): the tool catalogue is read INSIDE the spawned task,
        // after the (bounded) MCP settle wait, so a first turn never reads an
        // empty `mcp__` catalogue because the servers are still handshaking. The
        // wait lives here, not in `ensure_runtime`, so a cron profile's boot
        // pre-warm never blocks daemon readiness.
        let options = CompletionOptions {
            session_id: Some(conversation.id.clone()),
            model: resolved.model.clone(),
            max_tokens: 8192, // TODO: wire max_tokens from model config when available
            system_prompt,
            temperature: None,
            tools: Vec::new(),
        };

        let session_id = conversation
            .session_id
            .clone()
            .unwrap_or_else(|| conversation.id.clone());

        let provider = self.provider.clone();
        let domain_tx = domain_tx.clone();
        let security = self.security.clone();
        let tools = self.tools.clone();
        let tool_scheduler = self.tool_scheduler.clone();
        let conversation_id = conversation.id.clone();
        let storage = self.storage.clone();
        let conversation = conversation.clone();
        let usage_ledger = self.usage_ledger.clone();

        tokio::spawn(async move {
            let mut options = options;
            if !settle_mcp_before_tools(&tools, &domain_tx, &conversation_id, &turn_cancel).await {
                return;
            }
            options.tools = tools.available_tools();
            turn::run_turn(
                provider,
                messages,
                options,
                domain_tx,
                security,
                tools,
                tool_scheduler,
                conversation_id,
                storage,
                conversation,
                None,
                // Story 19.28 AC5 / A16 item 1 — proven-correct `None`, stated
                // rather than left silent. Measured at implementation and
                // re-measured at the 2026-09-13 code review: `grep -n
                // "ActiveAgent\|agent_snapshot\|active_agent"
                // adapters/daemon/runtime.rs` returns ZERO hits — a daemon turn
                // carries no custom-agent context and the wire protocol has no
                // field to carry one, so there is no restriction to thread.
                // ⛔ Not an oversight — the absence is structural.
                // Load-bearing-ness is proved by mutant AC5(b), not by this comment.
                None,
                turn_cancel,
                usage_ledger,
                resolved,
                None,
                0,
                None,
                session_id,
                turn_origin,
                context_tainted,
                None,
            )
            .await
        })
    }
}

/// Upper bound on how long a daemon turn waits for the MCP servers' initial
/// connect attempts (AD-4): above the 10 s per-server handshake + `tools/list`.
pub const DAEMON_MCP_SETTLE_BOUND: std::time::Duration = std::time::Duration::from_secs(15);

/// Story C-3 (AD-4) — wait (bounded, cancellable) for the composite toolset's MCP
/// servers to finish their initial connect attempts before the turn reads its
/// tool catalogue. Returns `false` when the turn was cancelled during the wait:
/// the terminal `run_turn` itself sends for a cancelled turn has then already
/// been emitted and the caller must return WITHOUT calling the provider
/// (`run_turn` checks cancellation only from its second iteration on, so falling
/// through would make one provider call for a cancelled turn). A toolset without
/// MCP clients, or an already-settled one, returns `true` at once.
async fn settle_mcp_before_tools(
    tools: &Arc<dyn ToolSetPort>,
    domain_tx: &mpsc::UnboundedSender<AppEvent>,
    conversation_id: &str,
    turn_cancel: &CancellationToken,
) -> bool {
    let Some(composite) = tools
        .as_any()
        .downcast_ref::<crate::adapters::composite_toolset_adapter::CompositeToolsetAdapter>(
    ) else {
        return true;
    };
    let close_cancelled = || {
        let _ = domain_tx.send(AppEvent::ProviderChunk {
            conversation_id: conversation_id.to_string(),
            chunk: StreamChunk::TurnComplete {
                stop_reason: StopReason::Cancelled,
            },
        });
        false
    };
    // C3-02: a cancellation that is already ready wins over everything below,
    // including the settled fast path and a settle signal that fires at the same
    // instant — `run_turn` checks cancellation only from its second iteration on.
    if turn_cancel.is_cancelled() {
        return close_cancelled();
    }
    if composite.is_mcp_settled() {
        return true;
    }
    let ids = composite.mcp_server_ids();
    tracing::info!(
        "waiting up to 15 s for {} MCP server(s) to settle: {}",
        ids.len(),
        ids.join(", ")
    );
    tokio::select! {
        biased;
        _ = turn_cancel.cancelled() => close_cancelled(),
        settled = composite.mcp_initial_connect_settled(DAEMON_MCP_SETTLE_BOUND) => {
            if !settled {
                tracing::warn!(
                    "MCP settle bound elapsed; the turn proceeds with the connected servers (configured: {})",
                    ids.join(", ")
                );
            }
            // Re-check: a cancel that became ready while settling still wins.
            if turn_cancel.is_cancelled() {
                close_cancelled()
            } else {
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::noop::{
        NoOpApprovalPersistence, NoOpContext, NoOpPersona, NoOpProvider, NoOpSecurity, NoOpStorage,
        NoOpToolSet, NoOpUsageLedger,
    };

    /// Build a minimal all-NoOp `DaemonTurnRuntime` for the laziness gate. Cheap,
    /// no network — exercises only the `OnceCell` build-once mechanism.
    fn noop_runtime(workspace: PathBuf) -> Arc<DaemonTurnRuntime> {
        let security: Arc<dyn SecurityPort> = Arc::new(NoOpSecurity);
        let tools: Arc<dyn ToolSetPort> = Arc::new(NoOpToolSet);
        let approval = ApprovalRuntime::new(16, Arc::new(NoOpApprovalPersistence));
        let tool_scheduler =
            ToolScheduler::new(security.clone(), tools.clone(), approval.clone(), 16);
        let sessions = crate::infrastructure::paths::sessions_dir(&workspace);
        Arc::new(DaemonTurnRuntime {
            recipient_context: Arc::new(
                crate::adapters::daemon::recipient_context::RecipientContextState::default(),
            ),
            provider: Arc::new(NoOpProvider),
            app_config: Arc::new(ArcSwap::from_pointee(AppConfig::default())),
            security,
            tools,
            tool_scheduler,
            persona: Arc::new(NoOpPersona),
            context_assembler: Arc::new(ArcSwap::from_pointee(None)),
            context: Arc::new(ArcSwap::from_pointee(
                Arc::new(NoOpContext) as Arc<dyn ContextPort>
            )),
            storage: Arc::new(NoOpStorage),
            fs_storage: Arc::new(FileSystemStorage::with_workspace_root(
                sessions,
                workspace.clone(),
            )),
            usage_ledger: Arc::new(NoOpUsageLedger),
            telemetry: ActiveRatioWindow::new_in_memory(),
            plan_injector: Arc::new(DefaultPlanInjector::new()),
            approval,
            workspace,
            #[cfg(feature = "mcp")]
            mcp_task_runtimes: Vec::new(),
        })
    }

    #[tokio::test]
    async fn concurrent_turns_keep_conversation_identity_across_runtime_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        let mut rt = noop_runtime(tmp.path().to_path_buf());
        let provider = Arc::new(crate::adapters::noop::RecordingProvider::default());
        Arc::get_mut(&mut rt).unwrap().provider = provider.clone();
        let (tx, _rx) = mpsc::unbounded_channel();
        let a = Conversation {
            id: "conversation-a".into(),
            session_id: Some("ephemeral-a".into()),
            ..Default::default()
        };
        let b = Conversation {
            id: "conversation-b".into(),
            session_id: Some("ephemeral-b".into()),
            ..Default::default()
        };
        let first = rt.spawn_turn(
            Vec::new(),
            &a,
            &tx,
            TurnOrigin::Interactive,
            CancellationToken::new(),
            false,
        );
        let second = rt.spawn_turn(
            Vec::new(),
            &b,
            &tx,
            TurnOrigin::Interactive,
            CancellationToken::new(),
            false,
        );
        first.await.unwrap();
        second.await.unwrap();
        let mut resumed = a.clone();
        resumed.session_id = Some("after-restart".into());
        rt.spawn_turn(
            Vec::new(),
            &resumed,
            &tx,
            TurnOrigin::Interactive,
            CancellationToken::new(),
            false,
        )
        .await
        .unwrap();
        let requests = provider.requests.lock().await;
        let mut ids: Vec<_> = requests
            .iter()
            .map(|o| o.session_id.as_deref().unwrap())
            .collect();
        ids.sort_unstable();
        assert_eq!(ids, ["conversation-a", "conversation-a", "conversation-b"]);
    }

    #[tokio::test]
    async fn common_turn_overrides_stale_or_missing_provider_session_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let rt = noop_runtime(tmp.path().to_path_buf());
        let provider = Arc::new(crate::adapters::noop::RecordingProvider::default());
        let (tx, _rx) = mpsc::unbounded_channel();
        let resolved = resolve_effective_model(
            &ModelResolutionRequest {
                explicit_override: None,
                tier_hint: None,
                step_kind: None,
                retry_count: 0,
                input_tokens: 0,
                fallback_model: "model".into(),
            },
            &AppConfig::default().router,
        );
        for session_id in [None, Some("stale-session".to_owned())] {
            turn::run_turn(
                provider.clone(),
                Vec::new(),
                CompletionOptions {
                    session_id,
                    model: "model".into(),
                    max_tokens: 16,
                    system_prompt: String::new(),
                    temperature: None,
                    tools: Vec::new(),
                },
                tx.clone(),
                rt.security.clone(),
                rt.tools.clone(),
                rt.tool_scheduler.clone(),
                "stable-conversation".into(),
                rt.storage.clone(),
                Conversation {
                    id: "stable-conversation".into(),
                    ..Default::default()
                },
                None,
                None,
                CancellationToken::new(),
                rt.usage_ledger.clone(),
                resolved.clone(),
                None,
                0,
                None,
                "transient-runtime-session".into(),
                TurnOrigin::Interactive,
                false,
                None,
            )
            .await;
        }
        let requests = provider.requests.lock().await;
        assert_eq!(requests.len(), 2);
        assert!(
            requests
                .iter()
                .all(|o| o.session_id.as_deref() == Some("stable-conversation"))
        );
    }

    fn noop_core() -> DaemonCore {
        let workspace = PathBuf::from("/tmp/daemon-core-test");
        let ws = workspace.clone();
        DaemonCore::new(
            workspace.clone(),
            Arc::new(ArcSwap::from_pointee(AppConfig::default())),
            Arc::new(crate::adapters::noop::NoOpMemory),
            Arc::new(NoOpStorage),
            Arc::new(NoOpSecurity),
            Arc::new(NoOpPersona),
            Arc::new(crate::adapters::rap::PeerTopicStore::new()),
            Box::new(move || Ok(noop_runtime(ws.clone()))),
        )
    }

    #[tokio::test]
    async fn runtime_is_lazy_and_built_exactly_once() {
        let core = noop_core();
        // AC1b: idle daemon has NOT built the live runtime.
        assert!(
            !core.is_runtime_initialized(),
            "runtime must be unset at idle"
        );
        assert_eq!(core.build_count(), 0, "no build at idle");

        // First activity builds it.
        let _ = core.ensure_runtime().await.unwrap();
        assert!(
            core.is_runtime_initialized(),
            "runtime set after first activity"
        );
        assert_eq!(core.build_count(), 1, "built exactly once");

        // Second activity reuses it — build-once (no eager/repeat build).
        let _ = core.ensure_runtime().await.unwrap();
        assert_eq!(core.build_count(), 1, "still 1 after a second activation");
    }

    #[tokio::test]
    async fn drive_turn_tags_user_message_with_supplied_origin() {
        let tmp = tempfile::tempdir().unwrap();
        let rt = noop_runtime(tmp.path().to_path_buf());
        let mut conversation = Conversation {
            id: "origin-test".into(),
            ..Default::default()
        };
        let (bus, _rx) = crate::infrastructure::runtime::event_bus::EventBus::new(8);
        let handle = rt
            .drive_turn(
                "hello".into(),
                ChannelKind::Telegram,
                &mut conversation,
                &bus.domain_tx,
                TurnOrigin::Interactive,
                CancellationToken::new(),
            )
            .await;
        handle.abort();
        assert_eq!(conversation.messages.len(), 1);
        assert_eq!(conversation.messages[0].origin, ChannelKind::Telegram);
    }
    #[tokio::test]
    async fn ensure_runtime_returns_the_same_instance() {
        let core = noop_core();
        let a = core.ensure_runtime().await.unwrap();
        let b = core.ensure_runtime().await.unwrap();
        assert!(Arc::ptr_eq(&a, &b), "OnceCell yields one shared runtime");
    }

    #[test]
    fn filtered_assembler_view_restores_message_and_turn_order_during_unwind() {
        use crate::domain::models::{HistorySource, Turn};
        let local = ChatMessage {
            id: "local".into(),
            ..Default::default()
        };
        let mut peer = ChatMessage {
            id: "peer".into(),
            ..Default::default()
        };
        peer.history_source = HistorySource::PeerDisplay;
        let local_turn = Turn::new(String::new(), 0);
        let mut peer_turn = Turn::new(String::new(), 0);
        let tail_turn = Turn::new(String::new(), 0);
        peer_turn.history_source = HistorySource::PeerDisplay;
        let mut conversation = Conversation {
            messages: vec![
                local.clone(),
                peer.clone(),
                ChatMessage {
                    id: "tail".into(),
                    ..Default::default()
                },
            ],
            turns: vec![local_turn, peer_turn, tail_turn],
            ..Default::default()
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let view = FilteredConversation::new(&mut conversation);
            assert_eq!(view.conversation.messages.len(), 2);
            assert_eq!(view.conversation.turns.len(), 2);
            panic!("assembler unwind");
        }));
        assert!(result.is_err());
        assert_eq!(
            conversation
                .messages
                .iter()
                .map(|message| message.id.as_str())
                .collect::<Vec<_>>(),
            ["local", "peer", "tail"],
        );
        assert_eq!(
            conversation
                .turns
                .iter()
                .map(|turn| turn.history_source)
                .collect::<Vec<_>>(),
            [
                HistorySource::Local,
                HistorySource::PeerDisplay,
                HistorySource::Local
            ],
        );
    }

    /// An unsettled composite: one MCP client that is never started, so the
    /// settle signal stays `false` and nothing is spawned.
    fn unsettled_composite() -> Arc<dyn ToolSetPort> {
        let spec = crate::domain::models::McpServerSpec {
            id: "c3-unsettled".into(),
            transport: crate::domain::models::McpTransport::Stdio,
            command: Some("c3-never-started".into()),
            args: Vec::new(),
            env: Default::default(),
            url: None,
            persistent: false,
            source: crate::domain::models::McpServerSource::Workspace,
        };
        let client = Arc::new(crate::adapters::mcp::client::McpClientAdapter::new(
            spec.clone(),
            None,
        ));
        Arc::new(
            crate::adapters::composite_toolset_adapter::CompositeToolsetAdapter::new(
                Arc::new(NoOpToolSet),
                vec![client],
                vec![spec],
                true,
                None,
                None,
                None,
            ),
        )
    }

    /// AD-4/AD-15(b): a turn cancelled while it waits for MCP to settle closes
    /// exactly as `run_turn` would (one `TurnComplete { Cancelled }`) and makes NO
    /// provider call; a settled composite then runs a normal turn (one call).
    #[tokio::test]
    async fn cancelled_settle_wait_closes_the_turn_without_a_provider_call() {
        let tmp = tempfile::tempdir().unwrap();
        let mut rt = noop_runtime(tmp.path().to_path_buf());
        let provider = Arc::new(crate::adapters::noop::RecordingProvider::default());
        let composite = unsettled_composite();
        {
            let rt = Arc::get_mut(&mut rt).unwrap();
            rt.provider = provider.clone();
            rt.tools = composite.clone();
        }
        let conversation = Conversation {
            id: "c3-settle".into(),
            ..Default::default()
        };
        let (tx, mut rx) = mpsc::unbounded_channel();
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        rt.spawn_turn(
            Vec::new(),
            &conversation,
            &tx,
            TurnOrigin::Interactive,
            cancelled,
            false,
        )
        .await
        .unwrap();
        let mut terminals = 0;
        while let Ok(event) = rx.try_recv() {
            if let AppEvent::ProviderChunk {
                conversation_id,
                chunk:
                    StreamChunk::TurnComplete {
                        stop_reason: StopReason::Cancelled,
                    },
            } = event
            {
                assert_eq!(conversation_id, "c3-settle");
                terminals += 1;
            }
        }
        assert_eq!(terminals, 1, "exactly one Cancelled terminal");
        assert!(
            provider.requests.lock().await.is_empty(),
            "a cancelled settle wait must not reach the provider"
        );

        // Settled composite + fresh token: one provider call.
        composite
            .as_any()
            .downcast_ref::<crate::adapters::composite_toolset_adapter::CompositeToolsetAdapter>()
            .unwrap()
            .mark_mcp_settled();
        rt.spawn_turn(
            Vec::new(),
            &conversation,
            &tx,
            TurnOrigin::Interactive,
            CancellationToken::new(),
            false,
        )
        .await
        .unwrap();
        assert_eq!(provider.requests.lock().await.len(), 1);
    }

    /// C3-02: a ready cancellation wins even when the composite is ALREADY
    /// settled (the fast path): one Cancelled terminal, no provider call.
    #[tokio::test]
    async fn a_cancelled_turn_on_a_settled_composite_makes_no_provider_call() {
        let tmp = tempfile::tempdir().unwrap();
        let mut rt = noop_runtime(tmp.path().to_path_buf());
        let provider = Arc::new(crate::adapters::noop::RecordingProvider::default());
        let composite = unsettled_composite();
        composite
            .as_any()
            .downcast_ref::<crate::adapters::composite_toolset_adapter::CompositeToolsetAdapter>()
            .unwrap()
            .mark_mcp_settled();
        {
            let rt = Arc::get_mut(&mut rt).unwrap();
            rt.provider = provider.clone();
            rt.tools = composite;
        }
        let conversation = Conversation {
            id: "c3-fast".into(),
            ..Default::default()
        };
        let (tx, mut rx) = mpsc::unbounded_channel();
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        rt.spawn_turn(
            Vec::new(),
            &conversation,
            &tx,
            TurnOrigin::Interactive,
            cancelled,
            false,
        )
        .await
        .unwrap();
        let mut terminals = 0;
        while let Ok(event) = rx.try_recv() {
            if matches!(
                event,
                AppEvent::ProviderChunk {
                    chunk: StreamChunk::TurnComplete {
                        stop_reason: StopReason::Cancelled
                    },
                    ..
                }
            ) {
                terminals += 1;
            }
        }
        assert_eq!(terminals, 1, "exactly one Cancelled terminal");
        assert!(provider.requests.lock().await.is_empty());
    }

    // ── C3-03 / AD-15(a): admission through the PRODUCTION shutdown, real child ──

    /// A composite with one stdio client that is NOT an MCP server: `tail -f` on
    /// a tagged file. It keeps the tag on its own command line, has no children,
    /// and never completes the handshake, so its connect stays in flight.
    #[cfg(target_os = "linux")]
    fn tail_composite(dir: &std::path::Path, tag: &str) -> Arc<dyn ToolSetPort> {
        let file = dir.join(tag);
        std::fs::write(&file, "").unwrap();
        let spec = crate::domain::models::McpServerSpec {
            id: "c3-tail".into(),
            transport: crate::domain::models::McpTransport::Stdio,
            command: Some("tail".into()),
            args: vec![
                "-f".into(),
                "/dev/null".into(),
                file.to_string_lossy().into(),
            ],
            env: Default::default(),
            url: None,
            persistent: false,
            source: crate::domain::models::McpServerSource::Workspace,
        };
        let client = Arc::new(crate::adapters::mcp::client::McpClientAdapter::new(
            spec.clone(),
            None,
        ));
        Arc::new(
            crate::adapters::composite_toolset_adapter::CompositeToolsetAdapter::new(
                Arc::new(NoOpToolSet),
                vec![client],
                vec![spec],
                true,
                None,
                None,
                None,
            ),
        )
    }

    #[cfg(target_os = "linux")]
    fn composite_of(
        tools: &Arc<dyn ToolSetPort>,
    ) -> &crate::adapters::composite_toolset_adapter::CompositeToolsetAdapter {
        tools
            .as_any()
            .downcast_ref::<crate::adapters::composite_toolset_adapter::CompositeToolsetAdapter>()
            .unwrap()
    }

    #[cfg(target_os = "linux")]
    fn tagged_process_exists(tag: &str) -> bool {
        std::process::Command::new("pgrep")
            .args(["-f", tag])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[cfg(target_os = "linux")]
    fn test_core(workspace: PathBuf, admission: Arc<McpAdmission>) -> DaemonCore {
        DaemonCore::new(
            workspace,
            Arc::new(ArcSwap::from_pointee(AppConfig::default())),
            Arc::new(crate::adapters::noop::NoOpMemory),
            Arc::new(NoOpStorage),
            Arc::new(NoOpSecurity),
            Arc::new(NoOpPersona),
            Arc::new(crate::adapters::rap::PeerTopicStore::new()),
            Box::new(|| {
                Err(AdapterCompositionError::MissingDimension {
                    port: crate::domain::models::PortDimension::Persona,
                })
            }),
        )
        .with_mcp_admission(admission)
    }

    /// `/proc/<pid>` is gone, or the process is a zombie awaiting its owner's
    /// reap: it has TERMINATED. (Shutdown guarantees exit; reaping is the owning
    /// `Child`'s / Tokio's job and is asserted separately within a bound.)
    #[cfg(target_os = "linux")]
    fn terminated(pid: u32) -> bool {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Err(_) => true,
            Ok(stat) => stat
                .rsplit(')')
                .next()
                .is_some_and(|rest| rest.trim_start().starts_with('Z')),
        }
    }

    #[cfg(target_os = "linux")]
    async fn reaped_within(pid: u32, secs: u64) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
        while std::path::Path::new(&format!("/proc/{pid}")).exists() {
            if std::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        true
    }

    /// The child is observed (pid registered) BEFORE shutdown, and TERMINATED
    /// when `shutdown_mcp` returns, through the production function. The connect
    /// is still in `initialize` when shutdown aborts it (C3-01).
    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_mcp_kills_a_real_child_before_returning() {
        if !crate::adapters::mcp::child_handle::pidfd_supported() {
            let _ = std::io::Write::write_all(
                &mut std::io::stderr(),
                b"SKIPPED shutdown_mcp_kills_a_real_child_before_returning: pidfd_open unavailable\n",
            );
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let tag = format!("c3-reap-{}", generate_message_id());
        let tools = tail_composite(tmp.path(), &tag);
        let admission = Arc::new(McpAdmission::default());
        let core = test_core(tmp.path().to_path_buf(), Arc::clone(&admission));
        assert!(admission.start_if_open(&tools, || composite_of(&tools).start_mcp_connections()));

        let client = Arc::clone(&composite_of(&tools).mcp_clients()[0]);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let pid = loop {
            if let Some(pid) = client.live_child_pids().await.first().copied() {
                break pid;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the child never started"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        };
        assert!(!terminated(pid), "control: alive before shutdown");

        core.shutdown_mcp(std::time::Duration::from_secs(3)).await;

        assert!(
            terminated(pid),
            "the child must have exited when shutdown returns"
        );
        assert!(reaped_within(pid, 5).await, "and be reaped by its owner");
        // Never-built / second call: returns at once, nothing to stop.
        core.shutdown_mcp(std::time::Duration::from_secs(3)).await;
    }

    /// AD-15(a) through the production shutdown, NON-VACUOUS: a starter parked
    /// between the closed-check and `start()` holds the admission; `shutdown_mcp`
    /// stays pending; the start closure then waits (still holding the admission)
    /// until the child is POSITIVELY observed, so shutdown can only run against a
    /// started child; after release shutdown terminates exactly that child.
    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_mcp_waits_for_a_parked_starter_then_stops_its_observed_child() {
        if !crate::adapters::mcp::child_handle::pidfd_supported() {
            let _ = std::io::Write::write_all(
                &mut std::io::stderr(),
                b"SKIPPED shutdown_mcp_waits_for_a_parked_starter_then_stops_its_observed_child: pidfd_open unavailable\n",
            );
            return;
        }
        use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
        use std::sync::mpsc;
        let tmp = tempfile::tempdir().unwrap();
        let tag = format!("c3-race-{}", generate_message_id());
        let tools = tail_composite(tmp.path(), &tag);
        let admission = Arc::new(McpAdmission::default());
        let core = Arc::new(test_core(tmp.path().to_path_buf(), Arc::clone(&admission)));

        let hook_reached = Arc::new(AtomicBool::new(false));
        let (entered_tx, entered_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = std::sync::Mutex::new(release_rx); // CONFORMANCE_EXCEPTION_STD_SYNC_LOCK: test-only, receiver shared into a Fn hook.
        {
            let hook_reached = Arc::clone(&hook_reached);
            admission.set_between_check_and_start_hook(Box::new(move || {
                hook_reached.store(true, Ordering::SeqCst);
                entered_tx.send(()).expect("entered");
                release_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(std::time::Duration::from_secs(20))
                    .expect("released");
            }));
        }

        let observed_pid = Arc::new(AtomicU32::new(0));
        let handle = tokio::runtime::Handle::current();
        let starter = {
            let admission = Arc::clone(&admission);
            let tools = Arc::clone(&tools);
            let observed_pid = Arc::clone(&observed_pid);
            std::thread::spawn(move || {
                admission.start_if_open(&tools, || {
                    let client = Arc::clone(&composite_of(&tools).mcp_clients()[0]);
                    {
                        let _entered = handle.enter();
                        composite_of(&tools).start_mcp_connections();
                    }
                    // Still holding the admission: wait until the child exists.
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                    loop {
                        if let Some(pid) = handle.block_on(client.live_child_pids()).first() {
                            observed_pid.store(*pid, Ordering::SeqCst);
                            break;
                        }
                        assert!(
                            std::time::Instant::now() < deadline,
                            "the child never started"
                        );
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                })
            })
        };
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("starter parked in the hook");
        assert!(
            hook_reached.load(Ordering::SeqCst),
            "the hook must have been reached"
        );

        let mut closer = {
            let core = Arc::clone(&core);
            tokio::spawn(async move { core.shutdown_mcp(std::time::Duration::from_secs(3)).await })
        };
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(300), &mut closer)
                .await
                .is_err(),
            "shutdown must stay pending while the starter holds the admission"
        );
        assert_eq!(
            observed_pid.load(Ordering::SeqCst),
            0,
            "no child before the release"
        );

        release_tx.send(()).expect("release");
        assert!(starter.join().expect("starter thread"), "start ran");
        let pid = observed_pid.load(Ordering::SeqCst);
        assert_ne!(
            pid, 0,
            "the child was positively observed before shutdown ran"
        );
        tokio::time::timeout(std::time::Duration::from_secs(20), closer)
            .await
            .expect("shutdown finishes once released")
            .expect("shutdown task");
        assert!(terminated(pid), "the observed child must have exited");
        assert!(reaped_within(pid, 5).await);
        // The gate is closed for good: a later start spawns nothing.
        assert!(!admission.start_if_open(&tools, || panic!("must not start after shutdown")));
    }
}
