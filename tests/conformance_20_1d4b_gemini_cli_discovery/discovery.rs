//! K1-K3: discovery requests, fallback triggers, the quota project, recovery and
//! cache scope; the Antigravity and quota rosters as served through 20-1c's
//! `refresh_target` on entries built by `init_provider_layer`.
use super::common::*;
use rustain::adapters::auth_store::FileAuthStore;
use rustain::adapters::model_catalog_cache::ModelCatalogCache;
use rustain::domain::models::provider::DiscoveryStatus;
use rustain::domain::models::{AppConfig, CasOutcome, Credential, ModelCapability, ProviderConfig};
use rustain::domain::ports::AuthStorePort;
use rustain::infrastructure::model_discovery::refresh_target;
use rustain::infrastructure::startup::{ProviderLayer, init_provider_layer};
use serde_json::{Value, json};
use serial_test::serial;
use std::collections::HashSet;
use wiremock::{
    Mock, Request, ResponseTemplate,
    matchers::{header as header_is, method, path},
};

/// Forced refresh of the Gemini CLI target of `layer`.
async fn refresh(layer: &ProviderLayer) -> rustain::infrastructure::model_discovery::RefreshReport {
    refresh_target(
        target(layer, PROVIDER),
        &ModelCatalogCache::new(),
        Some(&layer.registry),
        true,
    )
    .await
}

fn discovery_paths(requests: &[Request]) -> Vec<&str> {
    requests
        .iter()
        .map(|r| r.url.path())
        .filter(|p| [FETCH, QUOTA, LOAD].contains(p))
        .collect()
}

fn assert_antigravity(request: &Request, token: &str) {
    assert_eq!(request.method.as_str(), "POST");
    assert_eq!(String::from_utf8_lossy(&request.body), "{}");
    assert_eq!(header(request, "authorization"), format!("Bearer {token}"));
    assert_eq!(header(request, "content-type"), "application/json");
    assert_eq!(header(request, "user-agent"), ANTIGRAVITY_UA);
    assert!(request.headers.get("client-metadata").is_none());
}

