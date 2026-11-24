//! Story C-2 AC6 — a profile switch, a `/config reload` and an adapter override
//! reach the NEXT TURN's request `system` prompt.
//!
//! # Front doors
//!
//! Everything is driven through the production handlers
//! (`handle_profile_switch_requested`, `handle_config_reload_with_two_pass`,
//! `handle_apply_adapter_override`) and observed on the wire seam the existing
//! `LocalTurnDriver` conformance tests use: a recording `StreamingProvider`
//! behind a production `LocalTurnDriver::submit`; the observable is
//! `CompletionOptions::system_prompt` of the provider call. No test asserts a
//! slot's content instead of a turn's request. The `LocalTurnDriver` is built
//! ONCE, before any switch/reload/override, exactly as the event loop holds it
//! for the whole session; a driver rebuilt after the action would mask AD-12.
//!
//! The agent is activated through the production path
//! (`AgentRegistry::discover` + `AgentActivator::activate`) that
//! `tests/custom_agent_invocation.rs` uses; the resulting `ActiveAgent` is the
//! turn's `agent_snapshot`, as the event loop hands it to the driver.
//!
//! # What goes RED at the base, and why (see the story's AD-12..AD-14)
//!
//! * AD-12 live slot: `LocalTurnDriver` is handed the startup persona `Arc`
//!   (see the `C2-LIVE-SLOT` line below), so a swap never reaches a turn.
//! * AD-13 switcher: the target profile is resolved from `dirs::config_dir()`
//!   (not `compose_snapshot.config_dir`), and `TransitionPlan::from_selections`
//!   diffs persona by adapter name only.
//! * AD-14 reload: the profiles directory is `paths::config_dir()` (not
//!   `compose_snapshot.config_dir`), a failing profile reports success, and
//!   config/resolver are stored before the re-compose.
//! * C-2 itself: the persona file body reaches the prompt at all (every
//!   "starts with the body" assertion).
//!
//! Hermeticity: no process-environment mutation; the config root is a tempdir
//! carried by `ComposeContext::config_dir`. The reload handler additionally
//! reads the user-global config layer and `<cwd>/.claude/rustain-settings.json`
//! through the config figment (pre-existing, the `--config-file` flag points the
//! workspace layer at the tempdir); those layers cannot change `system`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use async_trait::async_trait;
use clap::Parser;
use serial_test::serial;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use rustain::adapters::agent_activation::AgentActivator;
use rustain::adapters::agent_registry::AgentRegistry;
use rustain::adapters::cli::commands::Cli;
use rustain::adapters::filesystem::FileSystemStorage;
use rustain::adapters::noop::{NoOpContext, NoOpSecurity, NoOpStorage, NoOpToolSet};
use rustain::adapters::profile_resolver::toml_resolver::TomlProfileResolver;
use rustain::adapters::skill_activation::SkillActivator;
use rustain::adapters::tui::handlers::HandlerOutcome;
use rustain::adapters::tui::handlers::adapter_override::{
    handle_apply_adapter_override, handle_clear_adapter_override,
};
use rustain::adapters::tui::handlers::config::{ReloadContext, handle_config_reload_with_two_pass};
use rustain::adapters::tui::handlers::profile_switch::handle_profile_switch_requested;
use rustain::adapters::tui::state::TuiState;
use rustain::domain::errors::ProviderError;
use rustain::domain::events::AppEvent;
use rustain::domain::models::{
    ActiveAgent, AdapterRef, AppConfig, CompletionOptions, Conversation, Message, ModelDescriptor,
    PortDimension, SessionManager, SessionState, StopReason, StreamChunk, StreamingState,
};
use rustain::domain::ports::{
    ConfigStorePort, ContextAssemblerPort, ContextPort, PersonaPort, ProfileResolver, SecurityPort,
    StreamingProvider, ToolSetPort, UsageLedgerPort,
};
use rustain::domain::services::approval_runtime::ApprovalRuntime;
use rustain::domain::services::swap_tier::TransitionPlan;
use rustain::domain::services::tool_scheduler::ToolScheduler;
use rustain::infrastructure::composition::ComposeContext;
use rustain::infrastructure::runtime::agent_core::AgentCore;
use rustain::infrastructure::runtime::turn_driver::{
    LocalTurnDriver, TurnViewState, UserSubmission,
};
use rustain::infrastructure::telemetry::ActiveRatioWindow;

const DEADLINE: Duration = Duration::from_secs(30);

const A_BODY: &str = "C2-SWITCH-A-BODY: you are profile a, the first persona.";
const A_BODY_EDITED: &str = "C2-SWITCH-A-BODY-EDITED: profile a after the edit.";
const B_BODY: &str = "C2-SWITCH-B-BODY: you are profile b, the second persona.";
const SHARED_BODY: &str = "C2-SWITCH-SHARED-BODY: both profiles name this file.";
const AGENT_BODY: &str = "C2-SWITCH-AGENT-BODY: you are the reviewer agent.";

// ── Recording provider (the observable) ─────────────────────────────────────

/// Ends every turn immediately and records the request `system` prompt.
struct RecordingProvider {
    systems: std::sync::Mutex<Vec<String>>,
}

#[async_trait]
impl StreamingProvider for RecordingProvider {
    async fn stream_completion(
        &self,
        _messages: Vec<Message>,
        options: CompletionOptions,
    ) -> Result<std::pin::Pin<Box<dyn futures::Stream<Item = StreamChunk> + Send>>, ProviderError>
    {
        self.systems.lock().unwrap().push(options.system_prompt);
        Ok(Box::pin(futures::stream::iter(vec![
            StreamChunk::TurnComplete {
                stop_reason: StopReason::EndTurn,
            },
        ])))
    }

