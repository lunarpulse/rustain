#![cfg(all(feature = "telegram", feature = "cron"))]

use crate::common::daemon_proc::{Env, Settled, Stub};
use std::time::Duration;

async fn assert_channel_refused(config: &str, missing: &str, secret: Option<&str>) {
    let stub = Stub::start().await;
    let env = Env::new(&stub);
    env.write_profile("c2b", &format!("name = \"c2b\"\nextends = \"base\"\n[channels]\nadapter = \"telegram\"\n[channels.config]\n{config}\n"));
    let mut daemon = env.spawn_daemon_foreground("c2b");
    match daemon.settle(&env, Duration::from_secs(30)).await {
        Settled::Ready => panic!("invalid channel started daemon"),
        Settled::Exited(status) => assert!(!status.success()),
    }
    let output = daemon.proc.finish(Duration::from_secs(10)).await;
    assert!(
        output.stderr.contains("composing daemon channel:"),
        "{}",
        output.stderr
    );
    assert!(output.stderr.contains(missing), "{}", output.stderr);
    if let Some(secret) = secret {
        assert!(!output.stderr.contains(secret));
    }
    assert!(!env.pid_path().exists());
    assert!(stub.requests().is_empty());
}

#[tokio::test]
async fn daemon_refuses_missing_allowlist_without_leaking_token() {
    assert_channel_refused(
        "bot_token = \"canary-c2b-secret\"",
        "allowed_chat_ids",
        Some("canary-c2b-secret"),
    )
    .await;
}

#[tokio::test]
async fn daemon_refuses_missing_bot_token() {
    assert_channel_refused("allowed_chat_ids = [123]", "bot_token", None).await;
}

#[tokio::test]
async fn terminal_none_and_missing_cron_file_start_normally() {
    for scheduler in ["none", "cron"] {
        let stub = Stub::start().await;
        let env = Env::new(&stub);
        env.write_profile("c2b", &format!(
            "name = \"c2b\"\nextends = \"base\"\n[channels]\nadapter = \"terminal\"\n[scheduler]\nadapter = \"{scheduler}\"\n"
        ));
        let mut daemon = env.spawn_daemon_foreground("c2b");
        daemon.wait_ready(&env, Duration::from_secs(30)).await;
        assert!(env.pid_path().exists());
        let status = env
            .run(&["daemon", "status", "--json"], Duration::from_secs(10))
            .await;
        assert!(status.status.success(), "{}", status.stderr);
        let snapshot: serde_json::Value =
            serde_json::from_str(&status.stdout).expect("daemon status JSON");
        assert_eq!(snapshot["running"], serde_json::json!(true));
        assert_eq!(snapshot["cron_jobs"], serde_json::json!([]));
        assert!(stub.requests().is_empty());
        let stopped = env.daemon_stop().await;
        assert!(stopped.status.success(), "{}", stopped.stderr);
        let _ = daemon.proc.finish(Duration::from_secs(10)).await;
    }
}
