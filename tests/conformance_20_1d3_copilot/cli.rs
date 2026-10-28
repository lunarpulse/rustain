//! Debug-binary Copilot flow (K12): device login with the Enterprise prompt
//! answered blank on piped stdin → status → three routed `ask` turns → logout.
//!
//! Real-binary proof on the 20-1d CLI `Fixture`: build the binary with
//! `--features test-instrumentation` first; `RUSTAIN_TEST_CODEX_BINARY` selects it,
//! else the cargo-bin fallback (same instrumentation requirement). Each test keeps
//! its command/output/request evidence under
//! `<RUSTAIN_TEST_CODEX_EVIDENCE_DIR>/20-1d3-<test>/` (a kept temp dir when unset).
//!
//! The device flow sends its first request right after the prompt, so a binary
//! built without instrumentation would reach the real GitHub host. `preflight`
//! first proves that the running binary rebases auth endpoints to the loopback
//! stub (the Codex browser flow prints its authorize URL before any HTTP and is
//! killed right there). No test follows the printed verification URL.

use super::common::{
    DEVICE_CODE, DEVICE_PATH, ENDPOINT_A, PROBE_PATH, PROMPT, PROVIDER, PUBLIC_CLIENT, ROOT_A,
    SEED_IDS, TOKEN_PATH, USER_CODE, chat_path, chat_sse, device_form, device_reply, json_reply,
    messages_path, messages_sse, poll_form, probe_reply, reply, responses_ok, responses_path,
    token_reply,
};
use crate::conformance_20_1d_codex_subscription::cli::Fixture;
use rustain::adapters::auth_store::FileAuthStore;
use rustain::domain::models::{Credential, StoredCredential};
use rustain::domain::ports::AuthStorePort;
use serde_json::{Value, json};
use serial_test::serial;
use std::process::{Output, Stdio};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use wiremock::{
    Mock, MockServer, Request,
    matchers::{method, path_regex},
};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const MODELS_DEV_PATH: &str = "/api.json";
/// The browser flow the preflight starts (and kills before any HTTP).
const CODEX: &str = "openai-codex";
/// Distinct per test: the identity cache is process-global.
const TOKEN: &str = "gho_K12CopilotCliSecretToken0001";
const LOGIN_NAME: &str = "GitHub Copilot";
/// The three models K12 asks, their route under the probed root and the stub text.
const TURNS: [(&str, &str, &str); 3] = [
    (
        "claude-sonnet-4.5",
        "messages",
        "hello Copilot Messages fixture",
    ),
    ("gpt-5.5", "responses", "hello Copilot Responses fixture"),
    ("grok-code-fast-1", "chat", "hello Copilot Chat fixture"),
];

/// The configured entry K12 activates (story K12 keys, plus `discover_models` so
/// the post-login refresh covers the configured alias), for model `m`.
fn copilot_config(model: &str) -> String {
    format!(
        "model = \"{model}\"\n\n[provider.copilot]\nprovider_id = \"github-copilot\"\nkind = \"github-copilot\"\napi_key_env = \"COPILOT_GITHUB_TOKEN\"\nmodel_id = \"{model}\"\nenabled = true\ndiscover_models = true\n"
    )
}

