//! Story 19.23 K03: the `x-rustain-selected-for` carrier enters through real
//! HTTP `message/send`. Absent → no selection; every D5 malformed form →
//! `Unreadable` while the message is still admitted; the raw value is never
//! stored or logged. Evidence is the journal the production ingress wrote.
use std::path::Path;
use std::sync::Arc;

use super::attached_harness::{
    AttachedClient, AttachedDaemon, InboundHttpHost, InboundHttpSender, InboundSenderIdentity,
    pinned_peer_for_inbound_sender,
};
use super::harness::{Journal, read_room_events};
use rustain::adapters::daemon::protocol::ConnectionTier;
use rustain::adapters::daemon::server::TeamSendTestHooks;
use rustain::domain::models::{
    IndividualDefaults, IndividualPolicy, NotificationUrgency, PrincipalKind, ResponseMode,
    RoomEvent, SemanticMessageType, TeamPolicy, TeamTypeOverride,
};
use serde_json::{Value, json};

/// Appears only inside malformed carriers or ignored fields, so any
/// appearance in durable storage or logs is the raw carrier leaking.
const RAW_MARKER: &str = "a3k03-raw-selection-marker";
/// A body is durable by design: the storage scan must find this one.
const BODY_CANARY: &str = "a3k03-durable-body-canary";
const SENDER: &str = "selection-sender";
const KINDS: [(&str, SemanticMessageType); 2] = [
    ("design_update", SemanticMessageType::DesignUpdate),
    ("consultation", SemanticMessageType::Consultation),
];

#[derive(Clone, Default)]
struct LogBuffer(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for LogBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuffer {
    type Writer = LogBuffer;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

impl LogBuffer {
    /// A thread-scoped TRACE subscriber: `#[tokio::test]` runs every spawned
    /// daemon/HTTP task on this thread, and no other test shares the buffer.
    fn install() -> (Self, tracing::subscriber::DefaultGuard) {
        let buffer = Self::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(buffer.clone())
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);
        tracing::info!(
            canary = "a3k03-log-capture-canary",
            "log capture positive control"
        );
        (buffer, guard)
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(
            &self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
        .into_owned()
    }
}

/// Every regular file under `root` whose bytes contain `needle`.
fn files_containing(root: &Path, needle: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&path) {
                pending.extend(entries.flatten().map(|entry| entry.path()));
            }
        } else if meta.is_file()
            && let Ok(bytes) = std::fs::read(&path)
            && String::from_utf8_lossy(&bytes).contains(needle)
        {
            found.push(path.display().to_string());
        }
    }
    found
}

fn assert_never_retained(workspace: &Path, logs: &LogBuffer, phase: &str) {
    assert!(
        !files_containing(workspace, BODY_CANARY).is_empty(),
        "{phase}: storage scan positive control must find the durable body"
    );
    assert!(
        logs.text().contains("a3k03-log-capture-canary"),
        "{phase}: log capture positive control"
    );
    assert_eq!(
        files_containing(workspace, RAW_MARKER),
        Vec::<String>::new(),
        "{phase}: a raw selection value reached durable storage"
    );
    assert!(
        !logs.text().contains(RAW_MARKER),
        "{phase}: a raw selection value was logged"
    );
}

fn metadata(kind: &str, selected_for: Option<&Value>) -> Value {
    let mut metadata = json!({"x-rustain-message-type": kind});
    if let Some(selected_for) = selected_for {
        metadata["x-rustain-selected-for"] = selected_for.clone();
    }
    metadata
}

/// The type and the serialized selection the production ingress journaled.
async fn recorded(daemon: &AttachedDaemon, task: &str) -> (SemanticMessageType, Value) {
    daemon.wait_for_received_task(task).await;
    read_room_events(&daemon.journal)
        .await
        .into_iter()
        .find_map(|(_, event)| match event {
            RoomEvent::RecipientItemReceived {
                task: found,
                message_type,
                selected_for,
                ..
            } if found == task => Some((
                message_type,
                serde_json::to_value(selected_for).expect("journaled selection serializes"),
            )),
            _ => None,
        })
        .expect("admitted HTTP task is durably recorded")
}

/// Raw journal lines naming `task` carry the field exactly when a carrier was
/// present (legacy-shaped lines stay byte-free of the new field).
fn assert_raw_field(daemon: &AttachedDaemon, task: &str, present: bool) {
    let raw = std::fs::read_to_string(daemon.journal.node.path()).expect("read raw journal");
    let quoted = format!("\"{task}\"");
    let lines: Vec<&str> = raw.lines().filter(|line| line.contains(&quoted)).collect();
    assert!(!lines.is_empty(), "{task}: journal lines exist");
    assert_eq!(
        lines.iter().any(|line| line.contains("selected_for")),
        present,
        "{task}: raw journal field presence"
    );
}