    async fn abort(&self) -> Result<(), ProviderError> {
        Ok(())
    }
    fn provider_id(&self) -> String {
        "c2-recording".to_string()
    }
    fn list_models(&self) -> Vec<ModelDescriptor> {
        vec![]
    }
    async fn health_check(&self) -> Result<(), ProviderError> {
        Ok(())
    }
    async fn connectivity_probe(
        &self,
    ) -> Result<rustain::domain::ports::ProbeOutcome, ProviderError> {
        Ok(rustain::domain::ports::ProbeOutcome {
            latency: Duration::ZERO,
        })
    }
}

/// The config store the reload handler writes to, sharing the driver's slot.
struct SharedConfigStore(Arc<ArcSwap<AppConfig>>);

impl ConfigStorePort for SharedConfigStore {
    fn load(&self) -> Arc<AppConfig> {
        self.0.load_full()
    }
    fn store(&self, config: AppConfig) {
        self.0.store(Arc::new(config));
    }
}

// ── Fixture ─────────────────────────────────────────────────────────────────

fn profile_toml(name: &str, file: &str) -> String {
    format!(
        "name = \"{name}\"\nextends = \"base\"\n\n[persona]\nadapter = \"coding\"\nconfig = {{ file = \"{file}\" }}\n"
    )
}

fn write_under(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, content).unwrap();
}

struct Fixture {
    /// The config root: `profiles/` and `personas/` live under it.
    root: tempfile::TempDir,
    workspace: tempfile::TempDir,
    core: Arc<AgentCore>,
    ctx: Arc<ComposeContext>,
    app_config: Arc<ArcSwap<AppConfig>>,
    resolver: Arc<ArcSwap<Arc<dyn ProfileResolver>>>,
    provider: Arc<RecordingProvider>,
    driver: LocalTurnDriver,
    _events: mpsc::UnboundedReceiver<AppEvent>,
}

impl Fixture {
    /// `profiles`: `(name, toml)`; `personas`: `(file, body)`; `active` composes
    /// the startup `AgentCore` exactly as startup does (`AgentCore::compose`).
    fn new(profiles: &[(&str, String)], personas: &[(&str, &str)], active: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("profiles")).unwrap();
        std::fs::create_dir_all(root.path().join("personas")).unwrap();
        for (name, toml) in profiles {
            write_under(root.path(), &format!("profiles/{name}.toml"), toml);
        }
        for (file, body) in personas {
            write_under(root.path(), &format!("personas/{file}"), body);
        }

        let ctx = Arc::new(ComposeContext {
            workspace_path: workspace.path().to_path_buf(),
            config_dir: root.path().to_path_buf(), // C2-CONFIG-DIR: lead adds config_dir here
            project_context: rustain::domain::models::project_context::ProjectContext::empty(),
            storage: Arc::new(NoOpStorage) as Arc<dyn rustain::domain::ports::StoragePort>,
            skill_activator: Arc::new(SkillActivator::new()),
            mcp_servers: Vec::new(),
            include_builtin_tools: true,
            domain_tx: None,
            channel_turn_tx: None,
            tool_exposure: "static-full".into(),
            assembler: "passthrough".into(),
            skill_exposure: "l1-metadata".into(),
            skill_cache: Arc::new(
                rustain::infrastructure::skill_cache::SkillCache::new_in_memory(),
            ),
            sandbox_adapter: "noop".into(),
            sandbox_startup_policy: rustain::domain::models::sandbox::SandboxPolicy::Permissive,
            sandbox_slot: Arc::new(ArcSwap::from_pointee(Arc::new(
                rustain::adapters::sandbox::NoOpSandbox,
            )
                as Arc<dyn rustain::domain::ports::SandboxManager>)),
            sandbox_policy: Arc::new(tokio::sync::RwLock::new(
                rustain::domain::models::sandbox::SandboxPolicy::Permissive,
            )),
            memory_slot: Arc::new(ArcSwap::from_pointee(Arc::new(
                rustain::adapters::noop::NoOpMemory,
            )
                as Arc<dyn rustain::domain::ports::MemoryPort>)),
            memory_write_gate: Arc::new(tokio::sync::RwLock::new(())),
            peer_topic_store: Arc::new(rustain::adapters::rap::PeerTopicStore::new()),
            #[cfg(feature = "meta-search")]
            search_config: rustain::domain::models::SearchConfig::default(),
            #[cfg(feature = "meta-search")]
            meta_search_engine: None,
            a2a_peers: Vec::new(),
        });

        let startup = TomlProfileResolver::new(active, root.path().join("profiles"))
            .unwrap_or_else(|e| panic!("startup profile {active} must resolve: {e}"));
        let resolved = startup.resolve_active().expect("active profile resolves");
        let core = Arc::new(
            AgentCore::compose(&resolved.name, &resolved.selection, &ctx)
                .unwrap_or_else(|e| panic!("startup compose of {active} must succeed: {e}")),
        );
        let resolver: Arc<ArcSwap<Arc<dyn ProfileResolver>>> = Arc::new(ArcSwap::from_pointee(
            Arc::new(startup) as Arc<dyn ProfileResolver>,
        ));
        let app_config = Arc::new(ArcSwap::from_pointee(AppConfig {
            active_profile: active.to_string(),
            ..AppConfig::default()
        }));

