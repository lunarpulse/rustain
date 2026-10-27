use super::flows::login_oauth;
use futures::StreamExt;
use rustain::adapters::anthropic::claude_code::{ClaudeCodeIdentity, pin_identity_for_test};
use rustain::adapters::auth_store::FileAuthStore;
use rustain::domain::models::{AuthHeaderScheme, CompletionOptions, Message, ProviderConfig};
use rustain::domain::models::{MessageRole, StreamChunk, ToolDefinition, ToolUseMessage};
use rustain::domain::ports::AuthStorePort;
use rustain::infrastructure::provider_factory::build_provider_for_config;
use serial_test::serial;
use std::ffi::OsString;
use std::sync::Arc;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

fn message(text: &str) -> Message {
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
fn pin() {
    pin_identity_for_test(ClaudeCodeIdentity {
        version: "2.1.280".into(),
        install_id: "install-test".into(),
        session: "session-test".into(),
    });
}
fn identity() -> serde_json::Value {
    serde_json::json!({"account":{"uuid":"acct","email_address":"me@example.test"},"organization":{"uuid":"org","name":"Example"}})
}
async fn login(server: &MockServer, expires: i64) {
    let store: Arc<dyn AuthStorePort> = Arc::new(FileAuthStore::new());
    login_oauth(
        server,
        &store,
        chrono::Utc::now().timestamp_millis(),
        expires,
        identity(),
    )
    .await;
}
const SSE: &str = "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call\",\"name\":\"_read\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{}\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n";
async fn endpoints(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_string(SSE))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200))
        .mount(server)
        .await;
}
fn headers(request: &wiremock::Request, tools: bool, token: &str, version: &str) {
    for (name, value) in [
        ("accept", "application/json"),
        ("content-type", "application/json"),
        ("x-claude-code-session-id", "session-test"),
        ("x-stainless-arch", "x64"),
        ("x-stainless-os", "Linux"),
        ("x-stainless-lang", "js"),
        ("x-stainless-package-version", "0.112.1"),
        ("x-stainless-retry-count", "0"),
        ("x-stainless-runtime", "node"),
        ("x-stainless-runtime-version", "v26.3.0"),
        ("x-stainless-timeout", "600"),
        ("anthropic-dangerous-direct-browser-access", "true"),
        ("anthropic-version", "2023-06-01"),
        ("x-app", "cli"),
        ("connection", "keep-alive"),
    ] {
        assert_eq!(request.headers[name], value, "{name}");
    }
    assert_eq!(request.headers["authorization"], format!("Bearer {token}"));
    assert_eq!(
        request.headers["user-agent"],
        format!("claude-cli/{version} (external, cli)")
    );
    assert_eq!(
        request.headers["anthropic-beta"],
        if tools {
            "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,fallback-credit-2026-06-01"
        } else {
            "oauth-2025-04-20,interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,structured-outputs-2025-12-15"
        }
    );
    assert!(request.headers.get("x-api-key").is_none());
    // reqwest appends headers, so a duplicated profile header would show up here.
    assert_eq!(request.headers.get_all("content-type").iter().count(), 1);
}

