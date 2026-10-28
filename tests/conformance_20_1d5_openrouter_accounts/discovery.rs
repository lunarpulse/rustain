//! K11 (AC11): OpenRouter discovery under 20-1c's credential gate, through
//! `init_provider_layer` + `refresh_target`, and `/auth/key` as the only
//! credential check.

use super::mint::{AUTH_KEY, MODELS, PROVIDER};
use super::openrouter_wire::{ENV_KEY, RESPONSES, config};
use crate::conformance_20_1d1b_codex_replay_discovery::common::{World, completed, sse, text};
use crate::conformance_20_1d3_copilot::common::{
    complete, header, json_body, options, reply, requests, user,
};
use rustain::adapters::auth_store::FileAuthStore;
use rustain::adapters::model_catalog_cache::ModelCatalogCache;
use rustain::domain::errors::ProviderError;
use rustain::domain::models::provider::{DiscoveryStatus, ModelCapability};
use rustain::domain::models::{AppConfig, ProviderConfig};
use rustain::domain::ports::AuthStorePort;
use rustain::infrastructure::model_discovery::refresh_target;
use rustain::infrastructure::provider_availability::{
    Availability, availability, effective_providers,
};
use rustain::infrastructure::provider_factory::build_provider_for_config;
use rustain::infrastructure::startup::init_provider_layer;
use serde_json::json;
use serial_test::serial;
use std::collections::HashSet;
use wiremock::{MockServer, ResponseTemplate};

const HINT: &str = "Set OPENROUTER_API_KEY or run `rustain auth login openrouter`";

fn discovering(model: &str) -> ProviderConfig {
    let mut cfg = config(model);
    cfg.discover_models = true;
    cfg
}

fn app(cfg: &ProviderConfig) -> AppConfig {
    let mut app = AppConfig {
        model: cfg.model_id.clone(),
        ..Default::default()
    };
    app.provider.insert(cfg.provider_id.clone(), cfg.clone());
    app
}

