//! Catalogue-only registration through `init_provider_layer` and the models.dev
//! republication path (K11), the credential gate and the untouched `google`
//! API-key provider (AC11), and the real debug binary (K12).
//!
//! K12 needs the real binary built with `test-instrumentation` first.
//! `RUSTAIN_TEST_GEMINI_CLI_BINARY` selects it explicitly; the cargo-bin fallback
//! has the SAME instrumentation requirement, checked before any HTTP action.
//! `RUSTAIN_TEST_GEMINI_CLI_EVIDENCE_DIR` retains command/output/request evidence.

use super::auth::{
    ACCESS, CLIENT_ID, CLIENT_SECRET, CODE, EMAIL, Env, FREE_LOAD_BODY, Google, LOAD, MODEL, NOW,
    ONBOARD, OPERATION, PROJECT, PROVIDER, REFRESH, STREAM, TOKEN, USERINFO, build, config, env,
    hits, login, mount, ok, options, query, store, token_body, user,
};
use futures::StreamExt;
use rustain::adapters::auth_store::FileAuthStore;
use rustain::adapters::model_catalog_cache::{DiscoveryTarget, ModelCatalogCache};
use rustain::adapters::tui::refresh_tracker::RefreshTracker;
use rustain::domain::errors::ProviderError;
use rustain::domain::events::AppEvent;
use rustain::domain::models::provider::{DiscoveryStatus, LockedProvider};
use rustain::domain::models::{
    AppConfig, AuthSource, Credential, ModelCapability, ModelDescriptor, StreamChunk,
};
use rustain::domain::ports::AuthStorePort;
use rustain::infrastructure::model_discovery::{
    rediscover_after_login, refresh_and_announce, rejected_notice, spawn_headless,
};
use rustain::infrastructure::provider_availability::{
    Availability, ProviderOrigin, availability, effective_providers,
};
use rustain::infrastructure::provider_factory::{
    build_provider_for_config, build_provider_for_config_with_key,
};
use rustain::infrastructure::runtime::event_bus::EventBus;
use rustain::infrastructure::startup::{ProviderLayer, init_provider_layer};
use rustain::infrastructure::utils::env_var_trimmed;
use serde_json::{Value, json};
use serial_test::serial;
use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
    process::{Output, Stdio},
    sync::Arc,
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{method, path},
};

const NAME: &str = "Google Cloud Code Assist (Gemini CLI)";
const HINT: &str = "Run `rustain auth login google-gemini-cli`";
/// Hang guard only: every wait below is released by an explicit event.
const GUARD: Duration = Duration::from_secs(30);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
/// Nothing listens here; in-process catalogue tests never fetch models.dev.
const NO_MODELS_DEV: &str = "http://127.0.0.1:1";
const CLIENT_METADATA: &str =
    "ideType=IDE_UNSPECIFIED,platform=PLATFORM_UNSPECIFIED,pluginType=GEMINI";
/// omp `models.json` `google-gemini-cli`, in omp order: id, name, context, reasoning.
const SEED: [(&str, &str, u32, bool); 7] = [
    ("gemini-2.0-flash", "Gemini 2.0 Flash", 1_048_576, false),
    ("gemini-2.5-flash", "Gemini 2.5 Flash", 1_048_576, true),
    ("gemini-2.5-pro", "Gemini 2.5 Pro", 1_048_576, true),
    (
        "gemini-3-flash-preview",
        "Gemini 3 Flash Preview",
        1_048_576,
        true,
    ),
    (
        "gemini-3-pro-preview",
        "Gemini 3 Pro Preview",
        1_000_000,
        true,
    ),
    (
        "gemini-3.1-flash-lite-preview",
        "Gemini 3.1 Flash Lite Preview",
        1_048_576,
        true,
    ),
    (
        "gemini-3.1-pro-preview",
        "Gemini 3.1 Pro Preview",
        1_048_576,
        true,
    ),
];

// ── in-process fixtures ─────────────────────────────────────────────────────

/// Every provider credential the bundled entries could pick up is cleared, so
/// the Gemini CLI login is the only signed entry; `GOOGLE_API_KEY` is cleared by
/// `env`. Config dir isolated; models.dev points at `models_dev`.
fn isolate(dir: &Path, models_dev: &str) -> Env {
    let config_dir = dir.join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    Env::set(&[
        ("RUSTAIN_CONFIG_DIR", Some(config_dir.to_str().unwrap())),
        ("RUSTAIN_MODELS_DEV_URL", Some(models_dev)),
        ("ANTHROPIC_API_KEY", None),
        ("ANTHROPIC_AUTH_TOKEN", None),
        ("ANTHROPIC_BASE_URL", None),
        ("OPENAI_API_KEY", None),
        ("OPENAI_CODEX_OAUTH_TOKEN", None),
        ("OPENROUTER_API_KEY", None),
        ("DEEPSEEK_API_KEY", None),
        ("MOONSHOT_API_KEY", None),
        ("XAI_API_KEY", None),
        ("XAI_OAUTH_TOKEN", None),
        ("XAI_BASE_URL", None),
    ])
}

/// No configured providers: `google-gemini-cli` is the bundled entry.
fn app_config() -> AppConfig {
    AppConfig {
        model: MODEL.into(),
        ..Default::default()
    }
}

fn seed() -> Vec<(String, String, u32)> {
    SEED.iter()
        .map(|(id, name, context, _)| ((*id).to_owned(), (*name).to_owned(), *context))
        .collect()
}

fn rows(models: &[ModelDescriptor]) -> Vec<(String, String, u32)> {
    models
        .iter()
        .map(|m| (m.model_id.clone(), m.display_name.clone(), m.context_window))
        .collect()
}

fn ids(models: &[ModelDescriptor]) -> Vec<String> {
    models.iter().map(|m| m.model_id.clone()).collect()
}

/// Vision and tools on every row; thinking exactly on the reasoning rows.
fn assert_seed_capabilities(models: &[ModelDescriptor]) {
    for (model, (id, _, _, reasoning)) in models.iter().zip(SEED) {
        assert_eq!(model.model_id, id);
        assert!(
            model.capabilities.contains(&ModelCapability::Vision),
            "{id}"
        );
        assert!(
            model.capabilities.contains(&ModelCapability::ToolUse),
            "{id}"
        );
        assert_eq!(
            model.capabilities.contains(&ModelCapability::Thinking),
            reasoning,
            "{id}"
        );
    }
}

