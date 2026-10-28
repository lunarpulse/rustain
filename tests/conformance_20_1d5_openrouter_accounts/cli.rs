//! The debug binary, each step a fresh process in an isolated home. Build the
//! binary with `test-instrumentation` first; `RUSTAIN_TEST_CODEX_EVIDENCE_DIR`
//! retains the command/output/request evidence (the 20-1d fixture).
//! - K12(b) (M12): two Codex workspace accounts: browser login ×2, `ask` serves
//!   the latest login, `auth use --id` switches, `auth logout --id` removes
//!   exactly one account and the other serves again.
//! - K12(a): two browser-minted OpenRouter keys, a re-mint of the first, `ask`
//!   on both transports, `auth use`/`auth logout --id`, and the environment key
//!   once the last row is gone.
//! - The ignored PTY evidence test pastes a key into masked entry under `script`.

use crate::conformance_20_1d_codex_subscription::auth::jwt;
use crate::conformance_20_1d_codex_subscription::cli::{Fixture, login};
use serde_json::{Value, json};
use serial_test::serial;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{method, path},
};

const RESPONSE_PATH: &str = "/backend-api/codex/responses";
const DISPLAY: &str = "ChatGPT Plus/Pro (Codex Subscription)";
const LABEL: &str = "operator@example.test (plus)";
const TEXT: &str = "two accounts fixture";

fn access(account: &str) -> String {
    jwt(json!({
        "https://api.openai.com/auth": {"chatgpt_account_id": account, "chatgpt_plan_type": "plus"},
        "https://api.openai.com/profile": {"email": "operator@example.test"},
    }))
}

/// The stub for one account's login, discovery and inference. Earlier mocks and
/// the request log are reset, so the token endpoint answers this account only.
async fn serve(server: &MockServer, account: &str) {
    server.reset().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": access(account),
            "refresh_token": format!("REFRESH-{account}"),
            "expires_in": 3600,
        })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/backend-api/codex/models"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"models": [{"slug": "gpt-5.6-luna"}]})),
        )
        .mount(server)
        .await;
    let sse = format!(
        "data: {}\n\ndata: {}\n\n",
        json!({"type":"response.output_text.delta","delta":TEXT}),
        json!({"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":1,"output_tokens":2}}})
    );
    Mock::given(method("POST"))
        .and(path(RESPONSE_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream"))
        .mount(server)
        .await;
}

/// The login helper's evidence is per-label; keep each login's copy.
fn keep_login_evidence(fixture: &Fixture, suffix: &str) {
    for file in [
        "login.stdout",
        "login.stderr",
        "login.exit.json",
        "login.command.json",
    ] {
        let from = fixture.evidence.join(file);
        let to = fixture
            .evidence
            .join(file.replacen("login", &format!("login-{suffix}"), 1));
        std::fs::rename(from, to).unwrap();
    }
}

/// One `ask` process; the `chatgpt-account-id` its single Responses request sent.
async fn ask_account(fixture: &Fixture, server: &MockServer, label: &str) -> String {
    let before = server.received_requests().await.unwrap().len();
    let ask = fixture
        .run(label, &["ask", "hi", "--output-format", "json"])
        .await;
    let answer: Value = serde_json::from_slice(&ask.stdout).unwrap();
    assert_eq!(answer["response"], TEXT);
    let requests = fixture.requests(server, label).await;
    let posts: Vec<_> = requests[before..]
        .iter()
        .filter(|request| request.method == "POST" && request.url.path() == RESPONSE_PATH)
        .collect();
    assert_eq!(posts.len(), 1, "{label}: one Codex request");
    posts[0].headers["chatgpt-account-id"]
        .to_str()
        .unwrap()
        .to_owned()
}

async fn accounts(fixture: &Fixture, label: &str) -> Value {
    let output = fixture
        .run(label, &["auth", "accounts", "openai-codex", "--json"])
        .await;
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["schema_version"], "1.0");
    assert_eq!(value["providers"].as_array().unwrap().len(), 1);
    value["providers"][0].clone()
}

fn account(id: u64, selected: bool, served: bool) -> Value {
    json!({"id": id, "kind": "oauth", "origin": "login", "label": LABEL,
        "selected": selected, "served": served, "disabled_cause": null})
}

