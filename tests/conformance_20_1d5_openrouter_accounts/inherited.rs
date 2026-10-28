//! Inherited stored-row fixes, each through its production front door:
//! - a subscription row leaves only for an origin its own rows key accepts,
//!   under any configured kind (Codex and Gemini rows behind `kind = "openai"`);
//! - a login-only kind serves its stored login alone, never a key or env value;
//! - a project-keyed Gemini row is claimed by the same project's identified login;
//! - a Copilot login supersedes every stored grant, its discovery facts (live
//!   ids, output limits) belong to the credential that listed them, and a
//!   session is warm only for the account whose response warmed it;
//! - routing is stored only on rows without an identity key, so the identity
//!   cache-scope form never needs it.

use crate::conformance_20_1d1b_codex_replay_discovery::common::{
    World, completed, enc, login as codex_login, reasoning,
};
use crate::conformance_20_1d3_copilot::chat_d4::entry;
use crate::conformance_20_1d3_copilot::common::{
    ENDPOINT_A, PROVIDER as COPILOT, ROOT_A, assistant, build, chat_path, complete, header,
    json_body, json_reply, login as copilot_login, mount_routes, options, reply, requests,
    responses_path, responses_sse, responses_text, user,
};
use crate::conformance_20_1d4_gemini_cli::auth::{
    self as gemini, EMAIL, Env, PROJECT, Sleeps, google, login_with, mount,
};
use rustain::adapters::auth_store::FileAuthStore;
use rustain::adapters::model_catalog_cache::ModelCatalogCache;
use rustain::domain::errors::ProviderError;
use rustain::domain::models::provider::DiscoveryStatus;
use rustain::domain::models::{AppConfig, Credential, ProviderConfig, StreamChunk};
use rustain::domain::ports::AuthStorePort;
use rustain::infrastructure::auth_accounts::accounts;
use rustain::infrastructure::model_discovery::refresh_target;
use rustain::infrastructure::provider_availability::{
    Availability, availability, effective_providers,
};
use rustain::infrastructure::provider_factory::build_provider_for_config;
use rustain::infrastructure::startup::init_provider_layer;
use serde_json::{Value, json};
use serial_test::serial;
use std::sync::Arc;
use wiremock::{MockServer, ResponseTemplate};

fn store() -> Arc<dyn AuthStorePort> {
    Arc::new(FileAuthStore::new())
}

fn app(cfg: &ProviderConfig) -> AppConfig {
    let mut app = AppConfig::default();
    app.provider.insert(cfg.provider_id.clone(), cfg.clone());
    app
}

/// What availability, the factory, the accounts service and startup say
/// about the configured entry `id`: locked with `hint`, never built, no source.
async fn assert_locked(app: &AppConfig, id: &str, hint: &str) {
    let entry = effective_providers(app)
        .into_iter()
        .find(|entry| entry.id == id)
        .unwrap();
    let snapshot = store().snapshot().await.unwrap();
    match availability(
        &entry,
        &snapshot,
        &rustain::infrastructure::utils::env_var_trimmed,
    ) {
        Availability::Locked { hint: got } => assert_eq!(got, hint),
        other => panic!("{id} must stay locked, got {other:?}"),
    }
    assert!(matches!(
        build_provider_for_config(id, &app.provider[id]),
        Err(ProviderError::AuthenticationFailed)
    ));
    assert_eq!(
        accounts(&store(), app, entry.rows_key())
            .await
            .unwrap()
            .served,
        None
    );
    let layer = init_provider_layer(app);
    assert!(layer.router.get_provider(id).is_none());
    assert!(
        layer
            .registry
            .list_locked()
            .iter()
            .any(|locked| locked.provider_id == id)
    );
}

// ── DF-20-1C-CROSS-KIND-OAUTH-ORIGIN ────────────────────────────────────────

/// A Codex grant behind `[provider.openai-codex] kind = "openai"` with a
/// foreign base: the generic kind has no origin rule, but the row's own key
/// does, so the entry is locked and the foreign origin sees nothing.
#[tokio::test]
#[serial]
async fn a_codex_grant_never_leaves_for_a_foreign_origin_under_a_generic_kind() {
    let server = MockServer::start().await;
    let foreign = MockServer::start().await;
    let _world = World::new(&server);
    codex_login(&server, "acct-cross-kind").await;
    let cfg = entry(&foreign, "openai-codex", "openai", "");
    assert_locked(
        &app(&cfg),
        "openai-codex",
        "Run `rustain auth login openai-codex`",
    )
    .await;
    assert!(foreign.received_requests().await.unwrap().is_empty());
    // The same row under its own kind is served (the row itself is usable).
    assert!(matches!(
        availability(
            &effective_providers(&AppConfig::default())
                .into_iter()
                .find(|entry| entry.id == "openai-codex")
                .unwrap(),
            &store().snapshot().await.unwrap(),
            &rustain::infrastructure::utils::env_var_trimmed,
        ),
        Availability::Signed(rustain::domain::models::AuthSource::StoredOAuth)
    ));
}