/// The 20-1d fixture with this story's knobs and a per-test evidence directory.
/// Device waits are scaled to zero; the child environment is exactly `env`, so no
/// provider credential variable reaches it.
fn fixture(server: &MockServer, test: &str, config: &str) -> Fixture {
    let mut fixture = Fixture::new(server);
    let shared = fixture.evidence.clone();
    fixture.evidence = shared.join(format!("20-1d3-{test}"));
    std::fs::create_dir_all(&fixture.evidence).unwrap();
    // A reused evidence destination must not make a failed rerun look passed.
    let passed = fixture.evidence.join("passed.json");
    if passed.exists() {
        std::fs::remove_file(passed).unwrap();
    }
    std::fs::copy(
        shared.join("binary.json"),
        fixture.evidence.join("binary.json"),
    )
    .unwrap();
    fixture
        .env
        .insert("RUSTAIN_TEST_AUTH_SLEEP_SCALE", "0".into());
    write_config(&fixture, config, "config.toml");
    for key in fixture.env.keys() {
        assert!(
            ![
                "COPILOT",
                "GITHUB",
                "GH_",
                "XAI_",
                "OPENAI",
                "ANTHROPIC",
                "OPENROUTER"
            ]
            .iter()
            .any(|prefix| key.starts_with(prefix)),
            "provider credential variable {key} must be stripped"
        );
    }
    for (key, dir) in [
        ("HOME", ""),
        ("RUSTAIN_DATA_DIR", "data"),
        ("RUSTAIN_CONFIG_DIR", "config"),
    ] {
        assert_eq!(
            std::path::PathBuf::from(&fixture.env[key]),
            fixture.home.path().join(dir),
            "{key} is isolated under the fixture home"
        );
    }
    fixture
}

/// Writes the active `config.toml` and keeps a copy as evidence `name`.
fn write_config(fixture: &Fixture, config: &str, name: &str) {
    let config_path = fixture.home.path().join("config/config.toml");
    std::fs::write(&config_path, config).unwrap();
    std::fs::copy(&config_path, fixture.evidence.join(name)).unwrap();
}

/// Loopback preflight. The auth origin must be the loopback stub, and the binary
/// must actually rebase to it (only instrumented builds do): the browser flow
/// prints its authorize URL before any HTTP, and is killed as soon as it does.
async fn preflight(fixture: &Fixture, server: &MockServer) {
    let origin = url::Url::parse(&server.uri()).unwrap();
    assert_eq!(origin.scheme(), "http");
    assert_eq!(origin.host_str(), Some("127.0.0.1"));
    assert_eq!(
        fixture.env["RUSTAIN_TEST_AUTH_ORIGIN"].to_str(),
        Some(server.uri().as_str())
    );
    let mut child = fixture
        .command("preflight", &["auth", "login", CODEX, "--method", "oauth"])
        .spawn()
        .unwrap();
    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let mut transcript = String::new();
    let printed = tokio::time::timeout(COMMAND_TIMEOUT, async {
        loop {
            let mut line = String::new();
            assert_ne!(
                stderr.read_line(&mut line).await.unwrap(),
                0,
                "preflight exited before printing its authorize URL: {transcript}"
            );
            transcript.push_str(&line);
            assert!(transcript.len() <= 65_536, "unbounded preflight stderr");
            if line.trim().starts_with("http") {
                break url::Url::parse(line.trim()).unwrap();
            }
        }
    })
    .await
    .expect("preflight did not print its authorize URL within its deadline");
    child.kill().await.unwrap();
    std::fs::write(fixture.evidence.join("preflight.stderr"), &transcript).unwrap();
    assert_eq!(
        printed.origin(),
        origin.origin(),
        "binary MUST be built with test-instrumentation: {printed}"
    );
    assert!(
        server.received_requests().await.unwrap().is_empty(),
        "the preflight sends no HTTP"
    );
    fixture.json(
        "preflight.json",
        &json!({
            "auth_origin": server.uri(),
            "authorize_url_from_child_stderr": printed.as_str(),
            "loopback_origin_asserted_before_any_device_request": true,
            "child_killed_before_any_http": true,
        }),
    );
}

/// `printf '\n' | rustain auth login github-copilot`: the Fixture's command with
/// stdin replaced by a pipe that carries one blank line (github.com) and closes.
async fn login_with_blank_answer(fixture: &Fixture) -> Output {
    let label = "login";
    let mut command = fixture.command(label, &["auth", "login", PROVIDER]);
    command.stdin(Stdio::piped());
    fixture.json(
        "login.stdin.json",
        &json!({"stdin": "piped", "bytes_utf8": "\n", "closed_after_write": true}),
    );
    let mut child = command.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"\n").await.unwrap();
    stdin.shutdown().await.unwrap();
    drop(stdin);
    let output = tokio::time::timeout(COMMAND_TIMEOUT, child.wait_with_output())
        .await
        .unwrap_or_else(|_| panic!("{label} exceeded {COMMAND_TIMEOUT:?}"))
        .unwrap_or_else(|error| panic!("cannot execute {label}: {error}"));
    fixture.capture(label, &output, &output.stderr);
    assert!(output.status.success(), "{label}: {}", text(&output.stderr));
    output
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// `METHOD path` of every provider/auth request, in arrival order (the public
/// catalogue fetch is not part of the flow).
fn routes(requests: &[Request]) -> Vec<String> {
    requests
        .iter()
        .filter(|request| request.url.path() != MODELS_DEV_PATH)
        .map(|request| format!("{} {}", request.method.as_str(), request.url.path()))
        .collect()
}

