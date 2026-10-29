//! K6 (AC6) the four command forms route through `submit_message` and open
//! their steps; AC4's texts, keys and errors.

use super::common::{
    K1, K2, Tui, World, account_line, config_with, key_login, openai, row_ids, stub,
    until_accounts, until_changed,
};
use rustain::adapters::auth_store::FileAuthStore;
use rustain::adapters::command_registry::CommandRegistry;
use rustain::adapters::tui::app::InputAction;
use rustain::adapters::tui::state::{LoginView, OverlayMode};
use rustain::domain::events::DomainKey;
use rustain::domain::models::visual::OverlayType;
use rustain::domain::models::{AppConfig, FeedbackLevel, FocusState};
use rustain::domain::ports::AuthStorePort;
use serial_test::serial;
use std::sync::Arc;
use wiremock::MockServer;

fn assert_routed(action: &InputAction, name: &str, args: Option<&str>) {
    match action {
        InputAction::ExecuteCommand {
            name: routed,
            args: routed_args,
        } => {
            assert_eq!(routed, name);
            assert_eq!(routed_args.as_deref(), args);
        }
        other => panic!("`/{name}` must reach its ExecuteCommand arm, got {other:?}"),
    }
}

const LOGIN: FocusState = FocusState::Overlay(OverlayType::Login);

fn block(tui: &Tui) -> (FeedbackLevel, String) {
    let block = &tui.state.feedback_blocks["accounts-result"];
    (block.level, block.message.clone())
}

/// K6 (M6): every form routes, opens its step, and with nothing stored ends
/// in its text; autocomplete lists both commands.
#[tokio::test]
#[serial]
async fn k6_logout_and_accounts_route_open_and_report_empty_stores() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let mut tui = Tui::new(AppConfig::default());

    for (line, name, args, mode, title, text) in [
        (
            "/logout",
            "logout",
            None,
            OverlayMode::Logout,
            " Log out ",
            "No stored provider credentials to log out. Remove env or config auth at its source.",
        ),
        (
            "/accounts",
            "accounts",
            None,
            OverlayMode::Accounts,
            " Accounts ",
            "No stored provider credentials. Sign in with /login.",
        ),
        (
            "/logout openai",
            "logout",
            Some("openai"),
            OverlayMode::Logout,
            " Log out ",
            "No stored credentials for OpenAI. Remove env or config auth at its source.",
        ),
        (
            "/accounts openai",
            "accounts",
            Some("openai"),
            OverlayMode::Accounts,
            " Accounts ",
            "No stored credentials for OpenAI. Sign in with /login openai.",
        ),
    ] {
        let action = tui.submit(line);
        assert_routed(&action, name, args);
        assert_eq!(tui.state.focus, LOGIN, "{line}");
        assert_eq!(tui.state.login.mode, mode, "{line}");
        let view = if args.is_some() {
            LoginView::AccountPicker
        } else {
            LoginView::ProviderPicker
        };
        assert_eq!(tui.state.login.view, view, "{line}");
        let frame = tui.frame();
        assert!(frame.contains(title), "{line}\n{frame}");
        tui.until(line, |tui| !tui.state.login.active).await;
        assert_eq!(tui.state.focus, FocusState::Input, "{line}");
        assert_eq!(
            block(&tui),
            (FeedbackLevel::Info, text.to_owned()),
            "{line}"
        );
    }

    tui.submit("/logout nosuch");
    tui.until("unknown", |tui| !tui.state.login.active).await;
    assert_eq!(
        block(&tui),
        (
            FeedbackLevel::Warning,
            "Unknown provider: nosuch".to_owned()
        )
    );

    let registry = CommandRegistry::new();
    for (typed, name) in [("logo", "logout"), ("acc", "accounts")] {
        assert!(
            registry
                .filter(typed)
                .iter()
                .any(|command| command.name == name),
            "/{name} completes from `/{typed}`"
        );
    }
}

/// AC4: Esc goes back to the provider step (query and selection kept), then
/// clears the query, then closes; a busy step ignores every key.
#[tokio::test]
#[serial]
async fn account_step_escape_rules_and_busy_keys() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let (listener, base) = stub().await;
    let mut tui = Tui::new(config_with(&[("openai", openai(&base, true))]));
    key_login(&mut tui, &listener, K1).await;
    key_login(&mut tui, &listener, K2).await;

    tui.submit("/accounts");
    tui.until("provider rows", |tui| !tui.state.login.loading)
        .await;
    tui.type_text("open");
    let frame = tui.frame();
    assert!(
        frame.contains("↑↓ navigate · Enter choose · Esc clear"),
        "{frame}"
    );
    tui.key(DomainKey::Enter);
    until_accounts(&mut tui).await;
    let frame = tui.frame();
    assert!(
        frame.contains("↑↓ navigate · Enter make active · Esc back"),
        "{frame}"
    );
    tui.key(DomainKey::Esc);
    assert_eq!(tui.state.login.view, LoginView::ProviderPicker);
    assert_eq!(tui.state.login.query, "open", "query kept");
    assert_eq!(
        tui.state
            .login
            .selected_row()
            .map(|row| row.row.id.as_str()),
        Some("openai")
    );
    tui.key(DomainKey::Esc);
    assert!(tui.state.login.query.is_empty(), "Esc clears the query");
    assert!(tui.state.login.active);
    tui.key(DomainKey::Esc);
    assert_eq!(tui.state.focus, FocusState::Input, "Esc closes");

    // Busy: keys are ignored until the result arrives.
    tui.submit("/accounts openai");
    until_accounts(&mut tui).await;
    tui.key(DomainKey::Down);
    tui.key(DomainKey::Enter);
    let step = tui.state.login.account.clone().expect("step");
    assert!(step.busy);
    for key in [
        DomainKey::Esc,
        DomainKey::Up,
        DomainKey::Enter,
        DomainKey::CtrlC,
    ] {
        tui.key(key);
    }
    assert_eq!(tui.state.login.account, Some(step), "every key was ignored");
    assert_eq!(tui.state.login.view, LoginView::AccountPicker);
    assert!(tui.frame().contains("Working…"));
    until_changed(&mut tui).await;
    assert_eq!(tui.state.focus, FocusState::Input);
    assert!(
        block(&tui)
            .1
            .starts_with("Active OpenAI account: API key #")
    );
}

