//! GitHub Copilot device login through `flows::login("github-copilot",
//! Device(&COPILOT_DEVICE))`: the public happy path with poll pacing, routing
//! probe and batched model-policy enablement (K01, AC1); deadline, terminal
//! replies and cancellation (K02, AC3); Enterprise domains (AC2); nonfatal
//! probe and policy failures (K03, AC4); re-login, a 401 on inference and the
//! stored / environment credential (K04, AC5).

use super::common::{
    DEVICE_PATH, ENDPOINT_A, ENTERPRISE_CLIENT, Event, LIFETIME_MS, NOW, POLICY_PATH, PROBE_PATH,
    PROMPT, PROVIDER, PUBLIC_CLIENT, ROOT_A, Recorder, SEED_IDS, TOKEN_PATH, USER_CODE, VERIFY,
    World, assert_nothing_stored, body, build, chat_path, complete, device_form, device_reply,
    failure, header, json_reply, login, login_with, mount_routes, oauth, options, policy_posts,
    poll_error, poll_form, probe_reply, received, requests, rows, text_of, token_reply, try_login,
    user,
};
use crate::conformance_20_1d2_device_and_xai::device::{display, get, post, progress};
use rustain::adapters::auth::flows::FlowError;
use rustain::adapters::github_copilot::catalog::SEED;
use rustain::domain::errors::ProviderError;
use rustain::domain::models::{CredentialIdentity, CredentialOrigin, CredentialRouting};
use serde_json::{Value, json};
use serial_test::serial;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use wiremock::{
    Mock, MockServer, Request, Respond, ResponseTemplate,
    matchers::{method, path, path_regex},
};

const LOGIN_USER_AGENT: &str = "copilot-developer-action/0.0.1";
const COPILOT_USER_AGENT: &str = "copilot/1.0.82";
const POLICY_BODY: &[u8] = br#"{"state":"enabled"}"#;
const SLOW_DOWN_TIMEOUT: &str = "Device flow timed out after one or more slow_down responses. This is often caused by clock drift in WSL or VM environments. Please sync or restart the VM clock and try again.";

// ── helpers ─────────────────────────────────────────────────────────────────

/// The seed ids, in file order.
fn seed_ids() -> Vec<&'static str> {
    SEED.iter().map(|row| row.id).collect()
}

/// The model id of a `<root>/models/<id>/policy` request.
fn policy_id(request: &Request) -> String {
    request
        .url
        .path()
        .rsplit('/')
        .nth(1)
        .expect("a policy path has a model id")
        .to_owned()
}

/// Every request the stub received, matched or not.
async fn total(server: &MockServer) -> usize {
    server.received_requests().await.unwrap().len()
}

/// The three login headers on a device start or poll request.
fn assert_login_headers(request: &Request) {
    assert_eq!(
        header(request, "accept").as_deref(),
        Some("application/json")
    );
    assert_eq!(
        header(request, "content-type").as_deref(),
        Some("application/x-www-form-urlencoded")
    );
    assert_eq!(
        header(request, "user-agent").as_deref(),
        Some(LOGIN_USER_AGENT)
    );
}

/// Device start, polls (answered `polls` in order, the last repeating), a probe
/// reporting `ENDPOINT_A` and every policy POST answered 200: a mutant that
/// keeps going after a failure would store a row.
async fn mount_flow(
    server: &MockServer,
    recorder: &Recorder,
    device: ResponseTemplate,
    polls: Vec<ResponseTemplate>,
) {
    recorder
        .mount(server, "POST", DEVICE_PATH, vec![device])
        .await;
    recorder.mount(server, "POST", TOKEN_PATH, polls).await;
    recorder
        .mount(server, "GET", PROBE_PATH, vec![probe_reply(ENDPOINT_A)])
        .await;
    mount_policy(server, json_reply(200, json!({}))).await;
}

async fn mount_policy(server: &MockServer, template: ResponseTemplate) {
    Mock::given(method("POST"))
        .and(path_regex(POLICY_PATH))
        .respond_with(template)
        .mount(server)
        .await;
}

/// A public login whose first poll yields `token`, with the probe answering
/// `probe` and every policy POST answering `policy`; it must succeed.
async fn login_with_policy(
    server: &MockServer,
    token: &str,
    probe: ResponseTemplate,
    policy: ResponseTemplate,
) -> Recorder {
    let recorder = Recorder::new();
    recorder
        .mount(server, "POST", DEVICE_PATH, vec![device_reply(5.0, 900.0)])
        .await;
    recorder
        .mount(server, "POST", TOKEN_PATH, vec![token_reply(token)])
        .await;
    recorder.mount(server, "GET", PROBE_PATH, vec![probe]).await;
    mount_policy(server, policy).await;
    try_login(&recorder)
        .await
        .expect("probe and policy failures never fail the login");
    recorder
}

