//! `K11` (production policy composition, `AC10`/`A14`) and `K14` legs
//! (i)–(v) and (vii) (bound policy provenance, `PQ1` = A, `P21`/`P22`) —
//! Story 19.18d.
//!
//! Front door: every host is a `BoundHost` composed with ONE written
//! interaction TOML (`HostSpec::interaction`): resolved once into an
//! `EffectivePolicy`, installed in the peer bus AND the `AttachServer`, and
//! handed to `configure_consent_policy`. Real consent only: a grant exists
//! because the operator's real `[a]` answer (`AlwaysAndSave { scope:
//! Tool("a2a/sender-consent") }`) travelled the attach socket, or because the
//! production journal writer planted a row BEFORE a restart (`K14` (i)). RAP
//! senders enter through the production RAP front door
//! (`VerifiedPeerFrameHandler::handle_verified_peer_frame` →
//! `DaemonPeerConsumer::consent` → `request_admission_approval(rap(pin))`,
//! `daemon/server.rs:279-309`), never a constructed policy value.
//!
//! Prompts and admissions are told apart by observable facts only: the
//! `ApprovalRuntimeEvent::Requested` stream, the journaled
//! `a2a-inbound-approval:` row and the waiting node with its pending response
//! card (a parked and a waiting task both read `auth-required` on the wire) —
//! never by card wording, except where the story pins the card (`K14` (vii)).
//!
//! Mutants this file turns RED (story MUTANT REGISTER): `M22` (K11 part A:
//! the override test's concrete `response_policy` for the bound pin and the
//! override-implied consent), `M55` (K11 part C: the conflicted pair must not
//! apply), `M58` (K14 (i), both fixtures), `M63` (K14 (v): the observed `via`),
//! `M69` (K14 (vii): one `Requested`, one `[a]` releases both), `M70` (K14
//! (vii): the bound-opened card's line 4), `M71` (K14 (vii): the pending
//! preview equals the `Requested` preview).

use std::sync::Arc;

use rustain::adapters::a2a::config::parse_workspace_a2a_config;
use rustain::adapters::a2a::exec::SubmitterKey;
use rustain::adapters::a2a::transparency::APPROVAL_GATE_PREFIX;
use rustain::adapters::policy::JournalConsentProjection;
use rustain::adapters::rap::{
    AgentSigner, IdentityKeyStore, PeerDeliveryError, PeerTopicStore, VerifiedPeerFrameHandler,
};
use rustain::adapters::tui::app::{InputAction, submit_message_for_test};
use rustain::adapters::tui::state::TuiState;
use rustain::domain::models::{
    AgentEnvelope, AgentId, ApprovalSource, CorrelationId, ItemAddress, ItemId, MessageKind,
    PeerId, PinnedVia, PolicyPrincipal, PrincipalKind, RequestId, ResponseMode, RoomEvent,
    SemanticMessageType, ToolRisk,
};
use rustain::domain::ports::{
    ConsentProjectionQuery, ConsentSlot, ConsentState, InboundPeerRuntime, RoomJournalReader,
};
use rustain::domain::services::approval_runtime::ApprovalRuntimeEvent;
use rustain::domain::services::peer_admission::BOUND_KEY_CONSENT_LINE;
use rustain::infrastructure::runtime::transparency_bridge::team_command;

use crate::harness::{
    BoundHost, HostSpec, K_P1A, K_P1B, K_P1C, K_P2, K_P2_ENV, K_U, K_U_ENV, Operator, RawClient,
    Reply, Roster, SETTLE, bound_item_address, item_id_for_task, listed_ids, operator_app_state,
    pin_of, raw_payloads, received_row_for_task, setup, status_rows,
};

type Events = tokio::sync::broadcast::Receiver<ApprovalRuntimeEvent>;

const CONSENT_TOOL: &str = "a2a/sender-consent";

/// Default delivery policy of every host here: `notify-and-wait`.
const WAIT_TOML: &str =
    "[interaction.defaults]\nresponse_mode = \"notify-and-wait\"\nnotification = \"queue\"\n";

/// Today's `Rap`/`Pseudonymous` line 4 of the consent card (`P22`).
const RAP_TRUST_LINE: &str =
    "[a] trusts the key this sender presents, not the person; a rotated key is a new peer.";

fn wait_spec() -> HostSpec {
    HostSpec::b2().interaction(WAIT_TOML)
}

// ── Observations ────────────────────────────────────────────────────────────

/// One `ApprovalRuntimeEvent::Requested` (`approval_runtime.rs:85-91`).
#[derive(Debug)]
struct Requested {
    id: RequestId,
    source: ApprovalSource,
    tool: String,
    preview: String,
    risk: ToolRisk,
}

impl Requested {
    fn from_event(event: ApprovalRuntimeEvent) -> Option<Self> {
        match event {
            ApprovalRuntimeEvent::Requested {
                id,
                source,
                tool,
                input_preview,
                risk,
            } => Some(Self {
                id,
                source,
                tool,
                preview: input_preview,
                risk,
            }),
            _ => None,
        }
    }

    /// The in-memory policy provenance and the wire-scope peer of the source.
    fn principal(&self) -> &PolicyPrincipal {
        self.source
            .policy_principal()
            .expect("an in-process approval source carries its policy principal")
    }

    fn assert_card_for(&self, expected: &PolicyPrincipal) {
        assert_eq!(self.tool, CONSENT_TOOL, "{self:?}");
        assert_eq!(self.principal(), expected, "{self:?}");
        match &self.source {
            ApprovalSource::RemotePeer { peer_id, .. } => {
                assert_eq!(peer_id, expected.peer(), "{self:?}")
            }
            other => panic!("a remote peer source, got {other:?}"),
        }
    }
}

/// Everything already on the approval stream (non-blocking).
fn drain_requested(events: &mut Events) -> Vec<Requested> {
    let mut seen = Vec::new();
    loop {
        match events.try_recv() {
            Ok(event) => seen.extend(Requested::from_event(event)),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => return seen,
            Err(other) => panic!("the approval stream broke: {other:?}"),
        }
    }
}

/// The next `Requested` (bounded wait).
async fn next_requested(events: &mut Events) -> Requested {
    loop {
        let event = tokio::time::timeout(SETTLE, events.recv())
            .await
            .expect("a Requested event before the deadline")
            .expect("the approval stream stays open");
        if let Some(requested) = Requested::from_event(event) {
            return requested;
        }
    }
}

