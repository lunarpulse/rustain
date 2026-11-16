//! FR41 (Grand Plan row C-1) — the Tier-1 skill catalog reaches the model.
//!
//! # Front doors
//!
//! K1–K3, K7, K8 drive `LocalTurnDriver::submit` (the nearest test-visible seam
//! on the TUI production path; `event_loop::run` hands it the exposure slot and
//! is covered by the native smoke N1). K6 drives the daemon's
//! `DaemonTurnRuntime::drive_turn`. K4 (ACP) lives in `conformance_acp.rs` and
//! K5 (`ask`) in `cli_ask.rs`.
//!
//! ⛔ No keystone calls `render_turn_skill_catalog` or
//! `assemble_system_prompt_with_agent`, not even to compute an expected value:
//! expectations are literals, or the persona's own output.
//!
//! Every keystone asserts the whole system prompt with `assert_eq!`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use async_trait::async_trait;
use parking_lot::Mutex as PlMutex;
use tokio::sync::{RwLock, mpsc};
use tokio_util::sync::CancellationToken;

use rustain::adapters::filesystem::FileSystemStorage;
use rustain::adapters::noop::{NoOpContext, NoOpSecurity, NoOpStorage, NoOpUsageLedger};
use rustain::adapters::skill_activation::SkillActivator;
use rustain::adapters::skill_exposure::{L1MetadataExposure, StaticFullExposure};
use rustain::adapters::skill_registry::SkillRegistry;
use rustain::domain::models::{
    CompletionOptions, Conversation, Message, ModelDescriptor, SessionManager, SessionState,
    SkillActivationSet, StopReason, StreamChunk, StreamingState, ToolDefinition, ToolResult,
};
use rustain::domain::ports::{
    PersonaPort, SecurityPort, SkillExposurePort, StreamingProvider, ToolSetPort, UsageLedgerPort,
};
use rustain::domain::services::approval_runtime::ApprovalRuntime;
use rustain::domain::services::tool_scheduler::ToolScheduler;
use rustain::infrastructure::runtime::turn_driver::{
    LocalTurnDriver, TurnViewState, UserSubmission,
};
use rustain::infrastructure::skill_cache::SkillCache;
use rustain::infrastructure::telemetry::ActiveRatioWindow;

const DEADLINE: Duration = Duration::from_secs(30);

const HEADER: &str = "<available_skills>\nInstalled skills you can load with the activate_skill tool. When a request matches a skill's description, call activate_skill with its exact name.";

// ── Harness ─────────────────────────────────────────────────────────────────

/// Persona with a fixed, recognisable prompt.
struct FixedPersona;

impl PersonaPort for FixedPersona {
    fn system_prompt(&self, _workspace_path: &Path) -> String {
        "PERSONA".to_string()
    }
}

/// Tool catalogue with configurable names; execution never happens.
struct OfferTools {
    names: Vec<&'static str>,
}

#[async_trait]
impl ToolSetPort for OfferTools {
    fn available_tools(&self) -> Vec<ToolDefinition> {
        self.names
            .iter()
            .map(|n| ToolDefinition {
                name: n.to_string(),
                description: format!("{n} tool"),
                input_schema: serde_json::json!({"type": "object", "properties": {}}),
                parallel_safe: true,
            })
            .collect()
    }

    async fn execute(
        &self,
        _tool_name: &str,
        _input: serde_json::Value,
        _cancel: CancellationToken,
    ) -> Result<ToolResult, rustain::domain::errors::ToolError> {
        Err(rustain::domain::errors::ToolError::NotFound(
            "offer-only in this harness".to_string(),
        ))
    }
}

/// Ends the turn at once and records the system prompt of the last request.
struct PromptCapture {
    prompt: PlMutex<Option<String>>,
}

impl PromptCapture {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            prompt: PlMutex::new(None),
        })
    }

    fn captured(&self) -> String {
        self.prompt
            .lock()
            .clone()
            .expect("the turn reached the provider")
    }
}

#[async_trait]
impl StreamingProvider for PromptCapture {
    async fn stream_completion(
        &self,
        _messages: Vec<Message>,
        options: CompletionOptions,
    ) -> Result<
        std::pin::Pin<Box<dyn futures::Stream<Item = StreamChunk> + Send>>,
        rustain::domain::errors::ProviderError,
    > {
        *self.prompt.lock() = Some(options.system_prompt);
        Ok(Box::pin(futures::stream::iter(vec![
            StreamChunk::TurnComplete {
                stop_reason: StopReason::EndTurn,
            },
        ])))
    }

