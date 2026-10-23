//! A real foreground rustain OS process, configured HTTP streaming provider,
//! authenticated Unix socket, abrupt kill, and fresh launch over the same disk.
//! In-process fixture destruction is deliberately not the restart mechanism.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{
    Json, Router,
    extract::State,
    http::header,
    response::IntoResponse,
    routing::{get, post},
};
use rustain::adapters::daemon::protocol::{
    AttachMode, ClientFrame, ConnectionTier, DaemonFrame, TeamProposalCloseOutcome,
};
use rustain::domain::models::{JournalRecord, RoomEvent, TeamProposalDecision};
use rustain::domain::ports::RoomJournalReader;
use rustain::infrastructure::subagent::node_journal::WorkspaceJournalReader;
use serde_json::{Value, json};
use tokio::process::{Child, Command};
use tokio_util::sync::CancellationToken;

use super::attached_harness::{AttachedClient, FIXTURE_TIMEOUT};
use super::g_harness::{BODY, completed, decide, fence, input, proposal};
use super::observed_host::{HttpCounts, ObservedHost};

#[derive(Clone, Default)]
struct ProviderState {
    scripts: Arc<Mutex<VecDeque<String>>>,
    requests: Arc<Mutex<Vec<Value>>>,
}

struct ProviderTransport {
    state: ProviderState,
    url: String,
    shutdown: CancellationToken,
    task: tokio::task::JoinHandle<Result<(), std::io::Error>>,
}

impl ProviderTransport {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind configured streaming provider");
        let url = format!(
            "http://{}",
            listener.local_addr().expect("provider address")
        );
        let state = ProviderState::default();
        let router = Router::new()
            .route("/v1/messages", post(provider_response))
            .route("/v1/models", get(|| async {
                Json(json!({"data":[{"id":"claude-sonnet-4-6","type":"model","display_name":"Conformance"}],"has_more":false}))
            }))
            .with_state(state.clone());
        let shutdown = CancellationToken::new();
        let stop = shutdown.clone();
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(stop.cancelled_owned())
                .await
        });
        Self {
            state,
            url,
            shutdown,
            task,
        }
    }

    fn queue_proposal(&self) {
        let mut scripts = self.state.scripts.lock().expect("provider script queue");
        scripts.push_back(tool_stream(input()));
        scripts.push_back(end_stream());
    }

    fn requests(&self) -> Vec<Value> {
        self.state
            .requests
            .lock()
            .expect("provider requests")
            .clone()
    }

    async fn stop(self) {
        self.shutdown.cancel();
        self.task
            .await
            .expect("provider task joined")
            .expect("provider shutdown");
    }
}

