//! Real CLI process -> real foreground daemon -> observed production A2A host.
use super::attached_harness::{AttachedClient, FIXTURE_TIMEOUT};
use super::observed_host::{HttpCounts, ObservedHost};
use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
use rustain::adapters::daemon::protocol::{AttachMode, ConnectionTier};
use rustain::adapters::tui::handlers::team_command::{TeamSendRow, render_team_send_rows};
use rustain::domain::models::{JournalRecord, RecipientOutcome, ResponseMode, RoomEvent};
use rustain::domain::ports::RoomJournalReader;
use rustain::infrastructure::subagent::node_journal::WorkspaceJournalReader;
use std::path::PathBuf;
use std::process::Stdio;

struct Workspace {
    workspace: tempfile::TempDir,
    data: tempfile::TempDir,
    config: tempfile::TempDir,
}
impl Workspace {
    fn new(host: &ObservedHost) -> Self {
        let this = Self {
            workspace: tempfile::tempdir().unwrap(),
            data: tempfile::tempdir().unwrap(),
            config: tempfile::tempdir().unwrap(),
        };
        let dir = this.workspace.path().join(".rustain");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("a2a.json"),
            serde_json::json!({"agents":{"lena-po":{"url":host.peer("lena-po").url}}}).to_string(),
        )
        .unwrap();
        this
    }
    fn command(&self) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(assert_cmd::cargo::cargo_bin("rustain"));
        command
            .current_dir(self.workspace.path())
            .env("HOME", self.config.path())
            .env("XDG_CONFIG_HOME", self.config.path().join("xdg-config"))
            .env("XDG_DATA_HOME", self.config.path().join("xdg-data"))
            .env("RUSTAIN_DATA_DIR", self.data.path())
            .env("RUSTAIN_CONFIG_DIR", self.config.path())
            .stdin(Stdio::null())
            .kill_on_drop(true);
        command
    }
    fn socket(&self) -> PathBuf {
        self.data.path().join("daemons").join(format!(
            "{}.sock",
            rustain::infrastructure::paths::workspace_hash(self.workspace.path())
        ))
    }
    async fn launch(&self, host: &ObservedHost) -> tokio::process::Child {
        let mut child = self
            .command()
            .args(["daemon", "start", "--foreground"])
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        tokio::time::timeout(FIXTURE_TIMEOUT, async {
            loop {
                assert!(
                    child.try_wait().unwrap().is_none(),
                    "daemon exited during boot"
                );
                if tokio::net::UnixStream::connect(self.socket()).await.is_ok()
                    && host.counts().get > 0
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("daemon socket and card readiness");
        let mut reader = AttachedClient::attach_without_proposals(
            &self.socket(),
            ConnectionTier::TrustedLocal,
            true,
        )
        .await;
        assert_eq!(reader.granted, AttachMode::ReadOnly);
        reader.team_status("boot").await;
        reader.wait_status_result("boot").await;
        reader.detach().await;
        host.reset_counts();
        child
    }
    async fn run(&self, args: &[&str]) -> std::process::Output {
        tokio::time::timeout(FIXTURE_TIMEOUT, self.command().args(args).output())
            .await
            .expect("CLI completion")
            .unwrap()
    }
    async fn events(&self) -> Vec<RoomEvent> {
        WorkspaceJournalReader::open_workspace(self.workspace.path())
            .load_entries()
            .await
            .unwrap()
            .into_iter()
            .filter_map(|entry| match entry.record {
                JournalRecord::Room(event) => Some(event),
                _ => None,
            })
            .collect()
    }
}

#[tokio::test]
async fn k01_plain_send_through_real_binary_and_daemon() {
    let host = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let workspace = Workspace::new(&host);
    let mut daemon = workspace.launch(&host).await;
    let body = "two  spaces\nline two — ünï";
    let output = workspace.run(&["team", "send", "lena-po", body]).await;
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("Sending to 1 recipient(s) as action sa_"),
        "{stdout}"
    );
    let rows = render_team_send_rows(&[TeamSendRow {
        alias: "lena-po".into(),
        outcome: Some(RecipientOutcome::Delivered),
    }]);
    assert!(stdout.ends_with(&format!("{rows}\n")), "{stdout}");
    assert_eq!(host.counts(), HttpCounts { get: 0, post: 1 });
    let post: serde_json::Value = serde_json::from_slice(host.posts()[0].body()).unwrap();
    assert_eq!(post["params"]["message"]["parts"][0]["text"], body);
    let events = workspace.events().await;
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, RoomEvent::TeamSendActionRecorded { .. }))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, RoomEvent::TeamSendRecipientSettled { .. }))
            .count(),
        1
    );
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
    host.stop().await;
}