        let provider = Arc::new(RecordingProvider {
            systems: std::sync::Mutex::new(Vec::new()),
        });
        let security: Arc<dyn SecurityPort> = Arc::new(NoOpSecurity);
        let tools: Arc<dyn ToolSetPort> = Arc::new(NoOpToolSet);
        let scheduler = ToolScheduler::new(
            security.clone(),
            tools.clone(),
            ApprovalRuntime::new(
                16,
                Arc::new(rustain::adapters::noop::NoOpApprovalPersistence),
            ),
            16,
        );
        let (event_tx, events) = mpsc::unbounded_channel();
        let driver = LocalTurnDriver::new(
            provider.clone(),
            app_config.clone(),
            event_tx,
            security,
            tools,
            scheduler,
            Arc::new(SkillActivator::new()),
            // AD-12: the production driver holds the live slot, not a snapshot.
            Arc::clone(&core.persona),
            Arc::new(ArcSwap::from_pointee(
                Arc::new(NoOpContext) as Arc<dyn ContextPort>
            )),
            Arc::new(ArcSwap::from_pointee(
                None as Option<Arc<dyn ContextAssemblerPort>>,
            )),
            Arc::new(ArcSwap::from_pointee(
                None as Option<Arc<dyn rustain::domain::ports::SkillExposurePort>>,
            )),
            workspace.path().to_path_buf(),
            Arc::new(FileSystemStorage::new(workspace.path().join("sessions"))),
            Arc::new(NoOpStorage),
            Arc::new(rustain::domain::services::plan_mode_injector::DefaultPlanInjector::new()),
            Arc::new(rustain::adapters::noop::NoOpUsageLedger) as Arc<dyn UsageLedgerPort>,
            ActiveRatioWindow::new_in_memory(),
        );

        Self {
            root,
            workspace,
            core,
            ctx,
            app_config,
            resolver,
            provider,
            driver,
            _events: events,
        }
    }

    fn root(&self) -> &Path {
        self.root.path()
    }

    fn profiles_dir(&self) -> PathBuf {
        self.root().join("profiles")
    }

    /// One turn through the production `LocalTurnDriver::submit`; returns the
    /// request `system` prompt the provider received for it.
    async fn turn(&self, agent: Option<ActiveAgent>) -> String {
        let before = self.provider.systems.lock().unwrap().len();
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
            peer_row_controls: Default::default(),
            peer_row_revision: 0,
            persist: Default::default(),
        };
        let mut streaming = StreamingState::default();
        let mut state = TuiState::new(120, 24);
        let mut lifecycle =
            rustain::infrastructure::runtime::tui_turn_lifecycle::TuiTurnLifecycle::default();
        let mut session_manager = SessionManager::new(SessionState::Empty);

        self.driver
            .submit(
                UserSubmission {
                    text: "C2 persona probe".into(),
                    images: vec![],
                    synthetic: false,
                    activation_set: None,
                    agent_snapshot: agent,
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
        let handle = lifecycle
            .take_task(&conversation.id)
            .expect("LocalTurnDriver launches run_turn");
        tokio::time::timeout(DEADLINE, handle)
            .await
            .expect("turn must finish")
            .expect("turn task must not panic");

        let systems = self.provider.systems.lock().unwrap();
        assert_eq!(
            systems.len(),
            before + 1,
            "exactly one provider request per turn"
        );
        systems[before].clone()
    }

    async fn switch_to(&self, target: &str) -> HandlerOutcome {
        let mut state = TuiState::new(120, 24);
        self.switch_with(&mut state, target).await
    }

    /// The same, with the caller's `TuiState` (it carries the session overrides).
    async fn switch_with(&self, state: &mut TuiState, target: &str) -> HandlerOutcome {
        let mut notices = Vec::new();
        handle_profile_switch_requested(
            state,
            &self.core,
            &self.ctx,
            &self.app_config,
            &self.resolver,
            target.to_string(),
            &mut notices,
        )
        .await
    }

    /// `/config reload` through the production handler; `active` is what
    /// `--profile` resolves to.
    fn reload(&self, active: &str) -> HandlerOutcome {
        self.reload_with(active, &[], &self.ctx)
    }

    /// The same with extra CLI arguments and an explicit compose snapshot.
    fn reload_with(
        &self,
        active: &str,
        extra: &[&str],
        snapshot: &Arc<ComposeContext>,
    ) -> HandlerOutcome {
        let none_config = self.root().join("no-such-config.toml");
        let mut args = vec![
            "rustain",
            "--profile",
            active,
            "--config-file",
            none_config.to_str().unwrap(),
        ];
        args.extend_from_slice(extra);
        let cli = Cli::try_parse_from(args).expect("cli parses");
        let store = SharedConfigStore(self.app_config.clone());
        let mut notices = Vec::new();
        handle_config_reload_with_two_pass(ReloadContext {
            cli: &cli,
            config_store: &store,
            profile_store: &self.resolver,
            cli_config_overrides: None,
            agent_core: &self.core,
            compose_snapshot: snapshot,
            mcp_config_notices: &mut notices,
        })
    }

    /// Identity handles of everything a failed reload/switch must not touch.
    fn identities(&self) -> Identities {
        Identities {
            config: self.app_config.load_full(),
            resolver: self.resolver.load_full(),
            persona: self.core.persona.load_full(),
        }
    }

    fn assert_unchanged(&self, before: &Identities, what: &str) {
        assert!(
            Arc::ptr_eq(&before.config, &self.app_config.load_full()),
            "{what}: the config store must be unchanged"
        );
        assert!(
            Arc::ptr_eq(&before.resolver, &self.resolver.load_full()),
            "{what}: the profile resolver must be unchanged"
        );
        assert!(
            Arc::ptr_eq(&before.persona, &self.core.persona.load_full()),
            "{what}: the persona slot must be unchanged"
        );
    }
}

