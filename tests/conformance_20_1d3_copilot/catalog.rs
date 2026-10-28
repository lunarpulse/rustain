//! Copilot gate, live discovery and discovery scope (K10, AC10), through
//! `init_provider_layer` + `refresh_target`. Stored `github-copilot` rows come
//! from a real `flows::login` against the loopback stub; every derived Copilot
//! URL is rebased onto the same stub (`RUSTAIN_TEST_AUTH_ORIGIN`), so the
//! probed root `/tenant-a` is where discovery and inference arrive.

use super::common::{
    ENDPOINT_A, ENDPOINT_B, PROBE_PATH, PROVIDER, ROOT_A, ROOT_B, Recorder, SEED_IDS, World,
    chat_path, chat_sse, complete, config, header, json_body, json_reply, login, login_with,
    messages_path, messages_sse, mount_routes, options, probe_reply, reply, requests,
    responses_path, text_of, user,
};
use rustain::adapters::model_catalog_cache::{
    CachedDiscoveryEntry, DiscoveryTarget, ModelCatalogCache,
};
use rustain::domain::models::provider::{DiscoveryStatus, LockedProvider};
use rustain::domain::models::{AppConfig, ModelDescriptor};
use rustain::infrastructure::model_discovery::{RefreshReport, refresh_target};
use rustain::infrastructure::provider_availability::effective_providers;
use rustain::infrastructure::startup::{ProviderLayer, init_provider_layer};
use serde_json::{Value, json};
use serial_test::serial;
use wiremock::MockServer;

const DEFAULT_MODEL: &str = "gpt-5.5";
const HINT: &str = "Set COPILOT_GITHUB_TOKEN or run `rustain auth login github-copilot`";
const LONG: &str = "claude-opus-5";
const LONG_1M: &str = "claude-opus-5-1m";
const EMBEDDING: &str = "text-embedding-3";
const NEW: &str = "new-model";

// ── fixtures ────────────────────────────────────────────────────────────────

/// No configured providers: `github-copilot` is the bundled entry.
pub(crate) fn app_config() -> AppConfig {
    AppConfig {
        model: DEFAULT_MODEL.into(),
        ..Default::default()
    }
}

/// A `[provider.github-copilot]` entry with a configured `base_url`.
fn configured(model: &str, base: &str) -> AppConfig {
    let mut cfg = app_config();
    cfg.provider
        .insert(PROVIDER.into(), config(model, Some(base)));
    cfg
}

/// The live `/models` payload of K10: a chat row, a row with a long-context
/// billing tier, an embedding row and a row without capabilities.
pub(crate) fn live_payload() -> Value {
    json!({"data": [
        {"id": DEFAULT_MODEL, "name": "GPT-5.5", "capabilities": {"type": "chat"}},
        {
            "id": LONG,
            "name": "Claude Opus 5",
            "capabilities": {
                "type": "chat",
                "limits": {
                    "max_context_window_tokens": 1_000_000,
                    "max_output_tokens": 64_000,
                    "max_prompt_tokens": 200_000
                }
            },
            "billing": {"token_prices": {
                "default": {"max_prompt_tokens": 200_000},
                "long_context": {"max_prompt_tokens": 936_000}
            }}
        },
        {"id": EMBEDDING, "capabilities": {"type": "embedding"}},
        {"id": NEW},
    ]})
}

pub(crate) fn models_path(root: &str) -> String {
    format!("{root}/models")
}

pub(crate) fn target<'a>(layer: &'a ProviderLayer, id: &str) -> &'a DiscoveryTarget {
    layer
        .discovery_targets
        .iter()
        .find(|t| t.provider_id == id)
        .unwrap_or_else(|| panic!("{id} is not a live discovery target"))
}

fn locked(layer: &ProviderLayer, id: &str) -> LockedProvider {
    layer
        .registry
        .list_locked()
        .into_iter()
        .find(|p| p.provider_id == id)
        .unwrap_or_else(|| panic!("{id} is not locked"))
}

fn is_locked(layer: &ProviderLayer, id: &str) -> bool {
    layer
        .registry
        .list_locked()
        .iter()
        .any(|p| p.provider_id == id)
}

pub(crate) fn served(layer: &ProviderLayer, id: &str) -> Vec<ModelDescriptor> {
    layer.registry.list_models_by_provider(id)
}

pub(crate) fn ids(models: &[ModelDescriptor]) -> Vec<String> {
    models.iter().map(|m| m.model_id.clone()).collect()
}

pub(crate) fn has(ids: &[String], id: &str) -> bool {
    ids.iter().any(|m| m == id)
}

pub(crate) async fn refresh(layer: &ProviderLayer, id: &str) -> RefreshReport {
    refresh_target(
        target(layer, id),
        &ModelCatalogCache::new(),
        Some(&layer.registry),
        true,
    )
    .await
}