    async fn abort(&self) -> Result<(), rustain::domain::errors::ProviderError> {
        Ok(())
    }
    fn provider_id(&self) -> String {
        "prompt-capture".to_string()
    }
    fn list_models(&self) -> Vec<ModelDescriptor> {
        vec![]
    }
    async fn health_check(&self) -> Result<(), rustain::domain::errors::ProviderError> {
        Ok(())
    }
    async fn connectivity_probe(
        &self,
    ) -> Result<rustain::domain::ports::ProbeOutcome, rustain::domain::errors::ProviderError> {
        Ok(rustain::domain::ports::ProbeOutcome {
            latency: Duration::ZERO,
        })
    }
}

/// One SKILL.md under `<ws>/.agents/skills/<name>/`; returns the file contents.
fn write_skill(ws: &Path, name: &str, description: &str, body: &str) -> String {
    let dir = ws.join(".agents").join("skills").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    let content = format!("---\nname: {name}\ndescription: {description}\n---\n{body}");
    std::fs::write(dir.join("SKILL.md"), &content).unwrap();
    content
}

const DESC_A: &str = "Alpha skill for the FR41 keystones";
const DESC_B: &str = "Beta skill for the FR41 keystones";

/// The two-skill fixture every positive keystone shares.
fn two_workspace_skills(ws: &Path) {
    write_skill(ws, "fr41-alpha", DESC_A, "Alpha body line");
    write_skill(ws, "fr41-beta", DESC_B, "Beta body line");
}

/// A TUI front door: the production `LocalTurnDriver` over a capturing provider.
struct TuiDoor {
    ws: PathBuf,
    tools: Vec<&'static str>,
    activator: Arc<SkillActivator>,
    exposure: Option<Arc<dyn SkillExposurePort>>,
    window: u32,
}

impl TuiDoor {
    /// An activator over the workspace's discovered skills that can prompt for
    /// trust (like the TUI's), kept alive by the returned receiver.
    async fn discovered_activator(
        ws: &Path,
    ) -> (
        Arc<SkillActivator>,
        mpsc::UnboundedReceiver<rustain::domain::events::AppEvent>,
    ) {
        // Discovery sorts by name; hand the registry the discovered definitions
        // in REVERSE so the catalog's own name sort is what orders the listing
        // (kills mutant M17 by reading, C1-R1).
        let discovered = SkillRegistry::discover(ws, None, &[]);
        let mut defs = discovered.skills().to_vec();
        defs.reverse();
        let registry = SkillRegistry::from_skills(defs);
        let activator = Arc::new(SkillActivator::with_registry(Arc::new(RwLock::new(
            registry,
        ))));
        let (tx, rx) = mpsc::unbounded_channel();
        activator.set_event_tx(tx).await;
        (activator, rx)
    }

    fn l1() -> Option<Arc<dyn SkillExposurePort>> {
        Some(Arc::new(L1MetadataExposure::new(Arc::new(
            SkillCache::new_in_memory(),
        ))))
    }

