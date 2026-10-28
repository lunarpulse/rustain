//! K16 (AC17): catalogue parity through `init_provider_layer` +
//! `refresh_target`, then the served adapter's `stream_completion`. Copilot:
//! live rows keep their live limits (no pins), the live output limit wins over
//! the seed, an id no layer describes has no fabricated window or output limit,
//! and a real upstream `-1m` clears a cached synthetic wire id. Gemini CLI: a
//! live roster owns its rows, so a quota-path refresh drops a cached Antigravity
//! override of a bundled row.

use crate::conformance_20_1d3_copilot::catalog::{
    app_config, live_payload, models_path, refresh, served, target,
};
use crate::conformance_20_1d3_copilot::common::{
    ENDPOINT_A, PROVIDER, ROOT_A, World, chat_path, chat_sse, complete, json_body, json_reply,
    login, messages_path, messages_sse, options, reply, requests, responses_ok, responses_path,
    user,
};
use crate::conformance_20_1d4b_gemini_cli_discovery::common as gemini;
use rustain::adapters::model_catalog_cache::ModelCatalogCache;
use rustain::domain::models::ModelDescriptor;
use rustain::domain::models::provider::DiscoveryStatus;
use rustain::infrastructure::model_discovery::refresh_target;
use rustain::infrastructure::startup::{ProviderLayer, init_provider_layer};
use serde_json::{Value, json};
use serial_test::serial;
use wiremock::{
    Mock, MockServer,
    matchers::{method, path},
};

/// Above every clamp: the `ask` default 8 192 would hide them all.
const MAX_TOKENS: u32 = 100_000;
/// A bundled Messages row whose long-context tier yields a synthetic `-1m`.
const LONG: &str = "claude-opus-5";
const LONG_1M: &str = "claude-opus-5-1m";
/// A bundled Responses row whose seed output limit is 128 000.
const PINNED: &str = "gpt-5.4";
/// A bundled Chat row whose seed output limit is 4 096.
const SEEDED: &str = "gpt-4o";
/// A Chat id no reference row describes (listed by `live_payload`).
const UNREFERENCED: &str = "new-model";
const MODELS_DEV_WINDOW: u32 = 400_000;

/// The second live roster: the real upstream `-1m`, a live output below the
/// seed, a live row without limits and the unreferenced id without limits.
fn second_roster() -> Value {
    json!({"data": [
        {
            "id": LONG,
            "name": "Claude Opus 5",
            "capabilities": {"type": "chat", "limits": {
                "max_context_window_tokens": 1_000_000,
                "max_output_tokens": 64_000,
                "max_prompt_tokens": 200_000
            }},
            "billing": {"token_prices": {
                "default": {"max_prompt_tokens": 200_000},
                "long_context": {"max_prompt_tokens": 936_000}
            }}
        },
        {
            "id": LONG_1M,
            "name": "Claude Opus 5 (upstream 1M)",
            "capabilities": {"type": "chat", "limits": {
                "max_context_window_tokens": 1_000_000,
                "max_output_tokens": 32_000
            }}
        },
        {
            "id": PINNED,
            "capabilities": {"type": "chat", "limits": {
                "max_context_window_tokens": 400_000,
                "max_output_tokens": 20_000
            }}
        },
        {"id": SEEDED, "capabilities": {"type": "chat"}},
        {"id": UNREFERENCED},
    ]})
}

/// The models.dev layer for Copilot, written through the production cache
/// writer (the models.dev source map carries no `github-copilot` key, so
/// `RUSTAIN_MODELS_DEV_URL` cannot feed this provider's layer).
#[cfg(feature = "models-dev")]
fn models_dev_window(id: &str, window: u32) {
    use rustain::adapters::models_dev::{PricingCache, save_cache};
    use rustain::domain::models::provider::ModelRecord;
    let fetched_at_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    save_cache(&PricingCache {
        version: 1,
        fetched_at_unix,
        pricing: Default::default(),
        catalog: [(
            PROVIDER.to_owned(),
            vec![ModelRecord {
                model_id: id.to_owned(),
                context_window: Some(window),
                ..Default::default()
            }],
        )]
        .into(),
    })
    .expect("the models.dev cache is written");
}

fn row(models: &[ModelDescriptor], id: &str) -> ModelDescriptor {
    models
        .iter()
        .find(|m| m.model_id == id)
        .cloned()
        .unwrap_or_else(|| panic!("{id} is not served"))
}

/// The body of the last POST to `route`.
async fn last_body(server: &MockServer, route: &str) -> Value {
    let posts = requests(server, "POST", route).await;
    json_body(posts.last().unwrap_or_else(|| panic!("no POST to {route}")))
}