/// Wait-mode recipient for both selection-bearing types, with a standing
/// pinned grant so no consent-setup item takes a NodeTree slot.
struct Recipient {
    daemon: AttachedDaemon,
    host: InboundHttpHost,
    sender: InboundHttpSender,
}

impl Recipient {
    async fn start() -> Self {
        let identity = InboundSenderIdentity::new(SENDER);
        let peer_id = identity.identity.peer_id.clone();
        let peers = vec![pinned_peer_for_inbound_sender(&identity)];
        let journal = Journal::open().await;
        journal
            .room
            .record_event(RoomEvent::ConsentGranted {
                sender: Some(peer_id.clone()),
                granted_at: 1_800_000_000_000,
                principal: Some(PrincipalKind::Pinned),
            })
            .await
            .expect("persist standing sender consent");
        let individual = IndividualPolicy {
            defaults: IndividualDefaults {
                response_mode: Some(ResponseMode::NotifyAndAuto),
                notification: Some(NotificationUrgency::Immediate),
                ..IndividualDefaults::default()
            },
            ..IndividualPolicy::default()
        };
        let mut team = TeamPolicy::default();
        for (token, _) in KINDS {
            team.overrides.per_type.insert(
                token.into(),
                toml::Value::try_from(TeamTypeOverride {
                    response_mode: Some(ResponseMode::NotifyAndWait),
                    notification: None,
                })
                .expect("wait override"),
            );
        }
        let policy = Arc::new(
            rustain::domain::services::team_policy::resolve_effective_policy(
                &individual,
                Some(&team),
                &peers,
            ),
        );
        let room = journal.room.clone();
        let daemon = AttachedDaemon::compose_with_clock_and_test_hooks_and_policy(
            journal,
            peers,
            room,
            Arc::new(rustain::domain::clock::MockClock::at_wall_ms(
                1_800_000_000_000,
            )),
            TeamSendTestHooks::new(None, None, false),
            Some(policy),
        )
        .await;
        daemon.wait_for_pinned_sender_consent(&peer_id).await;
        let host = daemon.serve_http(vec![identity]).await;
        let sender = host.sender(SENDER);
        Self {
            daemon,
            host,
            sender,
        }
    }

    async fn admit(&self, task: &str, kind: &str, body: &str, selected_for: Option<&Value>) {
        let response = self
            .sender
            .message_send_with_metadata(task, body, metadata(kind, selected_for))
            .await;
        assert!(response.json["error"].is_null(), "{task}: {response:?}");
        assert!(
            response.json["result"]["metadata"]["x-rustain-item-id"]
                .as_str()
                .is_some_and(|id| !id.is_empty()),
            "{task}: the message is admitted as a real item: {response:?}"
        );
    }

    async fn stop(self) {
        self.host.stop().await;
        self.daemon.stop().await;
    }
}

/// Every D5 malformed form; objects additionally carry a private field.
fn malformed() -> Vec<(&'static str, Value)> {
    let mut forms = vec![
        ("null", json!(null)),
        ("boolean", json!(true)),
        ("number", json!(42)),
        ("string", json!(RAW_MARKER)),
        ("array", json!([RAW_MARKER])),
        ("missing-stories", json!({})),
        ("stories-null", json!({"stories": null})),
        ("stories-string", json!({"stories": RAW_MARKER})),
        ("stories-object", json!({"stories": {"S7-06": RAW_MARKER}})),
        ("empty", json!({"stories": []})),
        (
            "sixty-five",
            json!({"stories": (0..65).map(|n| format!("S{n}")).collect::<Vec<_>>()}),
        ),
        ("number-entry", json!({"stories": [7]})),
        ("null-entry", json!({"stories": [null]})),
        (
            "object-entry",
            json!({"stories": [{"key": "S7-06", "private": RAW_MARKER}]}),
        ),
        ("mixed-entry", json!({"stories": ["S7-06", true]})),
        (
            "invalid-char",
            json!({"stories": [format!("{RAW_MARKER}!")]}),
        ),
        ("empty-key", json!({"stories": [""]})),
        ("long-key", json!({"stories": ["a".repeat(33)]})),
        ("leading-dash", json!({"stories": ["-S7-06"]})),
        ("duplicate", json!({"stories": ["S7-06", "S7-04", "S7-06"]})),
    ];
    for (_, value) in &mut forms {
        if let Some(object) = value.as_object_mut() {
            object.insert("private-extra".into(), json!(RAW_MARKER));
        }
    }
    forms
}

