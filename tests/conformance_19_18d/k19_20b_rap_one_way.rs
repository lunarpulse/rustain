//! Story 19.20b K01: production RAP ingress is one-way, while A2A still replies.
//!
//! Supersedes 19.20a's RAP auto-row expectation: accepted RAP messages now
//! persist a host notice, never an agent-composed template or a staged reply.

use rustain::adapters::filesystem::FileSystemStorage;
use rustain::domain::models::{
    Authorship, HistorySource, MessageRole, PeerRowStage, PolicyPrincipal, ResponseMode,
    SemanticMessageType, SendAuthorization,
};
use rustain::domain::ports::{InboundPeerRuntime, StoragePort};

use super::harness::{
    BoundHost, CONVERSATION_ID, HostSpec, K_P1A, SETTLE, detail, raw_payloads, setup,
};
use super::k11_k14_consent_policy::{RapSender, deliver, delivered, open_rap_front_door};

const AUTO_TOML: &str =
    "[interaction.defaults]\nresponse_mode = \"notify-and-auto\"\nnotification = \"immediate\"\n";
const TEMPLATE: &str = "Received. Reviewing.";
// Pin the public-facing copy independently of the crate-private renderer.
const NOTICE: &str = "[no reply over RAP] RAP carries messages one way: nothing can be sent back, so your notify-and-auto response setting does not apply here.";

#[tokio::test(flavor = "multi_thread")]
async fn k01_two_rap_frames_persist_notices_and_a2a_still_returns_the_template() {
    setup();
    let mut rap = RapSender::new();
    let pin = rap.pin();
    let mut host =
        BoundHost::start(HostSpec::b2().roster(rap.roster()).interaction(AUTO_TOML)).await;
    assert!(
        host.effective_policy.is_some(),
        "use the written production policy"
    );
    let policy = InboundPeerRuntime::response_policy(
        host.server.as_ref(),
        &PolicyPrincipal::rap(pin.clone()),
        SemanticMessageType::Unknown,
    );
    assert_eq!(policy.mode, ResponseMode::NotifyAndAuto);
    assert!(
        policy.auto_response.is_none(),
        "exercise the default template, not an override"
    );

    let mut operator = host.attach_operator().await;
    let handler = open_rap_front_door(&host).await;
    let bodies = ["first one-way RAP message", "second one-way RAP message"];
    let first = deliver(&handler, &mut rap, "inbox", bodies[0]);
    operator.always_allow().await;
    delivered(first).await;
    // The dispatch context is persistent, not a per-message task. A terminal
    // transition after the first frame would refuse this identical recipient.
    delivered(deliver(&handler, &mut rap, "inbox", bodies[1])).await;

    // delivered() joins the synchronous durable row commit: no polling or sleeps.
    let storage = FileSystemStorage::with_workspace_root(
        rustain::infrastructure::paths::sessions_dir(&host.journal.path),
        host.journal.path.clone(),
    );
    let conversation = storage
        .load_conversation(CONVERSATION_ID)
        .await
        .expect("read the persisted conversation")
        .expect("RAP commits the conversation");
    assert!(
        conversation
            .messages
            .iter()
            .all(|row| row.content != TEMPLATE)
    );
    let recipient = format!("{}/inbox", pin.as_str());
    let rows: Vec<_> = conversation
        .messages
        .iter()
        .filter(|row| {
            row.history_source == HistorySource::PeerDisplay
                || conversation
                    .peer_row_controls
                    .get(&row.id)
                    .is_some_and(|control| control.node_id == recipient)
        })
        .collect();
    assert_eq!(
        rows.len(),
        4,
        "exactly two transcript/notice pairs: {rows:?}"
    );
    for (pair, body) in rows.as_chunks::<2>().0.iter().zip(bodies) {
        let transcript = pair[0];
        assert_eq!(transcript.role, MessageRole::User);
        assert!(!transcript.synthetic);
        assert_eq!(transcript.history_source, HistorySource::PeerDisplay);
        assert_eq!(
            transcript.content,
            format!(
                "{}\n{body}\n{}\n{}",
                policy.provenance.peer_header(&pin),
                policy.provenance.response_clause(),
                policy.provenance.notification_clause(),
            ),
        );
        assert!(!conversation.peer_row_controls.contains_key(&transcript.id));
        let notice = pair[1];
        assert!(!notice.id.is_empty());
        assert!(!notice.id.starts_with("peer-response-"));
        assert_eq!(notice.content, NOTICE);
        assert_eq!(notice.role, MessageRole::Assistant);
        assert!(notice.synthetic);
        assert_eq!(notice.authorship, Authorship::HumanWritten);
        assert_eq!(notice.send_authorization, SendAuthorization::Unknown);
        let control = &conversation.peer_row_controls[&notice.id];
        assert_eq!(control.node_id, recipient);
        assert_eq!(control.stage, PeerRowStage::Settled);
        assert!(!control.node_resolvable);
        assert!(control.item_address.is_none());
    }
    assert_ne!(
        rows[1].id, rows[3].id,
        "each notice has its own generated id"
    );

    // Inspect before the A2A control, which legitimately journals disclosure.
    let accepted = raw_payloads(&host.journal, "remote_envelope_accepted");
    assert_eq!(accepted.len(), 2, "both RAP verdicts are durably accepted");
    for (index, row) in accepted.iter().enumerate() {
        assert_eq!(row["peer"], pin.as_str());
        assert_eq!(row["node"], recipient);
        assert_eq!(row["direction"], "inbound");
        assert_eq!(row["task"], format!("rap-corr-{}", index + 1));
    }
    assert!(raw_payloads(&host.journal, "peer_disclosure").is_empty());
    assert!(raw_payloads(&host.journal, "peer_draft_resolved").is_empty());

    // A2A remains reply-capable on this same host: assert the actual
    // tasks/get wire answer, not just an internal staging entry.
    let client = host.client(K_P1A);
    let task = "a2a-auto-positive-control";
    let sent = client
        .message_send(1, task, "reply over A2A", serde_json::json!({}))
        .await;
    assert!(sent.json["error"].is_null(), "{}", sent.body);
    let completed = tokio::time::timeout(SETTLE, async {
        loop {
            let reply = client.tasks_get(2, task, None).await;
            assert!(reply.json["error"].is_null(), "{}", reply.body);
            if reply.json["result"]["status"]["state"] == "completed" {
                break reply;
            }
            // Yield to the real inbound worker; HTTP round trips observe its
            // progress without a timer-based wait or a fixed retry count.
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("A2A reaches completed before the deadline");
    assert_eq!(detail(&completed), Some(TEMPLATE), "{}", completed.body);
    drop(operator);
}
