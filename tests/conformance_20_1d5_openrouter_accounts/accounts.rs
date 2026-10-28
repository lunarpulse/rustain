//! Account rows and the active row (K04–K07, AC4–AC7), through `flows::login`,
//! `auth_accounts::{accounts, select_account, logout_account}` over the real
//! file store, and `build_provider_for_config` → `stream_completion`, where the
//! served account is observed as the request credential at the stub.

use crate::conformance_20_1b_anthropic_subscription::flows::login_oauth as anthropic_login;
use crate::conformance_20_1d1b_codex_replay_discovery::common::{
    MODEL as CODEX_MODEL, NOW, World, codex, completed, login as codex_login, mount_script, sse,
    text,
};
use crate::conformance_20_1d3_copilot::chat_d4::{
    KeyUi, bearer, entry, key_login, mount_models, ok_turn, openai, opts,
};
use crate::conformance_20_1d3_copilot::common::{
    chat_sse, complete, header, options, reply, requests, user,
};
use parking_lot::Mutex;
use rustain::adapters::auth::flows::refresh::HttpOAuthRefresher;
use rustain::adapters::auth::flows::{self, FlowCtx, LoginResult};
use rustain::adapters::auth_store::FileAuthStore;
use rustain::domain::models::{
    AppConfig, AuthMethod, CredentialId, CredentialOrigin, ProviderConfig,
};
use rustain::domain::ports::{AuthStorePort, StreamingProvider};
use rustain::infrastructure::auth_accounts::{
    AccountError, ProviderAccounts, ServedSource, accounts, logout_account, select_account,
};
use rustain::infrastructure::key_resolver::{CredentialHandle, compose};
use rustain::infrastructure::model_discovery::rediscover_after_auth_change;
use rustain::infrastructure::provider_factory::{
    build_provider_for_config, build_provider_for_config_with_key, row_policy,
};
use serde_json::{Value, json};
use serial_test::serial;
use std::sync::Arc;
use wiremock::MockServer;

const CHAT: &str = "/v1/chat/completions";
const MESSAGES: &str = "/v1/messages";
const K1: &str = "sk-accounts-one";
const K2: &str = "sk-accounts-two";
const ENV_KEY: &str = "sk-accounts-env";

// ── fixtures ────────────────────────────────────────────────────────────────

fn store() -> Arc<dyn AuthStorePort> {
    Arc::new(FileAuthStore::new())
}

fn app(entries: &[&ProviderConfig]) -> AppConfig {
    let mut app = AppConfig::default();
    for cfg in entries {
        app.provider.insert(cfg.provider_id.clone(), (*cfg).clone());
    }
    app
}

fn auth_json(world: &World) -> std::path::PathBuf {
    world.dir.path().join("auth.json")
}

fn read_json(world: &World) -> Value {
    serde_json::from_slice(&std::fs::read(auth_json(world)).unwrap()).unwrap()
}

fn write_json(world: &World, value: &Value) {
    std::fs::write(auth_json(world), serde_json::to_vec(value).unwrap()).unwrap();
}

async fn rows(provider: &str) -> Vec<rustain::domain::models::StoredCredential> {
    store().snapshot().await.unwrap().rows(provider).to_vec()
}

async fn selected(provider: &str) -> Option<CredentialId> {
    store().snapshot().await.unwrap().selected(provider)
}

/// The real `flows::login(provider, ApiKey)` with `cfg` as its validation entry.
pub(super) async fn key_login_as(provider: &str, cfg: &ProviderConfig, key: &str) -> LoginResult {
    let ui = Arc::new(KeyUi {
        key: key.into(),
        prompts: Mutex::new(Vec::new()),
    });
    let ctx = FlowCtx::new(ui, Arc::new(|| NOW)).with_validation_config(cfg.clone());
    flows::login(provider, &AuthMethod::ApiKey, &ctx, &store())
        .await
        .expect("the key login succeeds")
}

/// The id of the stored `openai` row holding `key`.
async fn id_of(provider: &str, key: &str) -> CredentialId {
    rows(provider)
        .await
        .iter()
        .find(|row| row.credential.expose_api_key() == Some(key))
        .unwrap_or_else(|| panic!("no {provider} row holds the key"))
        .id
}

async fn last_chat_bearer(server: &MockServer) -> Option<String> {
    let posts = requests(server, "POST", CHAT).await;
    header(posts.last().expect("a Chat request"), "authorization")
}