    /// Submit one turn through `LocalTurnDriver::submit`; returns the system
    /// prompt the provider received.
    async fn prompt(
        &self,
        conversation_id: &str,
        activation: Option<SkillActivationSet>,
    ) -> String {
        let provider = PromptCapture::new();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let security: Arc<dyn SecurityPort> = Arc::new(NoOpSecurity);
        let tools: Arc<dyn ToolSetPort> = Arc::new(OfferTools {
            names: self.tools.clone(),
        });
        let approval = ApprovalRuntime::new(
            16,
            Arc::new(rustain::adapters::noop::NoOpApprovalPersistence),
        );
        let scheduler = ToolScheduler::new(security.clone(), tools.clone(), approval, 16);
        let driver = LocalTurnDriver::new(
            provider.clone(),
            Arc::new(ArcSwap::from_pointee(
                rustain::domain::models::AppConfig::default(),
            )),
            event_tx,
            security,
            tools,
            scheduler,
            self.activator.clone(),
            Arc::new(ArcSwap::from_pointee(
                Arc::new(FixedPersona) as Arc<dyn rustain::domain::ports::PersonaPort>
            )),
            Arc::new(ArcSwap::from_pointee(
                Arc::new(NoOpContext) as Arc<dyn rustain::domain::ports::ContextPort>
            )),
            Arc::new(ArcSwap::from_pointee(
                None as Option<Arc<dyn rustain::domain::ports::ContextAssemblerPort>>,
            )),
            Arc::new(ArcSwap::from_pointee(self.exposure.clone())),
            self.ws.clone(),
            Arc::new(FileSystemStorage::new(self.ws.join("sessions"))),
            Arc::new(NoOpStorage),
            Arc::new(rustain::domain::services::plan_mode_injector::DefaultPlanInjector::new()),
            Arc::new(NoOpUsageLedger) as Arc<dyn UsageLedgerPort>,
            ActiveRatioWindow::new_in_memory(),
        );

        let mut conversation = Conversation {
            id: conversation_id.to_string(),
            ..Default::default()
        };
        let mut streaming = StreamingState::default();
        let mut state = rustain::adapters::tui::state::TuiState::new(120, 24);
        state.active_context_window = self.window;
        let mut lifecycle =
            rustain::infrastructure::runtime::tui_turn_lifecycle::TuiTurnLifecycle::default();
        let mut session_manager = SessionManager::new(SessionState::Empty);

        driver
            .submit(
                UserSubmission {
                    text: "hello".into(),
                    images: vec![],
                    synthetic: false,
                    activation_set: activation,
                    agent_snapshot: None,
                    turn_cancel: CancellationToken::new(),
                },
                TurnViewState {
                    conversation: &mut conversation,
                    streaming: &mut streaming,
                    state: &mut state,
                    lifecycle: &mut lifecycle,
                    session_manager: &mut session_manager,
                },
            )
            .await;
        if let Some(handle) = lifecycle.take_task(&conversation.id) {
            tokio::time::timeout(DEADLINE, handle)
                .await
                .expect("turn must finish")
                .expect("turn task must not panic");
        }
        while event_rx.try_recv().is_ok() {}
        provider.captured()
    }
}

const ACTIVATE: &[&str] = &["Read", "activate_skill"];

/// Rust port of `_skill_names` in `tests_tui/fixtures/scene_provider.py` (the
/// J3 request-log parser): canonical `<skill …>` blocks count only as the
/// trailing `\n\n`-separated run of the prompt.
fn j3_skill_names(system: &str) -> Vec<String> {
    let re = regex::Regex::new(
        r#"(?ms)^<skill name="([^"]+)" source="(?:WorkspaceAgents|WorkspaceRustain|WorkspaceClaude|GlobalAgents)">\n<instructions>\n.*?\n</instructions>\n<skill_directory>.*?</skill_directory>\n<workspace_root>.*?</workspace_root>\n(?:<referenced_files>\n.*?\n</referenced_files>\n)?(?:<arguments>.*?</arguments>\n)?</skill>"#,
    )
    .unwrap();
    let matches: Vec<_> = re.captures_iter(system).collect();
    let Some(last) = matches.last() else {
        return Vec::new();
    };
    if last.get(0).unwrap().end() != system.len() {
        return Vec::new();
    }
    let mut trailing = vec![last];
    let mut cursor = last.get(0).unwrap().start();
    for m in matches[..matches.len() - 1].iter().rev() {
        let whole = m.get(0).unwrap();
        if &system[whole.end()..cursor] != "\n\n" {
            break;
        }
        trailing.push(m);
        cursor = whole.start();
    }
    trailing.iter().rev().map(|m| m[1].to_string()).collect()
}

// ── K1 — TUI lists ──────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn k1_tui_turn_lists_both_workspace_skills_in_name_order() {
    let tmp = tempfile::tempdir().unwrap();
    two_workspace_skills(tmp.path());
    let (activator, _rx) = TuiDoor::discovered_activator(tmp.path()).await;
    let door = TuiDoor {
        ws: tmp.path().to_path_buf(),
        tools: ACTIVATE.to_vec(),
        activator,
        exposure: TuiDoor::l1(),
        window: 0,
    };

    let prompt = door.prompt("conv-k1", None).await;

    let expected = format!(
        "PERSONA\n\n{HEADER}\n<skill_entry name=\"fr41-alpha\" trust=\"workspace\">{DESC_A}</skill_entry>\n<skill_entry name=\"fr41-beta\" trust=\"workspace\">{DESC_B}</skill_entry>\n</available_skills>"
    );
    assert_eq!(prompt, expected);
}