/// AC4: more than ten accounts scroll in a ten-row window by PgDn/PgUp.
#[tokio::test]
#[serial]
async fn account_step_scrolls_a_ten_row_window() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let (listener, base) = stub().await;
    let mut tui = Tui::new(config_with(&[("openai", openai(&base, true))]));
    for n in 0..12 {
        key_login(&mut tui, &listener, &format!("sk-openai-scroll-{n:02}")).await;
    }
    tui.submit("/logout openai");
    until_accounts(&mut tui).await;
    let rows = tui
        .state
        .login
        .account
        .as_ref()
        .unwrap()
        .listing
        .as_ref()
        .unwrap()
        .rows
        .clone();
    assert_eq!(rows.len(), 12);
    let visible = |frame: &str| {
        rows.iter()
            .filter(|row| frame.contains(&format!("#{}  API key #{}", row.id, row.id)))
            .count()
    };
    let frame = tui.frame();
    assert_eq!(visible(&frame), 10, "{frame}");
    assert!(account_line(&frame, rows[0].id).contains('▸'), "{frame}");
    tui.key(DomainKey::PageDown);
    let frame = tui.frame();
    assert!(account_line(&frame, rows[10].id).contains('▸'), "{frame}");
    assert!(!frame.contains(&format!("#{}  API key #{}", rows[0].id, rows[0].id)));
    tui.key(DomainKey::PageUp);
    let frame = tui.frame();
    assert!(account_line(&frame, rows[0].id).contains('▸'), "{frame}");
    tui.key(DomainKey::Esc);
}

/// AC4: a disabled row cannot be made active (notice, step open); a store
/// failure closes the overlay with an error block, and the provider step
/// reports an unreadable store as such, never as an empty one.
#[tokio::test]
#[serial]
async fn disabled_row_notice_and_store_failure() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    let (listener, base) = stub().await;
    let mut tui = Tui::new(config_with(&[("openai", openai(&base, true))]));
    key_login(&mut tui, &listener, K1).await;
    key_login(&mut tui, &listener, K2).await;
    let [id1, _] = row_ids().await[..] else {
        panic!("two rows");
    };
    // The store's own revocation path (a rejected refresh uses it).
    let store: Arc<dyn AuthStorePort> = Arc::new(FileAuthStore::new());
    let stamp = store.snapshot().await.unwrap().rows("openai")[0].stamp();
    store
        .disable_if_matches("openai", stamp, "revoked")
        .await
        .unwrap();

    tui.submit("/accounts openai");
    until_accounts(&mut tui).await;
    let frame = tui.frame();
    assert!(
        account_line(&frame, id1).contains(&format!(
            "#{id1}  API key #{id1}  api key  disabled: revoked"
        )),
        "{frame}"
    );
    tui.key(DomainKey::Down);
    tui.key(DomainKey::Enter);
    until_changed(&mut tui).await;
    assert_eq!(tui.state.login.view, LoginView::AccountPicker);
    assert!(
        tui.frame().contains(&format!(
            "Credential #{id1} for OpenAI is disabled (revoked); log in again."
        )),
        "{}",
        tui.frame()
    );
    tui.key(DomainKey::Esc);

    tui.submit("/logout openai");
    until_accounts(&mut tui).await;
    std::fs::write(world.dir.path().join("auth.json"), b"not json").unwrap();
    tui.key(DomainKey::Enter);
    until_changed(&mut tui).await;
    assert_eq!(tui.state.focus, FocusState::Input);
    let (level, message) = block(&tui);
    assert_eq!(level, FeedbackLevel::Error);
    assert!(message.starts_with("Account update failed: "), "{message}");

    tui.submit("/logout");
    tui.until("unreadable store", |tui| !tui.state.login.active)
        .await;
    assert_eq!(tui.state.focus, FocusState::Input);
    let (level, message) = block(&tui);
    assert_eq!(level, FeedbackLevel::Error);
    assert!(
        message.starts_with("Could not read stored accounts: "),
        "{message}"
    );
}
