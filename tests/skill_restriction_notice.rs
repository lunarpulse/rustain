//! Story 19.11 — skill command patterns are offered only when an execution
//! gate can honour them, while genuinely unavailable declarations remain
//! visible under FR42-a.
//!
//! # Front door
//!
//! Every keystone here drives `LocalTurnDriver::submit` with a real
//! `SkillRegistry` parse of a SKILL.md written to disk and a real
//! `SkillActivator` activation. Tests of agent-only policy follow the existing
//! `ActiveAgent` snapshot precedent because agent discovery is not their
//! subject. No test calls the turn filter or disclosure branch directly.
//!
//! The observable contracts are the provider's offered tool names and the
//! typed system notices emitted for that same turn.

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
        Arc::new(SkillActivator::new()),
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

// ── Story 19.11 — skill patterns are honourable at offer time ───────────────

#[tokio::test(flavor = "multi_thread")]
async fn ac2_prd_skill_patterns_offer_bash_without_disclosure() {
    let tmp = tempfile::tempdir().unwrap();
    write_skill(tmp.path(), "safe-deploy", PRD_J3_SKILL_MD);
    let activation = activation_from_skill_md(tmp.path(), "safe-deploy").await;

    let (notices, offered) = drive_turn(tmp.path(), vec!["Read", "Bash"], Some(activation)).await;

    assert!(
        !notices
            .iter()
            .any(|(level, _)| *level == NoticeLevel::Advisory),
        "honourable skill patterns must not be reported unavailable: {notices:?}"
    );
    assert_eq!(offered, vec!["Bash", "Read", "activate_skill"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn ac2_pattern_only_restriction_offers_bash_and_carveouts() {
    let tmp = tempfile::tempdir().unwrap();
    write_skill(
        tmp.path(),
        "pattern-only",
        "---\nname: pattern-only\ndescription: Test skill\nallowed-tools: Bash(kubectl:*) Bash(helm:*)\n---\n# Body\n",
    );
    let activation = activation_from_skill_md(tmp.path(), "pattern-only").await;

    let (notices, offered) =
        drive_turn(tmp.path(), vec!["Read", "Bash", "task"], Some(activation)).await;

    assert_eq!(
        offered,
        vec!["Bash", "activate_skill", "task"],
        "the enforced Bash tool and both carve-outs must be offered"
    );
    assert!(
        !notices
            .iter()
            .any(|(level, _)| *level == NoticeLevel::Advisory),
        "honourable skill patterns must not be disclosed: {notices:?}"
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

/// Finding 8: computing the disclosure over the post-intersection set let an
/// unmatchable item vanish. An agent that declares only `exclude-tools` yields
/// a catalogue-derived filter; `Glob` must remain visible even though the
/// intersection still contains the skill's honourable `Read` item.
#[tokio::test(flavor = "multi_thread")]
async fn review_exclude_only_agent_cannot_hide_an_unmatchable_skill_item() {
    let tmp = tempfile::tempdir().unwrap();
    write_skill(
        tmp.path(),
        "deployer",
        "---\nname: deployer\ndescription: Test skill\nallowed-tools: Glob Read\n---\n# Body\n",
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

    let (notices, offered) = drive_turn_with_agent(
        tmp.path(),
        vec!["Read", "Bash", "Write"],
        Some(activation),
        Some(agent),
    )
    .await;

    let disclosures: Vec<&String> = notices
        .iter()
        .filter(|(level, _)| *level == NoticeLevel::Advisory)
        .map(|(_, message)| message)
        .collect();
    assert_eq!(
        disclosures.len(),
        1,
        "the intersection must not swallow the disclosure: {notices:?}"
    );
    let disclosure = disclosures[0];
    assert!(
        disclosure.contains("Glob") && !disclosure.contains("Read"),
        "only the unavailable declaration must be named: {disclosure}"
    );
    assert!(
        !disclosure.contains("failed validation")
            && !disclosure.contains("invalid")
            && !disclosure.contains("failed to load"),
        "the disclosure must not claim the skill failed: {disclosure}"
    );
    assert!(
        !notices
            .iter()
            .any(|(_, message)| message.contains("disjoint"))
    );
    assert!(notices.iter().all(|(level, _)| !level.is_turn_fatal()));
    assert_eq!(offered, vec!["Read", "activate_skill"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn ac5_agent_pattern_stays_unoffered_and_disclosed() {
    let tmp = tempfile::tempdir().unwrap();
    let agent = rustain::domain::models::ActiveAgent {
        name: "ops".to_string(),
        file: tmp.path().join("ops.md"),
        body: String::new(),
        allowed_tools: Some(vec!["Bash(kubectl:*)".to_string()]),
        exclude_tools: None,
        model: None,
    };

    let (notices, offered) =
        drive_turn_with_agent(tmp.path(), vec!["Read", "Bash", "task"], None, Some(agent)).await;

    let disclosures: Vec<&String> = notices
        .iter()
        .filter(|(level, _)| *level == NoticeLevel::Advisory)
        .map(|(_, message)| message)
        .collect();
    assert_eq!(disclosures.len(), 1, "agent pattern must stay visible");
    assert!(disclosures[0].contains("Bash(kubectl:*)"));
    assert_eq!(offered, vec!["activate_skill", "task"]);
}

// ── Story 19.11 code review — the unruled axes stay honest ──────────────────

/// Review patch (P3): a skill whose every declared item is unmatchable in
/// this catalogue (e.g. `allowed-tools: Glob` with no Glob tool in the
/// build) takes the FR42-a disclosure branch — as it did before pattern
/// expansion — not the turn-fatal disjoint branch with its misleading
/// "agent and skill filters are disjoint" message.
#[tokio::test(flavor = "multi_thread")]
async fn review_all_unmatchable_skill_discloses_and_is_not_turn_fatal() {
    let tmp = tempfile::tempdir().unwrap();
    write_skill(
        tmp.path(),
        "ghost-tools",
        "---\nname: ghost-tools\ndescription: Declares a tool this build does not carry.\nallowed-tools: Glob\n---\n# Body\n",
    );
    let activation = activation_from_skill_md(tmp.path(), "ghost-tools").await;

    let (notices, offered) =
        drive_turn(tmp.path(), vec!["Read", "Bash", "task"], Some(activation)).await;

    assert!(
        !notices.iter().any(|(level, _)| level.is_turn_fatal()),
        "a solo unmatchable skill must not kill the turn: {notices:?}"
    );
    assert!(
        notices.iter().any(|(level, message)| {
            *level == NoticeLevel::Advisory
                && message.contains("[Glob]")
                && message.contains("unavailable")
                && !message.contains("disjoint")
        }),
        "the unmatchable item is disclosed on the turn it bites: {notices:?}"
    );
    assert_eq!(
        offered,
        vec!["activate_skill", "task"],
        "nothing beyond the carve-outs is offered"
    );
}

/// Review patch (P2): a non-Bash pattern has no execution-time command
/// gate, so it must not be widened to the bare tool at offer time. The
/// enforced Bash pattern still offers Bash; `Read(docs/*)` stays unoffered
/// and is disclosed as unmatched — instead of offering a Read tool that
/// every call would deny while the disclosure stays silent.
#[tokio::test(flavor = "multi_thread")]
async fn review_non_bash_pattern_stays_unoffered_and_disclosed() {
    let tmp = tempfile::tempdir().unwrap();
    write_skill(
        tmp.path(),
        "mixed-patterns",
        "---\nname: mixed-patterns\ndescription: One enforced and one unenforceable pattern.\nallowed-tools: Bash(kubectl:*) Read(docs/*)\n---\n# Body\n",
    );
    let activation = activation_from_skill_md(tmp.path(), "mixed-patterns").await;

    let (notices, offered) =
        drive_turn(tmp.path(), vec!["Read", "Bash", "task"], Some(activation)).await;

    assert_eq!(
        offered,
        vec!["Bash", "activate_skill", "task"],
        "the enforced Bash pattern offers Bash; the ungated Read pattern must not offer Read"
    );
    let disclosures: Vec<&String> = notices
        .iter()
        .filter(|(level, _)| *level == NoticeLevel::Advisory)
        .map(|(_, message)| message)
        .collect();
    assert_eq!(disclosures.len(), 1, "exactly one disclosure: {notices:?}");
    assert!(
        disclosures[0].contains("Read(docs/*)"),
        "the ungated pattern is named: {disclosures:?}"
    );
    assert!(!disclosures[0].contains("Bash(kubectl:*)"));
}
