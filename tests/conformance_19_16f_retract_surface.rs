//! Story 19.16f — the cross-host retract's operator surface, proven through
//! the front door against the REAL recipient.
//!
//! Every keystone here enters where the operator does (Rule 2): the typed
//! command goes through `submit_message_for_test` → the `pub`
//! `transparency_bridge::team_command` dispatch arm → the rail-3 preview spawn
//! → the preview-ready event's handler (`open_team_retract_card`) → the real
//! key path (`handle_input`) → the card resolver → the dispatch the accept
//! spawn awaits. The recipient is the real axum router over a real loopback
//! socket, and the assertions read the **recipient's durable mark** and the
//! **sender's journal**, never only the client's return value.
//!
//! ⚠ Every serving harness binds `127.0.0.1`, so every caller is the same
//! loopback principal: nothing here claims cross-principal separation — that
//! is Story 19.16d's fold-level keystone, consumed, not restated.

#![cfg(feature = "a2a")]

use std::sync::Arc;
use std::time::Duration;

use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
use rustain::adapters::a2a::auth::A2aServerSecurity;
use rustain::adapters::a2a::card_cache::SignedCardCache;
use rustain::adapters::a2a::egress::A2aEgress;
use rustain::adapters::a2a::exec::SubmitterKey;
use rustain::adapters::a2a::server::{ServeConfig, serve};
use rustain::adapters::a2a::transparency::TransparencySink;
use rustain::adapters::rap::IdentityKeyStore;
use rustain::adapters::tui::app::{InputAction, handle_input, submit_message_for_test};
use rustain::adapters::tui::handlers::team_command::{
    TEAM_RETRACT_BLOCK_ID, open_team_retract_card, resolve_team_retract_card,
};
use rustain::adapters::tui::state::TuiState;
use rustain::domain::events::{AppEvent, DomainInputEvent, DomainKey, TeamRetractPreview};
use rustain::domain::models::capability_registry::CapabilityRegistry;
use rustain::domain::models::{
    A2aPeerSource, A2aPeerSpec, DispatchAct, ItemAddress, ItemId, JournalRecord,
    RecipientItemState, RedactedUrl, RejectReason, RoomEvent,
};
use rustain::domain::ports::{RoomJournal, RoomJournalReader};
use rustain::infrastructure::runtime::app_state::AppState;
use rustain::infrastructure::runtime::transparency_bridge::{team_command, team_retract_dispatch};
use rustain::infrastructure::subagent::{NodeJournal, NodeRoomJournal, NodeTree};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

const SETTLE: Duration = Duration::from_secs(20);

// ── fixtures ────────────────────────────────────────────────────────────────

/// One journal: a workspace, its real `NodeJournal`, and the room port over it.
struct Journal {
    _workspace: tempfile::TempDir,
    path: std::path::PathBuf,
    journal: Arc<NodeJournal>,
    room: Arc<dyn RoomJournal>,
}

async fn journal() -> Journal {
    let workspace = tempfile::tempdir().expect("workspace");
    let journal = Arc::new(
        NodeJournal::open_workspace(workspace.path())
            .await
            .expect("open real node journal"),
    );
    let room: Arc<dyn RoomJournal> = Arc::new(NodeRoomJournal::new(journal.clone(), None));
    Journal {
        path: workspace.path().to_path_buf(),
        _workspace: workspace,
        journal,
        room,
    }
}

/// Seed one recipient item and drive it to `state` — the same `RoomEvent`s
/// the ingress and the `/team ack`/`/team remove` rails write. A fixture
/// seeder, never a bypass: the verbs under test still enter the served router.
async fn seed_item(room: &dyn RoomJournal, item_id: &str, task: &str, state: RecipientItemState) {
    let address = ItemAddress::from_a2a_ingress(
        SubmitterKey::loopback().pseudonymous_peer_id(),
        ItemId::from_replay(item_id),
    );
    room.record_event(RoomEvent::RecipientItemReceived {
        address: address.clone(),
        task: task.to_owned(),
        alias: None,
        content: format!("content for {item_id}"),
    })
    .await
    .expect("seed the recipient item");
    match state {
        RecipientItemState::Received { .. } => {}
        RecipientItemState::Acknowledged { .. } => room
            .record_event(RoomEvent::RecipientItemAcknowledged {
                address,
                alias: None,
            })
            .await
            .expect("seed the acknowledgement"),
        RecipientItemState::Removed { .. } => room
            .record_event(RoomEvent::RecipientItemRemoved { address })
            .await
            .expect("seed the removal"),
        _ => unreachable!("RecipientItemState is three states"),
    }
}

fn received() -> RecipientItemState {
    RecipientItemState::Received {
        content: String::new(),
    }
}

/// A real recipient host: the production router over a real loopback socket.
/// `sink` is the journal it appends through; it reads `host.journal`.
struct Host {
    endpoint: String,
    cancel: CancellationToken,
    http: tokio::task::JoinHandle<anyhow::Result<()>>,
    _keys: tempfile::TempDir,
}