/// The Gemini CLI extension: its grant behind `kind = "openai"` stays home, and
/// a `google-gemini-cli` entry serves only its stored login — never a login
/// key written under its rows key, nor its configured `api_key_env`.
#[tokio::test]
#[serial]
async fn gemini_cli_rows_stay_home_and_a_login_only_kind_serves_its_login_alone() {
    let server = MockServer::start().await;
    let foreign = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let _env = gemini::env(dir.path(), &server.uri());
    gemini::login(&server, "GEMINI-ACCESS-CROSS", PROJECT, EMAIL).await;
    let alias = entry(&foreign, "google-gemini-cli", "openai", "");
    assert_locked(
        &app(&alias),
        "google-gemini-cli",
        "Run `rustain auth login google-gemini-cli`",
    )
    .await;
    assert!(foreign.received_requests().await.unwrap().is_empty());

    // A key written by the real API-key flow under `my-gemini`, and its env var.
    reply(
        &server,
        "GET",
        "/v1/models",
        json_reply(200, json!({"data": []})),
    )
    .await;
    let key = super::accounts::key_login_as(
        "my-gemini",
        &entry(&server, "my-gemini", "openai", ""),
        "sk-my-gemini-key",
    )
    .await
    .credential_id;
    assert_eq!(
        store().snapshot().await.unwrap().selected("my-gemini"),
        Some(key)
    );
    let _key_env = Env::set(&[("MY_GEMINI_KEY", Some("sk-my-gemini-env"))]);
    let cfg = ProviderConfig {
        provider_id: "my-gemini".into(),
        kind: Some("google-gemini-cli".into()),
        model_id: gemini::MODEL.into(),
        api_key_env: "MY_GEMINI_KEY".into(),
        auth_header: None,
        enabled: true,
        base_url: None,
        context_window: None,
        supports_tools: None,
        discover_models: true,
        model_filter: vec!["*".into()],
        cache_ttl_seconds: 3600,
    };
    let before = server.received_requests().await.unwrap().len();
    assert_locked(
        &app(&cfg),
        "my-gemini",
        "Run `rustain auth login my-gemini`",
    )
    .await;
    assert_eq!(server.received_requests().await.unwrap().len(), before);
}

// ── DF-20-1D4-IDENTITY-KEY-FALLBACK ─────────────────────────────────────────

/// A Gemini login whose userinfo failed keys its row by project; the same
/// project's later identified login claims that row (one account, one row).
/// Another project's unidentified login stays its own account.
#[tokio::test]
#[serial]
async fn an_identified_gemini_login_claims_the_project_keyed_row_of_its_project() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let _env = gemini::env(dir.path(), &server.uri());
    let unidentified = |access: &str, project: &str| {
        let mut google = google(access, project, EMAIL);
        google.userinfo = ResponseTemplate::new(500);
        google
    };
    mount(&server, unidentified("GEMINI-ACCESS-ONE", PROJECT)).await;
    let (ui, mut seen) = gemini::ui(&[]);
    let (first, _) = login_with(&server, ui, &mut seen, Sleeps::default().sleeper(None)).await;
    let first = first.unwrap();
    assert_eq!(first.identity.email, None);
    assert_eq!(first.identity.project_id.as_deref(), Some(PROJECT));

    server.reset().await;
    let second = gemini::login(&server, "GEMINI-ACCESS-TWO", PROJECT, EMAIL).await;
    assert_eq!(second.credential_id, first.credential_id);
    let snapshot = store().snapshot().await.unwrap();
    let rows = snapshot.rows(gemini::PROVIDER);
    assert_eq!(rows.len(), 1, "one account, one row");
    assert_eq!(rows[0].rev, 2);
    let Credential::OAuth(oauth) = &rows[0].credential else {
        panic!("a Gemini login stores a grant");
    };
    assert_eq!(oauth.identity.email.as_deref(), Some(EMAIL));
    assert_eq!(oauth.access.expose_secret(), "GEMINI-ACCESS-TWO");
    assert!(
        oauth.routing.is_empty(),
        "identity-bearing rows carry no routing"
    );
    assert_eq!(
        snapshot.selected(gemini::PROVIDER),
        Some(second.credential_id)
    );

    server.reset().await;
    mount(
        &server,
        unidentified("GEMINI-ACCESS-THREE", "another-project"),
    )
    .await;
    let (ui, mut seen) = gemini::ui(&[]);
    let (third, _) = login_with(&server, ui, &mut seen, Sleeps::default().sleeper(None)).await;
    assert_ne!(third.unwrap().credential_id, second.credential_id);
    assert_eq!(
        store()
            .snapshot()
            .await
            .unwrap()
            .rows(gemini::PROVIDER)
            .len(),
        2
    );
}