/// `HandlerOutcome` has no `Debug`; render what a failing assertion needs.
fn show(outcome: &HandlerOutcome) -> String {
    match outcome {
        HandlerOutcome::Quiet => "Quiet".to_string(),
        HandlerOutcome::Notify(event) => format!("Notify({event:?})"),
        HandlerOutcome::RequestSpawn(_) => "RequestSpawn(..)".to_string(),
        HandlerOutcome::RequestCompaction(_) => "RequestCompaction(..)".to_string(),
    }
}

struct Identities {
    config: Arc<AppConfig>,
    resolver: Arc<Arc<dyn ProfileResolver>>,
    persona: Arc<Arc<dyn PersonaPort>>,
}

fn two_profiles() -> Fixture {
    Fixture::new(
        &[
            ("a", profile_toml("a", "a.md")),
            ("b", profile_toml("b", "b.md")),
        ],
        &[("a.md", A_BODY), ("b.md", B_BODY)],
        "a",
    )
}

// ── Plan level (public TransitionPlan::from_selections) ─────────────────────

fn selection_of(
    profiles_dir: &Path,
    name: &str,
) -> std::collections::BTreeMap<PortDimension, AdapterRef> {
    TomlProfileResolver::new(name, profiles_dir.to_path_buf())
        .unwrap_or_else(|e| panic!("profile {name} must resolve: {e}"))
        .resolve_active()
        .expect("resolves")
        .selection
        .dimensions
}

/// Positive control: two profiles naming the SAME persona adapter and file are
/// not a persona transition (green at the base and after the change).
#[test]
fn plan_has_no_persona_diff_when_both_profiles_name_the_same_file() {
    let fx = Fixture::new(
        &[
            ("a", profile_toml("a", "shared.md")),
            ("b", profile_toml("b", "shared.md")),
        ],
        &[("shared.md", SHARED_BODY)],
        "a",
    );
    let plan = TransitionPlan::from_selections(
        &selection_of(&fx.profiles_dir(), "a"),
        &selection_of(&fx.profiles_dir(), "b"),
        "b",
        3,
        None,
    );
    assert!(
        plan.diffs.is_empty(),
        "same adapter and same file: no diff, got {:?}",
        plan.diffs
    );
}

/// AD-13: same adapter name, different `config.file` IS a persona transition
/// (RED at the base: diffing is by adapter name only).
#[test]
fn plan_reports_a_persona_diff_when_only_the_file_differs() {
    let fx = two_profiles();
    let plan = TransitionPlan::from_selections(
        &selection_of(&fx.profiles_dir(), "a"),
        &selection_of(&fx.profiles_dir(), "b"),
        "b",
        3,
        None,
    );
    assert_eq!(plan.diffs.len(), 1, "exactly one diff: {:?}", plan.diffs);
    assert_eq!(plan.diffs[0].port, PortDimension::Persona);
    assert_eq!(plan.diffs[0].from_adapter, "coding");
    assert_eq!(plan.diffs[0].to_adapter, "coding");
}

// ── Switch ──────────────────────────────────────────────────────────────────

/// The startup turn carries a's body, `handle_profile_switch_requested("b")`
/// succeeds, and the NEXT turn's `system` is exactly b's body.
/// Needs: C-2 (file reaches the prompt), AD-13 (switcher resolves
/// `compose_snapshot.config_dir/profiles`; persona diff by `config`), AD-12.
#[tokio::test]
#[serial(c2_profile_switch)]
async fn switch_to_b_makes_the_next_turn_carry_b_body() {
    let fx = two_profiles();
    assert_eq!(fx.turn(None).await, A_BODY, "startup turn carries a's body");

    let outcome = fx.switch_to("b").await;
    match outcome {
        HandlerOutcome::Notify(AppEvent::ProfileSwitched {
            profile_name,
            summary,
            warm_cold_pending,
            ..
        }) => {
            assert_eq!(profile_name, "b");
            assert_eq!(summary, "1 hot, 0 warm, 0 cold");
            assert!(!warm_cold_pending);
        }
        other => panic!("expected ProfileSwitched for b, got {}", show(&other)),
    }
    assert_eq!(
        fx.resolver.load_full().resolve_active().unwrap().name,
        "b",
        "the resolver follows the switch"
    );
    assert_eq!(fx.app_config.load_full().active_profile, "b");

    assert_eq!(
        fx.turn(None).await,
        B_BODY,
        "the next turn must carry b's body, and only b's body"
    );
}

/// Positive control: the same file on both profiles is no persona transition;
/// the handler reports "no changes" and nothing is swapped.
#[tokio::test]
#[serial(c2_profile_switch)]
async fn switch_between_profiles_naming_the_same_file_swaps_nothing() {
    let fx = Fixture::new(
        &[
            ("a", profile_toml("a", "shared.md")),
            ("b", profile_toml("b", "shared.md")),
        ],
        &[("shared.md", SHARED_BODY)],
        "a",
    );
    assert_eq!(fx.turn(None).await, SHARED_BODY);
    let before = fx.identities();

    match fx.switch_to("b").await {
        HandlerOutcome::Notify(AppEvent::ProfileSwitched {
            profile_name,
            summary,
            warm_cold_pending,
            ..
        }) => {
            assert_eq!(profile_name, "b");
            assert_eq!(summary, "no changes");
            assert!(!warm_cold_pending);
        }
        other => panic!(
            "expected ProfileSwitched {{ no changes }}, got {}",
            show(&other)
        ),
    }
    assert!(
        Arc::ptr_eq(&before.persona, &fx.core.persona.load_full()),
        "no persona diff: the persona slot is not rebuilt"
    );
    assert_eq!(fx.turn(None).await, SHARED_BODY);
}