#[tokio::test]
async fn k02_typed_send_records_subject_and_story_carrier() {
    use rustain::domain::models::SemanticMessageType;
    let host = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let workspace = Workspace::new(&host);
    let mut daemon = workspace.launch(&host).await;
    let output = workspace
        .run(&[
            "team",
            "send",
            "--type",
            "story_assignment",
            "--subject",
            "sprint-7",
            "--stories",
            "S1=lena-po",
            "lena-po",
            "Sprint",
            "stories",
        ])
        .await;
    assert!(output.status.success(), "{output:?}");
    assert_eq!(host.counts(), HttpCounts { get: 0, post: 1 });
    let post: serde_json::Value = serde_json::from_slice(host.posts()[0].body()).unwrap();
    assert_eq!(
        post["params"]["message"]["parts"][0]["text"],
        "Sprint stories"
    );
    assert_eq!(
        post["params"]["message"]["metadata"]["x-rustain-stories"],
        serde_json::json!({"stories":[{"key":"S1","you":true}]})
    );
    let events = workspace.events().await;
    let recorded: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            RoomEvent::TeamSendActionRecorded {
                message_type,
                subject,
                stories,
                ..
            } => Some((message_type, subject, stories)),
            _ => None,
        })
        .collect();
    assert_eq!(recorded.len(), 1);
    assert_eq!(*recorded[0].0, SemanticMessageType::StoryAssignment);
    assert_eq!(recorded[0].1.as_deref(), Some("sprint-7"));
    assert_eq!(
        serde_json::to_value(recorded[0].2).unwrap(),
        serde_json::json!([{"key":"S1","to":0}])
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, RoomEvent::TeamSendRecipientSettled { .. }))
            .count(),
        1
    );
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
    host.stop().await;
}

#[tokio::test]
async fn k03_unknown_peer_and_held_writer_have_zero_effects() {
    let host = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let workspace = Workspace::new(&host);
    let mut daemon = workspace.launch(&host).await;
    let output = workspace.run(&["team", "send", "ghost", "hello"]).await;
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .starts_with("Nothing was sent: no A2A peer `ghost`"),
        "{output:?}"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("rustain: team send failed:"),
        "{output:?}"
    );
    assert_eq!(host.counts(), HttpCounts { get: 0, post: 0 });
    assert!(!workspace.events().await.iter().any(|event| matches!(
        event,
        RoomEvent::TeamSendActionRecorded { .. } | RoomEvent::RemoteEnvelopeDispatched { .. }
    )));

    let writer = AttachedClient::attach_without_proposals(
        &workspace.socket(),
        ConnectionTier::TrustedLocal,
        false,
    )
    .await;
    assert_eq!(writer.granted, AttachMode::ReadWrite);
    let output = workspace.run(&["team", "send", "lena-po", "hello"]).await;
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("{}\n", rustain::adapters::cli::team::send::WRITER_HELD)
    );
    assert_eq!(host.counts(), HttpCounts { get: 0, post: 0 });
    assert!(!workspace.events().await.iter().any(|event| matches!(
        event,
        RoomEvent::TeamSendActionRecorded { .. } | RoomEvent::RemoteEnvelopeDispatched { .. }
    )));
    writer.detach().await;
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
    host.stop().await;
}