/// K01's policy responder: records the arrival order of model ids. The
/// "exactly 5 in flight" bound is proven structurally (held requests released
/// by the test, quiescence on a paused clock) by
/// `conformance_20_1e_tui_login::inherited::policy_enablement_holds_exactly_five_in_flight`;
/// here the arrivals must come in the consecutive 5-id seed chunks, in order.
#[derive(Clone)]
struct PolicyGauge {
    arrivals: Arc<Mutex<Vec<String>>>,
}

impl PolicyGauge {
    async fn mount(server: &MockServer) -> Self {
        let gauge = Self {
            arrivals: Arc::new(Mutex::new(Vec::new())),
        };
        Mock::given(method("POST"))
            .and(path_regex(POLICY_PATH))
            .respond_with(gauge.clone())
            .mount(server)
            .await;
        gauge
    }
    fn ids(&self) -> Vec<String> {
        self.arrivals.lock().unwrap().clone()
    }
}

impl Respond for PolicyGauge {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        self.arrivals.lock().unwrap().push(policy_id(request));
        json_reply(200, json!({}))
    }
}

/// Every `routing` object anywhere in a JSON document.
fn routing_values(value: &Value, out: &mut Vec<Value>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                if key == "routing" {
                    out.push(child.clone());
                } else {
                    routing_values(child, out);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                routing_values(item, out);
            }
        }
        _ => {}
    }
}

// ── K01 / AC1: public happy path ────────────────────────────────────────────