// ── DF-20-1D3-SCOPED-ADAPTER-STATE (+ SCOPE-IDENTITY-ROUTING) ───────────────

/// A Copilot login supersedes every stored grant, including a second one a
/// pre-fix store may hold; the surviving row carries routing and no identity
/// key, so its cache scope is the grant form that folds routing in.
#[tokio::test]
#[serial]
async fn a_copilot_login_supersedes_every_stored_grant() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    copilot_login(&server, "gho_legacy_one", ENDPOINT_A).await;
    // The legacy layout: an older binary superseded only the first grant, so a
    // store could hold two. Written by duplicating the real row under a new id.
    let path = world.dir.path().join("auth.json");
    let mut json: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let next = json["next_id"].as_u64().unwrap();
    let mut legacy = json["providers"][COPILOT][0].clone();
    legacy["id"] = json!(next);
    json["providers"][COPILOT]
        .as_array_mut()
        .unwrap()
        .push(legacy);
    json["next_id"] = json!(next + 1);
    std::fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();
    assert_eq!(store().snapshot().await.unwrap().rows(COPILOT).len(), 2);

    let current = copilot_login(&server, "gho_current", ENDPOINT_A).await;
    let snapshot = store().snapshot().await.unwrap();
    let rows = snapshot.rows(COPILOT);
    assert_eq!(rows.len(), 1, "the login supersedes every stored grant");
    assert_eq!(rows[0].id, current.credential_id);
    assert_eq!(snapshot.selected(COPILOT), Some(current.credential_id));
    let Credential::OAuth(oauth) = &rows[0].credential else {
        panic!("a Copilot login stores a grant");
    };
    assert_eq!(oauth.access.expose_secret(), "gho_current");
    assert!(!oauth.routing.is_empty(), "Copilot stores its routing");
    assert_eq!(
        oauth.identity.key(false),
        None,
        "routing lives only on rows without an identity key"
    );
}

/// Discovery facts belong to the credential that listed them: after a new
/// Copilot grant replaces the one whose live listing served `grok-code-fast-1`,
/// the same adapter treats the id as seed-only again (`store: false`).
#[tokio::test]
#[serial]
async fn copilot_live_listing_facts_answer_only_the_account_that_listed_them() {
    const MODEL: &str = "grok-code-fast-1";
    let server = MockServer::start().await;
    let _world = World::new(&server);
    copilot_login(&server, "gho_scope_a", ENDPOINT_A).await;
    mount_routes(&server, ROOT_A, "ok").await;
    reply(
        &server,
        "GET",
        &format!("{ROOT_A}/models"),
        json_reply(
            200,
            json!({"data": [{"id": MODEL, "name": "Grok Code Fast 1", "capabilities": {"type": "chat"}}]}),
        ),
    )
    .await;
    let app = AppConfig {
        model: "gpt-5.5".into(),
        ..Default::default()
    };
    let layer = init_provider_layer(&app);
    let target = layer
        .discovery_targets
        .iter()
        .find(|target| target.provider_id == COPILOT)
        .expect("a signed Copilot entry is a discovery target");
    let report = refresh_target(
        target,
        &ModelCatalogCache::new(),
        Some(&layer.registry),
        true,
    )
    .await;
    assert_eq!(report.status, DiscoveryStatus::Ok);
    let provider = layer.router.get_provider(COPILOT).unwrap();
    let chat = chat_path(ROOT_A);

    complete(&provider, vec![user("hi")], options(MODEL, 1024, None))
        .await
        .unwrap();
    let first = requests(&server, "POST", &chat).await;
    assert_eq!(
        header(&first[0], "authorization").as_deref(),
        Some("Bearer gho_scope_a")
    );
    assert!(
        json_body(&first[0]).get("store").is_none(),
        "listed live for this account"
    );

    copilot_login(&server, "gho_scope_b", ENDPOINT_A).await;
    complete(&provider, vec![user("hi")], options(MODEL, 1024, None))
        .await
        .unwrap();
    let second = requests(&server, "POST", &chat).await;
    assert_eq!(second.len(), 2);
    assert_eq!(
        header(&second[1], "authorization").as_deref(),
        Some("Bearer gho_scope_b")
    );
    assert_eq!(
        json_body(&second[1])["store"],
        false,
        "another account's listing never answers this account"
    );
}

