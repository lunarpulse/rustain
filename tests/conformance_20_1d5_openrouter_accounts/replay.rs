//! K13 (AC14, M13): replay never follows a conversation to another account.
//! Codex (two workspace accounts), OpenRouter (two key logins on a `Thinking`
//! model) and Gemini CLI (two Google identities): a turn under account A is
//! folded by the production reducer, `auth_accounts::select_account(B)` makes
//! the next request of the same session and model carry none of A's items,
//! and switching back to A replays them. Every row comes from `flows::login`
//! against the loopback stub; every scoped item comes from the adapter stream.

use crate::conformance_20_1d1b_codex_replay_discovery::common::{
    MODEL as CODEX_MODEL, World, app_config as codex_app, assert_replays, codex, completed, enc,
    login as codex_login, mentions, mount_script, posts, reasoning, replayed, sse, summary, text,
};
use crate::conformance_20_1d1b_codex_replay_discovery::replay::{
    collect, fold, options, replay_chunks, user as chat_user,
};
use crate::conformance_20_1d3_copilot::common::{header, json_body, requests};
use crate::conformance_20_1d4_gemini_cli::auth::{
    Env, MODEL as GEMINI_MODEL, PROJECT, PROVIDER as GEMINI, STREAM, build as gemini,
    config as gemini_config, env as gemini_env, login as gemini_login, options as gemini_options,
    user as api_user,
};
use crate::conformance_20_1d4_gemini_cli::replay::{
    body as gemini_body, chat_user as gemini_chat_user, fold as gemini_fold, frame, signatures,
    sse as gemini_sse,
};
use futures::StreamExt;
use rustain::adapters::auth_store::FileAuthStore;
use rustain::domain::models::{
    AppConfig, Conversation, CredentialId, Message, ModelCapability, ModelDescriptor,
    ProviderConfig, ProviderReplayItem, StreamChunk,
};
use rustain::domain::ports::{AuthStorePort, StreamingProvider};
use rustain::domain::services::message_builder::build_api_messages;
use rustain::infrastructure::auth_accounts::select_account;
use rustain::infrastructure::provider_factory::build_provider_for_config;
use serde_json::{Value, json};
use serial_test::serial;
use std::sync::Arc;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const SESSION: &str = "k13-session";

fn store() -> Arc<dyn AuthStorePort> {
    Arc::new(FileAuthStore::new())
}

async fn selected(provider: &str) -> CredentialId {
    store()
        .snapshot()
        .await
        .unwrap()
        .selected(provider)
        .expect("the latest login is the active row")
}

async fn select(app: &AppConfig, provider: &str, id: CredentialId) {
    select_account(&store(), app, provider, id)
        .await
        .expect("the stored row is selectable");
}

/// The scope stamped on the stream's replay items (all items of one response
/// share it).
fn stamped(chunks: &[StreamChunk]) -> Option<String> {
    let scopes: Vec<_> = chunks
        .iter()
        .filter_map(|chunk| match chunk {
            StreamChunk::ProviderReplay { item } => Some(item.scope.clone()),
            _ => None,
        })
        .collect();
    assert!(!scopes.is_empty(), "the turn produced replay items");
    assert!(scopes.windows(2).all(|pair| pair[0] == pair[1]));
    scopes[0].clone()
}

/// The only hand-written item of this keystone, disclosed here (AC14): an
/// item read from a session file written before replay was scoped, i.e. the
/// pre-scope JSON shape with no `scope` key. Its provenance is unknown, so it
/// must never be replayed.
fn legacy(provider: &str, model: &str) -> ProviderReplayItem {
    serde_json::from_value(json!({
        "provider": provider,
        "model": model,
        "payload": {
            "type": "reasoning",
            "summary": [
                {"type": "summary_text", "text": summary("legacy")},
                {"type": "summary_text", "text": "second part"},
            ],
            "encrypted_content": enc("legacy"),
        },
    }))
    .unwrap()
}

