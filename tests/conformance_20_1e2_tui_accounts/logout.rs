//! K1 (AC1/AC10) `/logout` of the served account re-serves the other one
//! live; K3 (AC3/AC10) removing the last account relocks the provider live;
//! K4 (AC4) a row removed underneath the step keeps it open with fresh rows.

use super::common::{
    K1, K2, Tui, World, account_line, activate_openai, answer_models, assert_fitted,
    assert_no_keys, config_with, key_login, openai, row_ids, stub, turn_bearer, until_accounts,
    until_changed,
};
use rustain::adapters::auth_store::FileAuthStore;
use rustain::adapters::tui::state::LoginView;
use rustain::domain::events::DomainKey;
use rustain::domain::models::CredentialId;
use rustain::domain::models::FocusState;
use rustain::domain::models::visual::OverlayType;
use rustain::domain::ports::{AuthStorePort, ProviderInfoPort, StreamingProvider};
use serial_test::serial;
use std::sync::Arc;
use wiremock::MockServer;

/// K1 (M1) + K3 (M2): two TUI logins, `/logout` of the active account, then
/// of the last one.
#[tokio::test]
#[serial]
async fn k1_k3_logout_reserves_then_relocks_the_provider_live() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let (listener, base) = stub().await;
    let mut tui = Tui::new(config_with(&[("openai", openai(&base, true))]));
    key_login(&mut tui, &listener, K1).await;
    key_login(&mut tui, &listener, K2).await;
    let [id1, id2] = row_ids().await[..] else {
        panic!("two rows");
    };
    let model = activate_openai(&mut tui).await;
    // Positive control: the second login is the active, served account.
    assert_eq!(
        turn_bearer(&tui, &listener, &model).await,
        format!("Bearer {K2}")
    );

    // The provider step lists every provider with stored rows.
    tui.submit("/logout");
    tui.until("provider rows", |tui| !tui.state.login.loading)
        .await;
    let frame = tui.frame();
    assert!(frame.contains(" Log out "), "{frame}");
    assert!(frame.contains("OpenAI · login · 2 accounts"), "{frame}");
    assert!(
        frame.contains("↑↓ navigate · Enter choose · Esc close"),
        "{frame}"
    );
    assert!(
        !frame.contains("logged in"),
        "no validation marker\n{frame}"
    );
    tui.key(DomainKey::Esc);
    assert_eq!(tui.state.focus, FocusState::Input);

    // `/logout openai`: served row first, cursor on it, no confirmation.
    tui.submit("/logout openai");
    until_accounts(&mut tui).await;
    let frame = tui.frame();
    assert!(frame.contains(" Log out of OpenAI "), "{frame}");
    assert!(
        frame.contains(&format!("OpenAI (openai): serving API key #{id2}")),
        "{frame}"
    );
    let first = frame
        .find(&format!("#{id2}  API key #{id2}"))
        .expect("served row");
    let second = frame
        .find(&format!("#{id1}  API key #{id1}"))
        .expect("other row");
    assert!(first < second, "served row first\n{frame}");
    assert!(
        account_line(&frame, id2).contains(&format!("▸ #{id2}  API key #{id2}  api key  active")),
        "{frame}"
    );
    assert!(
        account_line(&frame, id1)
            .trim_end_matches(['│', ' '])
            .ends_with(&format!("#{id1}  API key #{id1}  api key")),
        "{frame}"
    );
    assert!(
        frame.contains("↑↓ navigate · Enter log out · Esc close"),
        "{frame}"
    );
    tui.key(DomainKey::Enter);
    assert!(tui.frame().contains("Working…"), "busy footer");
    until_changed(&mut tui).await;
    assert_eq!(tui.state.focus, FocusState::Input, "the overlay closed");
    assert_eq!(
        tui.state.feedback_blocks["accounts-result"].message,
        format!("Logged out of OpenAI account API key #{id2}.\nOpenAI now uses API key #{id1}.")
    );
    // Phase 2 rediscovered the account that now serves.
    assert_eq!(answer_models(&listener).await, K1);
    assert_eq!(
        turn_bearer(&tui, &listener, &model).await,
        format!("Bearer {K1}"),
        "the next request carries K1 without a restart"
    );

    // K3: the last account goes; openai relocks in place.
    tui.submit("/logout openai");
    until_accounts(&mut tui).await;
    tui.key(DomainKey::Enter);
    until_changed(&mut tui).await;
    tui.drain();
    assert_eq!(
        tui.state.feedback_blocks["accounts-result"].message,
        format!("Logged out of OpenAI account API key #{id1}.\nNo OpenAI credentials remain.")
    );
    assert!(
        tui.live
            .registry()
            .list_locked()
            .iter()
            .any(|locked| locked.provider_id == "openai" && !locked.sign_in_hint.is_empty())
    );
    match tui
        .live
        .router()
        .stream_completion(
            vec![crate::conformance_20_1d3_copilot::common::user("hello")],
            crate::conformance_20_1d3_copilot::common::options(&model, 64, None),
        )
        .await
    {
        Err(error) => assert!(
            error
                .to_string()
                .contains("OpenAI has no credentials configured"),
            "{error}"
        ),
        Ok(_) => panic!("a relocked provider cannot stream"),
    }
    assert!(
        !tui.live
            .discovery_targets()
            .iter()
            .any(|target| target.provider_id == "openai")
    );
    assert!(
        tui.event_log
            .iter()
            .any(|event| event.contains("OpenAI is now locked — choose another model (Ctrl+X, M).")),
        "the locked warning was posted"
    );
    tui.submit("/models");
    let column = tui
        .state
        .model_selector
        .columns
        .iter()
        .find(|column| column.provider_id == "openai")
        .expect("openai column");
    assert!(
        column.locked_hint.is_some(),
        "the picker shows openai locked"
    );
    assert!(
        tui.live
            .list_locked_providers()
            .iter()
            .any(|locked| locked.provider_id == "openai")
    );
    let _ = tui.frame();
    assert_no_keys(&tui, &[K1, K2]);
}