impl Host {
    async fn serve(host: &Journal, sink: Arc<dyn RoomJournal>, policy: A2aAdmissionPolicy) -> Self {
        let keys = tempfile::tempdir().expect("identity directory");
        let signer = IdentityKeyStore::new(keys.path())
            .load_or_generate()
            .expect("identity");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let endpoint = format!("http://{}/", listener.local_addr().expect("address"));
        let cancel = CancellationToken::new();
        let http = tokio::spawn(serve(
            listener,
            ServeConfig {
                registry: Arc::new(CapabilityRegistry::new(None)),
                signer,
                security: A2aServerSecurity::default(),
                runtime: None,
                transparency: Arc::new(
                    TransparencySink::new(sink).with_reader(host.journal.clone()),
                ),
                policy,
                workspace: host.path.clone(),
                advertised_host: None,
                cards: Arc::new(SignedCardCache::new()),
            },
            cancel.child_token(),
        ));
        Self {
            endpoint,
            cancel,
            http,
            _keys: keys,
        }
    }

    async fn allow(host: &Journal) -> Self {
        Self::serve(host, host.room.clone(), A2aAdmissionPolicy::Allow).await
    }

    fn peer(&self, alias: &str) -> A2aPeerSpec {
        A2aPeerSpec::new(
            alias,
            RedactedUrl::from(self.endpoint.clone()),
            A2aPeerSource::Workspace,
        )
    }

    async fn stop(self) {
        self.cancel.cancel();
        self.http
            .await
            .expect("server task")
            .expect("server shutdown");
    }
}

/// The sender: the PRODUCTION egress compose over its own journal, and an
/// `AppState` holding that runtime exactly where startup installs it.
struct Sender {
    journal: Journal,
    egress: A2aEgress,
    app_state: AppState,
    domain_rx: tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
}

async fn sender(peers: Vec<A2aPeerSpec>) -> Sender {
    let journal = journal().await;
    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
    let egress = A2aEgress::compose(
        peers,
        NodeTree::new(),
        journal.room.clone(),
        journal.journal.clone(),
        event_tx,
    )
    .expect("compose the sender egress");
    tokio::time::timeout(SETTLE, egress.await_cards_settled())
        .await
        .expect("boot card fetches settle");
    let (mut app_state, domain_rx) = app_state(&journal.path);
    app_state.a2a_send = Some(egress.runtime().clone());
    Sender {
        journal,
        egress,
        app_state,
        domain_rx,
    }
}

impl Sender {
    fn runtime(&self) -> &rustain::adapters::a2a::driver::A2aDelegationRuntime {
        self.egress.runtime()
    }

    /// Type `line` into the composer and run the real `/team` dispatch arm.
    async fn type_command(&mut self, state: &mut TuiState, line: &str) {
        state.input_buffer = line.to_owned();
        let InputAction::ExecuteCommand { name, args } = submit_message_for_test(state) else {
            panic!("{line:?} must route to ExecuteCommand");
        };
        assert_eq!(name, "team");
        team_command(state, "conv", args.as_deref(), &self.app_state).await;
    }

    /// The preview-ready event the rail-3 spawn publishes.
    async fn preview_ready(&mut self) -> TeamRetractPreview {
        tokio::time::timeout(SETTLE, async {
            loop {
                match self.domain_rx.recv().await {
                    Some(AppEvent::TeamRetractPreviewReady { preview, .. }) => break preview,
                    Some(_) => {}
                    None => panic!("the domain channel closed before the preview arrived"),
                }
            }
        })
        .await
        .expect("the preview spawn publishes TeamRetractPreviewReady")
    }

    /// Typed command → real preview read → the handler raises the card.
    async fn raise_card(&mut self, state: &mut TuiState, line: &str) {
        self.type_command(state, line).await;
        let preview = self.preview_ready().await;
        open_team_retract_card(state, "conv", preview);
    }

    async fn rows(&self) -> Vec<RoomEvent> {
        RoomJournalReader::load_entries(self.journal.journal.as_ref())
            .await
            .expect("load the sender journal")
            .into_iter()
            .filter_map(|entry| match entry.record {
                JournalRecord::Room(event) => Some(event),
                _ => None,
            })
            .collect()
    }

    async fn retract_dispatch_rows(&self) -> Vec<RoomEvent> {
        self.rows()
            .await
            .into_iter()
            .filter(|row| {
                matches!(
                    row,
                    RoomEvent::RemoteEnvelopeDispatched {
                        act: DispatchAct::ItemRetract { .. },
                        ..
                    }
                )
            })
            .collect()
    }

    async fn rejection_details(&self) -> Vec<String> {
        self.rows()
            .await
            .into_iter()
            .filter_map(|row| match row {
                RoomEvent::RemoteEnvelopeRejected {
                    reason: RejectReason::Policy { detail },
                    ..
                } => Some(detail),
                _ => None,
            })
            .collect()
    }
}