fn assert_quota(request: &Request, token: &str, project: &str) {
    assert_eq!(request.method.as_str(), "POST");
    assert_eq!(
        String::from_utf8_lossy(&request.body),
        format!(r#"{{"project":"{project}"}}"#)
    );
    assert_eq!(header(request, "authorization"), format!("Bearer {token}"));
    assert_eq!(header(request, "content-type"), "application/json");
    assert_eq!(
        header(request, "user-agent"),
        gemini_cli_agent("gemini-3.1-pro-preview")
    );
    assert_eq!(header(request, "client-metadata"), CLIENT_METADATA);
}

// ── K1: requests, fallback triggers, quota project, recovery ───────────────

#[tokio::test]
#[serial]
async fn k1_rejected_antigravity_falls_back_to_the_quota_with_the_stored_project() {
    let world = World::signed_in().await;
    let baseline = world.requests().await.len();
    mount_quota_path(&world.server, &["gemini-3.1-pro-low"]).await;
    let layer = init_provider_layer(&app_config());
    let report = refresh(&layer).await;
    // Antigravity's 401/403 is a fallback trigger, never a credential verdict.
    assert_eq!(report.status, DiscoveryStatus::Ok);
    assert_eq!(report.notice, None);

    let sent = world.since(baseline).await;
    assert_eq!(discovery_paths(&sent), [FETCH, FETCH, QUOTA]);
    assert_eq!(sent.len(), 3, "discovery sends nothing else");
    assert_antigravity(&sent[0], ACCESS);
    assert_antigravity(&sent[1], ACCESS);
    assert_quota(&sent[2], ACCESS, PROJECT);
    // All three reached the loopback stub every Google origin rebases to; the
    // un-rebased hosts are pinned by the `discovery` lib units.
    assert_no_long_lived_secrets(&sent);
    assert!(ids(&served(&layer)).contains(&"gemini-3.1-pro".to_owned()));

    // Antigravity 401s fall back the same way.
    world.server.reset().await;
    Mock::given(method("POST"))
        .and(path(FETCH))
        .respond_with(ResponseTemplate::new(401))
        .mount(&world.server)
        .await;
    mount_quota(&world.server, vec![ok(quota_body(&[]))]).await;
    let report = refresh(&layer).await;
    assert_eq!(report.status, DiscoveryStatus::Ok);
    let sent = world.requests().await;
    assert_eq!(discovery_paths(&sent), [FETCH, FETCH, QUOTA]);
    assert!(hits(&sent, TOKEN).is_empty(), "no recovery for Antigravity");
}

#[tokio::test]
#[serial]
async fn k1_server_error_then_network_error_falls_back_to_the_quota() {
    let _world = World::signed_in().await;
    let quota = quota_body(&["gemini-9-pro"]).to_string();
    let origin = RawOrigin::start(vec![Some((500, "{}".into())), None, Some((200, quota))]).await;
    let _route = Env::set(&[("RUSTAIN_TEST_AUTH_ORIGIN", Some(origin.uri.as_str()))]);
    let layer = init_provider_layer(&app_config());
    let report = refresh(&layer).await;
    assert_eq!(report.status, DiscoveryStatus::Ok);
    assert_eq!(origin.paths(), [FETCH, FETCH, QUOTA]);
    let seen = origin.seen.lock().unwrap().clone();
    assert_eq!(seen[0].body, "{}");
    assert_eq!(seen[1].body, "{}");
    assert_eq!(seen[2].body, format!(r#"{{"project":"{PROJECT}"}}"#));
    assert!(ids(&served(&layer)).contains(&"gemini-9-pro".to_owned()));
}

#[tokio::test]
#[serial]
async fn k1_invalid_payloads_fall_back_and_a_valid_one_never_reaches_the_quota() {
    let world = World::signed_in().await;
    let baseline = world.requests().await.len();
    let invalid = || ResponseTemplate::new(200).set_body_string("not json");
    mount_antigravity(
        &world.server,
        vec![invalid(), ok(json!(["not", "an", "object"]))],
    )
    .await;
    mount_quota(&world.server, vec![ok(quota_body(&["gemini-9-pro"]))]).await;
    let layer = init_provider_layer(&app_config());
    assert_eq!(refresh(&layer).await.status, DiscoveryStatus::Ok);
    let sent = world.since(baseline).await;
    assert_eq!(discovery_paths(&sent), [FETCH, FETCH, QUOTA]);
    assert_quota(&sent[2], ACCESS, PROJECT);

    // A primary failure followed by a sandbox payload: the sandbox answers.
    world.server.reset().await;
    mount_antigravity(
        &world.server,
        vec![
            invalid(),
            ok(json!({"models": {"gemini-x": {"displayName": "X"}}})),
        ],
    )
    .await;
    mount_quota(&world.server, vec![ok(quota_body(&[]))]).await;
    assert_eq!(refresh(&layer).await.status, DiscoveryStatus::Ok);
    assert_eq!(discovery_paths(&world.requests().await), [FETCH, FETCH]);

    // A primary 2xx payload issues no further request at all.
    world.server.reset().await;
    mount_antigravity(&world.server, vec![ok(json!({"models": {}}))]).await;
    mount_quota(&world.server, vec![ok(quota_body(&[]))]).await;
    assert_eq!(refresh(&layer).await.status, DiscoveryStatus::Ok);
    let sent = world.requests().await;
    assert_eq!(discovery_paths(&sent), [FETCH]);
    assert_antigravity(&sent[0], ACCESS);
}

#[tokio::test]
#[serial]
async fn k1_quota_401_recovers_once_with_the_new_token_and_the_same_project() {
    let world = World::signed_in().await;
    mount_refresh(&world.server).await;
    mount_antigravity(
        &world.server,
        vec![ResponseTemplate::new(403), ResponseTemplate::new(403)],
    )
    .await;
    Mock::given(method("POST"))
        .and(path(QUOTA))
        .and(header_is(
            "authorization",
            format!("Bearer {ACCESS}").as_str(),
        ))
        .respond_with(ResponseTemplate::new(401))
        .with_priority(1)
        .mount(&world.server)
        .await;
    mount_quota(&world.server, vec![ok(quota_body(&["gemini-9-pro"]))]).await;
    let baseline = world.requests().await.len();
    let layer = init_provider_layer(&app_config());
    let report = refresh(&layer).await;
    assert_eq!(report.status, DiscoveryStatus::Ok);
    let sent = world.since(baseline).await;
    assert_eq!(
        sent.iter().map(|r| r.url.path()).collect::<Vec<_>>(),
        [FETCH, FETCH, QUOTA, TOKEN, QUOTA]
    );
    assert_quota(&sent[2], ACCESS, PROJECT);
    // The resend carries the refreshed token and the same row's project.
    assert_quota(&sent[4], REFRESHED, PROJECT);
    assert_no_long_lived_secrets(&sent);
    let scope = disk(PROVIDER).await.credential_fingerprint;
    assert_eq!(scope, target(&layer, PROVIDER).credential_fingerprint);
}

#[tokio::test]
#[serial]
async fn k1_a_quota_401_after_the_one_recovery_and_a_quota_403_are_unauthenticated() {
    // A second 401 after the one recovery is final: Unauthenticated + notice.
    let world = World::signed_in().await;
    let baseline = world.requests().await.len();
    mount_refresh(&world.server).await;
    mount_antigravity(
        &world.server,
        vec![ResponseTemplate::new(403), ResponseTemplate::new(403)],
    )
    .await;
    Mock::given(method("POST"))
        .and(path(QUOTA))
        .respond_with(ResponseTemplate::new(401))
        .mount(&world.server)
        .await;
    let layer = init_provider_layer(&app_config());
    let report = refresh(&layer).await;
    assert_eq!(report.status, DiscoveryStatus::Unauthenticated);
    assert_eq!(
        report.notice.as_deref(),
        Some(
            "Google Cloud Code Assist (Gemini CLI) rejected its credential (HTTP 401/403) — models could not be discovered. Run `rustain auth login google-gemini-cli`"
        )
    );
    let sent = world.since(baseline).await;
    assert_eq!(
        sent.iter().map(|r| r.url.path()).collect::<Vec<_>>(),
        [FETCH, FETCH, QUOTA, TOKEN, QUOTA],
        "exactly one recovery and one resend"
    );
    assert_quota(&sent[4], REFRESHED, PROJECT);
    assert!(served(&layer).iter().all(|m| m.stale));

    // A quota 403 is final without a recovery.
    world.server.reset().await;
    mount_antigravity(
        &world.server,
        vec![ResponseTemplate::new(403), ResponseTemplate::new(403)],
    )
    .await;
    mount_quota(&world.server, vec![ResponseTemplate::new(403)]).await;
    let layer = init_provider_layer(&app_config());
    assert_eq!(
        refresh(&layer).await.status,
        DiscoveryStatus::Unauthenticated
    );
    assert!(hits(&world.requests().await, TOKEN).is_empty());
}

#[tokio::test]
#[serial]
async fn plain_project_less_grant_takes_the_project_from_load_code_assist() {
    let world = World::signed_in().await;
    // The single sanctioned rewrite of a login-produced row: drop its project.
    let store = store();
    let snapshot = store.snapshot().await.unwrap();
    let rows = snapshot.rows(PROVIDER);
    assert_eq!(rows.len(), 1);
    let Credential::OAuth(oauth) = &rows[0].credential else {
        panic!("login stores an OAuth row");
    };
    let mut oauth = oauth.clone();
    oauth.identity.project_id = None;
    let outcome = store
        .replace_if_matches(PROVIDER, rows[0].stamp(), Credential::OAuth(oauth))
        .await
        .unwrap();
    assert_eq!(outcome, CasOutcome::Applied);
    for (load, expected) in [
        (
            json!({"cloudaicompanionProject": {"id": "loaded-object"}}),
            r#"{"project":"loaded-object"}"#,
        ),
        (
            json!({"cloudaicompanionProject": "loaded-string"}),
            r#"{"project":"loaded-string"}"#,
        ),
        (json!({"cloudaicompanionProject": 7}), "{}"),
    ] {
        world.server.reset().await;
        Mock::given(method("POST"))
            .and(path(LOAD))
            .respond_with(ok(load))
            .mount(&world.server)
            .await;
        mount_quota_path(&world.server, &["gemini-9-pro"]).await;
        let layer = init_provider_layer(&app_config());
        assert_eq!(refresh(&layer).await.status, DiscoveryStatus::Ok);
        let sent = world.requests().await;
        assert_eq!(discovery_paths(&sent), [FETCH, FETCH, LOAD, QUOTA]);
        let load = hits(&sent, LOAD)[0];
        assert_eq!(
            String::from_utf8_lossy(&load.body),
            r#"{"metadata":{"ideType":"IDE_UNSPECIFIED","platform":"PLATFORM_UNSPECIFIED","pluginType":"GEMINI"}}"#
        );
        assert_eq!(header(load, "authorization"), format!("Bearer {ACCESS}"));
        assert_eq!(
            String::from_utf8_lossy(&hits(&sent, QUOTA)[0].body),
            expected
        );
    }
}

// ── K2: Antigravity roster ──────────────────────────────────────────────────

fn antigravity_roster() -> Value {
    json!({"models": {
        "gemini-3.1-pro-low": {"displayName": "Gemini 3.1 Pro (Low)", "supportsThinking": true, "supportsImages": true, "maxTokens": 1_048_576, "maxOutputTokens": 65_535},
        "gemini-pro-agent": {"displayName": "Gemini Pro Agent", "supportsThinking": true, "supportsImages": true, "maxTokens": 2_000_000},
        "gemini-3.1-pro-high": {"displayName": "Gemini 3.1 Pro (High)", "maxTokens": 100},
        "gemini-3.7-flash-low": {"displayName": "Gemini 3.7 Flash (Low)", "supportsThinking": true, "supportsImages": true, "maxTokens": 1_048_576},
        "gemini-3.7-flash-high": {"displayName": "Gemini 3.7 Flash (High)", "supportsThinking": true, "supportsImages": true},
        "gemini-2.5-pro": {"displayName": "Denied"},
        "chat_20706": {"displayName": "Chat"},
        "gemini-4-argon-disabled": {"displayName": "Argon", "disabled": true},
        "gemini-internal-preview": {"displayName": "Internal", "isInternal": true},
        "gemini-new-1": {"displayName": "Gemini New One", "maxTokens": 300_000},
        "claude-sonnet-4-6": {"displayName": "Claude Sonnet 4.6"},
        "gemini-3-flash-preview": {"displayName": "Gemini 3 Flash Live", "supportsImages": true},
    }})
}

#[tokio::test]
#[serial]
async fn k2_antigravity_roster_collapses_members_and_keeps_the_seed_first() {
    let world = World::signed_in().await;
    mount_antigravity(&world.server, vec![ok(antigravity_roster())]).await;
    let layer = init_provider_layer(&app_config());
    let report = refresh(&layer).await;
    assert_eq!(report.status, DiscoveryStatus::Ok);
    let models = served(&layer);
    let mut expected = seed_then(&[
        ("gemini-3.1-pro", "Gemini 3.1 Pro", 2_000_000),
        ("gemini-3.7-flash", "Gemini 3.7 Flash", 1_048_576),
        ("gemini-new-1", "Gemini New One", 300_000),
    ]);
    // omp's dynamic merge overrides a bundled row's name and window from Antigravity.
    expected[3] = (
        "gemini-3-flash-preview".into(),
        "Gemini 3 Flash Live".into(),
        200_000,
    );
    assert_eq!(rows(&models), expected);
    assert_eq!(report.model_count, expected.len());
    let caps = |id: &str| {
        models
            .iter()
            .find(|m| m.model_id == id)
            .unwrap()
            .capabilities
            .clone()
    };
    assert_eq!(
        caps("gemini-3.1-pro"),
        HashSet::from([
            ModelCapability::ToolUse,
            ModelCapability::Vision,
            ModelCapability::Thinking
        ])
    );
    assert_eq!(
        caps("gemini-new-1"),
        HashSet::from([ModelCapability::ToolUse])
    );
    // The bundled reasoning row keeps thinking (omp ORs it) and gains nothing it lacked.
    assert_eq!(
        caps("gemini-3-flash-preview"),
        HashSet::from([
            ModelCapability::ToolUse,
            ModelCapability::Vision,
            ModelCapability::Thinking
        ])
    );
    assert!(models.iter().all(|m| !m.stale));
    // A collapsed row names omp's default wire id (its first live non-retired
    // member); every other row is sent as itself.
    let wires: Vec<_> = models
        .iter()
        .filter_map(|m| Some((m.model_id.as_str(), m.wire_model_id.as_deref()?)))
        .collect();
    assert_eq!(
        wires,
        [
            ("gemini-3.1-pro", "gemini-3.1-pro-low"),
            ("gemini-3.7-flash", "gemini-3.7-flash-low")
        ]
    );
    // The authoritative cache holds the same roster for the sent grant's scope.
    let entry = disk(PROVIDER).await;
    assert!(entry.authoritative);
    assert_eq!(
        entry.credential_fingerprint,
        target(&layer, PROVIDER).credential_fingerprint
    );
}

// ── K3: quota roster, derived pairs on the quota path only ────────────────

#[tokio::test]
#[serial]
async fn k3_quota_roster_forms_derived_pairs_and_antigravity_never_does() {
    let world = World::signed_in().await;
    mount_quota_path(
        &world.server,
        &[
            "gemini-9-pro",
            "gemini-9-pro-thinking",
            "gemini-9-flash",
            "gemini-9-flash-thinking",
            " gemini-3.1-pro-low ",
            "gemini-3.1-pro-low",
            "gemini-2.5-flash",
            "gemini-2.5-flash-thinking",
            "gemini-3-pro-preview",
            "gpt-oss-120b-medium",
        ],
    )
    .await;
    let layer = init_provider_layer(&app_config());
    assert_eq!(refresh(&layer).await.status, DiscoveryStatus::Ok);
    let models = served(&layer);
    assert_eq!(
        rows(&models),
        seed_then(&[
            ("gemini-3.1-pro", "Gemini 3.1 Pro", 1_048_576),
            // The `{rev}` template claims gemini-9-flash: no derived pair forms.
            ("gemini-9-flash", "gemini-9-flash", 1_048_576),
            (
                "gemini-9-flash-thinking",
                "gemini-9-flash-thinking",
                1_048_576
            ),
            ("gemini-9-pro", "gemini-9-pro", 1_048_576),
        ])
    );
    let wire = |id: &str| {
        models
            .iter()
            .find(|m| m.model_id == id)
            .unwrap()
            .wire_model_id
            .clone()
    };
    assert_eq!(wire("gemini-9-pro").as_deref(), Some("gemini-9-pro"));
    assert_eq!(wire("gemini-9-flash"), None);
    assert_eq!(
        wire("gemini-3.1-pro").as_deref(),
        Some("gemini-3.1-pro-low")
    );

    // The same twins on the Antigravity path stay two rows.
    world.server.reset().await;
    mount_antigravity(
        &world.server,
        vec![ok(json!({"models": {
            "gemini-9-pro": {"supportsThinking": true},
            "gemini-9-pro-thinking": {"supportsThinking": true},
        }}))],
    )
    .await;
    assert_eq!(refresh(&layer).await.status, DiscoveryStatus::Ok);
    let models = served(&layer);
    assert_eq!(
        rows(&models),
        seed_then(&[
            ("gemini-9-pro", "gemini-9-pro", 200_000),
            ("gemini-9-pro-thinking", "gemini-9-pro-thinking", 200_000),
        ])
    );
    assert!(models.iter().all(|m| m.wire_model_id.is_none()));
}

#[tokio::test]
#[serial]
async fn plain_empty_quota_serves_the_seed_and_a_failure_keeps_the_last_catalogue() {
    let world = World::signed_in().await;
    mount_quota_path(&world.server, &[]).await;
    let layer = init_provider_layer(&app_config());
    let report = refresh(&layer).await;
    assert_eq!(report.status, DiscoveryStatus::Ok);
    assert_eq!(rows(&served(&layer)), seed_then(&[]));

    // A quota failure with no cached roster: the served seed stays, Unavailable.
    for (failure, message) in [
        (
            ResponseTemplate::new(500),
            "Code Assist quota discovery failed: 500",
        ),
        (
            ResponseTemplate::new(200).set_body_string("{"),
            "Code Assist quota discovery returned invalid JSON",
        ),
        (
            ok(json!(["buckets"])),
            "Code Assist quota discovery returned invalid JSON",
        ),
    ] {
        world.server.reset().await;
        let _ = std::fs::remove_file(world.temp.path().join("models_cache.json"));
        mount_antigravity(
            &world.server,
            vec![ResponseTemplate::new(403), ResponseTemplate::new(403)],
        )
        .await;
        mount_quota(&world.server, vec![failure]).await;
        let fresh = init_provider_layer(&app_config());
        let report = refresh(&fresh).await;
        assert_eq!(report.status, DiscoveryStatus::Unavailable, "{message}");
        assert!(
            report
                .notice
                .as_deref()
                .is_some_and(|n| n.contains(message)),
            "{:?}",
            report.notice
        );
        assert_eq!(rows(&served(&fresh)), seed_then(&[]));
    }

    // After a live roster, a failure keeps serving that roster (20-1c: Cached).
    world.server.reset().await;
    mount_quota_path(&world.server, &["gemini-9-pro"]).await;
    let live = init_provider_layer(&app_config());
    assert_eq!(refresh(&live).await.status, DiscoveryStatus::Ok);
    world.server.reset().await;
    mount_antigravity(
        &world.server,
        vec![ResponseTemplate::new(403), ResponseTemplate::new(403)],
    )
    .await;
    mount_quota(&world.server, vec![ResponseTemplate::new(502)]).await;
    assert_eq!(refresh(&live).await.status, DiscoveryStatus::Cached);
    assert!(ids(&served(&live)).contains(&"gemini-9-pro".to_owned()));
}

#[tokio::test]
#[serial]
async fn plain_model_filter_applies_to_the_merged_roster() {
    let world = World::signed_in().await;
    mount_quota_path(&world.server, &["gemini-9-pro", "gemini-3.1-pro-low"]).await;
    let mut cfg = app_config();
    cfg.provider.insert(
        "gemini".into(),
        ProviderConfig {
            provider_id: PROVIDER.into(),
            kind: Some(PROVIDER.into()),
            model_id: MODEL.into(),
            api_key_env: String::new(),
            auth_header: None,
            enabled: true,
            base_url: None,
            context_window: None,
            supports_tools: None,
            discover_models: true,
            model_filter: vec!["gemini-3*".into()],
            cache_ttl_seconds: 3600,
        },
    );
    let layer = init_provider_layer(&cfg);
    refresh_target(
        target(&layer, "gemini"),
        &ModelCatalogCache::new(),
        Some(&layer.registry),
        true,
    )
    .await;
    assert_eq!(
        ids(&layer.registry.list_models_by_provider("gemini")),
        [
            "gemini-3-flash-preview",
            "gemini-3-pro-preview",
            "gemini-3.1-flash-lite-preview",
            "gemini-3.1-pro-preview",
            "gemini-3.1-pro"
        ]
    );
}

// ── AC3: cache scope ───────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn k1_scope_matches_startup_survives_rotation_and_never_crosses_accounts() {
    let world = World::signed_in().await;
    mount_quota_path(&world.server, &["gemini-live-a"]).await;
    let cfg = app_config();
    let first = init_provider_layer(&cfg);
    refresh(&first).await;
    let scope_a = disk(PROVIDER).await.credential_fingerprint;
    assert_eq!(scope_a, target(&first, PROVIDER).credential_fingerprint);

    // Restart: the startup scope of the same row serves the roster without a request.
    world.server.reset().await;
    let restarted = init_provider_layer(&cfg);
    assert_eq!(target(&restarted, PROVIDER).credential_fingerprint, scope_a);
    assert!(ids(&served(&restarted)).contains(&"gemini-live-a".to_owned()));
    let report = refresh_target(
        target(&restarted, PROVIDER),
        &ModelCatalogCache::new(),
        Some(&restarted.registry),
        false,
    )
    .await;
    assert_eq!(report.status, DiscoveryStatus::Ok);
    assert!(world.requests().await.is_empty(), "fresh scoped cache");

    // Access-token rotation (a recovered quota 401) keeps the account scope.
    mount_refresh(&world.server).await;
    mount_antigravity(
        &world.server,
        vec![ResponseTemplate::new(403), ResponseTemplate::new(403)],
    )
    .await;
    mount_quota(
        &world.server,
        vec![
            ResponseTemplate::new(401),
            ok(quota_body(&["gemini-live-a"])),
        ],
    )
    .await;
    assert_eq!(refresh(&restarted).await.status, DiscoveryStatus::Ok);
    assert_eq!(hits(&world.requests().await, TOKEN).len(), 1);
    assert_eq!(disk(PROVIDER).await.credential_fingerprint, scope_a);

    // Logout and a login to another account, even with reused row ids: the old
    // roster is never served and the first refresh fetches.
    FileAuthStore::new().remove(PROVIDER).await.unwrap();
    let _ = std::fs::remove_file(world.temp.path().join("auth.json"));
    world.server.reset().await;
    login(
        &world.server,
        "GEMINI-ACCESS-B",
        "project-b",
        "second@example.test",
    )
    .await;
    let other = init_provider_layer(&cfg);
    let scope_b = target(&other, PROVIDER).credential_fingerprint.clone();
    assert_ne!(scope_b, scope_a);
    assert!(!ids(&served(&other)).contains(&"gemini-live-a".to_owned()));
    let baseline = world.requests().await.len();
    mount_quota_path(&world.server, &["gemini-live-b"]).await;
    refresh_target(
        target(&other, PROVIDER),
        &ModelCatalogCache::new(),
        Some(&other.registry),
        false,
    )
    .await;
    let sent = world.since(baseline).await;
    assert_eq!(
        discovery_paths(&sent),
        [FETCH, FETCH, QUOTA],
        "a new account fetches"
    );
    assert_quota(&sent[2], "GEMINI-ACCESS-B", "project-b");
    let served_b = ids(&served(&other));
    assert!(served_b.contains(&"gemini-live-b".to_owned()));
    assert!(!served_b.contains(&"gemini-live-a".to_owned()));
}

#[tokio::test]
#[serial]
async fn k1_alias_entries_keep_separate_cache_keys() {
    let world = World::signed_in().await;
    Mock::given(method("POST"))
        .and(path(FETCH))
        .respond_with(ResponseTemplate::new(403))
        .mount(&world.server)
        .await;
    Mock::given(method("POST"))
        .and(path(QUOTA))
        .respond_with(ok(quota_body(&["gemini-9-pro"])))
        .mount(&world.server)
        .await;
    let entry = || ProviderConfig {
        provider_id: PROVIDER.into(),
        kind: Some(PROVIDER.into()),
        model_id: MODEL.into(),
        api_key_env: String::new(),
        auth_header: None,
        enabled: true,
        base_url: None,
        context_window: None,
        supports_tools: None,
        discover_models: true,
        model_filter: vec![],
        cache_ttl_seconds: 3600,
    };
    let mut cfg = AppConfig {
        model: MODEL.into(),
        ..Default::default()
    };
    cfg.provider.insert("work".into(), entry());
    cfg.provider.insert("home".into(), entry());
    let layer = init_provider_layer(&cfg);
    let (work, home) = (target(&layer, "work"), target(&layer, "home"));
    assert_eq!(work.kind, PROVIDER);
    assert_ne!(work.scope_context, home.scope_context);
    assert_ne!(work.credential_fingerprint, home.credential_fingerprint);
    for t in [work, home] {
        refresh_target(t, &ModelCatalogCache::new(), Some(&layer.registry), true).await;
    }
    assert_ne!(
        disk("work").await.credential_fingerprint,
        disk("home").await.credential_fingerprint
    );
    assert_eq!(
        disk("work").await.credential_fingerprint,
        work.credential_fingerprint
    );
    assert_eq!(
        disk("home").await.credential_fingerprint,
        home.credential_fingerprint
    );
}