fn find<'a>(requests: &'a [Request], verb: &str, route: &str) -> Vec<&'a Request> {
    requests
        .iter()
        .filter(|request| request.method.as_str() == verb && request.url.path() == route)
        .collect()
}

fn is_policy(request: &Request) -> bool {
    let route = request.url.path();
    request.method.as_str() == "POST"
        && route.starts_with(&format!("{ROOT_A}/models/"))
        && route.ends_with("/policy")
}

fn header<'a>(request: &'a Request, name: &str) -> &'a str {
    request
        .headers
        .get(name)
        .unwrap_or_else(|| panic!("missing {name}"))
        .to_str()
        .unwrap()
}

/// Rows the login wrote; an absent `auth.json` holds none.
async fn rows(fixture: &Fixture) -> Vec<StoredCredential> {
    let file = fixture.home.path().join("data/auth.json");
    if !file.exists() {
        return Vec::new();
    }
    FileAuthStore::at(file)
        .snapshot()
        .await
        .unwrap()
        .rows(PROVIDER)
        .to_vec()
}

fn provider_row<'a>(value: &'a Value, provider: &str) -> &'a Value {
    value["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["provider"] == provider)
        .unwrap_or_else(|| panic!("{provider} row"))
}

fn assert_no_leak(outputs: &[&Output], secrets: &[&str]) {
    let all: String = outputs
        .iter()
        .map(|output| format!("{}{}", text(&output.stdout), text(&output.stderr)))
        .collect();
    for secret in secrets {
        assert!(
            !all.contains(secret),
            "fixture credential leaked into CLI output"
        );
    }
}

/// Device start, a first-poll success, the probe reporting `ENDPOINT_A`, every
/// policy POST answered 200, the live roster under `/tenant-a` and the three
/// inference routes under `/tenant-a`, each answering its own text.
async fn mount_copilot(server: &MockServer) {
    mount_session(server, TOKEN).await;
    let [messages, responses, chat] = [TURNS[0].2, TURNS[1].2, TURNS[2].2];
    reply(
        server,
        "POST",
        &messages_path(ROOT_A),
        messages_sse(messages),
    )
    .await;
    reply(
        server,
        "POST",
        &responses_path(ROOT_A),
        responses_ok(responses),
    )
    .await;
    reply(server, "POST", &chat_path(ROOT_A), chat_sse(chat)).await;
}

/// The login and discovery routes of one session whose grant is `token`: device
/// start, a first-poll success, the probe reporting `ENDPOINT_A`, every policy
/// POST answered 200 and the live roster under `/tenant-a`.
async fn mount_session(server: &MockServer, token: &str) {
    reply(server, "POST", DEVICE_PATH, device_reply(5.0, 900.0)).await;
    reply(server, "POST", TOKEN_PATH, token_reply(token)).await;
    reply(server, "GET", PROBE_PATH, probe_reply(ENDPOINT_A)).await;
    Mock::given(method("POST"))
        .and(path_regex(r"^/tenant-a/models/[^/]+/policy$"))
        .respond_with(json_reply(200, json!({})))
        .mount(server)
        .await;
    reply(server, "GET", MODELS_DEV_PATH, json_reply(200, json!({}))).await;
    reply(
        server,
        "GET",
        &format!("{ROOT_A}/models"),
        json_reply(
            200,
            json!({"data": [
                {"id": "claude-sonnet-4.5", "name": "Claude Sonnet 4.5", "capabilities": {"type": "chat"}},
                {"id": "gpt-5.5", "name": "GPT-5.5", "capabilities": {"type": "chat"}},
                {"id": "grok-code-fast-1", "name": "Grok Code Fast 1", "capabilities": {"type": "chat"}},
                {"id": "text-embedding-3-small", "capabilities": {"type": "embeddings"}},
            ]}),
        ),
    )
    .await;
}