// ── K2 — TUI dedupe + suffix ────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn k2_active_skill_leaves_the_catalog_and_stays_the_exact_suffix() {
    let tmp = tempfile::tempdir().unwrap();
    two_workspace_skills(tmp.path());
    let (activator, _rx) = TuiDoor::discovered_activator(tmp.path()).await;
    let def = activator.lookup_skill("fr41-alpha").await.unwrap();
    let active = activator
        .activate(&def, String::new(), "conv-k2", 0)
        .await
        .expect("activation succeeds");
    let mut set = SkillActivationSet::new();
    set.push(active);
    let door = TuiDoor {
        ws: tmp.path().to_path_buf(),
        tools: ACTIVATE.to_vec(),
        activator,
        exposure: TuiDoor::l1(),
        window: 0,
    };

    let prompt = door.prompt("conv-k2", Some(set)).await;

    let dir = tmp.path().join(".agents/skills/fr41-alpha");
    let expected = format!(
        "PERSONA\n\n{HEADER}\n<skill_entry name=\"fr41-beta\" trust=\"workspace\">{DESC_B}</skill_entry>\n</available_skills>\n\n<skill name=\"fr41-alpha\" source=\"WorkspaceAgents\">\n<instructions>\nAlpha body line\n</instructions>\n<skill_directory>{}</skill_directory>\n<workspace_root>{}</workspace_root>\n</skill>",
        dir.display(),
        tmp.path().display()
    );
    assert_eq!(prompt, expected);
    assert_eq!(j3_skill_names(&prompt), vec!["fr41-alpha"]);
}

// ── K3 — gate and None ──────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn k3_no_activate_skill_tool_or_no_port_leaves_the_prompt_untouched() {
    let tmp = tempfile::tempdir().unwrap();
    two_workspace_skills(tmp.path());

    // No `activate_skill` offered and no filter to force-add it.
    let (activator, _rx) = TuiDoor::discovered_activator(tmp.path()).await;
    let gated = TuiDoor {
        ws: tmp.path().to_path_buf(),
        tools: vec!["Read"],
        activator,
        exposure: TuiDoor::l1(),
        window: 0,
    };
    assert_eq!(gated.prompt("conv-k3a", None).await, "PERSONA");

    // `activate_skill` offered but the exposure slot is empty (headless shape).
    let (activator, _rx2) = TuiDoor::discovered_activator(tmp.path()).await;
    let no_port = TuiDoor {
        ws: tmp.path().to_path_buf(),
        tools: ACTIVATE.to_vec(),
        activator,
        exposure: None,
        window: 0,
    };
    assert_eq!(no_port.prompt("conv-k3b", None).await, "PERSONA");
}