/// Turn 1 folded by the production reducer, persisted and reloaded as a session
/// file, then the follow-up user turn; the legacy item rides the folded turn.
fn follow_up(
    mut conversation: Conversation,
    chunks: Vec<StreamChunk>,
    provider: &str,
) -> Vec<Message> {
    fold(&mut conversation, chunks);
    let saved = serde_json::to_string(&conversation).unwrap();
    assert!(
        saved.contains("\"scope\""),
        "the persisted item keeps its scope"
    );
    let mut reloaded: Conversation = serde_json::from_str(&saved).unwrap();
    reloaded.messages.push(chat_user("next"));
    let mut messages = build_api_messages(&reloaded);
    let turn = messages
        .iter_mut()
        .find(|message| !message.provider_replay.is_empty())
        .expect("the folded turn carries account A's item");
    let model = turn.provider_replay[0].model.clone();
    turn.provider_replay.push(legacy(provider, &model));
    messages
}

fn first_turn() -> (Conversation, Vec<Message>) {
    let mut conversation = Conversation {
        id: SESSION.into(),
        ..Default::default()
    };
    conversation.messages.push(chat_user("hello"));
    let first = build_api_messages(&conversation);
    (conversation, first)
}

// ── Codex ───────────────────────────────────────────────────────────────────

const CODEX_PATH: &str = "/backend-api/codex/responses";

async fn codex_account(server: &MockServer) -> String {
    header(
        requests(server, "POST", CODEX_PATH).await.last().unwrap(),
        "chatgpt-account-id",
    )
    .expect("account header")
}

#[tokio::test]
#[serial]
async fn k13_codex_replay_follows_only_the_producing_account() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let app = codex_app();
    codex_login(&server, "acct-A").await;
    let a = selected("openai-codex").await;
    server.reset().await;
    codex_login(&server, "acct-B").await;
    let b = selected("openai-codex").await;
    assert_ne!(a, b);
    server.reset().await;
    mount_script(
        &server,
        vec![
            sse(&[reasoning("acct-a"), text("first"), completed()]),
            sse(&[text("ok"), completed()]),
        ],
    )
    .await;
    let provider = codex();

    // Turn 1 under A: the item is stamped with A's scope.
    select(&app, "openai-codex", a).await;
    let (conversation, first) = first_turn();
    let chunks = collect(&provider, first, options(CODEX_MODEL, SESSION)).await;
    assert_eq!(replay_chunks(&chunks), 1);
    assert_eq!(codex_account(&server).await, "acct-A");
    let scope_a = provider.discovery_scope("openai-codex").await;
    assert!(scope_a.is_some());
    assert_eq!(stamped(&chunks), scope_a, "stamped with A's scope");
    let history = follow_up(conversation, chunks, "openai-codex");

    // B, same session and model: none of A's items, and never the legacy one.
    select(&app, "openai-codex", b).await;
    assert_ne!(provider.discovery_scope("openai-codex").await, scope_a);
    collect(&provider, history.clone(), options(CODEX_MODEL, SESSION)).await;
    assert_eq!(codex_account(&server).await, "acct-B");
    let sent = posts(&server).await.pop().unwrap();
    assert!(replayed(&sent).is_empty(), "{sent}");
    assert!(
        !mentions(&sent, "acct-a") && !mentions(&sent, "legacy"),
        "{sent}"
    );

    // Back to A: A's item is replayed; the unscoped legacy item still is not.
    select(&app, "openai-codex", a).await;
    collect(&provider, history, options(CODEX_MODEL, SESSION)).await;
    assert_eq!(codex_account(&server).await, "acct-A");
    let sent = posts(&server).await.pop().unwrap();
    assert_replays(&sent, "acct-a");
    assert_eq!(replayed(&sent).len(), 1, "{sent}");
    assert!(!mentions(&sent, "legacy"), "{sent}");
}

// ── OpenRouter ──────────────────────────────────────────────────────────────

/// Served with `Thinking` (from discovery), so `include` is requested.
const OR_MODEL: &str = "openai/gpt-5.5";
const OR_RESPONSES: &str = "/api/v1/responses";
const KEY_A: &str = "sk-or-v1-k13-account-a";
const KEY_B: &str = "sk-or-v1-k13-account-b";

fn openrouter(server: &MockServer) -> ProviderConfig {
    serde_json::from_value(json!({
        "provider_id": "openrouter",
        "kind": "openrouter",
        "model_id": OR_MODEL,
        "api_key_env": "OPENROUTER_API_KEY",
        "base_url": format!("{}/api/v1", server.uri()),
    }))
    .unwrap()
}

