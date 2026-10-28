//! Refresh, request-time credential isolation, request bytes, stream mapping,
//! resilience and probes, all through `flows::login` rows and the factory-built
//! `StreamingProvider`, observed at the stub, the store and the stream.

use super::auth::{
    ACCESS, CLIENT_ID, CLIENT_SECRET, EMAIL, Env, FREE_LOAD_BODY, LOAD, MODEL, NOW, ONBOARD,
    OPERATION, PROJECT, PROVIDER, REFRESH, STREAM, TOKEN, USERINFO, build, config, env, hits,
    login, ok, options, store, token_body, user,
};
use crate::conformance_20_1d_codex_subscription::boundaries::{
    assert_no_connection, assert_peer_eof, raw_listener, read_request, stream_headers, write_chunk,
};
use futures::StreamExt;
use rustain::domain::{
    errors::ProviderError,
    models::{
        CasOutcome, CompletionOptions, Credential, ImageAttachment, Message, MessageRole,
        OAuthCredential, StoredCredential, StreamChunk, ToolDefinition, ToolResultMessage,
        ToolUseMessage,
    },
    ports::StreamingProvider,
};
use rustain::infrastructure::provider_factory::build_provider_for_config;
use serde_json::{Value, json};
use serial_test::serial;
use std::{future::Future, sync::Arc, time::Duration};
use tokio::sync::oneshot;
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{body_string_contains, header, method, path},
};

const REFRESHED: &str = "GEMINI-ACCESS-REFRESHED";
const STREAM_TARGET: &str = "/v1internal:streamGenerateContent?alt=sse";
const CLIENT_METADATA: &str =
    "ideType=IDE_UNSPECIFIED,platform=PLATFORM_UNSPECIFIED,pluginType=GEMINI";
const FIRST_EVENT_TIMEOUT: &str =
    "Cloud Code Assist stream timed out while waiting for the first event";
const NO_FINISH: &str = "Cloud Code Assist stream ended without a finish reason (connection dropped or response truncated)";
const MISSING_PROJECT: &str = "Missing token or projectId in Google Cloud credentials. Run `rustain auth login google-gemini-cli` to re-authenticate.";
const PROJECT_LESS_REFRESH: &str = "OAuth refresh unavailable for google-gemini-cli: google-gemini-cli credentials are missing projectId; sign in again";

// ---------------------------------------------------------------------------
// Fixture plumbing
// ---------------------------------------------------------------------------

/// Bounds a structural wait; never a pacing delay.
async fn within<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(30), future)
        .await
        .expect("operation exceeded its deadline")
}

/// Isolated environment plus a production login of `ACCESS`/`PROJECT`/`EMAIL`.
async fn signed_in() -> (MockServer, tempfile::TempDir, Env) {
    let server = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();
    let guard = env(temp.path(), &server.uri());
    login(&server, ACCESS, PROJECT, EMAIL).await;
    (server, temp, guard)
}

async fn received(server: &MockServer) -> Vec<Request> {
    server.received_requests().await.unwrap()
}

async fn since(server: &MockServer, baseline: usize) -> Vec<Request> {
    received(server).await.split_off(baseline)
}

/// Stream POSTs whose body carries `marker`.
fn posts<'a>(requests: &'a [Request], marker: &str) -> Vec<&'a Request> {
    hits(requests, STREAM)
        .into_iter()
        .filter(|request| String::from_utf8_lossy(&request.body).contains(marker))
        .collect()
}

/// The bearer header and envelope `project` one request carried.
fn grant(request: &Request) -> (String, String) {
    let body: Value = serde_json::from_slice(&request.body).unwrap();
    (
        request.headers["authorization"]
            .to_str()
            .unwrap()
            .to_owned(),
        body["project"].as_str().unwrap().to_owned(),
    )
}

fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

fn agent(model: &str) -> String {
    let platform = match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => other,
    };
    format!("GeminiCLI/0.46.0/{model} ({platform}; {arch}; terminal)")
}

fn sse(events: &[Value]) -> String {
    events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect()
}

fn sse_ok(body: String) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body.into_bytes(), "text/event-stream")
}

fn parts(parts: Value) -> Value {
    json!({"response": {"candidates": [{"content": {"role": "model", "parts": parts}}]}})
}

fn finish(reason: &str) -> Value {
    json!({"response": {"candidates": [{"finishReason": reason}]}})
}

fn text_reply(text: &str) -> String {
    sse(&[json!({"response": {"candidates": [{
        "content": {"role": "model", "parts": [{"text": text}]},
        "finishReason": "STOP"
    }]}})])
}

fn replay(model: &str, parts: Value) -> String {
    format!(
        "replay:{PROVIDER}:{model}:{}",
        json!({ "gemini_parts": parts })
    )
}

/// One line per chunk; replay payloads print with serde's key order on both sides.
fn describe(chunks: &[StreamChunk]) -> Vec<String> {
    chunks
        .iter()
        .map(|chunk| match chunk {
            StreamChunk::Text { content, .. } => format!("text:{content}"),
            StreamChunk::Thinking { content, .. } => format!("thinking:{content}"),
            StreamChunk::ToolUse { id, name, input } => format!("tool:{id}:{name}:{input}"),
            StreamChunk::ProviderReplay { item } => {
                format!("replay:{}:{}:{}", item.provider, item.model, item.payload)
            }
            StreamChunk::Usage { usage, .. } => format!(
                "usage:{}:{}:{:?}:{:?}:{:?}",
                usage.input_tokens,
                usage.output_tokens,
                usage.cache_creation_input_tokens,
                usage.cache_read_input_tokens,
                usage.reasoning_tokens
            ),
            StreamChunk::TurnComplete { stop_reason } => format!("done:{stop_reason:?}"),
            StreamChunk::Error { content } => format!("error:{content}"),
            other => format!("{other:?}"),
        })
        .collect()
}

async fn collect(
    provider: &Arc<dyn StreamingProvider>,
    messages: Vec<Message>,
    options: CompletionOptions,
) -> Vec<StreamChunk> {
    match within(provider.stream_completion(messages, options)).await {
        Ok(stream) => within(stream.collect::<Vec<_>>()).await,
        Err(error) => panic!("stream_completion failed: {error:?}"),
    }
}

async fn refused(
    provider: &Arc<dyn StreamingProvider>,
    messages: Vec<Message>,
    options: CompletionOptions,
) -> ProviderError {
    match within(provider.stream_completion(messages, options)).await {
        Ok(_) => panic!("stream_completion must fail"),
        Err(error) => error,
    }
}

