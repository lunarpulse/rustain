//! Story 19.23 K02: every refusal a `--holders-of` send can reach over the
//! wire refuses the whole action on its request key before any journal write
//! or recipient I/O; the latest live statement per sender decides, removal and
//! release are honoured, and disagreeing staters are never silently resolved.
//!
//! Received holder statements are real HTTP `message/send` lists into sender
//! N; own statements are N's own attached `story_assignment` sends. The
//! `Unrecognized` holder and an unreadable journal are not reachable without
//! fabricating a holder or a journal, so they stay with the pure resolver
//! units (`story19_23_core_*`, `story19_23_resolution_reads_one_snapshot_*`).
//! The standalone rail's refusal is driven through the real `/team send`
//! command dispatcher at the end of this module.
use std::sync::Arc;

use super::attached_harness::{
    AttachedClient, AttachedDaemon, FIXTURE_TIMEOUT, InboundHttpHost, InboundSenderIdentity,
    RecipientHost,
};
use super::harness::{Journal, Sender, read_room_events};
use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
use rustain::adapters::daemon::protocol::{
    ClientFrame, ConnectionTier, DaemonFrame, ProtocolError, RecipientAction,
    RecipientActionOutcome, RecipientActionTarget, TeamSendOutcome,
};
use rustain::adapters::daemon::server::TeamSendTestHooks;
use rustain::adapters::tui::state::TuiState;
use rustain::domain::models::{
    A2aPeerSpec, IndividualPolicy, PeerId, PrincipalKind, ReceivedStories, RoomEvent,
    SemanticMessageType, StoryManifest, StorySelector,
};
use serde_json::{Value, json};

const SELECTED_FOR: &str = "x-rustain-selected-for";

/// One attempted attached send, shaped as the raw frame carries it.
struct Attempt {
    recipients: Vec<String>,
    kind: Option<SemanticMessageType>,
    stories: Option<StoryManifest>,
    holders: Option<StorySelector>,
}

impl Attempt {
    fn holders(token: &str) -> Self {
        Self::selector(StorySelector::parse(token).unwrap())
    }
    /// Deliberately bypass `StorySelector::parse`: the transparent key
    /// deserializer admits what only the daemon's re-validation can refuse.
    fn raw_holders(value: Value) -> Self {
        Self::selector(serde_json::from_value(value).unwrap())
    }
    fn selector(selector: StorySelector) -> Self {
        Self {
            recipients: Vec::new(),
            kind: Some(SemanticMessageType::DesignUpdate),
            stories: None,
            holders: Some(selector),
        }
    }
}