/// Press `y` exactly as the event loop would act on it: only a
/// `TeamRetractConfirm` resolves the card, and only a resolved card is
/// dispatched. Returns the answer's sentence when a dispatch ran.
async fn press_y(sender: &Sender, state: &mut TuiState) -> Option<String> {
    if handle_input(state, &DomainInputEvent::KeyPress('y')) != InputAction::TeamRetractConfirm {
        return None;
    }
    let pending = resolve_team_retract_card(state, true)?;
    Some(
        team_retract_dispatch(sender.runtime(), &pending)
            .await
            .message,
    )
}

/// Every `RecipientItemRetracted` on the recipient's durable journal.
async fn retract_records(host: &Journal) -> usize {
    RoomJournalReader::load_entries(host.journal.as_ref())
        .await
        .expect("load the recipient journal")
        .into_iter()
        .filter(|entry| {
            matches!(
                entry.record,
                JournalRecord::Room(RoomEvent::RecipientItemRetracted { .. })
            )
        })
        .count()
}

/// `AppState` built exactly as `tests/conformance_18_3a_c_artifacts.rs`
/// builds it; the receiver is the domain channel `AppState::new` returns.
fn app_state(
    workspace: &std::path::Path,
) -> (AppState, tokio::sync::mpsc::UnboundedReceiver<AppEvent>) {
    use arc_swap::ArcSwap;
    use clap::Parser;
    use rustain::adapters::noop::{NoOpProvider, NoOpStorage};
    use rustain::domain::ports::StreamingProvider;
    use rustain::domain::services::plan_manager::PlanManager;
    use rustain::domain::services::plan_mode_injector::DefaultPlanInjector;
    use rustain::infrastructure::composition::ComposeContext;
    use rustain::infrastructure::runtime::agent_core::AgentCore;
    use rustain::infrastructure::runtime::event_bus::EventBus;

    let approval_runtime = rustain::domain::services::approval_runtime::ApprovalRuntime::new(
        16,
        Arc::new(rustain::adapters::noop::NoOpApprovalPersistence),
    );
    let provider_swap = Arc::new(ArcSwap::from_pointee(
        Arc::new(NoOpProvider) as Arc<dyn StreamingProvider>
    ));
    let (event_bus, domain_rx) = EventBus::new(64);
    let compose_snapshot = Arc::new(ComposeContext {
        workspace_path: workspace.to_path_buf(),
        project_context: rustain::domain::models::project_context::ProjectContext::empty(),
        storage: Arc::new(NoOpStorage) as Arc<dyn rustain::domain::ports::StoragePort>,
        skill_activator: Arc::new(rustain::adapters::skill_activation::SkillActivator::new()),
        mcp_servers: Vec::new(),
        include_builtin_tools: true,
        domain_tx: None,
        channel_turn_tx: None,
        tool_exposure: "static-full".into(),
        assembler: "passthrough".into(),
        skill_exposure: "l1-metadata".into(),
        skill_cache: Arc::new(rustain::infrastructure::skill_cache::SkillCache::new_in_memory()),
        sandbox_adapter: "noop".into(),
        sandbox_startup_policy: rustain::domain::models::sandbox::SandboxPolicy::Permissive,
        sandbox_slot: Arc::new(ArcSwap::from_pointee(Arc::new(
            rustain::adapters::sandbox::NoOpSandbox,
        )
            as Arc<dyn rustain::domain::ports::SandboxManager>)),
        memory_slot: Arc::new(ArcSwap::from_pointee(
            Arc::new(rustain::adapters::noop::NoOpMemory)
                as Arc<dyn rustain::domain::ports::MemoryPort>,
        )),
        sandbox_policy: Arc::new(tokio::sync::RwLock::new(
            rustain::domain::models::sandbox::SandboxPolicy::Permissive,
        )),
        memory_write_gate: Arc::new(tokio::sync::RwLock::new(())),
        peer_topic_store: Arc::new(rustain::adapters::rap::PeerTopicStore::new()),
        #[cfg(feature = "meta-search")]
        search_config: rustain::domain::models::SearchConfig::default(),
        #[cfg(feature = "meta-search")]
        meta_search_engine: None,
        a2a_peers: Vec::new(),
    });
    AppState::new(
        Arc::new(event_bus),
        domain_rx,
        approval_runtime,
        Arc::new(tokio::sync::RwLock::new(
            rustain::domain::models::SandboxPolicy::Permissive,
        )),
        Arc::new(PlanManager::new(workspace.to_path_buf())),
        Arc::new(DefaultPlanInjector::new()),
        provider_swap,
        Arc::new(rustain::adapters::provider::ProviderRegistry::new()),
        Arc::new(rustain::adapters::noop::NoOpUsageLedger),
        Arc::new(rustain::adapters::budget::BudgetStateStore::new()),
        Arc::new(ArcSwap::from_pointee(
            rustain::domain::models::AppConfig::default(),
        )),
        Arc::new(AgentCore::test_noop()),
        None,
        compose_snapshot,
        Arc::new(ArcSwap::from_pointee(Arc::new(
            rustain::adapters::profile_resolver::noop::NoopProfileResolver,
        )
            as Arc<dyn rustain::domain::ports::ProfileResolver>)),
        rustain::adapters::cli::commands::Cli::try_parse_from(["rustain"]).expect("bare cli"),
        None,
        rustain::infrastructure::telemetry::ActiveRatioWindow::new_in_memory(),
        #[cfg(feature = "meta-search")]
        None,
    )
}