/// K12 (M12): the debug binary logs in through the device flow (blank Enterprise
/// answer on piped stdin), reports `oauth`, and serves Claude/GPT-5/other ids on
/// `<probed root>/v1/messages`, `<root>/responses` and `<root>/chat/completions`
/// with the Copilot headers; inference that ignores the stored `api_endpoint`
/// (requests at bare `/v1/messages`, …) fails here.
#[tokio::test]
#[serial]
async fn k12_debug_binary_copilot_device_login_status_ask_routes_logout() {
    // No sleeps: device waits are scaled to zero and every command is bounded.
    tokio::time::timeout(Duration::from_secs(240), async {
        let server = MockServer::start().await;
        let fixture = fixture(&server, "k12-copilot", &copilot_config(TURNS[0].0));
        preflight(&fixture, &server).await;
        mount_copilot(&server).await;

        // ── login ────────────────────────────────────────────────────────────
        let login = login_with_blank_answer(&fixture).await;
        let login_out = text(&login.stdout);
        assert!(
            login_out
                .lines()
                .any(|line| line.trim() == format!("Logged in to {LOGIN_NAME}")),
            "{login_out}"
        );
        let login_err = text(&login.stderr);
        assert!(login_err.contains(PROMPT), "{login_err}");
        assert!(
            login_err.contains(&format!("Enter code: {USER_CODE}\n")),
            "{login_err}"
        );
        assert!(
            login_err.lines().any(|line| line == "Enabling models..."),
            "{login_err}"
        );
        let discovered = login_err
            .lines()
            .find_map(|line| {
                line.strip_prefix("Discovered ")
                    .and_then(|rest| rest.strip_suffix(&format!(" models for {LOGIN_NAME}.")))
            })
            .unwrap_or_else(|| panic!("no discovery line: {login_err}"));
        let discovered: usize = discovered.parse().expect("N is an integer");
        assert!(discovered > 0, "{login_err}");
        let login_requests = fixture.requests(&server, "login").await;
        let login_routes = routes(&login_requests);
        assert_eq!(
            login_routes[..3],
            [
                format!("POST {DEVICE_PATH}"),
                format!("POST {TOKEN_PATH}"),
                format!("GET {PROBE_PATH}"),
            ],
            "{login_routes:?}"
        );
        assert_eq!(find(&login_requests, "POST", DEVICE_PATH).len(), 1);
        assert_eq!(find(&login_requests, "POST", TOKEN_PATH).len(), 1);
        assert_eq!(find(&login_requests, "GET", PROBE_PATH).len(), 1);
        let device = find(&login_requests, "POST", DEVICE_PATH)[0];
        assert_eq!(text(&device.body), device_form(PUBLIC_CLIENT));
        let poll = find(&login_requests, "POST", TOKEN_PATH)[0];
        assert_eq!(text(&poll.body), poll_form(PUBLIC_CLIENT));
        let policies: Vec<&Request> = login_requests
            .iter()
            .filter(|request| is_policy(request))
            .collect();
        assert_eq!(policies.len(), SEED_IDS, "one policy POST per seed id");
        let bearer = format!("Bearer {TOKEN}");
        for policy in &policies {
            assert_eq!(header(policy, "authorization"), bearer);
        }
        assert!(
            login_requests
                .iter()
                .all(|request| request.url.path() != "/login/device"),
            "the verification URL is never fetched"
        );
        let stored = rows(&fixture).await;
        assert_eq!(stored.len(), 1, "one Copilot grant");
        assert!(
            matches!(stored[0].credential, Credential::OAuth(_)),
            "the device login stores an OAuth row"
        );
        std::fs::copy(
            fixture.home.path().join("data/auth.json"),
            fixture.evidence.join("auth-after-login.json"),
        )
        .unwrap();

        // ── status ───────────────────────────────────────────────────────────
        let status = fixture.run("status", &["auth", "status", "--json"]).await;
        let status_json: Value = serde_json::from_slice(&status.stdout).unwrap();
        assert_eq!(provider_row(&status_json, PROVIDER)["source"], "oauth");

        // ── three routed turns ───────────────────────────────────────────────
        let mut seen = fixture.requests(&server, "before-ask").await.len();
        let mut asks = Vec::new();
        let mut evidence = Vec::new();
        for (model, route, stub_text) in TURNS {
            write_config(&fixture, &copilot_config(model), &format!("config-{model}.toml"));
            let label = format!("ask-{model}");
            let ask = fixture
                .run(&label, &["ask", "hi", "--output-format", "json"])
                .await;
            let ask_json: Value = serde_json::from_slice(&ask.stdout)
                .unwrap_or_else(|error| panic!("{label} stdout is JSON: {error}"));
            assert_eq!(ask_json["response"], stub_text, "{label}");
            assert!(text(&ask.stdout).contains(stub_text), "{label}");
            let all = fixture.requests(&server, &label).await;
            let turn = &all[seen..];
            seen = all.len();
            let (expected, others) = match route {
                "messages" => (
                    messages_path(ROOT_A),
                    [responses_path(ROOT_A), chat_path(ROOT_A)],
                ),
                "responses" => (
                    responses_path(ROOT_A),
                    [messages_path(ROOT_A), chat_path(ROOT_A)],
                ),
                _ => (
                    chat_path(ROOT_A),
                    [messages_path(ROOT_A), responses_path(ROOT_A)],
                ),
            };
            let posts = find(turn, "POST", &expected);
            assert_eq!(posts.len(), 1, "{label}: one POST {expected}");
            for other in &others {
                assert!(find(turn, "POST", other).is_empty(), "{label}: no {other}");
            }
            // M12: the stored endpoint's root is honoured, never the canonical root.
            for bare in [messages_path(""), responses_path(""), chat_path("")] {
                assert!(
                    find(turn, "POST", &bare).is_empty(),
                    "{label}: inference ignored the stored api_endpoint ({bare})"
                );
            }
            for route in [DEVICE_PATH, TOKEN_PATH] {
                assert!(find(turn, "POST", route).is_empty(), "{label}: no {route}");
            }
            let request = posts[0];
            for (key, value) in [
                ("copilot-integration-id", "copilot-chat"),
                ("x-github-api-version", "2026-08-01"),
                ("authorization", bearer.as_str()),
            ] {
                assert_eq!(header(request, key), value, "{label}: {key}");
            }
            let body: Value = request.body_json().unwrap();
            assert_eq!(body["model"], model, "{label}");
            fixture.json(&format!("{label}-request-body.json"), &body);
            evidence.push(json!({
                "model": model, "route": expected, "response": ask_json["response"],
            }));
            asks.push(ask);
        }

        // ── logout ───────────────────────────────────────────────────────────
        let logout = fixture.run("logout", &["auth", "logout", PROVIDER]).await;
        assert_eq!(
            text(&logout.stdout).trim(),
            format!("Logged out of {PROVIDER}.")
        );
        assert!(rows(&fixture).await.is_empty());
        let list = fixture.run("list", &["auth", "list", "--json"]).await;
        let list_json: Value = serde_json::from_slice(&list.stdout).unwrap();
        assert_eq!(provider_row(&list_json, PROVIDER)["configured"], false);
        let finals = fixture.requests(&server, "final").await;
        assert_eq!(finals.len(), seen, "logout/list make no requests");

        let mut outputs = vec![&login, &status, &logout, &list];
        outputs.extend(asks.iter());
        assert_no_leak(&outputs, &[TOKEN, DEVICE_CODE]);
        // Written only after every runtime assertion above succeeded.
        fixture.json("passed.json", &json!({
            "test": "conformance_20_1d3_copilot::cli::k12_debug_binary_copilot_device_login_status_ask_routes_logout",
            "stdin": "\n",
            "login_routes_head": login_routes[..3].to_vec(),
            "policy_posts": policies.len(),
            "discovered_line": format!("Discovered {discovered} models for {LOGIN_NAME}."),
            "status_source": provider_row(&status_json, PROVIDER)["source"],
            "turns": evidence,
            "configured_after_logout": provider_row(&list_json, PROVIDER)["configured"],
        }));
    })
    .await
    .expect("K12 Copilot debug-binary flow exceeded its structural deadline");
}