/// One Chat turn on `provider`; the bearer it sent.
async fn chat_turn(server: &MockServer, provider: &Arc<dyn StreamingProvider>) -> Option<String> {
    ok_turn(provider, opts("", None)).await;
    last_chat_bearer(server).await
}

async fn served(app: &AppConfig, provider: &str) -> ProviderAccounts {
    accounts(&store(), app, provider).await.unwrap()
}

// Anthropic subscription fixtures.

fn anthropic_config(base: &str) -> ProviderConfig {
    serde_json::from_value(json!({"provider_id":"anthropic", "kind":"anthropic", "model_id":"claude-test", "api_key_env":"ANTHROPIC_API_KEY", "base_url":base})).unwrap()
}

/// A token answer for one Anthropic organization account (`ACCESS-<org>`).
fn org(name: &str) -> Value {
    json!({
        "access_token": format!("ACCESS-{name}"),
        "refresh_token": format!("REFRESH-{name}"),
        "account": {"uuid": format!("acct-{name}"), "email_address": format!("{name}@example.test")},
        "organization": {"uuid": format!("org-{name}"), "name": format!("Org {name}")},
    })
}

/// A real Anthropic browser login of `name`; the token mock is reset first so
/// each login gets its own answer.
async fn org_login(server: &MockServer, name: &str) -> CredentialId {
    server.reset().await;
    anthropic_login(server, &store(), NOW, 3600, org(name)).await;
    selected("anthropic")
        .await
        .expect("a login selects its row")
}

async fn mount_messages(server: &MockServer) {
    reply(
        server,
        "POST",
        MESSAGES,
        wiremock::ResponseTemplate::new(200).set_body_raw(
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
            "text/event-stream",
        ),
    )
    .await;
}

/// One Anthropic turn; returns the `authorization` and `x-api-key` it sent.
async fn anthropic_turn(
    server: &MockServer,
    provider: &Arc<dyn StreamingProvider>,
) -> (Option<String>, Option<String>) {
    complete(provider, vec![user("hi")], options("claude-test", 8, None))
        .await
        .expect("the Messages request opens");
    let posts = requests(server, "POST", MESSAGES).await;
    let last = posts.last().expect("a Messages request");
    (header(last, "authorization"), header(last, "x-api-key"))
}

async fn codex_turn(server: &MockServer) -> String {
    complete(&codex(), vec![user("hi")], options(CODEX_MODEL, 1024, None))
        .await
        .expect("the Codex request opens");
    let posts = requests(server, "POST", "/backend-api/codex/responses").await;
    header(posts.last().unwrap(), "chatgpt-account-id").expect("account header")
}

// ── K04 ─────────────────────────────────────────────────────────────────────

/// K04 (M04, AC4): a distinct key is a second row that becomes active and is
/// served; the first key logged in again keeps its id, gains a revision and
/// is served again, on the same adapter instance.
#[tokio::test]
#[serial]
async fn k04_distinct_keys_coexist_and_the_latest_login_is_served() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let cfg = openai(&server);
    let app = app(&[&cfg]);
    mount_models(&server).await;
    reply(&server, "POST", CHAT, chat_sse("ok")).await;

    key_login(&cfg, K1).await;
    let k1 = id_of("openai", K1).await;
    key_login(&cfg, K2).await;
    let k2 = id_of("openai", K2).await;
    assert_ne!(k1, k2, "a distinct key is another row");
    let stored = rows("openai").await;
    assert_eq!(stored.len(), 2);
    assert_eq!(
        stored.iter().map(|row| row.id).collect::<Vec<_>>(),
        [k1, k2]
    );
    assert_eq!(selected("openai").await, Some(k2));

    let provider = build_provider_for_config("openai", &cfg).expect("openai builds");
    ok_turn(&provider, opts("", None)).await;
    assert_eq!(last_chat_bearer(&server).await, bearer(K2));
    let listed = served(&app, "openai").await;
    assert_eq!(listed.served, Some(ServedSource::Row(k2)));
    assert!(listed.row(k2).unwrap().selected && listed.row(k2).unwrap().served);
    assert!(!listed.row(k1).unwrap().selected && !listed.row(k1).unwrap().served);

    // The same key again: same row, one more revision, active and served.
    let rev = stored[0].rev;
    key_login(&cfg, K1).await;
    let stored = rows("openai").await;
    assert_eq!(stored.len(), 2, "an equal key never appends");
    assert_eq!(stored[0].id, k1);
    assert_eq!(stored[0].rev, rev + 1);
    assert_eq!(stored[1].id, k2);
    assert_eq!(selected("openai").await, Some(k1));
    ok_turn(&provider, opts("", None)).await;
    assert_eq!(last_chat_bearer(&server).await, bearer(K1));
    assert_eq!(
        served(&app, "openai").await.served,
        Some(ServedSource::Row(k1))
    );
}