// ── AC2 + AC4 + AC5 + AC10 · the headline ───────────────────────────────────

/// The whole path: typed command → preview read over a real socket → card →
/// `y` → real POST → the recipient's durable mark; the sender's ledger holds
/// exactly one retract dispatch row and no rejection; the remembered board
/// re-renders with the mark, without a second fan-out.
///
/// **Mutants → RED:** `M02` respell the client's method string (the peer
/// answers `-32601`, nothing is marked); `M05` resolve the card on any key
/// but `y` (the stray `x` dispatches); `M13` leave the slot armed after
/// accept (the second `y` dispatches again — two retract dispatch rows).
#[tokio::test]
async fn the_typed_retract_reaches_the_recipients_durable_mark_through_the_confirm_gate() {
    let host = journal().await;
    seed_item(host.room.as_ref(), "ri_live", "task-live", received()).await;
    let server = Host::allow(&host).await;
    let mut sender = sender(vec![server.peer("jun-dev")]).await;
    // The sender dispatched that task, so the board lists the item.
    sender
        .journal
        .room
        .record_event(RoomEvent::RemoteEnvelopeDispatched {
            peer: rustain::domain::models::a2a_peer_spec::alias_pseudonym("jun-dev"),
            task: Some("task-live".to_owned()),
            bytes: 8,
            act: DispatchAct::Task,
        })
        .await
        .expect("seed the sender's dispatch");
    let board =
        rustain::adapters::a2a::board::collect_board(sender.runtime(), std::time::Instant::now())
            .await
            .expect("the first board refresh is admitted");
    assert!(
        rustain::adapters::a2a::board::render_board(&board)
            .contains("    item ri_live · arrival 1 · task task-live · received"),
        "the picker row the operator copies the id from"
    );

    let mut state = TuiState::new(120, 40);
    sender
        .raise_card(&mut state, "/team retract jun-dev ri_live")
        .await;
    let card = state
        .pending_team_retract
        .as_ref()
        .expect("the preview raised the decision card");
    assert!(card.armed, "a verified, unmarked item arms [y]");
    for line in [
        "Retract on jun-dev's host",
        "item         ri_live",
        "their state  received",
        "sent as      task-live",
        "mark         none — not yet retracted",
        "This host addresses jun-dev with one credential and cannot tell which operator sent \
         this item.",
        "This marks the item on their host. It does not delete it, and there is no un-retract. \
         What they already read, they already read.",
        "Awaiting your decision.  [y] Retract  [n] Cancel (Esc)",
    ] {
        assert!(
            card.card.contains(line),
            "{line:?} missing from:\n{}",
            card.card
        );
    }
    assert_eq!(
        retract_records(&host).await,
        0,
        "opening the card writes nothing"
    );

    // A stray key never resolves the gate.
    assert_eq!(
        handle_input(&mut state, &DomainInputEvent::KeyPress('x')),
        InputAction::Consumed
    );
    assert!(state.pending_team_retract.is_some());

    let landed = press_y(&sender, &mut state).await.expect("[y] dispatches");
    assert_eq!(
        landed,
        "Retracted on jun-dev's host. Marked there — never deleted.\n\
         What they already read, they already read."
    );
    // The double press: the slot is gone, so a second `y` confirms nothing.
    assert_eq!(press_y(&sender, &mut state).await, None);

    assert_eq!(
        retract_records(&host).await,
        1,
        "the recipient's durable mark"
    );
    let dispatched = sender.retract_dispatch_rows().await;
    assert_eq!(dispatched.len(), 1, "exactly one POST: {dispatched:?}");
    assert!(
        matches!(
            &dispatched[0],
            RoomEvent::RemoteEnvelopeDispatched {
                task: Some(task),
                act: DispatchAct::ItemRetract { item },
                ..
            } if task == "task-live" && item == "ri_live"
        ),
        "{dispatched:?}"
    );
    assert!(
        sender.rejection_details().await.is_empty(),
        "a landed retract journals no rejection"
    );
    server.stop().await;
}

/// `AC10(d)` — after a landed retract the board re-renders itself from the
/// remembered view with the mark applied, ⛔ never by a second fan-out (the
/// refresh floor would refuse one inside the same second).
#[tokio::test]
async fn a_landed_retract_re_renders_the_remembered_board_with_its_mark() {
    let host = journal().await;
    seed_item(host.room.as_ref(), "ri_mark", "task-mark", received()).await;
    let server = Host::allow(&host).await;
    let mut sender = sender(vec![server.peer("jun-dev")]).await;
    sender
        .journal
        .room
        .record_event(RoomEvent::RemoteEnvelopeDispatched {
            peer: rustain::domain::models::a2a_peer_spec::alias_pseudonym("jun-dev"),
            task: Some("task-mark".to_owned()),
            bytes: 8,
            act: DispatchAct::Task,
        })
        .await
        .expect("seed the sender's dispatch");
    let before =
        rustain::adapters::a2a::board::collect_board(sender.runtime(), std::time::Instant::now())
            .await
            .expect("board admitted");
    assert!(
        !rustain::adapters::a2a::board::render_board(&before).contains("[retracted "),
        "positive control: the remembered board carries no mark before the retract"
    );

    let mut state = TuiState::new(120, 40);
    sender
        .raise_card(&mut state, "/team retract jun-dev ri_mark")
        .await;
    assert_eq!(
        handle_input(&mut state, &DomainInputEvent::KeyPress('y')),
        InputAction::TeamRetractConfirm
    );
    let pending = resolve_team_retract_card(&mut state, true).expect("accepted");
    let answer = team_retract_dispatch(sender.runtime(), &pending).await;
    let board = answer.board.expect("the remembered board re-renders");
    assert!(
        board.contains("    item ri_mark · arrival 1 · task task-mark · received · [retracted "),
        "{board}"
    );
    server.stop().await;
}