struct Env(Vec<(&'static str, Option<OsString>)>);
impl Env {
    fn set(values: &[(&'static str, Option<&str>)]) -> Self {
        let old = values
            .iter()
            .map(|(key, value)| {
                let old = std::env::var_os(key);
                unsafe {
                    match value {
                        Some(value) => std::env::set_var(key, value),
                        None => std::env::remove_var(key),
                    }
                }
                (*key, old)
            })
            .collect();
        rustain::adapters::anthropic::claude_code::reset_for_test();
        Self(old)
    }
}
impl Drop for Env {
    fn drop(&mut self) {
        rustain::adapters::anthropic::claude_code::reset_for_test();
        for (key, value) in self.0.drain(..).rev() {
            unsafe {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}
fn config(base: &str, env: &str) -> ProviderConfig {
    ProviderConfig {
        provider_id: "anthropic".into(),
        model_id: "test-model".into(),
        api_key_env: env.into(),
        auth_header: None,
        enabled: true,
        kind: Some("anthropic".into()),
        base_url: Some(base.into()),
        context_window: None,
        supports_tools: None,
        discover_models: false,
        model_filter: vec!["*".into()],
        cache_ttl_seconds: 3600,
    }
}
fn options() -> CompletionOptions {
    CompletionOptions {
        session_id: None,
        model: "test-model".into(),
        max_tokens: 32,
        system_prompt: "Be concise.".into(),
        temperature: Some(0.5),
        tools: vec![],
    }
}

#[tokio::test]
#[serial]
async fn k10_custom_env_defaults_to_api_key_and_explicit_scheme_wins() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let _env = Env::set(&[
        ("RUSTAIN_DATA_DIR", Some(dir.path().to_str().unwrap())),
        ("GW_TOKEN", Some("gateway-key")),
    ]);
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    for (scheme, header) in [
        (None, "x-api-key"),
        (Some(AuthHeaderScheme::Bearer), "authorization"),
    ] {
        let mut cfg = config(&server.uri(), "GW_TOKEN");
        cfg.auth_header = scheme;
        build_provider_for_config("anthropic", &cfg)
            .unwrap()
            .connectivity_probe()
            .await
            .unwrap();
        let requests = server.received_requests().await.unwrap();
        let request = requests.last().unwrap();
        assert_eq!(
            request.headers.get(header).unwrap().to_str().unwrap(),
            if scheme.is_some() {
                "Bearer gateway-key"
            } else {
                "gateway-key"
            }
        );
        assert!(request.headers.get("anthropic-beta").is_none());
    }
}

#[tokio::test]
#[serial]
async fn k09_two_factory_adapters_share_one_stale_refresh_flight() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let _env = Env::set(&[
        ("RUSTAIN_DATA_DIR", Some(dir.path().to_str().unwrap())),
        ("RUSTAIN_TEST_AUTH_ORIGIN", Some(&server.uri())),
        ("ANTHROPIC_API_KEY", None),
        ("ANTHROPIC_AUTH_TOKEN", None),
    ]);
    pin();
    login(&server, 0).await;
    server.reset().await;
    endpoints(&server).await;
    // Transient results cannot be adopted from disk: only a shared flight can
    // satisfy the two overlapping requests with exactly one exchange.
    Mock::given(method("POST"))
        .and(path("/v1/oauth/token"))
        .respond_with(ResponseTemplate::new(503))
        .expect(1)
        .mount(&server)
        .await;
    let cfg = config(&server.uri(), "ANTHROPIC_API_KEY");
    let a = build_provider_for_config("anthropic", &cfg).unwrap();
    let b = build_provider_for_config("anthropic", &cfg).unwrap();
    let (a, b) = tokio::join!(
        a.stream_completion(vec![message("hello")], options()),
        b.stream_completion(vec![message("hello")], options())
    );
    assert!(matches!(
        a,
        Err(rustain::domain::errors::ProviderError::ConnectionFailed(_))
    ));
    assert!(matches!(
        b,
        Err(rustain::domain::errors::ProviderError::ConnectionFailed(_))
    ));
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.url.path() == "/v1/oauth/token")
            .count(),
        1
    );
    assert!(!requests.iter().any(|r| r.url.path() == "/v1/messages"));
}

#[tokio::test]
#[serial]
async fn k10_cc_env_explicit_override_and_stored_oauth_origin_guard() {
    let official = MockServer::start().await;
    let gateway = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let _env = Env::set(&[
        ("RUSTAIN_DATA_DIR", Some(dir.path().to_str().unwrap())),
        ("RUSTAIN_TEST_AUTH_ORIGIN", Some(&official.uri())),
        ("ANTHROPIC_AUTH_TOKEN", Some("operator-token")),
    ]);
    endpoints(&official).await;
    endpoints(&gateway).await;
    let mut cfg = config(&gateway.uri(), "ANTHROPIC_AUTH_TOKEN");
    for (scheme, header, value) in [
        (None, "authorization", "Bearer operator-token"),
        (
            Some(AuthHeaderScheme::ApiKey),
            "x-api-key",
            "operator-token",
        ),
    ] {
        cfg.auth_header = scheme;
        build_provider_for_config("anthropic", &cfg)
            .unwrap()
            .connectivity_probe()
            .await
            .unwrap();
        assert_eq!(
            gateway
                .received_requests()
                .await
                .unwrap()
                .last()
                .unwrap()
                .headers[header],
            value
        );
    }
    login(&official, 3600).await;
    cfg.base_url = Some(official.uri().into());
    pin();
    build_provider_for_config("anthropic", &cfg)
        .unwrap()
        .connectivity_probe()
        .await
        .unwrap();
    assert_eq!(
        official
            .received_requests()
            .await
            .unwrap()
            .last()
            .unwrap()
            .headers["authorization"],
        "Bearer LOGIN-ACCESS-CANARY"
    );
    cfg.base_url = Some(gateway.uri().into());
    build_provider_for_config("anthropic", &cfg)
        .unwrap()
        .connectivity_probe()
        .await
        .unwrap();
    assert_eq!(
        gateway
            .received_requests()
            .await
            .unwrap()
            .last()
            .unwrap()
            .headers["x-api-key"],
        "operator-token"
    );
    assert!(!gateway.received_requests().await.unwrap().iter().any(|r| {
        r.headers
            .get("authorization")
            .is_some_and(|h| h == "Bearer LOGIN-ACCESS-CANARY")
    }));
    let no_env = config(&gateway.uri(), "");
    assert!(matches!(
        build_provider_for_config("anthropic", &no_env),
        Err(rustain::domain::errors::ProviderError::AuthenticationFailed)
    ));
}

#[tokio::test]
#[serial]
async fn k11_all_entrypoints_full_profile_and_profile_only_stream_strip() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let _env = Env::set(&[
        ("RUSTAIN_DATA_DIR", Some(dir.path().to_str().unwrap())),
        ("RUSTAIN_TEST_AUTH_ORIGIN", Some(&server.uri())),
        ("ANTHROPIC_API_KEY", None),
    ]);
    pin();
    login(&server, 3600).await;
    server.reset().await;
    endpoints(&server).await;
    let cfg = config(&server.uri(), "ANTHROPIC_API_KEY");
    let provider = build_provider_for_config("anthropic", &cfg).unwrap();
    let mut opts = options();
    opts.tools = [
        "read",
        "WEB_SEARCH",
        "code_execution",
        "text_editor",
        "computer",
        "_custom",
    ]
    .iter()
    .map(|name| ToolDefinition {
        name: (*name).into(),
        description: "tool".into(),
        input_schema: serde_json::json!({"type":"object"}),
        parallel_safe: false,
    })
    .collect();
    let mut replay = message("");
    replay.role = MessageRole::Assistant;
    replay.tool_uses = vec![ToolUseMessage {
        id: "old".into(),
        name: "read".into(),
        input: serde_json::json!({}),
    }];
    let chunks = provider
        .stream_completion(vec![message("hello"), replay], opts)
        .await
        .unwrap()
        .collect::<Vec<_>>()
        .await;
    assert!(
        chunks
            .iter()
            .any(|c| matches!(c, StreamChunk::ToolUse { name, .. } if name == "read"))
    );
    provider.health_check().await.unwrap();
    provider.connectivity_probe().await.unwrap();
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 3);
    for (i, request) in requests.iter().enumerate() {
        headers(request, i == 0, "LOGIN-ACCESS-CANARY", "2.1.280");
        if i == 2 {
            assert_eq!(request.url.path(), "/v1/models");
            assert!(request.url.query().is_none());
            assert!(request.body.is_empty());
            continue;
        }
        assert_eq!(request.url.path(), "/v1/messages");
        assert_eq!(request.url.query(), Some("beta=true"));
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        assert!(
            body["system"][0]["text"]
                .as_str()
                .unwrap()
                .starts_with(if i == 0 {
                    "x-anthropic-billing-header: cc_version=2.1.280.790;"
                } else {
                    "x-anthropic-billing-header: cc_version=2.1.280.d7b;"
                })
        );
        assert_eq!(
            body["system"][1]["text"],
            "You are Claude Code, Anthropic's official CLI for Claude."
        );
        let system = body["system"].as_array().unwrap();
        assert_eq!(
            system.last().unwrap()["cache_control"],
            serde_json::json!({"type":"ephemeral","ttl":"1h"})
        );
        assert!(
            system[..system.len() - 1]
                .iter()
                .all(|b| b.get("cache_control").is_none())
        );
        assert_eq!(
            body["metadata"]["user_id"],
            r#"{"session_id":"session-test","account_uuid":"acct","device_id":"d683badf64165b13acd91a7aa340d96c52d2f109ce2616943fa4b76464d31385"}"#
        );
        if i == 0 {
            assert_eq!(body["system"][2]["text"], "Be concise.");
            let tools = body["tools"].as_array().unwrap();
            assert_eq!(
                tools
                    .iter()
                    .map(|t| t["name"].as_str().unwrap())
                    .collect::<Vec<_>>(),
                [
                    "_read",
                    "WEB_SEARCH",
                    "code_execution",
                    "text_editor",
                    "computer",
                    "__custom"
                ]
            );
            assert!(tools[..5].iter().all(|t| t.get("cache_control").is_none()));
            assert_eq!(tools[5]["cache_control"]["ttl"], "1h");
            assert_eq!(body["messages"][1]["content"][0]["name"], "_read");
        } else {
            assert_eq!(body["tools"], serde_json::json!([]));
            assert_eq!(body["stream"], false);
        }
    }
}

#[tokio::test]
#[serial]
async fn k12_independent_literal_raw_body_and_attestation() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let _env = Env::set(&[
        ("RUSTAIN_DATA_DIR", Some(dir.path().to_str().unwrap())),
        ("RUSTAIN_TEST_AUTH_ORIGIN", Some(&server.uri())),
    ]);
    pin();
    login(&server, 3600).await;
    server.reset().await;
    endpoints(&server).await;
    build_provider_for_config("anthropic", &config(&server.uri(), ""))
        .unwrap()
        .stream_completion(vec![message("hello")], options())
        .await
        .unwrap()
        .collect::<Vec<_>>()
        .await;
    // Independent Python json + system libxxhash oracle (seed 0x4d659218e32a3268).
    const EXPECTED: &str = r#"{"model":"test-model","max_tokens":32,"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.280.790; cc_entrypoint=cli; cch=41c75;"},{"type":"text","text":"You are Claude Code, Anthropic's official CLI for Claude."},{"type":"text","text":"Be concise.","cache_control":{"type":"ephemeral","ttl":"1h"}}],"messages":[{"role":"user","content":[{"type":"text","text":"hello"}]}],"stream":true,"temperature":0.5,"tools":[],"metadata":{"user_id":"{\"session_id\":\"session-test\",\"account_uuid\":\"acct\",\"device_id\":\"d683badf64165b13acd91a7aa340d96c52d2f109ce2616943fa4b76464d31385\"}"}}"#;
    assert_eq!(
        server.received_requests().await.unwrap()[0].body,
        EXPECTED.as_bytes()
    );
}

