//! DF-20-1D3-TEST-EVIDENCE, device-flow items: the Copilot policy enablement
//! is gauged structurally (requests held at a raw stub until the test releases
//! them; quiescence read on a paused clock, never wall time), cancellation
//! during a held poll and during enablement, a policy transport failure, and
//! `prompt_line` end of input.

use super::common::{Held, World, accept};
use crate::conformance_20_1d3_copilot::common::{
    DEVICE_PATH, ENDPOINT_A, PROBE_PATH, PUBLIC_CLIENT, Recorder, SEED_IDS, TOKEN_PATH,
    assert_nothing_stored, device_form, mount_login, oauth, probe_reply, rows, try_login,
};
use rustain::adapters::auth::flows::FlowError;
use serde_json::json;
use serial_test::serial;
use std::time::Duration;
use tokio::net::TcpListener;
use wiremock::MockServer;

const TOKEN: &str = "t-df-structural";

/// A raw origin for every Copilot login request; returns the listener.
async fn raw_world(server: &MockServer) -> (World, TcpListener) {
    let world = World::new(server);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    world.set(
        "RUSTAIN_TEST_AUTH_ORIGIN",
        &format!("http://{}", listener.local_addr().unwrap()),
    );
    (world, listener)
}

/// Serves the device start, one successful poll and the routing probe.
async fn serve_until_policies(listener: &TcpListener) {
    let start = accept(listener).await;
    assert_eq!(start.path(), DEVICE_PATH);
    start
        .json(&json!({
            "device_code": "DC", "user_code": "ABCD-1234",
            "verification_uri": "https://github.com/login/device", "interval": 5, "expires_in": 900,
        }))
        .await;
    let poll = accept(listener).await;
    assert_eq!(poll.path(), TOKEN_PATH);
    poll.json(&json!({"access_token": TOKEN, "token_type": "bearer", "scope": "read:user"}))
        .await;
    let probe = accept(listener).await;
    assert_eq!(probe.path(), PROBE_PATH);
    probe
        .json(&json!({"login": "octocat", "endpoints": {"api": ENDPOINT_A}}))
        .await;
}

/// No further request arrives while the client is quiescent: the clock is
/// paused for the probe, and a paused runtime advances time only once no I/O
/// is pending, so the 1 s bound is virtual, never a wall-clock wait.
async fn assert_quiet(listener: &TcpListener, why: &str) {
    tokio::time::pause();
    let quiet = tokio::time::timeout(Duration::from_secs(1), listener.accept())
        .await
        .is_err();
    tokio::time::resume();
    assert!(quiet, "{why}");
}

/// Exactly five policy requests are in flight at a time: five arrive and are
/// held, no sixth arrives until all five are released, for all 55 seed ids.
#[tokio::test]
#[serial]
async fn policy_enablement_holds_exactly_five_in_flight() {
    let server = MockServer::start().await;
    let (_world, listener) = raw_world(&server).await;
    let recorder = Recorder::new().answering("");
    let login = {
        let recorder = recorder.clone();
        tokio::spawn(async move { try_login(&recorder).await })
    };
    serve_until_policies(&listener).await;
    let mut released = 0;
    while released < SEED_IDS {
        let batch = (SEED_IDS - released).min(5);
        let mut held: Vec<Held> = Vec::new();
        for _ in 0..batch {
            let request = accept(&listener).await;
            assert!(request.path().ends_with("/policy"), "{}", request.path());
            held.push(request);
        }
        assert_eq!(held.len(), batch);
        assert_quiet(&listener, "a sixth policy request while five are held").await;
        for request in held {
            request.json(&json!({})).await;
        }
        released += batch;
    }
    login.await.unwrap().expect("the login succeeds");
    assert_eq!(rows("github-copilot").await.len(), 1);
}

/// Cancel while a device poll is in flight: `Login cancelled`, nothing stored.
#[tokio::test]
#[serial]
async fn cancel_during_an_in_flight_poll_stores_nothing() {
    let server = MockServer::start().await;
    let (world, listener) = raw_world(&server).await;
    let recorder = Recorder::new().answering("");
    let login = {
        let recorder = recorder.clone();
        tokio::spawn(async move { try_login(&recorder).await })
    };
    let start = accept(&listener).await;
    start
        .json(&json!({
            "device_code": "DC", "user_code": "ABCD-1234",
            "verification_uri": "https://github.com/login/device", "interval": 5, "expires_in": 900,
        }))
        .await;
    let held_poll = accept(&listener).await;
    assert_eq!(held_poll.path(), TOKEN_PATH);
    recorder.cancel.cancel();
    assert!(matches!(login.await.unwrap(), Err(FlowError::Cancelled)));
    drop(held_poll);
    assert_nothing_stored(&world);
}

/// Cancel during policy enablement: `Login cancelled`, nothing stored.
#[tokio::test]
#[serial]
async fn cancel_during_enablement_stores_nothing() {
    let server = MockServer::start().await;
    let (world, listener) = raw_world(&server).await;
    let recorder = Recorder::new().answering("");
    let login = {
        let recorder = recorder.clone();
        tokio::spawn(async move { try_login(&recorder).await })
    };
    serve_until_policies(&listener).await;
    let held = accept(&listener).await;
    assert!(held.path().ends_with("/policy"));
    recorder.cancel.cancel();
    assert!(matches!(login.await.unwrap(), Err(FlowError::Cancelled)));
    drop(held);
    assert_nothing_stored(&world);
}

/// Every policy request failing in transport never fails the login.
#[tokio::test]
#[serial]
async fn policy_transport_failure_never_fails_the_login() {
    let server = MockServer::start().await;
    let (_world, listener) = raw_world(&server).await;
    let recorder = Recorder::new().answering("");
    let login = {
        let recorder = recorder.clone();
        tokio::spawn(async move { try_login(&recorder).await })
    };
    serve_until_policies(&listener).await;
    for _ in 0..SEED_IDS {
        // Closed without a response: a transport error at the client.
        drop(accept(&listener).await);
    }
    login.await.unwrap().expect("policy failures are ignored");
    let stored = rows("github-copilot").await;
    assert_eq!(oauth(&stored[0]).access.expose_secret(), TOKEN);
}

/// `prompt_line` end of input (`None`) is a blank answer: github.com.
#[tokio::test]
#[serial]
async fn prompt_line_end_of_input_is_github_com() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let recorder = Recorder::new();
    let _guards = mount_login(&server, &recorder, "t-eof", probe_reply(ENDPOINT_A)).await;
    try_login(&recorder).await.expect("EOF selects github.com");
    let starts: Vec<String> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.url.path() == DEVICE_PATH)
        .map(|request| String::from_utf8(request.body.clone()).unwrap())
        .collect();
    assert_eq!(starts, [device_form(PUBLIC_CLIENT)]);
    assert_eq!(
        oauth(&rows("github-copilot").await[0])
            .routing
            .enterprise_domain,
        None
    );
}