async fn disk(id: &str) -> CachedDiscoveryEntry {
    ModelCatalogCache::new()
        .load()
        .await
        .providers
        .remove(id)
        .unwrap_or_else(|| panic!("no cache entry for {id}"))
}

/// `(method, path)` of every request the stub saw from index `from` on.
pub(crate) async fn seen_since(server: &MockServer, from: usize) -> Vec<(String, String)> {
    server.received_requests().await.unwrap()[from..]
        .iter()
        .map(|r| (r.method.to_string(), r.url.path().to_owned()))
        .collect()
}

pub(crate) async fn seen_count(server: &MockServer) -> usize {
    server.received_requests().await.unwrap().len()
}

// ── K10: gate ───────────────────────────────────────────────────────────────

/// K10 (M10): without a credential `github-copilot` is locked with the login
/// hint and advertises the 55-row public catalogue defaulting to `gpt-5.5`;
/// after a login it is built.
#[tokio::test]
#[serial]
async fn k10_without_credentials_copilot_is_locked_with_the_login_hint_and_the_55_row_catalogue() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let cfg = app_config();
    let entry = effective_providers(&cfg)
        .into_iter()
        .find(|entry| entry.id == PROVIDER)
        .expect("github-copilot is a bundled provider");
    assert_eq!(entry.cfg.model_id, DEFAULT_MODEL);

    let layer = init_provider_layer(&cfg);
    let gate = locked(&layer, PROVIDER);
    assert_eq!(gate.sign_in_hint, HINT);
    let catalogue = ids(&gate.models);
    assert_eq!(catalogue.len(), SEED_IDS, "{catalogue:?}");
    assert!(has(&catalogue, DEFAULT_MODEL), "{catalogue:?}");
    assert!(layer.router.get_provider(PROVIDER).is_none());
    assert!(
        layer
            .discovery_targets
            .iter()
            .all(|t| t.provider_id != PROVIDER)
    );
    assert!(server.received_requests().await.unwrap().is_empty());

    login(&server, "t-gate", ENDPOINT_A).await;
    let signed = init_provider_layer(&cfg);
    assert!(!is_locked(&signed, PROVIDER));
    assert!(signed.router.get_provider(PROVIDER).is_some());
    target(&signed, PROVIDER);
}

/// K10 (M10): a stored row never signs a configured non-Copilot origin (the
/// entry stays locked and that origin sees nothing); `COPILOT_GITHUB_TOKEN`
/// signs the same entry and its Chat request reaches the configured base.
#[tokio::test]
#[serial]
async fn k10_configured_foreign_base_locks_the_stored_row_and_the_env_token_signs_it() {
    let server = MockServer::start().await;
    let foreign = MockServer::start().await;
    let world = World::new(&server);
    login(&server, "t-guard", ENDPOINT_A).await;
    let after_login = seen_count(&server).await;
    let base = format!("{}/proxy", foreign.uri());
    let model = "grok-code-fast-1";
    let cfg = configured(model, &base);

    let layer = init_provider_layer(&cfg);
    assert_eq!(locked(&layer, PROVIDER).sign_in_hint, HINT);
    assert!(layer.router.get_provider(PROVIDER).is_none());
    assert!(
        layer
            .discovery_targets
            .iter()
            .all(|t| t.provider_id != PROVIDER)
    );
    assert!(foreign.received_requests().await.unwrap().is_empty());
    assert_eq!(seen_count(&server).await, after_login);

    world.set("COPILOT_GITHUB_TOKEN", "tok");
    reply(
        &foreign,
        "POST",
        &chat_path("/proxy"),
        chat_sse("k10 signed"),
    )
    .await;
    let signed = init_provider_layer(&cfg);
    assert!(!is_locked(&signed, PROVIDER));
    let provider = signed
        .router
        .get_provider(PROVIDER)
        .expect("signed by COPILOT_GITHUB_TOKEN");
    let chunks = complete(&provider, vec![user("hi")], options(model, 1024, None))
        .await
        .expect("the configured base answers");
    assert_eq!(text_of(&chunks), "k10 signed");
    let posts = requests(&foreign, "POST", &chat_path("/proxy")).await;
    assert_eq!(posts.len(), 1);
    assert_eq!(
        header(&posts[0], "authorization").as_deref(),
        Some("Bearer tok")
    );
    assert_eq!(json_body(&posts[0])["model"], model);
    assert_eq!(seen_count(&server).await, after_login);
}

// ── K10: discovery ──────────────────────────────────────────────────────────

