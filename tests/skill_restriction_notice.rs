//! Story 19.2 — a tool restriction this build cannot honour is disclosed on
//! the turn it bites (FR42-a, AC3/AC4).
//!
//! # Front door
//!
//! Every keystone here drives `LocalTurnDriver::submit` — the production
//! turn-origination door (`turn_driver.rs:202`, the relocated
//! `start_turn_inner`) — with a real `SkillRegistry` parse of a SKILL.md
//! written to disk and a real `SkillActivator` activation. ⛔ No keystone
//! fabricates an `ActiveSkill` by hand or calls the notice branch directly:
//! the notice must be reached through the same `submit` path the event loop
//! uses, or it proves nothing about the turn.
//!
//! The needles asserted (`Bash(helm:*)`, `Bash(kubectl:*)`) come from the
//! fixture frontmatter, and the discriminating assertion is an *absence*
//! (`Read` must not be named) — never a full sentence the product composed
//! about itself (the 18-4e rule).

use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use parking_lot::Mutex as PlMutex;
use rustain::adapters::filesystem::FileSystemStorage;
use rustain::adapters::noop::{NoOpContext, NoOpPersona, NoOpSecurity, NoOpStorage};
use rustain::adapters::skill_activation::SkillActivator;
use rustain::adapters::skill_registry::SkillRegistry;
use rustain::domain::events::AppEvent;
use rustain::domain::models::{
    CompletionOptions, Conversation, Message, ModelDescriptor, NoticeLevel, SessionManager,
    SessionState, SkillActivationSet, StopReason, StreamChunk, StreamingState, ToolDefinition,
    ToolResult,
};
use rustain::domain::ports::{SecurityPort, StreamingProvider, ToolSetPort, UsageLedgerPort};
use rustain::domain::services::approval_runtime::ApprovalRuntime;
use rustain::domain::services::tool_scheduler::ToolScheduler;
use rustain::infrastructure::runtime::turn_driver::{
    LocalTurnDriver, TurnViewState, UserSubmission,
};
use rustain::infrastructure::telemetry::ActiveRatioWindow;

const DEADLINE: Duration = Duration::from_secs(30);

// ── Harness ─────────────────────────────────────────────────────────────────

/// A tool catalogue with configurable names; execution never happens (the
/// scripted provider ends the turn without tool use).
struct CatalogueTools {
    names: Vec<&'static str>,
}

#[async_trait]
impl ToolSetPort for CatalogueTools {
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
            "catalogue is offer-only in this harness".to_string(),
        ))
    }
}

/// Ends the turn immediately and captures the tool definitions the provider
/// was offered — the offer-time observable.
struct OfferCapturingProvider {
    offered: PlMutex<Vec<String>>,
}

