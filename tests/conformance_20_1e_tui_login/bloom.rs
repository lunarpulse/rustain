//! K01 (AC6/AC7): locked provider → sign in from the model picker → bloom in
//! the reopened picker → a turn reaches the provider with the new key; K02
//! (AC12) secret hygiene of that login; K09 (AC7) the live layer from a TUI
//! that started with zero discovery targets.

use super::common::{
    Held, Tui, World, accept, buffer_text, clipboard_file, config_with, listing, reply_sse_text,
    stubai,
};
use crate::conformance_20_1d3_copilot::common::{drain, options, text_of, user};
use rustain::adapters::tui::app::InputAction;
use rustain::adapters::tui::handlers::model_switch;
use rustain::domain::events::{DomainKey, LoginEvent};
use rustain::domain::models::{Conversation, StreamingState};
use rustain::domain::ports::{ProviderInfoPort, StreamingProvider};
use rustain::infrastructure::live_providers::LiveProviders;
use serial_test::serial;
use std::io::Write;
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;
use wiremock::MockServer;

const KEY: &str = "sk-stubai-K01-SECRET-0042";

pub(crate) async fn stub() -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    (listener, base)
}

fn bearer(held: &Held) -> Option<String> {
    held.header("authorization")
}

/// Picker Enter on the locked `stubai` column → masked field → pasted key →
/// validation at the stub → `Finished`. Returns with the picker reopened.
pub(crate) async fn picker_login(tui: &mut Tui, listener: &TcpListener, key: &str) {
    tui.dispatch(InputAction::OpenModelSelector);
    let column = &tui.state.model_selector.columns[tui.state.model_selector.selected_provider];
    assert_eq!(column.provider_id, "stubai");
    assert!(column.locked_hint.is_some(), "stubai starts locked");
    let frame = tui.frame();
    assert!(frame.contains("stubai · not signed in"), "{frame}");
    assert!(
        frame.contains("Enter sign in · ←→ provider · Esc close"),
        "{frame}"
    );
    tui.key(DomainKey::Enter);
    tui.until("masked key field", Tui::field_pending).await;
    tui.paste(&format!("{key}\n"));
    let frame = tui.frame();
    assert!(frame.contains("Enter stubai API key:"), "{frame}");
    assert!(
        frame.contains(&format!("> {}", "•".repeat(key.len()))),
        "{frame}"
    );
    tui.key(DomainKey::Enter);
    let validation = accept(listener).await;
    assert_eq!(validation.path(), "/stubai/v1/models");
    assert_eq!(bearer(&validation), Some(format!("Bearer {key}")));
    validation.json(&listing(&["stub-a"])).await;
    tui.until_login("login finished", |event| {
        matches!(event, LoginEvent::Finished { .. })
    })
    .await;
}

async fn k01_body(logs: Arc<Mutex<Vec<u8>>>) {
    let server = MockServer::start().await;
    let world = World::new(&server);
    let (listener, base) = stub().await;
    let mut tui = Tui::new(config_with(&[("stubai", stubai(&base))]));

    // Positive control: before the login the router names the locked hint.
    let router = tui.live.router().clone();
    match router
        .stream_completion(vec![user("hello")], options("stub-a", 64, None))
        .await
    {
        Err(error) => assert!(
            error
                .to_string()
                .contains("stubai has no credentials configured"),
            "{error}"
        ),
        Ok(_) => panic!("a locked provider cannot stream"),
    }

    picker_login(&mut tui, &listener, KEY).await;
    assert!(
        tui.feedback()
            .contains("✓ stubai credentials stored successfully.")
    );
    assert!(tui.feedback().contains("Credentials saved to "));

    // The reopened picker is on stubai, available, while discovery is held.
    let discovery = accept(&listener).await;
    assert_eq!(discovery.path(), "/stubai/v1/models");
    assert_eq!(bearer(&discovery), Some(format!("Bearer {KEY}")));
    tui.drain();
    let frame = tui.frame();
    assert!(frame.contains("◄ stubai ✓ connected"), "{frame}");
    assert!(
        frame.contains("↻ Discovering models for stubai..."),
        "{frame}"
    );
    assert!(!frame.contains("stub-c"), "{frame}");
    discovery
        .json(&listing(&["stub-a", "stub-b", "stub-c"]))
        .await;
    tui.until_login("discovery line", |event| {
        matches!(event, LoginEvent::Discovered { .. })
    })
    .await;
    let frame = tui.frame();
    for id in [
        "stub-a",
        "stub-b",
        "stub-c",
        "Discovered 3 models for stubai.",
    ] {
        assert!(frame.contains(id), "{id}\n{frame}");
    }
    assert!(!frame.contains("↻ Discovering"), "{frame}");
    assert!(tui.live.registry().provider_ids().contains("stubai"));
    assert!(tui.live.router().get_provider("stubai").is_some());
    assert!(
        !tui.live
            .list_locked_providers()
            .iter()
            .any(|locked| locked.provider_id == "stubai")
    );

    // Select stub-b through the existing switch handler, then send a turn.
    for _ in 0..8 {
        if tui
            .state
            .model_selector
            .selected()
            .is_some_and(|(_, model)| model.model_id == "stub-b")
        {
            break;
        }
        tui.key(DomainKey::Down);
    }
    let Some(InputAction::SwitchModelProvider {
        provider_id,
        model_id,
    }) = tui.key(DomainKey::Enter)
    else {
        panic!("Enter on an available model switches to it");
    };
    assert_eq!(model_id, "stub-b");
    let domain_tx = tui.ctx.events.domain_tx.clone();
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
    assert_eq!(tui.live.active_delegate_id(), "stubai");
    let turn = tokio::spawn(async move {
        let stream = router
            .stream_completion(vec![user("hello")], options("stub-b", 64, None))
            .await
            .unwrap_or_else(|error| panic!("{error}"));
        drain(stream).await
    });
    let chat = accept(&listener).await;
    assert_eq!(chat.path(), "/stubai/v1/chat/completions");
    assert_eq!(bearer(&chat), Some(format!("Bearer {KEY}")));
    reply_sse_text(chat, "pong").await;
    assert_eq!(text_of(&turn.await.unwrap()), "pong");

    // K02: the key never reached a frame, an event, a notice, the clipboard
    // or the log; the field showed only bullets.
    tui.drain();
    for frame in &tui.frames {
        assert!(!frame.contains(KEY), "a frame shows the key");
    }
    for event in &tui.event_log {
        assert!(!event.contains(KEY), "an event carries the key: {event}");
    }
    assert!(!tui.feedback().contains(KEY) && !tui.flash().contains(KEY));
    assert!(tui.copied.lock().unwrap().iter().all(|c| !c.contains(KEY)));
    assert!(!clipboard_file(&world).contains(KEY));
    assert!(!tui.state.input_buffer.contains(KEY));
    let _ = (&logs, buffer_text);
}