async fn provider_response(
    State(state): State<ProviderState>,
    Json(request): Json<Value>,
) -> axum::response::Response {
    // Production adapters may probe connectivity without streaming. Such probes
    // cannot consume the model-turn script or count as a ToolUse positive control.
    if request.get("stream").and_then(Value::as_bool) != Some(true) {
        return Json(json!({"id":"probe","type":"message","role":"assistant","content":[],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":0}})).into_response();
    }
    state
        .requests
        .lock()
        .expect("provider requests")
        .push(request);
    let script = state
        .scripts
        .lock()
        .expect("provider script queue")
        .pop_front()
        .expect("every production provider turn must have a queued SSE response");
    ([(header::CONTENT_TYPE, "text/event-stream")], script).into_response()
}

fn event(value: Value) -> String {
    format!(
        "event: {}\ndata: {value}\n\n",
        value["type"].as_str().expect("SSE event type")
    )
}

fn end_stream() -> String {
    [
        event(json!({"type":"message_start","message":{"usage":{"input_tokens":10}}})),
        event(json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":0}})),
        event(json!({"type":"message_stop"})),
    ].concat()
}

fn tool_stream(input: Value) -> String {
    [
        event(json!({"type":"message_start","message":{"usage":{"input_tokens":10}}})),
        event(json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":nanoid::nanoid!(),"name":"propose_team_message","input":{}}})),
        event(json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":input.to_string()}})),
        event(json!({"type":"content_block_stop","index":0})),
        event(json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":20}})),
        event(json!({"type":"message_stop"})),
    ].concat()
}

struct ProcessWorkspace {
    workspace: tempfile::TempDir,
    data: tempfile::TempDir,
    config: tempfile::TempDir,
    home: tempfile::TempDir,
}

impl ProcessWorkspace {
    fn new(provider: &ProviderTransport, host: &ObservedHost) -> Self {
        let this = Self {
            workspace: tempfile::tempdir().expect("process workspace"),
            data: tempfile::tempdir().expect("process data directory"),
            config: tempfile::tempdir().expect("process config directory"),
            home: tempfile::tempdir().expect("isolated process home"),
        };
        let directory = this.workspace.path().join(".rustain");
        std::fs::create_dir_all(&directory).expect("create process workspace configuration");
        std::fs::write(directory.join("config.toml"), format!(
            "[provider.anthropic]\nprovider_id = \"anthropic\"\nmodel_id = \"claude-sonnet-4-6\"\napi_key_env = \"PROPOSAL_PROCESS_PROVIDER_KEY\"\nbase_url = {:?}\ndiscover_models = false\n",
            provider.url,
        )).expect("write actual streaming provider configuration");
        let endpoint = host.peer("lena-po").url;
        std::fs::write(
            directory.join("a2a.json"),
            json!({
                "agents":{"lena-po":{"url":endpoint}},
            })
            .to_string(),
        )
        .expect("write process egress roster");
        this
    }

    fn socket(&self) -> PathBuf {
        self.data.path().join("daemons").join(format!(
            "{}.sock",
            rustain::infrastructure::paths::workspace_hash(self.workspace.path())
        ))
    }

    async fn launch(&self) -> Child {
        // Same production binary/front door as conformance_17_1a_real_peer.
        let mut child = Command::new(assert_cmd::cargo::cargo_bin("rustain"))
            .args(["daemon", "start", "--foreground"])
            .current_dir(self.workspace.path())
            .env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.home.path().join("xdg-config"))
            .env("XDG_DATA_HOME", self.home.path().join("xdg-data"))
            .env("RUSTAIN_DATA_DIR", self.data.path())
            .env("RUSTAIN_CONFIG_DIR", self.config.path())
            .env("PROPOSAL_PROCESS_PROVIDER_KEY", "process-conformance-key")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("launch real rustain daemon process");
        let path = self.socket();
        tokio::time::timeout(FIXTURE_TIMEOUT, async {
            loop {
                assert!(
                    child.try_wait().expect("observe daemon process").is_none(),
                    "real proposal daemon exited before its authenticated socket became ready"
                );
                if tokio::net::UnixStream::connect(&path).await.is_ok() {
                    break;
                }
                // Readiness polling only, never a decision, FIFO or send race.
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("real daemon socket readiness deadline");
        child
    }
}

async fn warm_boot(host: &ObservedHost, writer: &mut AttachedClient, key: &str) {
    tokio::time::timeout(FIXTURE_TIMEOUT, async {
        while host.counts().get == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("real process boot must fetch its configured recipient AgentCard");
    fence(writer, key).await;
    assert_eq!(
        host.counts(),
        HttpCounts { get: 1, post: 0 },
        "each fresh process has exactly its one explicit boot AgentCard warm-up and no sends"
    );
    host.reset_counts();
}

async fn process_raise(provider: &ProviderTransport, writer: &mut AttachedClient) -> String {
    let before = provider.requests().len();
    provider.queue_proposal();
    writer
        .send_frame(ClientFrame::UserMessage {
            text: "Propose the supplied architecture message. Only the operator may send it."
                .into(),
            images: Vec::new(),
        })
        .await;
    completed(writer).await;
    let id = proposal(writer).await;
    assert!(
        id.starts_with("tp_"),
        "real process proposal must carry a host-minted sender-local id"
    );
    let requests = provider.requests();
    assert_eq!(
        requests.len() - before,
        2,
        "real process must execute configured HTTP ToolUse and tool-result continuation"
    );
    assert!(
        requests[before]["tools"]
            .as_array()
            .expect("production tool catalogue")
            .iter()
            .any(|tool| tool["name"] == "propose_team_message"),
        "real process production composition must offer the proposal tool"
    );
    let continuation = requests[before + 1].to_string();
    assert!(
        continuation.contains("Proposal shown to the operator. Nothing has been sent;"),
        "real process ToolUse must raise through the production scheduler and shared registry"
    );
    assert!(
        !continuation.contains(&id),
        "sender-local proposal id must never enter model continuation"
    );
    id
}

async fn room_events(workspace: &Path) -> Vec<RoomEvent> {
    WorkspaceJournalReader::open_workspace(workspace)
        .load_entries()
        .await
        .expect("read actual process journal")
        .into_iter()
        .filter_map(|entry| match entry.record {
            JournalRecord::Room(event) => Some(event),
            _ => None,
        })
        .collect()
}

async fn zero(workspace: &Path, host: &ObservedHost, diagnostic: &str) {
    assert_eq!(
        host.counts(),
        HttpCounts { get: 0, post: 0 },
        "{diagnostic}"
    );
    assert!(
        !room_events(workspace).await.iter().any(|event| matches!(
            event,
            RoomEvent::TeamSendActionRecorded { .. } | RoomEvent::RemoteEnvelopeDispatched { .. }
        )),
        "{diagnostic}"
    );
}

#[tokio::test]
async fn killed_daemon_process_withdraws_proposal_before_fresh_human_send() {
    use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
    use rustain::domain::models::ResponseMode;

    let provider = ProviderTransport::start().await;
    let host = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let disk = ProcessWorkspace::new(&provider, &host);
    let mut first = disk.launch().await;
    let first_pid = first.id().expect("first daemon OS pid");
    let mut writer =
        AttachedClient::attach(&disk.socket(), ConnectionTier::TrustedLocal, false).await;
    assert_eq!(writer.granted, AttachMode::ReadWrite);
    warm_boot(&host, &mut writer, "first-process-boot").await;
    let old_id = process_raise(&provider, &mut writer).await;
    zero(
        disk.workspace.path(),
        &host,
        "pending process proposal must make zero recipient requests or send journal effects",
    )
    .await;

    // Tokio Child::kill sends SIGKILL on Unix. This destroys an actual daemon
    // process, not an Arc, runtime or fixture. Reap before launching the next one.
    first
        .kill()
        .await
        .expect("kill actual pending-proposal daemon process");
    let exit = first.wait().await.expect("reap killed daemon process");
    assert!(
        !exit.success(),
        "restart proof must observe an abrupt OS-process death"
    );
    drop(writer);
    zero(
        disk.workspace.path(),
        &host,
        "OS daemon kill must withdraw without recipient requests or send journal effects",
    )
    .await;

    let mut second = disk.launch().await;
    assert_ne!(
        second.id(),
        Some(first_pid),
        "restart must create a different OS process"
    );
    let mut fresh =
        AttachedClient::attach(&disk.socket(), ConnectionTier::TrustedLocal, false).await;
    assert_eq!(fresh.granted, AttachMode::ReadWrite);
    warm_boot(&host, &mut fresh, "restarted-process-boot").await;
    // TeamStatus is in the production operator FIFO, after writer grant/reemit.
    // Seeing its correlated result drains every earlier retained-card frame.
    assert!(
        !fresh
            .received
            .iter()
            .any(|frame| matches!(frame, DaemonFrame::TeamProposal { .. })),
        "process restart must not retain or reemit the prior proposal card"
    );
    decide(&mut fresh, &old_id, TeamProposalDecision::Send).await;
    let refused = fresh
        .wait_for(|frame| {
            matches!(frame,
                DaemonFrame::TeamProposalClosed { proposal, .. } if proposal == &old_id
            )
        })
        .await;
    assert!(
        matches!(refused,
            DaemonFrame::TeamProposalClosed { outcome: TeamProposalCloseOutcome::Refused { reason }, .. }
            if reason == "That proposal is no longer waiting. Nothing was sent."
        ),
        "process restart must refuse the old proposal id as no longer waiting"
    );
    fence(&mut fresh, "restarted-old-decision-fence").await;
    zero(
        disk.workspace.path(),
        &host,
        "process restart and stale Send must make zero recipient GET/POST or send journal effects",
    )
    .await;

    // Same disk, fresh production process, provider transport and signed writer:
    // a new ToolUse still raises, then a deliberate human Send works exactly once.
    let new_id = process_raise(&provider, &mut fresh).await;
    assert_ne!(
        old_id, new_id,
        "fresh process must mint a new proposal snapshot id"
    );
    zero(
        disk.workspace.path(),
        &host,
        "fresh process proposal must remain unsent until human Send",
    )
    .await;
    // An old decision remains invalid even after a new snapshot exists. It must
    // not consume that snapshot or send its different content/recipients.
    decide(&mut fresh, &old_id, TeamProposalDecision::Send).await;
    assert!(
        matches!(
            fresh
                .wait_for(|frame| matches!(frame,
                    DaemonFrame::TeamProposalClosed { proposal, .. } if proposal == &old_id
                ))
                .await,
            DaemonFrame::TeamProposalClosed {
                outcome: TeamProposalCloseOutcome::Refused { .. },
                ..
            }
        ),
        "stale pre-restart decision must not authorize the fresh proposal snapshot"
    );
    fence(&mut fresh, "fresh-proposal-old-decision-fence").await;
    zero(
        disk.workspace.path(),
        &host,
        "old id after fresh raise must make zero recipient requests or journal effects",
    )
    .await;
    decide(&mut fresh, &new_id, TeamProposalDecision::Send).await;
    assert!(
        matches!(
            fresh.wait_send_started(&new_id).await,
            DaemonFrame::TeamSendStarted { .. }
        ),
        "fresh process human Send positive control must start the canonical action"
    );
    assert!(
        matches!(
            fresh.wait_send_settled(&new_id, 0).await,
            DaemonFrame::TeamSendSettled {
                outcome: rustain::adapters::daemon::protocol::TeamSendOutcome::Delivered,
                ..
            }
        ),
        "fresh process human Send positive control must deliver"
    );
    assert_eq!(
        host.counts(),
        HttpCounts { get: 0, post: 1 },
        "fresh process human Send must POST once without another recipient GET"
    );
    let events = room_events(disk.workspace.path()).await;
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, RoomEvent::TeamSendActionRecorded { .. }))
            .count(),
        1,
        "fresh process human Send must journal exactly one send action"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, RoomEvent::RemoteEnvelopeDispatched { .. }))
            .count(),
        1,
        "fresh process human Send must journal exactly one dispatch"
    );
    let posts = host.posts();
    assert_eq!(posts.len(), 1);
    let wire: Value =
        serde_json::from_slice(posts[0].body()).expect("real process recipient POST JSON");
    assert_eq!(
        wire.pointer("/params/message/parts/0/text")
            .and_then(Value::as_str),
        Some(BODY),
        "fresh process human Send must dispatch the proposed body unchanged"
    );
    second.kill().await.expect("stop fresh daemon process");
    second.wait().await.expect("reap fresh daemon process");
    provider.stop().await;
    host.stop().await;
}
