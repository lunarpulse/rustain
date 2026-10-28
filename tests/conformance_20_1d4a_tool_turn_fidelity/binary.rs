//! K7: the real debug binary built with `test-instrumentation` (20-1d4's K12
//! setup: isolated dirs, the loopback safety gate before any HTTP action, a row
//! from `auth login google-gemini-cli`). Process 1 runs the AC2 tool loop with
//! the built-in `Read`; process 2 resumes it with `rustain --session <id> ask`,
//! the id taken from process 1's stderr hint.
use super::harness::{assert_prefix, mount};
use crate::conformance_20_1d4_gemini_cli::auth::{
    ACCESS, EMAIL, Google, LOAD, PROJECT, REFRESH, STREAM, ok, token_body,
};
use crate::conformance_20_1d4_gemini_cli::cli::{Fixture, cli_login, stream_posts};
use crate::conformance_20_1d4_gemini_cli::replay::{frame, leaks, signatures, sse};
use serde_json::json;
use serial_test::serial;
use std::time::Duration;
use wiremock::{
    Mock, MockServer,
    matchers::{method, path},
};

const SIG: [&str; 5] = ["U0lHMA==", "U0lHMQ==", "U0lHMg==", "U0lHMw==", "U0lHNA=="];

fn read(file: &str) -> serde_json::Value {
    json!({"name": "Read", "args": {"file_path": file}})
}

#[tokio::test]
#[serial]
async fn k7_real_debug_binary_ask_then_session_resume_reproduces_the_in_turn_history() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let server = MockServer::start().await;
        let fixture = Fixture::new(&server);
        crate::conformance_20_1d4_gemini_cli::auth::mount(
            &server,
            Google {
                token: ok(token_body(ACCESS, Some(REFRESH))),
                userinfo: ok(json!({"email": EMAIL, "id": "1"})),
                load: ok(json!({"allowedTiers": [{"id": "free-tier", "isDefault": true}]})),
                onboard: ok(json!({"name": "operations/fixture-op", "done": false})),
                polls: vec![ok(json!({"done": true, "response": {"cloudaicompanionProject": {"id": PROJECT}}}))],
            },
        )
        .await;
        Mock::given(method("GET"))
            .and(path("/api.json"))
            .respond_with(ok(json!({})))
            .mount(&server)
            .await;
        mount(
            &server,
            STREAM,
            vec![
                sse(&[
                    frame(json!([{"thought": true, "text": "T", "thoughtSignature": SIG[0]}]), None),
                    frame(json!([{"text": "V"}]), None),
                    frame(json!([{"text": "", "thoughtSignature": SIG[1]}]), None),
                    frame(
                        json!([
                            {"functionCall": read("a.txt"), "thoughtSignature": SIG[2]},
                            {"functionCall": read("b.txt")},
                        ]),
                        Some("STOP"),
                    ),
                ]),
                sse(&[frame(
                    json!([{"functionCall": read("c.txt"), "thoughtSignature": SIG[3]}]),
                    Some("STOP"),
                )]),
                sse(&[frame(
                    json!([{"text": "done", "thoughtSignature": SIG[4]}]),
                    Some("STOP"),
                )]),
                sse(&[frame(json!([{"text": "resumed"}]), Some("STOP"))]),
            ],
        )
        .await;
        for file in ["a", "b", "c"] {
            std::fs::write(
                fixture.home.path().join(format!("{file}.txt")),
                format!("contents of {file}\n"),
            )
            .unwrap();
        }
        cli_login(&fixture, &server).await;

        let first = fixture
            .run("k7-ask-1", &["ask", "read the notes", "--yolo"])
            .await;
        let first_err = String::from_utf8_lossy(&first.stderr).into_owned();
        let session = first_err
            .lines()
            .find_map(|line| line.trim().strip_prefix("Session saved. Resume with: rustain --session "))
            .unwrap_or_else(|| panic!("no resume hint: {first_err}"))
            .trim()
            .to_owned();
        let second = fixture
            .run("k7-ask-2", &["--session", &session, "ask", "next", "--yolo"])
            .await;
        let stdout = format!(
            "{}{}",
            String::from_utf8_lossy(&first.stdout),
            String::from_utf8_lossy(&second.stdout)
        );
        assert!(stdout.contains("done") && stdout.contains("resumed"), "{stdout}");
        assert!(!leaks(&stdout), "stdout carries no signature: {stdout}");

        let requests = fixture.requests(&server, "k7").await;
        let posts: Vec<String> = stream_posts(&requests)
            .into_iter()
            .map(|request| String::from_utf8_lossy(&request.body).into_owned())
            .collect();
        assert_eq!(posts.len(), 4, "three in-turn requests, then the resumed one");
        assert_eq!(hits_load(&requests), 1, "no request re-runs provisioning");
        let in_turn: serde_json::Value = serde_json::from_str(&posts[2]).unwrap();
        let model_turns: Vec<_> = in_turn["request"]["contents"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| c["role"] == "model")
            .map(signatures)
            .collect();
        assert_eq!(
            model_turns,
            vec![
                SIG[..3].iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                vec![SIG[3].to_owned()]
            ],
            "one model turn per response, every signature on its own part"
        );
        let rest = assert_prefix(&posts[2], &posts[3], "contents");
        assert_eq!(rest.len(), 2, "{rest:?}");
        assert_eq!(
            rest[0],
            json!({"role": "model", "parts": [{"text": "done", "thoughtSignature": SIG[4]}]})
        );
        assert_eq!(rest[1]["role"], "user");
        assert!(rest[1].to_string().contains("next"), "{rest:?}");
        fixture.json(
            "k7-passed.json",
            &json!({
                "test": "conformance_20_1d4a_tool_turn_fidelity::binary::k7_real_debug_binary_ask_then_session_resume_reproduces_the_in_turn_history",
                "session": session,
                "stream_posts": posts.len(),
                "post_4_starts_with_post_3_contents": true,
            }),
        );
    })
    .await
    .expect("K7 exceeded its structural 120-second deadline");
}

fn hits_load(requests: &[wiremock::Request]) -> usize {
    requests.iter().filter(|r| r.url.path() == LOAD).count()
}