// ── K05 ─────────────────────────────────────────────────────────────────────

/// K05 (M05, AC5): logging out the active account removes that row only and
/// clears the choice, so the remaining row serves; other providers' rows and
/// choices are byte-unchanged; a stale or foreign id changes nothing; the
/// environment serves after the last row; provider-wide logout also clears
/// the choice.
#[tokio::test]
#[serial]
async fn k05_logout_of_one_account_keeps_the_others_and_falls_back() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    let cfg = openai(&server);
    let other = entry(&server, "deepseek", "deepseek", "DEEPSEEK_API_KEY");
    let app = app(&[&cfg, &other]);
    mount_models(&server).await;
    reply(&server, "POST", CHAT, chat_sse("ok")).await;

    key_login(&cfg, K1).await;
    key_login_as("deepseek", &other, "sk-deepseek-kept").await;
    key_login(&cfg, K2).await;
    let (k1, k2) = (id_of("openai", K1).await, id_of("openai", K2).await);
    let deepseek = id_of("deepseek", "sk-deepseek-kept").await;
    let untouched = |json: &Value| {
        (
            json["providers"]["deepseek"].clone(),
            json["selected"]["deepseek"].clone(),
        )
    };
    let before = untouched(&read_json(&world));
    assert_eq!(before.1, json!(deepseek.0));

    let after = logout_account(&store(), &app, "openai", k2).await.unwrap();
    assert_eq!(after.served, Some(ServedSource::Row(k1)));
    assert_eq!(
        after.rows.iter().map(|row| row.id).collect::<Vec<_>>(),
        [k1]
    );
    assert!(!after.rows[0].selected, "no neighbour is auto-selected");
    assert_eq!(selected("openai").await, None);
    let provider = build_provider_for_config("openai", &cfg).expect("openai builds");
    ok_turn(&provider, opts("", None)).await;
    assert_eq!(last_chat_bearer(&server).await, bearer(K1));
    assert_eq!(untouched(&read_json(&world)), before);

    // A stale id, or another provider's id, is not found and writes nothing.
    let bytes = std::fs::read(auth_json(&world)).unwrap();
    for id in [k2, deepseek, CredentialId(99)] {
        let error = logout_account(&store(), &app, "openai", id)
            .await
            .unwrap_err();
        assert!(matches!(error, AccountError::NotFound { .. }));
        assert_eq!(
            error.to_string(),
            format!(
                "No stored credential #{} for openai. Run `rustain auth accounts openai`.",
                id.0
            )
        );
    }
    assert_eq!(std::fs::read(auth_json(&world)).unwrap(), bytes);

    // The last row gone, the environment serves the provider.
    world.set("OPENAI_API_KEY", ENV_KEY);
    let after = logout_account(&store(), &app, "openai", k1).await.unwrap();
    assert!(after.rows.is_empty());
    assert_eq!(
        after.served,
        Some(ServedSource::Env("OPENAI_API_KEY".into()))
    );
    assert_eq!(after.served_phrase().as_deref(), Some("OPENAI_API_KEY"));
    ok_turn(&provider, opts("", None)).await;
    assert_eq!(last_chat_bearer(&server).await, bearer(ENV_KEY));
    assert_eq!(untouched(&read_json(&world)), before);

    // Provider-wide logout keeps its behaviour and clears the choice too.
    key_login(&cfg, K1).await;
    assert!(selected("openai").await.is_some());
    rustain::adapters::cli::auth::logout::run_auth_logout("openai", true, &store())
        .await
        .unwrap();
    let snapshot = store().snapshot().await.unwrap();
    assert!(snapshot.rows("openai").is_empty());
    assert_eq!(snapshot.selected("openai"), None);
    assert_eq!(untouched(&read_json(&world)), before);
}

