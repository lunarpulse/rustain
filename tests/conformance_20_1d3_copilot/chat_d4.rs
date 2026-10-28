//! D-4: the resolver-backed OpenAI Chat Completions adapter on the existing
//! `openai` provider (K05/M05, AC6), through `build_provider_for_config`. Every
//! stored key comes from a real `flows::login("openai", ApiKey)` whose
//! validation probe (`GET /v1/models`) is answered by the loopback stub.

use super::common::{
    NOW, World, assistant, chat_sse, complete, header, json_body, json_reply, options, reply,
    requests, rows, text_of, user,
};
use futures::future::BoxFuture;
use parking_lot::Mutex;
use rustain::adapters::auth::flows::{self, FlowCtx, LoginUi};
use rustain::adapters::auth_store::FileAuthStore;
use rustain::domain::errors::ProviderError;
use rustain::domain::models::{
    AuthMethod, CompletionOptions, Credential, ProviderConfig, RedactedUrl, SecretString,
};
use rustain::domain::ports::{AuthStorePort, StreamingProvider};
use rustain::infrastructure::provider_factory::{
    build_provider_for_config, build_provider_for_config_with_key,
};
use serde_json::{Value, json};
use serial_test::serial;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use wiremock::MockServer;

const CHAT: &str = "/v1/chat/completions";
const MODELS: &str = "/v1/models";
const MODEL: &str = "gpt-4o";
const SYSTEM: &str = "You are terse.";
const ENTER: &str = "Enter OpenAI API key: ";
const OVERWRITE: &str = "Overwrite existing OpenAI API key? [y/n] ";

// ── fixtures ────────────────────────────────────────────────────────────────

/// A `[provider.<id>]`-shaped entry of `kind` whose base is the stub's `/v1`.
fn entry(server: &MockServer, id: &str, kind: &str, env: &str) -> ProviderConfig {
    ProviderConfig {
        provider_id: id.into(),
        kind: Some(kind.into()),
        model_id: MODEL.into(),
        api_key_env: env.into(),
        auth_header: None,
        enabled: true,
        base_url: Some(RedactedUrl::from(format!("{}/v1", server.uri()))),
        context_window: None,
        supports_tools: None,
        discover_models: true,
        model_filter: vec!["*".into()],
        cache_ttl_seconds: 3600,
    }
}
/// The `[provider.openai]` entry.
fn openai(server: &MockServer) -> ProviderConfig {
    entry(server, "openai", "openai", "OPENAI_API_KEY")
}

/// An API-key login UI: confirms an overwrite with `y`, answers the key prompt
/// with `key`, and records every secret prompt label.
struct KeyUi {
    key: String,
    prompts: Mutex<Vec<String>>,
}
impl LoginUi for KeyUi {
    fn on_auth_url(&self, _: &str, _: &str, _: &str) {
        panic!("an API-key login never publishes an authorization URL");
    }
    fn on_device_code(&self, _: &str, _: &str, _: &str) {
        panic!("an API-key login never displays a device code");
    }
    fn on_progress(&self, _: &str) {}
    fn prompt_secret(&self, label: &str) -> BoxFuture<'_, Option<SecretString>> {
        self.prompts.lock().push(label.into());
        let answer = if label.starts_with("Overwrite") {
            "y".to_owned()
        } else {
            self.key.clone()
        };
        Box::pin(async move { Some(SecretString::new(answer)) })
    }
    fn manual_input(&self, _: &str) -> Option<BoxFuture<'static, Option<String>>> {
        None
    }
    fn prompt_line(&self, _: &str) -> BoxFuture<'_, Option<String>> {
        Box::pin(async { None })
    }
    fn cancel_token(&self) -> CancellationToken {
        CancellationToken::new()
    }
}

/// The real `flows::login("openai", ApiKey)` with the production validation
/// config (the CLI passes the entry); returns the secret prompt labels.
async fn key_login(cfg: &ProviderConfig, key: &str) -> Vec<String> {
    let ui = Arc::new(KeyUi {
        key: key.into(),
        prompts: Mutex::new(Vec::new()),
    });
    let store: Arc<dyn AuthStorePort> = Arc::new(FileAuthStore::new());
    let ctx = FlowCtx::new(ui.clone(), Arc::new(|| NOW)).with_validation_config(cfg.clone());
    let result = flows::login("openai", &AuthMethod::ApiKey, &ctx, &store)
        .await
        .expect("the OpenAI key login succeeds");
    assert!(result.validated, "the stubbed probe validates the key");
    ui.prompts.lock().clone()
}

/// The single stored `openai` key.
async fn stored_key() -> String {
    let rows = rows("openai").await;
    assert_eq!(rows.len(), 1, "one served openai row");
    match &rows[0].credential {
        Credential::ApiKey(key) => key.expose_secret().to_owned(),
        _ => panic!("an API-key login stores a key row"),
    }
}

/// `GET /v1/models` answers one chat model (login probe and discovery).
async fn mount_models(server: &MockServer) {
    reply(
        server,
        "GET",
        MODELS,
        json_reply(
            200,
            json!({"object":"list","data":[{"id":MODEL,"object":"model","created":0,"owned_by":"openai"}]}),
        ),
    )
    .await;
}