/// Mounts the SSE answer for requests carrying `marker`, then streams it.
async fn case(
    server: &MockServer,
    provider: &Arc<dyn StreamingProvider>,
    model: &str,
    marker: &str,
    body: String,
) -> Vec<String> {
    Mock::given(method("POST"))
        .and(path(STREAM))
        .and(body_string_contains(marker))
        .respond_with(sse_ok(body))
        .mount(server)
        .await;
    describe(&collect(provider, vec![user(marker)], options(model)).await)
}

async fn mount_stream(server: &MockServer, body: String) {
    Mock::given(method("POST"))
        .and(path(STREAM))
        .respond_with(sse_ok(body))
        .mount(server)
        .await;
}

/// The refresh grant answers a new access token and omits refresh token,
/// email and project, which the merge must keep.
async fn mount_refresh(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path(TOKEN))
        .and(body_string_contains("grant_type=refresh_token"))
        .respond_with(ok(token_body(REFRESHED, None)))
        .with_priority(1)
        .mount(server)
        .await;
}

fn refresh_form() -> String {
    format!(
        "grant_type=refresh_token&client_id={CLIENT_ID}&client_secret={CLIENT_SECRET}&refresh_token={REFRESH}"
    )
}

async fn rows() -> Vec<StoredCredential> {
    store().snapshot().await.unwrap().rows(PROVIDER).to_vec()
}

fn oauth(row: &StoredCredential) -> &OAuthCredential {
    match &row.credential {
        Credential::OAuth(oauth) => oauth,
        _ => panic!("login stores an OAuth row"),
    }
}

/// The single sanctioned rewrite of a login-produced row, through the store's CAS.
async fn rewrite(edit: impl FnOnce(&mut OAuthCredential)) {
    let store = store();
    let snapshot = store.snapshot().await.unwrap();
    let rows = snapshot.rows(PROVIDER);
    assert_eq!(rows.len(), 1);
    let mut credential = oauth(&rows[0]).clone();
    edit(&mut credential);
    let outcome = store
        .replace_if_matches(PROVIDER, rows[0].stamp(), Credential::OAuth(credential))
        .await
        .unwrap();
    assert_eq!(outcome, CasOutcome::Applied);
}

fn assert_no_project_discovery(requests: &[Request]) {
    for path in [USERINFO, LOAD, ONBOARD, OPERATION] {
        assert!(
            hits(requests, path).is_empty(),
            "refresh must never call {path}"
        );
    }
}

// ---------------------------------------------------------------------------
// K04 — refresh
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial]
async fn k04_stale_row_refreshes_once_with_the_secret_and_never_rediscovers_the_project() {
    let (server, _temp, _env) = signed_in().await;
    let baseline = received(&server).await.len();
    mount_refresh(&server).await;
    mount_stream(&server, text_reply("after refresh")).await;
    // The login row expires at NOW + 3 300 000; this clock makes it stale.
    let later = (NOW + 3_300_000).to_string();
    let _clock = Env::set(&[("RUSTAIN_TEST_NOW_MS", Some(&later))]);

    let chunks = collect(&build(), vec![user("stale row")], options(MODEL)).await;
    assert_eq!(
        describe(&chunks),
        [
            "text:after refresh".to_owned(),
            replay(MODEL, json!([{"text": "after refresh"}])),
            "done:EndTurn".to_owned(),
        ]
    );

    let sent = since(&server, baseline).await;
    let token = hits(&sent, TOKEN);
    assert_eq!(token.len(), 1, "exactly one refresh");
    assert_eq!(
        token[0].headers["content-type"],
        "application/x-www-form-urlencoded"
    );
    assert_eq!(String::from_utf8_lossy(&token[0].body), refresh_form());
    assert_no_project_discovery(&sent);
    let stream = hits(&sent, STREAM);
    assert_eq!(stream.len(), 1);
    assert_eq!(grant(stream[0]), (bearer(REFRESHED), PROJECT.to_owned()));

    // Project, email and the unrotated refresh token survive the merge.
    let rows = rows().await;
    assert_eq!(rows.len(), 1);
    let row = oauth(&rows[0]);
    assert_eq!(row.access.expose_secret(), REFRESHED);
    assert_eq!(row.refresh.expose_secret(), REFRESH);
    assert_eq!(row.expires_at_ms, NOW + 3_300_000 + 3_600_000 - 300_000);
    assert_eq!(row.identity.email.as_deref(), Some(EMAIL));
    assert_eq!(row.identity.project_id.as_deref(), Some(PROJECT));
    assert!(rows[0].disabled_cause.is_none());
}

#[tokio::test]
#[serial]
async fn k04_rejected_bearer_refreshes_once_and_resends_with_the_same_project() {
    let (server, _temp, _env) = signed_in().await;
    let baseline = received(&server).await.len();
    mount_refresh(&server).await;
    Mock::given(method("POST"))
        .and(path(STREAM))
        .and(header("authorization", bearer(ACCESS).as_str()))
        .respond_with(ResponseTemplate::new(401).set_body_string("expired"))
        .with_priority(1)
        .mount(&server)
        .await;
    mount_stream(&server, text_reply("resent")).await;

    let chunks = collect(&build(), vec![user("rejected bearer")], options(MODEL)).await;
    assert_eq!(describe(&chunks)[0], "text:resent");
    assert_eq!(describe(&chunks).last().unwrap(), "done:EndTurn");

    let sent = since(&server, baseline).await;
    let token = hits(&sent, TOKEN);
    assert_eq!(token.len(), 1, "one recovery per logical request");
    assert_eq!(String::from_utf8_lossy(&token[0].body), refresh_form());
    assert_no_project_discovery(&sent);
    let stream = hits(&sent, STREAM);
    assert_eq!(stream.len(), 2, "exactly one resend");
    assert_eq!(grant(stream[0]), (bearer(ACCESS), PROJECT.to_owned()));
    assert_eq!(grant(stream[1]), (bearer(REFRESHED), PROJECT.to_owned()));
    assert_eq!(
        stream[0].body, stream[1].body,
        "the same project re-encodes the same body"
    );

    let rows = rows().await;
    let row = oauth(&rows[0]);
    assert_eq!(row.access.expose_secret(), REFRESHED);
    assert_eq!(row.refresh.expose_secret(), REFRESH);
    assert_eq!(row.identity.email.as_deref(), Some(EMAIL));
    assert_eq!(row.identity.project_id.as_deref(), Some(PROJECT));
}