/// K10 (M10): `refresh_target` lists `<root>/models` with the discovery
/// headers; embeddings are dropped, the long-context tier adds `-1m` right
/// after its base id, every seed id stays, and the `-1m` request sends the
/// base id on the wire.
#[tokio::test]
#[serial]
async fn k10_refresh_lists_the_probed_root_and_serves_sorted_rows_with_the_1m_sibling() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    login(&server, "t-discovery", ENDPOINT_A).await;
    reply(
        &server,
        "GET",
        &models_path(ROOT_A),
        json_reply(200, live_payload()),
    )
    .await;
    reply(
        &server,
        "POST",
        &messages_path(ROOT_A),
        messages_sse("one million"),
    )
    .await;

    let layer = init_provider_layer(&app_config());
    let seeded = ids(&target(&layer, PROVIDER).bundled);
    let report = refresh(&layer, PROVIDER).await;
    assert_eq!(report.status, DiscoveryStatus::Ok);

    let gets = requests(&server, "GET", &models_path(ROOT_A)).await;
    assert_eq!(gets.len(), 1);
    let get = &gets[0];
    assert_eq!(
        header(get, "authorization").as_deref(),
        Some("Bearer t-discovery")
    );
    assert_eq!(header(get, "accept").as_deref(), Some("application/json"));
    assert_eq!(header(get, "x-initiator").as_deref(), Some("user"));
    assert_eq!(
        header(get, "copilot-integration-id").as_deref(),
        Some("copilot-developer-cli")
    );
    assert_eq!(
        header(get, "x-github-api-version").as_deref(),
        Some("2026-08-01")
    );
    assert_eq!(header(get, "x-interaction-type"), None);
    assert!(requests(&server, "GET", "/models").await.is_empty());

    let models = served(&layer, PROVIDER);
    let served_ids = ids(&models);
    assert_eq!(report.model_count, served_ids.len());
    let mut sorted = served_ids.clone();
    sorted.sort();
    assert_eq!(served_ids, sorted, "served rows are sorted by id");
    let at = served_ids
        .iter()
        .position(|id| id == LONG)
        .expect("the long-context base row is served");
    assert_eq!(served_ids.get(at + 1).map(String::as_str), Some(LONG_1M));
    assert!(has(&served_ids, NEW), "{served_ids:?}");
    assert!(!has(&served_ids, EMBEDDING), "{served_ids:?}");
    for id in &seeded {
        assert!(has(&served_ids, id), "seed id {id} missing: {served_ids:?}");
    }
    let sibling = models.iter().find(|m| m.model_id == LONG_1M).unwrap();
    assert!(
        sibling.display_name.ends_with(" (1M)"),
        "{}",
        sibling.display_name
    );

    // The served adapter sends the base id for the `-1m` sibling.
    let provider = target(&layer, PROVIDER).provider.clone();
    let chunks = complete(&provider, vec![user("hi")], options(LONG_1M, 1024, None))
        .await
        .expect("the -1m sibling answers");
    assert_eq!(text_of(&chunks), "one million");
    let posts = requests(&server, "POST", &messages_path(ROOT_A)).await;
    assert_eq!(posts.len(), 1);
    assert_eq!(json_body(&posts[0])["model"], LONG);
}

/// K10 (M10): a row without an endpoint sits on the canonical root, so the
/// refresh re-probes first, lists the found root and the next inference uses it.
#[tokio::test]
#[serial]
async fn k10_canonical_root_reprobes_then_lists_and_infers_on_the_found_root() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    login_with(
        &server,
        "",
        "t-reprobe",
        json_reply(500, json!({"message": "probe down"})),
    )
    .await;
    reply(&server, "GET", PROBE_PATH, probe_reply(ENDPOINT_B)).await;
    reply(
        &server,
        "GET",
        &models_path(ROOT_B),
        json_reply(200, live_payload()),
    )
    .await;
    mount_routes(&server, ROOT_B, "reprobed").await;

    let layer = init_provider_layer(&app_config());
    let before = seen_count(&server).await;
    assert_eq!(refresh(&layer, PROVIDER).await.status, DiscoveryStatus::Ok);
    assert_eq!(
        seen_since(&server, before).await,
        vec![
            ("GET".to_owned(), PROBE_PATH.to_owned()),
            ("GET".to_owned(), models_path(ROOT_B)),
        ]
    );
    let gets = requests(&server, "GET", &models_path(ROOT_B)).await;
    assert_eq!(
        header(&gets[0], "authorization").as_deref(),
        Some("Bearer t-reprobe")
    );
    assert!(has(&ids(&served(&layer, PROVIDER)), NEW));

    let provider = target(&layer, PROVIDER).provider.clone();
    let chunks = complete(
        &provider,
        vec![user("hi")],
        options(DEFAULT_MODEL, 1024, None),
    )
    .await
    .expect("the learned root answers");
    assert_eq!(text_of(&chunks), "reprobed");
    assert_eq!(
        requests(&server, "POST", &responses_path(ROOT_B))
            .await
            .len(),
        1
    );
    assert!(requests(&server, "POST", "/responses").await.is_empty());
}