#[tokio::test]
#[serial]
async fn k11_without_a_credential_openrouter_is_locked_and_sends_nothing() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let cfg = discovering("openai/gpt-5.5");
    let app = app(&cfg);
    let entry = effective_providers(&app)
        .into_iter()
        .find(|entry| entry.id == PROVIDER)
        .unwrap();
    let store: std::sync::Arc<dyn AuthStorePort> = std::sync::Arc::new(FileAuthStore::new());
    let snapshot = store.snapshot().await.unwrap();
    match availability(
        &entry,
        &snapshot,
        &rustain::infrastructure::utils::env_var_trimmed,
    ) {
        Availability::Locked { hint } => assert_eq!(hint, HINT),
        other => panic!("openrouter must stay locked, got {other:?}"),
    }
    assert!(matches!(
        build_provider_for_config(PROVIDER, &cfg),
        Err(ProviderError::AuthenticationFailed)
    ));
    let layer = init_provider_layer(&app);
    assert!(layer.router.get_provider(PROVIDER).is_none());
    assert!(
        layer
            .registry
            .list_locked()
            .iter()
            .any(|locked| locked.provider_id == PROVIDER)
    );
    assert!(
        !layer
            .discovery_targets
            .iter()
            .any(|target| target.provider_id == PROVIDER)
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
#[serial]
async fn k11_a_keyed_refresh_serves_only_tool_rows_with_mapped_capabilities() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    world.set("OPENROUTER_API_KEY", ENV_KEY);
    reply(
        &server,
        "GET",
        MODELS,
        ResponseTemplate::new(200).set_body_json(json!({"data": [
            {"id": "openai/gpt-5.5", "name": "GPT-5.5", "context_length": 400000,
             "supported_parameters": ["tools", "reasoning"],
             "architecture": {"input_modalities": ["text", "image"]}},
            {"id": "anthropic/claude-sonnet-4.6", "supported_parameters": ["tools"],
             "architecture": {"modality": "text+image->text"}},
            {"id": "meta/text-only", "supported_parameters": ["tools"],
             "architecture": {"input_modalities": ["text"]}},
            {"id": "vendor/no-tools", "supported_parameters": ["top_p", "reasoning"]},
            {"id": "vendor/no-parameters"}
        ]})),
    )
    .await;
    reply(&server, "POST", RESPONSES, sse(&[text("ok"), completed()])).await;

    let app = app(&discovering("openai/gpt-5.5"));
    let layer = init_provider_layer(&app);
    let target = layer
        .discovery_targets
        .iter()
        .find(|target| target.provider_id == PROVIDER)
        .expect("a keyed OpenRouter entry is a discovery target");
    let cache = ModelCatalogCache::new();
    let report = refresh_target(target, &cache, Some(&layer.registry), true).await;
    assert_eq!(report.status, DiscoveryStatus::Ok);

    let listings = requests(&server, "GET", MODELS).await;
    assert_eq!(listings.len(), 1);
    assert_eq!(
        header(&listings[0], "authorization").as_deref(),
        Some(format!("Bearer {ENV_KEY}").as_str())
    );
    // A successful `/models` is never credential validation.
    assert!(requests(&server, "GET", AUTH_KEY).await.is_empty());

    let served = layer.registry.list_models_by_provider(PROVIDER);
    let caps = |id: &str| -> HashSet<ModelCapability> {
        served
            .iter()
            .find(|m| m.model_id == id)
            .unwrap_or_else(|| panic!("{id} served"))
            .capabilities
            .iter()
            .cloned()
            .collect()
    };
    let ids: HashSet<&str> = served.iter().map(|m| m.model_id.as_str()).collect();
    assert!(!ids.contains("vendor/no-tools"), "{ids:?}");
    assert!(!ids.contains("vendor/no-parameters"), "{ids:?}");
    assert_eq!(
        caps("openai/gpt-5.5"),
        HashSet::from([
            ModelCapability::ToolUse,
            ModelCapability::Vision,
            ModelCapability::Thinking
        ])
    );
    assert_eq!(
        caps("anthropic/claude-sonnet-4.6"),
        HashSet::from([ModelCapability::ToolUse, ModelCapability::Vision])
    );
    assert_eq!(
        caps("meta/text-only"),
        HashSet::from([ModelCapability::ToolUse])
    );

    // The cache scope is the scope of the credential actually sent.
    let scope = target
        .provider
        .discovery_scope(&target.scope_context)
        .await
        .unwrap();
    assert_eq!(
        cache.load().await.providers[PROVIDER].credential_fingerprint,
        scope
    );

    // The registered target is the OpenRouter adapter: the served reasoning row
    // now asks the Responses route for encrypted reasoning.
    let provider = layer.router.get_provider(PROVIDER).unwrap();
    complete(
        &provider,
        vec![user("hi")],
        options("openai/gpt-5.5", 8_192, None),
    )
    .await
    .unwrap();
    let posts = requests(&server, "POST", RESPONSES).await;
    assert_eq!(posts.len(), 1);
    assert_eq!(
        json_body(&posts[0])["include"],
        json!(["reasoning.encrypted_content"])
    );
}

#[tokio::test]
#[serial]
async fn k11_an_invalid_key_is_unauthenticated_although_models_answers() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    world.set("OPENROUTER_API_KEY", "sk-or-v1-invalid-canary");
    reply(
        &server,
        "GET",
        MODELS,
        ResponseTemplate::new(200).set_body_json(json!({"data": []})),
    )
    .await;
    reply(
        &server,
        "GET",
        AUTH_KEY,
        ResponseTemplate::new(401).set_body_json(json!({"error": {"message": "No auth"}})),
    )
    .await;
    let provider = build_provider_for_config(PROVIDER, &config("openai/gpt-5.5")).unwrap();
    assert!(matches!(
        provider.health_check().await,
        Err(ProviderError::AuthenticationFailed)
    ));
    assert!(matches!(
        provider.connectivity_probe().await,
        Err(ProviderError::AuthenticationFailed)
    ));
    assert_eq!(requests(&server, "GET", AUTH_KEY).await.len(), 2);
    assert!(requests(&server, "GET", MODELS).await.is_empty());

    // A base without `/auth/key`: health is reachable-but-unsupported, while the
    // probe refuses so a key is never stored unvalidated.
    server.reset().await;
    reply(&server, "GET", AUTH_KEY, ResponseTemplate::new(404)).await;
    assert!(matches!(
        provider.health_check().await,
        Err(ProviderError::EndpointUnsupported(404))
    ));
    match provider.connectivity_probe().await {
        Err(ProviderError::Other(text)) => assert_eq!(text, "Probe failed: HTTP 404"),
        other => panic!("{other:?}"),
    }
}