/// K16 (M16): Copilot live roster via `refresh_target`, then
/// `stream_completion` with `max_tokens = 100_000`: a real upstream `-1m`
/// clears the cached synthetic wire id; a live output below the seed clamps
/// the body; a live row without output uses the seed; an unreferenced id is
/// clamped at 64 000 on an OpenAI-wire route and keeps its models.dev window.
#[cfg(feature = "models-dev")]
#[tokio::test]
#[serial]
async fn k16_copilot_live_limits_own_the_clamp_and_unknown_ids_keep_null_defaults() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    login(&server, "t-limits", ENDPOINT_A).await;
    models_dev_window(UNREFERENCED, MODELS_DEV_WINDOW);
    reply(
        &server,
        "POST",
        &messages_path(ROOT_A),
        messages_sse("messages"),
    )
    .await;
    reply(
        &server,
        "POST",
        &responses_path(ROOT_A),
        responses_ok("responses"),
    )
    .await;
    reply(&server, "POST", &chat_path(ROOT_A), chat_sse("chat")).await;

    let layer = init_provider_layer(&app_config());
    {
        let _first = Mock::given(method("GET"))
            .and(path(models_path(ROOT_A)))
            .respond_with(json_reply(200, live_payload()))
            .mount_as_scoped(&server)
            .await;
        assert_eq!(refresh(&layer, PROVIDER).await.status, DiscoveryStatus::Ok);
    }
    // Positive control: the first roster's synthetic sibling is cached with its
    // base id on the wire.
    assert_eq!(
        row(&served(&layer, PROVIDER), LONG_1M)
            .wire_model_id
            .as_deref(),
        Some(LONG)
    );

    reply(
        &server,
        "GET",
        &models_path(ROOT_A),
        json_reply(200, second_roster()),
    )
    .await;
    assert_eq!(refresh(&layer, PROVIDER).await.status, DiscoveryStatus::Ok);
    let models = served(&layer, PROVIDER);
    assert_eq!(row(&models, LONG_1M).wire_model_id, None);
    assert_eq!(
        row(&models, UNREFERENCED).context_window,
        MODELS_DEV_WINDOW,
        "no fabricated window overwrites the models.dev one"
    );

    let provider = target(&layer, PROVIDER).provider.clone();
    let send = |model: &'static str| {
        let provider = provider.clone();
        async move {
            complete(
                &provider,
                vec![user("hi")],
                options(model, MAX_TOKENS, None),
            )
            .await
            .unwrap_or_else(|error| panic!("{model}: {error:?}"));
        }
    };

    send(LONG_1M).await;
    let body = last_body(&server, &messages_path(ROOT_A)).await;
    assert_eq!(body["model"], LONG_1M, "the real upstream id is sent");
    assert_eq!(body["max_tokens"], 32_000);

    send(PINNED).await;
    let body = last_body(&server, &responses_path(ROOT_A)).await;
    assert_eq!(body["model"], PINNED);
    assert_eq!(
        body["max_output_tokens"], 20_000,
        "the live output limit wins over the seed"
    );

    send(SEEDED).await;
    let body = last_body(&server, &chat_path(ROOT_A)).await;
    assert_eq!(body["model"], SEEDED);
    assert_eq!(
        body["max_completion_tokens"], 4_096,
        "a live row without output uses the seed"
    );

    send(UNREFERENCED).await;
    let body = last_body(&server, &chat_path(ROOT_A)).await;
    assert_eq!(body["model"], UNREFERENCED);
    assert_eq!(
        body["max_completion_tokens"], 64_000,
        "no fabricated output limit: the OpenAI-wire clamp applies"
    );
}

async fn refresh_gemini(layer: &ProviderLayer) -> DiscoveryStatus {
    refresh_target(
        gemini::target(layer, gemini::PROVIDER),
        &ModelCatalogCache::new(),
        Some(&layer.registry),
        true,
    )
    .await
    .status
}

/// K16: a Gemini CLI Antigravity refresh overrides a bundled row's name and
/// window (cached); a later quota-path refresh lists the row again, and the
/// cached override does not survive it — name, window and capabilities are the
/// bundled row's.
#[tokio::test]
#[serial]
async fn k16_gemini_cli_quota_refresh_drops_a_cached_antigravity_override() {
    const OVERRIDDEN: &str = "gemini-3-flash-preview";
    let world = gemini::World::signed_in().await;
    gemini::mount_antigravity(
        &world.server,
        vec![gemini::ok(json!({"models": {
            "gemini-3-flash-preview": {"displayName": "Gemini 3 Flash Live", "supportsImages": true}
        }}))],
    )
    .await;
    let layer = init_provider_layer(&gemini::app_config());
    assert_eq!(refresh_gemini(&layer).await, DiscoveryStatus::Ok);
    // Positive control: the Antigravity override is served (and cached).
    let live = row(&gemini::served(&layer), OVERRIDDEN);
    assert_eq!(
        (live.display_name.as_str(), live.context_window),
        ("Gemini 3 Flash Live", 200_000)
    );

    world.server.reset().await;
    gemini::mount_quota_path(&world.server, &[OVERRIDDEN]).await;
    assert_eq!(refresh_gemini(&layer).await, DiscoveryStatus::Ok);
    let bundled = row(
        &gemini::target(&layer, gemini::PROVIDER).bundled,
        OVERRIDDEN,
    );
    let quota = row(&gemini::served(&layer), OVERRIDDEN);
    assert_eq!(
        (quota.display_name.as_str(), quota.context_window),
        ("Gemini 3 Flash Preview", 1_048_576)
    );
    assert_eq!(
        (quota.display_name.as_str(), quota.context_window),
        (bundled.display_name.as_str(), bundled.context_window)
    );
    assert_eq!(quota.capabilities, bundled.capabilities);
}