/// K01 (M01): a public device login prompts once, paces polls
/// `[6000, 6000, 14000, 9800]` through pending / slow_down / slow_down(7),
/// probes once with `token`, enables all 55 seed models in sequential batches
/// of 5 with the policy headers, and stores the far-future grant with the
/// normalized endpoint.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn k01_public_login_paces_polls_probes_enables_models_in_batches_and_stores_the_grant() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let token = "t-k01-public";
    let recorder = Recorder::new().answering("");
    recorder
        .mount(&server, "POST", DEVICE_PATH, vec![device_reply(5.0, 900.0)])
        .await;
    recorder
        .mount(
            &server,
            "POST",
            TOKEN_PATH,
            vec![
                poll_error("authorization_pending"),
                poll_error("slow_down"),
                json_reply(200, json!({"error": "slow_down", "interval": 7})),
                token_reply(token),
            ],
        )
        .await;
    recorder
        .mount(&server, "GET", PROBE_PATH, vec![probe_reply(ENDPOINT_A)])
        .await;
    let gauge = PolicyGauge::mount(&server).await;

    let result = try_login(&recorder)
        .await
        .expect("the public login succeeds");
    assert_eq!(result.provider, PROVIDER);
    assert_eq!(result.method, "oauth");
    assert_eq!(result.identity, CredentialIdentity::default());

    // Pacing: 5 s × 1.2; slow_down adds 5 s and switches to × 1.4; a slow_down
    // with `interval: 7` replaces the interval.
    assert_eq!(recorder.sleeps(), [6000, 6000, 14000, 9800]);
    let done = NOW + 35_800;
    assert_eq!(recorder.now(), done);
    // The prompt is the first event, exactly once, before any request.
    assert_eq!(
        recorder.events(),
        [
            Event::Prompt(PROMPT.into()),
            post(DEVICE_PATH, NOW),
            display(VERIFY, USER_CODE),
            Event::Sleep(6000),
            post(TOKEN_PATH, NOW + 6000),
            Event::Sleep(6000),
            post(TOKEN_PATH, NOW + 12_000),
            Event::Sleep(14_000),
            post(TOKEN_PATH, NOW + 26_000),
            Event::Sleep(9800),
            post(TOKEN_PATH, done),
            get(PROBE_PATH, done),
            progress("Enabling models..."),
        ]
    );

    // Device start and poll bytes and headers.
    let starts = received(&server, DEVICE_PATH).await;
    assert_eq!(starts.len(), 1);
    assert_eq!(body(&starts[0]), device_form(PUBLIC_CLIENT));
    assert_eq!(
        body(&starts[0]),
        "client_id=Ov23li8tweQw6odWQebz&scope=read%3Auser"
    );
    assert_login_headers(&starts[0]);
    let polls = received(&server, TOKEN_PATH).await;
    assert_eq!(polls.len(), 4);
    for poll in &polls {
        assert_eq!(body(poll), poll_form(PUBLIC_CLIENT));
        assert_login_headers(poll);
    }

    // One probe: `token`, the Copilot user agent, no API version.
    let probes = requests(&server, "GET", PROBE_PATH).await;
    assert_eq!(probes.len(), 1);
    let probe = &probes[0];
    assert_eq!(
        header(probe, "authorization"),
        Some(format!("token {token}"))
    );
    assert_eq!(
        header(probe, "user-agent").as_deref(),
        Some(COPILOT_USER_AGENT)
    );
    assert_eq!(header(probe, "accept").as_deref(), Some("application/json"));
    assert_eq!(header(probe, "x-github-api-version"), None);

    // 55 policy POSTs under the probed root, one per seed id, with the policy
    // headers and body.
    let posts = policy_posts(&server).await;
    assert_eq!(posts.len(), SEED_IDS);
    let mut seen = BTreeSet::new();
    for request in &posts {
        let id = policy_id(request);
        assert_eq!(
            request.url.path(),
            format!("{ROOT_A}/models/{id}/policy"),
            "policy POSTs go under the probed endpoint"
        );
        assert!(seen.insert(id.clone()), "{id} enabled twice");
        assert_eq!(request.body, POLICY_BODY, "{id}");
        for (name, value) in [
            ("content-type", "application/json"),
            ("user-agent", COPILOT_USER_AGENT),
            ("editor-version", COPILOT_USER_AGENT),
            ("copilot-harness-id", "copilot-sdk"),
            ("openai-intent", "chat-policy"),
            ("x-github-api-version", "2026-08-01"),
            ("x-initiator", "user"),
            ("x-interaction-type", "chat-policy"),
            ("copilot-integration-id", "copilot-chat"),
        ] {
            assert_eq!(
                header(request, name).as_deref(),
                Some(value),
                "{id}: {name}"
            );
        }
        assert_eq!(
            header(request, "authorization"),
            Some(format!("Bearer {token}")),
            "{id}"
        );
    }
    let expected: BTreeSet<String> = seed_ids().into_iter().map(str::to_owned).collect();
    assert_eq!(seen, expected);

    // Batching: 11 consecutive arrival groups, each exactly one 5-id seed chunk,
    // in chunk order.
    let arrived = gauge.ids();
    assert_eq!(arrived.len(), SEED_IDS);
    let seeds = seed_ids();
    for (index, chunk) in seeds.chunks(5).enumerate() {
        let group: BTreeSet<&str> = arrived[index * 5..index * 5 + chunk.len()]
            .iter()
            .map(String::as_str)
            .collect();
        let chunk: BTreeSet<&str> = chunk.iter().copied().collect();
        assert_eq!(group, chunk, "arrival group {index}");
    }

    // The stored row.
    let stored = rows(PROVIDER).await;
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].origin, CredentialOrigin::Login);
    let grant = oauth(&stored[0]);
    assert_eq!(grant.access.expose_secret(), token);
    assert_eq!(grant.refresh.expose_secret(), token);
    assert_eq!(grant.expires_at_ms, done + LIFETIME_MS);
    assert_eq!(grant.authorized_at_ms, Some(done));
    assert_eq!(grant.identity, CredentialIdentity::default());
    assert_eq!(
        grant.routing,
        CredentialRouting {
            enterprise_domain: None,
            api_endpoint: Some(ENDPOINT_A.into()),
        }
    );
}

// ── K02 / AC3: failure semantics ────────────────────────────────────────────

/// K02 (M02): `expires_in: 10` caps the second wait at the remaining 4 s; after
/// two pending polls the login fails `Device flow timed out`, poll 3 (which
/// would succeed) is never sent and nothing is stored.
#[tokio::test]
#[serial]
async fn k02_expiry_caps_the_last_wait_and_never_sends_the_late_poll() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    let recorder = Recorder::new();
    mount_flow(
        &server,
        &recorder,
        device_reply(5.0, 10.0),
        vec![
            poll_error("authorization_pending"),
            poll_error("authorization_pending"),
            token_reply("t-k02-late"),
        ],
    )
    .await;

    assert_eq!(failure(try_login(&recorder).await), "Device flow timed out");
    assert_eq!(recorder.sleeps(), [6000, 4000]);
    assert_eq!(recorder.times(TOKEN_PATH), [NOW + 6000, NOW + 10_000]);
    assert_eq!(recorder.paths(), [DEVICE_PATH, TOKEN_PATH, TOKEN_PATH]);
    assert_eq!(total(&server).await, 3, "no probe, no policy");
    assert_nothing_stored(&world);
}