#[async_trait]
impl StreamingProvider for OfferCapturingProvider {
    async fn stream_completion(
        &self,
        _messages: Vec<Message>,
        options: CompletionOptions,
    ) -> Result<
        std::pin::Pin<Box<dyn futures::Stream<Item = StreamChunk> + Send>>,
        rustain::domain::errors::ProviderError,
    > {
        let mut names: Vec<String> = options.tools.iter().map(|t| t.name.clone()).collect();
        names.sort();
        *self.offered.lock() = names;
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
        "offer-capture".to_string()
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

/// Parse a SKILL.md from disk through the real registry, activate it through
/// the real activator, and hand back the activation set the event loop would
/// carry on a submission.
async fn activation_from_skill_md(ws: &std::path::Path, rel: &str) -> SkillActivationSet {
    let registry = SkillRegistry::discover(ws, None, &[]);
    let def = registry
        .find(rel)
        .unwrap_or_else(|| panic!("{rel} must be discovered"))
        .clone();
    let activator = SkillActivator::new();
    activator.on_new_conversation("conv-19-2").await;
    let active = activator
        .activate(&def, String::new(), "conv-19-2", 0)
        .await
        .expect("activation succeeds");
    let mut set = SkillActivationSet::new();
    set.push(active);
    set
}

/// Drive the production turn door with the given catalogue and activation
/// set. Returns (system notices with their level, offered tool names).
async fn drive_turn(
    ws: &std::path::Path,
    catalogue: Vec<&'static str>,
    activation: Option<SkillActivationSet>,
) -> (Vec<(NoticeLevel, String)>, Vec<String>) {
    drive_turn_with_agent(ws, catalogue, activation, None).await
}

/// As `drive_turn`, but the turn also carries an active agent — the
/// configuration the code review found could hide an unmatchable item.
async fn drive_turn_with_agent(
    ws: &std::path::Path,
    catalogue: Vec<&'static str>,
    activation: Option<SkillActivationSet>,
    agent: Option<rustain::domain::models::ActiveAgent>,
) -> (Vec<(NoticeLevel, String)>, Vec<String>) {
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let security: Arc<dyn SecurityPort> = Arc::new(NoOpSecurity);
    let tools: Arc<dyn ToolSetPort> = Arc::new(CatalogueTools { names: catalogue });
    let approval = ApprovalRuntime::new(
        16,
        Arc::new(rustain::adapters::noop::NoOpApprovalPersistence),
    );
    let scheduler = ToolScheduler::new(security.clone(), tools.clone(), approval, 16);
    let provider = Arc::new(OfferCapturingProvider {
        offered: PlMutex::new(Vec::new()),
    });

    let driver = LocalTurnDriver::new(
        provider.clone(),
        Arc::new(ArcSwap::from_pointee(
            rustain::domain::models::AppConfig::default(),
        )),
        event_tx,
        security,
        tools,
        scheduler,
        Arc::new(NoOpPersona),
        Arc::new(ArcSwap::from_pointee(
            Arc::new(NoOpContext) as Arc<dyn rustain::domain::ports::ContextPort>
        )),
        Arc::new(ArcSwap::from_pointee(
            None as Option<Arc<dyn rustain::domain::ports::ContextAssemblerPort>>,
        )),
        ws.to_path_buf(),
        Arc::new(FileSystemStorage::new(ws.join("sessions"))),
        Arc::new(NoOpStorage),
        Arc::new(rustain::domain::services::plan_mode_injector::DefaultPlanInjector::new()),
        Arc::new(rustain::adapters::noop::NoOpUsageLedger) as Arc<dyn UsageLedgerPort>,
        ActiveRatioWindow::new_in_memory(),
    );

    let mut conversation = Conversation {
        id: rustain::domain::models::generate_conversation_id(),
        title: String::new(),
        messages: vec![],
        turns: Vec::new(),
        created_at: 0,
        updated_at: 0,
        last_response_at: None,
        session_id: None,
        usage: None,
        plans: std::collections::HashMap::new(),
        fork_source: None,
        compaction: None,
    };
    let mut streaming = StreamingState::default();
    let mut state = rustain::adapters::tui::state::TuiState::new(120, 24);
    let mut active_turn = None;
    let mut session_manager = SessionManager::new(SessionState::Empty);
    let cancel = CancellationToken::new();

    driver
        .submit(
            UserSubmission {
                text: "deploy billing to production".into(),
                images: vec![],
                synthetic: false,
                activation_set: activation,
                agent_snapshot: agent,
                turn_cancel: cancel,
            },
            TurnViewState {
                conversation: &mut conversation,
                streaming: &mut streaming,
                state: &mut state,
                active_turn: &mut active_turn,
                session_manager: &mut session_manager,
            },
        )
        .await;

    // Let the spawned turn finish so no event races the assertions.
    if let Some(handle) = active_turn.take() {
        let _ = tokio::time::timeout(DEADLINE, handle).await;
    }

    let mut notices = Vec::new();
    while let Ok(event) = event_rx.try_recv() {
        if let AppEvent::SystemNotice { level, message, .. } = event {
            notices.push((level, message));
        }
    }
    let offered = provider.offered.lock().clone();
    (notices, offered)
}

/// The PRD § Journey 3 SKILL.md, byte-exact (see prd.md, the fenced block).
const PRD_J3_SKILL_MD: &str = "---\nname: safe-deploy\ndescription: Deploy services following team safety protocols. Use when deploying any service to staging or production.\nallowed-tools: Bash(kubectl:*) Bash(helm:*) Read\n---\n## Protocol\n1. Read deploy.yaml for environment config\n2. Run helm diff to preview changes — show diff to user\n3. Check migrations/ for pending changes — if breaking, STOP and warn\n4. If staging: deploy and run smoke tests\n5. If production: require explicit user confirmation BEFORE deploying\n6. Run smoke tests post-deploy. If any fail, auto-rollback.\nNever skip smoke tests. Never force-push to production.\n";

fn write_skill(ws: &std::path::Path, name: &str, content: &str) {
    let dir = ws.join(".agents").join("skills").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("SKILL.md"), content).unwrap();
}

// ── AC3 — Marco's file does not lose `Bash` in silence ─────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn ac3_prd_skill_unmatched_items_disclosed_not_silent() {
    let tmp = tempfile::tempdir().unwrap();
    write_skill(tmp.path(), "safe-deploy", PRD_J3_SKILL_MD);
    let activation = activation_from_skill_md(tmp.path(), "safe-deploy").await;