#[tokio::test]
#[serial]
async fn k12b_debug_binary_two_codex_accounts_switch_and_log_out_by_id() {
    tokio::time::timeout(Duration::from_secs(180), async {
        let server = MockServer::start().await;
        let fixture = Fixture::new(&server);
        let auth = fixture.home.path().join("data/auth.json");

        serve(&server, "acct-A").await;
        let (_, first) = login(&fixture, &server).await;
        keep_login_evidence(&fixture, "a");
        assert!(!first.contains("Active "), "one account prints no account lines: {first}");
        serve(&server, "acct-B").await;
        let (_, second) = login(&fixture, &server).await;
        keep_login_evidence(&fixture, "b");
        assert!(
            second.contains(&format!("Active {DISPLAY} account: {LABEL} (#2).\n")),
            "{second}"
        );
        assert!(
            second.contains("Switch with `rustain auth use openai-codex --id <N>`; remove one with `rustain auth logout openai-codex --id <N>`.\n"),
            "{second}"
        );

        let listed = accounts(&fixture, "accounts-two").await;
        assert_eq!(listed["provider"], "openai-codex");
        assert_eq!(listed["display_name"], DISPLAY);
        assert_eq!(listed["served"], json!({"source": "oauth", "id": 2}));
        assert_eq!(
            listed["accounts"],
            json!([account(1, false, false), account(2, true, true)])
        );
        assert_eq!(ask_account(&fixture, &server, "ask-latest").await, "acct-B");

        let used = fixture
            .run("use-a", &["auth", "use", "openai-codex", "--id", "1"])
            .await;
        assert_eq!(
            String::from_utf8_lossy(&used.stdout),
            format!("Active {DISPLAY} account: {LABEL} (#1).\n")
        );
        let used_err = String::from_utf8_lossy(&used.stderr);
        assert!(!used_err.contains("Note:"), "{used_err}");
        assert!(
            used_err.contains(&format!("Discovered 1 models for {DISPLAY}.")),
            "{used_err}"
        );
        assert_eq!(ask_account(&fixture, &server, "ask-used").await, "acct-A");

        let logout = fixture
            .run("logout-a", &["auth", "logout", "openai-codex", "--id", "1"])
            .await;
        assert_eq!(
            String::from_utf8_lossy(&logout.stdout),
            format!("Logged out of {DISPLAY} account {LABEL}.\n")
        );
        assert!(
            String::from_utf8_lossy(&logout.stderr)
                .contains(&format!("{DISPLAY} now uses {LABEL} (#2).\n")),
            "{}",
            String::from_utf8_lossy(&logout.stderr)
        );
        assert_eq!(ask_account(&fixture, &server, "ask-after-logout").await, "acct-B");
        let listed = accounts(&fixture, "accounts-after-logout").await;
        assert_eq!(listed["served"], json!({"source": "oauth", "id": 2}));
        assert_eq!(listed["accounts"], json!([account(2, false, true)]));
        std::fs::copy(&auth, fixture.evidence.join("auth-after-logout.json")).unwrap();

        // A stale id exits non-zero with the not-found text and writes nothing.
        let bytes = std::fs::read(&auth).unwrap();
        let stale = fixture
            .command("logout-stale", &["auth", "logout", "openai-codex", "--id", "1", "--json"])
            .output()
            .await
            .unwrap();
        fixture.capture("logout-stale", &stale, &stale.stderr);
        assert!(!stale.status.success());
        assert_eq!(
            serde_json::from_slice::<Value>(&stale.stdout).unwrap(),
            json!({"provider": "openai-codex", "status": "not_found", "id": 1})
        );
        assert_eq!(
            String::from_utf8_lossy(&stale.stderr),
            "No stored credential #1 for openai-codex. Run `rustain auth accounts openai-codex`.\n"
        );
        assert_eq!(std::fs::read(&auth).unwrap(), bytes);

        let mut output = String::new();
        for entry in std::fs::read_dir(&fixture.evidence).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|ext| ext == "stdout" || ext == "stderr") {
                output.push_str(&std::fs::read_to_string(path).unwrap());
            }
        }
        for secret in [access("acct-A"), access("acct-B"), "REFRESH-acct-A".into(), "REFRESH-acct-B".into()] {
            assert!(!output.contains(&secret), "a fixture credential reached CLI output");
        }
        // Written only after every assertion above held.
        fixture.json("passed.json", &json!({
            "test": "conformance_20_1d5_openrouter_accounts::cli::k12b_debug_binary_two_codex_accounts_switch_and_log_out_by_id",
            "logins": 2, "served_after_logins": "acct-B", "served_after_use": "acct-A",
            "served_after_logout": "acct-B", "stale_logout_exit": stale.status.code(),
            "fixture_secrets_only": true,
        }));
    })
    .await
    .expect("the two-account flow exceeded its structural deadline");
}

