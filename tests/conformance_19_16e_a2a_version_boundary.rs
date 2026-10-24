#![cfg(feature = "a2a")]

use std::sync::Arc;

use rustain::adapters::a2a::client::A2aClientAdapter;
use rustain::adapters::a2a::driver::TaskClient;
use rustain::adapters::a2a::error::A2aError;
use rustain::adapters::a2a::jsonrpc::{
    A2A_PROTOCOL_VERSION, A2A_VERSION_HEADER, CODE_TASK_NOT_FOUND, CODE_VERSION_NOT_SUPPORTED,
};
use rustain::adapters::a2a::lifecycle::A2aTaskTransport;
use rustain::adapters::a2a::server::{ServeConfig, serve};
use rustain::adapters::a2a::transparency::TransparencySink;
use rustain::adapters::rap::IdentityKeyStore;
use rustain::domain::models::{
    A2aPeerSource, A2aPeerSpec, CapabilityRegistry, JournalRecord, RedactedUrl, RoomEvent,
};
use rustain::infrastructure::subagent::{NodeJournal, NodeRoomJournal};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

async fn start_server(
    config: ServeConfig,
) -> (
    String,
    CancellationToken,
    tokio::task::JoinHandle<anyhow::Result<()>>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/", listener.local_addr().unwrap());
    let cancel = CancellationToken::new();
    let task = tokio::spawn(serve(listener, config, cancel.child_token()));
    (endpoint, cancel, task)
}

fn discovery_config(workspace: &std::path::Path) -> ServeConfig {
    let signer = IdentityKeyStore::new(workspace.join("identity"))
        .load_or_generate()
        .unwrap();
    ServeConfig::discovery_only(
        Arc::new(CapabilityRegistry::new(None)),
        signer,
        workspace.to_path_buf(),
    )
}