fn bearer(key: &str) -> Option<String> {
    Some(format!("Bearer {key}"))
}

fn turn() -> Vec<rustain::domain::models::Message> {
    vec![user("hi"), assistant("hello"), user("again")]
}
fn turn_json() -> Vec<Value> {
    vec![
        json!({"role":"user","content":"hi"}),
        json!({"role":"assistant","content":"hello"}),
        json!({"role":"user","content":"again"}),
    ]
}
fn opts(system: &str, temperature: Option<f32>) -> CompletionOptions {
    let mut options = options(MODEL, 1024, Some("conv-d4"));
    options.system_prompt = system.into();
    options.temperature = temperature;
    options
}

/// The pre-story request literal for `turn()` (no `store`, no
/// `max_completion_tokens` for plain `openai`), with `messages` prefixed by
/// `leading` and `temperature` when supplied.
fn literal(leading: Option<Value>, temperature: Option<f64>) -> Value {
    let mut messages: Vec<Value> = leading.into_iter().collect();
    messages.extend(turn_json());
    let mut body = json!({
        "model": MODEL,
        "messages": messages,
        "stream": true,
        "stream_options": {"include_usage": true},
    });
    if let Some(temperature) = temperature {
        body["temperature"] = json!(temperature);
    }
    body
}

async fn ok_turn(provider: &Arc<dyn StreamingProvider>, options: CompletionOptions) {
    let chunks = complete(provider, turn(), options)
        .await
        .expect("the completion opens");
    assert_eq!(text_of(&chunks), "ok");
}

// ── K05 ─────────────────────────────────────────────────────────────────────

/// K05 (M05): one `openai` adapter built once serves the stored key current at
/// each request — `Bearer k1`, then (after a real re-login) `Bearer k2` on the
/// same instance; the system prompt leads and the rest is the pre-story body.
#[tokio::test]
#[serial]
async fn k05_stored_key_change_reaches_the_same_adapter_instance() {
    const K1: &str = "sk-d4-k05-one";
    const K2: &str = "sk-d4-k05-two";
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let cfg = openai(&server);
    mount_models(&server).await;
    reply(&server, "POST", CHAT, chat_sse("ok")).await;

    assert_eq!(key_login(&cfg, K1).await, [ENTER]);
    assert_eq!(stored_key().await, K1);
    let provider = build_provider_for_config("openai", &cfg).expect("openai builds");

    ok_turn(&provider, opts(SYSTEM, Some(0.5))).await;
    let posts = requests(&server, "POST", CHAT).await;
    assert_eq!(posts.len(), 1);
    assert_eq!(header(&posts[0], "authorization"), bearer(K1));
    assert_eq!(
        json_body(&posts[0]),
        literal(Some(json!({"role":"system","content":SYSTEM})), Some(0.5))
    );

    // Re-login through the production flow: the overwrite is confirmed and
    // the candidate is probed with its own bearer.
    assert_eq!(key_login(&cfg, K2).await, [OVERWRITE, ENTER]);
    assert_eq!(stored_key().await, K2);
    let probes = requests(&server, "GET", MODELS).await;
    assert_eq!(header(probes.last().unwrap(), "authorization"), bearer(K2));

    ok_turn(&provider, opts(SYSTEM, None)).await;
    let posts = requests(&server, "POST", CHAT).await;
    assert_eq!(posts.len(), 2);
    assert_eq!(
        header(&posts[1], "authorization"),
        bearer(K2),
        "the second request on the same adapter resolves the re-logged key"
    );
    assert_eq!(
        json_body(&posts[1]),
        literal(Some(json!({"role":"system","content":SYSTEM})), None)
    );
}

/// K05 (M05): a 401 on a stored API key → exactly one POST and
/// `AuthenticationFailed` (keys never recover).
#[tokio::test]
#[serial]
async fn k05_api_key_401_is_one_post_and_authentication_failed() {
    const KEY: &str = "sk-d4-k05-rejected";
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let cfg = openai(&server);
    mount_models(&server).await;
    reply(
        &server,
        "POST",
        CHAT,
        json_reply(
            401,
            json!({"error":{"message":"Incorrect API key provided"}}),
        ),
    )
    .await;
    key_login(&cfg, KEY).await;
    let provider = build_provider_for_config("openai", &cfg).expect("openai builds");
    let models_before = requests(&server, "GET", MODELS).await.len();

    match complete(&provider, turn(), opts(SYSTEM, None)).await {
        Err(ProviderError::AuthenticationFailed) => {}
        other => panic!("expected AuthenticationFailed, got {:?}", other.err()),
    }
    let posts = requests(&server, "POST", CHAT).await;
    assert_eq!(posts.len(), 1, "an API key is never re-sent after a 401");
    assert_eq!(header(&posts[0], "authorization"), bearer(KEY));
    assert_eq!(
        requests(&server, "GET", MODELS).await.len(),
        models_before,
        "no recovery traffic"
    );
    assert_eq!(stored_key().await, KEY, "the rejected key row is untouched");
}