#[tokio::test]
#[serial]
async fn k04_project_less_stale_row_fails_before_any_http_and_stays_enabled() {
    let (server, _temp, _env) = signed_in().await;
    rewrite(|row| {
        row.identity.project_id = None;
        row.expires_at_ms = NOW - 1;
    })
    .await;
    mount_refresh(&server).await;
    mount_stream(&server, text_reply("never")).await;
    let baseline = received(&server).await.len();

    let error = refused(&build(), vec![user("project-less")], options(MODEL)).await;
    assert_eq!(
        error.to_string(),
        format!("Connection failed: {PROJECT_LESS_REFRESH}")
    );
    match &error {
        ProviderError::ConnectionFailed(text) => assert_eq!(text, PROJECT_LESS_REFRESH),
        other => panic!("{other:?}"),
    }
    assert!(
        since(&server, baseline).await.is_empty(),
        "the precondition runs before any HTTP"
    );
    let rows = rows().await;
    assert_eq!(rows.len(), 1);
    assert!(
        rows[0].disabled_cause.is_none(),
        "a transient failure never disables"
    );
    assert_eq!(oauth(&rows[0]).identity.project_id, None);
}

#[tokio::test]
#[serial]
async fn k04_fresh_project_less_row_is_refused_before_any_post() {
    let (server, _temp, _env) = signed_in().await;
    rewrite(|row| row.identity.project_id = None).await;
    mount_stream(&server, text_reply("never")).await;
    let baseline = received(&server).await.len();

    match refused(&build(), vec![user("fresh project-less")], options(MODEL)).await {
        ProviderError::Other(text) => assert_eq!(text, MISSING_PROJECT),
        other => panic!("{other:?}"),
    }
    let sent = since(&server, baseline).await;
    assert!(hits(&sent, STREAM).is_empty());
    assert!(sent.is_empty());
}

// ---------------------------------------------------------------------------
// K05 — request-time credential isolation
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial]
async fn k05_each_request_takes_bearer_and_project_from_one_current_grant() {
    const ACCESS_B: &str = "GEMINI-ACCESS-SECOND";
    const PROJECT_B: &str = "gemini-second-project";
    const EMAIL_B: &str = "second@example.test";
    let (server, _temp, _env) = signed_in().await;
    // Built once; every later request resolves its own grant.
    let provider = build();
    mount_refresh(&server).await;
    Mock::given(method("POST"))
        .and(path(STREAM))
        .and(header("authorization", bearer(ACCESS).as_str()))
        .respond_with(ResponseTemplate::new(401))
        .with_priority(1)
        .mount(&server)
        .await;
    mount_stream(&server, text_reply("served")).await;

    // A 401 swaps bearer and project together, from the recovered grant.
    let baseline = received(&server).await.len();
    collect(&provider, vec![user("grant A")], options(MODEL)).await;
    let sent = since(&server, baseline).await;
    let stream = hits(&sent, STREAM);
    assert_eq!(stream.len(), 2);
    assert_eq!(grant(stream[0]), (bearer(ACCESS), PROJECT.to_owned()));
    assert_eq!(grant(stream[1]), (bearer(REFRESHED), PROJECT.to_owned()));

    // Logout A: no env credential stands in for the removed rows.
    store().remove(PROVIDER).await.unwrap();
    {
        let _keys = Env::set(&[
            ("GOOGLE_API_KEY", Some("env-key-never-used")),
            ("GEMINI_API_KEY", Some("env-key-never-used")),
        ]);
        let baseline = received(&server).await.len();
        assert!(matches!(
            refused(&provider, vec![user("signed out")], options(MODEL)).await,
            ProviderError::AuthenticationFailed
        ));
        assert!(matches!(
            build_provider_for_config(PROVIDER, &config(None)),
            Err(ProviderError::AuthenticationFailed)
        ));
        assert!(hits(&since(&server, baseline).await, STREAM).is_empty());
    }

    // Login B in the same process; the provider built before it sends B's grant.
    let second = MockServer::start().await;
    {
        let _origin = Env::set(&[("RUSTAIN_TEST_AUTH_ORIGIN", Some(&second.uri()))]);
        login(&second, ACCESS_B, PROJECT_B, EMAIL_B).await;
    }
    let baseline = received(&server).await.len();
    {
        let _project = Env::set(&[("GOOGLE_CLOUD_PROJECT", Some("env-project-never-sent"))]);
        collect(&provider, vec![user("grant B")], options(MODEL)).await;
    }
    let sent = since(&server, baseline).await;
    let stream = hits(&sent, STREAM);
    assert_eq!(stream.len(), 1);
    assert_eq!(grant(stream[0]), (bearer(ACCESS_B), PROJECT_B.to_owned()));
    assert!(!String::from_utf8_lossy(&stream[0].body).contains("env-project-never-sent"));

    // A configured base on another origin locks the stored row.
    let foreign = MockServer::start().await;
    assert!(matches!(
        build_provider_for_config(PROVIDER, &config(Some(&foreign.uri()))),
        Err(ProviderError::AuthenticationFailed)
    ));
    assert!(received(&foreign).await.is_empty());

    for stub in [&server, &second, &foreign] {
        assert!(
            received(stub)
                .await
                .iter()
                .all(|request| !request.url.path().contains("chat/completions")),
            "no Chat Completions request for this kind"
        );
    }
}

// ---------------------------------------------------------------------------
// K06 — request bytes
// ---------------------------------------------------------------------------

const FIXTURE_BODY: &str = concat!(
    r#"{"project":"gemini-fixture-project","model":"gemini-3.1-pro-preview","request":{"contents":["#,
    r#"{"role":"user","parts":[{"text":"ctx\n\nLook"},{"inlineData":{"mimeType":"image/png","data":"AAAA"}}]},"#,
    r#"{"role":"model","parts":[{"text":"Checking."},"#,
    r#"{"functionCall":{"name":"read","args":{"path":"a"}},"thoughtSignature":"skip_thought_signature_validator"},"#,
    r#"{"functionCall":{"name":"grep","args":{"q":"x"}}}]},"#,
    r#"{"role":"user","parts":[{"functionResponse":{"name":"read","response":{"output":"a-out"}}},"#,
    r#"{"functionResponse":{"name":"grep","response":{"output":"b-out"}}}]},"#,
    r#"{"role":"user","parts":[{"text":"<stale-tool-result tool=\"tool\" id=\"ghost\">\nstale text\n</stale-tool-result>"}]},"#,
    r#"{"role":"model","parts":[{"functionCall":{"name":"run","args":{}},"thoughtSignature":"skip_thought_signature_validator"}]},"#,
    r#"{"role":"user","parts":[{"functionResponse":{"name":"run","response":{"error":"No result provided"}}}]},"#,
    r#"{"role":"user","parts":[{"text":"continue"}]}],"#,
    r#""systemInstruction":{"parts":[{"text":"Be terse."}]},"#,
    r#""tools":[{"functionDeclarations":[{"name":"lookup","description":"Look up an action.","parametersJsonSchema":{"properties":{"action":{"enum":["definition","references","code_actions"],"type":"string"}},"required":["action"],"type":"object"}}]}],"#,
    r#""generationConfig":{"maxOutputTokens":8192,"thinkingConfig":{"includeThoughts":true,"thinkingLevel":"LOW"}}}}"#,
);