/// K10 (M10): a 401 on `/models` marks the provider unauthenticated.
#[tokio::test]
#[serial]
async fn k10_models_401_is_unauthenticated() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    login(&server, "t-401", ENDPOINT_A).await;
    reply(
        &server,
        "GET",
        &models_path(ROOT_A),
        json_reply(401, json!({"message": "Bad credentials"})),
    )
    .await;
    let layer = init_provider_layer(&app_config());
    let report = refresh(&layer, PROVIDER).await;
    assert_eq!(report.status, DiscoveryStatus::Unauthenticated);
    assert_eq!(
        layer.registry.discovery_status(PROVIDER),
        Some(DiscoveryStatus::Unauthenticated)
    );
    assert!(
        !requests(&server, "GET", &models_path(ROOT_A))
            .await
            .is_empty()
    );
}

/// K10 (M10): a 403 on `/models` is not an authentication failure and keeps the
/// catalogue the previous refresh cached.
#[tokio::test]
#[serial]
async fn k10_models_403_keeps_the_cached_catalogue_and_is_not_unauthenticated() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    login(&server, "t-403", ENDPOINT_A).await;
    Recorder::new()
        .mount(
            &server,
            "GET",
            &models_path(ROOT_A),
            vec![
                json_reply(200, live_payload()),
                json_reply(403, json!({"message": "Forbidden"})),
            ],
        )
        .await;
    let layer = init_provider_layer(&app_config());
    assert_eq!(refresh(&layer, PROVIDER).await.status, DiscoveryStatus::Ok);
    let cached = disk(PROVIDER).await;
    let first = ids(&served(&layer, PROVIDER));
    assert!(has(&first, NEW) && has(&first, LONG_1M), "{first:?}");

    let report = refresh(&layer, PROVIDER).await;
    assert_ne!(report.status, DiscoveryStatus::Unauthenticated);
    assert_eq!(report.status, DiscoveryStatus::Cached);
    assert_ne!(
        layer.registry.discovery_status(PROVIDER),
        Some(DiscoveryStatus::Unauthenticated)
    );
    let notice = report.notice.expect("a denied listing is reported");
    assert!(
        notice.contains("GitHub Copilot denied model listing (HTTP 403)"),
        "{notice}"
    );
    assert_eq!(
        ids(&served(&layer, PROVIDER)),
        first,
        "the cached roster stays"
    );
    let kept = disk(PROVIDER).await;
    assert_eq!(kept.credential_fingerprint, cached.credential_fingerprint);
    assert!(kept.models.iter().any(|m| m.model_id == NEW));
    assert_eq!(
        requests(&server, "GET", &models_path(ROOT_A)).await.len(),
        2
    );
}

// ── K10: scope ──────────────────────────────────────────────────────────────

/// K10 (M10): two logins with the same token differing only by the probed
/// endpoint have different discovery scopes, and within each login the startup
/// scope equals the scope `refresh_target` stamps on the cache.
#[tokio::test]
#[serial]
async fn k10_logins_differing_only_by_endpoint_have_distinct_scopes_equal_at_startup_and_refresh() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    for root in [ROOT_A, ROOT_B] {
        reply(
            &server,
            "GET",
            &models_path(root),
            json_reply(200, live_payload()),
        )
        .await;
    }
    let cfg = app_config();

    login(&server, "t-scope", ENDPOINT_A).await;
    let first = init_provider_layer(&cfg);
    let startup_a = target(&first, PROVIDER).credential_fingerprint.clone();
    assert_eq!(refresh(&first, PROVIDER).await.status, DiscoveryStatus::Ok);
    let refresh_a = disk(PROVIDER).await.credential_fingerprint;
    assert_eq!(
        startup_a, refresh_a,
        "login 1: startup scope == refresh scope"
    );
    assert_eq!(
        requests(&server, "GET", &models_path(ROOT_A)).await.len(),
        1
    );

    login(&server, "t-scope", ENDPOINT_B).await;
    let second = init_provider_layer(&cfg);
    let startup_b = target(&second, PROVIDER).credential_fingerprint.clone();
    assert_eq!(refresh(&second, PROVIDER).await.status, DiscoveryStatus::Ok);
    let refresh_b = disk(PROVIDER).await.credential_fingerprint;
    assert_eq!(
        startup_b, refresh_b,
        "login 2: startup scope == refresh scope"
    );
    assert_eq!(
        requests(&server, "GET", &models_path(ROOT_B)).await.len(),
        1
    );

    assert_ne!(
        startup_a, startup_b,
        "the startup scope includes the endpoint"
    );
    assert_ne!(
        refresh_a, refresh_b,
        "the cache scope includes the endpoint"
    );
}