/// The live output limit belongs to the account whose discovery listed it:
/// after another Copilot grant replaces that account, the same adapter no
/// longer clamps the request to the listed limit.
#[tokio::test]
#[serial]
async fn copilot_live_output_limits_clamp_only_the_account_that_listed_them() {
    // Not a bundled id, so its output limit comes from the live listing alone.
    const MODEL: &str = "scoped-limit-chat";
    let server = MockServer::start().await;
    let _world = World::new(&server);
    copilot_login(&server, "gho_limit_a", ENDPOINT_A).await;
    mount_routes(&server, ROOT_A, "ok").await;
    reply(
        &server,
        "GET",
        &format!("{ROOT_A}/models"),
        json_reply(
            200,
            json!({"data": [{"id": MODEL, "capabilities": {"type": "chat", "limits": {"max_output_tokens": 100}}}]}),
        ),
    )
    .await;
    let app = AppConfig {
        model: "gpt-5.5".into(),
        ..Default::default()
    };
    let layer = init_provider_layer(&app);
    let target = layer
        .discovery_targets
        .iter()
        .find(|target| target.provider_id == COPILOT)
        .expect("a signed Copilot entry is a discovery target");
    let report = refresh_target(
        target,
        &ModelCatalogCache::new(),
        Some(&layer.registry),
        true,
    )
    .await;
    assert_eq!(report.status, DiscoveryStatus::Ok);
    let provider = layer.router.get_provider(COPILOT).unwrap();

    complete(&provider, vec![user("hi")], options(MODEL, 1024, None))
        .await
        .unwrap();
    copilot_login(&server, "gho_limit_b", ENDPOINT_A).await;
    complete(&provider, vec![user("hi")], options(MODEL, 1024, None))
        .await
        .unwrap();
    let sent = requests(&server, "POST", &chat_path(ROOT_A)).await;
    assert_eq!(sent.len(), 2);
    assert_eq!(
        header(&sent[0], "authorization").as_deref(),
        Some("Bearer gho_limit_a")
    );
    assert_eq!(
        json_body(&sent[0])["max_completion_tokens"],
        100,
        "the listing account's own limit"
    );
    assert_eq!(
        header(&sent[1], "authorization").as_deref(),
        Some("Bearer gho_limit_b")
    );
    assert_eq!(
        json_body(&sent[1])["max_completion_tokens"],
        1024,
        "another account's listing never clamps this account"
    );
}

/// A session is warm per account: after another Copilot grant replaces the
/// account whose completed response warmed the session, the same adapter's
/// next turn in that session replays nothing.
#[tokio::test]
#[serial]
async fn copilot_warm_sessions_belong_to_the_account_that_warmed_them() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    copilot_login(&server, "gho_warm_a", ENDPOINT_A).await;
    reply(
        &server,
        "POST",
        &responses_path(ROOT_A),
        responses_sse(&[reasoning("acct"), responses_text("ok"), completed()]),
    )
    .await;
    let provider = build("gpt-5.5");
    let chunks = complete(
        &provider,
        vec![user("first")],
        options("gpt-5.5", 8192, Some("S")),
    )
    .await
    .unwrap();
    let items: Vec<_> = chunks
        .into_iter()
        .filter_map(|chunk| match chunk {
            StreamChunk::ProviderReplay { item } => Some(item),
            _ => None,
        })
        .collect();
    assert_eq!(items.len(), 1, "one captured replay item");
    let mut answer = assistant("ok");
    answer.provider_replay = items;
    let history = vec![user("first"), answer, user("second")];

    copilot_login(&server, "gho_warm_b", ENDPOINT_A).await;
    complete(&provider, history, options("gpt-5.5", 8192, Some("S")))
        .await
        .unwrap();
    let sent = requests(&server, "POST", &responses_path(ROOT_A)).await;
    assert_eq!(sent.len(), 2);
    assert_eq!(
        header(&sent[1], "authorization").as_deref(),
        Some("Bearer gho_warm_b")
    );
    assert!(
        !String::from_utf8_lossy(&sent[1].body).contains(&enc("acct")),
        "another account's turn in the session is cold"
    );
}
