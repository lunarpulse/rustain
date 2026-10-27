//! K08: the real debug binary (built with `test-instrumentation`). `login`
//! asserts the printed authorize origin is the loopback stub before any HTTP.
use super::common::*;
use crate::conformance_20_1d_codex_subscription::auth::jwt;
use crate::conformance_20_1d_codex_subscription::cli::{Fixture, login};
use serde_json::{Value, json};
use serial_test::serial;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const MARK: &str = "k08";

#[tokio::test]
#[serial]
async fn k08_debug_binary_login_discovers_then_two_turn_ask_replays_reasoning() {
    tokio::time::timeout(std::time::Duration::from_secs(120), async {
        let server = MockServer::start().await;
        let fixture = Fixture::new(&server);
        let access = jwt(json!({
            "https://api.openai.com/auth": {"chatgpt_account_id":"k08-account", "chatgpt_plan_type":"Plus "},
            "https://api.openai.com/profile": {"email":" Operator@EXAMPLE.test "},
        }));
        Mock::given(method("POST")).and(path("/oauth/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token":access,"refresh_token":"K08-REFRESH","expires_in":3600})))
            .mount(&server).await;
        Mock::given(method("GET")).and(path("/api.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({}))).mount(&server).await;
        Mock::given(method("GET")).and(path("/backend-api/codex/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models":[
                {"slug":"gpt-5.6-luna","priority":1},{"slug":"gpt-6.1-sol","priority":2},{"slug":"k08-live","display_name":"K08 Live","priority":3}
            ]})))
            .mount(&server).await;
        mount_script(&server, vec![
            sse(&[reasoning(MARK), tool_call("call-k08"), completed()]),
            sse(&[text("tool round answered"), completed()]),
            sse(&[text("resumed answer"), completed()]),
        ]).await;

        let (_, transcript) = login(&fixture, &server).await;
        assert!(
            transcript.contains("Discovered 3 models for ChatGPT Plus/Pro (Codex Subscription)."),
            "{transcript}"
        );

        let ask = fixture.run("k08-ask", &["ask", "use the tool", "--output-format", "json", "--yolo"]).await;
        let stdout = String::from_utf8_lossy(&ask.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&ask.stderr).into_owned();
        assert!(!mentions(&stdout, MARK) && !mentions(&stderr, MARK), "{stdout}");
        let doc: Value = serde_json::from_str(&stdout).unwrap();
        assert_eq!(doc["response"], "tool round answered");
        fixture.requests(&server, "k08-ask").await;
        let bodies = posts(&server).await;
        assert_eq!(bodies.len(), 2, "turn N, then turn N+1 after the tool round");
        assert!(replayed(&bodies[0]).is_empty());
        assert_replays(&bodies[1], MARK);
        let item = position(&bodies[1], |i| i["type"] == "reasoning").unwrap();
        let call = position(&bodies[1], |i| i["type"] == "function_call").unwrap();
        assert!(item < call, "replay precedes the function_call");

        let session = doc["session_id"].as_str().unwrap().to_owned();
        let resumed = fixture
            .run("k08-resume", &["--session", &session, "ask", "again", "--output-format", "json"])
            .await;
        let resumed_out = String::from_utf8_lossy(&resumed.stdout).into_owned();
        assert!(!mentions(&resumed_out, MARK), "{resumed_out}");
        fixture.requests(&server, "k08-resume").await;
        let bodies = posts(&server).await;
        assert_eq!(bodies.len(), 3);
        assert_replays(&bodies[2], MARK);
        fixture.json("k08-passed.json", &json!({
            "test": "conformance_20_1d1b_codex_replay_discovery::cli::k08_debug_binary_login_discovers_then_two_turn_ask_replays_reasoning",
            "discovered_line": "Discovered 3 models for ChatGPT Plus/Pro (Codex Subscription).",
            "codex_posts": 3, "replayed_on_post": [2, 3], "stdout_marker_free": true,
        }));
    })
    .await
    .expect("K08 structural deadline");
}