// ── K12(a): OpenRouter browser minting end to end ───────────────────────────

const OR_DISPLAY: &str = "OpenRouter";
const OR_TEXT: &str = "openrouter fixture";
const OR_RESPONSES: &str = "/api/v1/responses";
const OR_CHAT: &str = "/api/v1/chat/completions";
const OR_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const KEY_A: &str = "sk-or-v1-fixture-key-a";
const KEY_B: &str = "sk-or-v1-fixture-key-b";
const KEY_ENV: &str = "sk-or-v1-fixture-key-env";

/// Every K12(a) step reads this entry; the fixture's Codex entry is replaced
/// before any process runs.
fn openrouter_config(fixture: &Fixture, discover: bool) {
    let config = format!(
        "model = \"openai/gpt-5.5\"\n\n[provider.openrouter]\nprovider_id = \"openrouter\"\nkind = \"openrouter\"\nmodel_id = \"openai/gpt-5.5\"\napi_key_env = \"OPENROUTER_API_KEY\"\nenabled = true\ndiscover_models = {discover}\n"
    );
    std::fs::write(fixture.home.path().join("config/config.toml"), &config).unwrap();
    std::fs::write(fixture.evidence.join("config.toml"), &config).unwrap();
}

/// The OpenRouter stub; the exchange mints `key`. Earlier mocks are reset; the
/// request log is kept so later assertions can slice it.
async fn serve_openrouter(server: &MockServer, key: &str) {
    server.reset().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/auth/keys"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"key": key})))
        .mount(server)
        .await;
    for (route, body) in [
        ("/api/v1/auth/key", json!({"data": {"label": "fixture"}})),
        (
            "/api/v1/models",
            json!({"data": [{"id": "openai/gpt-5.5", "supported_parameters": ["tools"]}]}),
        ),
        ("/api.json", json!({})),
    ] {
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }
    let responses = format!(
        "data: {}\n\ndata: {}\n\n",
        json!({"type":"response.output_text.delta","delta":OR_TEXT}),
        json!({"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":1,"output_tokens":2}}})
    );
    Mock::given(method("POST"))
        .and(path(OR_RESPONSES))
        .respond_with(ResponseTemplate::new(200).set_body_raw(responses, "text/event-stream"))
        .mount(server)
        .await;
    let chat = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant","content":OR_TEXT},"finish_reason":null}]}),
        json!({"object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})
    );
    Mock::given(method("POST"))
        .and(path(OR_CHAT))
        .respond_with(ResponseTemplate::new(200).set_body_raw(chat, "text/event-stream"))
        .mount(server)
        .await;
}