/// One real login, active model selection and last-row logout, with a
/// successful inference and unlocked frame as positive controls.
async fn last_logout() -> (Tui, String) {
    let (listener, base) = stub().await;
    let mut tui = Tui::new(config_with(&[("openai", openai(&base, true))]));
    key_login(&mut tui, &listener, K1).await;
    let model = activate_openai(&mut tui).await;
    assert_eq!(
        turn_bearer(&tui, &listener, &model).await,
        format!("Bearer {K1}")
    );
    tui.submit("/models");
    let before = tui.frame();
    assert!(
        before.contains("OpenAI") && before.contains(&model),
        "{before}"
    );
    assert!(!before.contains("not signed in"), "{before}");
    tui.key(DomainKey::Esc);
    tui.submit("/logout openai");
    until_accounts(&mut tui).await;
    tui.key(DomainKey::Enter);
    until_changed(&mut tui).await;
    (tui, model)
}

#[tokio::test]
#[serial]
async fn last_logout_preserves_the_complete_sign_in_hint() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let (tui, model) = last_logout().await;
    let locked = tui.live.registry().list_locked();
    let openai = locked
        .iter()
        .find(|entry| entry.provider_id == "openai")
        .unwrap();
    assert_eq!(
        openai.sign_in_hint,
        "Set OPENAI_API_KEY or run `rustain auth login openai`"
    );
    match tui.live.router().stream_completion(
        vec![crate::conformance_20_1d3_copilot::common::user("hello")],
        crate::conformance_20_1d3_copilot::common::options(&model, 64, None),
    ).await {
        Err(error) => assert!(error.to_string().contains(
            "OpenAI has no credentials configured. Set OPENAI_API_KEY or run `rustain auth login openai`"
        ), "{error}"),
        Ok(_) => panic!("the relocked router cannot infer"),
    }
    assert_no_keys(&tui, &[K1]);
}

#[tokio::test]
#[serial]
async fn last_logout_renders_the_locked_openai_column() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let (mut tui, _) = last_logout().await;
    tui.submit("/models");
    for _ in 0..tui.state.model_selector.columns.len() {
        if tui.state.model_selector.columns[tui.state.model_selector.selected_provider].provider_id
            == "openai"
        {
            break;
        }
        tui.key(DomainKey::Right);
    }
    assert_eq!(
        tui.state.model_selector.columns[tui.state.model_selector.selected_provider].provider_id,
        "openai"
    );
    let frame = tui.frame();
    for text in [
        "OpenAI",
        "not signed in",
        "OpenAI has no credentials configured",
        "Enter sign in",
    ] {
        assert!(frame.contains(text), "{text:?}\n{frame}");
    }
    // Remove only frame border/padding; preserve every word across wrapping.
    let joined = frame
        .lines()
        .map(|line| line.trim_matches([' ', '│']))
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        joined.contains("Set OPENAI_API_KEY or run `rustain auth login openai`"),
        "{frame}"
    );
    assert_no_keys(&tui, &[K1]);
}

/// K4 (M10): a row removed underneath the open step (as a concurrent
/// `rustain auth logout --id` does) leaves the step open with fresh rows and
/// a notice; the positive control removes and closes. The step fits 60×16.
#[tokio::test]
#[serial]
async fn k4_stale_row_keeps_the_step_open_with_fresh_rows() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let (listener, base) = stub().await;
    let mut tui = Tui::new(config_with(&[("openai", openai(&base, true))]));
    key_login(&mut tui, &listener, K1).await;
    key_login(&mut tui, &listener, K2).await;
    let [id1, id2] = row_ids().await[..] else {
        panic!("two rows");
    };

    tui.submit("/logout openai");
    until_accounts(&mut tui).await;
    assert_fitted(
        &mut tui,
        "OpenAI (openai): serving",
        &format!("#{id2}  API key #{id2}"),
        "Esc close",
    );
    let other: Arc<dyn AuthStorePort> = Arc::new(FileAuthStore::new());
    other.remove_row("openai", CredentialId(id2)).await.unwrap();
    tui.key(DomainKey::Enter);
    until_changed(&mut tui).await;
    assert_eq!(
        tui.state.login.view,
        LoginView::AccountPicker,
        "the step stays open"
    );
    assert!(tui.state.login.active, "the overlay stays open");
    assert_eq!(tui.state.focus, FocusState::Overlay(OverlayType::Login));
    let frame = tui.frame();
    assert!(
        frame.contains(&format!("Credential #{id2} for OpenAI no longer exists.")),
        "{frame}"
    );
    assert!(
        !frame.contains(&format!("#{id2}  API key #{id2}")),
        "{frame}"
    );
    assert!(
        frame.contains(&format!("#{id1}  API key #{id1}")),
        "{frame}"
    );
    assert!(!tui.state.feedback_blocks.contains_key("accounts-result"));

    // Positive control: the remaining row is removed and the step closes.
    tui.key(DomainKey::Enter);
    until_changed(&mut tui).await;
    assert_eq!(tui.state.focus, FocusState::Input);
    assert_eq!(
        tui.state.feedback_blocks["accounts-result"].message,
        format!("Logged out of OpenAI account API key #{id1}.\nNo OpenAI credentials remain.")
    );
    assert!(row_ids().await.is_empty());
}
