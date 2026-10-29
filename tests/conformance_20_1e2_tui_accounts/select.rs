//! K2 (AC2) `/accounts` makes another account active and the live layer
//! rediscovers with it; the environment note; and the replaced-target
//! refresh order (`DF-20-1E-REPLACED-TARGET-REFRESH-ORDER`).

use super::common::{
    K1, K2, Tui, World, accept, activate_openai, answer_models, bearer, config_with, key_login,
    listing, openai, row_ids, stub, turn_bearer, until_accounts, until_changed,
};
use rustain::adapters::auth_store::FileAuthStore;
use rustain::domain::events::{DomainKey, LoginEvent};
use rustain::domain::models::provider::DiscoveryStatus;
use rustain::domain::models::{Credential, CredentialOrigin};
use rustain::domain::ports::{AuthStorePort, ProviderInfoPort};
use serial_test::serial;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use wiremock::MockServer;

const REFRESHED: &str = "ProviderCatalogRefreshed { provider_id: \"openai\" }";

async fn refreshed_since(tui: &mut Tui, seen: usize) {
    tui.until("openai catalogue republished", |tui| {
        tui.event_log[seen..].iter().any(|event| event == REFRESHED)
    })
    .await;
}

/// K2 (M3): `/accounts openai` → Down → Enter makes the older account active;
/// it serves the next request and the live roster is rediscovered with it.
#[tokio::test]
#[serial]
async fn k2_accounts_select_serves_and_rediscovers_the_chosen_account() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let (listener, base) = stub().await;
    let app = config_with(&[("openai", openai(&base, true))]);
    let mut tui = Tui::new(app.clone());
    key_login(&mut tui, &listener, K1).await;
    key_login(&mut tui, &listener, K2).await;
    let [id1, id2] = row_ids().await[..] else {
        panic!("two rows");
    };
    let model = activate_openai(&mut tui).await;

    tui.submit("/accounts openai");
    until_accounts(&mut tui).await;
    let frame = tui.frame();
    assert!(frame.contains(" OpenAI accounts "), "{frame}");
    assert!(
        frame.contains("↑↓ navigate · Enter make active · Esc close"),
        "{frame}"
    );
    assert!(
        frame.contains(&format!("▸ #{id2}  API key #{id2}  api key  active")),
        "{frame}"
    );
    tui.key(DomainKey::Down);
    tui.drain();
    let seen = tui.event_log.len();
    tui.key(DomainKey::Enter);
    until_changed(&mut tui).await;
    let held = accept(&listener).await;
    assert_eq!(held.path(), "/openai/v1/models");
    assert_eq!(bearer(&held), Some(format!("Bearer {K1}")));
    assert!(
        !tui.live
            .list_models_by_provider("openai")
            .iter()
            .any(|entry| entry.model_id == "m-select-fresh")
    );
    // Phase 1 announces the credential replacement before AccountsChanged;
    // the held HTTP response must produce a second, newer publication.
    assert_eq!(
        tui.event_log[seen..]
            .iter()
            .filter(|event| *event == REFRESHED)
            .count(),
        1
    );
    let discovery_seen = tui.event_log.len();
    assert_eq!(
        tui.state.feedback_blocks["accounts-result"].message,
        format!("Active OpenAI account: API key #{id1}.")
    );
    let store: Arc<dyn AuthStorePort> = Arc::new(FileAuthStore::new());
    let after = rustain::infrastructure::auth_accounts::accounts(&store, &app, "openai")
        .await
        .unwrap();
    let chosen = after
        .rows
        .iter()
        .find(|row| row.id.0 == id1)
        .expect("row one");
    assert!(chosen.selected && chosen.served);

    // Phase 2 rediscovers with the chosen account; its roster reaches the picker.
    held.json(&listing(&["m-select-fresh"])).await;
    refreshed_since(&mut tui, discovery_seen).await;
    assert_eq!(
        tui.event_log[seen..]
            .iter()
            .filter(|event| *event == REFRESHED)
            .count(),
        2
    );
    assert_eq!(
        tui.live
            .list_models_by_provider("openai")
            .iter()
            .map(|entry| entry.model_id.as_str())
            .collect::<Vec<_>>(),
        ["m-select-fresh"]
    );
    tui.submit("/models");
    let frame = tui.frame();
    assert!(
        frame.contains("OpenAI") && frame.contains("m-select-fresh"),
        "{frame}"
    );
    assert!(
        !frame.contains("m-one") && !frame.contains("m-two"),
        "{frame}"
    );
    tui.key(DomainKey::Esc);
    assert_eq!(
        turn_bearer(&tui, &listener, &model).await,
        format!("Bearer {K1}")
    );
}

