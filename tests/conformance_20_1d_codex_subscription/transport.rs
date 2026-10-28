use super::auth::{jwt, login_oauth};
use futures::StreamExt;
use rustain::{
    adapters::auth_store::FileAuthStore,
    domain::{
        errors::ProviderError,
        models::{CompletionOptions, Message, MessageRole, ProviderConfig, StreamChunk},
        ports::{AuthStorePort, StreamingProvider},
    },
    infrastructure::provider_factory::build_provider_for_config,
};
use serde_json::{Value, json};
use serial_test::serial;
use std::{
    ffi::OsString,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{method, path},
};

const NOW: i64 = 1_800_000_000_000;
const RESPONSE_PATH: &str = "/backend-api/codex/responses";
const COMPLETE: &str =
    "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n";
struct Env(Vec<(&'static str, Option<OsString>)>);
impl Env {
    fn new(dir: &std::path::Path, origin: &str) -> Self {
        let keys = [
            "RUSTAIN_DATA_DIR",
            "RUSTAIN_TEST_AUTH_ORIGIN",
            "RUSTAIN_TEST_AUTH_CALLBACK_PORT",
            "RUSTAIN_TEST_RESPONSES_RETRY_SCALE",
            "RUSTAIN_TEST_NOW_MS",
            "OPENAI_CODEX_OAUTH_TOKEN",
            "OPENAI_API_KEY",
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
        ];
        let env = Self(keys.iter().map(|k| (*k, std::env::var_os(k))).collect());
        // SAFETY: all environment-mutating tests use the same unkeyed serial lock.
        unsafe {
            for k in keys {
                std::env::remove_var(k);
            }
            std::env::set_var("RUSTAIN_DATA_DIR", dir);
            std::env::set_var("RUSTAIN_TEST_AUTH_ORIGIN", origin);
            std::env::set_var("RUSTAIN_TEST_RESPONSES_RETRY_SCALE", "0");
            std::env::set_var("RUSTAIN_TEST_NOW_MS", NOW.to_string());
            let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            std::env::set_var(
                "RUSTAIN_TEST_AUTH_CALLBACK_PORT",
                socket.local_addr().unwrap().port().to_string(),
            );
        }
        env
    }
    fn set(k: &str, v: &str) {
        // SAFETY: caller holds serial environment guard.
        unsafe {
            std::env::set_var(k, v);
        }
    }
}
impl Drop for Env {
    fn drop(&mut self) {
        for (k, v) in self.0.drain(..) {
            // SAFETY: serial lock outlives guard.
            unsafe {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
    }
}
fn token(account: Option<&str>, residency: Option<&str>, compute: Option<&str>) -> String {
    jwt(
        json!({"https://api.openai.com/auth":{"chatgpt_account_id":account,"chatgpt_data_residency":residency,"chatgpt_compute_residency":compute,"chatgpt_plan_type":"plus"},"https://api.openai.com/profile":{"email":"operator@example.test"}}),
    )
}
fn config() -> ProviderConfig {
    ProviderConfig {
        provider_id: "openai-codex".into(),
        kind: Some("openai-codex".into()),
        model_id: "gpt-5.6-luna".into(),
        api_key_env: "OPENAI_CODEX_OAUTH_TOKEN".into(),
        auth_header: None,
        enabled: true,
        base_url: None,
        context_window: None,
        supports_tools: None,
        discover_models: true,
        model_filter: vec!["*".into()],
        cache_ttl_seconds: 3600,
    }
}
fn options(id: &str) -> CompletionOptions {
    CompletionOptions {
        model: "gpt-5.6-luna".into(),
        max_tokens: 4096,
        system_prompt: String::new(),
        temperature: Some(0.7),
        tools: vec![],
        session_id: Some(id.into()),
    }
}
fn user(text: &str) -> Message {
    Message {
        role: MessageRole::User,
        content: text.into(),
        images: vec![],
        tool_results: vec![],
        tool_uses: vec![],
        context_prefix: None,
        reasoning_content: None,
        provider_replay: vec![],
    }
}
async fn provider(
    server: &MockServer,
    access: &str,
    id_token: Option<&str>,
) -> Arc<dyn StreamingProvider> {
    let store: Arc<dyn AuthStorePort> = Arc::new(FileAuthStore::new());
    login_oauth(server,&store,NOW,json!({"access_token":access,"id_token":id_token,"refresh_token":"REFRESH-INITIAL","expires_in":3600})).await;
    build_provider_for_config("openai-codex", &config()).unwrap()
}
fn sse(events: &[Value]) -> String {
    events.iter().map(|v| format!("data: {v}\n\n")).collect()
}
async fn mount_sse(server: &MockServer, body: String) {
    Mock::given(method("POST"))
        .and(path(RESPONSE_PATH))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(server)
        .await;
}
async fn chunks(
    provider: &Arc<dyn StreamingProvider>,
    messages: Vec<Message>,
    options: CompletionOptions,
) -> Vec<StreamChunk> {
    provider
        .stream_completion(messages, options)
        .await
        .unwrap()
        .collect()
        .await
}
fn posts(requests: &[Request]) -> Vec<&Request> {
    requests
        .iter()
        .filter(|r| r.url.path() == RESPONSE_PATH)
        .collect()
}

#[tokio::test]
#[serial]
async fn k03_request_core_and_k04_headers_come_from_selected_bearer() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let _env = Env::new(dir.path(), &server.uri());
    let access = token(Some("access-account"), Some("eu"), None);
    let provider = provider(&server, &access, None).await;
    mount_sse(&server, COMPLETE.into()).await;
    let mut options = options("conversation-A");
    options.system_prompt = "Be helpful".into();
    options.tools = vec![rustain::domain::models::ToolDefinition {
        name: "lookup".into(),
        description: "Find a record".into(),
        input_schema: json!({"type":"object","oneOf":[{"type":"string"}],"pattern":"(?=a)a"}),
        parallel_safe: false,
    }];
    let mut message = user("hello");
    message.context_prefix = Some("context".into());
    message
        .images
        .push(rustain::domain::models::ImageAttachment {
            media_type: "image/png".into(),
            data: "AA==".into(),
        });
    let mut system = user("History instruction");
    system.role = MessageRole::System;
    let mut assistant = user("Calling tools");
    assistant.role = MessageRole::Assistant;
    assistant.reasoning_content = Some("private-reasoning-never-echo".into());
    assistant.tool_uses = vec![
        rustain::domain::models::ToolUseMessage {
            id: "paired".into(),
            name: "lookup".into(),
            input: json!({"id":1}),
        },
        rustain::domain::models::ToolUseMessage {
            id: "interrupted".into(),
            name: "lookup".into(),
            input: json!({}),
        },
    ];
    let mut result = user("");
    result.tool_results = vec![
        rustain::domain::models::ToolResultMessage {
            tool_use_id: "paired".into(),
            content: "found".into(),
            is_error: true,
            images: vec![],
        },
        rustain::domain::models::ToolResultMessage {
            tool_use_id: "orphan".into(),
            content: "old result".into(),
            is_error: false,
            images: vec![],
        },
    ];
    let output = chunks(&provider, vec![system, assistant, result, message], options).await;
    assert!(matches!(
        output.last(),
        Some(StreamChunk::TurnComplete { .. })
    ));
    let requests = server.received_requests().await.unwrap();
    let request = posts(&requests)[0];
    let body: Value = request.body_json().unwrap();
    assert_eq!(body["model"], "gpt-5.6-luna");
    assert_eq!(body["instructions"], "Be helpful");
    assert_eq!(
        body["input"],
        json!([
            {"type":"message","role":"developer","content":[{"type":"input_text","text":"History instruction"}]},
            {"type":"message","role":"assistant","content":[{"type":"output_text","text":"Calling tools","annotations":[]}]},
            {"type":"function_call","call_id":"paired","name":"lookup","arguments":"{\"id\":1}"},
            {"type":"function_call","call_id":"interrupted","name":"lookup","arguments":"{}"},
            {"type":"function_call_output","call_id":"interrupted","output":"[No tool output recorded: the tool call was interrupted before it produced a result.]"},
            {"type":"function_call_output","call_id":"paired","output":"Error: found"},
            {"type":"message","role":"assistant","content":"[Previous tool result; call_id=orphan]: old result"},
            {"role":"user","content":[{"type":"input_text","text":"context\n\nhello"},{"type":"input_image","detail":"auto","image_url":"data:image/png;base64,AA=="}]}
        ])
    );
    assert_eq!(
        body["tools"][0],
        json!({"type":"function","name":"lookup","description":"Find a record","parameters":{"type":"object","properties":{},"anyOf":[{"type":"string"}]}})
    );
    assert_eq!(body["store"], false);
    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    for field in [
        "temperature",
        "max_output_tokens",
        "max_completion_tokens",
        "top_p",
        "stop",
        "parallel_tool_calls",
        "reasoning",
    ] {
        assert!(body.get(field).is_none(), "unexpected {field}");
    }
    let text = std::str::from_utf8(&request.body).unwrap();
    let mut previous = 0;
    assert!(!text.contains("private-reasoning-never-echo"));
    for key in [
        "\"model\"",
        "\"input\"",
        "\"stream\"",
        "\"prompt_cache_key\"",
        "\"tools\"",
        "\"instructions\"",
        "\"store\"",
        "\"include\"",
        "\"client_metadata\"",
    ] {
        let position = text.find(key).unwrap();
        assert!(position >= previous, "field order {key}");
        previous = position;
    }
    for (key, value) in [
        ("originator", "omp"),
        ("user-agent", "omp/18.12.6"),
        ("version", "0.159.0"),
        ("openai-beta", "responses=experimental"),
        ("chatgpt-account-id", "access-account"),
        ("x-openai-internal-codex-residency", "eu"),
        ("x-codex-routing-hint", "model=gpt-5.6-luna"),
        ("conversation_id", "conversation-A"),
        ("session_id", "conversation-A"),
        ("x-client-request-id", "conversation-A"),
    ] {
        assert_eq!(request.headers.get(key).unwrap(), value, "{key}");
    }
    assert!(request.headers.get("x-api-key").is_none());
    let metadata: &Value = &body["client_metadata"];
    let turn: Value = serde_json::from_str(
        request
            .headers
            .get("x-codex-turn-metadata")
            .unwrap()
            .to_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(turn["session_id"], "conversation-A");
    assert_eq!(turn["request_kind"], "turn");
    assert_eq!(
        metadata["x-codex-turn-metadata"],
        request
            .headers
            .get("x-codex-turn-metadata")
            .unwrap()
            .to_str()
            .unwrap()
    );
    assert_eq!(
        metadata["thread_id"],
        request.headers.get("thread-id").unwrap().to_str().unwrap()
    );
    assert_eq!(
        metadata["x-codex-window-id"],
        request
            .headers
            .get("x-codex-window-id")
            .unwrap()
            .to_str()
            .unwrap()
    );
    assert_eq!(body["prompt_cache_key"], "conversation-A");
}

#[tokio::test]
#[serial]
async fn k04_id_token_stored_identity_is_not_an_access_claim_and_no_constraint_wins() {
    for (account, residency, compute, expected) in [
        (None, None, None, None),
        (
            Some("current"),
            Some("no_constraint"),
            Some("us"),
            Some("current"),
        ),
    ] {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let _env = Env::new(dir.path(), &server.uri());
        let id = token(Some("stored-only"), None, None);
        let provider = provider(&server, &token(account, residency, compute), Some(&id)).await;
        mount_sse(&server, COMPLETE.into()).await;
        chunks(&provider, vec![user("hi")], options("id")).await;
        let requests = server.received_requests().await.unwrap();
        let request = posts(&requests)[0];
        assert_eq!(
            request
                .headers
                .get("chatgpt-account-id")
                .map(|v| v.to_str().unwrap()),
            expected
        );
        assert!(
            request
                .headers
                .get("x-openai-internal-codex-residency")
                .is_none()
        );
        let snapshot = FileAuthStore::new().snapshot().await.unwrap();
        assert!(
            matches!(&snapshot.rows("openai-codex")[0].credential,rustain::domain::models::Credential::OAuth(o) if o.identity.account_id.as_deref()==account.or(Some("stored-only")))
        );
    }
}

#[tokio::test]
#[serial]
async fn k05_interleaved_final_tool_arguments_usage_and_missing_terminal() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let _env = Env::new(dir.path(), &server.uri());
    let provider = provider(&server, &token(Some("a"), None, None), None).await;
    let events = vec![
        json!({"type":"response.output_text.delta","delta":"hello 🦀"}),
        json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call-a","name":"a"}}),
        json!({"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","call_id":"call-b","name":"b"}}),
        json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"x\":"}),
        json!({"type":"response.function_call_arguments.delta","output_index":1,"delta":"{\"y\":2}"}),
        json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":"1}"}),
        json!({"type":"response.function_call_arguments.done","output_index":0,"arguments":"{\"x\":1}"}),
        json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","call_id":"call-a","name":"a","arguments":"{\"x\":9}"}}),
        json!({"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","call_id":"call-b","name":"b","arguments":"{\"y\":8}"}}),
        json!({"type":"response.output_item.done","item":{"type":"reasoning","encrypted_content":"not-exposed"}}),
        json!({"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":100,"input_tokens_details":{"cached_tokens":40},"output_tokens":7,"output_tokens_details":{"reasoning_tokens":5}}}}),
    ];
    mount_sse(&server, sse(&events)).await;
    let out = chunks(&provider, vec![user("hi")], options("tools")).await;
    let tools: Vec<_> = out
        .iter()
        .filter_map(|v| {
            if let StreamChunk::ToolUse { id, name, input } = v {
                Some((id.as_str(), name.as_str(), input.clone()))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        tools,
        vec![
            ("call-a", "a", json!({"x":9})),
            ("call-b", "b", json!({"y":8}))
        ]
    );
    assert_eq!(
        out.iter()
            .filter(|c| matches!(c, StreamChunk::Text { .. }))
            .count(),
        1
    );
    // Story 20.1d1b: the reasoning item is released as exactly one
    // operator-private replay chunk, after both tools and before usage/stop;
    // it is never a visible Thinking chunk.
    let kinds: Vec<_> = out
        .iter()
        .map(|c| match c {
            StreamChunk::Text { .. } => "text",
            StreamChunk::ToolUse { .. } => "tool",
            StreamChunk::ProviderReplay { .. } => "replay",
            StreamChunk::Usage { .. } => "usage",
            StreamChunk::TurnComplete { .. } => "complete",
            _ => "other",
        })
        .collect();
    assert_eq!(
        kinds,
        ["text", "tool", "tool", "replay", "usage", "complete"]
    );
    let replay: Vec<_> = out
        .iter()
        .filter_map(|c| match c {
            StreamChunk::ProviderReplay { item } => Some(item),
            _ => None,
        })
        .collect();
    assert_eq!(replay.len(), 1);
    assert_eq!(replay[0].provider, "openai-codex");
    assert_eq!(replay[0].model, "gpt-5.6-luna");
    assert_eq!(
        *replay[0].payload,
        json!({"type":"reasoning","encrypted_content":"not-exposed"})
    );
    assert!(
        !out.iter()
            .any(|c| matches!(c, StreamChunk::Thinking { .. }))
    );
    assert!(out.iter().any(|c|matches!(c,StreamChunk::Usage{usage,..} if usage.input_tokens==60 && usage.cache_read_input_tokens==Some(40) && usage.output_tokens==7 && usage.reasoning_tokens==Some(5))));
    assert!(matches!(
        out.last(),
        Some(StreamChunk::TurnComplete {
            stop_reason: rustain::domain::models::StopReason::ToolUse
        })
    ));
    server.reset().await;
    mount_sse(
        &server,
        sse(&[json!({"type":"response.output_text.delta","delta":"partial"})]),
    )
    .await;
    let out = chunks(&provider, vec![user("hi")], options("truncated")).await;
    assert!(matches!(out.first(),Some(StreamChunk::Text{content,..}) if content=="partial"));
    assert!(
        matches!(out.last(),Some(StreamChunk::Error{content}) if content.contains("Codex stream ended before terminal completion event"))
    );
    assert_eq!(posts(&server.received_requests().await.unwrap()).len(), 1);
    // A terminal event closed by EOF instead of a blank line still completes, once.
    server.reset().await;
    mount_sse(&server, COMPLETE.trim_end().to_owned()).await;
    let out = chunks(&provider, vec![user("hi")], options("unterminated")).await;
    assert!(matches!(
        out.last(),
        Some(StreamChunk::TurnComplete {
            stop_reason: rustain::domain::models::StopReason::EndTurn
        })
    ));
    assert_eq!(posts(&server.received_requests().await.unwrap()).len(), 1);
}

#[tokio::test]
#[serial]
async fn k06_turn_state_sessions_new_turn_and_side_request_are_isolated() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let _env = Env::new(dir.path(), &server.uri());
    let provider = provider(&server, &token(Some("a"), None, None), None).await;
    Mock::given(method("POST"))
        .and(path(RESPONSE_PATH))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .insert_header("x-codex-turn-state", "A")
                .set_body_string(format!(
                    "{}{}",
                    sse(&[
                        json!({"type":"response.metadata","headers":{"x-models-etag":"etag-A"}})
                    ]),
                    COMPLETE
                )),
        )
        .mount(&server)
        .await;
    chunks(&provider, vec![user("turn1")], options("session-A")).await;
    let mut continuation = user("");
    continuation
        .tool_results
        .push(rustain::domain::models::ToolResultMessage {
            tool_use_id: "call".into(),
            content: "result".into(),
            is_error: false,
            images: Vec::new(),
        });
    let (a, b) = tokio::join!(
        chunks(&provider, vec![continuation], options("session-A")),
        chunks(&provider, vec![user("B")], options("session-B"))
    );
    assert!(!a.is_empty() && !b.is_empty());
    chunks(&provider, vec![user("turn2")], options("session-A")).await;
    chunks(
        &provider,
        vec![user("compact")],
        options("session-A:compaction"),
    )
    .await;
    let requests = server.received_requests().await.unwrap();
    let requests = posts(&requests);
    let continuation = requests
        .iter()
        .find(|r| {
            r.body_json::<Value>().unwrap()["input"]
                .as_array()
                .unwrap()
                .iter()
                .any(|i| i["type"] == "function_call_output" || i["role"] == "assistant")
        })
        .unwrap();
    assert_eq!(continuation.headers.get("x-codex-turn-state").unwrap(), "A");
    assert_eq!(continuation.headers.get("x-models-etag").unwrap(), "etag-A");
    for r in requests.iter().filter(|r| {
        r.body_json::<Value>().unwrap()["input"][0]["content"][0]["text"] == "turn2"
            || r.headers
                .get("session_id")
                .is_some_and(|v| v == "session-B" || v == "session-A:compaction")
    }) {
        assert!(r.headers.get("x-codex-turn-state").is_none());
    }
}