/// `(spoke, principal)` of every approval-parked `AdmissionDeferred` row for
/// `task`, in journal order (the raw on-disk wire).
fn parked_rows(host: &BoundHost, task: &str) -> Vec<(String, Option<String>)> {
    let gate = format!("{APPROVAL_GATE_PREFIX}{task}");
    raw_payloads(&host.journal, "admission_deferred")
        .into_iter()
        .filter(|row| row["gate"] == gate)
        .map(|row| {
            (
                row["spoke"].as_str().expect("spoke").to_owned(),
                row["principal"].as_str().map(str::to_owned),
            )
        })
        .collect()
}

/// `(sender, principal)` of every on-disk `consent_granted` row, in order
/// (`principal` is `None` for a legacy row: no member on the wire).
fn grants(host: &BoundHost) -> Vec<(String, Option<String>)> {
    raw_payloads(&host.journal, "consent_granted")
        .into_iter()
        .map(|row| {
            (
                row["sender"].as_str().expect("sender").to_owned(),
                row.get("principal")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
            )
        })
        .collect()
}

/// The consent projection folded from the host's durable journal.
async fn projection(host: &BoundHost) -> JournalConsentProjection {
    let entries = RoomJournalReader::load_entries(host.journal.node.as_ref())
        .await
        .expect("journal entries");
    JournalConsentProjection::from_entries(&entries)
}

fn wire_state(reply: &Reply) -> &str {
    reply.json["result"]["status"]["state"]
        .as_str()
        .unwrap_or("<no state>")
}

async fn send(client: &RawClient, task: &str) -> Reply {
    let reply = client
        .message_send(1, task, task, serde_json::json!({}))
        .await;
    assert!(reply.json["error"].is_null(), "{task}: {}", reply.body);
    reply
}

/// The caller is ASKED: exactly one new sender-consent card for `expected`, one approval-parked row typed
/// `pinned`/`pseudonymous` for that principal, and no launch.
async fn expect_prompt(
    host: &BoundHost,
    events: &mut Events,
    client: &RawClient,
    task: &str,
    expected: &PolicyPrincipal,
) -> Requested {
    send(client, task).await;
    let mut cards = drain_requested(events);
    assert_eq!(cards.len(), 1, "{task}: exactly one card: {cards:?}");
    let card = cards.remove(0);
    card.assert_card_for(expected);
    assert_eq!(
        parked_rows(host, task),
        vec![(
            expected.peer().as_str().to_owned(),
            Some(kind_wire(expected).to_owned())
        )],
        "{task}: the approval-parked row is typed for the asking principal"
    );
    card
}

/// The caller is ADMITTED with no card: the task parks as a waiting inbound
/// node with its pending response card, no `Requested`, no parked row.
async fn expect_admitted(host: &BoundHost, events: &mut Events, client: &RawClient, task: &str) {
    send(client, task).await;
    let node = host.waiting_node(task).await;
    host.pending_card(&node).await;
    assert!(
        drain_requested(events).is_empty(),
        "{task}: a trusted principal raises no card"
    );
    assert!(
        parked_rows(host, task).is_empty(),
        "{task}: a trusted principal is not parked for approval"
    );
}

fn kind_wire(principal: &PolicyPrincipal) -> &'static str {
    match principal.kind() {
        PrincipalKind::Pinned => "pinned",
        PrincipalKind::Pseudonymous => "pseudonymous",
        other => panic!("a constructed principal never has kind {other:?}"),
    }
}

/// Answer the sender-consent card with `[a]` and wait until the asking task
/// has launched (its node waiting with a pending response card).
async fn allow_always_and_launch(host: &BoundHost, operator: &mut Operator, task: &str) {
    operator.always_allow().await;
    let node = host.waiting_node(task).await;
    host.pending_card(&node).await;
}

// ── Seeding (production journal writer, before the host starts serving) ─────

/// Journal `ConsentGranted` rows with the production journal writer
/// (`NodeJournal::append_room`, `process_runs.rs::plant_consent`), then
/// restart the host over the same journal so the policy composition folds
/// them. `None` is a LEGACY row (no `principal` member).
async fn start_seeded(spec: HostSpec, grants: &[(PeerId, Option<PrincipalKind>)]) -> BoundHost {
    let host = BoundHost::start(spec.clone()).await;
    for (sender, principal) in grants {
        host.journal
            .node
            .append_room(RoomEvent::ConsentGranted {
                sender: Some(sender.clone()),
                granted_at: 1,
                principal: *principal,
            })
            .await
            .expect("journal the consent grant");
    }
    host.restart(spec).await
}

// ── RAP front door ──────────────────────────────────────────────────────────

/// A RAP peer with a real Ed25519 identity. Its public key is the roster pin
/// of `p1`, so bound keys and this sender share one `PeerId`.
struct RapSender {
    signer: AgentSigner,
    _keys: tempfile::TempDir,
    next: u64,
}

impl RapSender {
    fn new() -> Self {
        let keys = tempfile::tempdir().expect("rap keys");
        let signer = IdentityKeyStore::new(keys.path())
            .load_or_generate()
            .expect("rap identity");
        Self {
            signer,
            _keys: keys,
            next: 0,
        }
    }

    fn pin(&self) -> PeerId {
        self.signer.identity().peer_id.clone()
    }

    fn x(&self) -> [u8; 32] {
        self.signer
            .identity()
            .public_key
            .clone()
            .try_into()
            .expect("a 32-byte Ed25519 key")
    }

    /// The standard roster with `p1` and `p1-alt` pinned to THIS sender's key.
    fn roster(&self) -> Roster {
        Roster::standard()
            .pinned("p1", self.x())
            .pinned("p1-alt", self.x())
    }

    /// `conformance_p2p_ingress.rs:196-225` (the envelope helper): a frame
    /// rooted at the sender's own namespace.
    fn frame(&mut self, recipient: &str, body: &str) -> AgentEnvelope<serde_json::Value> {
        self.next += 1;
        let peer = self.pin();
        self.signer
            .sign(
                AgentId::from_peer_path(&format!("{}/peer-transport", peer.as_str()))
                    .expect("peer-rooted sender"),
                AgentId::parse(&format!("{}/{recipient}", peer.as_str())).expect("recipient"),
                CorrelationId::new(format!("rap-corr-{}", self.next)),
                MessageKind::PeerMessage,
                String::new(),
                self.next,
                i64::MAX,
                format!("rap-nonce-{}", self.next),
                Vec::new(),
                serde_json::json!(body),
            )
            .expect("rap frame")
    }
}

type RapDelivery = tokio::task::JoinHandle<Result<(), PeerDeliveryError>>;