// ── K06 ─────────────────────────────────────────────────────────────────────

/// K06 (M06, AC6): the second Anthropic organization login is served; the
/// chosen first account then serves the next request, a fresh resolver, and a
/// hand-reordered row array; with three accounts, removing a neighbour never
/// shifts the choice and removing the active one falls back to file order.
#[tokio::test]
#[serial]
async fn k06_the_active_anthropic_org_survives_restart_reorder_and_removal() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    let cfg = anthropic_config(&server.uri());
    let app = app(&[&cfg]);
    let a = org_login(&server, "a").await;
    let b = org_login(&server, "b").await;
    assert_ne!(a, b);
    assert_eq!(rows("anthropic").await.len(), 2);
    mount_messages(&server).await;
    let provider = build_provider_for_config("anthropic", &cfg).expect("anthropic builds");
    assert_eq!(
        anthropic_turn(&server, &provider).await.0.as_deref(),
        Some("Bearer ACCESS-b"),
        "the latest login is served"
    );

    let after = select_account(&store(), &app, "anthropic", a)
        .await
        .unwrap();
    assert_eq!(after.served, Some(ServedSource::Row(a)));
    assert_eq!(after.row(a).unwrap().label, "a@example.test (Org a)");
    assert_eq!(
        anthropic_turn(&server, &provider).await.0.as_deref(),
        Some("Bearer ACCESS-a"),
        "the same adapter serves the new choice on its next request"
    );

    // Restart: a fresh resolver over the same file.
    let fresh = CredentialHandle::new(
        compose(
            store(),
            Arc::new(HttpOAuthRefresher::new()),
            Arc::new(|| NOW),
        ),
        "anthropic".into(),
        vec!["ANTHROPIC_API_KEY".into()],
        row_policy("anthropic", "anthropic", &server.uri()),
        None,
    );
    let resolved = fresh.current().await.unwrap();
    assert_eq!(resolved.auth.bearer_secret().expose_secret(), "ACCESS-a");
    assert_eq!(resolved.row.unwrap().id, a);

    // Rows reordered by hand (ids kept): the choice is by id, not position.
    let mut json = read_json(&world);
    json["providers"]["anthropic"]
        .as_array_mut()
        .unwrap()
        .reverse();
    write_json(&world, &json);
    assert_eq!(
        rows("anthropic")
            .await
            .iter()
            .map(|row| row.id)
            .collect::<Vec<_>>(),
        [b, a]
    );
    let rebuilt = build_provider_for_config("anthropic", &cfg).expect("anthropic builds");
    assert_eq!(
        anthropic_turn(&server, &rebuilt).await.0.as_deref(),
        Some("Bearer ACCESS-a")
    );

    // A, B and C: choose C, remove A, C still serves; remove C, B serves.
    let c = org_login(&server, "c").await;
    mount_messages(&server).await;
    select_account(&store(), &app, "anthropic", b)
        .await
        .unwrap();
    select_account(&store(), &app, "anthropic", c)
        .await
        .unwrap();
    let after = logout_account(&store(), &app, "anthropic", a)
        .await
        .unwrap();
    assert_eq!(after.served, Some(ServedSource::Row(c)));
    assert_eq!(
        anthropic_turn(&server, &provider).await.0.as_deref(),
        Some("Bearer ACCESS-c")
    );
    let after = logout_account(&store(), &app, "anthropic", c)
        .await
        .unwrap();
    assert_eq!(selected("anthropic").await, None);
    assert_eq!(after.served, Some(ServedSource::Row(b)));
    assert_eq!(
        anthropic_turn(&server, &provider).await.0.as_deref(),
        Some("Bearer ACCESS-b")
    );
}

/// K06 (M06, AC6): Codex workspace accounts switch the same way; the account
/// is observed as `chatgpt-account-id`.
#[tokio::test]
#[serial]
async fn k06_codex_workspace_accounts_follow_the_active_row() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let app = app(&[&crate::conformance_20_1d1b_codex_replay_discovery::common::config()]);
    codex_login(&server, "acct-A").await;
    let a = selected("openai-codex").await.unwrap();
    server.reset().await;
    codex_login(&server, "acct-B").await;
    let b = selected("openai-codex").await.unwrap();
    assert_ne!(a, b);
    server.reset().await;
    mount_script(&server, vec![sse(&[text("ok"), completed()])]).await;

    assert_eq!(codex_turn(&server).await, "acct-B");
    select_account(&store(), &app, "openai-codex", a)
        .await
        .unwrap();
    assert_eq!(codex_turn(&server).await, "acct-A");
    let after = logout_account(&store(), &app, "openai-codex", a)
        .await
        .unwrap();
    assert_eq!(after.served, Some(ServedSource::Row(b)));
    assert_eq!(codex_turn(&server).await, "acct-B");
}