#[tokio::test]
#[serial]
async fn k07_transport_attempts_hint_ceiling_and_in_band_commit_boundary() {
    for (status, success_after, expected) in [
        (503, 2, 3),
        (500, usize::MAX, 6),
        (403, usize::MAX, 1),
        (404, usize::MAX, 1),
        (429, usize::MAX, 1),
    ] {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let _env = Env::new(dir.path(), &server.uri());
        let provider = provider(&server, &token(Some("a"), None, None), None).await;
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        Mock::given(method("POST"))
            .and(path(RESPONSE_PATH))
            .respond_with(move |_: &Request| {
                if seen.fetch_add(1, Ordering::SeqCst) >= success_after {
                    ResponseTemplate::new(200).set_body_string(COMPLETE)
                } else {
                    let r = ResponseTemplate::new(status).set_body_string("failure");
                    if status == 429 {
                        r.insert_header("retry-after", "400")
                    } else {
                        r
                    }
                }
            })
            .mount(&server)
            .await;
        let result = provider
            .stream_completion(vec![user("hi")], options("retry"))
            .await;
        if success_after != usize::MAX {
            assert!(result.is_ok());
            let _ = result.unwrap().collect::<Vec<_>>().await;
        } else {
            assert!(result.is_err());
            if status == 500 {
                assert!(matches!(&result, Err(ProviderError::ConnectionFailed(_))));
            }
            if status == 429 {
                assert!(matches!(
                    &result,
                    Err(ProviderError::RateLimited {
                        retry_after_ms: Some(400000)
                    })
                ));
            }
        }
        assert_eq!(count.load(Ordering::SeqCst), expected, "status {status}");
    }
    for committed in [false, true] {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let _env = Env::new(dir.path(), &server.uri());
        let provider = provider(&server, &token(Some("a"), None, None), None).await;
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        Mock::given(method("POST")).and(path(RESPONSE_PATH)).respond_with(move |_:&Request|{let n=seen.fetch_add(1,Ordering::SeqCst);let body=if n>0{COMPLETE.into()}else{let mut events=vec![];if committed{events.push(json!({"type":"response.output_text.delta","delta":"already delivered"}));}events.push(json!({"type":"response.failed","response":{"error":{"code":"server_error","message":"retry your request"}}}));sse(&events)};ResponseTemplate::new(200).set_body_string(body)}).mount(&server).await;
        let out = chunks(&provider, vec![user("hi")], options("inband")).await;
        assert_eq!(count.load(Ordering::SeqCst), if committed { 1 } else { 2 });
        assert_eq!(
            out.iter().any(|c| matches!(c, StreamChunk::Error { .. })),
            committed
        );
    }
}