/// `M05` positive control — `n` and `Esc` clear the slot and send nothing.
#[tokio::test]
async fn declining_the_card_clears_the_slot_and_sends_nothing() {
    let host = journal().await;
    seed_item(host.room.as_ref(), "ri_keep", "task-keep", received()).await;
    let server = Host::allow(&host).await;
    let mut sender = sender(vec![server.peer("jun-dev")]).await;

    let mut state = TuiState::new(120, 40);
    for decline in [
        DomainInputEvent::KeyPress('n'),
        DomainInputEvent::SpecialKey(DomainKey::Esc),
    ] {
        sender
            .raise_card(&mut state, "/team retract jun-dev ri_keep")
            .await;
        assert_eq!(
            handle_input(&mut state, &decline),
            InputAction::TeamRetractDecline
        );
        assert!(resolve_team_retract_card(&mut state, false).is_none());
        assert!(state.pending_team_retract.is_none());
    }
    assert_eq!(retract_records(&host).await, 0);
    assert!(sender.retract_dispatch_rows().await.is_empty());
    server.stop().await;
}

// ── AC4(c) · the confirm-time read disarms what it cannot verify ────────────

/// Owner ruling — an unverified state DISARMS `[y]`: the peer went away after
/// its card was cached, the preview cannot read the item, and the card says
/// so while `y` is consumed.
///
/// **Mutant `M12` → RED:** arm `[y]` on an unresolved read.
#[tokio::test]
async fn an_unverified_read_raises_a_disarmed_card_that_consumes_y() {
    let host = journal().await;
    seed_item(host.room.as_ref(), "ri_gone", "task-gone", received()).await;
    let server = Host::allow(&host).await;
    let mut sender = sender(vec![server.peer("jun-dev")]).await;
    server.stop().await;

    let mut state = TuiState::new(120, 40);
    sender
        .raise_card(&mut state, "/team retract jun-dev ri_gone")
        .await;
    let card = state
        .pending_team_retract
        .as_ref()
        .expect("the card renders");
    assert!(!card.armed);
    assert!(
        card.card
            .contains("their state  not verified — could not reach jun-dev's host"),
        "{}",
        card.card
    );
    assert!(
        card.card
            .ends_with("Cannot verify on jun-dev's host.  [n] Cancel (Esc)"),
        "the key line stays last and paints no [y]: {}",
        card.card
    );
    assert_eq!(
        handle_input(&mut state, &DomainInputEvent::KeyPress('y')),
        InputAction::Consumed
    );
    assert!(state.pending_team_retract.is_some(), "`y` resolved nothing");
    assert!(sender.retract_dispatch_rows().await.is_empty());
}

/// `F9` (owner-confirmed) — `Retract × Removed` is a deterministic refusal, so
/// a `removed` item's card disarms `[y]`.
///
/// **Mutant `M16` → RED:** arm `[y]` on `removed`. **Positive control:** an
/// `acknowledged` item arms it.
#[tokio::test]
async fn a_removed_item_disarms_y_and_an_acknowledged_one_arms_it() {
    let host = journal().await;
    seed_item(
        host.room.as_ref(),
        "ri_removed",
        "task-r",
        RecipientItemState::Removed {
            acknowledged_before: false,
        },
    )
    .await;
    seed_item(
        host.room.as_ref(),
        "ri_acked",
        "task-a",
        RecipientItemState::Acknowledged {
            content: String::new(),
        },
    )
    .await;
    let server = Host::allow(&host).await;
    let mut sender = sender(vec![server.peer("jun-dev")]).await;

    let mut state = TuiState::new(120, 40);
    sender
        .raise_card(&mut state, "/team retract jun-dev ri_removed")
        .await;
    let card = state.pending_team_retract.as_ref().expect("card");
    assert!(
        card.card
            .contains("mark         removed there — nothing to mark")
    );
    assert!(
        card.card
            .ends_with("Already removed on jun-dev's host — nothing to mark.  [n] Cancel (Esc)"),
        "{}",
        card.card
    );
    assert_eq!(press_y(&sender, &mut state).await, None);
    assert!(resolve_team_retract_card(&mut state, false).is_none());

    sender
        .raise_card(&mut state, "/team retract jun-dev ri_acked")
        .await;
    assert!(state.pending_team_retract.as_ref().expect("card").armed);
    assert!(press_y(&sender, &mut state).await.is_some());
    assert_eq!(retract_records(&host).await, 1);
    server.stop().await;
}

