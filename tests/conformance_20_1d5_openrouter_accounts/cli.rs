//! K12(b) (M12): the debug binary with two Codex workspace accounts, each step
//! a fresh process in an isolated home: browser login ×2, `ask` serves the
//! latest login, `auth use --id` switches, `auth logout --id` removes exactly
//! one account and the other serves again. Build the binary with
//! `test-instrumentation` first; `RUSTAIN_TEST_CODEX_EVIDENCE_DIR` retains the
//! command/output/request evidence (the 20-1d fixture).

use crate::conformance_20_1d_codex_subscription::auth::jwt;
use crate::conformance_20_1d_codex_subscription::cli::{Fixture, login};
use serde_json::{Value, json};
use serial_test::serial;
use std::time::Duration;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
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