#[derive(Clone)]
struct Capture(Arc<Mutex<Vec<u8>>>);
impl Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// K01 (M01) + K02 (M02): a locked configured provider is signed into from
/// its picker column and blooms in place; the key reaches only the stub.
#[test]
#[serial]
fn k01_k02_locked_provider_signs_in_from_the_picker_blooms_and_serves_a_turn() {
    let logs = Arc::new(Mutex::new(Vec::new()));
    let writer = Capture(logs.clone());
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(k01_body(logs.clone()));
    });
    let captured = String::from_utf8_lossy(&logs.lock().unwrap()).into_owned();
    assert!(
        captured.contains("router registered: stubai"),
        "the TRACE capture saw the unlock"
    );
    assert!(!captured.contains(KEY), "a log line carries the key");
}

/// K09 (M09): a TUI that started with zero discovery targets refreshes a
/// provider unlocked in-process on the periodic tick and keeps it unlocked
/// across a models.dev remerge.
#[tokio::test]
#[serial]
async fn k09_signed_in_provider_joins_the_periodic_refresh_and_survives_remerge() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let (listener, base) = stub().await;
    let mut tui = Tui::new(config_with(&[("stubai", stubai(&base))]));
    assert!(
        tui.live.discovery_targets().is_empty(),
        "zero startup targets"
    );
    picker_login(&mut tui, &listener, KEY).await;
    accept(&listener)
        .await
        .json(&listing(&["stub-a", "stub-b", "stub-c"]))
        .await;
    tui.until_login("discovery line", |event| {
        matches!(event, LoginEvent::Discovered { .. })
    })
    .await;

    // (a) The function the 4 h loop calls discovers stubai once more.
    let live = tui.live.clone();
    let tick = tokio::spawn(async move { live.periodic_tick().await });
    let again = accept(&listener).await;
    assert_eq!(again.path(), "/stubai/v1/models");
    again
        .json(&listing(&["stub-a", "stub-b", "stub-c", "stub-d"]))
        .await;
    tick.await.unwrap();
    assert!(
        tui.live
            .list_models_by_provider("stubai")
            .iter()
            .any(|model| model.model_id == "stub-d")
    );

    // (b) A models.dev remerge republishes stubai's live catalogue (the
    // current target set, not the startup one) and keeps it unlocked.
    tui.drain();
    let seen = tui.event_log.len();
    tui.live.remerge().await;
    tui.drain();
    assert!(
        tui.event_log[seen..]
            .iter()
            .any(|event| event == "ProviderCatalogRefreshed { provider_id: \"stubai\" }"),
        "the remerge republished stubai: {:?}",
        &tui.event_log[seen..]
    );
    assert!(
        !tui.live
            .list_locked_providers()
            .iter()
            .any(|locked| locked.provider_id == "stubai")
    );
    assert!(
        tui.live
            .list_models_by_provider("stubai")
            .iter()
            .any(|model| model.model_id == "stub-d")
    );
}

/// The periodic refresh task starts (and stays running) with no targets.
#[tokio::test]
#[serial]
async fn periodic_refresh_runs_with_zero_startup_targets() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let tui = Tui::new(config_with(&[]));
    let live: Arc<LiveProviders> = tui.live.clone();
    assert!(live.discovery_targets().is_empty());
    let handle = rustain::infrastructure::startup::spawn_periodic_catalog_refresh(live);
    tokio::task::yield_now().await;
    assert!(
        !handle.is_finished(),
        "no early return on an empty target set"
    );
    handle.abort();
}