// ── AC6 ─────────────────────────────────────────────────────────────────────

/// AC6: `OPENAI_API_KEY` changed between two requests is used by the second;
/// a blank system prompt adds no system message.
#[tokio::test]
#[serial]
async fn ac6_env_key_change_is_used_by_the_next_request_and_blank_system_adds_nothing() {
    const ENV1: &str = "sk-d4-env-one";
    const ENV2: &str = "sk-d4-env-two";
    let server = MockServer::start().await;
    let world = World::new(&server);
    let cfg = openai(&server);
    reply(&server, "POST", CHAT, chat_sse("ok")).await;
    world.set("OPENAI_API_KEY", ENV1);
    let provider = build_provider_for_config("openai", &cfg).expect("openai builds");

    ok_turn(&provider, opts(" \n\t ", Some(0.5))).await;
    world.set("OPENAI_API_KEY", ENV2);
    ok_turn(&provider, opts("", None)).await;

    let posts = requests(&server, "POST", CHAT).await;
    assert_eq!(posts.len(), 2);
    assert_eq!(header(&posts[0], "authorization"), bearer(ENV1));
    assert_eq!(header(&posts[1], "authorization"), bearer(ENV2));
    assert_eq!(json_body(&posts[0]), literal(None, Some(0.5)));
    assert_eq!(json_body(&posts[1]), literal(None, None));
    assert!(rows("openai").await.is_empty(), "env keys are never stored");
}

/// AC6: `build_provider_for_config_with_key` probes the candidate key against
/// `GET /v1/models` and writes nothing to the store.
#[tokio::test]
#[serial]
async fn ac6_candidate_key_probe_uses_the_candidate_and_touches_no_store() {
    const CANDIDATE: &str = "sk-d4-candidate";
    let server = MockServer::start().await;
    let world = World::new(&server);
    let cfg = openai(&server);
    mount_models(&server).await;

    let provider = build_provider_for_config_with_key("openai", &cfg, CANDIDATE)
        .expect("a candidate-key adapter builds without env or store");
    provider
        .connectivity_probe()
        .await
        .expect("the stubbed probe succeeds");

    let probes = requests(&server, "GET", MODELS).await;
    assert_eq!(probes.len(), 1);
    assert_eq!(header(&probes[0], "authorization"), bearer(CANDIDATE));
    assert!(rows("openai").await.is_empty());
    assert!(!world.dir.path().join("auth.json").exists());
}

/// AC6: keyless `openai-compatible` (no `api_key_env`, no stored row) sends no
/// `Authorization` header.
#[tokio::test]
#[serial]
async fn ac6_keyless_openai_compatible_sends_no_authorization() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let cfg = entry(&server, "local-gateway", "openai-compatible", "");
    reply(&server, "POST", CHAT, chat_sse("ok")).await;

    let provider = build_provider_for_config("local-gateway", &cfg).expect("keyless entry builds");
    ok_turn(&provider, opts(SYSTEM, None)).await;

    let posts = requests(&server, "POST", CHAT).await;
    assert_eq!(posts.len(), 1);
    assert_eq!(header(&posts[0], "authorization"), None);
    assert_eq!(
        json_body(&posts[0]),
        literal(Some(json!({"role":"system","content":SYSTEM})), None)
    );
}

/// AC6: `/models` discovery is scoped to the credential actually sent — the
/// scope differs between stored keys k1 and k2 on the same adapter and equals
/// the fixed-key scope of the key on the wire.
#[tokio::test]
#[serial]
async fn ac6_discovery_scope_is_bound_to_the_credential_sent() {
    const K1: &str = "sk-d4-scope-one";
    const K2: &str = "sk-d4-scope-two";
    const CONTEXT: &str = "ctx";
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let cfg = openai(&server);
    mount_models(&server).await;

    key_login(&cfg, K1).await;
    let provider = build_provider_for_config("openai", &cfg).expect("openai builds");
    let first = provider
        .discover_models(&["*".into()], CONTEXT)
        .await
        .expect("openai discovers")
        .expect("discovery succeeds");
    let listing = requests(&server, "GET", MODELS).await;
    assert_eq!(header(listing.last().unwrap(), "authorization"), bearer(K1));

    key_login(&cfg, K2).await;
    let second = provider
        .discover_models(&["*".into()], CONTEXT)
        .await
        .expect("openai discovers")
        .expect("discovery succeeds");
    let listing = requests(&server, "GET", MODELS).await;
    assert_eq!(header(listing.last().unwrap(), "authorization"), bearer(K2));

    assert_ne!(first.credential_scope, second.credential_scope);
    let fixed_scope = |key: &str| {
        build_provider_for_config_with_key("openai", &cfg, key)
            .expect("candidate adapter")
            .discovery_credential_fingerprint(CONTEXT)
    };
    assert_eq!(Some(first.credential_scope), fixed_scope(K1));
    assert_eq!(Some(second.credential_scope.clone()), fixed_scope(K2));
    assert_eq!(
        provider.discovery_scope(CONTEXT).await,
        Some(second.credential_scope),
        "the adapter's scope follows the current key"
    );
}