fn served(layer: &ProviderLayer) -> Vec<ModelDescriptor> {
    layer.registry.list_models_by_provider(PROVIDER)
}

fn catalogue_target(layer: &ProviderLayer) -> &DiscoveryTarget {
    layer
        .catalogue_targets
        .iter()
        .find(|t| t.provider_id == PROVIDER)
        .expect("a signed Gemini CLI entry is a catalogue target")
}

fn locked(layer: &ProviderLayer, id: &str) -> LockedProvider {
    layer
        .registry
        .list_locked()
        .into_iter()
        .find(|p| p.provider_id == id)
        .unwrap_or_else(|| panic!("{id} is not locked"))
}

fn not_a_live_target(layer: &ProviderLayer) {
    assert!(
        layer
            .discovery_targets
            .iter()
            .all(|t| t.provider_id != PROVIDER && t.kind != PROVIDER),
        "catalogue-only: never a live discovery target"
    );
    assert!(
        layer
            .unsupported_discovery
            .iter()
            .all(|(id, kind)| id != PROVIDER && kind != PROVIDER),
        "no \"doesn't support model discovery\" toast"
    );
}

fn notices(rx: &mut tokio::sync::mpsc::UnboundedReceiver<AppEvent>) -> (Vec<String>, Vec<String>) {
    let (mut refreshed, mut notices) = (Vec::new(), Vec::new());
    while let Ok(event) = rx.try_recv() {
        match event {
            AppEvent::ProviderCatalogRefreshed { provider_id } => refreshed.push(provider_id),
            AppEvent::SystemNotice { message, .. } => notices.push(message),
            _ => {}
        }
    }
    (refreshed, notices)
}

/// `Ok` as `"ok"`, an error by its variant, so one table row states both ports.
fn port(result: Result<(), ProviderError>) -> String {
    match result {
        Ok(()) => "ok".into(),
        Err(error) => format!("{error:?}"),
    }
}

/// The Gemini CLI wire identity with Node platform/arch names.
fn user_agent(model: &str) -> String {
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

fn header<'a>(request: &'a Request, name: &str) -> &'a str {
    request
        .headers
        .get(name)
        .unwrap_or_else(|| panic!("missing header {name}"))
        .to_str()
        .unwrap()
}

// ── K11: catalogue-only ─────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn k11_catalogue_only_target_is_seeded_never_discovered_and_never_claims_discovery() {
    let server = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();
    let _env = env(temp.path(), &server.uri());
    let _isolated = isolate(temp.path(), NO_MODELS_DEV);
    let cfg = app_config();
    let before = store().snapshot().await.unwrap();
    login(&server, ACCESS, PROJECT, EMAIL).await;
    let baseline = server.received_requests().await.unwrap().len();

    // The post-login refresh finds no live target for the kind: nothing to announce.
    assert!(
        rediscover_after_login(PROVIDER, &cfg, &before)
            .await
            .is_none(),
        "a catalogue-only login claims no discovery"
    );

    let layer = init_provider_layer(&cfg);
    let target = catalogue_target(&layer);
    assert_eq!(target.kind, PROVIDER);
    assert_eq!(target.display_name, NAME);
    assert_eq!(target.sign_in_hint, HINT);
    not_a_live_target(&layer);
    // Every other bundled entry is locked in this world, so the live consumers
    // (startup `refresh_and_announce`, `spawn_periodic_catalog_refresh`,
    // `spawn_headless`) have no input at all.
    assert!(
        layer.discovery_targets.is_empty(),
        "{:?}",
        layer
            .discovery_targets
            .iter()
            .map(|t| &t.provider_id)
            .collect::<Vec<_>>()
    );
    assert!(layer.router.get_provider(PROVIDER).is_some());
    assert_eq!(rows(&target.bundled), seed());
    let models = served(&layer);
    assert_eq!(rows(&models), seed(), "seeded through `seed_target`");
    assert_seed_capabilities(&models);
    assert!(models.iter().all(|m| m.provider_id == PROVIDER && !m.stale));
    assert_eq!(
        layer.registry.discovery_status(PROVIDER),
        Some(DiscoveryStatus::Idle)
    );
    assert_eq!(layer.registry.sign_in_hint(PROVIDER).as_deref(), Some(HINT));

    // A rejected credential names the login and claims no discovery failure.
    let notice = rejected_notice(target);
    assert_eq!(
        notice,
        "Google Cloud Code Assist (Gemini CLI) rejected its credential (HTTP 401/403). Run `rustain auth login google-gemini-cli`"
    );
    assert!(!notice.contains("models could not be discovered"));

    // The live consumers over exactly the inputs production hands them.
    let (bus, mut domain_rx) = EventBus::new(64);
    let bus = Arc::new(bus);
    let tracker = RefreshTracker::new();
    let cache = ModelCatalogCache::new();
    for live in &layer.discovery_targets {
        refresh_and_announce(live, &cache, &layer.registry, true, &bus, &tracker).await;
    }
    spawn_headless(layer.discovery_targets.clone(), layer.registry.clone());
    // Even handed to the refresh path directly, the adapter has no discovery
    // request: it keeps serving the seed, raises no notice and sends nothing.
    refresh_and_announce(target, &cache, &layer.registry, true, &bus, &tracker).await;
    assert_eq!(rows(&served(&layer)), seed());
    let (_, raised) = notices(&mut domain_rx);
    assert!(raised.is_empty(), "{raised:?}");

    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests.len(),
        baseline,
        "no live consumer sends a request for the kind: {:?}",
        requests[baseline..]
            .iter()
            .map(|r| r.url.path().to_owned())
            .collect::<Vec<_>>()
    );
}