/// K20: a Copilot 403 on inference is an organization or policy denial, never a
/// rejected credential: after it, `auth status --json` still reports the stored
/// grant as `oauth` and the row is kept enabled.
#[tokio::test]
#[serial]
async fn k20_status_reports_oauth_after_a_copilot_403() {
    // Distinct per test: the identity cache is keyed by the grant.
    const DENIED_TOKEN: &str = "gho_K20CopilotCliSecretToken0001";
    tokio::time::timeout(Duration::from_secs(240), async {
        let server = MockServer::start().await;
        let fixture = fixture(
            &server,
            "k20-forbidden-status",
            &copilot_config("grok-code-fast-1"),
        );
        preflight(&fixture, &server).await;
        mount_session(&server, DENIED_TOKEN).await;
        reply(
            &server,
            "POST",
            &chat_path(ROOT_A),
            json_reply(403, json!({"error": {"message": "denied by policy"}})),
        )
        .await;
        let login = login_with_blank_answer(&fixture).await;
        assert_eq!(rows(&fixture).await.len(), 1, "one Copilot grant");

        // ── a denied turn ────────────────────────────────────────────────────
        let before = fixture.requests(&server, "before-ask").await.len();
        let label = "ask-403";
        let ask = tokio::time::timeout(
            COMMAND_TIMEOUT,
            fixture
                .command(label, &["ask", "hi", "--output-format", "json"])
                .output(),
        )
        .await
        .unwrap_or_else(|_| panic!("{label} exceeded {COMMAND_TIMEOUT:?}"))
        .unwrap_or_else(|error| panic!("cannot execute {label}: {error}"));
        fixture.capture(label, &ask, &ask.stderr);
        let ask_out = format!("{}{}", text(&ask.stdout), text(&ask.stderr));
        assert!(
            ask_out.contains("GitHub Copilot access denied (HTTP 403)"),
            "{ask_out}"
        );
        let after = fixture.requests(&server, label).await;
        let denied = find(&after[before..], "POST", &chat_path(ROOT_A));
        assert!(!denied.is_empty(), "the turn reached the Chat route");
        let bearer = format!("Bearer {DENIED_TOKEN}");
        for request in &denied {
            assert_eq!(header(request, "authorization"), bearer);
        }

        // ── status after the 403 ─────────────────────────────────────────────
        let status = fixture.run("status", &["auth", "status", "--json"]).await;
        let status_json: Value = serde_json::from_slice(&status.stdout).unwrap();
        assert_eq!(provider_row(&status_json, PROVIDER)["source"], "oauth");
        let stored = rows(&fixture).await;
        assert_eq!(stored.len(), 1, "the grant is kept");
        assert!(
            matches!(stored[0].credential, Credential::OAuth(_)),
            "still the device login's OAuth row"
        );
        assert_eq!(
            stored[0].disabled_cause, None,
            "a 403 never disables the row"
        );
        assert_no_leak(&[&login, &ask, &status], &[DENIED_TOKEN, DEVICE_CODE]);
        fixture.json(
            "passed.json",
            &json!({
                "test": "conformance_20_1d3_copilot::cli::k20_status_reports_oauth_after_a_copilot_403",
                "denied_posts": denied.len(),
                "status_source": provider_row(&status_json, PROVIDER)["source"],
            }),
        );
    })
    .await
    .expect("K20 Copilot 403 status flow exceeded its structural deadline");
}