/// K02 (M02): an expiry after a `slow_down` reports the WSL clock-drift text.
#[tokio::test]
#[serial]
async fn k02_expiry_after_slow_down_reports_clock_drift() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    let recorder = Recorder::new();
    mount_flow(
        &server,
        &recorder,
        device_reply(5.0, 20.0),
        vec![
            poll_error("slow_down"),
            poll_error("authorization_pending"),
            token_reply("t-k02-wsl-late"),
        ],
    )
    .await;

    assert_eq!(failure(try_login(&recorder).await), SLOW_DOWN_TIMEOUT);
    assert_eq!(recorder.sleeps(), [6000, 14_000]);
    assert_eq!(recorder.times(TOKEN_PATH), [NOW + 6000, NOW + 20_000]);
    assert_eq!(total(&server).await, 3);
    assert_nothing_stored(&world);
}

/// K02 (M02): terminal poll replies fail with their exact text after one poll,
/// never echo an unsafe error string, and store nothing.
#[tokio::test]
#[serial]
async fn k02_terminal_poll_replies_fail_with_exact_text_and_stop_polling() {
    let cases = [
        (
            poll_error("access_denied"),
            "Device flow failed: access_denied",
        ),
        (
            poll_error("expired_token"),
            "Device flow failed: expired_token",
        ),
        (poll_error("Weird Error!"), "Device flow failed"),
        (
            ResponseTemplate::new(400),
            "github-copilot device token request failed: 400",
        ),
        (
            json_reply(
                400,
                json!({"error": "invalid_grant", "error_description": "SECRET"}),
            ),
            "github-copilot device token request failed: 400 invalid_grant",
        ),
        (
            ResponseTemplate::new(200).set_body_string("SECRET-NOT-JSON"),
            "github-copilot device token response was not JSON",
        ),
    ];
    for (index, (reply, expected)) in cases.into_iter().enumerate() {
        let server = MockServer::start().await;
        let world = World::new(&server);
        let recorder = Recorder::new();
        mount_flow(
            &server,
            &recorder,
            device_reply(5.0, 900.0),
            vec![reply, token_reply(&format!("t-k02-terminal-{index}"))],
        )
        .await;

        let message = failure(try_login(&recorder).await);
        assert_eq!(message, expected);
        assert!(!message.contains("SECRET"), "{expected}");
        assert_eq!(recorder.sleeps(), [6000], "{expected}");
        assert_eq!(
            recorder.paths(),
            [DEVICE_PATH, TOKEN_PATH],
            "{expected}: no further request"
        );
        assert_eq!(total(&server).await, 2, "{expected}");
        assert_nothing_stored(&world);
    }
}

/// AC3: a failed or malformed device start fails with its exact text before
/// any display, wait or poll, and stores nothing.
#[tokio::test]
#[serial]
async fn ac3_device_start_failures_fail_before_any_poll() {
    let fields = |device_code: Value, interval: Value| {
        json_reply(
            200,
            json!({
                "device_code": device_code,
                "user_code": USER_CODE,
                "verification_uri": VERIFY,
                "interval": interval,
                "expires_in": 900,
            }),
        )
    };
    let cases = [
        (
            ResponseTemplate::new(500),
            "github-copilot device authorization failed: 500",
        ),
        (
            json_reply(
                400,
                json!({"error": "unauthorized_client", "error_description": "SECRET"}),
            ),
            "github-copilot device authorization failed: 400 unauthorized_client",
        ),
        (
            json_reply(403, json!({"error": "Not Whitelisted"})),
            "github-copilot device authorization failed: 403",
        ),
        (
            json_reply(200, json!([1, 2])),
            "Invalid device code response",
        ),
        (
            ResponseTemplate::new(200).set_body_string("SECRET-NOT-JSON"),
            "Invalid device code response",
        ),
        (
            fields(json!(7), json!(5)),
            "Invalid device code response fields",
        ),
        (
            fields(json!("DEVICE-CODE-COPILOT"), json!("5")),
            "Invalid device code response fields",
        ),
    ];
    for (index, (reply, expected)) in cases.into_iter().enumerate() {
        let server = MockServer::start().await;
        let world = World::new(&server);
        let recorder = Recorder::new();
        mount_flow(
            &server,
            &recorder,
            reply,
            vec![token_reply(&format!("t-ac3-start-{index}"))],
        )
        .await;

        let message = failure(try_login(&recorder).await);
        assert_eq!(message, expected);
        assert!(!message.contains("SECRET"), "{expected}");
        assert_eq!(
            recorder.events(),
            [Event::Prompt(PROMPT.into()), post(DEVICE_PATH, NOW)],
            "{expected}: no display, wait or poll"
        );
        assert_eq!(total(&server).await, 1, "{expected}");
        assert_nothing_stored(&world);
    }
}