#[cfg(feature = "models-dev")]
#[tokio::test]
#[serial]
async fn k11_models_dev_refresh_republishes_the_seed_through_the_catalogue_only_path() {
    let server = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();
    let _env = env(temp.path(), &server.uri());
    let _isolated = isolate(temp.path(), &server.uri());
    login(&server, ACCESS, PROJECT, EMAIL).await;
    // `google` is mapped to models.dev; the subscription kind has no mapping.
    Mock::given(method("GET"))
        .and(path("/api.json"))
        .respond_with(ok(json!({
            "google": {"models": {"models-dev-only": {"tool_call": true}}},
            "google-gemini-cli": {"models": {"gemini-cli-models-dev-only": {"tool_call": true}}},
        })))
        .mount(&server)
        .await;
    let cfg = app_config();
    let layer = init_provider_layer(&cfg);
    let target = catalogue_target(&layer);
    not_a_live_target(&layer);
    let baseline = server.received_requests().await.unwrap().len();

    rustain::adapters::models_dev::refresh().await.unwrap();
    // Unpublished, so the seed can only come back through the republication path.
    target.provider.set_discovered_models(Vec::new());
    assert!(served(&layer).is_empty());
    let (bus, mut domain_rx) = EventBus::new(64);
    let bus = Arc::new(bus);
    rustain::infrastructure::model_discovery::remerge_all(
        &layer.registry,
        &layer.catalogue_targets,
        &bus,
        &cfg,
    )
    .await;

    let models = served(&layer);
    assert_eq!(
        rows(&models),
        seed(),
        "the republished catalogue is the seed"
    );
    assert_seed_capabilities(&models);
    let served_ids = ids(&models);
    assert!(!served_ids.contains(&"models-dev-only".to_owned()));
    assert!(!served_ids.contains(&"gemini-cli-models-dev-only".to_owned()));
    let (refreshed, raised) = notices(&mut domain_rx);
    assert!(refreshed.iter().any(|id| id == PROVIDER), "{refreshed:?}");
    assert!(raised.is_empty(), "{raised:?}");
    // Control: the same refresh did extend the mapped API-key provider.
    assert!(ids(&locked(&layer, "google").models).contains(&"models-dev-only".to_owned()));

    let requests = server.received_requests().await.unwrap();
    let after: Vec<_> = requests[baseline..]
        .iter()
        .map(|r| r.url.path().to_owned())
        .collect();
    assert!(!after.is_empty(), "models.dev was fetched from the stub");
    assert!(
        after.iter().all(|p| p == "/api.json"),
        "republication sends nothing for the kind: {after:?}"
    );
}

// ── AC11: credential gate, probes, the untouched API-key provider ──────────

#[tokio::test]
#[serial]
async fn plain_without_a_login_the_bundled_entry_is_locked_then_login_serves_the_seed() {
    let server = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();
    let _env = env(temp.path(), &server.uri());
    let _isolated = isolate(temp.path(), NO_MODELS_DEV);
    let cfg = app_config();
    let entry = effective_providers(&cfg)
        .into_iter()
        .find(|entry| entry.id == PROVIDER)
        .expect("google-gemini-cli is a bundled provider");
    assert_eq!(entry.origin, ProviderOrigin::Bundled);
    assert_eq!(entry.kind(), PROVIDER);
    // omp's default model, not the first seed row.
    assert_eq!(entry.cfg.model_id, "gemini-3.1-pro-preview");
    assert_eq!(entry.cfg.api_key_env, "");
    assert!(entry.env_vars().is_empty());
    assert_eq!(entry.sign_in_hint(), HINT);
    {
        // There is no environment credential: even a Google API key leaves it locked.
        let _key = Env::set(&[("GOOGLE_API_KEY", Some("AIza-gemini-cli-gate-fixture"))]);
        let snapshot = store().snapshot().await.unwrap();
        assert!(matches!(
            availability(&entry, &snapshot, &env_var_trimmed),
            Availability::Locked { hint } if hint == HINT
        ));
        let layer = init_provider_layer(&cfg);
        let locked = locked(&layer, PROVIDER);
        assert_eq!(locked.display_name, NAME);
        assert_eq!(locked.sign_in_hint, HINT);
        assert_eq!(rows(&locked.models), seed());
        assert_seed_capabilities(&locked.models);
        assert!(layer.router.get_provider(PROVIDER).is_none());
        assert!(
            layer
                .catalogue_targets
                .iter()
                .all(|t| t.provider_id != PROVIDER)
        );
        not_a_live_target(&layer);
        assert!(matches!(
            build_provider_for_config(PROVIDER, &config(None)).err(),
            Some(ProviderError::AuthenticationFailed)
        ));
    }
    assert!(server.received_requests().await.unwrap().is_empty());

    login(&server, ACCESS, PROJECT, EMAIL).await;
    let baseline = server.received_requests().await.unwrap().len();
    let snapshot = store().snapshot().await.unwrap();
    assert!(matches!(
        availability(&entry, &snapshot, &env_var_trimmed),
        Availability::Signed(AuthSource::StoredOAuth)
    ));
    let layer = init_provider_layer(&cfg);
    assert!(
        layer
            .registry
            .list_locked()
            .iter()
            .all(|p| p.provider_id != PROVIDER)
    );
    let provider = layer
        .router
        .get_provider(PROVIDER)
        .expect("built from the stored login");
    assert_eq!(provider.provider_id(), PROVIDER);
    let models = served(&layer);
    assert_eq!(rows(&models), seed());
    assert_seed_capabilities(&models);
    catalogue_target(&layer);
    not_a_live_target(&layer);
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        baseline,
        "registration is offline"
    );
}