#[tokio::test]
#[serial]
async fn env_subscription_gateway_cache_and_plain_api_key_bytes() {
    let server = MockServer::start().await;
    let official = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let _env = Env::set(&[
        ("RUSTAIN_DATA_DIR", Some(dir.path().to_str().unwrap())),
        ("RUSTAIN_TEST_AUTH_ORIGIN", Some(&official.uri())),
        ("ANTHROPIC_API_KEY", Some("prefix-sk-ant-oat-token")),
    ]);
    pin();
    endpoints(&server).await;
    let cfg = config(&server.uri(), "ANTHROPIC_API_KEY");
    let provider = build_provider_for_config("anthropic", &cfg).unwrap();
    provider
        .stream_completion(vec![message("hello")], options())
        .await
        .unwrap()
        .collect::<Vec<_>>()
        .await;
    provider.health_check().await.unwrap();
    provider.connectivity_probe().await.unwrap();
    let requests = server.received_requests().await.unwrap();
    for request in &requests {
        headers(request, false, "prefix-sk-ant-oat-token", "2.1.280");
    }
    for request in &requests[..2] {
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body["tools"], serde_json::json!([]));
        assert_eq!(
            body["metadata"]["user_id"],
            r#"{"session_id":"session-test"}"#
        );
        assert_eq!(
            body["system"].as_array().unwrap().last().unwrap()["cache_control"],
            serde_json::json!({"type":"ephemeral"})
        );
    }
    assert!(requests[2].body.is_empty());
    assert!(requests[2].url.query().is_none());
    let _key = Env::set(&[("ANTHROPIC_API_KEY", Some("plain-console-key"))]);
    server.reset().await;
    endpoints(&server).await;
    let plain = build_provider_for_config("anthropic", &cfg).unwrap();
    let chunks = plain
        .stream_completion(vec![message("hello")], options())
        .await
        .unwrap()
        .collect::<Vec<_>>()
        .await;
    assert!(
        chunks
            .iter()
            .any(|c| matches!(c, StreamChunk::ToolUse { name, .. } if name == "_read"))
    );
    plain.health_check().await.unwrap();
    plain.connectivity_probe().await.unwrap();
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests[0].body, br#"{"model":"test-model","max_tokens":32,"system":"Be concise.","messages":[{"role":"user","content":[{"type":"text","text":"hello"}]}],"stream":true,"temperature":0.5}"#);
    assert_eq!(
        requests[1].body,
        br#"{"max_tokens":1,"messages":[{"content":"hi","role":"user"}],"model":"test-model"}"#
    );
    for request in &requests {
        assert_eq!(request.headers["x-api-key"], "plain-console-key");
        assert!(request.url.query().is_none());
        for header in [
            "anthropic-beta",
            "x-app",
            "x-claude-code-session-id",
            "x-stainless-lang",
            "user-agent",
            "authorization",
        ] {
            assert!(request.headers.get(header).is_none(), "{header}");
        }
    }
}