fn assistant(content: &str, calls: &[(&str, &str, Value)]) -> Message {
    let mut message = user(content);
    message.role = MessageRole::Assistant;
    message.tool_uses = calls
        .iter()
        .map(|(id, name, input)| ToolUseMessage {
            id: (*id).into(),
            name: (*name).into(),
            input: input.clone(),
        })
        .collect();
    message
}

fn results(results: &[(&str, &str, bool)]) -> Message {
    let mut message = user("");
    message.tool_results = results
        .iter()
        .map(|(id, content, is_error)| ToolResultMessage {
            tool_use_id: (*id).into(),
            content: (*content).into(),
            is_error: *is_error,
        })
        .collect();
    message
}

#[tokio::test]
#[serial]
async fn k06_fixture_conversation_bytes_url_and_headers_are_exact() {
    let (server, _temp, _env) = signed_in().await;
    mount_stream(&server, text_reply("ok")).await;
    let mut first = user("Look");
    first.context_prefix = Some("ctx".into());
    first.images.push(ImageAttachment {
        media_type: "image/png".into(),
        data: "AAAA".into(),
    });
    let messages = vec![
        first,
        // Two calls without a replay payload plus a malformed one.
        assistant(
            "Checking.",
            &[
                ("call_a", "read", json!({"path": "a"})),
                ("call_b", "grep", json!({"q": "x"})),
                ("call_m", "bad name", json!({})),
            ],
        ),
        // Answered in reverse order; the malformed call's result goes with it.
        results(&[
            ("call_b", "b-out", false),
            ("call_a", "a-out", false),
            ("call_m", "m-out", true),
        ]),
        // An orphan result outside any open call window.
        results(&[("ghost", "stale text", false)]),
        // An unmatched call.
        assistant("", &[("call_c", "run", json!({}))]),
        user("continue"),
    ];
    let mut options = options(MODEL);
    options.system_prompt = "Be terse.".into();
    options.tools.push(ToolDefinition {
        name: "lookup".into(),
        description: "Look up an action.".into(),
        input_schema: json!({
            "type": "object",
            "properties": {"action": {"enum": ["definition", "references", "code_actions"]}},
            "required": ["action"]
        }),
        parallel_safe: false,
    });
    let baseline = received(&server).await.len();
    collect(&build(), messages, options).await;

    let sent = since(&server, baseline).await;
    let stream = hits(&sent, STREAM);
    assert_eq!(stream.len(), 1);
    let request = stream[0];
    assert_eq!(request.method.as_str(), "POST");
    // wiremock rebuilds origin-form targets as `http://localhost<target>`, so
    // the exact URL is the target plus the authority the client addressed.
    assert_eq!(
        format!("{}?{}", request.url.path(), request.url.query().unwrap()),
        STREAM_TARGET
    );
    assert_eq!(
        request.headers["host"],
        server.address().to_string().as_str()
    );
    assert_eq!(request.headers["authorization"], bearer(ACCESS).as_str());
    assert_eq!(request.headers["content-type"], "application/json");
    assert_eq!(request.headers["accept"], "text/event-stream");
    assert_eq!(request.headers["user-agent"], agent(MODEL).as_str());
    assert_eq!(request.headers["client-metadata"], CLIENT_METADATA);
    let body = String::from_utf8(request.body.clone()).unwrap();
    assert_eq!(body, FIXTURE_BODY);
    assert!(!body.contains("toolConfig"));
    let parsed: Value = serde_json::from_str(&body).unwrap();
    for content in parsed["request"]["contents"].as_array().unwrap() {
        for part in content["parts"].as_array().unwrap() {
            for key in ["functionCall", "functionResponse"] {
                if let Some(object) = part.get(key) {
                    assert!(object.get("id").is_none(), "{part}");
                }
            }
        }
    }
}