/// AC2: a stored (non-login) key the environment outranks is still made
/// active, and the block names what serves the provider now.
#[tokio::test]
#[serial]
async fn selected_row_outranked_by_the_environment_is_noted() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    let (_listener, base) = stub().await;
    world.set("OPENAI_API_KEY", "sk-env-openai-note");
    // Legacy stored keys: no login writes this origin. The second upsert
    // makes its row active, so the step's Enter changes the selection.
    let store: Arc<dyn AuthStorePort> = Arc::new(FileAuthStore::new());
    for key in ["sk-legacy-stored-note-1", "sk-legacy-stored-note-2"] {
        store
            .upsert(
                "openai",
                Credential::new_api_key(key.into()),
                CredentialOrigin::Stored,
                false,
            )
            .await
            .unwrap();
    }
    let [id1, id2] = row_ids().await[..] else {
        panic!("two rows");
    };
    let app = config_with(&[("openai", openai(&base, true))]);
    let mut tui = Tui::new(app.clone());
    tui.submit("/accounts openai");
    until_accounts(&mut tui).await;
    // Nothing stored is served (the environment is), so the cursor starts on
    // the first row by label: `#<id1>`, not the active `#<id2>`.
    tui.key(DomainKey::Enter);
    until_changed(&mut tui).await;
    assert_eq!(
        tui.state.feedback_blocks["accounts-result"].message,
        format!(
            "Active OpenAI account: API key #{id1}.\nNote: OpenAI is currently served by OPENAI_API_KEY."
        )
    );
    let after = rustain::infrastructure::auth_accounts::accounts(&store, &app, "openai")
        .await
        .unwrap();
    let selected: Vec<_> = after.rows.iter().filter(|row| row.selected).collect();
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].id.0, id1, "the selection moved off #{id2}");
    assert!(!selected[0].served, "the environment still serves");
}

/// `DF-20-1E-REPLACED-TARGET-REFRESH-ORDER` (M11): a refresh of a replaced
/// target still in flight cannot publish over the replacement's refresh.
#[tokio::test]
#[serial]
async fn a_replaced_targets_late_refresh_never_publishes() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let (listener, base) = stub().await;
    let mut tui = Tui::new(config_with(&[("openai", openai(&base, true))]));
    key_login(&mut tui, &listener, K1).await;

    // The second login's discovery is held in flight.
    tui.submit("/login openai");
    tui.until("masked key field", Tui::field_pending).await;
    tui.paste(K2);
    tui.key(DomainKey::Enter);
    assert_eq!(answer_models(&listener).await, K2);
    tui.until_login("login finished", |event| {
        matches!(event, LoginEvent::Finished { .. })
    })
    .await;
    let held = accept(&listener).await;
    assert_eq!(
        bearer(&held).as_deref(),
        Some(format!("Bearer {K2}").as_str())
    );

    // `/accounts` replaces the target; its refresh publishes first.
    tui.submit("/accounts openai");
    until_accounts(&mut tui).await;
    tui.key(DomainKey::Down);
    tui.key(DomainKey::Enter);
    until_changed(&mut tui).await;
    let seen = tui.event_log.len();
    assert_eq!(answer_models(&listener).await, K1);
    refreshed_since(&mut tui, seen).await;
    assert_eq!(
        tui.live.registry().discovery_status("openai"),
        Some(DiscoveryStatus::Ok)
    );

    // The old target's response arrives last, rejected.
    let seen = tui.event_log.len();
    let mut stream = held.stream;
    stream
        .write_all(b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
        .await
        .unwrap();
    stream.shutdown().await.ok();
    refreshed_since(&mut tui, seen).await;
    assert_eq!(
        tui.live.registry().discovery_status("openai"),
        Some(DiscoveryStatus::Ok),
        "the superseded refresh published its status"
    );
    assert!(
        !tui.event_log
            .iter()
            .any(|event| event.contains("rejected its credential")),
        "the superseded refresh announced a rejection"
    );
}