#[tokio::test]
#[serial]
async fn plain_google_api_key_provider_is_unchanged_and_stays_on_chat_completions() {
    const GOOGLE_KEY: &str = "AIza-google-chat-fixture";
    let server = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();
    let _env = env(temp.path(), &server.uri());
    let _isolated = isolate(temp.path(), NO_MODELS_DEV);
    let cfg = app_config();
    let entry = effective_providers(&cfg)
        .into_iter()
        .find(|entry| entry.id == "google")
        .expect("google is a bundled provider");
    assert_eq!(entry.kind(), "google");
    assert_eq!(entry.cfg.api_key_env, "GOOGLE_API_KEY");
    assert_eq!(entry.env_vars(), vec!["GOOGLE_API_KEY"]);
    assert_eq!(
        entry.sign_in_hint(),
        "Set GOOGLE_API_KEY or run `rustain auth login google`"
    );
    let layer = init_provider_layer(&cfg);
    let google = locked(&layer, "google");
    assert_eq!(google.display_name, "Google AI");
    assert_eq!(
        google
            .models
            .iter()
            .map(|m| (
                m.model_id.as_str(),
                m.display_name.as_str(),
                m.context_window,
                m.pricing_tier.as_deref()
            ))
            .collect::<Vec<_>>(),
        [
            (
                "gemini-3.1-pro-preview",
                "Gemini 3.1 Pro",
                1_048_576,
                Some("flagship")
            ),
            (
                "gemini-3.1-flash-lite-preview",
                "Gemini 3.1 Flash Lite",
                1_048_576,
                Some("cheap")
            ),
        ]
    );

    // A key signs `google` onto the OpenAI-compatible Chat route, never Cloud Code Assist.
    let _key = Env::set(&[("GOOGLE_API_KEY", Some(GOOGLE_KEY))]);
    let chat = [
        json!({"id":"c","object":"chat.completion.chunk","created":0,"model":MODEL,"choices":[{"index":0,"delta":{"role":"assistant","content":"google chat"},"finish_reason":null}]}),
        json!({"id":"c","object":"chat.completion.chunk","created":0,"model":MODEL,"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
    ]
    .iter()
    .map(|frame| format!("data: {frame}\n\n"))
    .chain(["data: [DONE]\n\n".to_owned()])
    .collect::<String>();
    Mock::given(method("POST"))
        .and(path("/v1beta/openai/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(chat, "text/event-stream"))
        .mount(&server)
        .await;
    let mut google_cfg = config(Some(&format!("{}/v1beta/openai", server.uri())));
    google_cfg.provider_id = "google".into();
    google_cfg.kind = Some("google".into());
    google_cfg.api_key_env = "GOOGLE_API_KEY".into();
    let provider = build_provider_for_config("google", &google_cfg).expect("signed by the key");
    assert_eq!(provider.provider_id(), "google");
    let chunks: Vec<StreamChunk> = tokio::time::timeout(GUARD, async {
        provider
            .stream_completion(vec![user("hi")], options(MODEL))
            .await
            .expect("the Chat route answers")
            .collect::<Vec<_>>()
            .await
    })
    .await
    .expect("stream finished");
    assert!(
        chunks
            .iter()
            .any(|c| matches!(c, StreamChunk::Text { content, .. } if content == "google chat")),
        "{chunks:?}"
    );
    let requests = server.received_requests().await.unwrap();
    let posts = hits(&requests, "/v1beta/openai/chat/completions");
    assert_eq!(posts.len(), 1);
    assert!(header(posts[0], "authorization").ends_with(GOOGLE_KEY));
    assert!(hits(&requests, STREAM).is_empty());
    assert!(hits(&requests, LOAD).is_empty());
    assert!(build_provider_for_config_with_key("google", &google_cfg, "candidate").is_ok());
}

#[test]
fn plain_api_key_construction_is_refused_for_the_subscription_kind() {
    for id in [PROVIDER, "gemini"] {
        assert!(
            matches!(
                build_provider_for_config_with_key(id, &config(None), "candidate-key").err(),
                Some(ProviderError::Other(message)) if message == "google-gemini-cli has no API-key login"
            ),
            "{id}"
        );
    }
}

#[tokio::test]
#[serial]
async fn plain_health_and_probe_map_load_code_assist_statuses() {
    let server = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();
    let _env = env(temp.path(), &server.uri());
    login(&server, ACCESS, PROJECT, EMAIL).await;
    let provider = build();
    let other = |status: u16| format!("Other(\"Cloud Code Assist loadCodeAssist HTTP {status}\")");
    for (status, health, probe) in [
        (200_u16, "ok".to_owned(), "ok".to_owned()),
        (
            403,
            "AuthenticationFailed".to_owned(),
            "AuthenticationFailed".to_owned(),
        ),
        (404, "EndpointUnsupported(404)".to_owned(), other(404)),
        (405, "EndpointUnsupported(405)".to_owned(), other(405)),
        (500, other(500), other(500)),
    ] {
        server.reset().await;
        Mock::given(method("POST"))
            .and(path(LOAD))
            .respond_with(ResponseTemplate::new(status).set_body_json(json!({})))
            .mount(&server)
            .await;
        assert_eq!(
            port(provider.health_check().await),
            health,
            "health {status}"
        );
        assert_eq!(
            port(provider.connectivity_probe().await.map(|_| ())),
            probe,
            "probe {status}"
        );
        let requests = server.received_requests().await.unwrap();
        let loads = hits(&requests, LOAD);
        assert_eq!(loads.len(), 2, "health and probe once each: {status}");
        assert_eq!(requests.len(), 2, "status-only probes: {status}");
        for load in loads {
            assert_eq!(header(load, "authorization"), format!("Bearer {ACCESS}"));
            assert_eq!(header(load, "content-type"), "application/json");
            assert_eq!(header(load, "user-agent"), user_agent(MODEL));
            assert_eq!(header(load, "client-metadata"), CLIENT_METADATA);
            assert_eq!(String::from_utf8_lossy(&load.body), FREE_LOAD_BODY);
        }
    }
}

// ── K12: the real debug binary ──────────────────────────────────────────────

const NOTE: &str = "GEMINI-NOTE-CONTENT";
const THOUGHT: &str = "GEMINI-THOUGHT-MARK planning the read.";
const VISIBLE: &str = "Reading the notes.";
const ANSWER: &str = "Gemini tool round answered";
const SIGNATURES: [&str; 3] = ["U0lHMA==", "U0lHMQ==", "U0lHMg=="];

pub(crate) struct Fixture {
    pub(crate) home: tempfile::TempDir,
    evidence: PathBuf,
    binary: PathBuf,
    env: BTreeMap<&'static str, OsString>,
    /// Held for the whole run: the binary must fall back from the port it is given.
    held: std::net::TcpListener,
}

impl Fixture {
    pub(crate) fn new(server: &MockServer) -> Self {
        let home = tempfile::tempdir().unwrap();
        let evidence = std::env::var_os("RUSTAIN_TEST_GEMINI_CLI_EVIDENCE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                tempfile::Builder::new()
                    .prefix("rustain-gemini-cli-evidence-")
                    .tempdir()
                    .unwrap()
                    .keep()
            });
        std::fs::create_dir_all(&evidence).unwrap();
        let evidence = evidence.canonicalize().unwrap();
        // A reused evidence destination must not make a failed rerun look passed.
        let passed = evidence.join("passed.json");
        if passed.exists() {
            std::fs::remove_file(passed).unwrap();
        }
        let explicit_binary = std::env::var_os("RUSTAIN_TEST_GEMINI_CLI_BINARY");
        let binary = explicit_binary
            .clone()
            .map(PathBuf::from)
            .unwrap_or_else(|| assert_cmd::cargo::cargo_bin("rustain"))
            .canonicalize()
            .expect("build the real rustain binary with --features test-instrumentation first");
        let held = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let held_port = held.local_addr().unwrap().port();
        let mut env = BTreeMap::new();
        for (key, value) in [
            ("HOME", home.path().to_owned().into_os_string()),
            (
                "RUSTAIN_DATA_DIR",
                home.path().join("data").into_os_string(),
            ),
            (
                "RUSTAIN_CONFIG_DIR",
                home.path().join("config").into_os_string(),
            ),
            (
                "XDG_CONFIG_HOME",
                home.path().join("xdg-config").into_os_string(),
            ),
            (
                "XDG_DATA_HOME",
                home.path().join("xdg-data").into_os_string(),
            ),
            (
                "XDG_CACHE_HOME",
                home.path().join("xdg-cache").into_os_string(),
            ),
            ("RUSTAIN_TEST_AUTH_ORIGIN", server.uri().into()),
            (
                "RUSTAIN_TEST_AUTH_CALLBACK_PORT",
                held_port.to_string().into(),
            ),
            ("RUSTAIN_TEST_AUTH_SLEEP_SCALE", "0".into()),
            ("RUSTAIN_TEST_GEMINI_CLI_RETRY_SCALE", "0".into()),
            ("RUSTAIN_TEST_NOW_MS", NOW.to_string().into()),
            ("RUSTAIN_MODELS_DEV_URL", server.uri().into()),
            ("NO_PROXY", "*".into()),
        ] {
            env.insert(key, value);
        }
        let fixture = Self {
            home,
            evidence,
            binary,
            env,
            held,
        };
        std::fs::create_dir_all(fixture.home.path().join("config")).unwrap();
        let config = "model = \"gemini-3.1-pro-preview\"\n\n[provider.gemini]\nkind = \"google-gemini-cli\"\nprovider_id = \"google-gemini-cli\"\nmodel_id = \"gemini-3.1-pro-preview\"\napi_key_env = \"\"\nenabled = true\ndiscover_models = true\n";
        std::fs::write(fixture.home.path().join("config/config.toml"), config).unwrap();
        std::fs::write(fixture.evidence.join("config.toml"), config).unwrap();
        fixture.json("binary.json", &json!({
            "path": fixture.binary,
            "selection": if explicit_binary.is_some() { "RUSTAIN_TEST_GEMINI_CLI_BINARY" } else { "assert_cmd::cargo::cargo_bin" },
            "required_build_feature": "test-instrumentation",
            "instrumentation_verified_by": "exact printed authorize URL origin before any launch or callback request",
            "bytes": std::fs::metadata(&fixture.binary).unwrap().len(),
        }));
        println!("Gemini CLI evidence: {}", fixture.evidence.display());
        fixture
    }

    fn held_port(&self) -> u16 {
        self.held.local_addr().unwrap().port()
    }

    pub(crate) fn json(&self, file: &str, value: &Value) {
        std::fs::write(
            self.evidence.join(file),
            serde_json::to_vec_pretty(value).unwrap(),
        )
        .unwrap();
    }

    fn command(&self, label: &str, args: &[&str]) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(&self.binary);
        command
            .env_clear()
            .envs(&self.env)
            .current_dir(self.home.path())
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        self.json(&format!("{label}.command.json"), &json!({
            "program": self.binary, "args": args, "cwd": self.home.path(),
            "env_clear": true,
            "env": self.env.iter().map(|(key, value)| (*key, value.to_string_lossy().into_owned())).collect::<BTreeMap<_, _>>(),
            "stdin": "null", "timeout_seconds": COMMAND_TIMEOUT.as_secs(),
        }));
        command
    }

    fn capture(&self, label: &str, output: &Output, stderr: &[u8]) {
        std::fs::write(
            self.evidence.join(format!("{label}.stdout")),
            &output.stdout,
        )
        .unwrap();
        std::fs::write(self.evidence.join(format!("{label}.stderr")), stderr).unwrap();
        self.json(
            &format!("{label}.exit.json"),
            &json!({"success": output.status.success(), "code": output.status.code()}),
        );
    }

    pub(crate) async fn run(&self, label: &str, args: &[&str]) -> Output {
        let output = tokio::time::timeout(COMMAND_TIMEOUT, self.command(label, args).output())
            .await
            .unwrap_or_else(|_| panic!("{label} exceeded {COMMAND_TIMEOUT:?}"))
            .unwrap_or_else(|error| panic!("cannot execute {label}: {error}"));
        self.capture(label, &output, &output.stderr);
        assert!(
            output.status.success(),
            "{label}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    pub(crate) async fn requests(&self, server: &MockServer, label: &str) -> Vec<Request> {
        let requests = server.received_requests().await.unwrap();
        let values: Vec<_> = requests.iter().map(|request| json!({
            "method": request.method.as_str(), "url": request.url.as_str(),
            "headers": request.headers.iter().map(|(key, value)| (key.as_str(), value.to_str().unwrap())).collect::<BTreeMap<_, _>>(),
            // Evidence keeps the form shape but never the embedded client secret.
            "body_utf8": String::from_utf8_lossy(&request.body).replace(CLIENT_SECRET, "[client-secret]"),
        })).collect();
        self.json(&format!("{label}.requests.json"), &json!(values));
        requests
    }
}

fn provider_row<'a>(value: &'a Value, id: &str) -> &'a Value {
    value["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["provider"] == id)
        .unwrap_or_else(|| panic!("{id} row: {value}"))
}

/// `auth login google-gemini-cli` driven like a browser. Returns the output, the
/// whole stderr transcript and the actual redirect URI.
pub(crate) async fn cli_login(fixture: &Fixture, server: &MockServer) -> (Output, String, String) {
    let mut child = fixture
        .command("login", &["auth", "login", PROVIDER])
        .spawn()
        .unwrap();
    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let mut transcript = String::new();
    let (authorize, launch) = tokio::time::timeout(COMMAND_TIMEOUT, async {
        let mut authorize = None;
        loop {
            let mut line = String::new();
            assert_ne!(
                stderr.read_line(&mut line).await.unwrap(),
                0,
                "login exited before URLs: {transcript}"
            );
            transcript.push_str(&line);
            std::fs::write(fixture.evidence.join("login.stderr"), &transcript).unwrap();
            assert!(transcript.len() <= 65_536, "unbounded login stderr");
            if line.trim().starts_with("http") {
                authorize = Some(url::Url::parse(line.trim()).unwrap());
            }
            if let Some(launch) = line.trim().strip_prefix("Local browser launch: ") {
                break (
                    authorize.expect("authorize URL must precede launch URL"),
                    url::Url::parse(launch).unwrap(),
                );
            }
        }
    })
    .await
    .expect("login did not print URLs within its deadline");

    // This is the safety gate: never drive even /launch before proving that the
    // actual running binary rebased its printed authorization URL to our stub.
    let origin = url::Url::parse(&server.uri()).unwrap();
    assert_eq!(origin.scheme(), "http");
    assert_eq!(origin.host_str(), Some("127.0.0.1"));
    assert_eq!(
        authorize.origin(),
        origin.origin(),
        "binary MUST be built with test-instrumentation: {authorize}"
    );
    assert_eq!(authorize.path(), "/o/oauth2/v2/auth");
    let pairs = query(&authorize);
    let keys: Vec<_> = pairs.iter().map(|(key, _)| key.as_str()).collect();
    assert_eq!(
        keys,
        [
            "client_id",
            "response_type",
            "redirect_uri",
            "scope",
            "state",
            "access_type",
            "prompt"
        ]
    );
    let query: BTreeMap<_, _> = pairs.into_iter().collect();
    assert_eq!(query["client_id"], CLIENT_ID);
    let state = query["state"].clone();
    assert_eq!(state.len(), 32);
    assert!(state.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')));
    // The held port is unavailable: the binary chose and reported its own.
    let held = fixture.held_port();
    let preferred = format!("Preferred port {held} unavailable, using port ");
    let actual: u16 = transcript
        .lines()
        .find_map(|line| line.strip_prefix(preferred.as_str()))
        .unwrap_or_else(|| panic!("no fallback line: {transcript}"))
        .trim()
        .parse()
        .unwrap();
    assert_ne!(actual, held);
    let redirect = query["redirect_uri"].clone();
    let mut callback = url::Url::parse(&redirect).unwrap();
    for url in [&callback, &launch] {
        assert_eq!(url.scheme(), "http");
        assert_eq!(url.host_str(), Some("127.0.0.1"));
        assert_eq!(url.port(), Some(actual));
        assert!(url.username().is_empty());
        assert!(url.password().is_none());
    }
    assert_eq!(callback.path(), "/oauth2callback");
    assert_eq!(launch.path(), "/launch");
    let lines: Vec<_> = transcript.lines().collect();
    let at = lines
        .iter()
        .position(|line| line.trim().starts_with("http"))
        .unwrap();
    assert_eq!(lines[at - 1], "Complete the sign-in in your browser.");
    fixture.json(
        "safety.json",
        &json!({
            "authorize_url_from_child_stderr": authorize.as_str(),
            "expected_loopback_origin": origin.origin().ascii_serialization(),
            "actual_authorize_origin": authorize.origin().ascii_serialization(),
            "exact_loopback_origin_asserted_before_any_http": true,
            "launch_url_from_child_stderr": launch.as_str(),
            "callback_before_fixture_code": callback.as_str(),
            "held_port": held,
            "fallback_port": actual,
            "http_actions_started": false,
        }),
    );
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let response = client.get(launch.clone()).send().await.unwrap();
    assert_eq!(response.status(), 302);
    assert_eq!(response.headers()["location"], authorize.as_str());
    fixture.json("launch.json", &json!({"url": launch.as_str(), "status": response.status().as_u16(), "location": response.headers()["location"].to_str().unwrap()}));
    callback
        .query_pairs_mut()
        .append_pair("code", CODE)
        .append_pair("state", &state);
    let response = client.get(callback.clone()).send().await.unwrap();
    assert!(response.status().is_success());
    fixture.json("callback.json", &json!({"url": callback.as_str(), "state_matches_authorize": true, "status": response.status().as_u16()}));
    let (output, read) = tokio::time::timeout(COMMAND_TIMEOUT, async {
        tokio::join!(
            child.wait_with_output(),
            stderr.read_to_string(&mut transcript)
        )
    })
    .await
    .expect("login did not exit within its deadline");
    read.unwrap();
    let output = output.unwrap();
    fixture.capture("login", &output, transcript.as_bytes());
    assert!(output.status.success(), "{transcript}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "Logged in to Google Cloud Code Assist (Gemini CLI) as operator@example.test"
    );
    (output, transcript, redirect)
}

/// One `streamGenerateContent` SSE body; the last frame carries the finish reason.
fn sse(frames: &[Value]) -> ResponseTemplate {
    let body = frames
        .iter()
        .map(|parts| format!("data: {}\n\n", json!({"response": {"candidates": [{"content": {"role": "model", "parts": parts}}]}})))
        .chain([format!(
            "data: {}\n\n",
            json!({"response": {"candidates": [{"content": {"role": "model", "parts": []}, "finishReason": "STOP"}], "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 3, "thoughtsTokenCount": 2, "totalTokenCount": 15}}})
        )])
        .collect::<String>();
    ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
}

pub(crate) fn stream_posts(requests: &[Request]) -> Vec<&Request> {
    requests
        .iter()
        .filter(|r| r.method == "POST" && r.url.path() == STREAM)
        .collect()
}

#[tokio::test]
#[serial]
async fn k12_real_debug_binary_login_status_two_turn_ask_logout() {
    // No sleeps or polling: URL publication gates the callback, and each
    // subprocess is bounded, including shutdown. The outer deadline bounds the flow.
    tokio::time::timeout(Duration::from_secs(120), async {
        let server = MockServer::start().await;
        let fixture = Fixture::new(&server);
        // Free tier: `done:false` onboarding, then exactly one (done) poll.
        mount(&server, Google {
            token: ok(token_body(ACCESS, Some(REFRESH))),
            userinfo: ok(json!({"email": EMAIL, "id": "1"})),
            load: ok(json!({"allowedTiers": [{"id": "free-tier", "isDefault": true}]})),
            onboard: ok(json!({"name": "operations/fixture-op", "done": false})),
            polls: vec![ok(json!({"done": true, "response": {"cloudaicompanionProject": {"id": PROJECT}}}))],
        }).await;
        Mock::given(method("GET")).and(path("/api.json"))
            .respond_with(ok(json!({}))).mount(&server).await;
        // Turn 1: signed thinking, signed text, a signed real `Read` call. Turn 2: the answer.
        Mock::given(method("POST")).and(path(STREAM))
            .respond_with(sse(&[
                json!([{"thought": true, "text": THOUGHT, "thoughtSignature": SIGNATURES[0]}]),
                json!([{"text": VISIBLE}]),
                json!([{"text": "", "thoughtSignature": SIGNATURES[1]}]),
                json!([{"functionCall": {"name": "Read", "args": {"file_path": "notes.txt"}}, "thoughtSignature": SIGNATURES[2]}]),
            ]))
            .up_to_n_times(1).mount(&server).await;
        Mock::given(method("POST")).and(path(STREAM))
            .respond_with(sse(&[json!([{"text": ANSWER}])]))
            .up_to_n_times(1).mount(&server).await;

        let (login, transcript, redirect) = cli_login(&fixture, &server).await;
        let checking = transcript.find("Checking for existing Cloud Code Assist project...\n")
            .unwrap_or_else(|| panic!("{transcript}"));
        let provisioning = transcript.find("Provisioning Cloud Code Assist project (this may take a moment)...\n")
            .unwrap_or_else(|| panic!("{transcript}"));
        assert!(checking < provisioning);
        assert!(!transcript.contains("Waiting for project provisioning"), "one immediate done poll: {transcript}");
        assert!(!transcript.contains("Discovered"), "catalogue-only login claims no discovery: {transcript}");

        let requests = fixture.requests(&server, "login").await;
        let provider_paths: Vec<_> = requests.iter().map(|r| r.url.path()).filter(|p| *p != "/api.json").collect();
        assert_eq!(provider_paths, [TOKEN, USERINFO, LOAD, ONBOARD, OPERATION]);
        let token = hits(&requests, TOKEN)[0];
        assert_eq!(header(token, "content-type"), "application/x-www-form-urlencoded");
        let exchange = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs([
                ("grant_type", "authorization_code"),
                ("client_id", CLIENT_ID),
                ("client_secret", CLIENT_SECRET),
                ("code", CODE),
                ("redirect_uri", redirect.as_str()),
            ])
            .finish();
        assert_eq!(String::from_utf8_lossy(&token.body), exchange);
        let load = hits(&requests, LOAD)[0];
        assert_eq!(String::from_utf8_lossy(&load.body), FREE_LOAD_BODY);
        assert_eq!(header(load, "user-agent"), user_agent(MODEL));
        assert_eq!(header(load, "client-metadata"), CLIENT_METADATA);
        let auth_json = fixture.home.path().join("data/auth.json");
        let store = FileAuthStore::at(auth_json.clone());
        let snapshot = store.snapshot().await.unwrap();
        let stored = snapshot.rows(PROVIDER);
        assert_eq!(stored.len(), 1);
        let Credential::OAuth(row) = &stored[0].credential else { panic!("an OAuth row") };
        assert_eq!(row.identity.project_id.as_deref(), Some(PROJECT));
        assert_eq!(row.identity.email.as_deref(), Some(EMAIL));
        let file = std::fs::read_to_string(&auth_json).unwrap();
        assert!(!file.contains(CLIENT_SECRET) && !file.contains(CODE));
        std::fs::copy(&auth_json, fixture.evidence.join("auth-after-login.json")).unwrap();

        let status = fixture.run("status", &["auth", "status", "--json"]).await;
        let status_json: Value = serde_json::from_slice(&status.stdout).unwrap();
        assert_eq!(provider_row(&status_json, PROVIDER)["source"], "oauth");

        // A real built-in tool, run non-interactively inside the temporary HOME.
        std::fs::write(fixture.home.path().join("notes.txt"), format!("{NOTE}\n")).unwrap();
        let ask = fixture.run("ask", &["ask", "read the notes", "--output-format", "json", "--yolo"]).await;
        let ask_out = String::from_utf8_lossy(&ask.stdout).into_owned();
        let ask_err = String::from_utf8_lossy(&ask.stderr).into_owned();
        for marker in SIGNATURES.iter().copied().chain(["thoughtSignature", "gemini_parts", "GEMINI-THOUGHT-MARK"]) {
            assert!(!ask_out.contains(marker) && !ask_err.contains(marker), "{marker} leaked: {ask_out}");
        }
        let ask_json: Value = serde_json::from_str(&ask_out).unwrap();
        // Landed `ask --output-format json` contract (`ask.rs` `finish`): without
        // `--final-message-only`, `response` is every visible text chunk of the
        // run, in order; the flag narrows it to the final block
        // (`cli_ask.rs::p0_13_1b_final_message_json`). 20-1d1b's K08 obeys the
        // same rule; its turn N carries no visible text. Thinking never joins.
        assert_eq!(ask_json["response"], format!("{VISIBLE}{ANSWER}"));
        assert!(!ask_json["response"].as_str().unwrap().contains(THOUGHT));
        let calls = ask_json["tool_calls"].as_array().unwrap();
        assert_eq!(calls.len(), 1, "{ask_json}");
        assert_eq!(calls[0]["name"], "Read");
        assert_eq!(calls[0]["input"], json!({"file_path": "notes.txt"}));
        assert_eq!(calls[0]["result"]["is_error"], false);
        assert!(calls[0]["result"]["content"].as_str().unwrap().contains(NOTE));

        let requests = fixture.requests(&server, "ask").await;
        let posts = stream_posts(&requests);
        assert_eq!(posts.len(), 2, "turn N, then turn N+1 after the tool round");
        assert!(requests.iter().all(|r| !r.url.path().ends_with("/chat/completions")), "Chat fallback is forbidden");
        assert_eq!(hits(&requests, TOKEN).len(), 1, "a fresh login is never refreshed");
        assert_eq!(hits(&requests, LOAD).len(), 1, "no request re-runs provisioning");
        for post in &posts {
            assert_eq!(post.url.query(), Some("alt=sse"));
            for (key, value) in [
                ("authorization", format!("Bearer {ACCESS}")),
                ("content-type", "application/json".into()),
                ("accept", "text/event-stream".into()),
                ("user-agent", user_agent(MODEL)),
                ("client-metadata", CLIENT_METADATA.into()),
            ] {
                assert_eq!(header(post, key), value, "{key}");
            }
            assert!(!post.headers.contains_key("x-goog-api-key"));
            let raw = String::from_utf8_lossy(&post.body);
            assert!(
                raw.starts_with(r#"{"project":"gemini-fixture-project","model":"gemini-3.1-pro-preview","request":{"contents":["#),
                "envelope literal: {raw}"
            );
        }
        let first: Value = posts[0].body_json().unwrap();
        let second: Value = posts[1].body_json().unwrap();
        fixture.json("gemini-request-1.json", &first);
        fixture.json("gemini-request-2.json", &second);
        assert_eq!(first["project"], PROJECT);
        assert_eq!(
            first["request"]["generationConfig"],
            json!({"maxOutputTokens": 8192, "thinkingConfig": {"includeThoughts": true, "thinkingLevel": "LOW"}})
        );
        assert!(first["request"].get("toolConfig").is_none());
        let declared: Vec<_> = first["request"]["tools"][0]["functionDeclarations"].as_array().unwrap()
            .iter().map(|d| d["name"].as_str().unwrap()).collect();
        assert!(declared.contains(&"Read"), "{declared:?}");
        let first_contents = first["request"]["contents"].as_array().unwrap();
        let prompt = first_contents.last().unwrap();
        assert_eq!(prompt["role"], "user");
        assert!(prompt["parts"].as_array().unwrap().iter().any(|p| p["text"].as_str().is_some_and(|t| t.contains("read the notes"))), "{prompt}");
        assert!(!String::from_utf8_lossy(&posts[0].body).contains("thoughtSignature"));

        // Turn N+1 replays turn N's parts with their signatures, then the result.
        let contents = second["request"]["contents"].as_array().unwrap();
        let models: Vec<_> = contents.iter().enumerate().filter(|(_, c)| c["role"] == "model").collect();
        assert_eq!(models.len(), 1, "{second}");
        let (at, model) = models[0];
        assert_eq!(
            *model,
            json!({"role": "model", "parts": [
                {"thought": true, "text": THOUGHT, "thoughtSignature": SIGNATURES[0]},
                {"text": VISIBLE, "thoughtSignature": SIGNATURES[1]},
                {"functionCall": {"name": "Read", "args": {"file_path": "notes.txt"}}, "thoughtSignature": SIGNATURES[2]},
            ]})
        );
        let result = &contents[at + 1];
        assert_eq!(result["role"], "user");
        let response = &result["parts"][0]["functionResponse"];
        assert_eq!(response["name"], "Read");
        assert!(response.get("id").is_none());
        assert!(response["response"]["output"].as_str().unwrap().contains(NOTE), "{result}");
        let raw = String::from_utf8_lossy(&posts[1].body);
        for signature in SIGNATURES {
            assert_eq!(raw.matches(signature).count(), 1, "{signature} sent once");
        }
        assert!(!raw.contains("callId") && !raw.contains("skip_thought_signature_validator"));

        let logout = fixture.run("logout", &["auth", "logout", PROVIDER]).await;
        assert_eq!(String::from_utf8_lossy(&logout.stdout).trim(), "Logged out of google-gemini-cli.");
        assert!(store.snapshot().await.unwrap().rows(PROVIDER).is_empty());
        std::fs::copy(&auth_json, fixture.evidence.join("auth-after-logout.json")).unwrap();
        let list = fixture.run("list", &["auth", "list", "--json"]).await;
        let list_json: Value = serde_json::from_slice(&list.stdout).unwrap();
        let gemini = provider_row(&list_json, PROVIDER);
        assert_eq!(gemini["configured"], false);
        assert_eq!(gemini["auth_methods"], json!(["oauth"]));
        assert_eq!(gemini["display_name"], NAME);
        assert_eq!(provider_row(&list_json, "gemini")["is_default"], true, "the explicit home config stays active");
        let final_requests = fixture.requests(&server, "final").await;
        assert_eq!(final_requests.len(), requests.len(), "logout/list make no provider requests");
        assert!(final_requests.iter().all(|r| !r.url.path().ends_with("/chat/completions")));

        let all_output = format!("{transcript}{}{}{}{}{}{}{}{}{}",
            String::from_utf8_lossy(&login.stdout), String::from_utf8_lossy(&status.stdout), String::from_utf8_lossy(&status.stderr),
            ask_out, ask_err, String::from_utf8_lossy(&logout.stdout),
            String::from_utf8_lossy(&logout.stderr), String::from_utf8_lossy(&list.stdout), String::from_utf8_lossy(&list.stderr));
        for secret in [ACCESS, REFRESH, CODE, CLIENT_SECRET] {
            assert!(!all_output.contains(secret), "fixture credential leaked into CLI output");
        }
        // Written only after every runtime assertion above succeeded. This is
        // not a substitute for cargo's result; failed runs have no pass marker.
        fixture.json("passed.json", &json!({
            "test": "conformance_20_1d4_gemini_cli::cli::k12_real_debug_binary_login_status_two_turn_ask_logout",
            "login_exit": login.status.code(), "status_exit": status.status.code(), "ask_exit": ask.status.code(),
            "logout_exit": logout.status.code(), "list_exit": list.status.code(),
            "login_requests": [TOKEN, USERINFO, LOAD, ONBOARD, OPERATION],
            "status_source": provider_row(&status_json, PROVIDER)["source"],
            "response": ask_json["response"], "tool": "Read",
            "stream_posts": posts.len(), "envelope_project": first["project"],
            "replayed_signatures_on_post_2": SIGNATURES,
            "configured_after_logout": gemini["configured"],
            "chat_completions_requests": 0, "discovered_line": false,
            "fixture_secrets_only": true,
        }));
    }).await.expect("CLI fixture exceeded its structural 120-second deadline");
}
