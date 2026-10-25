//! Served taskId references authorize before revealing state or running FR169.
use super::*;
use rustain::adapters::a2a::auth::{A2aServerAuth, API_KEY_HEADER, ApiKeySet};
use rustain::adapters::a2a::item_capability::{ItemCapabilityKey, derive_item_capability};
use serde_json::{Value, json};

const KEY_A: &str = "19-18b-task-owner";
const KEY_B: &str = "19-18b-other-principal";

fn capability(id: &str) -> String {
    derive_item_capability(&ItemCapabilityKey::from_file_bytes(&[71; 32]), id)
        .as_wire_str()
        .to_owned()
}

fn message(id: &str, task: Option<&str>, reply_cap: Option<&str>) -> Value {
    let mut body = json!({"jsonrpc":"2.0","id":7,"method":"message/send","params":{"message":{
        "kind":"message","messageId":id,"role":"user","parts":[{"kind":"text","text":"answer"}],
        "metadata":{"x-rustain-item-capability":capability(id)}
    }}});
    if let Some(task) = task {
        body["params"]["message"]["taskId"] = task.into();
    }
    if let Some(cap) = reply_cap {
        body["params"]["message"]["metadata"]["x-rustain-reply-capability"] = cap.into();
    }
    body
}

fn task_verb(method: &str, task: &str, cap: Option<&str>) -> Value {
    let mut body = json!({"jsonrpc":"2.0","id":7,"method":method,"params":{"id":task}});
    if let Some(cap) = cap {
        body["params"]["metadata"]["x-rustain-item-capability"] = cap.into();
    }
    body
}

struct CredentialHost {
    host: Host,
    client: reqwest::Client,
    _tls: tempfile::TempDir,
}

impl CredentialHost {
    async fn new(policy: A2aAdmissionPolicy) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("key.pem");
        std::fs::write(&cert_path, cert.cert.pem()).unwrap();
        std::fs::write(&key_path, cert.signing_key.serialize_pem()).unwrap();
        let mut options = HostOptions::new(policy, ResponseMode::NotifyAndWait);
        options.bind = "0.0.0.0:0";
        options.security = A2aServerSecurity {
            tls: Some(
                rustain::adapters::a2a::tls::load_tls_material(&cert_path, &key_path).unwrap(),
            ),
            auth: Some(A2aServerAuth::ApiKey(ApiKeySet::unbound(vec![
                KEY_A.into(),
                KEY_B.into(),
            ]))),
            thread_reads: Default::default(),
        };
        Self {
            host: Host::serve_with(options).await,
            client: reqwest::Client::builder()
                .danger_accept_invalid_certs(true)
                .no_proxy()
                .build()
                .unwrap(),
            _tls: dir,
        }
    }

    async fn call(&self, key: &str, body: Value) -> Value {
        self.client
            .post(&self.host.endpoint)
            .header(API_KEY_HEADER, key)
            .json(&body)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    async fn refusal(&self, key: &str, body: Value, expected: &Value) {
        let before = room_events(&self.host.journal).await.len();
        let actual = self.call(key, body).await;
        assert_eq!(
            serde_json::to_vec(&actual).unwrap(),
            serde_json::to_vec(expected).unwrap(),
            "full same-id error object: {actual}"
        );
        assert_eq!(
            room_events(&self.host.journal).await.len(),
            before,
            "refusal wrote a row"
        );
    }
}

#[tokio::test]
async fn unknown_task_reference_is_identical_to_tasks_get() {
    let host = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let before = room_events(&host.journal).await.len();
    let unknown = rpc(
        &host.endpoint,
        task_verb("tasks/get", "never-existed", None),
    )
    .await;
    let followup = rpc(
        &host.endpoint,
        message("new-message", Some("never-existed"), None),
    )
    .await;
    assert_eq!(followup, unknown, "taskId must authorize before admission");
    assert_eq!(room_events(&host.journal).await.len(), before);
    host.cancel.cancel();
}