/// (case, carrier, expected serialized selection) for both types.
fn rows() -> Vec<(
    String,
    &'static str,
    SemanticMessageType,
    Option<Value>,
    Value,
)> {
    let sixty_four: Vec<String> = (0..64).map(|n| format!("S{n}")).collect();
    let mut cases: Vec<(&str, Option<Value>, Value)> = vec![
        ("absent", None, Value::Null),
        (
            "listed-one",
            Some(json!({"stories": ["S7-06"], "private-extra": RAW_MARKER})),
            json!({"state": "Listed", "stories": ["S7-06"]}),
        ),
        (
            "listed-sixty-four",
            Some(json!({"stories": sixty_four})),
            json!({"state": "Listed", "stories": sixty_four}),
        ),
    ];
    cases.extend(
        malformed()
            .into_iter()
            .map(|(case, value)| (case, Some(value), json!({"state": "Unreadable"}))),
    );
    let mut rows = Vec::new();
    for (case, carrier, expected) in cases {
        for (token, kind) in KINDS {
            rows.push((
                format!("a3k03-{token}-{case}"),
                token,
                kind,
                carrier.clone(),
                expected.clone(),
            ));
        }
    }
    rows
}

#[tokio::test]
async fn a3_k03_http_selection_absent_listed_and_every_malformed_form_on_both_types() {
    let (logs, _guard) = LogBuffer::install();
    let rows = rows();
    // Each daemon's NodeTree permits ten root children; keep each batch of
    // real HTTP admissions below that production bound.
    for batch in rows.chunks(8) {
        let recipient = Recipient::start().await;
        for (task, token, kind, carrier, expected) in batch {
            recipient
                .admit(
                    task,
                    token,
                    &format!("{BODY_CANARY} {task}"),
                    carrier.as_ref(),
                )
                .await;
            let (message_type, selection) = recorded(&recipient.daemon, task).await;
            assert_eq!(message_type, *kind, "{task}: type is kept");
            assert_eq!(&selection, expected, "{task}: carrier {carrier:?}");
            assert_raw_field(&recipient.daemon, task, carrier.is_some());
        }
        assert_never_retained(
            recipient.daemon.journal.workspace.path(),
            &logs,
            "tolerant ingest batch",
        );
        recipient.stop().await;
    }
}

#[tokio::test]
async fn a3_k03_consent_deferred_launch_retains_the_selection() {
    let (logs, _guard) = LogBuffer::install();
    for (token, kind, carrier, expected) in [
        (
            "design_update",
            SemanticMessageType::DesignUpdate,
            json!({"stories": ["S7-04", "S7-06"], "private-extra": RAW_MARKER}),
            json!({"state": "Listed", "stories": ["S7-04", "S7-06"]}),
        ),
        (
            "consultation",
            SemanticMessageType::Consultation,
            json!({"stories": [format!("{RAW_MARKER}!"), 7], "private-extra": RAW_MARKER}),
            json!({"state": "Unreadable"}),
        ),
    ] {
        // Production workspace policy: a first-contact sender is deferred
        // behind the real sender-consent approval card.
        let daemon = AttachedDaemon::start(vec![]).await;
        let identity = InboundSenderIdentity::new(SENDER);
        let principal = identity.identity.peer_id.clone();
        let host = daemon.serve_http(vec![identity]).await;
        let sender = host.sender(SENDER);
        let mut writer =
            AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
        let task = format!("a3k03-deferred-{token}");
        let pending = sender
            .message_send_with_metadata(
                &task,
                &format!("{BODY_CANARY} deferred {token}"),
                metadata(token, Some(&carrier)),
            )
            .await;
        assert!(pending.json["error"].is_null(), "{pending:?}");
        assert_eq!(pending.task_state.as_deref(), Some("auth-required"));
        assert!(
            !read_room_events(&daemon.journal)
                .await
                .iter()
                .any(|(_, event)| matches!(event, RoomEvent::RecipientItemReceived { task: found, .. } if found == &task)),
            "{token}: nothing is recorded before the operator's consent"
        );
        let (request, _) = writer.wait_sender_consent_request().await;
        writer.answer_sender_consent_always_and_save(request).await;
        daemon.wait_for_pinned_sender_consent(&principal).await;
        let (message_type, selection) = recorded(&daemon, &task).await;
        assert_eq!(message_type, kind);
        assert_eq!(
            selection, expected,
            "{token}: the consent launch keeps the ingress selection"
        );
        assert_raw_field(&daemon, &task, true);
        assert_never_retained(daemon.journal.workspace.path(), &logs, "consent deferral");
        writer.detach().await;
        host.stop().await;
        daemon.stop().await;
    }
}