    // Catalogue contains Read and Bash but neither pattern item (AC3 given).
    let (notices, offered) = drive_turn(tmp.path(), vec!["Read", "Bash"], Some(activation)).await;

    let warnings: Vec<&String> = notices
        .iter()
        .filter(|(level, _)| *level == NoticeLevel::Advisory)
        .map(|(_, msg)| msg)
        .collect();
    assert_eq!(
        warnings.len(),
        1,
        "exactly one Advisory disclosure for the turn; got {notices:?}"
    );
    let message = warnings[0];
    assert!(
        message.contains("Bash(helm:*)") && message.contains("Bash(kubectl:*)"),
        "the notice must name both pattern items: {message}"
    );
    assert!(
        !message.contains("Read"),
        "Read is honoured — it must not be named: {message}"
    );
    assert!(
        !message.contains("failed validation")
            && !message.contains("invalid")
            && !message.contains("failed to load"),
        "the notice must not claim the skill failed: {message}"
    );
    assert!(
        !notices.iter().any(|(_, m)| m.contains("disjoint")),
        "the disjoint notice must not fire — the declared set is non-empty: {notices:?}"
    );
    // Code review: the disclosure must NOT be turn-fatal. A `Warning` here made
    // the TUI consumer abort the very turn this notice describes, so no notice
    // emitted by this path may carry a turn-fatal level.
    assert!(
        notices.iter().all(|(level, _)| !level.is_turn_fatal()),
        "a disclosure must never end the turn it describes: {notices:?}"
    );

    // Offer-time consequence: Read is honoured, Bash is filtered out, and the
    // skill-chaining carve-out is force-added. Nothing else is offered.
    let mut offered = offered;
    offered.sort();
    assert_eq!(offered, vec!["Read", "activate_skill"]);
}

// ── AC4 — a pattern-only restriction fails closed, loudly ──────────────────