/// K02 (M02): cancelling during the second recorded wait returns
/// `Login cancelled` with the poll count frozen at one.
#[tokio::test]
#[serial]
async fn k02_cancel_during_a_recorded_wait_freezes_polling() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    let recorder = Recorder::cancelling_at_sleep(1);
    mount_flow(
        &server,
        &recorder,
        device_reply(5.0, 900.0),
        vec![
            poll_error("authorization_pending"),
            token_reply("t-k02-cancelled"),
        ],
    )
    .await;

    let result = try_login(&recorder).await;
    assert!(matches!(result, Err(FlowError::Cancelled)), "{result:?}");
    assert_eq!(failure(result), "Login cancelled");
    assert_eq!(recorder.sleeps(), [6000, 6000]);
    assert_eq!(recorder.times(TOKEN_PATH), [NOW + 6000]);
    assert_eq!(received(&server, TOKEN_PATH).await.len(), 1);
    assert_eq!(total(&server).await, 2);
    assert_nothing_stored(&world);
}

/// AC3: a poll 2xx `{}` or `[]` keeps polling at the unchanged cadence.
#[tokio::test]
#[serial]
async fn ac3_empty_object_and_array_polls_continue_at_the_same_cadence() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let recorder = Recorder::new();
    let token = "t-ac3-empty";
    mount_flow(
        &server,
        &recorder,
        device_reply(5.0, 900.0),
        vec![
            json_reply(200, json!({})),
            json_reply(200, json!([])),
            token_reply(token),
        ],
    )
    .await;

    try_login(&recorder).await.expect("the login continues");
    assert_eq!(recorder.sleeps(), [6000, 6000, 6000]);
    assert_eq!(
        recorder.times(TOKEN_PATH),
        [NOW + 6000, NOW + 12_000, NOW + 18_000]
    );
    let stored = rows(PROVIDER).await;
    assert_eq!(stored.len(), 1);
    assert_eq!(oauth(&stored[0]).access.expose_secret(), token);
}

// ── AC2: Enterprise ─────────────────────────────────────────────────────────

/// AC2: an Enterprise answer uses the Copilot CLI client on start and poll,
/// probes the public API path, stores the lowercased domain and (no probed
/// endpoint) enables models under the Enterprise root.
#[tokio::test]
#[serial]
async fn ac2_enterprise_login_uses_the_cli_client_and_the_enterprise_root() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let result = login_with(
        &server,
        "https://Company.GHE.com/path",
        "t-ac2-enterprise",
        ResponseTemplate::new(500),
    )
    .await;
    assert_eq!(result.provider, PROVIDER);

    let starts = received(&server, DEVICE_PATH).await;
    assert_eq!(starts.len(), 1);
    assert_eq!(body(&starts[0]), device_form(ENTERPRISE_CLIENT));
    let polls = received(&server, TOKEN_PATH).await;
    assert_eq!(polls.len(), 1);
    assert_eq!(body(&polls[0]), poll_form(ENTERPRISE_CLIENT));
    assert_eq!(requests(&server, "GET", PROBE_PATH).await.len(), 1);

    let stored = rows(PROVIDER).await;
    assert_eq!(stored.len(), 1);
    assert_eq!(
        oauth(&stored[0]).routing,
        CredentialRouting {
            enterprise_domain: Some("company.ghe.com".into()),
            api_endpoint: None,
        }
    );

    // `https://copilot-api.company.ghe.com` rebases to the bare stub origin.
    let paths: BTreeSet<String> = policy_posts(&server)
        .await
        .iter()
        .map(|request| request.url.path().to_owned())
        .collect();
    let expected: BTreeSet<String> = seed_ids()
        .into_iter()
        .map(|id| format!("/models/{id}/policy"))
        .collect();
    assert_eq!(paths, expected);
    assert_eq!(policy_posts(&server).await.len(), SEED_IDS);

    // Inference from the stored Enterprise row goes to the same Enterprise
    // root as the CLI identity, with the stored grant.
    mount_routes(&server, "", "ok").await;
    let provider = build("grok-code-fast-1");
    let chunks = complete(
        &provider,
        vec![user("hi")],
        options("grok-code-fast-1", 8192, None),
    )
    .await
    .expect("the Enterprise inference succeeds");
    assert_eq!(text_of(&chunks), "ok");
    let sent = requests(&server, "POST", &chat_path("")).await;
    assert_eq!(sent.len(), 1, "one POST to the Enterprise root");
    assert_eq!(
        header(&sent[0], "authorization").as_deref(),
        Some("Bearer t-ac2-enterprise")
    );
    assert_eq!(
        header(&sent[0], "copilot-integration-id").as_deref(),
        Some("copilot-developer-cli")
    );
}