// ── K6 — daemon absence ─────────────────────────────────────────────────────

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn k6_daemon_turn_carries_no_catalog_even_with_skills_on_disk() {
    use rustain::adapters::daemon::runtime::DaemonTurnRuntime;

    let tmp = tempfile::tempdir().unwrap();
    two_workspace_skills(tmp.path());
    let provider = PromptCapture::new();
    let security: Arc<dyn SecurityPort> = Arc::new(NoOpSecurity);
    let tools: Arc<dyn ToolSetPort> = Arc::new(OfferTools {
        names: ACTIVATE.to_vec(),
    });
    let approval = ApprovalRuntime::new(
        16,
        Arc::new(rustain::adapters::noop::NoOpApprovalPersistence),
    );
    let scheduler = ToolScheduler::new(security.clone(), tools.clone(), approval.clone(), 16);
    let runtime = DaemonTurnRuntime {
        recipient_context: Arc::new(Default::default()),
        provider: provider.clone(),
        app_config: Arc::new(ArcSwap::from_pointee(
            rustain::domain::models::AppConfig::default(),
        )),
        security,
        tools,
        tool_scheduler: scheduler,
        persona: Arc::new(FixedPersona),
        context_assembler: Arc::new(ArcSwap::from_pointee(
            None as Option<Arc<dyn rustain::domain::ports::ContextAssemblerPort>>,
        )),
        context: Arc::new(ArcSwap::from_pointee(
            Arc::new(NoOpContext) as Arc<dyn rustain::domain::ports::ContextPort>
        )),
        storage: Arc::new(NoOpStorage),
        fs_storage: Arc::new(FileSystemStorage::new(tmp.path().join("sessions"))),
        usage_ledger: Arc::new(NoOpUsageLedger) as Arc<dyn UsageLedgerPort>,
        telemetry: ActiveRatioWindow::new_in_memory(),
        plan_injector: Arc::new(
            rustain::domain::services::plan_mode_injector::DefaultPlanInjector::new(),
        ),
        approval,
        workspace: tmp.path().to_path_buf(),
        #[cfg(feature = "mcp")]
        mcp_task_runtimes: Vec::new(),
    };
    let mut conversation = Conversation {
        id: "k6-daemon".into(),
        ..Default::default()
    };
    let (domain_tx, _domain_rx) = mpsc::unbounded_channel();
    let handle = runtime
        .drive_turn(
            "hello".into(),
            rustain::domain::models::ChannelKind::Terminal,
            &mut conversation,
            &domain_tx,
            rustain::domain::models::TurnOrigin::Interactive,
            CancellationToken::new(),
        )
        .await;
    tokio::time::timeout(DEADLINE, handle)
        .await
        .expect("daemon turn must finish")
        .expect("daemon turn must not panic");

    assert_eq!(provider.captured(), "PERSONA");
}

// ── K7 — cap from the window ────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn k7_cap_follows_the_selected_models_context_window() {
    let tmp = tempfile::tempdir().unwrap();
    // Each entry ≈ 59 estimated tokens: (9 + 170) / 4 + 15. Window 5_000 → cap 100.
    let long = "d".repeat(170);
    write_skill(tmp.path(), "fr41-k7-a", &long, "a");
    write_skill(tmp.path(), "fr41-k7-b", &long, "b");
    let (activator, _rx) = TuiDoor::discovered_activator(tmp.path()).await;
    let door = TuiDoor {
        ws: tmp.path().to_path_buf(),
        tools: ACTIVATE.to_vec(),
        activator,
        exposure: TuiDoor::l1(),
        window: 5_000,
    };

    let prompt = door.prompt("conv-k7", None).await;

    let expected = format!(
        "PERSONA\n\n{HEADER}\n<skill_entry name=\"fr41-k7-a\" trust=\"workspace\">{long}</skill_entry>\n<omitted count=\"1\">1 more installed skills are not listed.</omitted>\n</available_skills>"
    );
    assert_eq!(prompt, expected);
}

// ── K8 — static-full trust gating ───────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn k8_static_full_shows_a_workspace_body_only_after_trust() {
    let tmp = tempfile::tempdir().unwrap();
    let file_text = write_skill(tmp.path(), "fr41-trust", DESC_A, "Trusted body line");
    let registry = SkillRegistry::discover(tmp.path(), None, &[]);
    let file = registry.find("fr41-trust").unwrap().file.clone();
    let cache = Arc::new(SkillCache::new_in_memory());
    cache.populate_from_registry(&registry).await;
    let activator = Arc::new(SkillActivator::with_registry(Arc::new(RwLock::new(
        registry,
    ))));
    let (tx, _rx) = mpsc::unbounded_channel();
    activator.set_event_tx(tx).await;
    let door = TuiDoor {
        ws: tmp.path().to_path_buf(),
        tools: ACTIVATE.to_vec(),
        activator: activator.clone(),
        exposure: Some(Arc::new(StaticFullExposure::new(cache))),
        window: 0,
    };

    let untrusted = door.prompt("conv-k8", None).await;
    assert_eq!(
        untrusted,
        format!(
            "PERSONA\n\n{HEADER}\n<skill_entry name=\"fr41-trust\" trust=\"workspace\">{DESC_A}</skill_entry>\n</available_skills>"
        )
    );

    activator.mark_trusted("conv-k8", file).await;
    let trusted = door.prompt("conv-k8", None).await;
    assert_eq!(
        trusted,
        format!(
            "PERSONA\n\n{HEADER}\n<skill_entry name=\"fr41-trust\" trust=\"workspace\">{DESC_A}\n<instructions>\n{file_text}\n</instructions>\n</skill_entry>\n</available_skills>"
        )
    );
}