/// A target whose persona file is missing fails the switch, rolled back; the
/// next turn still carries a's body and nothing was stored.
#[tokio::test]
#[serial(c2_profile_switch)]
async fn switch_to_a_profile_with_a_missing_file_rolls_back() {
    let fx = Fixture::new(
        &[
            ("a", profile_toml("a", "a.md")),
            ("b", profile_toml("b", "missing.md")),
        ],
        &[("a.md", A_BODY)],
        "a",
    );
    assert_eq!(fx.turn(None).await, A_BODY);
    let before = fx.identities();

    match fx.switch_to("b").await {
        HandlerOutcome::Notify(AppEvent::ProfileSwitchFailed {
            profile_name,
            error,
            rolled_back,
        }) => {
            assert_eq!(profile_name, "b");
            assert!(rolled_back);
            assert!(
                error.contains("missing.md"),
                "the failure must name the missing file; got: {error}"
            );
        }
        other => panic!(
            "expected ProfileSwitchFailed {{ rolled_back: true }}, got {}",
            show(&other)
        ),
    }
    fx.assert_unchanged(&before, "failed switch");
    assert_eq!(
        fx.turn(None).await,
        A_BODY,
        "the next turn still carries a's body after the failed switch"
    );
}

// ── Reload ──────────────────────────────────────────────────────────────────

/// Editing `a.md` and reloading makes the next turn carry the new body.
/// Needs: C-2, AD-14 (reload resolves `compose_snapshot.config_dir/profiles`), AD-12.
#[tokio::test]
async fn reload_after_editing_the_file_makes_the_next_turn_carry_the_new_body() {
    let fx = two_profiles();
    assert_eq!(fx.turn(None).await, A_BODY);

    write_under(fx.root(), "personas/a.md", A_BODY_EDITED);
    match fx.reload("a") {
        HandlerOutcome::Notify(AppEvent::ConfigReloaded { success, error, .. }) => {
            assert!(success, "reload must succeed; error: {error:?}");
            assert!(error.is_none());
        }
        other => panic!(
            "expected ConfigReloaded {{ success: true }}, got {}",
            show(&other)
        ),
    }
    assert_eq!(fx.turn(None).await, A_BODY_EDITED);
}

/// An unknown persona config key makes the reload fail closed (AD-14): reported
/// as `success: false`, nothing stored, the next turn keeps a's old body.
#[tokio::test]
async fn reload_with_an_unknown_persona_key_fails_closed() {
    let fx = two_profiles();
    assert_eq!(fx.turn(None).await, A_BODY);
    let before = fx.identities();

    write_under(
        fx.root(),
        "profiles/a.toml",
        "name = \"a\"\nextends = \"base\"\n\n[persona]\nadapter = \"coding\"\nconfig = { file = \"a.md\", tone = \"x\" }\n",
    );
    match fx.reload("a") {
        HandlerOutcome::Notify(AppEvent::ConfigReloaded { success, error, .. }) => {
            assert!(!success, "an invalid profile must not report success");
            let error = error.expect("a failed reload carries an error");
            assert!(
                error.contains("tone"),
                "the error must name the offending key; got: {error}"
            );
        }
        other => panic!(
            "expected ConfigReloaded {{ success: false }}, got {}",
            show(&other)
        ),
    }
    fx.assert_unchanged(&before, "invalid reload");
    assert_eq!(fx.turn(None).await, A_BODY, "the old body is still served");
}

/// A deleted persona file makes the reload fail closed too: config, resolver
/// and persona unchanged (AD-14 atomic commit), the old body still served.
#[tokio::test]
async fn reload_after_deleting_the_file_fails_closed() {
    let fx = two_profiles();
    assert_eq!(fx.turn(None).await, A_BODY);
    let before = fx.identities();

    std::fs::remove_file(fx.root().join("personas/a.md")).unwrap();
    match fx.reload("a") {
        HandlerOutcome::Notify(AppEvent::ConfigReloaded { success, error, .. }) => {
            assert!(!success, "a missing persona file must not report success");
            let error = error.expect("a failed reload carries an error");
            assert!(
                error.contains("a.md"),
                "the error must name the missing file; got: {error}"
            );
        }
        other => panic!(
            "expected ConfigReloaded {{ success: false }}, got {}",
            show(&other)
        ),
    }
    fx.assert_unchanged(&before, "reload with a deleted file");
    assert_eq!(fx.turn(None).await, A_BODY, "the old body is still served");
}

// ── Override and agent ──────────────────────────────────────────────────────

/// `/persona minimal` empties the persona prompt on the next turn (control: the
/// turn before the override carries a's body).
#[tokio::test]
async fn persona_override_to_minimal_leaves_no_body_on_the_next_turn() {
    let fx = two_profiles();
    assert_eq!(fx.turn(None).await, A_BODY, "control: body before override");

    let mut state = TuiState::new(120, 24);
    let outcome = handle_apply_adapter_override(
        &mut state,
        &fx.core,
        &fx.ctx,
        PortDimension::Persona,
        AdapterRef {
            adapter: "minimal".to_string(),
            _config: None,
        },
    )
    .await;
    match outcome {
        HandlerOutcome::Notify(AppEvent::SessionAdapterOverridden {
            port, adapter_name, ..
        }) => {
            assert_eq!(port, PortDimension::Persona);
            assert_eq!(adapter_name, "minimal");
        }
        other => panic!("expected SessionAdapterOverridden, got {}", show(&other)),
    }
    assert_eq!(
        fx.turn(None).await,
        "",
        "the minimal persona sends no persona text"
    );
}