/// AC2: an unparsable Enterprise answer fails before any request and stores
/// nothing.
#[tokio::test]
#[serial]
async fn ac2_invalid_enterprise_answer_fails_without_any_request() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    let recorder = Recorder::new().answering("foo bar");
    mount_flow(
        &server,
        &recorder,
        device_reply(5.0, 900.0),
        vec![token_reply("t-ac2-invalid")],
    )
    .await;

    assert_eq!(
        failure(try_login(&recorder).await),
        "Invalid GitHub Enterprise URL/domain"
    );
    assert_eq!(recorder.events(), [Event::Prompt(PROMPT.into())]);
    assert_eq!(total(&server).await, 0);
    assert_nothing_stored(&world);
}

// ── K03 / AC4: probe and policy are nonfatal ────────────────────────────────

/// K03 (M03): every policy POST denied 403 is resent once as the Copilot CLI
/// (110 POSTs) and the login still stores the row.
#[tokio::test]
#[serial]
async fn k03_denied_policy_resends_once_as_cli_and_the_login_stores_the_row() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let token = "t-k03-denied";
    login_with_policy(
        &server,
        token,
        probe_reply(ENDPOINT_A),
        ResponseTemplate::new(403),
    )
    .await;

    let posts = policy_posts(&server).await;
    assert_eq!(posts.len(), 2 * SEED_IDS);
    let mut by_id: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for request in &posts {
        by_id
            .entry(policy_id(request))
            .or_default()
            .push(header(request, "copilot-integration-id").unwrap_or_default());
    }
    let ids: BTreeSet<String> = by_id.keys().cloned().collect();
    let expected: BTreeSet<String> = seed_ids().into_iter().map(str::to_owned).collect();
    assert_eq!(ids, expected);
    for (id, sent) in &by_id {
        assert_eq!(
            sent,
            &["copilot-chat", "copilot-developer-cli"],
            "{id}: once per identity"
        );
    }

    let stored = rows(PROVIDER).await;
    assert_eq!(stored.len(), 1);
    assert_eq!(oauth(&stored[0]).access.expose_secret(), token);
}

/// K03 (M03): a `COPILOT_INTEGRATION_ID` pin is sent on every policy POST and
/// a denial is never resent under another identity.
#[tokio::test]
#[serial]
async fn k03_pinned_integration_id_is_sent_on_every_policy_post_without_fallback() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    world.set("COPILOT_INTEGRATION_ID", "custom-id");
    login_with_policy(
        &server,
        "t-k03-pinned",
        probe_reply(ENDPOINT_A),
        ResponseTemplate::new(403),
    )
    .await;

    let posts = policy_posts(&server).await;
    assert_eq!(posts.len(), SEED_IDS);
    for request in &posts {
        assert_eq!(
            header(request, "copilot-integration-id").as_deref(),
            Some("custom-id")
        );
    }
    assert_eq!(rows(PROVIDER).await.len(), 1);
}

/// K03 (M03): policy 500s are neither retried nor fatal.
#[tokio::test]
#[serial]
async fn k03_policy_server_errors_are_not_retried_and_not_fatal() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    login_with_policy(
        &server,
        "t-k03-500",
        probe_reply(ENDPOINT_A),
        ResponseTemplate::new(500),
    )
    .await;

    let posts = policy_posts(&server).await;
    assert_eq!(posts.len(), SEED_IDS);
    for request in &posts {
        assert_eq!(
            header(request, "copilot-integration-id").as_deref(),
            Some("copilot-chat")
        );
    }
    assert_eq!(rows(PROVIDER).await.len(), 1);
}