/// One `auth login openrouter --method oauth` process: the printed authorize URL
/// is proven to be the stub's before the test GETs its `callback_url` (whatever
/// port the binary bound) with a code. Returns stdout and the stderr transcript.
async fn mint(fixture: &Fixture, server: &MockServer, label: &str) -> (String, String) {
    let mut child = fixture
        .command(label, &["auth", "login", "openrouter", "--method", "oauth"])
        .spawn()
        .unwrap();
    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let mut transcript = String::new();
    let authorize = tokio::time::timeout(OR_COMMAND_TIMEOUT, async {
        let mut authorize = None;
        loop {
            let mut line = String::new();
            assert_ne!(
                stderr.read_line(&mut line).await.unwrap(),
                0,
                "{label} exited before its URLs: {transcript}"
            );
            transcript.push_str(&line);
            assert!(transcript.len() <= 65_536, "unbounded login stderr");
            if line.trim().starts_with("http") {
                authorize = Some(url::Url::parse(line.trim()).unwrap());
            }
            if line.trim().starts_with("Local browser launch: ") {
                break authorize.expect("the authorize URL precedes the launch URL");
            }
        }
    })
    .await
    .expect("login did not print its URLs within the deadline");
    // The safety gate: no HTTP before the binary proved it rebased to the stub.
    assert_eq!(
        authorize.origin(),
        url::Url::parse(&server.uri()).unwrap().origin(),
        "the binary MUST be built with test-instrumentation: {authorize}"
    );
    assert_eq!(authorize.path(), "/auth");
    let query: std::collections::BTreeMap<_, _> = authorize.query_pairs().into_owned().collect();
    assert_eq!(
        query.keys().map(String::as_str).collect::<Vec<_>>(),
        ["callback_url", "code_challenge", "code_challenge_method"]
    );
    let mut callback = url::Url::parse(&query["callback_url"]).unwrap();
    assert_eq!(callback.host_str(), Some("localhost"));
    callback
        .query_pairs_mut()
        .append_pair("code", "CLI-MINT-CODE");
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    assert!(
        client
            .get(callback)
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    let (output, read) = tokio::time::timeout(OR_COMMAND_TIMEOUT, async {
        tokio::join!(
            child.wait_with_output(),
            stderr.read_to_string(&mut transcript)
        )
    })
    .await
    .expect("login did not exit within its deadline");
    read.unwrap();
    let output = output.unwrap();
    fixture.capture(label, &output, transcript.as_bytes());
    assert!(output.status.success(), "{label}: {transcript}");
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        transcript,
    )
}

/// One `ask` process; the single inference request it sent on `route`.
async fn ask_request(fixture: &Fixture, server: &MockServer, label: &str, route: &str) -> Request {
    let before = server.received_requests().await.unwrap().len();
    let ask = fixture
        .run(label, &["ask", "hi", "--output-format", "json"])
        .await;
    let answer: Value = serde_json::from_slice(&ask.stdout).unwrap();
    assert_eq!(answer["response"], OR_TEXT, "{label}");
    let requests = fixture.requests(server, label).await;
    let posts: Vec<_> = requests[before..]
        .iter()
        .filter(|request| {
            request.method == "POST"
                && (request.url.path() == OR_RESPONSES || request.url.path() == OR_CHAT)
        })
        .cloned()
        .collect();
    assert_eq!(posts.len(), 1, "{label}: one inference request");
    assert_eq!(posts[0].url.path(), route, "{label}");
    posts[0].clone()
}

fn bearer(request: &Request) -> String {
    request.headers["authorization"]
        .to_str()
        .unwrap()
        .to_owned()
}

async fn or_accounts(fixture: &Fixture, label: &str) -> Value {
    let output = fixture
        .run(label, &["auth", "accounts", "openrouter", "--json"])
        .await;
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["schema_version"], "1.0");
    value["providers"][0].clone()
}

fn key_row(id: u64, selected: bool, served: bool) -> Value {
    json!({"id": id, "kind": "api_key", "origin": "login", "label": format!("API key #{id}"),
        "selected": selected, "served": served, "disabled_cause": null})
}

