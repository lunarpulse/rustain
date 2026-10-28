//! K6: the real debug binary (20-1d4 K12 setup) — `auth login` discovers
//! through the Antigravity fallback and prints the discovered line, `ask` sends
//! a collapsed logical id's no-effort wire id and thinking, `auth logout` ends
//! the login.
//!
//! Needs the binary built with `test-instrumentation` first
//! (`RUSTAIN_TEST_GEMINI_CLI_BINARY` or the cargo-bin fallback);
//! `RUSTAIN_TEST_GEMINI_CLI_EVIDENCE_DIR` retains the evidence.
use super::common::*;
use crate::conformance_20_1d4_gemini_cli::auth::{Google, ONBOARD, OPERATION, USERINFO, mount};
use crate::conformance_20_1d4_gemini_cli::cli::{Fixture, cli_login};
use rustain::adapters::auth_store::FileAuthStore;
use rustain::domain::ports::AuthStorePort;
use serde_json::{Value, json};
use serial_test::serial;
use std::time::Duration;
use wiremock::{
    Mock, ResponseTemplate,
    matchers::{method, path},
};

const LOGICAL: &str = "gemini-3.1-pro";
const DISCOVERED: &str = "Discovered 8 models for Google Cloud Code Assist (Gemini CLI).";

#[tokio::test]
#[serial]
async fn k6_debug_binary_login_discovers_ask_routes_the_logical_id_logout() {
    // No sleeps: URL publication gates the callback; each subprocess is bounded.
    tokio::time::timeout(Duration::from_secs(120), async {
        let server = wiremock::MockServer::start().await;
        let fixture = Fixture::new(&server);
        let config = format!(
            "model = \"{LOGICAL}\"\n\n[provider.gemini]\nkind = \"google-gemini-cli\"\nprovider_id = \"google-gemini-cli\"\nmodel_id = \"{LOGICAL}\"\napi_key_env = \"\"\nenabled = true\ndiscover_models = true\n"
        );
        fixture.config(&config);
        mount(&server, Google {
            token: ok(token_body(ACCESS, Some(REFRESH))),
            userinfo: ok(json!({"email": EMAIL, "id": "1"})),
            load: ok(json!({"allowedTiers": [{"id": "free-tier", "isDefault": true}]})),
            onboard: ok(json!({"name": "operations/fixture-op", "done": false})),
            polls: vec![ok(json!({"done": true, "response": {"cloudaicompanionProject": {"id": PROJECT}}}))],
        }).await;
        Mock::given(method("GET")).and(path("/api.json"))
            .respond_with(ok(json!({}))).mount(&server).await;
        // Antigravity rejects this account on both hosts: the quota answers.
        Mock::given(method("POST")).and(path(FETCH))
            .respond_with(ResponseTemplate::new(403)).mount(&server).await;
        Mock::given(method("POST")).and(path(QUOTA))
            .respond_with(ok(quota_body(&["gemini-3.1-pro-low", "gemini-pro-agent", "gemini-3.1-pro-preview"])))
            .mount(&server).await;
        mount_stream(&server).await;

        let (login, transcript, _) = cli_login(&fixture, &server).await;
        let lines: Vec<&str> = transcript.lines().collect();
        assert!(lines.contains(&DISCOVERED), "{transcript}");
        let requests = fixture.requests(&server, "login").await;
        let paths: Vec<_> = requests.iter().map(|r| r.url.path()).filter(|p| *p != "/api.json").collect();
        assert_eq!(paths, [TOKEN, USERINFO, LOAD, ONBOARD, OPERATION, FETCH, FETCH, QUOTA]);
        for fetch in hits(&requests, FETCH) {
            assert_eq!(String::from_utf8_lossy(&fetch.body), "{}");
            assert_eq!(header(fetch, "authorization"), format!("Bearer {ACCESS}"));
            assert_eq!(header(fetch, "user-agent"), ANTIGRAVITY_UA);
        }
        let quota = hits(&requests, QUOTA)[0];
        assert_eq!(String::from_utf8_lossy(&quota.body), format!(r#"{{"project":"{PROJECT}"}}"#));
        assert_eq!(header(quota, "user-agent"), gemini_cli_agent("gemini-3.1-pro-preview"));
        assert_no_long_lived_secrets(&requests);

        let ask = fixture.run("ask", &["ask", "hi", "--output-format", "json"]).await;
        let ask_json: Value = serde_json::from_slice(&ask.stdout).unwrap();
        assert_eq!(ask_json["response"], "ok");
        let requests = fixture.requests(&server, "ask").await;
        let posts = hits(&requests, STREAM);
        assert_eq!(posts.len(), 1);
        let raw = String::from_utf8_lossy(&posts[0].body).into_owned();
        assert!(
            raw.starts_with(&format!(r#"{{"project":"{PROJECT}","model":"gemini-3.1-pro-low","request":{{"#)),
            "{raw}"
        );
        let body: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            body["request"]["generationConfig"]["thinkingConfig"],
            json!({"includeThoughts": false, "thinkingLevel": "MINIMAL"})
        );
        assert_eq!(header(posts[0], "user-agent"), gemini_cli_agent(LOGICAL));
        // The login's roster is cached for this account: `ask` discovers nothing.
        assert_eq!(hits(&requests, QUOTA).len(), 1);
        assert_eq!(hits(&requests, FETCH).len(), 2);
        fixture.json("gemini-request.json", &body);

        let logout = fixture.run("logout", &["auth", "logout", PROVIDER]).await;
        assert_eq!(String::from_utf8_lossy(&logout.stdout).trim(), "Logged out of google-gemini-cli.");
        let auth_json = fixture.home.path().join("data/auth.json");
        assert!(FileAuthStore::at(auth_json).snapshot().await.unwrap().rows(PROVIDER).is_empty());
        let list = fixture.run("list", &["auth", "list", "--json"]).await;
        let list_json: Value = serde_json::from_slice(&list.stdout).unwrap();
        let gemini = list_json["providers"].as_array().unwrap().iter()
            .find(|row| row["provider"] == PROVIDER).unwrap();
        assert_eq!(gemini["configured"], false);

        let all_output = format!("{transcript}{}{}{}{}{}",
            String::from_utf8_lossy(&login.stdout), String::from_utf8_lossy(&ask.stdout),
            String::from_utf8_lossy(&ask.stderr), String::from_utf8_lossy(&logout.stdout),
            String::from_utf8_lossy(&list.stdout));
        for secret in [ACCESS, REFRESH, CLIENT_SECRET] {
            assert!(!all_output.contains(secret), "fixture credential leaked into CLI output");
        }
        // Written only after every runtime assertion above succeeded.
        fixture.json("passed.json", &json!({
            "test": "conformance_20_1d4b_gemini_cli_discovery::cli::k6_debug_binary_login_discovers_ask_routes_the_logical_id_logout",
            "discovered_line": DISCOVERED,
            "login_discovery_requests": [FETCH, FETCH, QUOTA],
            "antigravity_statuses": [403, 403],
            "quota_body": format!(r#"{{"project":"{PROJECT}"}}"#),
            "ask_exit": ask.status.code(), "ask_model": LOGICAL,
            "wire_model": "gemini-3.1-pro-low",
            "thinking_config": {"includeThoughts": false, "thinkingLevel": "MINIMAL"},
            "logout_exit": logout.status.code(), "configured_after_logout": gemini["configured"],
            "fixture_secrets_only": true,
        }));
    }).await.expect("CLI fixture exceeded its structural 120-second deadline");
}