/// K03 (M03): an unusable probe answer stores no endpoint, the login succeeds
/// and policy enablement falls back to the canonical root.
#[tokio::test]
#[serial]
async fn k03_unusable_probe_answers_store_no_endpoint_and_the_login_succeeds() {
    let cases = [
        ("500", ResponseTemplate::new(500)),
        (
            "invalid json",
            ResponseTemplate::new(200).set_body_string("not json"),
        ),
        ("http", probe_reply("http://api.example/x")),
        ("no host", probe_reply("https://")),
        ("empty object", json_reply(200, json!({}))),
    ];
    for (index, (label, probe)) in cases.into_iter().enumerate() {
        let server = MockServer::start().await;
        let _world = World::new(&server);
        let token = format!("t-k03-probe-{index}");
        login_with_policy(&server, &token, probe, json_reply(200, json!({}))).await;

        assert_eq!(
            requests(&server, "GET", PROBE_PATH).await.len(),
            1,
            "{label}"
        );
        let stored = rows(PROVIDER).await;
        assert_eq!(stored.len(), 1, "{label}");
        let grant = oauth(&stored[0]);
        assert_eq!(grant.access.expose_secret(), token, "{label}");
        assert_eq!(grant.routing, CredentialRouting::default(), "{label}");
        let posts = policy_posts(&server).await;
        assert_eq!(posts.len(), SEED_IDS, "{label}");
        for request in &posts {
            assert_eq!(
                request.url.path(),
                format!("/models/{}/policy", policy_id(request)),
                "{label}: canonical root"
            );
        }
    }
}

/// AC4: a probed endpoint loses its trailing slash and keeps its path.
#[tokio::test]
#[serial]
async fn ac4_probe_trailing_slash_is_stripped_and_the_path_kept() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    login(
        &server,
        "t-ac4-slash",
        "https://api.business.githubcopilot.com/x/",
    )
    .await;

    let stored = rows(PROVIDER).await;
    assert_eq!(stored.len(), 1);
    assert_eq!(
        oauth(&stored[0]).routing.api_endpoint.as_deref(),
        Some("https://api.business.githubcopilot.com/x")
    );
    let posts = policy_posts(&server).await;
    assert_eq!(posts.len(), SEED_IDS);
    assert!(
        posts
            .iter()
            .all(|request| request.url.path().starts_with("/x/models/"))
    );
}

// ── K04 / AC5: re-login, 401, stored and environment credentials ────────────

/// K04 (M04): a second login replaces the served row (one enabled row, the new
/// token, a new generation) and the next completion sends the new bearer.
#[tokio::test]
#[serial]
async fn k04_relogin_replaces_the_served_row_and_the_next_request_uses_it() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let (first, second) = ("t1-k04-relogin", "t2-k04-relogin");
    login(&server, first, ENDPOINT_A).await;
    let before = rows(PROVIDER).await;
    assert_eq!(before.len(), 1);
    let first_generation = before[0].generation;

    login(&server, second, ENDPOINT_A).await;
    let enabled: Vec<_> = rows(PROVIDER)
        .await
        .into_iter()
        .filter(|row| row.disabled_cause.is_none())
        .collect();
    assert_eq!(enabled.len(), 1, "exactly one enabled Copilot row");
    assert_eq!(oauth(&enabled[0]).access.expose_secret(), second);
    assert_ne!(enabled[0].generation, first_generation, "a new login epoch");

    mount_routes(&server, ROOT_A, "ok").await;
    let provider = build("grok-code-fast-1");
    let chunks = complete(
        &provider,
        vec![user("hi")],
        options("grok-code-fast-1", 256, None),
    )
    .await
    .expect("the completion succeeds");
    assert_eq!(text_of(&chunks), "ok");
    let sent = requests(&server, "POST", &chat_path(ROOT_A)).await;
    assert_eq!(sent.len(), 1);
    assert_eq!(
        header(&sent[0], "authorization"),
        Some(format!("Bearer {second}"))
    );
}