/// Install the production RAP front door on `host` the way the daemon root
/// does (`daemon/mod.rs:610-616`: the transparency sink as the recorder,
/// `configure_peer_recorder`) and hand back the handler the RAP transports
/// call (`verified_peer_handler`).
async fn open_rap_front_door(host: &BoundHost) -> Arc<VerifiedPeerFrameHandler> {
    let recorder = Arc::new(
        rustain::adapters::a2a::transparency::TransparencySink::new(host.journal.room.clone())
            .with_reader(host.journal.node.clone()),
    );
    host.server
        .configure_peer_recorder(
            recorder as Arc<dyn rustain::domain::ports::PeerInteractionRecorder>,
            Arc::new(PeerTopicStore::new()),
        )
        .await;
    host.server
        .verified_peer_handler()
        .await
        .expect("the RAP front door is configured")
}

/// Hand one RAP frame to the front door; it settles when the sender-consent
/// card is answered and the frame is ingested.
fn deliver(
    handler: &Arc<VerifiedPeerFrameHandler>,
    rap: &mut RapSender,
    recipient: &str,
    body: &str,
) -> RapDelivery {
    let frame = rap.frame(recipient, body);
    let peer = rap.pin();
    let handler = handler.clone();
    tokio::spawn(async move { handler.handle_verified_peer_frame(frame, peer).await })
}

async fn delivered(delivery: RapDelivery) {
    tokio::time::timeout(SETTLE, delivery)
        .await
        .expect("the RAP frame settles before the deadline")
        .expect("the delivery task joins")
        .expect("the RAP frame is accepted");
}

// ── Card copy ───────────────────────────────────────────────────────────────

/// The six lines of `consent_card_text` (`transparency_bridge.rs:433-440`)
/// with `trust_line` as line 4; every other line is today's copy.
fn card_lines(peer: &PeerId, trust_line: &str) -> Vec<String> {
    vec![
        format!("Consent required — {peer}"),
        "No standing consent is recorded for this sender.".to_owned(),
        "[y] Allow once  [a] Always allow  [n] Decline".to_owned(),
        trust_line.to_owned(),
        "Awaiting your decision.".to_owned(),
        "Logged immediately for safety; interruption timing follows your policy.".to_owned(),
    ]
}

fn assert_card(preview: &str, peer: &PeerId, trust_line: &str) {
    assert_eq!(
        preview.split('\n').map(str::to_owned).collect::<Vec<_>>(),
        card_lines(peer, trust_line),
        "the card the operator was shown"
    );
}

// ── The operator's `/team` rail ─────────────────────────────────────────────

/// Type one `/team …` line through the production dispatcher against the
/// host's own workspace and roster; returns the status block text.
async fn team(host: &BoundHost, line: &str) -> String {
    let peers = parse_workspace_a2a_config(
        &rustain::infrastructure::paths::workspace_a2a_config_path(&host.journal.path),
    )
    .expect("the written roster parses");
    let (app_state, _domain_rx) = operator_app_state(&host.journal.path, peers);
    let mut state = TuiState::new(120, 40);
    state.input_buffer = line.to_owned();
    let InputAction::ExecuteCommand { name, args } = submit_message_for_test(&mut state) else {
        panic!("{line:?} must reach the command dispatcher");
    };
    assert_eq!(name, "team");
    team_command(&mut state, "conv", args.as_deref(), &app_state).await;
    state.feedback_blocks["team-status"].message.clone()
}

// ═════════════════════════════════════════════════════════════════════════
// K11 — production policy composition
// ═════════════════════════════════════════════════════════════════════════

/// `K11` part 1 (`AC10(a)`, `A14`, `P17`): K_P1A prompts; its real `[a]`
/// persists a typed `"principal":"pinned"` grant; every other key bound to
/// that pin (K_P1B, and K_P1C whose alias is `p1-alt`) is then admitted with
/// no card; P2 and the unbound K_U still prompt.
#[tokio::test(flavor = "multi_thread")]
async fn k11_always_allow_persists_a_typed_pinned_grant_that_every_key_of_the_pin_shares() {
    setup();
    let mut host = BoundHost::start(wait_spec()).await;
    assert!(
        host.effective_policy.is_some(),
        "the policy composition is the production one"
    );
    assert!(
        InboundPeerRuntime::enforces_sender_consent(host.server.as_ref()),
        "the consent projection is composed with the policy"
    );
    let mut operator = host.attach_operator().await;
    let mut events = host.approval_events().await;
    let pin_p1 = pin_of(crate::harness::P1_X);
    let pin_p2 = pin_of(crate::harness::P2_X);
    let (p1a, p1b, p1c) = (host.client(K_P1A), host.client(K_P1B), host.client(K_P1C));

    // K_P1A asks; nothing is granted until the operator answers.
    expect_prompt(
        &host,
        &mut events,
        &p1a,
        "a1",
        &PolicyPrincipal::bound_a2a(pin_p1.clone()),
    )
    .await;
    assert!(grants(&host).is_empty(), "no grant before an answer");

    // The real `[a]` (AlwaysAndSave over the attach socket) writes the typed
    // grant.
    allow_always_and_launch(&host, &mut operator, "a1").await;
    assert_eq!(
        grants(&host),
        vec![(pin_p1.as_str().to_owned(), Some("pinned".to_owned()))],
        "`[a]` persists ConsentGranted with \"principal\":\"pinned\""
    );
    let raw = raw_payloads(&host.journal, "consent_granted");
    assert_eq!(raw[0]["principal"], "pinned", "{}", raw[0]);
    assert_eq!(
        received_row_for_task(&host.journal, "a1")["address"],
        bound_item_address(&item_id_for_task(&host.journal, "a1"), pin_p1.as_str()),
        "the admitted task is a bound item"
    );

    // Positive controls: the grant is the pin's, so every key bound to it —
    // whatever its alias — is admitted without a card.
    expect_admitted(&host, &mut events, &p1b, "b1").await;
    expect_admitted(&host, &mut events, &p1c, "c1").await;
    assert_eq!(grants(&host).len(), 1, "no further grant was needed");

    // The grant is per pin and per rail: P2 and the unbound K_U still prompt.
    expect_prompt(
        &host,
        &mut events,
        &host.client(K_P2),
        "p2a",
        &PolicyPrincipal::bound_a2a(pin_p2),
    )
    .await;
    expect_prompt(
        &host,
        &mut events,
        &host.client(K_U),
        "u1",
        &SubmitterKey::from_api_key(K_U).policy_principal(),
    )
    .await;
    assert_eq!(grants(&host).len(), 1, "still only K_P1A's grant");
    drop(operator);
}