#[tokio::test]
#[serial]
async fn install_id_persists_0600_and_env_version_pin_is_read_each_request() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let _env = Env::set(&[
        ("RUSTAIN_DATA_DIR", Some(dir.path().to_str().unwrap())),
        ("RUSTAIN_TEST_AUTH_ORIGIN", Some(&server.uri())),
        ("RUSTAIN_CLAUDE_CODE_VERSION", Some(" 9.8.7 ")),
    ]);
    login(&server, 3600).await;
    server.reset().await;
    endpoints(&server).await;
    let cfg = config(&server.uri(), "");
    build_provider_for_config("anthropic", &cfg)
        .unwrap()
        .health_check()
        .await
        .unwrap();
    let path = dir.path().join("install-id");
    let id = std::fs::read_to_string(&path).unwrap();
    assert_eq!(id.len(), 36);
    assert_eq!(&id[14..15], "4");
    assert!(matches!(&id[19..20], "8" | "9" | "a" | "b"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    build_provider_for_config("anthropic", &cfg)
        .unwrap()
        .health_check()
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), id);
    // A foreign or partial id file is replaced instead of failing every request.
    std::fs::write(&path, [0xff, 0xfe, b'x']).unwrap();
    build_provider_for_config("anthropic", &cfg)
        .unwrap()
        .health_check()
        .await
        .unwrap();
    let healed = std::fs::read_to_string(&path).unwrap();
    assert_eq!(healed.len(), 36);
    assert_ne!(healed, id);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests[0].headers["user-agent"],
        "claude-cli/9.8.7 (external, cli)"
    );
    assert_eq!(
        requests[0].headers["x-claude-code-session-id"],
        requests[1].headers["x-claude-code-session-id"]
    );
    let first: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    let second: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
    assert_eq!(first["metadata"], second["metadata"]);
    assert!(
        first["system"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("x-anthropic-billing-header: cc_version=9.8.7.")
    );
    let _version = Env::set(&[("RUSTAIN_CLAUDE_CODE_VERSION", Some("8.7.6"))]);
    build_provider_for_config("anthropic", &cfg)
        .unwrap()
        .connectivity_probe()
        .await
        .unwrap();
    assert_eq!(
        server
            .received_requests()
            .await
            .unwrap()
            .last()
            .unwrap()
            .headers["user-agent"],
        "claude-cli/8.7.6 (external, cli)"
    );
}