/// K04 (M04): a 401 on inference is final `AuthenticationFailed` after exactly
/// one POST, sends nothing to the token endpoint, keeps the row's routing, and
/// the next request still reaches the probed root.
#[tokio::test]
#[serial]
async fn k04_inference_401_is_final_without_token_traffic_and_keeps_routing() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let token = "t-k04-revoked";
    login(&server, token, ENDPOINT_A).await;
    let before = rows(PROVIDER).await[0].stamp();
    let token_requests = requests(&server, "POST", TOKEN_PATH).await.len();
    let device_requests = requests(&server, "POST", DEVICE_PATH).await.len();
    let route = chat_path(ROOT_A);

    let denied = Mock::given(method("POST"))
        .and(path(route.as_str()))
        .respond_with(ResponseTemplate::new(401))
        .mount_as_scoped(&server)
        .await;
    let provider = build("grok-code-fast-1");
    match complete(
        &provider,
        vec![user("hi")],
        options("grok-code-fast-1", 256, None),
    )
    .await
    {
        Err(ProviderError::AuthenticationFailed) => {}
        Err(other) => panic!("expected AuthenticationFailed, got {other:?}"),
        Ok(_) => panic!("a 401 must fail the request"),
    }
    assert_eq!(requests(&server, "POST", &route).await.len(), 1);
    assert_eq!(
        requests(&server, "POST", TOKEN_PATH).await.len(),
        token_requests,
        "refresh sends zero HTTP"
    );
    assert_eq!(
        requests(&server, "POST", DEVICE_PATH).await.len(),
        device_requests
    );
    let stored = rows(PROVIDER).await;
    assert_eq!(stored.len(), 1);
    // The 401 forced the stored grant's refresh-same: a revision bump of the
    // same row and generation, whose identical bearer recovery then refuses.
    let after = stored[0].stamp();
    assert_eq!(
        (after.id, after.generation, after.rev),
        (before.id, before.generation, before.rev + 1)
    );
    let grant = oauth(&stored[0]);
    assert_eq!(grant.access.expose_secret(), token);
    assert_eq!(
        grant.routing,
        CredentialRouting {
            enterprise_domain: None,
            api_endpoint: Some(ENDPOINT_A.into()),
        }
    );
    drop(denied);

    mount_routes(&server, ROOT_A, "ok").await;
    let chunks = complete(
        &provider,
        vec![user("again")],
        options("grok-code-fast-1", 256, None),
    )
    .await
    .expect("the next request succeeds");
    assert_eq!(text_of(&chunks), "ok");
    let sent = requests(&server, "POST", &route).await;
    assert_eq!(
        sent.len(),
        2,
        "the next request still reaches the probed root"
    );
    assert_eq!(
        header(&sent[1], "authorization"),
        Some(format!("Bearer {token}"))
    );
}

/// AC5: an Enterprise login with an endpoint writes exactly that `routing`
/// object to `auth.json`.
#[tokio::test]
#[serial]
async fn ac5_routing_is_written_to_auth_json_exactly() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    login_with(
        &server,
        "company.ghe.com",
        "t-ac5-routing",
        probe_reply(ENDPOINT_A),
    )
    .await;

    let file = std::fs::read_to_string(world.dir.path().join("auth.json")).unwrap();
    let document: Value = serde_json::from_str(&file).unwrap();
    let mut found = Vec::new();
    routing_values(&document, &mut found);
    assert_eq!(
        found,
        [json!({"enterprise_domain": "company.ghe.com", "api_endpoint": ENDPOINT_A})]
    );
}

/// AC5: a raw `COPILOT_GITHUB_TOKEN` (no stored row) is sent as the bearer to
/// the canonical root.
#[tokio::test]
#[serial]
async fn ac5_raw_env_token_is_sent_as_the_bearer() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    let token = "t-ac5-raw-env";
    world.set("COPILOT_GITHUB_TOKEN", token);
    mount_routes(&server, "", "ok").await;

    let provider = build("grok-code-fast-1");
    let chunks = complete(
        &provider,
        vec![user("hi")],
        options("grok-code-fast-1", 256, None),
    )
    .await
    .expect("the env token serves the request");
    assert_eq!(text_of(&chunks), "ok");
    let sent = requests(&server, "POST", &chat_path("")).await;
    assert_eq!(sent.len(), 1);
    assert_eq!(
        header(&sent[0], "authorization"),
        Some(format!("Bearer {token}"))
    );
    assert!(rows(PROVIDER).await.is_empty());
}

/// AC5: a JSON-envelope `COPILOT_GITHUB_TOKEN` sends only its `token` as the
/// bearer, to its normalized `apiEndpoint`.
#[tokio::test]
#[serial]
async fn ac5_env_json_envelope_sends_only_its_token_to_its_endpoint() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    let token = "t-ac5-envelope";
    let envelope = json!({
        "token": token,
        "enterpriseUrl": "Company.GHE.com",
        "apiEndpoint": "https://x.githubcopilot.com/p/",
    })
    .to_string();
    world.set("COPILOT_GITHUB_TOKEN", &envelope);
    mount_routes(&server, "/p", "ok").await;
    mount_routes(&server, "", "wrong root").await;

    let provider = build("grok-code-fast-1");
    let chunks = complete(
        &provider,
        vec![user("hi")],
        options("grok-code-fast-1", 256, None),
    )
    .await
    .expect("the env envelope serves the request");
    assert_eq!(text_of(&chunks), "ok");
    let sent = requests(&server, "POST", &chat_path("/p")).await;
    assert_eq!(sent.len(), 1);
    assert_eq!(
        header(&sent[0], "authorization"),
        Some(format!("Bearer {token}")),
        "the envelope itself is never sent"
    );
    assert!(requests(&server, "POST", &chat_path("")).await.is_empty());
    assert!(rows(PROVIDER).await.is_empty());
}