#[tokio::test]
#[serial]
async fn k12a_debug_binary_minted_openrouter_keys_serve_switch_and_log_out() {
    tokio::time::timeout(Duration::from_secs(240), async {
        let server = MockServer::start().await;
        let mut fixture = Fixture::new(&server);
        openrouter_config(&fixture, true);
        let auth = fixture.home.path().join("data/auth.json");

        // Mint ×2: two key rows, the second active.
        serve_openrouter(&server, KEY_A).await;
        let (stdout, stderr) = mint(&fixture, &server, "mint-a").await;
        assert_eq!(stdout, "✓ Logged in to OpenRouter.\n");
        assert!(stderr.contains("Discovered 1 models for OpenRouter."), "{stderr}");
        assert!(!stderr.contains("Active "), "one account prints no account lines: {stderr}");
        serve_openrouter(&server, KEY_B).await;
        let (stdout, stderr) = mint(&fixture, &server, "mint-b").await;
        assert_eq!(stdout, "✓ Logged in to OpenRouter.\n");
        assert!(
            stderr.contains(&format!("Active {OR_DISPLAY} account: API key #2.\n")),
            "{stderr}"
        );
        let listed = or_accounts(&fixture, "accounts-two").await;
        assert_eq!(listed["served"]["id"], 2);
        assert_eq!(listed["accounts"], json!([key_row(1, false, false), key_row(2, true, true)]));

        // Re-mint A: the exchange answers A's bytes again; equality upsert keeps
        // two rows and A's id, and A becomes active.
        serve_openrouter(&server, KEY_A).await;
        let (stdout, _) = mint(&fixture, &server, "mint-a-again").await;
        assert_eq!(stdout, "✓ Logged in to OpenRouter.\n");
        let listed = or_accounts(&fixture, "accounts-reminted").await;
        assert_eq!(listed["served"]["id"], 1);
        assert_eq!(listed["accounts"], json!([key_row(1, true, true), key_row(2, false, false)]));

        // `ask` serves the active key on Responses with rustain's identity.
        let request = ask_request(&fixture, &server, "ask-a", OR_RESPONSES).await;
        assert_eq!(bearer(&request), format!("Bearer {KEY_A}"));
        for (name, value) in [
            ("user-agent", format!("rustain/{}", env!("CARGO_PKG_VERSION"))),
            ("http-referer", "https://github.com/lunarpulse/rustain".to_owned()),
            ("x-openrouter-title", "rustain".to_owned()),
            ("x-openrouter-categories", "cli-agent".to_owned()),
            ("x-openrouter-cache", "true".to_owned()),
            ("x-openrouter-cache-ttl", "3600".to_owned()),
        ] {
            assert_eq!(request.headers[name].to_str().unwrap(), value, "{name}");
        }
        fixture
            .run("use-b", &["auth", "use", "openrouter", "--id", "2"])
            .await;
        let request = ask_request(&fixture, &server, "ask-b", OR_RESPONSES).await;
        assert_eq!(bearer(&request), format!("Bearer {KEY_B}"));

        // Per-row logout: B goes, A serves on both transports.
        fixture
            .run("logout-b", &["auth", "logout", "openrouter", "--id", "2"])
            .await;
        let request = ask_request(&fixture, &server, "ask-after-logout", OR_RESPONSES).await;
        assert_eq!(bearer(&request), format!("Bearer {KEY_A}"));
        fixture.env.insert("RUSTAIN_OPENROUTER_RESPONSES", "0".into());
        let request = ask_request(&fixture, &server, "ask-chat", OR_CHAT).await;
        assert_eq!(bearer(&request), format!("Bearer {KEY_A}"));
        fixture.env.remove("RUSTAIN_OPENROUTER_RESPONSES");

        // The last row goes: the environment key serves.
        fixture.env.insert("OPENROUTER_API_KEY", KEY_ENV.into());
        fixture
            .run("logout-a", &["auth", "logout", "openrouter", "--id", "1"])
            .await;
        let listed = or_accounts(&fixture, "accounts-env").await;
        assert_eq!(listed["served"], json!({"source": "env", "var": "OPENROUTER_API_KEY"}));
        assert_eq!(listed["accounts"], json!([]));
        let request = ask_request(&fixture, &server, "ask-env", OR_RESPONSES).await;
        assert_eq!(bearer(&request), format!("Bearer {KEY_ENV}"));
        std::fs::copy(&auth, fixture.evidence.join("auth-after-logout.json")).unwrap();

        let mut output = String::new();
        for entry in std::fs::read_dir(&fixture.evidence).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|ext| ext == "stdout" || ext == "stderr") {
                output.push_str(&std::fs::read_to_string(path).unwrap());
            }
        }
        for secret in [KEY_A, KEY_B, KEY_ENV] {
            assert!(!output.contains(secret), "a fixture key reached CLI output");
        }
        // Written only after every assertion above held.
        fixture.json("passed.json", &json!({
            "test": "conformance_20_1d5_openrouter_accounts::cli::k12a_debug_binary_minted_openrouter_keys_serve_switch_and_log_out",
            "mints": 3, "rows_after_remint": 2, "served_after_remint": 1,
            "served_after_use": 2, "served_after_logout": 1, "chat_switch_served": 1,
            "served_after_last_logout": "env:OPENROUTER_API_KEY", "fixture_keys_only": true,
        }));
    })
    .await
    .expect("the OpenRouter flow exceeded its structural deadline");
}