#[tokio::test]
#[serial]
async fn k08_usage_limit_is_friendly_after_six_attempts() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let _env = Env::new(dir.path(), &server.uri());
    let secret = token(Some("secret-account"), Some("secret-residency"), None);
    let provider = provider(&server, &secret, None).await;
    Mock::given(method("POST")).and(path(RESPONSE_PATH)).respond_with(ResponseTemplate::new(429).set_body_json(json!({"error":{"code":"usage_limit_reached","plan_type":"Plus","resets_at":NOW/1000+1800}}))).mount(&server).await;
    let error = provider
        .stream_completion(vec![user("hi")], options("quota"))
        .await
        .err()
        .unwrap();
    assert!(
        matches!(&error,ProviderError::Other(s) if s=="You have hit your ChatGPT usage limit (plus plan). Try again in ~30 min.")
    );
    let display = error.to_string();
    for secret in [&secret, "secret-account", "secret-residency"] {
        assert!(!display.contains(secret));
    }
    assert_eq!(posts(&server.received_requests().await.unwrap()).len(), 6);
}

#[tokio::test]
#[serial]
async fn k09_stored_401_refreshes_once_preserves_org_and_env_never_refreshes() {
    for env in [false, true] {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let _guard = Env::new(dir.path(), &server.uri());
        let initial = token(Some("account"), None, None);
        let next = token(Some("new-account"), None, None);
        let provider = provider(&server, &initial, None).await;
        if env {
            Env::set("OPENAI_CODEX_OAUTH_TOKEN", &initial);
            FileAuthStore::new().remove("openai-codex").await.unwrap();
        }
        let refresh_count = Arc::new(AtomicUsize::new(0));
        let refreshed = refresh_count.clone();
        let refresh_token = next.clone();
        Mock::given(method("POST")).and(path("/oauth/token")).and(|r:&Request|String::from_utf8_lossy(&r.body).contains("grant_type=refresh_token")).respond_with(move |r:&Request|{let fields:std::collections::HashMap<_,_>=url::form_urlencoded::parse(&r.body).into_owned().collect();assert_eq!(fields.len(),3);assert_eq!(fields["client_id"],"app_EMoamEEZ73f0CkXaXp7hrann");assert_eq!(fields["refresh_token"],"REFRESH-INITIAL");refreshed.fetch_add(1,Ordering::SeqCst);ResponseTemplate::new(200).set_body_json(json!({"access_token":refresh_token,"refresh_token":"ROTATED-REFRESH","expires_in":3600}))}).with_priority(1).mount(&server).await;
        let bearer = format!("Bearer {next}");
        Mock::given(method("POST"))
            .and(path(RESPONSE_PATH))
            .respond_with(move |r: &Request| {
                if r.headers
                    .get("authorization")
                    .is_some_and(|v| v == bearer.as_str())
                {
                    ResponseTemplate::new(200).set_body_string(COMPLETE)
                } else {
                    ResponseTemplate::new(401)
                }
            })
            .mount(&server)
            .await;
        let result = provider
            .stream_completion(vec![user("hi")], options("refresh"))
            .await;
        if env {
            assert!(matches!(result, Err(ProviderError::AuthenticationFailed)));
        } else {
            assert!(result.is_ok());
            let _: Vec<_> = result.unwrap().collect().await;
            let snapshot = FileAuthStore::new().snapshot().await.unwrap();
            assert!(
                matches!(&snapshot.rows("openai-codex")[0].credential,rustain::domain::models::Credential::OAuth(o) if o.identity.org_id.as_deref()==Some("account") && o.identity.org_name.as_deref()==Some("plus") && o.refresh.expose_secret()=="ROTATED-REFRESH")
            );
        }
        assert_eq!(refresh_count.load(Ordering::SeqCst), usize::from(!env));
        assert_eq!(
            posts(&server.received_requests().await.unwrap()).len(),
            if env { 1 } else { 2 }
        );
    }
}
