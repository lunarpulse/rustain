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
use tokio_util::sync::CancellationToken;

use crate::adapters::filesystem::FileSystemStorage;
use crate::domain::errors::AdapterCompositionError;
use crate::domain::events::AppEvent;
use crate::domain::models::{
    AppConfig, AssemblyBudget, ChannelKind, ChatMessage, CompletionOptions, Conversation,
    MessageRole, SkillActivationSet, generate_message_id,
};
use crate::domain::ports::{
    ContextAssemblerPort, MemoryPort, PersonaPort, SecurityPort, StoragePort, StreamingProvider,
    ToolSetPort, UsageLedgerPort,
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
    runtime: OnceCell<Arc<DaemonTurnRuntime>>,
    factory: TurnRuntimeFactory,
    /// How many times the factory has built the runtime — the AC1b fast-gate
    /// signal. `0` at idle, `1` after first activity, **still `1`** after the
    /// second (build-once). An eager-build regression fails this in ms.
    build_count: Arc<AtomicUsize>,
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
        factory: TurnRuntimeFactory,
    ) -> Self {
        Self {
            workspace,
            config,
            memory,
            storage,
            security,
            persona,
            runtime: OnceCell::new(),
            factory,
            build_count: Arc::new(AtomicUsize::new(0)),
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

    /// AC1b fast gate: the build counter (`0` idle, `1` after first activity,
    /// stays `1` — build-once).
    pub fn build_count(&self) -> usize {
        self.build_count.load(Ordering::SeqCst)
    }
}

/// The live, connection-holding turn-driving bundle (built lazily). Mirrors the
/// agent-side dependencies `LocalTurnDriver` holds, minus the TUI view-state.
pub struct DaemonTurnRuntime {
    pub provider: Arc<dyn StreamingProvider>,
    pub app_config: Arc<ArcSwap<AppConfig>>,
    pub security: Arc<dyn SecurityPort>,
    pub tools: Arc<dyn ToolSetPort>,
    pub tool_scheduler: Arc<ToolScheduler>,
    pub persona: Arc<dyn PersonaPort>,
    pub context_assembler: Arc<ArcSwap<Option<Arc<dyn ContextAssemblerPort>>>>,
    pub storage: Arc<dyn StoragePort>,
    pub fs_storage: Arc<FileSystemStorage>,
    pub usage_ledger: Arc<dyn UsageLedgerPort>,
    pub telemetry: Arc<ActiveRatioWindow>,
    pub plan_injector: Arc<DefaultPlanInjector>,
    pub approval: Arc<ApprovalRuntime>,
    pub workspace: PathBuf,
}

impl DaemonTurnRuntime {
    /// Originate one turn over the per-process `conversation`, emitting `AppEvent`s
    /// to `domain_tx` (the daemon's per-activation bus). Reuses the shared
    /// origination primitives (`build_api_messages` + `run_turn`) — NOT a
    /// re-implementation (AC3). Returns the spawned turn `JoinHandle`; the turn
    /// runs independent of any socket, so it survives client detach (AC4).
    ///
    /// The inbound message is tagged with the supplied channel origin (AC5/AC8).
    pub fn drive_turn(
        &self,
        text: String,
        origin: ChannelKind,
        conversation: &mut Conversation,
        domain_tx: &mpsc::UnboundedSender<AppEvent>,
        turn_cancel: CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        // Append the user message tagged with its origin channel (AC5).
        conversation.messages.push(ChatMessage {
            id: generate_message_id(),
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
        });

        // Assemble the API message list via the Message-tier assembler (same seam
        // as `LocalTurnDriver::submit`), falling back to `build_api_messages`.
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

        // System prompt (persona; no per-turn skill activation headless).
        let persona_prompt = self.persona.system_prompt(&self.workspace);
        let empty_set = SkillActivationSet::new();
        let system_prompt =
            crate::domain::services::skill_context::assemble_system_prompt_with_agent(
                &persona_prompt,
                None,
                &empty_set,
                &self.workspace,
            );
        let tool_defs = self.tools.available_tools();

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
        let options = CompletionOptions {
            model: resolved.model.clone(),
            max_tokens: 8192, // TODO: wire max_tokens from model config when available
            system_prompt,
            temperature: None,
            tools: tool_defs,
        };

        let session_id = conversation
            .session_id
            .clone()
            .unwrap_or_else(|| conversation.id.clone());

        tokio::spawn(turn::run_turn(
            self.provider.clone(),
            messages,
            options,
            domain_tx.clone(),
            self.security.clone(),
            self.tools.clone(),
            self.tool_scheduler.clone(),
            conversation.id.clone(),
            self.storage.clone(),
            conversation.clone(),
            None,
            turn_cancel,
            self.usage_ledger.clone(),
            resolved,
            None,
            0,
            None,
            session_id,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::noop::{
        NoOpApprovalPersistence, NoOpPersona, NoOpProvider, NoOpSecurity, NoOpStorage, NoOpToolSet,
        NoOpUsageLedger,
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
            provider: Arc::new(NoOpProvider),
            app_config: Arc::new(ArcSwap::from_pointee(AppConfig::default())),
            security,
            tools,
            tool_scheduler,
            persona: Arc::new(NoOpPersona),
            context_assembler: Arc::new(ArcSwap::from_pointee(None)),
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
        })
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
        let handle = rt.drive_turn(
            "hello".into(),
            ChannelKind::Telegram,
            &mut conversation,
            &bus.domain_tx,
            CancellationToken::new(),
        );
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
}