/// Developer evidence (not run in CI): masked API-key entry needs a TTY, so the
/// key is pasted through `script` (util-linux). The transcript goes to
/// `/tmp/20-1d5b-gate/paste-flow.txt`.
#[tokio::test]
#[serial]
#[ignore = "developer PTY evidence; needs util-linux `script`"]
async fn pty_paste_flow_evidence() {
    const PASTED: &str = "sk-or-v1-fixture-pasted-key";
    if std::process::Command::new("script")
        .arg("--version")
        .output()
        .is_err()
    {
        println!("skipped: util-linux `script` is not installed");
        return;
    }
    let server = MockServer::start().await;
    let fixture = Fixture::new(&server);
    // No live discovery, so `/models` is never requested by anything.
    openrouter_config(&fixture, false);
    serve_openrouter(&server, "unused").await;
    let before = or_accounts(&fixture, "pty-accounts-before").await;
    assert_eq!(before["accounts"], json!([]));

    let command = format!(
        "{} auth login openrouter --method api-key",
        fixture.binary.display()
    );
    let mut child = std::process::Command::new("script")
        .args(["-qec", &command, "/dev/null"])
        .env_clear()
        .envs(&fixture.env)
        .current_dir(fixture.home.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // Paste only once the masked prompt is on screen: typeahead written before
    // the prompt turns echo off is echoed by the pty itself, as in a real
    // terminal, and would not show the masked-entry behaviour.
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let output = tokio::task::spawn_blocking(move || {
        use std::io::{Read, Write};
        let mut seen = Vec::new();
        let mut buffer = [0_u8; 256];
        while !String::from_utf8_lossy(&seen).contains("API key: ") {
            let read = stdout.read(&mut buffer).unwrap();
            assert_ne!(read, 0, "login ended before its prompt");
            seen.extend_from_slice(&buffer[..read]);
        }
        stdin.write_all(format!("{PASTED}\n").as_bytes()).unwrap();
        drop(stdin);
        stdout.read_to_end(&mut seen).unwrap();
        let mut output = child.wait_with_output().unwrap();
        output.stdout = seen;
        output
    })
    .await
    .unwrap();
    let transcript = format!(
        "$ printf '<key>\\n' | script -qec \"{command}\" /dev/null\n--- stdout ---\n{}\n--- stderr ---\n{}\n--- exit: {:?} ---\n",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
        output.status.code()
    );
    std::fs::create_dir_all("/tmp/20-1d5b-gate").unwrap();
    std::fs::write("/tmp/20-1d5b-gate/paste-flow.txt", &transcript).unwrap();
    assert!(output.status.success(), "{transcript}");
    assert!(
        !transcript.contains(PASTED),
        "the pasted key is echoed: {transcript}"
    );

    let requests = fixture.requests(&server, "pty-requests").await;
    let checks: Vec<_> = requests
        .iter()
        .filter(|request| request.url.path() == "/api/v1/auth/key")
        .collect();
    assert_eq!(checks.len(), 1);
    assert_eq!(bearer(checks[0]), format!("Bearer {PASTED}"));
    assert!(
        !requests
            .iter()
            .any(|request| request.url.path() == "/api/v1/models")
    );
    assert!(
        !requests
            .iter()
            .any(|request| request.url.path() == "/api/v1/auth/keys")
    );

    let after = or_accounts(&fixture, "pty-accounts-after").await;
    assert_eq!(after["accounts"], json!([key_row(1, true, true)]));
    // A second, independent row; then per-row logout removes only the pasted one.
    serve_openrouter(&server, "sk-or-v1-fixture-minted-key").await;
    mint(&fixture, &server, "pty-mint").await;
    fixture
        .run("pty-logout", &["auth", "logout", "openrouter", "--id", "1"])
        .await;
    let left = or_accounts(&fixture, "pty-accounts-left").await;
    assert_eq!(left["accounts"], json!([key_row(2, true, true)]));
    std::fs::write(
        "/tmp/20-1d5b-gate/paste-flow.txt",
        format!("{transcript}\naccounts before: {before}\naccounts after paste: {after}\naccounts after logout --id 1: {left}\n"),
    )
    .unwrap();
}