/// `K11` part 2 (`AC10(a)`, `OQ3` = A, `B4`): a typed `Pinned` grant produced
/// by a RAP card answer (the production RAP front door) admits bound A2A
/// without a card and WITHOUT item-space union — the RAP item is not in the
/// bound caller's list.
#[tokio::test(flavor = "multi_thread")]
async fn k11_a_typed_pinned_grant_from_a_rap_card_answer_is_reused_by_bound_a2a_without_item_union()
{
    setup();
    let mut rap = RapSender::new();
    let pin = rap.pin();
    let mut host =
        BoundHost::start(HostSpec::b2().roster(rap.roster()).interaction(WAIT_TOML)).await;
    let mut operator = host.attach_operator().await;
    let mut events = host.approval_events().await;
    let handler = open_rap_front_door(&host).await;

    // The RAP sender asks; the operator's real `[a]` answers.
    let delivery = deliver(&handler, &mut rap, "inbox", "hello from the RAP rail");
    let card = next_requested(&mut events).await;
    card.assert_card_for(&PolicyPrincipal::rap(pin.clone()));
    operator.always_allow().await;
    delivered(delivery).await;
    assert_eq!(
        grants(&host),
        vec![(pin.as_str().to_owned(), Some("pinned".to_owned()))],
        "the RAP card answer wrote the typed pinned grant"
    );

    // A RAP-addressed item for the same pin exists in the journal (written by
    // the production room journal, as an earlier RAP exchange would leave it).
    host.journal
        .room
        .record_event(RoomEvent::RecipientItemReceived {
            message_type: rustain::domain::models::SemanticMessageType::Unknown,
            address: ItemAddress::from_rap_ingress(
                pin.clone(),
                ItemId::from_replay("ri_rap_planted"),
            ),
            task: "rap-planted".to_owned(),
            alias: None,
            content: "an item that arrived over the RAP rail".to_owned(),
            in_reply_to: None,
            capability_hash: None,
            reply_capability_matched: false,
        })
        .await
        .expect("journal the RAP item");
    assert_eq!(
        received_row_for_task(&host.journal, "rap-planted")["address"]["principal"]["kind"],
        "rap",
        "positive control: the planted item is a RAP item"
    );

    // Bound A2A reuses the grant: no card, no parked row.
    let (p1a, p1b) = (host.client(K_P1A), host.client(K_P1B));
    expect_admitted(&host, &mut events, &p1a, "bound1").await;
    expect_admitted(&host, &mut events, &p1b, "bound2").await;
    assert_eq!(grants(&host).len(), 1, "the RAP grant was enough");

    // B4: the bound scope is the bound items only; the RAP item is neither
    // listed nor readable.
    let mut own = vec![
        item_id_for_task(&host.journal, "bound1"),
        item_id_for_task(&host.journal, "bound2"),
    ];
    own.sort();
    let listed = p1a.items_list(2).await;
    let mut seen = listed_ids(&listed.json);
    seen.sort();
    assert_eq!(seen, own, "{}", listed.body);
    assert!(!listed.body.contains("ri_rap_planted"), "{}", listed.body);
    assert_eq!(
        p1a.items_thread(3, "ri_rap_planted", None).await.body,
        p1a.items_thread(3, "ri_never_planted", None).await.body,
        "a RAP item reads exactly like an item that never existed"
    );

    // The grant is the pin's: a different pin's bound key still prompts.
    expect_prompt(
        &host,
        &mut events,
        &host.client(K_P2),
        "p2a",
        &PolicyPrincipal::bound_a2a(pin_of(crate::harness::P2_X)),
    )
    .await;
    drop(operator);
}

/// An override on `p1-alt` ONLY (the alias of K_P1C, not of K_P1A), so a
/// policy chosen by the matched alias would miss K_P1A (`M55`).
const OVERRIDE_TOML: &str = "[interaction.defaults]\nresponse_mode = \"notify-and-wait\"\nnotification = \"queue\"\n\
     [interaction.overrides.\"p1-alt\"]\nresponse_mode = \"notify-and-auto\"\nnotification = \"immediate\"\n\
     auto_response = \"p1 automatic answer\"\n";

/// Two aliases of ONE pin, both overridden (different texts: a genuine
/// conflict), plus an unambiguous override for `p2` as the positive control.
const CONFLICT_TOML: &str = "[interaction.defaults]\nresponse_mode = \"notify-and-wait\"\nnotification = \"queue\"\n\
     [interaction.overrides.\"p1\"]\nresponse_mode = \"notify-and-auto\"\nnotification = \"immediate\"\n\
     auto_response = \"first alias answer\"\n\
     [interaction.overrides.\"p1-alt\"]\nresponse_mode = \"notify-and-auto\"\nnotification = \"immediate\"\n\
     auto_response = \"second alias answer\"\n\
     [interaction.overrides.\"p2\"]\nresponse_mode = \"notify-and-auto\"\nnotification = \"immediate\"\n\
     auto_response = \"p2 automatic answer\"\n";

/// The response policy the runtime applies to `principal` — what `launch`
/// hands `InboundPeerTask.response_policy` (`a2a/server.rs:1599`).
fn applied(host: &BoundHost, principal: &PolicyPrincipal) -> (ResponseMode, Option<String>) {
    let policy = InboundPeerRuntime::response_policy(
        host.server.as_ref(),
        principal,
        SemanticMessageType::Unknown,
    );
    (
        policy.mode,
        policy.auto_response.map(|text| text.to_string()),
    )
}