#[tokio::test]
#[serial]
async fn billing_utf16_and_already_profiled_system_use_production_serializer() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let _env = Env::set(&[
        ("RUSTAIN_DATA_DIR", Some(dir.path().to_str().unwrap())),
        ("ANTHROPIC_API_KEY", Some("sk-ant-oat-env")),
    ]);
    pin();
    endpoints(&server).await;
    let provider =
        build_provider_for_config("anthropic", &config(&server.uri(), "ANTHROPIC_API_KEY"))
            .unwrap();
    provider
        .stream_completion(vec![message("abcd😀")], options())
        .await
        .unwrap()
        .collect::<Vec<_>>()
        .await;
    let first: serde_json::Value =
        serde_json::from_slice(&server.received_requests().await.unwrap()[0].body).unwrap();
    assert!(
        first["system"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("x-anthropic-billing-header: cc_version=2.1.280.fc4;")
    );
    let mut opts = options();
    opts.system_prompt = "x-anthropic-billing-header: cc_version=custom; cch=00000;".into();
    provider
        .stream_completion(vec![message("hello")], opts)
        .await
        .unwrap()
        .collect::<Vec<_>>()
        .await;
    let second: serde_json::Value =
        serde_json::from_slice(&server.received_requests().await.unwrap()[1].body).unwrap();
    assert_eq!(second["system"].as_array().unwrap().len(), 1);
    assert!(
        second["system"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("x-anthropic-billing-header: cc_version=custom; cch=")
    );
    // omp patches when the placeholder starts at most 150 bytes after the marker.
    for (offset, patched) in [(141, true), (150, true), (151, false)] {
        let mut opts = options();
        opts.system_prompt = format!("x-anthropic-billing-header:{}cch=00000", ".".repeat(offset));
        provider
            .stream_completion(vec![message("hello")], opts)
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        let requests = server.received_requests().await.unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&requests.last().unwrap().body).unwrap();
        let text = body["system"][0]["text"].as_str().unwrap();
        assert_eq!(
            !text.ends_with("cch=00000"),
            patched,
            "placeholder must start within the 150-byte window at offset {offset}"
        );
    }
}

#[test]
#[serial]
fn config_validate_prints_migration_warning_and_honors_explicit_header() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    for (env, explicit, expected) in [
        ("GW_TOKEN", "", true),
        ("GW_TOKEN", "auth_header = \"bearer\"\n", false),
        ("ANTHROPIC_AUTH_TOKEN", "", false),
    ] {
        std::fs::write(&config, format!("[provider.gateway]\nprovider_id = \"gateway\"\nkind = \"anthropic\"\nmodel_id = \"test-model\"\napi_key_env = \"{env}\"\n{explicit}")).unwrap();
        let result = std::process::Command::new(env!("CARGO_BIN_EXE_rustain"))
            .args([
                "--config-file",
                config.to_str().unwrap(),
                "config",
                "validate",
                "--json",
            ])
            .env("HOME", dir.path())
            .env("RUSTAIN_CONFIG_DIR", dir.path())
            .env("RUSTAIN_DATA_DIR", dir.path())
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let output: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(output["valid"], true);
        assert_eq!(!output["warnings"].as_array().unwrap().is_empty(), expected);
        assert_eq!(
            String::from_utf8_lossy(&result.stderr).contains("auth_header is unset"),
            expected
        );
    }
}