// ── K07 ─────────────────────────────────────────────────────────────────────

/// K07 (M06, AC7): a selected login key beats an unselected OAuth row and the
/// reverse; a selected OAuth row the endpoint may not receive is skipped.
#[tokio::test]
#[serial]
async fn k07_selected_rows_rank_above_the_default_stored_order_only_where_eligible() {
    let server = MockServer::start().await;
    let foreign = MockServer::start().await;
    let _world = World::new(&server);
    let cfg = anthropic_config(&server.uri());
    let gateway = anthropic_config(&foreign.uri());
    let a = org_login(&server, "a").await;
    mount_messages(&server).await;
    mount_messages(&foreign).await;
    reply(
        &server,
        "GET",
        "/v1/models",
        wiremock::ResponseTemplate::new(200),
    )
    .await;
    let key = key_login_as("anthropic", &cfg, "sk-ant-login-key")
        .await
        .credential_id;
    assert_eq!(selected("anthropic").await, Some(key));

    // The selected login key beats the unselected OAuth row.
    let official = build_provider_for_config("anthropic", &cfg).unwrap();
    assert_eq!(
        anthropic_turn(&server, &official).await,
        (None, Some("sk-ant-login-key".into()))
    );
    assert_eq!(
        served(&app(&[&cfg]), "anthropic").await.served,
        Some(ServedSource::Row(key))
    );
    // The selected OAuth row beats the unselected login key.
    select_account(&store(), &app(&[&cfg]), "anthropic", a)
        .await
        .unwrap();
    assert_eq!(
        anthropic_turn(&server, &official).await,
        (Some("Bearer ACCESS-a".into()), None)
    );
    // A gateway base may not receive the grant: the selection is skipped.
    assert_eq!(
        served(&app(&[&gateway]), "anthropic").await.served,
        Some(ServedSource::Row(key))
    );
    let through_gateway = build_provider_for_config("anthropic", &gateway).unwrap();
    assert_eq!(
        anthropic_turn(&foreign, &through_gateway).await,
        (None, Some("sk-ant-login-key".into()))
    );
    assert!(
        !foreign
            .received_requests()
            .await
            .unwrap()
            .iter()
            .any(|request| header(request, "authorization").is_some()),
        "the grant never reaches the gateway"
    );
}

/// K07 (M07, AC7): a selected `Stored`-origin key (migrated from a v1 file)
/// never outranks `OPENAI_API_KEY`; a runtime key beats any selection.
#[tokio::test]
#[serial]
async fn k07_a_selected_stored_key_stays_below_the_environment_and_runtime_wins() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    let cfg = openai(&server);
    let app = app(&[&cfg]);
    reply(&server, "POST", CHAT, chat_sse("ok")).await;
    // The legacy v1 layout only a pre-v2 binary wrote; the real reader migrates it.
    std::fs::write(
        auth_json(&world),
        br#"{"version":1,"providers":{"openai":{"type":"api_key","api_key":"sk-legacy-stored","last_validated":null}}}"#,
    )
    .unwrap();
    let legacy = rows("openai").await;
    assert_eq!(legacy[0].origin, CredentialOrigin::Stored);
    let id = legacy[0].id;
    select_account(&store(), &app, "openai", id).await.unwrap();
    assert_eq!(selected("openai").await, Some(id));
    let provider = build_provider_for_config("openai", &cfg).unwrap();
    ok_turn(&provider, opts("", None)).await;
    assert_eq!(last_chat_bearer(&server).await, bearer("sk-legacy-stored"));

    world.set("OPENAI_API_KEY", ENV_KEY);
    let listed = served(&app, "openai").await;
    assert_eq!(
        listed.served,
        Some(ServedSource::Env("OPENAI_API_KEY".into()))
    );
    assert!(listed.row(id).unwrap().selected && !listed.row(id).unwrap().served);
    ok_turn(&provider, opts("", None)).await;
    assert_eq!(last_chat_bearer(&server).await, bearer(ENV_KEY));

    // A candidate (runtime) key wins over the environment and the choice.
    let runtime =
        build_provider_for_config_with_key("openai", &cfg, "sk-runtime-candidate").unwrap();
    ok_turn(&runtime, opts("", None)).await;
    assert_eq!(
        last_chat_bearer(&server).await,
        bearer("sk-runtime-candidate")
    );
}