/// `AC4(c)` — an id the peer does not list raises NO card; the outcome block
/// renders the not-found sentence (byte-identical whether it never existed or
/// is not this credential's).
#[tokio::test]
async fn an_unlisted_item_raises_no_card_and_renders_not_found() {
    let host = journal().await;
    let server = Host::allow(&host).await;
    let mut sender = sender(vec![server.peer("jun-dev")]).await;

    let mut state = TuiState::new(120, 40);
    sender
        .raise_card(&mut state, "/team retract jun-dev ri_nope")
        .await;
    assert!(state.pending_team_retract.is_none());
    assert_eq!(
        state.feedback_blocks[TEAM_RETRACT_BLOCK_ID].message,
        "jun-dev's host has no item ri_nope this host can address. Nothing was marked."
    );
    server.stop().await;
}

// ── AC3(b) · one peer, never a fan-out ──────────────────────────────────────

/// **Mutant `M09` → RED:** fan the retract across the roster — a second
/// dispatch row lands for the peer that never minted the id. **Positive
/// control:** the bound peer that minted it lands its POST.
#[tokio::test]
async fn a_retract_addresses_one_peer_and_never_fans_out() {
    let minted = journal().await;
    seed_item(minted.room.as_ref(), "ri_one", "task-one", received()).await;
    let other = journal().await;
    let a = Host::allow(&minted).await;
    let b = Host::allow(&other).await;
    let mut sender = sender(vec![a.peer("alpha"), b.peer("bravo")]).await;

    let mut state = TuiState::new(120, 40);
    sender
        .raise_card(&mut state, "/team retract alpha ri_one")
        .await;
    assert!(press_y(&sender, &mut state).await.is_some());

    assert_eq!(retract_records(&minted).await, 1);
    let dispatched = sender.retract_dispatch_rows().await;
    assert_eq!(dispatched.len(), 1, "{dispatched:?}");
    assert!(matches!(
        &dispatched[0],
        RoomEvent::RemoteEnvelopeDispatched { peer, .. }
            if *peer == rustain::domain::models::a2a_peer_spec::alias_pseudonym("alpha")
    ));
    a.stop().await;
    b.stop().await;
}

// ── AC10(c) + AC5(a) · each answer, its own sentence and its own ledger ─────

/// **Mutant `M14` → RED:** ignore `alreadyRetracted` — the second retract of a
/// marked item renders the landed sentence. **Positive control:** the first
/// renders landed.
#[tokio::test]
async fn a_second_retract_says_already_retracted_and_changes_nothing() {
    let host = journal().await;
    seed_item(host.room.as_ref(), "ri_twice", "task-twice", received()).await;
    let server = Host::allow(&host).await;
    let mut sender = sender(vec![server.peer("jun-dev")]).await;

    let mut state = TuiState::new(120, 40);
    sender
        .raise_card(&mut state, "/team retract jun-dev ri_twice")
        .await;
    let first = press_y(&sender, &mut state).await.expect("dispatched");
    assert!(first.starts_with("Retracted on jun-dev's host."), "{first}");

    // `[y]` stays armed on an already-marked item (Open question 3, default).
    sender
        .raise_card(&mut state, "/team retract jun-dev ri_twice")
        .await;
    let card = state.pending_team_retract.as_ref().expect("card");
    assert!(card.armed);
    assert!(
        card.card.contains("mark         already retracted "),
        "{}",
        card.card
    );
    let second = press_y(&sender, &mut state).await.expect("dispatched");
    assert!(
        second.starts_with("already retracted ") && second.ends_with(" — nothing changed"),
        "{second}"
    );
    assert_eq!(retract_records(&host).await, 1, "no second append");
    assert!(sender.rejection_details().await.is_empty());
    server.stop().await;
}

/// The race the card cannot close: the item is removed on the recipient AFTER
/// the confirm-time read. The recipient's own refusal is the backstop.
///
/// **Mutant `M17` → RED:** drop the `ItemRemoved` classification — the
/// tombstone renders as unknown. **Mutant `M06` → RED:** drop the rejection
/// row — the sender's ledger keeps only the dispatch.
#[tokio::test]
async fn a_tombstone_after_the_read_renders_its_own_sentence_and_a_rejection_row() {
    let host = journal().await;
    seed_item(host.room.as_ref(), "ri_race", "task-race", received()).await;
    let server = Host::allow(&host).await;
    let mut sender = sender(vec![server.peer("jun-dev")]).await;

    let mut state = TuiState::new(120, 40);
    sender
        .raise_card(&mut state, "/team retract jun-dev ri_race")
        .await;
    assert!(state.pending_team_retract.as_ref().expect("card").armed);
    host.room
        .record_event(RoomEvent::RecipientItemRemoved {
            address: ItemAddress::from_a2a_ingress(
                SubmitterKey::loopback().pseudonymous_peer_id(),
                ItemId::from_replay("ri_race"),
            ),
        })
        .await
        .expect("the recipient removes the item after the read");

    let answer = press_y(&sender, &mut state).await.expect("dispatched");
    assert_eq!(
        answer,
        "jun-dev's host already removed that item — its content is no longer shown there.\n\
         Nothing was marked."
    );
    assert_eq!(sender.retract_dispatch_rows().await.len(), 1);
    assert_eq!(
        sender.rejection_details().await,
        vec!["item retract refused: already removed by its recipient".to_owned()]
    );
    server.stop().await;
}