#[tokio::test]
#[serial]
async fn k06_generation_config_follows_each_bundled_class() {
    let table = [
        ("gemini-2.0-flash", r#"{"maxOutputTokens":8192}"#),
        ("gemini-2.5-flash", r#"{"maxOutputTokens":8192}"#),
        (
            "gemini-2.5-pro",
            r#"{"maxOutputTokens":9216,"thinkingConfig":{"includeThoughts":true,"thinkingBudget":1024}}"#,
        ),
        (
            "gemini-3-flash-preview",
            r#"{"maxOutputTokens":8192,"thinkingConfig":{"includeThoughts":true,"thinkingLevel":"MINIMAL"}}"#,
        ),
        (
            "gemini-3-pro-preview",
            r#"{"maxOutputTokens":8192,"thinkingConfig":{"includeThoughts":true,"thinkingLevel":"LOW"}}"#,
        ),
        (
            "gemini-3.1-flash-lite-preview",
            r#"{"maxOutputTokens":8192,"thinkingConfig":{"includeThoughts":true,"thinkingLevel":"MINIMAL"}}"#,
        ),
        (
            "gemini-3.1-pro-preview",
            r#"{"maxOutputTokens":8192,"thinkingConfig":{"includeThoughts":true,"thinkingLevel":"LOW"}}"#,
        ),
    ];
    let (server, _temp, _env) = signed_in().await;
    mount_stream(&server, text_reply("ok")).await;
    let provider = build();
    for (id, generation) in table {
        let text = format!("class {id}");
        let baseline = received(&server).await.len();
        collect(&provider, vec![user(&text)], options(id)).await;
        let sent = since(&server, baseline).await;
        let stream = hits(&sent, STREAM);
        assert_eq!(stream.len(), 1, "{id}");
        assert_eq!(
            String::from_utf8_lossy(&stream[0].body),
            format!(
                r#"{{"project":"{PROJECT}","model":"{id}","request":{{"contents":[{{"role":"user","parts":[{{"text":"{text}"}}]}}],"generationConfig":{generation}}}}}"#
            ),
            "{id}"
        );
        let body: Value = serde_json::from_slice(&stream[0].body).unwrap();
        assert_eq!(body["model"], id, "{id} wire id");
        assert_eq!(
            body["request"]["generationConfig"],
            serde_json::from_str::<Value>(generation).unwrap(),
            "{id}"
        );
        assert_eq!(stream[0].headers["user-agent"], agent(id).as_str());
    }
}

// ---------------------------------------------------------------------------
// K07 — stream mapping
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial]
async fn k07_parts_map_in_arrival_order_and_the_terminal_order_is_replay_usage_turn_complete() {
    let (server, _temp, _env) = signed_in().await;
    let provider = build();

    let order = sse(&[
        parts(json!([{"thought": true, "text": "plan"}])),
        parts(json!([{"text": "Hello"}])),
        json!({"response": {
            "candidates": [{
                "content": {"role": "model", "parts": [
                    {"functionCall": {"name": "lookup", "args": {"action": "definition"}, "id": "call-1"}}
                ]},
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 100,
                "cachedContentTokenCount": 30,
                "candidatesTokenCount": 20,
                "thoughtsTokenCount": 5,
                "totalTokenCount": 125
            }
        }}),
    ]);
    assert_eq!(
        case(&server, &provider, MODEL, "k07 order", order).await,
        [
            "thinking:plan".to_owned(),
            "text:Hello".to_owned(),
            r#"tool:call-1:lookup:{"action":"definition"}"#.to_owned(),
            replay(
                MODEL,
                json!([
                    {"thought": true, "text": "plan"},
                    {"text": "Hello"},
                    {"functionCall": {"name": "lookup", "args": {"action": "definition"}}, "callId": "call-1"}
                ])
            ),
            // input = prompt − cacheRead; output = candidates + thoughts.
            "usage:70:25:None:Some(30):Some(5)".to_owned(),
            "done:ToolUse".to_owned(),
        ]
    );

    // MAX_TOKENS → MaxTokens; a missing prompt count is total − candidates − thoughts.
    let length = sse(&[json!({"response": {
        "candidates": [{
            "content": {"role": "model", "parts": [{"text": "partial"}]},
            "finishReason": "MAX_TOKENS"
        }],
        "usageMetadata": {"candidatesTokenCount": 10, "totalTokenCount": 50}
    }})]);
    assert_eq!(
        case(&server, &provider, MODEL, "k07 max tokens", length).await,
        [
            "text:partial".to_owned(),
            replay(MODEL, json!([{"text": "partial"}])),
            "usage:40:10:None:None:None".to_owned(),
            "done:MaxTokens".to_owned(),
        ]
    );
}

fn generated(id: &str, name: &str) -> bool {
    let Some(rest) = id.strip_prefix(&format!("{name}_")) else {
        return false;
    };
    let Some((millis, counter)) = rest.split_once('_') else {
        return false;
    };
    [millis, counter]
        .iter()
        .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
}

#[tokio::test]
#[serial]
async fn k07_missing_and_duplicate_tool_ids_are_generated() {
    let (server, _temp, _env) = signed_in().await;
    Mock::given(method("POST"))
        .and(path(STREAM))
        .respond_with(sse_ok(sse(&[json!({"response": {"candidates": [{
            "content": {"role": "model", "parts": [
                {"functionCall": {"name": "lookup"}},
                {"functionCall": {"name": "lookup", "args": {"n": 1}, "id": "dup"}},
                {"functionCall": {"name": "grep", "args": {"n": 2}, "id": "dup"}},
                {"functionCall": {"args": {"n": 3}}}
            ]},
            "finishReason": "STOP"
        }]}})])))
        .mount(&server)
        .await;
    let chunks = collect(&build(), vec![user("ids")], options(MODEL)).await;
    let calls: Vec<(String, String, Value)> = chunks
        .iter()
        .filter_map(|chunk| match chunk {
            StreamChunk::ToolUse { id, name, input } => {
                Some((id.clone(), name.clone(), input.clone()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(calls.len(), 4);
    assert!(generated(&calls[0].0, "lookup"), "{}", calls[0].0);
    assert_eq!(calls[1].0, "dup");
    assert!(generated(&calls[2].0, "grep"), "{}", calls[2].0);
    assert!(generated(&calls[3].0, "tool"), "{}", calls[3].0);
    assert_ne!(calls[0].0, calls[2].0);
    assert_ne!(calls[2].0, calls[3].0);
    let names: Vec<_> = calls.iter().map(|call| call.1.as_str()).collect();
    assert_eq!(names, ["lookup", "lookup", "grep", ""]);
    let inputs: Vec<_> = calls.iter().map(|call| call.2.clone()).collect();
    assert_eq!(
        inputs,
        [json!({}), json!({"n": 1}), json!({"n": 2}), json!({"n": 3})]
    );
    // The replay carries the same ids, in order, after the calls.
    let StreamChunk::ProviderReplay { item } = &chunks[4] else {
        panic!("{:?}", describe(&chunks));
    };
    let ids: Vec<_> = item.payload["gemini_parts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|part| part["callId"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        ids,
        calls.iter().map(|call| call.0.clone()).collect::<Vec<_>>()
    );
    assert_eq!(describe(&chunks[5..]), ["done:ToolUse"]);
}

#[tokio::test]
#[serial]
async fn k07_failure_finishes_and_frames_end_with_their_text_and_no_turn_complete() {
    let (server, _temp, _env) = signed_in().await;
    let provider = build();

    // Any other finish reason fails, even with calls.
    let safety = sse(&[json!({"response": {"candidates": [{
        "content": {"role": "model", "parts": [
            {"functionCall": {"name": "lookup", "args": {"action": "definition"}, "id": "safety-call"}}
        ]},
        "finishReason": "SAFETY"
    }]}})]);
    assert_eq!(
        case(&server, &provider, MODEL, "k07 safety", safety).await,
        [
            r#"tool:safety-call:lookup:{"action":"definition"}"#,
            "error:Generation failed with finish reason: SAFETY",
        ]
    );

    let blocked = sse(&[json!({"response": {"promptFeedback": {
        "blockReason": "PROHIBITED_CONTENT",
        "blockReasonMessage": "Prompt was blocked"
    }}})]);
    assert_eq!(
        case(&server, &provider, MODEL, "k07 blocked", blocked).await,
        ["error:Request blocked by Google (PROHIBITED_CONTENT): Prompt was blocked"]
    );

    let inband = sse(&[
        parts(json!([{"text": "early"}])),
        json!({"error": {"code": 503, "message": "Backend overloaded", "status": "UNAVAILABLE"}}),
    ]);
    assert_eq!(
        case(&server, &provider, MODEL, "k07 inband", inband).await,
        [
            "text:early",
            "error:Cloud Code Assist stream error: Backend overloaded"
        ]
    );
    let status_only = sse(&[json!({"error": {"status": "UNAVAILABLE"}})]);
    assert_eq!(
        case(&server, &provider, MODEL, "k07 status only", status_only).await,
        ["error:Cloud Code Assist stream error: UNAVAILABLE"]
    );

    // A malformed frame that is not the stream's tail.
    let malformed = format!(
        "{}data: {{\"response\": nope\n\n{}",
        sse(&[parts(json!([{"text": "ok"}]))]),
        sse(&[finish("STOP")])
    );
    assert_eq!(
        case(&server, &provider, MODEL, "k07 malformed", malformed).await,
        [
            "text:ok",
            "error:Cloud Code Assist stream returned invalid JSON"
        ]
    );

    // A container-shaped tail cut at EOF ends cleanly, then the missing finish fails.
    let truncated = format!(
        "{}data: {{\"response\":{{\"candidates\":[",
        sse(&[parts(json!([{"text": "partial"}]))])
    );
    assert_eq!(
        case(&server, &provider, MODEL, "k07 truncated", truncated).await,
        ["text:partial".to_owned(), format!("error:{NO_FINISH}")]
    );

    let unfinished = sse(&[parts(json!([{"text": "hello"}]))]);
    assert_eq!(
        case(&server, &provider, MODEL, "k07 unfinished", unfinished).await,
        ["text:hello".to_owned(), format!("error:{NO_FINISH}")]
    );
}

#[tokio::test]
#[serial]
async fn k07_flash_planning_leak_is_stripped_on_flash_rows_only() {
    let (server, _temp, _env) = signed_in().await;
    let provider = build();
    let leak = || text_reply(r#"{"thought":"plan"}Answer"#);
    assert_eq!(
        case(
            &server,
            &provider,
            "gemini-2.5-flash",
            "k07 leak flash",
            leak()
        )
        .await,
        [
            "text:Answer".to_owned(),
            replay("gemini-2.5-flash", json!([{"text": "Answer"}])),
            "done:EndTurn".to_owned(),
        ]
    );
    assert_eq!(
        case(&server, &provider, "gemini-2.5-pro", "k07 leak pro", leak()).await,
        [
            r#"text:{"thought":"plan"}Answer"#.to_owned(),
            replay(
                "gemini-2.5-pro",
                json!([{"text": r#"{"thought":"plan"}Answer"#}])
            ),
            "done:EndTurn".to_owned(),
        ]
    );
}

// ---------------------------------------------------------------------------
// K08 — resilience
// ---------------------------------------------------------------------------

const OVERLOADED: &str = r#"{"error":{"message":"overloaded"}}"#;

async fn mount_status(
    server: &MockServer,
    marker: &str,
    template: ResponseTemplate,
    times: Option<u64>,
) {
    let mock = Mock::given(method("POST"))
        .and(path(STREAM))
        .and(body_string_contains(marker))
        .respond_with(template)
        .with_priority(1);
    match times {
        Some(times) => mock.up_to_n_times(times).mount(server).await,
        None => mock.mount(server).await,
    }
}

#[tokio::test]
#[serial]
async fn k08_retryable_statuses_get_four_attempts() {
    let (server, _temp, _env) = signed_in().await;
    let provider = build();

    let marker = "k08 transient";
    mount_status(
        &server,
        marker,
        ResponseTemplate::new(503).set_body_string(OVERLOADED),
        Some(2),
    )
    .await;
    let chunks = case(&server, &provider, MODEL, marker, text_reply("recovered")).await;
    assert_eq!(
        chunks,
        [
            "text:recovered".to_owned(),
            replay(MODEL, json!([{"text": "recovered"}])),
            "done:EndTurn".to_owned(),
        ]
    );
    assert_eq!(posts(&received(&server).await, marker).len(), 3);

    let marker = "k08 exhausted";
    mount_status(
        &server,
        marker,
        ResponseTemplate::new(503).set_body_string(OVERLOADED),
        None,
    )
    .await;
    match refused(&provider, vec![user(marker)], options(MODEL)).await {
        ProviderError::ConnectionFailed(text) => {
            assert_eq!(text, "Cloud Code Assist API error (503): overloaded");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(posts(&received(&server).await, marker).len(), 4);
}

#[tokio::test]
#[serial]
async fn k08_rate_limit_hint_above_the_budget_returns_after_one_post() {
    let (server, _temp, _env) = signed_in().await;
    let marker = "k08 rate";
    mount_status(
        &server,
        marker,
        ResponseTemplate::new(429)
            .insert_header("retry-after", "400")
            .set_body_string(r#"{"error":{"message":"quota"}}"#),
        None,
    )
    .await;
    assert!(matches!(
        refused(&build(), vec![user(marker)], options(MODEL)).await,
        ProviderError::RateLimited {
            retry_after_ms: Some(400_000)
        }
    ));
    assert_eq!(posts(&received(&server).await, marker).len(), 1);
}

#[tokio::test]
#[serial]
async fn k08_empty_responses_retry_twice_and_thought_only_is_not_rerun() {
    let (server, _temp, _env) = signed_in().await;
    let provider = build();
    // Whitespace-only text with STOP is empty; it is never forwarded.
    let empty = || text_reply("  \n");

    let marker = "k08 empty twice";
    mount_status(&server, marker, sse_ok(empty()), Some(2)).await;
    assert_eq!(
        case(&server, &provider, MODEL, marker, text_reply("content")).await,
        [
            "text:content".to_owned(),
            replay(MODEL, json!([{"text": "content"}])),
            "done:EndTurn".to_owned(),
        ]
    );
    assert_eq!(posts(&received(&server).await, marker).len(), 3);

    let marker = "k08 empty always";
    assert_eq!(
        case(&server, &provider, MODEL, marker, empty()).await,
        ["error:Cloud Code Assist API returned an empty response"]
    );
    assert_eq!(posts(&received(&server).await, marker).len(), 3);

    let marker = "k08 thought only";
    let thought = sse(&[json!({"response": {"candidates": [{
        "content": {"role": "model", "parts": [{"thought": true, "text": "pondering"}]},
        "finishReason": "STOP"
    }]}})]);
    assert_eq!(
        case(&server, &provider, MODEL, marker, thought).await,
        [
            "thinking:pondering",
            "error:Cloud Code Assist API returned a thought-only response without final output",
        ]
    );
    assert_eq!(posts(&received(&server).await, marker).len(), 1);
}

#[tokio::test]
#[serial]
async fn k08_pre_response_timeout_is_not_retried() {
    let (_server, _temp, _env) = signed_in().await;
    let (listener, origin) = raw_listener().await;
    // gemini-2.5-flash's 60 000 ms watchdog becomes 60 ms.
    let _route = Env::set(&[
        ("RUSTAIN_TEST_AUTH_ORIGIN", Some(&origin)),
        ("RUSTAIN_TEST_GEMINI_CLI_RETRY_SCALE", Some("0.001")),
    ]);
    let provider = build();
    let call = tokio::spawn(async move {
        provider
            .stream_completion(vec![user("silent")], options("gemini-2.5-flash"))
            .await
            .err()
    });
    let (mut socket, _) = within(listener.accept()).await.unwrap();
    let request = read_request(&mut socket).await;
    assert_eq!(
        (request.method.as_str(), request.target.as_str()),
        ("POST", STREAM_TARGET)
    );
    match within(call).await.unwrap() {
        Some(ProviderError::ConnectionFailed(text)) => assert_eq!(text, FIRST_EVENT_TIMEOUT),
        other => panic!("{other:?}"),
    }
    assert_no_connection(&listener);
    drop(socket);
}

#[tokio::test]
#[serial]
async fn k08_first_event_timeout_is_not_retried() {
    let (_server, _temp, _env) = signed_in().await;
    let (listener, origin) = raw_listener().await;
    // 60 000 ms → 3 s: headers must beat the pre-response guard, then the
    // fresh first-event allowance expires.
    let _route = Env::set(&[
        ("RUSTAIN_TEST_AUTH_ORIGIN", Some(&origin)),
        ("RUSTAIN_TEST_GEMINI_CLI_RETRY_SCALE", Some("0.05")),
    ]);
    let provider = build();
    let call = tokio::spawn(async move {
        match provider
            .stream_completion(vec![user("headers only")], options("gemini-2.5-flash"))
            .await
        {
            Ok(stream) => stream.collect::<Vec<_>>().await,
            Err(error) => panic!("headers arrived, so the stream opens: {error:?}"),
        }
    });
    let (mut socket, _) = within(listener.accept()).await.unwrap();
    read_request(&mut socket).await;
    stream_headers(&mut socket).await;
    let chunks = within(call).await.unwrap();
    assert_eq!(describe(&chunks), [format!("error:{FIRST_EVENT_TIMEOUT}")]);
    assert_no_connection(&listener);
    drop(socket);
}

#[tokio::test]
#[serial]
async fn k08_dropping_the_stream_closes_the_connection() {
    let (_server, _temp, _env) = signed_in().await;
    let (listener, origin) = raw_listener().await;
    let _route = Env::set(&[("RUSTAIN_TEST_AUTH_ORIGIN", Some(&origin))]);
    let provider = build();
    let (dropped, dropped_rx) = oneshot::channel::<()>();
    let stub = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        stream_headers(&mut socket).await;
        write_chunk(
            &mut socket,
            sse(&[parts(json!([{"text": "first"}]))]).as_bytes(),
        )
        .await;
        dropped_rx.await.unwrap();
        assert_peer_eof(&mut socket).await;
        assert_no_connection(&listener);
    });
    let mut stream =
        match within(provider.stream_completion(vec![user("drop")], options(MODEL))).await {
            Ok(stream) => stream,
            Err(error) => panic!("{error:?}"),
        };
    let first = within(stream.next()).await.unwrap();
    assert_eq!(describe(&[first]), ["text:first"]);
    drop(stream);
    dropped.send(()).unwrap();
    within(stub).await.unwrap();
}

#[tokio::test]
#[serial]
async fn k08_abort_during_backoff_sends_no_further_post() {
    use tokio::io::AsyncWriteExt;
    let (_server, _temp, _env) = signed_in().await;
    let (listener, origin) = raw_listener().await;
    // Unscaled: the hinted 100 s backoff is pending when the abort lands.
    let _route = Env::set(&[
        ("RUSTAIN_TEST_AUTH_ORIGIN", Some(&origin)),
        ("RUSTAIN_TEST_GEMINI_CLI_RETRY_SCALE", Some("1")),
    ]);
    let provider = build();
    let call = {
        let provider = provider.clone();
        tokio::spawn(async move {
            provider
                .stream_completion(vec![user("backoff")], options(MODEL))
                .await
                .err()
        })
    };
    let (mut socket, _) = within(listener.accept()).await.unwrap();
    read_request(&mut socket).await;
    socket
        .write_all(b"HTTP/1.1 503 Service Unavailable\r\nRetry-After: 100\r\nContent-Length: 4\r\nConnection: close\r\n\r\nbusy")
        .await
        .unwrap();
    // The client closes only after it has read the whole 503, so the abort lands
    // at the backoff wait, never inside the first exchange.
    assert_peer_eof(&mut socket).await;
    provider.abort().await.unwrap();
    match within(call).await.unwrap() {
        Some(ProviderError::Cancelled) => {}
        other => panic!("{other:?}"),
    }
    assert_no_connection(&listener);
}

#[tokio::test]
#[serial]
async fn k08_validation_required_names_the_account_and_url() {
    let (server, _temp, _env) = signed_in().await;
    let marker = "k08 validation";
    mount_status(
        &server,
        marker,
        ResponseTemplate::new(403).set_body_string(
            r#"{"error":{"code":403,"message":"Verify","details":[{"reason":"VALIDATION_REQUIRED","metadata":{"validation_url":"https://accounts.google.com/verify-fixture"}}]}}"#,
        ),
        None,
    )
    .await;
    match refused(&build(), vec![user(marker)], options(MODEL)).await {
        ProviderError::Other(text) => assert_eq!(
            text,
            format!(
                "Account verification required for {EMAIL}. Visit https://accounts.google.com/verify-fixture to continue, then retry your request."
            )
        ),
        other => panic!("{other:?}"),
    }
    assert_eq!(posts(&received(&server).await, marker).len(), 1);
}

#[tokio::test]
#[serial]
async fn k08_redirect_is_never_followed() {
    let (server, _temp, _env) = signed_in().await;
    let elsewhere = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(sse_ok(text_reply("followed")))
        .mount(&elsewhere)
        .await;
    let marker = "k08 redirect";
    mount_status(
        &server,
        marker,
        ResponseTemplate::new(307)
            .insert_header("location", format!("{}{STREAM_TARGET}", elsewhere.uri()))
            .set_body_string("moved"),
        None,
    )
    .await;
    match refused(&build(), vec![user(marker)], options(MODEL)).await {
        ProviderError::Other(text) => assert_eq!(text, "Cloud Code Assist API error (307): moved"),
        other => panic!("{other:?}"),
    }
    assert_eq!(posts(&received(&server).await, marker).len(), 1);
    assert!(received(&elsewhere).await.is_empty());
}

// ---------------------------------------------------------------------------
// Plain: streaming details, error texts, probes
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial]
async fn plain_signature_only_parts_sign_the_open_block_or_are_dropped() {
    let (server, _temp, _env) = signed_in().await;
    let body = sse(&[
        // No open block: dropped.
        parts(json!([{"text": "", "thoughtSignature": "U0lHMA=="}])),
        parts(json!([{"text": "V"}])),
        // Signs the open text block.
        parts(json!([{"text": "", "thoughtSignature": "U0lHMQ=="}])),
        finish("STOP"),
    ]);
    assert_eq!(
        case(&server, &build(), MODEL, "plain signature", body).await,
        [
            "text:V".to_owned(),
            replay(
                MODEL,
                json!([{"text": "V", "thoughtSignature": "U0lHMQ=="}])
            ),
            "done:EndTurn".to_owned(),
        ]
    );
}

#[tokio::test]
#[serial]
async fn plain_done_marker_ends_the_stream() {
    let (server, _temp, _env) = signed_in().await;
    let body = format!(
        "{}data: [DONE]\n\ndata: not json\n\n",
        text_reply("before done")
    );
    assert_eq!(
        case(&server, &build(), MODEL, "plain done", body).await,
        [
            "text:before done".to_owned(),
            replay(MODEL, json!([{"text": "before done"}])),
            "done:EndTurn".to_owned(),
        ]
    );
}

#[tokio::test]
#[serial]
async fn plain_403_maps_to_other_and_the_bearer_is_redacted() {
    let (server, _temp, _env) = signed_in().await;
    let provider = build();
    let cases = [
        (
            "plain forbidden",
            403u16,
            r#"{"error":{"message":"denied"}}"#.to_owned(),
            "Cloud Code Assist API error (403): denied".to_owned(),
        ),
        (
            "plain echo json",
            400,
            format!(r#"{{"error":{{"message":"token {ACCESS} was rejected"}}}}"#),
            "Cloud Code Assist API error (400): token [redacted] was rejected".to_owned(),
        ),
        (
            "plain echo text",
            400,
            format!("bad bearer {ACCESS}"),
            "Cloud Code Assist API error (400): bad bearer [redacted]".to_owned(),
        ),
    ];
    for (marker, status, body, expected) in cases {
        mount_status(
            &server,
            marker,
            ResponseTemplate::new(status).set_body_string(body),
            None,
        )
        .await;
        match refused(&provider, vec![user(marker)], options(MODEL)).await {
            ProviderError::Other(text) => assert_eq!(text, expected),
            other => panic!("{marker}: {other:?}"),
        }
        assert_eq!(posts(&received(&server).await, marker).len(), 1, "{marker}");
    }

    // An in-band error frame echoing the bearer is redacted as well.
    let echoed = sse(&[json!({"error": {"message": format!("bearer {ACCESS} expired")}})]);
    assert_eq!(
        case(&server, &provider, MODEL, "plain echo frame", echoed).await,
        ["error:Cloud Code Assist stream error: bearer [redacted] expired"]
    );
}

fn assert_probe_request(request: &Request, token: &str) {
    assert_eq!(request.method.as_str(), "POST");
    assert_eq!(String::from_utf8_lossy(&request.body), FREE_LOAD_BODY);
    assert_eq!(request.headers["authorization"], bearer(token).as_str());
    assert_eq!(request.headers["content-type"], "application/json");
    assert_eq!(
        request.headers["user-agent"],
        agent("gemini-3.1-pro-preview").as_str()
    );
    assert_eq!(request.headers["client-metadata"], CLIENT_METADATA);
    // reqwest's default `accept: */*` may be present; the probe never asks
    // for the event stream.
    assert!(
        request
            .headers
            .get("accept")
            .is_none_or(|value| value != "text/event-stream"),
        "{:?}",
        request.headers.get("accept")
    );
}

#[tokio::test]
#[serial]
async fn plain_probes_map_load_code_assist_statuses() {
    let (server, _temp, _env) = signed_in().await;
    let provider = build();

    // The login's loadCodeAssist stub answers 200.
    let baseline = received(&server).await.len();
    within(provider.health_check()).await.unwrap();
    within(provider.connectivity_probe()).await.unwrap();
    let sent = since(&server, baseline).await;
    let load = hits(&sent, LOAD);
    assert_eq!(load.len(), 2);
    for request in load {
        assert_eq!(request.url.path(), LOAD);
        assert_eq!(request.url.query(), None);
        assert_eq!(
            request.headers["host"],
            server.address().to_string().as_str()
        );
        assert_probe_request(request, ACCESS);
    }
    assert!(hits(&sent, STREAM).is_empty());

    for status in [403u16, 404, 405] {
        server.reset().await;
        Mock::given(method("POST"))
            .and(path(LOAD))
            .respond_with(ResponseTemplate::new(status))
            .mount(&server)
            .await;
        let health = within(provider.health_check()).await;
        let probe = within(provider.connectivity_probe()).await;
        match status {
            403 => {
                assert!(matches!(health, Err(ProviderError::AuthenticationFailed)));
                assert!(matches!(probe, Err(ProviderError::AuthenticationFailed)));
            }
            _ => {
                assert!(
                    matches!(health, Err(ProviderError::EndpointUnsupported(s)) if s == status),
                    "{status}: {health:?}"
                );
                match probe {
                    Err(ProviderError::Other(text)) => assert_eq!(
                        text,
                        format!("Cloud Code Assist loadCodeAssist HTTP {status}")
                    ),
                    other => panic!("{status}: {other:?}"),
                }
            }
        }
        assert_eq!(
            hits(&received(&server).await, LOAD).len(),
            2,
            "{status}: one status-only POST per probe"
        );
    }
}

#[tokio::test]
#[serial]
async fn plain_probe_401_recovers_once_then_fails_authentication() {
    let (server, _temp, _env) = signed_in().await;
    mount_refresh(&server).await;
    Mock::given(method("POST"))
        .and(path(LOAD))
        .respond_with(ResponseTemplate::new(401))
        .with_priority(1)
        .mount(&server)
        .await;
    let provider = build();
    let baseline = received(&server).await.len();
    assert!(matches!(
        within(provider.health_check()).await,
        Err(ProviderError::AuthenticationFailed)
    ));
    let sent = since(&server, baseline).await;
    let token = hits(&sent, TOKEN);
    assert_eq!(token.len(), 1);
    assert_eq!(String::from_utf8_lossy(&token[0].body), refresh_form());
    let load = hits(&sent, LOAD);
    assert_eq!(load.len(), 2);
    assert_probe_request(load[0], ACCESS);
    assert_probe_request(load[1], REFRESHED);
    assert!(matches!(
        within(provider.connectivity_probe()).await,
        Err(ProviderError::AuthenticationFailed)
    ));
}

#[tokio::test]
#[serial]
async fn plain_probe_transport_error_is_offline() {
    let (_server, _temp, _env) = signed_in().await;
    let dead = {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        format!("http://{}", listener.local_addr().unwrap())
    };
    let _route = Env::set(&[("RUSTAIN_TEST_AUTH_ORIGIN", Some(&dead))]);
    let provider = build();
    match within(provider.connectivity_probe()).await {
        Err(ProviderError::Offline(_)) => {}
        other => panic!("{other:?}"),
    }
}