/// K07 (M06, AC7): a disabled, dangling or unrecognised selected id falls
/// back to the default order (the first login key in file order); a disabled
/// row cannot be selected and says so.
#[tokio::test]
#[serial]
async fn k07_an_unusable_choice_falls_back_to_the_default_order() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    let cfg = openai(&server);
    let app = app(&[&cfg]);
    mount_models(&server).await;
    reply(&server, "POST", CHAT, chat_sse("ok")).await;
    key_login(&cfg, K1).await;
    key_login(&cfg, K2).await;
    let (k1, k2) = (id_of("openai", K1).await, id_of("openai", K2).await);
    let provider = build_provider_for_config("openai", &cfg).unwrap();
    assert_eq!(chat_turn(&server, &provider).await, bearer(K2));

    // Dangling: the choice names no row.
    let mut json = read_json(&world);
    json["selected"]["openai"] = json!(99);
    write_json(&world, &json);
    assert_eq!(chat_turn(&server, &provider).await, bearer(K1));
    // Unrecognised: the choice names a row this binary cannot read.
    let mut json = read_json(&world);
    json["providers"]["openai"]
        .as_array_mut()
        .unwrap()
        .push(json!({"id": 50, "type": "future", "opaque": true}));
    json["next_id"] = json!(51);
    json["selected"]["openai"] = json!(50);
    write_json(&world, &json);
    assert_eq!(chat_turn(&server, &provider).await, bearer(K1));
    assert!(matches!(
        select_account(&store(), &app, "openai", CredentialId(50)).await,
        Err(AccountError::NotFound { .. })
    ));
    // Disabled: the chosen row's grant was revoked.
    select_account(&store(), &app, "openai", k2).await.unwrap();
    assert_eq!(chat_turn(&server, &provider).await, bearer(K2));
    let stamp = rows("openai").await[1].stamp();
    store()
        .disable_if_matches("openai", stamp, "revoked")
        .await
        .unwrap();
    assert_eq!(
        selected("openai").await,
        Some(k2),
        "a disable keeps the choice"
    );
    assert_eq!(chat_turn(&server, &provider).await, bearer(K1));
    assert_eq!(
        served(&app, "openai").await.served,
        Some(ServedSource::Row(k1))
    );
    let error = select_account(&store(), &app, "openai", k2)
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        format!(
            "Credential #{} for openai is disabled (revoked); log in again.",
            k2.0
        )
    );
}

// ── plain tests ─────────────────────────────────────────────────────────────

/// A selection-only change rediscovers with the newly served credential; a
/// no-op `auth use` of the already-active row does not.
#[tokio::test]
#[serial]
async fn rediscovery_follows_a_selection_change_and_skips_a_no_op() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let cfg = openai(&server);
    let app = app(&[&cfg]);
    mount_models(&server).await;
    key_login(&cfg, K1).await;
    key_login(&cfg, K2).await;
    let k1 = id_of("openai", K1).await;
    let before = store().snapshot().await.unwrap();
    let probes = requests(&server, "GET", "/v1/models").await.len();
    select_account(&store(), &app, "openai", k1).await.unwrap();
    let report = rediscover_after_auth_change("openai", &app, &before)
        .await
        .expect("a new active row is a credential change");
    assert_eq!(report.model_count, 1);
    let gets = requests(&server, "GET", "/v1/models").await;
    assert_eq!(gets.len(), probes + 1);
    assert_eq!(header(gets.last().unwrap(), "authorization"), bearer(K1));

    let before = store().snapshot().await.unwrap();
    select_account(&store(), &app, "openai", k1).await.unwrap();
    assert!(
        rediscover_after_auth_change("openai", &app, &before)
            .await
            .is_none()
    );
    assert_eq!(
        requests(&server, "GET", "/v1/models").await.len(),
        probes + 1
    );
}