/// `F5` — a `-32603` can follow a durable mark, so it is UNKNOWN: its own
/// sentence and ⛔ no rejection row. A `-32040` proves nothing was marked.
///
/// **Mutant `M15` → RED:** render `-32603` as "Nothing was marked".
/// **Positive control:** `-32040` renders the authority sentence and journals
/// a rejection.
#[tokio::test]
async fn an_internal_error_is_unknown_and_a_policy_refusal_is_proven() {
    struct BrokenSink;
    #[async_trait::async_trait]
    impl RoomJournal for BrokenSink {
        async fn record_event(
            &self,
            _event: RoomEvent,
        ) -> Result<(), rustain::domain::ports::RoomJournalError> {
            Err(rustain::domain::ports::RoomJournalError::Append(
                "disk full".to_owned(),
            ))
        }
    }

    let broken = journal().await;
    seed_item(broken.room.as_ref(), "ri_broken", "task-b", received()).await;
    let server = Host::serve(&broken, Arc::new(BrokenSink), A2aAdmissionPolicy::Allow).await;
    let mut sender_a = sender(vec![server.peer("jun-dev")]).await;
    let mut state = TuiState::new(120, 40);
    sender_a
        .raise_card(&mut state, "/team retract jun-dev ri_broken")
        .await;
    let unknown = press_y(&sender_a, &mut state).await.expect("dispatched");
    assert_eq!(
        unknown,
        "No usable answer from jun-dev's host — whether the item was marked is unknown. \
         '/team board' shows its current mark."
    );
    assert!(
        sender_a.rejection_details().await.is_empty(),
        "a rejection for a write that may have landed is a false claim"
    );
    server.stop().await;

    let denying = journal().await;
    seed_item(denying.room.as_ref(), "ri_denied", "task-d", received()).await;
    let server = Host::serve(&denying, denying.room.clone(), A2aAdmissionPolicy::Deny).await;
    let mut sender_b = sender(vec![server.peer("jun-dev")]).await;
    sender_b
        .raise_card(&mut state, "/team retract jun-dev ri_denied")
        .await;
    let refused = press_y(&sender_b, &mut state).await.expect("dispatched");
    assert_eq!(
        refused,
        "jun-dev's host refused the retract. Nothing was marked."
    );
    assert_eq!(
        sender_b.rejection_details().await,
        vec!["item retract refused: refused by the peer's policy".to_owned()]
    );
    assert_eq!(retract_records(&denying).await, 0);
    server.stop().await;
}

/// `M08`'s positive control — a POST through the real `A2aClientAdapter` to a
/// port that closed after the card was read: `reqwest::Error::is_connect()`
/// fires inside `map_transport_error`, so the answer is PROVEN undelivered.
#[tokio::test]
async fn a_closed_port_renders_could_not_reach_and_journals_a_rejection() {
    let host = journal().await;
    seed_item(host.room.as_ref(), "ri_closed", "task-c", received()).await;
    let server = Host::allow(&host).await;
    let mut sender = sender(vec![server.peer("jun-dev")]).await;

    let mut state = TuiState::new(120, 40);
    sender
        .raise_card(&mut state, "/team retract jun-dev ri_closed")
        .await;
    assert!(state.pending_team_retract.as_ref().expect("card").armed);
    server.stop().await;

    let answer = press_y(&sender, &mut state).await.expect("dispatched");
    assert_eq!(
        answer,
        "Could not reach jun-dev's host. Nothing was marked."
    );
    assert_eq!(
        sender.rejection_details().await,
        vec!["item retract refused: could not reach the peer".to_owned()]
    );
}

// ── AC5(a) · the dispatch row is durable BEFORE the POST ────────────────────