/// Sender N with its real roster hosts and real inbound list senders.
struct Scene {
    daemon: AttachedDaemon,
    writer: AttachedClient,
    inbound: InboundHttpHost,
    hosts: Vec<(&'static str, RecipientHost)>,
}

impl Scene {
    /// N is NotifyAndWait and every inbound sender already holds a standing
    /// pinned grant, so a received list is journaled without a consent card
    /// and no model turn runs behind the refusal deltas.
    async fn start(
        hosts: Vec<(&'static str, RecipientHost)>,
        roster: Vec<A2aPeerSpec>,
        senders: Vec<InboundSenderIdentity>,
    ) -> Self {
        let principals = senders
            .iter()
            .map(|sender| sender.identity.peer_id.clone())
            .collect::<Vec<_>>();
        let journal = Journal::open().await;
        for principal in &principals {
            journal
                .room
                .record_event(RoomEvent::ConsentGranted {
                    sender: Some(principal.clone()),
                    granted_at: 1_800_000_000_000,
                    principal: Some(PrincipalKind::Pinned),
                })
                .await
                .expect("persist standing inbound sender consent");
        }
        let policy = Arc::new(
            rustain::domain::services::team_policy::resolve_effective_policy(
                &IndividualPolicy::default(),
                None,
                &roster,
            ),
        );
        let room = journal.room.clone();
        let daemon = AttachedDaemon::compose_with_clock_and_test_hooks_and_policy(
            journal,
            roster,
            room,
            Arc::new(rustain::domain::clock::MockClock::at_wall_ms(
                1_800_000_000_000,
            )),
            TeamSendTestHooks::new(None, None, false),
            Some(policy),
        )
        .await;
        for principal in &principals {
            daemon.wait_for_pinned_sender_consent(principal).await;
        }
        let inbound = daemon.serve_http(senders).await;
        let writer =
            AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
        Self {
            daemon,
            writer,
            inbound,
            hosts,
        }
    }

    fn host(&self, name: &str) -> &RecipientHost {
        &self
            .hosts
            .iter()
            .find(|(host, _)| *host == name)
            .unwrap_or_else(|| panic!("no recipient host {name:?}"))
            .1
    }

    /// The actual signer pin of the host behind roster alias `alias`.
    fn pin(&self, host: &str, alias: &str) -> PeerId {
        self.host(host)
            .pinned_peer(alias)
            .pinned_identity()
            .expect("actual recipient signer pin")
    }

    fn posts(&self) -> Vec<usize> {
        self.hosts
            .iter()
            .map(|(_, host)| host.method_count("message/send"))
            .collect()
    }

    /// A real inbound `message/send` list from `sender`; returns after the
    /// HTTP answer (sent after the production launch) and the durable,
    /// readable item.
    async fn receive(&self, sender: &str, task: &str, kind: &str, stories: Value) {
        let result = self
            .inbound
            .sender(sender)
            .message_send_with_metadata(
                task,
                &format!("{task} list body"),
                json!({"x-rustain-message-type": kind, "x-rustain-stories": {"stories": stories}}),
            )
            .await;
        assert!(result.json["error"].is_null(), "{task}: {result:?}");
        self.daemon.wait_for_received_task(task).await;
        assert!(
            read_room_events(&self.daemon.journal)
                .await
                .iter()
                .any(|(_, event)| matches!(
                    event,
                    RoomEvent::RecipientItemReceived {
                        task: received,
                        stories: Some(ReceivedStories::Listed { .. }),
                        ..
                    } if received == task
                )),
            "{task}: the fixture list must be journaled as readable on N"
        );
    }

    /// `/team remove` through the attached frame. Removal answers success
    /// with no frame, so wait for its durable tombstone before going on.
    async fn remove(&mut self, task: &str) {
        let address = read_room_events(&self.daemon.journal)
            .await
            .into_iter()
            .find_map(|(_, event)| match event {
                RoomEvent::RecipientItemReceived {
                    task: received,
                    address,
                    ..
                } if received == task => Some(address),
                _ => None,
            })
            .expect("the received list is on N");
        self.writer
            .send_frame(ClientFrame::RemoveRecipientItem {
                item_id: address.item().as_str().to_owned(),
            })
            .await;
        tokio::time::timeout(FIXTURE_TIMEOUT, async {
            loop {
                if read_room_events(&self.daemon.journal)
                    .await
                    .iter()
                    .any(|(_, event)| matches!(event, RoomEvent::RecipientItemRemoved { address: removed, .. } if *removed == address))
                {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{task}: removal tombstone deadline"));
    }

    async fn delivered(&mut self, request: &str, recipients: usize) -> String {
        let started = self.writer.wait_send_started(request).await;
        let DaemonFrame::TeamSendStarted { action, .. } = started else {
            panic!("{request}: expected TeamSendStarted, got {started:?}")
        };
        for index in 0..recipients {
            let frame = self.writer.wait_send_settled(request, index).await;
            assert!(
                matches!(
                    frame,
                    DaemonFrame::TeamSendSettled {
                        outcome: TeamSendOutcome::Delivered,
                        ..
                    }
                ),
                "{request}[{index}]: {frame:?}"
            );
        }
        action.as_str().to_owned()
    }

    /// N's own story statement: an ordinary attached `story_assignment`.
    async fn assign(&mut self, request: &str, alias: &str, manifest: &str) {
        self.writer
            .send_team(
                request,
                vec![alias.to_owned()],
                format!("{request} assignment"),
                Some(SemanticMessageType::StoryAssignment),
                None,
                Some(StoryManifest::parse(manifest).unwrap()),
                None,
            )
            .await;
        self.delivered(request, 1).await;
    }

    /// A plain, explicitly addressed send: proves the hosts are reachable.
    async fn plain(&mut self, request: &str, alias: &str) {
        let before = self.posts();
        self.writer
            .send_team(
                request,
                vec![alias.to_owned()],
                format!("{request} plain body"),
                Some(SemanticMessageType::DesignUpdate),
                None,
                None,
                None,
            )
            .await;
        self.delivered(request, 1).await;
        let after = self.posts();
        for ((name, _), (before, after)) in self.hosts.iter().zip(before.into_iter().zip(after)) {
            assert_eq!(
                after - before,
                usize::from(*name == alias),
                "{request}: {name}"
            );
        }
    }

    /// A `--holders-of` design update that must reach exactly `expected`, in
    /// order, each POST and recipient journal carrying only its own keys
    /// (`(alias, "K1,K2")`, keys in selector order).
    async fn selected(&mut self, request: &str, token: &str, expected: &[(&str, &str)]) {
        let before = self.posts();
        let body = format!("{request} design update");
        self.writer
            .send_team(
                request,
                Vec::new(),
                body.clone(),
                Some(SemanticMessageType::DesignUpdate),
                None,
                None,
                Some(StorySelector::parse(token).unwrap()),
            )
            .await;
        let action = self.delivered(request, expected.len()).await;
        let recipients = read_room_events(&self.daemon.journal)
            .await
            .into_iter()
            .find_map(|(_, event)| match event {
                RoomEvent::TeamSendActionRecorded {
                    action: recorded,
                    recipients,
                    stories,
                    ..
                } if recorded.as_str() == action => {
                    assert!(stories.is_empty(), "{request}: a selection is not a list");
                    Some(recipients.into_iter().map(|r| r.alias).collect::<Vec<_>>())
                }
                _ => None,
            })
            .expect("the started action is recorded on N");
        assert_eq!(
            recipients,
            expected.iter().map(|(alias, _)| *alias).collect::<Vec<_>>(),
            "{request}"
        );
        let after = self.posts();
        for ((name, _), (before, after)) in self.hosts.iter().zip(before.into_iter().zip(after)) {
            let selected = expected.iter().any(|(alias, _)| alias == name);
            assert_eq!(after - before, usize::from(selected), "{request}: {name}");
        }
        for (alias, keys) in expected {
            let keys = keys.split(',').collect::<Vec<_>>();
            let host = self.host(alias);
            let posts = host
                .message_send_posts()
                .into_iter()
                .filter(|post| post["params"]["message"]["parts"][0]["text"] == body.as_str())
                .collect::<Vec<_>>();
            assert_eq!(posts.len(), 1, "{request}: one POST to {alias}");
            assert_eq!(
                posts[0]["params"]["message"]["metadata"][SELECTED_FOR],
                json!({"stories": keys}),
                "{request}: {alias}'s carrier"
            );
            let stored = read_room_events(&host.journal)
                .await
                .into_iter()
                .find_map(|(_, event)| match event {
                    RoomEvent::RecipientItemReceived {
                        content,
                        selected_for,
                        ..
                    } if content == body => Some(serde_json::to_value(selected_for).unwrap()),
                    _ => None,
                })
                .expect("the real recipient journaled the item");
            assert_eq!(
                stored,
                json!({"state": "Listed", "stories": keys}),
                "{request}: {alias}'s journal"
            );
        }
    }

    /// The attempt is refused with exactly `expected` on its request key: it
    /// never starts or settles, N's journal gains no row and no host is POSTed.
    async fn refused(&mut self, request: &str, attempt: Attempt, expected: &str) {
        let rows = self.daemon.journal.entries().await.len();
        let posts = self.posts();
        self.writer
            .send_team(
                request,
                attempt.recipients,
                format!("{request} must not leave N"),
                attempt.kind,
                None,
                attempt.stories,
                attempt.holders,
            )
            .await;
        let frame = self.writer.wait_send_started(request).await;
        let DaemonFrame::RecipientActionResult {
            action: RecipientAction::TeamSend,
            target: RecipientActionTarget::Request { request: actual },
            outcome:
                RecipientActionOutcome::Refused {
                    error: ProtocolError::Malformed(text),
                },
        } = frame
        else {
            panic!("{request}: expected a request-keyed malformed refusal, got {frame:?}")
        };
        assert_eq!(actual, request);
        assert_eq!(text, expected, "{request}");
        assert!(
            self.writer
                .frames_for_request(request)
                .iter()
                .all(|frame| !matches!(
                    frame,
                    DaemonFrame::TeamSendStarted { .. } | DaemonFrame::TeamSendSettled { .. }
                )),
            "{request}: a refused send never starts or settles"
        );
        assert_eq!(
            self.daemon.journal.entries().await.len(),
            rows,
            "{request}: zero new journal rows on N"
        );
        assert_eq!(
            self.posts(),
            posts,
            "{request}: zero message/send on every host"
        );
    }

    async fn stop(self) {
        self.writer.detach().await;
        self.inbound.stop().await;
        self.daemon.stop().await;
        for (_, host) in self.hosts {
            host.stop().await;
        }
    }
}

fn pinned(key: &str, display_alias: &str, pin: &PeerId) -> Value {
    json!({"key": key, "alias": display_alias, "peer": pin.as_str()})
}

async fn jun_and_aisha(senders: Vec<InboundSenderIdentity>) -> Scene {
    let jun = RecipientHost::start(A2aAdmissionPolicy::Allow).await;
    let aisha = RecipientHost::start(A2aAdmissionPolicy::Allow).await;
    let roster = vec![jun.pinned_peer("jun"), aisha.pinned_peer("aisha")];
    Scene::start(vec![("jun", jun), ("aisha", aisha)], roster, senders).await
}

#[tokio::test]
async fn a3_k02_every_http_reachable_holder_row_refuses_the_whole_send() {
    let jun = RecipientHost::start(A2aAdmissionPolicy::Allow).await;
    let aisha = RecipientHost::start(A2aAdmissionPolicy::Allow).await;
    let twin = RecipientHost::start(A2aAdmissionPolicy::Allow).await;
    // Two roster aliases deliberately pin the same real host.
    let roster = vec![
        jun.pinned_peer("jun"),
        aisha.pinned_peer("aisha"),
        twin.pinned_peer("twin-a"),
        twin.pinned_peer("twin-b"),
    ];
    let mut scene = Scene::start(
        vec![("jun", jun), ("aisha", aisha), ("twin", twin)],
        roster,
        vec![InboundSenderIdentity::new("lead-l")],
    )
    .await;
    let jun_pin = scene.pin("jun", "jun");
    let twin_pin = scene.pin("twin", "twin-a");
    let stranger = PeerId::from_public_key(&[7; 32]).unwrap();
    scene
        .receive(
            "lead-l",
            "l-rows",
            "story_assignment",
            json!([
                pinned("S8-01", "jun-as-l-calls-them", &jun_pin),
                {"key": "S8-02"},
                {"key": "S8-03", "you": true},
                // The display alias equals N's roster alias, but has no pin:
                // it must never be matched by alias text.
                {"key": "S8-04", "alias": "jun"},
                pinned("S8-05", "ghost", &stranger),
                pinned("S8-06", "twin", &twin_pin),
            ]),
        )
        .await;

    for (request, token, expected) in [
        (
            "a3-k02-not-listed",
            "S8-99",
            "no story list on this host names 'S8-99'",
        ),
        (
            "a3-k02-no-holder",
            "S8-02",
            "story 'S8-02' is assigned to no one",
        ),
        (
            "a3-k02-held-here",
            "S8-03",
            "story 'S8-03' is assigned to you",
        ),
        (
            "a3-k02-unpinned",
            "S8-04",
            "story 'S8-04' is assigned to 'jun' without a pinned identity; address them by alias",
        ),
        (
            "a3-k02-no-roster-match",
            "S8-05",
            "no roster entry here matches the holder of story 'S8-05'",
        ),
        (
            "a3-k02-several-roster-matches",
            "S8-06",
            "the holder of story 'S8-06' matches more than one roster entry: twin-a, twin-b",
        ),
        // One resolvable key does not carry the action: the whole send is refused.
        (
            "a3-k02-whole-action",
            "S8-01,S8-02",
            "story 'S8-02' is assigned to no one",
        ),
    ] {
        scene
            .refused(request, Attempt::holders(token), expected)
            .await;
    }

    // Positive control: the same list selects its pinned holder.
    scene
        .selected("a3-k02-rows-control", "S8-01", &[("jun", "S8-01")])
        .await;
    scene.stop().await;
}

#[tokio::test]
async fn a3_k02_raw_frames_are_refused_on_the_request_key_without_rows_or_posts() {
    let mut scene = jun_and_aisha(vec![InboundSenderIdentity::new("lead-l")]).await;
    let jun_pin = scene.pin("jun", "jun");
    let aisha_pin = scene.pin("aisha", "aisha");
    scene
        .receive(
            "lead-l",
            "l-raw",
            "story_assignment",
            json!([
                pinned("S8-01", "jun-by-l", &jun_pin),
                pinned("S8-02", "aisha-by-l", &aisha_pin),
            ]),
        )
        .await;

    let invalid_key = "story key 'bad!' is not valid: 1–32 characters from A–Z a–z 0–9 . _ -, starting with a letter or digit";
    let too_many = json!({"stories": (0..65).map(|n| format!("S{n}")).collect::<Vec<_>>()});
    let cases = [
        (
            "a3-k02-raw-invalid-key",
            Attempt::raw_holders(json!({"stories": ["S8-01", "bad!"]})),
            invalid_key,
        ),
        (
            "a3-k02-raw-duplicate",
            Attempt::raw_holders(json!({"stories": ["S8-01", "S8-01"]})),
            "story key 'S8-01' is listed twice",
        ),
        (
            "a3-k02-raw-65-keys",
            Attempt::raw_holders(too_many),
            "a send lists at most 64 stories",
        ),
        (
            "a3-k02-raw-empty",
            Attempt::raw_holders(json!({"stories": []})),
            "Missing story list after '/team send --holders-of'",
        ),
        (
            "a3-k02-raw-with-recipients",
            Attempt {
                recipients: vec!["jun".into()],
                ..Attempt::holders("S8-01")
            },
            "'--holders-of' cannot be combined with explicit recipients",
        ),
        (
            "a3-k02-raw-with-stories",
            Attempt {
                kind: Some(SemanticMessageType::StoryAssignment),
                stories: Some(StoryManifest::parse("S8-01=jun").unwrap()),
                ..Attempt::holders("S8-01")
            },
            "'--holders-of' cannot be combined with '--stories'",
        ),
        (
            "a3-k02-raw-repeated-alias",
            Attempt {
                recipients: vec!["jun".into(), "jun".into()],
                kind: Some(SemanticMessageType::DesignUpdate),
                stories: None,
                holders: None,
            },
            "Invalid or duplicate peer id in '/team send'",
        ),
    ];
    for (request, attempt, expected) in cases {
        scene.refused(request, attempt, expected).await;
    }

    // Positive controls on the same connection: both peers are reachable and
    // the same selection resolves once the frame is well formed.
    scene.plain("a3-k02-raw-plain-control", "jun").await;
    scene
        .selected(
            "a3-k02-raw-select-control",
            "S8-01,S8-02",
            &[("jun", "S8-01"), ("aisha", "S8-02")],
        )
        .await;
    scene.stop().await;
}

#[tokio::test]
async fn a3_k02_latest_live_statement_per_sender_decides_release_and_removal() {
    let mut scene = jun_and_aisha(vec![InboundSenderIdentity::new("lead-l")]).await;
    let jun_pin = scene.pin("jun", "jun");
    let aisha_pin = scene.pin("aisha", "aisha");
    scene
        .receive(
            "lead-l",
            "l-first",
            "story_assignment",
            json!([
                pinned("S7-04", "jun-by-l", &jun_pin),
                pinned("S7-06", "aisha-by-l", &aisha_pin),
            ]),
        )
        .await;
    scene
        .selected("a3-k02-first", "S7-06", &[("aisha", "S7-06")])
        .await;

    // Supersession: L's later list moves S7-06 to jun.
    scene
        .receive(
            "lead-l",
            "l-moves",
            "story_assignment",
            json!([pinned("S7-06", "jun-by-l", &jun_pin)]),
        )
        .await;
    scene
        .selected("a3-k02-moved", "S7-06", &[("jun", "S7-06")])
        .await;
    // Both keys now resolve to jun: one recipient, its keys in selector order.
    scene
        .selected("a3-k02-merged", "S7-06,S7-04", &[("jun", "S7-06,S7-04")])
        .await;

    // A later release states that no one holds S7-06.
    scene
        .receive(
            "lead-l",
            "l-release",
            "scope_change",
            json!([{"key": "S7-06"}]),
        )
        .await;
    scene
        .refused(
            "a3-k02-released",
            Attempt::holders("S7-06"),
            "story 'S7-06' is assigned to no one",
        )
        .await;
    // Removing that newer item restores the older eligible statement.
    scene.remove("l-release").await;
    scene
        .selected("a3-k02-restored", "S7-06", &[("jun", "S7-06")])
        .await;

    // Removal: a fresh key with exactly one live statement.
    scene
        .receive(
            "lead-l",
            "l-fresh",
            "story_assignment",
            json!([pinned("S7-50", "aisha-by-l", &aisha_pin)]),
        )
        .await;
    scene
        .selected("a3-k02-fresh", "S7-50", &[("aisha", "S7-50")])
        .await;
    scene.remove("l-fresh").await;
    scene
        .refused(
            "a3-k02-removed",
            Attempt::holders("S7-50"),
            "no story list on this host names 'S7-50'",
        )
        .await;
    scene.stop().await;
}

#[tokio::test]
async fn a3_k02_disagreeing_staters_refuse_until_one_holder_remains() {
    let mut scene = jun_and_aisha(vec![
        InboundSenderIdentity::new("lead-l"),
        InboundSenderIdentity::new("lead-m"),
    ])
    .await;
    let jun_pin = scene.pin("jun", "jun");
    let aisha_pin = scene.pin("aisha", "aisha");
    let conflict = "story 'S7-04' has different holders in different senders' lists; address recipients by alias";

    // Cross-sender: L names jun, M names aisha.
    scene
        .receive(
            "lead-l",
            "l-jun",
            "story_assignment",
            json!([pinned("S7-04", "jun-by-l", &jun_pin)]),
        )
        .await;
    scene
        .receive(
            "lead-m",
            "m-aisha",
            "story_assignment",
            json!([pinned("S7-04", "aisha-by-m", &aisha_pin)]),
        )
        .await;
    scene
        .refused("a3-k02-cross-sender", Attempt::holders("S7-04"), conflict)
        .await;
    scene.remove("l-jun").await;
    scene
        .selected("a3-k02-m-holder", "S7-04", &[("aisha", "S7-04")])
        .await;

    // Own vs received (OR-1a): N assigned S7-30 to jun; L says aisha.
    scene.assign("a3-k02-own-s7-30", "jun", "S7-30=jun").await;
    scene
        .receive(
            "lead-l",
            "l-aisha-30",
            "story_assignment",
            json!([pinned("S7-30", "aisha-by-l", &aisha_pin)]),
        )
        .await;
    scene
        .refused(
            "a3-k02-own-vs-received",
            Attempt::holders("S7-30"),
            "story 'S7-30' has different holders in different senders' lists; address recipients by alias",
        )
        .await;
    // L's later list and M's list now agree with N's own statement.
    scene
        .receive(
            "lead-l",
            "l-jun-30",
            "story_assignment",
            json!([pinned("S7-30", "jun-by-l", &jun_pin)]),
        )
        .await;
    scene
        .receive(
            "lead-m",
            "m-jun-30",
            "story_assignment",
            json!([pinned("S7-30", "jun-by-m", &jun_pin)]),
        )
        .await;
    scene
        .selected("a3-k02-agreement", "S7-30", &[("jun", "S7-30")])
        .await;
    scene.stop().await;
}

/// The standalone rail: `/team send --holders-of` through the real command
/// dispatcher refuses an unresolvable key with no painted row, no journal
/// row and no POST; its own-statement control reaches the holder.
#[tokio::test]
async fn a3_k02_standalone_unresolvable_selector_paints_no_row_and_records_nothing() {
    let jun = RecipientHost::start(A2aAdmissionPolicy::Allow).await;
    let mut sender = Sender::start(vec![jun.pinned_peer("jun")]).await;
    let mut state = TuiState::new(100, 40);
    let rows = sender.rows().await.len();
    let posts = jun.method_count("message/send");
    sender
        .command(
            &mut state,
            "/team send --type design_update --holders-of S9-99 a3-k02 standalone refused body",
        )
        .await;
    let block = state
        .active_feedback_id
        .clone()
        .expect("the refusal is shown");
    assert_eq!(
        state.feedback_blocks[&block].message,
        "no story list on this host names 'S9-99'"
    );
    assert!(state.team_send_blocks.is_empty(), "no row was painted");
    assert_eq!(sender.rows().await.len(), rows, "nothing was journaled");
    assert_eq!(jun.method_count("message/send"), posts, "nothing was sent");

    // Control: the host's own statement selects jun on the same rail.
    sender
        .command(
            &mut state,
            "/team send --type story_assignment --stories S9-01=jun jun a3-k02 standalone assignment",
        )
        .await;
    sender.settle(&mut state, 1).await;
    let body = "a3-k02 standalone selected body";
    sender
        .command(
            &mut state,
            &format!("/team send --type design_update --holders-of S9-01 {body}"),
        )
        .await;
    let selected_block = state
        .active_feedback_id
        .clone()
        .expect("selected send block");
    assert_eq!(
        state
            .team_send_blocks
            .get(&selected_block)
            .expect("the selected send painted its row")
            .iter()
            .map(|row| row.alias.as_str())
            .collect::<Vec<_>>(),
        ["jun"]
    );
    sender.settle(&mut state, 1).await;
    let painted = &state.feedback_blocks[&selected_block].message;
    assert!(
        painted.contains("jun") && painted.contains("delivered"),
        "{painted}"
    );
    assert_eq!(jun.method_count("message/send"), posts + 2);
    let post = jun
        .message_send_posts()
        .into_iter()
        .find(|post| post["params"]["message"]["parts"][0]["text"] == body)
        .expect("the selected body reached jun");
    assert_eq!(
        post["params"]["message"]["metadata"][SELECTED_FOR],
        json!({"stories": ["S9-01"]})
    );
    jun.stop().await;
}