/// Wait (bounded) for `task`'s wire state as seen by its owner.
async fn await_wire_state(client: &RawClient, task: &str, wanted: &str) {
    let deadline = tokio::time::Instant::now() + SETTLE;
    loop {
        let reply = client.tasks_get(9, task, None).await;
        if wire_state(&reply) == wanted {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{task} never reached {wanted}: {}",
            reply.body
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

/// `K11` part 3 (`AC10(b)`, `P3`, `M22`, `M55`): an `[interaction.overrides]`
/// entry reaches the bound pin by identity (a concrete response mode,
/// different from K_U's, and TOML-implied consent), while two conflicting
/// alias overrides on one pin apply neither.
#[tokio::test(flavor = "multi_thread")]
async fn k11_an_override_reaches_the_bound_pin_and_a_conflicted_pair_applies_neither() {
    setup();
    let pin_p1 = pin_of(crate::harness::P1_X);
    let pin_p2 = pin_of(crate::harness::P2_X);
    let bound_p1 = PolicyPrincipal::bound_a2a(pin_p1.clone());
    let bound_p2 = PolicyPrincipal::bound_a2a(pin_p2.clone());
    let unbound = SubmitterKey::from_api_key(K_U).policy_principal();

    // ── One unambiguous override (on the alias `p1-alt`) ───────────────────
    let mut host = BoundHost::start(HostSpec::b2().interaction(OVERRIDE_TOML)).await;
    let policy = host.effective_policy.clone().expect("composed policy");
    assert_eq!(policy.sender_overrides.len(), 1);
    assert!(policy.sender_conflicts.is_empty());
    let mut operator = host.attach_operator().await;
    let mut events = host.approval_events().await;

    // The override gives the pin a concrete mode, different from K_U's and
    // from the default every other principal gets (`M22`).
    assert_eq!(
        applied(&host, &bound_p1),
        (
            ResponseMode::NotifyAndAuto,
            Some("p1 automatic answer".to_owned())
        )
    );
    assert_eq!(
        applied(&host, &PolicyPrincipal::rap(pin_p1.clone())),
        (
            ResponseMode::NotifyAndAuto,
            Some("p1 automatic answer".to_owned())
        ),
        "RAP and bound A2A share the pin's override"
    );
    assert_eq!(
        applied(&host, &unbound),
        (ResponseMode::NotifyAndWait, None)
    );
    assert_eq!(
        applied(&host, &bound_p2),
        (ResponseMode::NotifyAndWait, None)
    );

    // Front door: the override is the operator's authored consent, so K_P1A
    // (matched alias `p1`, override under `p1-alt`) raises no card and its
    // task is answered automatically; K_U is asked and, once allowed, WAITS.
    let p1a = host.client(K_P1A);
    send(&p1a, "o1").await;
    await_wire_state(&p1a, "o1", "completed").await;
    assert!(
        drain_requested(&mut events).is_empty(),
        "an applicable override implies consent: no card"
    );
    assert!(parked_rows(&host, "o1").is_empty());
    // … for every key of the pin, whatever its alias.
    let p1c = host.client(K_P1C);
    send(&p1c, "o2").await;
    await_wire_state(&p1c, "o2", "completed").await;
    assert!(drain_requested(&mut events).is_empty());
    assert!(grants(&host).is_empty(), "implied consent writes no grant");

    expect_prompt(&host, &mut events, &host.client(K_U), "ou1", &unbound).await;
    allow_always_and_launch(&host, &mut operator, "ou1").await;
    let waiting = host.waiting_node("ou1").await;
    assert_eq!(
        host.persisted_card(&waiting).await.as_deref(),
        Some("[awaiting your response]"),
        "K_U's default mode waits for the operator"
    );
    expect_prompt(&host, &mut events, &host.client(K_P2), "op2", &bound_p2).await;
    drop(operator);

    // ── Two conflicting alias overrides on one pin ─────────────────────────
    // Whichever key of the pin presents (K_P1A matches alias `p1`, K_P1C
    // alias `p1-alt`: the two conflicting ones), no override applies.
    for (key, task) in [(K_P1A, "k1"), (K_P1C, "k3")] {
        let mut host = BoundHost::start(HostSpec::b2().interaction(CONFLICT_TOML)).await;
        let policy = host.effective_policy.clone().expect("composed policy");
        assert_eq!(
            policy.sender_conflicts.len(),
            1,
            "{:?}",
            policy.sender_conflicts
        );
        assert_eq!(
            policy.sender_overrides.len(),
            1,
            "only p2's unambiguous override survives"
        );
        let mut operator = host.attach_operator().await;
        let mut events = host.approval_events().await;

        // Neither alias's override applies to the pin …
        assert_eq!(
            applied(&host, &bound_p1),
            (ResponseMode::NotifyAndWait, None)
        );
        assert_eq!(
            applied(&host, &PolicyPrincipal::rap(pin_p1.clone())),
            (ResponseMode::NotifyAndWait, None)
        );
        // … so the caller is asked (no implied consent) …
        expect_prompt(&host, &mut events, &host.client(key), task, &bound_p1).await;
        allow_always_and_launch(&host, &mut operator, task).await;
        // … and waits (the default) instead of being answered automatically.
        let node = host.waiting_node(task).await;
        assert_eq!(
            host.persisted_card(&node).await.as_deref(),
            Some("[awaiting your response]")
        );
        // Positive control: the unambiguous override for p2 still applies, and
        // implies consent.
        assert_eq!(
            applied(&host, &bound_p2),
            (
                ResponseMode::NotifyAndAuto,
                Some("p2 automatic answer".to_owned())
            )
        );
        let p2 = host.client(K_P2);
        send(&p2, "k2").await;
        await_wire_state(&p2, "k2", "completed").await;
        assert!(drain_requested(&mut events).is_empty());
        drop(operator);
    }
}

// ═════════════════════════════════════════════════════════════════════════
// K14 — bound policy provenance
// ═════════════════════════════════════════════════════════════════════════

/// `RAP`-keyed spec: `p1`/`p1-alt` pinned to the RAP sender's own key.
fn rap_spec(rap: &RapSender) -> HostSpec {
    HostSpec::b2().roster(rap.roster()).interaction(WAIT_TOML)
}

/// `K14` (i) (`AC10(a)`, `PQ1`, `F19`, `M58`): a grant written before typed
/// provenance — legacy (no `principal`) or of a future/`Unknown` kind — still
/// admits the RAP rail, but NEVER a bound caller.
#[tokio::test(flavor = "multi_thread")]
async fn k14_i_a_legacy_or_unknown_kind_grant_never_admits_a_bound_caller() {
    setup();
    for (label, planted) in [
        ("legacy", None),
        ("unknown-kind", Some(PrincipalKind::Unknown)),
    ] {
        let mut rap = RapSender::new();
        let pin = rap.pin();
        let mut host = start_seeded(rap_spec(&rap), &[(pin.clone(), planted)]).await;
        let wire = raw_payloads(&host.journal, "consent_granted");
        assert_eq!(wire.len(), 1, "{label}: {wire:?}");
        assert_eq!(wire[0]["sender"], pin.as_str(), "{label}: {}", wire[0]);
        match planted {
            None => assert!(
                wire[0].get("principal").is_none(),
                "{label}: a legacy row has no principal member: {}",
                wire[0]
            ),
            Some(_) => assert_eq!(wire[0]["principal"], "unknown", "{label}: {}", wire[0]),
        }
        let mut operator = host.attach_operator().await;
        let mut events = host.approval_events().await;
        let handler = open_rap_front_door(&host).await;

        // Positive control: the pre-typed grant IS effective where it always
        // was — the RAP rail from that pin is admitted with no card.
        let projection = projection(&host).await;
        assert_eq!(
            projection.consent_for(&PolicyPrincipal::rap(pin.clone())),
            ConsentState::Trusted,
            "{label}: the legacy slot reaches RAP"
        );
        let delivery = deliver(&handler, &mut rap, "inbox", "rap after a legacy grant");
        delivered(delivery).await;
        assert!(
            drain_requested(&mut events).is_empty(),
            "{label}: RAP is admitted by the pre-typed grant, without a card"
        );

        // The bound caller of the same pin is NOT: it is asked.
        assert_eq!(
            projection.consent_for(&PolicyPrincipal::bound_a2a(pin.clone())),
            ConsentState::None,
            "{label}: the bound arm never consults the legacy slot"
        );
        expect_prompt(
            &host,
            &mut events,
            &host.client(K_P1A),
            "l1",
            &PolicyPrincipal::bound_a2a(pin.clone()),
        )
        .await;
        allow_always_and_launch(&host, &mut operator, "l1").await;
        assert_eq!(
            grants(&host),
            vec![
                (
                    pin.as_str().to_owned(),
                    planted.map(|_| "unknown".to_owned())
                ),
                (pin.as_str().to_owned(), Some("pinned".to_owned())),
            ],
            "{label}: the planted row stays; `[a]` adds a typed pinned row"
        );
        // The typed grant admits the pin's other key.
        expect_admitted(&host, &mut events, &host.client(K_P1B), "l2").await;
        drop(operator);
    }
}

/// Roster pin `pc` whose raw key is `sha256("apikey:" + b64url(sha256(K_U)))`
/// (the 32 bytes behind `SubmitterKey::from_api_key(K_U)`'s pseudonym), so
/// `PeerId(pc) == pseudo(K_U)` (`A14`, `PQ1`, `F19(b)`).
fn collision_x() -> [u8; 32] {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(K_U.as_bytes());
    let handle = format!(
        "apikey:{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
    );
    Sha256::digest(handle.as_bytes()).into()
}

fn collision_spec() -> HostSpec {
    HostSpec::with_server(serde_json::json!({
        "admission": "allow",
        "peerKeys": { K_P2_ENV: "pc" },
        "apiKeys": [K_U_ENV],
    }))
    .roster(Roster::standard().pinned("pc", collision_x()))
    .interaction(WAIT_TOML)
}

/// `K14` (ii) (`PQ1`, `F19(b)`): a bound pin whose `PeerId` EQUALS an unbound
/// key's pseudonym keeps two separate consent cells. Told apart by journaled
/// slot and admission outcome (never by card text), in both orders.
#[tokio::test(flavor = "multi_thread")]
async fn k14_ii_an_equal_peer_id_pseudonym_and_bound_pin_keep_separate_consent_slots() {
    setup();
    let pseudo = SubmitterKey::from_api_key(K_U);
    let collided = pin_of(collision_x());
    assert_eq!(
        collided,
        pseudo.pseudonymous_peer_id(),
        "PeerId(pc) == pseudo(K_U)"
    );
    let same = collided.as_str().to_owned();
    let as_pseudonym = PolicyPrincipal::Pseudonymous(collided.clone());
    let as_bound = PolicyPrincipal::bound_a2a(collided.clone());
    assert_eq!(pseudo.policy_principal(), as_pseudonym);
    assert_ne!(as_pseudonym, as_bound, "same label, different authority");

    // ── Order 1: K_U's pseudonymous grant first ────────────────────────────
    let mut host = BoundHost::start(collision_spec()).await;
    let mut operator = host.attach_operator().await;
    let mut events = host.approval_events().await;
    let (unbound, bound) = (host.client(K_U), host.client(K_P2));

    expect_prompt(&host, &mut events, &unbound, "u1", &as_pseudonym).await;
    allow_always_and_launch(&host, &mut operator, "u1").await;
    assert_eq!(
        grants(&host),
        vec![(same.clone(), Some("pseudonymous".to_owned()))]
    );
    // Positive control: K_U's own second send is admitted.
    expect_admitted(&host, &mut events, &unbound, "u2").await;
    // The bound caller of the equal-labelled pin is still asked.
    {
        let cells = projection(&host).await;
        assert_eq!(cells.consent_for(&as_pseudonym), ConsentState::Trusted);
        assert_eq!(cells.consent_for(&as_bound), ConsentState::None);
        assert_eq!(
            cells.slot_state(ConsentSlot::Pinned, &collided),
            ConsentState::None
        );
    }
    expect_prompt(&host, &mut events, &bound, "c1", &as_bound).await;
    allow_always_and_launch(&host, &mut operator, "c1").await;
    assert_eq!(
        grants(&host),
        vec![
            (same.clone(), Some("pseudonymous".to_owned())),
            (same.clone(), Some("pinned".to_owned())),
        ],
        "both typed grants coexist under one label"
    );
    {
        let cells = projection(&host).await;
        assert_eq!(
            cells.slot_state(ConsentSlot::Pseudonymous, &collided),
            ConsentState::Trusted
        );
        assert_eq!(
            cells.slot_state(ConsentSlot::Pinned, &collided),
            ConsentState::Trusted
        );
    }
    expect_admitted(&host, &mut events, &bound, "c2").await;
    expect_admitted(&host, &mut events, &unbound, "u3").await;
    drop(operator);

    // ── Order 2: the bound pin's grant first; it never admits K_U ──────────
    let mut host = BoundHost::start(collision_spec()).await;
    let mut operator = host.attach_operator().await;
    let mut events = host.approval_events().await;
    let (unbound, bound) = (host.client(K_U), host.client(K_P2));

    expect_prompt(&host, &mut events, &bound, "c1", &as_bound).await;
    allow_always_and_launch(&host, &mut operator, "c1").await;
    assert_eq!(
        grants(&host),
        vec![(same.clone(), Some("pinned".to_owned()))]
    );
    expect_admitted(&host, &mut events, &bound, "c2").await;
    {
        let cells = projection(&host).await;
        assert_eq!(cells.consent_for(&as_bound), ConsentState::Trusted);
        assert_eq!(cells.consent_for(&as_pseudonym), ConsentState::None);
    }
    expect_prompt(&host, &mut events, &unbound, "u1", &as_pseudonym).await;
    allow_always_and_launch(&host, &mut operator, "u1").await;
    assert_eq!(
        grants(&host),
        vec![
            (same.clone(), Some("pinned".to_owned())),
            (same.clone(), Some("pseudonymous".to_owned())),
        ]
    );
    expect_admitted(&host, &mut events, &unbound, "u2").await;
    expect_admitted(&host, &mut events, &bound, "c3").await;
    drop(operator);
}

/// `K14` (iii) (`OQ3` = A): a typed `Pinned` grant is shared by the RAP rail
/// and bound A2A in both directions. The legs run sequentially; each card is
/// asserted on its own.
#[tokio::test(flavor = "multi_thread")]
async fn k14_iii_a_typed_pinned_grant_is_shared_between_the_rap_rail_and_bound_a2a() {
    setup();

    // ── RAP card answered → bound admitted ─────────────────────────────────
    let mut rap = RapSender::new();
    let pin = rap.pin();
    let mut host = BoundHost::start(rap_spec(&rap)).await;
    let mut operator = host.attach_operator().await;
    let mut events = host.approval_events().await;
    let handler = open_rap_front_door(&host).await;
    let delivery = deliver(&handler, &mut rap, "inbox", "rap asks first");
    next_requested(&mut events)
        .await
        .assert_card_for(&PolicyPrincipal::rap(pin.clone()));
    operator.always_allow().await;
    delivered(delivery).await;
    assert_eq!(
        grants(&host),
        vec![(pin.as_str().to_owned(), Some("pinned".to_owned()))]
    );
    expect_admitted(&host, &mut events, &host.client(K_P1A), "after-rap").await;
    assert_eq!(grants(&host).len(), 1);
    drop(operator);

    // ── Bound card answered → the pin's other key and RAP admitted ─────────
    let mut rap = RapSender::new();
    let pin = rap.pin();
    let mut host = BoundHost::start(rap_spec(&rap)).await;
    let mut operator = host.attach_operator().await;
    let mut events = host.approval_events().await;
    let handler = open_rap_front_door(&host).await;
    expect_prompt(
        &host,
        &mut events,
        &host.client(K_P1A),
        "bound-first",
        &PolicyPrincipal::bound_a2a(pin.clone()),
    )
    .await;
    allow_always_and_launch(&host, &mut operator, "bound-first").await;
    assert_eq!(
        grants(&host),
        vec![(pin.as_str().to_owned(), Some("pinned".to_owned()))]
    );
    expect_admitted(&host, &mut events, &host.client(K_P1B), "bound-second").await;
    let delivery = deliver(&handler, &mut rap, "inbox", "rap after a bound grant");
    delivered(delivery).await;
    assert!(
        drain_requested(&mut events).is_empty(),
        "RAP from the pin is admitted by the bound `[a]`, without a card"
    );
    assert_eq!(grants(&host).len(), 1);
    drop(operator);
}

/// `K14` (iv) (`AC10(a)`): `/team untrust p1` — the production operator rail
/// writing a bare `ConsentRevoked` — revokes the shared cell, so the pin's
/// other key is asked again.
#[tokio::test(flavor = "multi_thread")]
async fn k14_iv_untrusting_the_pin_makes_its_bound_keys_prompt_again() {
    setup();
    let mut host = BoundHost::start(wait_spec()).await;
    let mut operator = host.attach_operator().await;
    let mut events = host.approval_events().await;
    let pin = pin_of(crate::harness::P1_X);
    let bound = PolicyPrincipal::bound_a2a(pin.clone());

    expect_prompt(&host, &mut events, &host.client(K_P1A), "r1", &bound).await;
    allow_always_and_launch(&host, &mut operator, "r1").await;
    // Positive control: before the revoke the pin's other key is admitted.
    expect_admitted(&host, &mut events, &host.client(K_P1B), "r2").await;

    let message = team(&host, "/team untrust p1").await;
    assert!(
        message.starts_with("Revoked trust for p1"),
        "the operator rail revoked: {message}"
    );
    let revoked = raw_payloads(&host.journal, "consent_revoked");
    assert_eq!(revoked.len(), 1, "{revoked:?}");
    assert_eq!(revoked[0]["sender"], pin.as_str(), "{}", revoked[0]);
    assert!(
        revoked[0].get("principal").is_none(),
        "a bare revoke carries no principal: {}",
        revoked[0]
    );

    expect_prompt(&host, &mut events, &host.client(K_P1B), "r3", &bound).await;
    drop(operator);
}

/// `K14` (v) (`AC10(e)`, `D1`, `M63`): a bound caller's approval carries the
/// bound provenance IN MEMORY — `Requested.source.policy_principal()` is
/// `bound_a2a(pin)` with `peer_id == pin` — its `AdmissionDeferred` row is
/// typed `pinned`, the `[a]` answer persists a typed `pinned` grant, and the
/// restart reseeding of its tasks finds the status-query row they wrote
/// (no duplicate). The RAP rail's observation reads `via: Rap`.
///
/// ⛔ Nothing is asserted about `ApprovalResolved` provenance (`D1`).
#[tokio::test(flavor = "multi_thread")]
async fn k14_v_bound_provenance_is_carried_in_memory_and_survives_restart_reseeding() {
    setup();
    let pin = pin_of(crate::harness::P1_X);
    let pin_p2 = pin_of(crate::harness::P2_X);
    let mut host = BoundHost::start(wait_spec()).await;
    let mut operator = host.attach_operator().await;
    let mut events = host.approval_events().await;
    let (p1a, p2) = (host.client(K_P1A), host.client(K_P2));

    // The bound caller's ask: the in-memory provenance on the production path.
    let card = expect_prompt(
        &host,
        &mut events,
        &p1a,
        "v1",
        &PolicyPrincipal::bound_a2a(pin.clone()),
    )
    .await;
    match card.principal() {
        PolicyPrincipal::Pinned { peer, via } => {
            assert_eq!(peer, &pin);
            assert_eq!(
                *via,
                PinnedVia::BoundA2a,
                "a bound caller is not `rap(pin)`"
            );
        }
        other => panic!("a pinned principal, got {other:?}"),
    }
    assert_eq!(
        parked_rows(&host, "v1"),
        vec![(pin.as_str().to_owned(), Some("pinned".to_owned()))]
    );
    // The operator's `[a]` persists the typed grant (possible only because the
    // provenance is present).
    allow_always_and_launch(&host, &mut operator, "v1").await;
    let granted = raw_payloads(&host.journal, "consent_granted");
    assert_eq!(granted.len(), 1, "{granted:?}");
    assert_eq!(granted[0]["principal"], "pinned", "{}", granted[0]);
    assert_eq!(granted[0]["sender"], pin.as_str(), "{}", granted[0]);

    // Another pin's task is left unanswered: it restarts through the
    // pending-file block instead of the node (orphan) block.
    expect_prompt(
        &host,
        &mut events,
        &p2,
        "v2",
        &PolicyPrincipal::bound_a2a(pin_p2.clone()),
    )
    .await;

    // Each owner's FIRST poll writes one status-query row (typed `pinned`).
    for (client, task, spoke) in [(&p1a, "v1", &pin), (&p2, "v2", &pin_p2)] {
        client.tasks_get(3, task, None).await;
        assert_eq!(
            status_rows(&host, task),
            vec![(spoke.as_str().to_owned(), Some("pinned".to_owned()))],
            "{task}: the first poll wrote one typed row"
        );
    }
    let spec = host.spec.clone();
    drop(operator);
    drop(events);
    let host = host.restart(spec).await;
    let (p1a, p2) = (host.client(K_P1A), host.client(K_P2));

    // After the restart both bound tasks are reseeded under their pins: the
    // owner reads `failed`, the surviving status-query row is found (a repeat
    // poll never appends), and no second approval row appears.
    for (client, task, spoke) in [(&p1a, "v1", &pin), (&p2, "v2", &pin_p2)] {
        for _ in 0..2 {
            let reply = client.tasks_get(4, task, None).await;
            assert_eq!(wire_state(&reply), "failed", "{task}: {}", reply.body);
            assert_eq!(
                status_rows(&host, task),
                vec![(spoke.as_str().to_owned(), Some("pinned".to_owned()))],
                "{task}: the reseeded observation finds its row; no duplicate"
            );
        }
        assert_eq!(parked_rows(&host, task).len(), 1, "{task}: one parked row");
    }

    // ── Positive control: the RAP rail observes `via: Rap` ─────────────────
    let mut rap = RapSender::new();
    let rap_pin = rap.pin();
    let mut host = BoundHost::start(rap_spec(&rap)).await;
    let mut operator = host.attach_operator().await;
    let mut events = host.approval_events().await;
    let handler = open_rap_front_door(&host).await;
    let delivery = deliver(&handler, &mut rap, "inbox", "rap ask");
    let card = next_requested(&mut events).await;
    card.assert_card_for(&PolicyPrincipal::rap(rap_pin.clone()));
    match card.principal() {
        PolicyPrincipal::Pinned { via, .. } => assert_eq!(*via, PinnedVia::Rap),
        other => panic!("a pinned principal, got {other:?}"),
    }
    operator.always_allow().await;
    delivered(delivery).await;
    drop(operator);
}

/// `K14` (vii) (`P21`, `P22`, `M69`, `M70`, `M71`): one consent cell, one
/// card, one `[a]`.
///
/// Host 1 — a RAP sender for `p1` asks first; bound K_P1A sends while that
/// group is pending: exactly ONE `Requested`, with today's `Rap` line 4, and
/// `pending_sender_consent()` returns that request with the same preview
/// bytes; one `[a]` admits BOTH tasks and journals exactly one typed grant.
///
/// Host 2 — bound K_P1B asks first: line 4 is `"[a] " +
/// BOUND_KEY_CONSENT_LINE` (the constant, ⛔ not a copied literal) and the
/// other five lines are today's copy; the RAP sender then joins the same card.
#[tokio::test(flavor = "multi_thread")]
async fn k14_vii_one_cell_one_card_and_a_truthful_bound_clause() {
    setup();

    // ── Host 1: RAP opens the group ────────────────────────────────────────
    let mut rap = RapSender::new();
    let pin = rap.pin();
    let mut host = BoundHost::start(rap_spec(&rap)).await;
    let mut operator = host.attach_operator().await;
    let mut events = host.approval_events().await;
    let handler = open_rap_front_door(&host).await;
    let approval = host
        .core
        .ensure_runtime()
        .await
        .expect("daemon runtime")
        .approval
        .clone();

    let delivery = deliver(&handler, &mut rap, "inbox", "rap asks first");
    let opened = next_requested(&mut events).await;
    opened.assert_card_for(&PolicyPrincipal::rap(pin.clone()));
    assert_card(&opened.preview, &pin, RAP_TRUST_LINE);

    // The bound caller joins the pending group: no second card.
    send(&host.client(K_P1A), "joined").await;
    assert!(
        drain_requested(&mut events).is_empty(),
        "the bound task joined the RAP-opened group: exactly one `Requested`"
    );
    assert_eq!(
        parked_rows(&host, "joined"),
        vec![(pin.as_str().to_owned(), Some("pinned".to_owned()))],
        "the joined task is parked under its own bound principal"
    );
    let pending = approval.pending_sender_consent().await;
    assert_eq!(pending.len(), 1, "{pending:?}");
    assert_eq!(pending[0].0, opened.id);
    assert_eq!(
        pending[0].1, opened.preview,
        "the lag re-render preview equals the `Requested` preview bytes"
    );
    assert_eq!(pending[0].2, opened.risk);

    // One `[a]` releases BOTH tasks and journals exactly one typed grant.
    operator.always_allow().await;
    delivered(delivery).await;
    let node = host.waiting_node("joined").await;
    host.pending_card(&node).await;
    assert_eq!(
        grants(&host),
        vec![(pin.as_str().to_owned(), Some("pinned".to_owned()))],
        "exactly one ConsentGranted for p1"
    );
    assert!(drain_requested(&mut events).is_empty());
    assert!(approval.pending_sender_consent().await.is_empty());
    drop(operator);

    // ── Host 2: the bound caller opens the group ───────────────────────────
    let mut rap = RapSender::new();
    let pin = rap.pin();
    let mut host = BoundHost::start(rap_spec(&rap)).await;
    let mut operator = host.attach_operator().await;
    let mut events = host.approval_events().await;
    let handler = open_rap_front_door(&host).await;
    let approval = host
        .core
        .ensure_runtime()
        .await
        .expect("daemon runtime")
        .approval
        .clone();

    let opened = expect_prompt(
        &host,
        &mut events,
        &host.client(K_P1B),
        "bound-opens",
        &PolicyPrincipal::bound_a2a(pin.clone()),
    )
    .await;
    assert_card(
        &opened.preview,
        &pin,
        &format!("[a] {BOUND_KEY_CONSENT_LINE}"),
    );
    let pending = approval.pending_sender_consent().await;
    assert_eq!(pending.len(), 1, "{pending:?}");
    assert_eq!(pending[0].0, opened.id);
    assert_eq!(
        pending[0].1, opened.preview,
        "the lag re-render preview equals the `Requested` preview bytes"
    );

    // The RAP sender joins the bound-opened card; one `[a]` releases both.
    let delivery = deliver(&handler, &mut rap, "inbox", "rap joins a bound card");
    // The frame reaches the consent registration through the bus and the
    // ingest worker; give it time to open a (wrongly) second card.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(
        !delivery.is_finished(),
        "the RAP frame is waiting on the shared card"
    );
    assert!(
        drain_requested(&mut events).is_empty(),
        "the RAP sender joined the bound-opened group: no second `Requested`"
    );
    assert_eq!(approval.pending_sender_consent().await.len(), 1);
    operator.always_allow().await;
    delivered(delivery).await;
    let node = host.waiting_node("bound-opens").await;
    host.pending_card(&node).await;
    assert_eq!(
        grants(&host),
        vec![(pin.as_str().to_owned(), Some("pinned".to_owned()))],
        "exactly one ConsentGranted for p1"
    );
    drop(operator);
}