/// Rule 4, a structural ordering — ⛔ no timing window. The recipient blocks
/// inside its journal append, so the POST is provably in flight; the sender's
/// dispatch row must already be durable.
///
/// **Mutant `M20` → RED:** journal the dispatch row after the POST returns.
/// **Positive control:** after release the mark lands and the sender holds
/// exactly one dispatch row.
#[tokio::test]
async fn the_dispatch_row_is_durable_before_the_post_is_answered() {
    struct BlockingRetractJournal {
        inner: Arc<dyn RoomJournal>,
        append_started: Arc<Notify>,
        resume_append: Arc<Notify>,
    }
    #[async_trait::async_trait]
    impl RoomJournal for BlockingRetractJournal {
        async fn record_event(
            &self,
            event: RoomEvent,
        ) -> Result<(), rustain::domain::ports::RoomJournalError> {
            if matches!(event, RoomEvent::RecipientItemRetracted { .. }) {
                self.append_started.notify_one();
                self.resume_append.notified().await;
            }
            self.inner.record_event(event).await
        }
    }

    let host = journal().await;
    seed_item(host.room.as_ref(), "ri_order", "task-o", received()).await;
    let append_started = Arc::new(Notify::new());
    let resume_append = Arc::new(Notify::new());
    let server = Host::serve(
        &host,
        Arc::new(BlockingRetractJournal {
            inner: host.room.clone(),
            append_started: append_started.clone(),
            resume_append: resume_append.clone(),
        }),
        A2aAdmissionPolicy::Allow,
    )
    .await;
    let mut sender = sender(vec![server.peer("jun-dev")]).await;
    let mut state = TuiState::new(120, 40);
    sender
        .raise_card(&mut state, "/team retract jun-dev ri_order")
        .await;
    assert_eq!(
        handle_input(&mut state, &DomainInputEvent::KeyPress('y')),
        InputAction::TeamRetractConfirm
    );
    let pending = resolve_team_retract_card(&mut state, true).expect("accepted");
    let runtime = sender.runtime().clone();
    let dispatch = tokio::spawn(async move { team_retract_dispatch(&runtime, &pending).await });

    tokio::time::timeout(SETTLE, append_started.notified())
        .await
        .expect("the POST reached the recipient's append");
    assert_eq!(
        sender.retract_dispatch_rows().await.len(),
        1,
        "the dispatch row is durable while the POST is still in flight"
    );
    resume_append.notify_one();
    let answer = dispatch.await.expect("dispatch task");
    assert!(
        answer.message.starts_with("Retracted on jun-dev's host."),
        "{}",
        answer.message
    );
    assert_eq!(sender.retract_dispatch_rows().await.len(), 1);
    assert_eq!(retract_records(&host).await, 1);
    server.stop().await;
}

// ── AC1 + AC9 · the picker and its escape hatch ─────────────────────────────

/// **Mutant `M11` → RED:** delete the narrowing arm — `/team board jun-dev`
/// no longer renders the peer's 61st item. **Positive control:** bare
/// `/team board` caps at 60 and states the cut.
#[tokio::test]
async fn a_narrowed_board_renders_every_item_and_the_full_board_states_its_cap() {
    let host = journal().await;
    let sender_journal_tasks: Vec<String> = (0..61).map(|n| format!("task-{n:02}")).collect();
    for (n, task) in sender_journal_tasks.iter().enumerate() {
        seed_item(host.room.as_ref(), &format!("ri_{n:02}"), task, received()).await;
    }
    let server = Host::allow(&host).await;
    let mut sender = sender(vec![server.peer("jun-dev")]).await;
    for task in &sender_journal_tasks {
        sender
            .journal
            .room
            .record_event(RoomEvent::RemoteEnvelopeDispatched {
                peer: rustain::domain::models::a2a_peer_spec::alias_pseudonym("jun-dev"),
                task: Some(task.clone()),
                bytes: 1,
                act: DispatchAct::Task,
            })
            .await
            .expect("seed dispatch");
    }

    let full =
        rustain::adapters::a2a::board::collect_board(sender.runtime(), std::time::Instant::now())
            .await
            .expect("board admitted");
    let full = rustain::adapters::a2a::board::render_board(&full);
    assert!(
        full.contains(
            "    · showing the 60 most recent of 61 items for this peer — narrow with \
             '/team board <peer-id>'"
        ),
        "{full}"
    );
    assert!(!full.contains("item ri_00 "), "the oldest is past the cap");
    assert!(full.contains("item ri_60 · arrival 61"), "{full}");

    // The escape hatch, through the typed command. The typed rail stamps
    // `Instant::now()`, so it must arrive after the stated 500 ms floor — a
    // wait past a design bound, not a race window.
    let mut state = TuiState::new(120, 40);
    tokio::time::sleep(rustain::adapters::a2a::board::board_refresh_floor()).await;
    sender.type_command(&mut state, "/team board jun-dev").await;
    let narrowed = tokio::time::timeout(SETTLE, async {
        loop {
            if let Some(AppEvent::TeamBoardReady { message, .. }) = sender.domain_rx.recv().await {
                break message;
            }
        }
    })
    .await
    .expect("the board spawn publishes");
    assert!(narrowed.contains("item ri_00 · arrival 1"), "{narrowed}");
    assert!(
        !narrowed.contains("showing the 60 most recent"),
        "{narrowed}"
    );

    // An id that names no roster peer: no network call, and it says so.
    sender.type_command(&mut state, "/team board nobody").await;
    let unknown = tokio::time::timeout(SETTLE, async {
        loop {
            if let Some(AppEvent::TeamBoardReady { message, .. }) = sender.domain_rx.recv().await {
                break message;
            }
        }
    })
    .await
    .expect("the board spawn publishes");
    assert_eq!(
        unknown,
        "· no configured A2A peer is named 'nobody' — configured: jun-dev."
    );
    server.stop().await;
}