async fn stop_server(cancel: CancellationToken, task: tokio::task::JoinHandle<anyhow::Result<()>>) {
    cancel.cancel();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn k1_real_card_advertises_the_served_dialect() {
    let workspace = tempfile::tempdir().unwrap();
    let (endpoint, cancel, task) = start_server(discovery_config(workspace.path())).await;
    let response = reqwest::Client::new()
        .get(format!("{endpoint}.well-known/agent-card.json"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let card: serde_json::Value = response.json().await.unwrap();
    assert_eq!(card["supportedInterfaces"][0]["protocolVersion"], "0.3");
    assert_eq!(
        card["supportedInterfaces"][0]["protocolVersion"],
        A2A_PROTOCOL_VERSION
    );
    assert_code(rpc(&endpoint, None, &get_task()).await, CODE_TASK_NOT_FOUND).await;
    stop_server(cancel, task).await;
}

fn get_task() -> Value {
    json!({"jsonrpc": "2.0", "id": "version-call", "method": "tasks/get", "params": {"id": "absent"}})
}

async fn rpc(endpoint: &str, version: Option<&str>, body: &Value) -> reqwest::Response {
    let mut request = reqwest::Client::new().post(endpoint).json(body);
    if let Some(version) = version {
        request = request.header(A2A_VERSION_HEADER, version);
    }
    request.send().await.unwrap()
}

async fn assert_code(response: reqwest::Response, code: i64) -> Value {
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["jsonrpc"], "2.0", "{body}");
    assert_eq!(body["error"]["code"], code, "{body}");
    assert!(body.get("result").is_none(), "{body}");
    body
}

#[tokio::test]
async fn k2_supported_versions_dispatch_to_task_lookup() {
    let workspace = tempfile::tempdir().unwrap();
    let (endpoint, cancel, task) = start_server(discovery_config(workspace.path())).await;
    for version in [None, Some("0.3"), Some("0.3.0"), Some("")] {
        let body = assert_code(
            rpc(&endpoint, version, &get_task()).await,
            CODE_TASK_NOT_FOUND,
        )
        .await;
        assert_eq!(body["id"], "version-call");
    }
    stop_server(cancel, task).await;
}

#[tokio::test]
async fn k3_mismatches_are_named_and_envelope_errors_keep_precedence() {
    let workspace = tempfile::tempdir().unwrap();
    let (endpoint, cancel, task) = start_server(discovery_config(workspace.path())).await;
    for version in ["1.0", "0.4", "v0.3"] {
        let body = assert_code(
            rpc(&endpoint, Some(version), &get_task()).await,
            CODE_VERSION_NOT_SUPPORTED,
        )
        .await;
        assert_eq!(body["id"], "version-call");
        assert_eq!(
            body["error"]["message"],
            "A2A protocol version not supported: this agent serves A2A 0.3 on this interface; send `A2A-Version: 0.3` or omit it"
        );
        assert_code(rpc(&endpoint, None, &get_task()).await, CODE_TASK_NOT_FOUND).await;
    }
    let mut numeric_id = get_task();
    numeric_id["id"] = json!(42);
    let body = assert_code(
        rpc(&endpoint, Some("1.0"), &numeric_id).await,
        CODE_VERSION_NOT_SUPPORTED,
    )
    .await;
    assert_eq!(body["id"], 42);

    for (raw, code) in [
        ("{", -32700),
        (r#"{"jsonrpc":"1.0","id":1,"method":"tasks/get"}"#, -32600),
        (
            r#"{"jsonrpc":"2.0","id":null,"method":"tasks/get"}"#,
            -32600,
        ),
    ] {
        let response = reqwest::Client::new()
            .post(&endpoint)
            .header(A2A_VERSION_HEADER, "1.0")
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(raw)
            .send()
            .await
            .unwrap();
        assert_code(response, code).await;
    }
    let response = reqwest::Client::new()
        .post(&endpoint)
        .header(A2A_VERSION_HEADER, "1.0")
        .header(reqwest::header::CONTENT_TYPE, "text/plain")
        .body(get_task().to_string())
        .send()
        .await
        .unwrap();
    assert_code(response, -32600).await;
    stop_server(cancel, task).await;
}

#[tokio::test]
async fn k4_version_refusal_never_reaches_admission_or_the_journal() {
    let workspace = tempfile::tempdir().unwrap();
    let journal = Arc::new(NodeJournal::open_workspace(workspace.path()).await.unwrap());
    let room = Arc::new(NodeRoomJournal::new(journal.clone(), None));
    let mut config = discovery_config(workspace.path());
    config.transparency = Arc::new(TransparencySink::new(room).with_reader(journal.clone()));
    let (endpoint, cancel, task) = start_server(config).await;
    let request = json!({
        "jsonrpc": "2.0", "id": "send-call", "method": "message/send",
        "params": {"message": {
            "kind": "message", "role": "user", "messageId": "version-send",
            "parts": [{"kind": "text", "text": "a version boundary test"}]
        }}
    });
    assert!(journal.load().await.unwrap().is_empty());
    assert_code(
        rpc(&endpoint, Some("1.0"), &request).await,
        CODE_VERSION_NOT_SUPPORTED,
    )
    .await;
    assert!(journal.load().await.unwrap().is_empty());

    let response = rpc(&endpoint, None, &request).await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["result"]["status"]["state"], "rejected", "{body}");
    let entries = journal.load().await.unwrap();
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert!(matches!(
        entries[0].record,
        JournalRecord::Room(RoomEvent::RemoteEnvelopeRejected { .. })
    ));
    stop_server(cancel, task).await;
}

#[tokio::test]
async fn k5_an_unsupported_notification_gets_a_null_id_refusal() {
    let workspace = tempfile::tempdir().unwrap();
    let (endpoint, cancel, task) = start_server(discovery_config(workspace.path())).await;
    let mut request = get_task();
    request.as_object_mut().unwrap().remove("id");
    let body = assert_code(
        rpc(&endpoint, Some("1.0"), &request).await,
        CODE_VERSION_NOT_SUPPORTED,
    )
    .await;
    assert!(body.get("id").unwrap().is_null(), "{body}");
    let response = rpc(&endpoint, None, &request).await;
    assert_eq!(response.status(), reqwest::StatusCode::NO_CONTENT);
    assert!(response.bytes().await.unwrap().is_empty());
    stop_server(cancel, task).await;
}

#[tokio::test]
async fn k6_namespaced_reads_share_the_same_gate() {
    let workspace = tempfile::tempdir().unwrap();
    let (endpoint, cancel, task) = start_server(discovery_config(workspace.path())).await;
    let request = json!({"jsonrpc": "2.0", "id": "items", "method": "x-rustain-items/list"});
    assert_code(
        rpc(&endpoint, Some("1.0"), &request).await,
        CODE_VERSION_NOT_SUPPORTED,
    )
    .await;
    let response = rpc(&endpoint, None, &request).await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: Value = response.json().await.unwrap();
    assert!(body.get("result").is_some(), "{body}");
    assert!(body.get("error").is_none(), "{body}");
    stop_server(cancel, task).await;
}

#[tokio::test]
async fn k7_the_shipped_client_omission_reaches_the_served_task_lookup() {
    let workspace = tempfile::tempdir().unwrap();
    let (endpoint, cancel, task) = start_server(discovery_config(workspace.path())).await;
    let peer = A2aPeerSpec::new(
        "version-peer",
        RedactedUrl::from(endpoint.clone()),
        A2aPeerSource::Workspace,
    );
    let adapter = Arc::new(A2aClientAdapter::new(&peer, None).unwrap());
    let transport = TaskClient::new(adapter, endpoint.clone());
    assert!(matches!(
        transport.tasks_get("absent").await,
        Err(A2aError::JsonRpc {
            code: CODE_TASK_NOT_FOUND,
            ..
        })
    ));
    assert_code(
        rpc(&endpoint, Some("1.0"), &get_task()).await,
        CODE_VERSION_NOT_SUPPORTED,
    )
    .await;
    stop_server(cancel, task).await;
}

#[tokio::test]
async fn k8_query_versions_and_header_precedence_reach_the_real_handler() {
    let workspace = tempfile::tempdir().unwrap();
    let (endpoint, cancel, task) = start_server(discovery_config(workspace.path())).await;
    for (query, header, code) in [
        ("A2A-Version=1.0", None, CODE_VERSION_NOT_SUPPORTED),
        ("A2A-Version=0.3", None, CODE_TASK_NOT_FOUND),
        ("a2a-version=1.0", None, CODE_VERSION_NOT_SUPPORTED),
        ("A2A-Version=0%2E3", None, CODE_TASK_NOT_FOUND),
        ("A2A-Version=0.3&A2A-Version=1.0", None, CODE_TASK_NOT_FOUND),
        ("A2A-Version=1.0", Some(""), CODE_VERSION_NOT_SUPPORTED),
        ("A2A-Version=1.0", Some("0.3"), CODE_TASK_NOT_FOUND),
    ] {
        assert_code(
            rpc(&format!("{endpoint}?{query}"), header, &get_task()).await,
            code,
        )
        .await;
    }
    stop_server(cancel, task).await;
}