/// An active custom agent REPLACES the persona (AD-7), through the production
/// activation path; without the agent (positive control) the persona body leads.
#[tokio::test]
async fn an_active_agent_replaces_the_persona_body_and_without_it_the_body_leads() {
    let fx = two_profiles();
    write_under(
        fx.workspace.path(),
        ".claude/agents/reviewer.md",
        &format!("---\nname: reviewer\ndescription: test agent\n---\n{AGENT_BODY}\n"),
    );
    let registry = AgentRegistry::discover(fx.workspace.path());
    let activator = AgentActivator::new(Arc::new(NoOpSecurity));
    activator.set_registry(registry).await;
    let agent = activator
        .activate("c2-conv", "reviewer")
        .await
        .expect("the reviewer agent activates");
    assert!(
        agent.body.starts_with(AGENT_BODY),
        "activation parsed the agent body; got {:?}",
        agent.body
    );

    // Positive control first: no agent, the persona body is the whole prompt.
    assert_eq!(fx.turn(None).await, A_BODY);

    let with_agent = fx.turn(Some(agent.clone())).await;
    assert!(
        with_agent.starts_with(AGENT_BODY),
        "the agent body must lead; got {with_agent:?}"
    );
    assert_eq!(with_agent, agent.body, "the agent body replaces the prompt");
    assert!(
        !with_agent.contains(A_BODY),
        "the persona body must not appear next to an active agent; got {with_agent:?}"
    );
}

// ── Patch round (review C2-02 .. C2-06) ─────────────────────────────────────

async fn apply_minimal(fx: &Fixture, state: &mut TuiState) {
    let outcome = handle_apply_adapter_override(
        state,
        &fx.core,
        &fx.ctx,
        PortDimension::Persona,
        AdapterRef {
            adapter: "minimal".to_string(),
            _config: None,
        },
    )
    .await;
    assert!(
        matches!(
            outcome,
            HandlerOutcome::Notify(AppEvent::SessionAdapterOverridden { .. })
        ),
        "override applies: {}",
        show(&outcome)
    );
}

/// C2-02: `--persona` keeps replacing the whole persona reference on reload,
/// with the profile's file present and with it missing (the override bypasses it).
#[tokio::test]
async fn reload_keeps_the_cli_persona_override_with_the_file_present_and_missing() {
    let fx = two_profiles();
    assert_eq!(fx.turn(None).await, A_BODY, "control: body at startup");

    let outcome = fx.reload_with("a", &["--persona", "minimal"], &fx.ctx);
    assert!(
        matches!(
            outcome,
            HandlerOutcome::Notify(AppEvent::ConfigReloaded { success: true, .. })
        ),
        "{}",
        show(&outcome)
    );
    assert_eq!(fx.turn(None).await, "", "the override survives the reload");

    std::fs::remove_file(fx.root().join("personas/a.md")).unwrap();
    let outcome = fx.reload_with("a", &["--persona", "minimal"], &fx.ctx);
    assert!(
        matches!(
            outcome,
            HandlerOutcome::Notify(AppEvent::ConfigReloaded { success: true, .. })
        ),
        "a missing file the override bypasses must not fail the reload: {}",
        show(&outcome)
    );
    assert_eq!(fx.turn(None).await, "");
}

/// C2-03: clearing a persona override restores the profile's COMPLETE persona
/// reference (the file), and a failed restore (file gone) changes nothing.
#[tokio::test]
async fn clearing_the_persona_override_restores_the_profile_file() {
    let fx = two_profiles();
    let mut state = TuiState::new(120, 24);
    apply_minimal(&fx, &mut state).await;
    assert_eq!(fx.turn(None).await, "", "control: override active");

    // Failure preservation first: the file is gone, the clear must not apply.
    std::fs::rename(
        fx.root().join("personas/a.md"),
        fx.root().join("personas/a.moved"),
    )
    .unwrap();
    let failed = handle_clear_adapter_override(
        &mut state,
        &fx.core,
        &fx.ctx,
        &fx.resolver,
        PortDimension::Persona,
    )
    .await;
    assert!(
        matches!(
            failed,
            HandlerOutcome::Notify(AppEvent::SessionAdapterOverrideFailed { .. })
        ),
        "{}",
        show(&failed)
    );
    assert!(
        state
            .session_overrides
            .contains_key(&PortDimension::Persona)
    );
    assert_eq!(fx.turn(None).await, "", "the override stays live");

    std::fs::rename(
        fx.root().join("personas/a.moved"),
        fx.root().join("personas/a.md"),
    )
    .unwrap();
    let cleared = handle_clear_adapter_override(
        &mut state,
        &fx.core,
        &fx.ctx,
        &fx.resolver,
        PortDimension::Persona,
    )
    .await;
    assert!(
        matches!(
            cleared,
            HandlerOutcome::Notify(AppEvent::SessionAdapterOverridden { .. })
        ),
        "{}",
        show(&cleared)
    );
    assert_eq!(
        fx.turn(None).await,
        A_BODY,
        "the profile's persona file is restored, not just its adapter name"
    );
}

/// C2-04: an override applied in A, then a switch to B naming the SAME persona
/// reference: the switch must rebuild B's persona (the consumer clears the
/// override record, so the live slot cannot keep the override).
#[tokio::test]
#[serial(c2_profile_switch)]
async fn a_switch_to_the_same_reference_replaces_an_active_override() {
    let fx = Fixture::new(
        &[
            ("a", profile_toml("a", "shared.md")),
            ("b", profile_toml("b", "shared.md")),
        ],
        &[("shared.md", SHARED_BODY)],
        "a",
    );
    let mut state = TuiState::new(120, 24);
    apply_minimal(&fx, &mut state).await;
    assert_eq!(fx.turn(None).await, "", "control: override active in A");

    let outcome = fx.switch_with(&mut state, "b").await;
    assert!(
        matches!(
            outcome,
            HandlerOutcome::Notify(AppEvent::ProfileSwitched { .. })
        ),
        "{}",
        show(&outcome)
    );
    assert_eq!(
        fx.turn(None).await,
        SHARED_BODY,
        "B's persona is live after the switch"
    );
}