#[tokio::test(flavor = "multi_thread")]
async fn ac4_pattern_only_restriction_offers_only_carveouts_and_warns() {
    let tmp = tempfile::tempdir().unwrap();
    write_skill(
        tmp.path(),
        "pattern-only",
        "---\nname: pattern-only\ndescription: Test skill\nallowed-tools: Bash(kubectl:*) Bash(helm:*)\n---\n# Body\n",
    );
    let activation = activation_from_skill_md(tmp.path(), "pattern-only").await;

    let (notices, offered) =
        drive_turn(tmp.path(), vec!["Read", "Bash", "task"], Some(activation)).await;

    // (a) exactly the two carve-outs are offered — nothing else survives.
    assert_eq!(
        offered,
        vec!["activate_skill", "task"],
        "a pattern-only allowlist must offer exactly the carve-outs"
    );

    // (b) the unmatched-item notice fires, naming both items.
    let warnings: Vec<&String> = notices
        .iter()
        .filter(|(level, _)| *level == NoticeLevel::Advisory)
        .map(|(_, msg)| msg)
        .collect();
    assert_eq!(warnings.len(), 1, "exactly one Advisory; got {notices:?}");
    assert!(
        warnings[0].contains("Bash(helm:*)") && warnings[0].contains("Bash(kubectl:*)"),
        "both pattern items must be named: {}",
        warnings[0]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn ac4_pattern_only_restriction_denies_task_at_execution_time() {
    // (c) the fail-closed pin (A6): `task` is OFFERED by the driver carve-out
    // but the permission chain DENIES it — a restricted skill cannot escape
    // through delegation. If either enforcer's carve-out list changes, this
    // breaks (DF-19-2-TASK-CARVEOUT-DIVERGENCE).
    use rustain::domain::services::permission_chain;
    let tmp = tempfile::tempdir().unwrap();
    write_skill(
        tmp.path(),
        "pattern-only",
        "---\nname: pattern-only\ndescription: Test skill\nallowed-tools: Bash(kubectl:*) Bash(helm:*)\n---\n# Body\n",
    );
    let registry = SkillRegistry::discover(tmp.path(), None, &[]);
    let def = registry.find("pattern-only").unwrap().clone();
    let activator = SkillActivator::new();
    activator.on_new_conversation("conv-pin").await;
    activator
        .activate(&def, String::new(), "conv-pin", 0)
        .await
        .unwrap();
    let snap = activator.snapshot_for_turn("conv-pin").await.unwrap();
    let skills: Option<&[rustain::domain::models::ActiveSkill]> = Some(snap.active_skills());
    let security = NoOpSecurity;
    let decision = permission_chain::check(
        &security,
        "task",
        &serde_json::json!({"prompt": "do a thing"}),
        skills,
        None,
        &rustain::adapters::noop::NoOpToolSet,
    )
    .await;
    assert!(
        matches!(
            decision,
            rustain::domain::services::permission_chain::PermissionDecision::Deny(_)
        ),
        "task must be DENIED at execution time under a pattern-only allowlist"
    );
}

// ── Code-review regressions (2026-08-29) ───────────────────────────────────

/// Finding 3: `unmatched` was computed against the catalogue captured BEFORE
/// the driver force-adds `activate_skill`, so a skill that declares the
/// carve-out was told it is unavailable — in the same turn that offers it.
/// Reachable in production: `toolset_adapter.rs` drops `activate_skill` from
/// the catalogue when `expose_activate_skill` is false.
#[tokio::test(flavor = "multi_thread")]
async fn review_declared_activate_skill_is_never_reported_unavailable() {
    let tmp = tempfile::tempdir().unwrap();
    write_skill(
        tmp.path(),
        "chainer",
        "---\nname: chainer\ndescription: Test skill\nallowed-tools: activate_skill Read\n---\n# Body\n",
    );
    let activation = activation_from_skill_md(tmp.path(), "chainer").await;

    // Catalogue omits `activate_skill` — the driver force-adds it.
    let (notices, offered) = drive_turn(tmp.path(), vec!["Read"], Some(activation)).await;

    assert!(
        !notices.iter().any(|(_, m)| m.contains("activate_skill")),
        "a force-added carve-out must never be named unavailable: {notices:?}"
    );
    assert!(
        offered.contains(&"activate_skill".to_string()),
        "the turn offers activate_skill, so nothing may call it unavailable: {offered:?}"
    );
}

/// Finding 8: computing the disclosure over the POST-INTERSECTION set let an
/// unmatchable item vanish. An agent that declares only `exclude-tools` yields
/// a catalogue-derived filter, and intersecting it with the skill's pattern
/// item dropped that item before it could be disclosed — FR42-a's silence,
/// one layer down.
#[tokio::test(flavor = "multi_thread")]
async fn review_exclude_only_agent_cannot_hide_an_unmatchable_skill_item() {
    let tmp = tempfile::tempdir().unwrap();
    write_skill(
        tmp.path(),
        "deployer",
        "---\nname: deployer\ndescription: Test skill\nallowed-tools: Bash(kubectl:*) Read\n---\n# Body\n",
    );
    let activation = activation_from_skill_md(tmp.path(), "deployer").await;
    let agent = rustain::domain::models::ActiveAgent {
        name: "ops".to_string(),
        file: tmp.path().join("ops.md"),
        body: String::new(),
        allowed_tools: None,
        exclude_tools: Some(vec!["Write".to_string()]),
        model: None,
    };

    let (notices, _offered) = drive_turn_with_agent(
        tmp.path(),
        vec!["Read", "Bash", "Write"],
        Some(activation),
        Some(agent),
    )
    .await;

    let disclosures: Vec<&String> = notices
        .iter()
        .filter(|(level, _)| *level == NoticeLevel::Advisory)
        .map(|(_, msg)| msg)
        .collect();
    assert_eq!(
        disclosures.len(),
        1,
        "the intersection must not swallow the disclosure: {notices:?}"
    );
    assert!(
        disclosures[0].contains("Bash(kubectl:*)"),
        "the unmatchable item must still be named: {}",
        disclosures[0]
    );
}
