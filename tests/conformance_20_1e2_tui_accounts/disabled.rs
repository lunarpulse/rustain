//! K5 (AC5, `DF-20-1D5A-ACCOUNTS-DISABLED-ENTRY`): stored credentials no
//! enabled entry reads are reported unread on every surface, stay removable
//! and selectable, and are left out of the sign-in lists.

use super::common::{
    K1, Tui, World, config_with, key_login, openai, row_ids, stub, until_accounts, until_changed,
};
use rustain::adapters::auth::host;
use rustain::adapters::auth_store::FileAuthStore;
use rustain::domain::events::{DomainKey, LoginEvent};
use rustain::domain::models::{AppConfig, CredentialId};
use rustain::domain::ports::AuthStorePort;
use rustain::infrastructure::auth_accounts::{ServedSource, Unread, accounts};
use serial_test::serial;
use std::sync::Arc;
use wiremock::MockServer;

fn store() -> Arc<dyn AuthStorePort> {
    Arc::new(FileAuthStore::new())
}

/// K5 (M4, M5).
#[tokio::test]
#[serial]
async fn k5_disabled_configuration_leaves_stored_rows_unread() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    let (listener, base) = stub().await;
    let enabled = config_with(&[("openai", openai(&base, true))]);
    let mut tui = Tui::new(enabled.clone());
    key_login(&mut tui, &listener, K1).await;
    let [id] = row_ids().await[..] else {
        panic!("one row");
    };
    // Positive control: the enabled entry serves the login row.
    assert_eq!(
        accounts(&store(), &enabled, "openai").await.unwrap().served,
        Some(ServedSource::Row(CredentialId(id)))
    );

    // The same rows under `enabled = false`, with OPENAI_API_KEY set.
    world.set("OPENAI_API_KEY", "sk-env-openai-K5");
    let disabled = config_with(&[("openai", openai(&base, false))]);
    let that = accounts(&store(), &disabled, "openai").await.unwrap();
    assert_eq!(
        (that.served.clone(), that.unread),
        (None, Some(Unread::Disabled))
    );
    assert_eq!(
        host::account_header(&that),
        "OpenAI (openai): disabled in configuration"
    );
    let mut tui = Tui::new(disabled);

    // `/logout` lists it as disabled.
    tui.submit("/logout");
    tui.until("provider rows", |tui| !tui.state.login.loading)
        .await;
    let frame = tui.frame();
    assert!(
        frame.contains("OpenAI · disabled in configuration"),
        "{frame}"
    );
    tui.key(DomainKey::Esc);

    // `/login` omits it; `/login openai` still signs it in.
    tui.submit("/login");
    tui.until("login rows", |tui| !tui.state.login.loading)
        .await;
    assert!(
        !tui.state
            .login
            .rows
            .iter()
            .any(|row| row.row.id == "openai"),
        "the sign-in picker omits a provider nothing reads"
    );
    assert!(!tui.state.login.rows.is_empty());
    tui.key(DomainKey::Esc);
    tui.submit("/login openai");
    tui.until("masked key field", Tui::field_pending).await;
    tui.key(DomainKey::Esc);
    tui.until_login("login cancelled", |event| {
        matches!(event, LoginEvent::Finished { .. })
    })
    .await;

    // `/accounts openai` still selects it, with the disabled note.
    tui.submit("/accounts openai");
    until_accounts(&mut tui).await;
    assert!(
        tui.frame()
            .contains("OpenAI (openai): disabled in configuration"),
        "{}",
        tui.frame()
    );
    tui.key(DomainKey::Enter);
    until_changed(&mut tui).await;
    assert_eq!(
        tui.state.feedback_blocks["accounts-result"].message,
        format!(
            "Active OpenAI account: API key #{id}.\nNote: OpenAI is disabled in configuration."
        )
    );

    // `/logout openai` still removes it.
    tui.submit("/logout openai");
    until_accounts(&mut tui).await;
    tui.key(DomainKey::Enter);
    until_changed(&mut tui).await;
    assert_eq!(
        tui.state.feedback_blocks["accounts-result"].message,
        format!("Logged out of OpenAI account API key #{id}.\nNo OpenAI credentials remain.")
    );
    assert!(row_ids().await.is_empty());
}

/// An enabled kind alias with its own rows shadows the built-in: its rows
/// are read by no enabled provider.
#[tokio::test]
#[serial]
async fn alias_shadowed_built_in_rows_have_no_reader() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let (listener, base) = stub().await;
    let enabled = config_with(&[("openai", openai(&base, true))]);
    let mut tui = Tui::new(enabled);
    key_login(&mut tui, &listener, K1).await;
    let [id] = row_ids().await[..] else {
        panic!("one row");
    };
    let mut work = openai(&base, true);
    work.provider_id = "work".into();
    let alias: AppConfig = config_with(&[("work", work)]);
    let that = accounts(&store(), &alias, "openai").await.unwrap();
    assert_eq!(
        (that.served.clone(), that.unread),
        (None, Some(Unread::NoReader))
    );
    assert!(
        !host::login_targets(&alias)
            .iter()
            .any(|target| target.id == "openai")
    );
    // A logout that leaves this row behind names the missing reader.
    assert_eq!(
        host::logout_lines(&that, CredentialId(id + 1), None)[1],
        "No enabled provider reads OpenAI credentials."
    );
}