/// C2-05: a reload whose composition fails AFTER the memory port was built must
/// not leave the new memory adapter published in the shared memory slot.
#[tokio::test]
async fn a_failed_reload_composition_restores_the_published_memory_slot() {
    let profiles = [
        ("a", profile_toml("a", "a.md")),
        (
            "m",
            "name = \"m\"\nextends = \"a\"\n\n[memory]\nadapter = \"daily-log\"\n".to_string(),
        ),
    ];
    let fx = Fixture::new(&profiles, &[("a.md", A_BODY)], "a");
    let before_slot = fx.ctx.memory_slot.load_full();
    let before = fx.identities();

    // The same slot, but an assembler name no factory knows: persona and memory
    // compose (memory publishes), then the assembler step fails.
    let mut bad = (*fx.ctx).clone();
    bad.assembler = "c2-no-such-assembler".into();
    let bad = Arc::new(bad);
    let outcome = fx.reload_with("m", &[], &bad);
    assert!(
        matches!(
            outcome,
            HandlerOutcome::Notify(AppEvent::ConfigReloaded { success: false, .. })
        ),
        "{}",
        show(&outcome)
    );
    fx.assert_unchanged(&before, "failed composition");
    assert!(
        Arc::ptr_eq(&before_slot, &fx.ctx.memory_slot.load_full()),
        "the shared memory slot must hold the previous adapter after a refused reload"
    );
}

fn memory_profiles() -> [(&'static str, String); 2] {
    [
        ("a", profile_toml("a", "a.md")),
        (
            "m",
            "name = \"m\"\nextends = \"a\"\n\n[memory]\nadapter = \"daily-log\"\n".to_string(),
        ),
    ]
}

/// C2-05 (structural): composing for a reload builds the memory adapter
/// UNPUBLISHED; the control, the startup `compose`, publishes it.
#[tokio::test]
async fn staged_composition_never_touches_the_shared_memory_slot() {
    let fx = Fixture::new(&memory_profiles(), &[("a.md", A_BODY)], "a");
    let selection = rustain::domain::models::ProfileSelection {
        dimensions: selection_of(&fx.profiles_dir(), "m"),
    };
    let before = fx.ctx.memory_slot.load_full();

    let staged = AgentCore::compose_staged("m", &selection, &fx.ctx).expect("composes");
    assert!(
        Arc::ptr_eq(&before, &fx.ctx.memory_slot.load_full()),
        "a staged composition must not publish its memory adapter"
    );
    drop(staged);

    let _published = AgentCore::compose("m", &selection, &fx.ctx).expect("composes");
    assert!(
        !Arc::ptr_eq(&before, &fx.ctx.memory_slot.load_full()),
        "control: the startup compose does publish"
    );
}

/// C2-05: while a reload's composition is failing, a concurrent consumer of the
/// live slot (what `remember` reads) only ever sees the previous adapter.
#[tokio::test]
async fn a_concurrent_slot_reader_never_sees_a_refused_candidate() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let fx = Fixture::new(&memory_profiles(), &[("a.md", A_BODY)], "a");
    let before = fx.ctx.memory_slot.load_full();
    let mut bad = (*fx.ctx).clone();
    bad.assembler = "c2-no-such-assembler".into();
    let bad = Arc::new(bad);

    let stop = Arc::new(AtomicBool::new(false));
    let samples = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let slot = Arc::clone(&fx.ctx.memory_slot);
    let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
    let reader = {
        let stop = Arc::clone(&stop);
        let samples = Arc::clone(&samples);
        std::thread::spawn(move || {
            let mut seen = std::collections::BTreeSet::new();
            // First sample BEFORE signalling readiness: the publisher cannot start
            // until the reader has observed the slot at least once.
            seen.insert(Arc::as_ptr(&slot.load_full()) as usize);
            samples.fetch_add(1, Ordering::SeqCst);
            started_tx.send(()).expect("signal started");
            while !stop.load(Ordering::SeqCst) {
                seen.insert(Arc::as_ptr(&slot.load_full()) as usize);
                samples.fetch_add(1, Ordering::SeqCst);
            }
            seen
        })
    };
    started_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the reader started sampling before any reload");
    for _ in 0..25 {
        let outcome = fx.reload_with("m", &[], &bad);
        assert!(
            matches!(
                outcome,
                HandlerOutcome::Notify(AppEvent::ConfigReloaded { success: false, .. })
            ),
            "{}",
            show(&outcome)
        );
    }
    // Do not stop before the reader has sampled a meaningful number of times
    // AFTER the reloads ran (no stop-before-start window).
    let after_reloads = samples.load(Ordering::SeqCst);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while samples.load(Ordering::SeqCst) < after_reloads + 100 {
        assert!(std::time::Instant::now() < deadline, "the reader stalled");
        std::thread::yield_now();
    }
    stop.store(true, Ordering::SeqCst);
    let seen = reader.join().expect("reader thread");
    assert!(samples.load(Ordering::SeqCst) >= 100);
    assert_eq!(
        seen,
        std::collections::BTreeSet::from([Arc::as_ptr(&before) as usize]),
        "the live slot must show only the previous adapter during refused reloads"
    );
}