/// The served catalogue row as discovery delivers it to the adapter at startup
/// (`set_discovered_models`): a tool-capable reasoning model.
fn thinking_row() -> ModelDescriptor {
    ModelDescriptor {
        model_id: OR_MODEL.into(),
        display_name: "GPT-5.5".into(),
        provider_id: "openrouter".into(),
        context_window: 1_050_000,
        capabilities: [ModelCapability::ToolUse, ModelCapability::Thinking]
            .into_iter()
            .collect(),
        pricing_tier: None,
        stale: false,
        wire_model_id: None,
    }
}

async fn openrouter_post(server: &MockServer) -> (Value, Option<String>) {
    let sent = requests(server, "POST", OR_RESPONSES).await;
    let last = sent.last().expect("a Responses request");
    (json_body(last), header(last, "authorization"))
}

#[tokio::test]
#[serial]
async fn k13_openrouter_replay_follows_only_the_producing_key() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let cfg = openrouter(&server);
    let mut app = AppConfig::default();
    app.provider.insert("openrouter".into(), cfg.clone());
    Mock::given(method("GET"))
        .and(path("/api/v1/auth/key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {}})))
        .mount(&server)
        .await;
    let a = super::accounts::key_login_as("openrouter", &cfg, KEY_A)
        .await
        .credential_id;
    let b = super::accounts::key_login_as("openrouter", &cfg, KEY_B)
        .await
        .credential_id;
    assert_ne!(a, b);
    // First registered wins: turn 1 once, then plain answers.
    Mock::given(method("POST"))
        .and(path(OR_RESPONSES))
        .respond_with(sse(&[reasoning("or-a"), text("first"), completed()]))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(OR_RESPONSES))
        .respond_with(sse(&[text("ok"), completed()]))
        .mount(&server)
        .await;
    let provider = build_provider_for_config("openrouter", &cfg).unwrap();
    provider.set_discovered_models(vec![thinking_row()]);

    // Turn 1 under A on the default Responses route, encrypted reasoning requested.
    select(&app, "openrouter", a).await;
    let (conversation, first) = first_turn();
    let chunks = collect(&provider, first, options(OR_MODEL, SESSION)).await;
    assert_eq!(replay_chunks(&chunks), 1);
    let (sent, bearer) = openrouter_post(&server).await;
    assert_eq!(bearer.as_deref(), Some(format!("Bearer {KEY_A}").as_str()));
    assert_eq!(sent["include"], json!(["reasoning.encrypted_content"]));
    let scope_a = provider.discovery_scope("openrouter").await;
    assert!(scope_a.is_some());
    assert_eq!(stamped(&chunks), scope_a, "stamped with key A's scope");
    let history = follow_up(conversation, chunks, "openrouter");

    // Key B, same session and model: none of A's items.
    select(&app, "openrouter", b).await;
    collect(&provider, history.clone(), options(OR_MODEL, SESSION)).await;
    let (sent, bearer) = openrouter_post(&server).await;
    assert_eq!(bearer.as_deref(), Some(format!("Bearer {KEY_B}").as_str()));
    assert!(replayed(&sent).is_empty(), "{sent}");
    assert!(
        !mentions(&sent, "or-a") && !mentions(&sent, "legacy"),
        "{sent}"
    );

    // Back to key A: A's item is replayed, the legacy item is not.
    select(&app, "openrouter", a).await;
    collect(&provider, history, options(OR_MODEL, SESSION)).await;
    let (sent, bearer) = openrouter_post(&server).await;
    assert_eq!(bearer.as_deref(), Some(format!("Bearer {KEY_A}").as_str()));
    assert_replays(&sent, "or-a");
    assert_eq!(replayed(&sent).len(), 1, "{sent}");
    assert!(!mentions(&sent, "legacy"), "{sent}");
}

// ── Gemini CLI ──────────────────────────────────────────────────────────────

const ACCESS_A: &str = "GEMINI-ACCESS-K13-A";
const ACCESS_B: &str = "GEMINI-ACCESS-K13-B";
const OTHER_MODEL: &str = "gemini-2.5-pro";
/// Every fixture thought signature starts with this Base64 prefix.
const MARK: &str = "U0lH";

/// Signed thinking `T`, text `V`, then the signature-only part and `STOP`.
fn signed_text_turn() -> ResponseTemplate {
    gemini_sse(&[
        frame(
            json!([{"thought": true, "text": "T", "thoughtSignature": "U0lHMA=="}]),
            None,
        ),
        frame(json!([{"text": "V"}]), None),
        frame(json!([{"text": "", "thoughtSignature": "U0lHMQ=="}]), None),
        frame(json!([]), Some("STOP")),
    ])
}