#[tokio::test]
async fn task_reference_shape_capability_scope_state_and_item_link() {
    let mut tls = CredentialHost::new(A2aAdmissionPolicy::Allow).await;
    let (_operator, shutdown) = attach_operator(&mut tls.host).await;
    let unknown = tls
        .call(KEY_A, task_verb("tasks/get", "never-existed", None))
        .await;
    assert_eq!(
        unknown,
        json!({"jsonrpc":"2.0","id":7,"error":{"code":-32001,"message":"Task not found"}})
    );
    tls.refusal(
        KEY_A,
        message("unknown-reference", Some("never-existed"), None),
        &unknown,
    )
    .await;
    let root = tls.call(KEY_A, message("m1", None, None)).await;
    assert_eq!(root["result"]["id"], "m1", "{root}");
    let item = root["result"]["metadata"]["x-rustain-item-id"]
        .as_str()
        .unwrap()
        .to_owned();
    let node = waiting_node(&tls.host, "m1").await;
    let card = persisted_card(&tls.host, &node).await;
    let invalid =
        json!({"jsonrpc":"2.0","id":7,"error":{"code":-32602,"message":"Invalid params"}});
    for shape in [
        json!(""),
        json!(12),
        json!(false),
        json!([]),
        json!({}),
        json!("x".repeat(257)),
    ] {
        let mut body = message("malformed", None, None);
        body["params"]["message"]["taskId"] = shape;
        tls.refusal(KEY_A, body, &invalid).await;
    }
    let mut conflict = message("two-keys", Some("m1"), Some(&capability("m1")));
    conflict["params"]["message"]["metadata"]["x-rustain-in-reply-to"] = item.clone().into();
    tls.refusal(KEY_A, conflict, &invalid).await;
    for cap in [None, Some(capability("wrong-task"))] {
        tls.refusal(
            KEY_A,
            message("wrong-cap-live", Some("m1"), cap.as_deref()),
            &unknown,
        )
        .await;
    }
    tls.refusal(
        KEY_B,
        message("other-principal", Some("m1"), Some(&capability("m1"))),
        &unknown,
    )
    .await;
    assert_eq!(persisted_card(&tls.host, &node).await, card);
    let followup = tls
        .call(KEY_A, message("m2", Some("m1"), Some(&capability("m1"))))
        .await;
    assert_eq!(followup["result"]["id"], "m2", "{followup}");
    assert_ne!(followup["result"]["metadata"]["x-rustain-item-id"], item);
    let events = room_events(&tls.host.journal).await;
    assert!(
        events.iter().any(|(_, event)| matches!(event,
            RoomEvent::RecipientItemReceived { task, in_reply_to: Some(reference), .. }
                if task == "m2" && reference == &item
        )),
        "taskId links item(m1), never raw m1"
    );
    let original = tls
        .call(KEY_A, task_verb("tasks/get", "m1", Some(&capability("m1"))))
        .await;
    assert_eq!(original["result"]["status"]["state"], "auth-required");
    assert_eq!(
        persisted_card(&tls.host, &node).await,
        card,
        "following up cannot settle T"
    );
    // Null taskId keeps the exact legacy follow-up path, including its item reference.
    let mut null = message("m-null", None, Some(&capability("m1")));
    null["params"]["message"]["taskId"] = Value::Null;
    null["params"]["message"]["metadata"]["x-rustain-in-reply-to"] = item.clone().into();
    let null_answer = tls.call(KEY_A, null).await;
    assert_eq!(null_answer["result"]["id"], "m-null", "{null_answer}");
    // Symmetrically, a null reply-to beside a taskId is absent, not a second key.
    let mut null_reply = message("m-null-reply", Some("m1"), Some(&capability("m1")));
    null_reply["params"]["message"]["metadata"]["x-rustain-in-reply-to"] = Value::Null;
    let null_reply_answer = tls.call(KEY_A, null_reply).await;
    assert_eq!(
        null_reply_answer["result"]["id"], "m-null-reply",
        "{null_reply_answer}"
    );
    let cancelled = tls
        .call(
            KEY_A,
            task_verb("tasks/cancel", "m1", Some(&capability("m1"))),
        )
        .await;
    assert_eq!(
        cancelled["result"]["status"]["state"], "canceled",
        "{cancelled}"
    );
    for cap in [None, Some(capability("wrong-task"))] {
        tls.refusal(
            KEY_A,
            message("wrong-cap-terminal", Some("m1"), cap.as_deref()),
            &unknown,
        )
        .await;
    }
    let terminal = json!({"jsonrpc":"2.0","id":7,"error":{"code":-32004,"message":"Task is in a terminal state and cannot accept further messages"}});
    tls.refusal(
        KEY_A,
        message("terminal", Some("m1"), Some(&capability("m1"))),
        &terminal,
    )
    .await;
    shutdown.cancel();
    tls.host.cancel.cancel();

    let mut pending = CredentialHost::new(A2aAdmissionPolicy::Ask).await;
    let (_operator, pending_shutdown) = attach_operator(&mut pending.host).await;
    let seed = pending.call(KEY_A, message("approval", None, None)).await;
    assert_eq!(seed["result"]["status"]["state"], "auth-required", "{seed}");
    assert!(seed["result"]["metadata"]["x-rustain-item-id"].is_null());
    for cap in [None, Some(capability("wrong-task"))] {
        pending
            .refusal(
                KEY_A,
                message("wrong-cap-approval", Some("approval"), cap.as_deref()),
                &unknown,
            )
            .await;
    }
    pending
        .refusal(
            KEY_B,
            message(
                "cross-approval",
                Some("approval"),
                Some(&capability("approval")),
            ),
            &unknown,
        )
        .await;
    let admitted = json!({"jsonrpc":"2.0","id":7,"error":{"code":-32004,"message":"Task cannot accept messages until it is admitted"}});
    pending
        .refusal(
            KEY_A,
            message(
                "not-admitted",
                Some("approval"),
                Some(&capability("approval")),
            ),
            &admitted,
        )
        .await;
    pending_shutdown.cancel();
    pending.host.cancel.cancel();
}