/// C2-05: a legitimate publication (a warm swap) that is live when a reload is
/// refused survives it: nothing is saved or restored over it.
#[tokio::test]
async fn a_refused_reload_leaves_a_concurrent_warm_swap_publication_in_place() {
    let fx = Fixture::new(&memory_profiles(), &[("a.md", A_BODY)], "a");
    let swapped: Arc<dyn rustain::domain::ports::MemoryPort> =
        Arc::new(rustain::adapters::noop::NoOpMemory);
    fx.ctx.memory_slot.store(Arc::new(Arc::clone(&swapped)));
    fx.core.memory.store(Arc::new(Arc::clone(&swapped)));
    let published = fx.ctx.memory_slot.load_full();
    let core_memory = fx.core.memory.load_full();

    let mut bad = (*fx.ctx).clone();
    bad.assembler = "c2-no-such-assembler".into();
    let outcome = fx.reload_with("m", &[], &Arc::new(bad));
    assert!(
        matches!(
            outcome,
            HandlerOutcome::Notify(AppEvent::ConfigReloaded { success: false, .. })
        ),
        "{}",
        show(&outcome)
    );
    assert!(Arc::ptr_eq(&published, &fx.ctx.memory_slot.load_full()));
    assert!(Arc::ptr_eq(&core_memory, &fx.core.memory.load_full()));
}

/// C2-05: the commit takes the warm-swap write gate: while it is held the reload
/// is refused with nothing changed, and it applies once the gate is free.
#[tokio::test]
async fn the_reload_commit_respects_the_memory_write_gate() {
    let fx = Fixture::new(&memory_profiles(), &[("a.md", A_BODY)], "a");
    let before = fx.identities();
    let before_slot = fx.ctx.memory_slot.load_full();
    {
        let _held = fx.ctx.memory_write_gate.try_write().expect("gate free");
        let outcome = fx.reload("m");
        assert!(
            matches!(
                outcome,
                HandlerOutcome::Notify(AppEvent::ConfigReloaded { success: false, .. })
            ),
            "{}",
            show(&outcome)
        );
        fx.assert_unchanged(&before, "gate busy");
        assert!(Arc::ptr_eq(&before_slot, &fx.ctx.memory_slot.load_full()));
    }
    let outcome = fx.reload("m");
    assert!(
        matches!(
            outcome,
            HandlerOutcome::Notify(AppEvent::ConfigReloaded { success: true, .. })
        ),
        "{}",
        show(&outcome)
    );
    assert!(
        !Arc::ptr_eq(&before_slot, &fx.ctx.memory_slot.load_full()),
        "a successful reload publishes the new memory adapter at the commit"
    );
}

/// C2-06: the active profile vanished and the `coding` fallback is invalid: the
/// reload fails and changes nothing.
#[tokio::test]
async fn a_failed_coding_fallback_does_not_report_reload_success() {
    let fx = Fixture::new(
        &[
            ("a", profile_toml("a", "a.md")),
            (
                "coding",
                "name = \"coding\"\nextends = \"base\"\n\n[persona]\nadapter = \"coding\"\nconfig = { tone = \"x\" }\n"
                    .to_string(),
            ),
        ],
        &[("a.md", A_BODY)],
        "a",
    );
    let before = fx.identities();
    let outcome = fx.reload("no-such-profile");
    assert!(
        matches!(
            outcome,
            HandlerOutcome::Notify(AppEvent::ConfigReloaded { success: false, .. })
        ),
        "{}",
        show(&outcome)
    );
    fx.assert_unchanged(&before, "failed fallback");
    assert_eq!(fx.turn(None).await, A_BODY);
}

#[cfg(all(feature = "telegram", feature = "cron"))]
#[tokio::test]
#[serial(c2_profile_switch)]
async fn switch_to_daemon_only_adapters_keeps_local_noops_and_notifies() {
    let profiles = [
        ("a", "name = \"a\"\nextends = \"base\"\n".to_owned()),
        ("b", "name = \"b\"\nextends = \"base\"\n[channels]\nadapter = \"telegram\"\n[channels.config]\nallowed_chat_ids = [123]\n[scheduler]\nadapter = \"cron\"\n".to_owned()),
    ];
    let fx = Fixture::new(&profiles, &[], "a");
    let mut state = TuiState::new(80, 24);
    let mut notices = Vec::new();
    let outcome = handle_profile_switch_requested(
        &mut state,
        &fx.core,
        &fx.ctx,
        &fx.app_config,
        &fx.resolver,
        "b".into(),
        &mut notices,
    )
    .await;
    let HandlerOutcome::RequestSpawn(rustain::adapters::tui::handlers::SpawnRequest::ProfileSwap {
        profile_name,
        identity_color,
        warm_cold_diffs,
        agent_core,
        compose_snapshot,
        profile_resolver,
        app_config,
        guard: _guard,
        new_resolver,
    }) = outcome
    else {
        panic!("expected cold profile swap, got {}", show(&outcome));
    };
    let event = rustain::adapters::tui::handlers::profile_switch::handle_profile_swap_continuation(
        warm_cold_diffs,
        agent_core,
        compose_snapshot,
        profile_name,
        identity_color,
        app_config,
        profile_resolver,
        new_resolver,
    )
    .await
    .expect("cold swap succeeds");
    assert!(matches!(event, AppEvent::ProfileSwitched { .. }));
    assert_eq!(
        notices,
        vec![
            "Profile 'b' uses daemon-only adapters (channels = telegram, scheduler = cron); they run under 'rustain daemon start'. This session uses terminal and none."
        ]
    );
    let channel = fx.core.channels.load().health_snapshot();
    let scheduler = fx.core.scheduler.load().health_snapshot();
    assert_eq!(
        channel.level,
        rustain::domain::models::HealthSummary::unknown().level
    );
    assert_eq!(
        scheduler.level,
        rustain::domain::models::HealthSummary::unknown().level
    );
}