async fn gemini_turn(
    server: &MockServer,
    provider: &Arc<dyn StreamingProvider>,
    messages: Vec<Message>,
    model: &str,
) -> (String, Option<String>) {
    let chunks: Vec<StreamChunk> = provider
        .stream_completion(messages, gemini_options(model))
        .await
        .unwrap()
        .collect()
        .await;
    assert!(
        !chunks
            .iter()
            .any(|chunk| matches!(chunk, StreamChunk::Error { .. })),
        "{chunks:?}"
    );
    let sent = requests(server, "POST", STREAM).await;
    let last = sent.last().expect("a Cloud Code Assist request");
    (
        String::from_utf8(last.body.clone()).unwrap(),
        header(last, "authorization"),
    )
}

/// B's request carries neither A's signed parts nor an `OtherModel` rebuild of
/// them: it is byte-identical to the same history with every replay item
/// removed, on the item's own model and on another Gemini model.
#[tokio::test]
#[serial]
async fn k13_gemini_signatures_follow_only_the_producing_identity() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().to_str().unwrap().to_owned();
    let _env = (
        gemini_env(dir.path(), &server.uri()),
        Env::set(&[
            ("RUSTAIN_CONFIG_DIR", Some(data.as_str())),
            ("RUSTAIN_MODELS_DEV_URL", Some("http://127.0.0.1:1")),
            ("OPENAI_API_KEY", None),
        ]),
    );
    let mut app = AppConfig::default();
    app.provider.insert(GEMINI.into(), gemini_config(None));
    let a = gemini_login(&server, ACCESS_A, PROJECT, "account-a@example.test")
        .await
        .credential_id;
    server.reset().await;
    let b = gemini_login(&server, ACCESS_B, PROJECT, "account-b@example.test")
        .await
        .credential_id;
    assert_ne!(a, b);
    server.reset().await;
    Mock::given(method("POST"))
        .and(path(STREAM))
        .respond_with(signed_text_turn())
        .mount(&server)
        .await;
    let provider = gemini();

    // Turn 1 under A, folded by the TUI reducer.
    select(&app, GEMINI, a).await;
    let chunks: Vec<StreamChunk> = provider
        .stream_completion(vec![api_user("hello")], gemini_options(GEMINI_MODEL))
        .await
        .unwrap()
        .collect()
        .await;
    let scope_a = provider.discovery_scope(GEMINI).await;
    assert!(scope_a.is_some());
    assert_eq!(stamped(&chunks), scope_a, "stamped with A's scope");
    let mut conversation = Conversation {
        id: SESSION.into(),
        ..Default::default()
    };
    conversation.messages.push(gemini_chat_user("hello"));
    gemini_fold(&mut conversation, chunks);
    conversation.messages.push(gemini_chat_user("next"));
    let history = build_api_messages(&conversation);
    assert!(history.iter().any(|m| !m.provider_replay.is_empty()));
    let mut stripped = history.clone();
    for message in &mut stripped {
        message.provider_replay.clear();
    }

    // B: same model, then another Gemini model; both equal the item-free history.
    select(&app, GEMINI, b).await;
    for model in [GEMINI_MODEL, OTHER_MODEL] {
        let (with_items, bearer) = gemini_turn(&server, &provider, history.clone(), model).await;
        assert_eq!(
            bearer.as_deref(),
            Some(format!("Bearer {ACCESS_B}").as_str())
        );
        let (without, _) = gemini_turn(&server, &provider, stripped.clone(), model).await;
        assert!(!with_items.contains(MARK), "{with_items}");
        assert!(
            signatures(&gemini_body(&with_items)).is_empty(),
            "{with_items}"
        );
        assert_eq!(gemini_body(&with_items), gemini_body(&without), "{model}");
    }

    // Back to A: A's signed parts are replayed.
    select(&app, GEMINI, a).await;
    let (sent, bearer) = gemini_turn(&server, &provider, history, GEMINI_MODEL).await;
    assert_eq!(
        bearer.as_deref(),
        Some(format!("Bearer {ACCESS_A}").as_str())
    );
    let signed = signatures(&gemini_body(&sent));
    assert!(
        signed.iter().any(|s| s == "U0lHMA==") && signed.iter().any(|s| s == "U0lHMQ=="),
        "{sent}"
    );
}
