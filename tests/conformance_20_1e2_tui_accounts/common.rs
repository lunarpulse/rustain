//! The openai world the keystones share, on top of the 20-1e TUI harness:
//! TUI key logins against a loopback stub, account-step waits, router turns
//! and the 60×16 fit check.
#![allow(dead_code)]

use crate::conformance_20_1d3_copilot::common::{drain, options, text_of, user};
pub(crate) use crate::conformance_20_1e_tui_login::common::{
    Held, Tui, World, accept, config_with, listing, reply_sse_text,
};
use rustain::adapters::auth_store::FileAuthStore;
use rustain::adapters::tui::app::InputAction;
use rustain::adapters::tui::handlers::model_switch;
use rustain::domain::events::{DomainKey, LoginEvent};
use rustain::domain::models::{Conversation, ProviderConfig, StreamingState};
use rustain::domain::ports::{AuthStorePort, ProviderInfoPort, StreamingProvider};
use std::sync::Arc;
use tokio::net::TcpListener;

pub(crate) const K1: &str = "sk-openai-K1-SECRET-0001";
pub(crate) const K2: &str = "sk-openai-K2-SECRET-0002";

pub(crate) async fn stub() -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    (listener, base)
}

/// The smoke's `[provider.openai]` entry against the stub.
pub(crate) fn openai(base: &str, enabled: bool) -> ProviderConfig {
    ProviderConfig {
        provider_id: "openai".into(),
        model_id: "m-one".into(),
        api_key_env: "OPENAI_API_KEY".into(),
        enabled,
        kind: Some("openai".into()),
        base_url: Some(format!("{base}/openai/v1").into()),
        auth_header: None,
        context_window: None,
        supports_tools: None,
        discover_models: true,
        model_filter: vec!["*".into()],
        cache_ttl_seconds: 0,
    }
}

pub(crate) fn bearer(held: &Held) -> Option<String> {
    held.header("authorization")
}

/// The roster the stub serves a key: K1 → `m-one`, K2 → `m-two`.
pub(crate) fn roster(key: &str) -> serde_json::Value {
    listing(if key == K1 { &["m-one"] } else { &["m-two"] })
}

/// Answers one `/openai/v1/models` request with the bearer's roster; returns the bearer.
pub(crate) async fn answer_models(listener: &TcpListener) -> String {
    let held = accept(listener).await;
    assert_eq!(held.path(), "/openai/v1/models");
    let key = bearer(&held)
        .and_then(|value| value.strip_prefix("Bearer ").map(str::to_owned))
        .expect("a bearer");
    held.json(&roster(&key)).await;
    key
}

/// A TUI `/login openai` with `key`: the masked field, validation at the stub,
/// `Finished`, then the login's own discovery and its `Discovered` line.
pub(crate) async fn key_login(tui: &mut Tui, listener: &TcpListener, key: &str) {
    tui.submit("/login openai");
    tui.until("masked key field", Tui::field_pending).await;
    tui.paste(key);
    tui.key(DomainKey::Enter);
    assert_eq!(
        answer_models(listener).await,
        key,
        "validation carries the key"
    );
    tui.until_login("login finished", |event| {
        matches!(event, LoginEvent::Finished { .. })
    })
    .await;
    assert_eq!(
        answer_models(listener).await,
        key,
        "discovery carries the key"
    );
    tui.until_login("discovery line", |event| {
        matches!(event, LoginEvent::Discovered { .. })
    })
    .await;
    tui.drain();
}

/// The stored openai row ids in file order.
pub(crate) async fn row_ids() -> Vec<u64> {
    let store: Arc<dyn AuthStorePort> = Arc::new(FileAuthStore::new());
    store
        .snapshot()
        .await
        .unwrap()
        .rows("openai")
        .iter()
        .map(|row| row.id.0)
        .collect()
}

/// Waits until the account step shows its rows.
pub(crate) async fn until_accounts(tui: &mut Tui) {
    tui.until("account rows", |tui| {
        tui.state
            .login
            .account
            .as_ref()
            .is_some_and(|step| step.listing.is_some())
    })
    .await;
}

/// Waits for the result of a listing, logout or selection.
pub(crate) async fn until_changed(tui: &mut Tui) {
    tui.until_login("accounts changed", |event| {
        matches!(event, LoginEvent::AccountsChanged { .. })
    })
    .await;
}

/// Makes openai the router's active provider through the model picker;
/// returns the chosen model.
pub(crate) async fn activate_openai(tui: &mut Tui) -> String {
    model_switch::open_selector(&mut tui.state, &*tui.live, &tui.app, Some("openai"));
    let Some(InputAction::SwitchModelProvider {
        provider_id,
        model_id,
    }) = tui.key(DomainKey::Enter)
    else {
        panic!("Enter on an openai model switches to it\n{}", tui.frame());
    };
    let domain_tx = tui.ctx.events.domain_tx.clone();
    let chosen = model_id.clone();
    model_switch::handle_apply_model_switch(
        &mut tui.state,
        &*tui.live,
        &StreamingState::default(),
        &domain_tx,
        &Conversation::default(),
        provider_id,
        model_id,
    )
    .await;
    assert_eq!(tui.live.active_delegate_id(), "openai");
    tui.drain();
    chosen
}

/// One turn of `model` through the live router; returns the bearer the stub saw.
pub(crate) async fn turn_bearer(tui: &Tui, listener: &TcpListener, model: &str) -> String {
    let router = tui.live.router().clone();
    let model = model.to_owned();
    let turn = tokio::spawn(async move {
        let stream = router
            .stream_completion(vec![user("hello")], options(&model, 64, None))
            .await
            .unwrap_or_else(|error| panic!("{error}"));
        drain(stream).await
    });
    let chat = accept(listener).await;
    assert_eq!(chat.path(), "/openai/v1/chat/completions");
    let seen = bearer(&chat).expect("a bearer");
    reply_sse_text(chat, "pong").await;
    assert_eq!(text_of(&turn.await.unwrap()), "pong");
    seen
}

/// No key byte on any frame, drained event or feedback text.
pub(crate) fn assert_no_keys(tui: &Tui, keys: &[&str]) {
    for key in keys {
        for frame in &tui.frames {
            assert!(!frame.contains(key), "a frame shows a key");
        }
        for event in &tui.event_log {
            assert!(!event.contains(key), "an event carries a key: {event}");
        }
        assert!(!tui.feedback().contains(key));
    }
}

/// A 60×16 frame keeps the modal's four corners, `header`, at least one row
/// and `footer`.
pub(crate) fn assert_fitted(tui: &mut Tui, header: &str, row: &str, footer: &str) {
    let frame = tui.frame_at(60, 16);
    for corner in ['┌', '┐', '└', '┘'] {
        assert!(frame.contains(corner), "corner {corner}\n{frame}");
    }
    for part in [header, row, footer] {
        assert!(frame.contains(part), "{part:?}\n{frame}");
    }
}

/// The account-step frame row for credential `id` (selected or not).
pub(crate) fn account_line(frame: &str, id: u64) -> String {
    frame
        .lines()
        .find(|line| line.contains(&format!("#{id}  API key #{id}")))
        .unwrap_or_else(|| panic!("row #{id}\n{frame}"))
        .to_owned()
}
