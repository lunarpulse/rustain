//! K-H1: both real provider steps retain the account service's ordering.
use super::common::{Tui, World, config_with, openai, stub};
use rustain::adapters::auth_store::FileAuthStore;
use rustain::domain::events::DomainKey;
use rustain::domain::models::{Credential, CredentialOrigin};
use rustain::domain::ports::AuthStorePort;
use serial_test::serial;
use wiremock::MockServer;

#[tokio::test]
#[serial]
async fn provider_steps_preserve_stored_account_order() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let (_listener, base) = stub().await;
    let mut app = config_with(&[]);
    let store = FileAuthStore::new();
    for (id, kind) in [
        ("z-google", "google"),
        ("google", "google"),
        ("z-claude", "anthropic"),
        ("openrouter", "openrouter"),
        ("a-google", "google"),
        ("openai", "openai"),
    ] {
        let mut cfg = openai(&base, id != "google");
        cfg.provider_id = id.into();
        cfg.kind = Some(kind.into());
        app.provider.insert(id.into(), cfg);
        store
            .upsert(
                id,
                Credential::new_api_key("ordering-fixture".into()),
                CredentialOrigin::Stored,
                false,
            )
            .await
            .unwrap();
    }
    let mut tui = Tui::new(app);
    for command in ["/logout", "/accounts"] {
        tui.submit(command);
        tui.until("provider rows", |tui| !tui.state.login.loading)
            .await;
        assert_eq!(
            tui.state
                .login
                .rows
                .iter()
                .map(|row| row.row.id.as_str())
                .collect::<Vec<_>>(),
            [
                "openai",
                "openrouter",
                "google",
                "z-claude",
                "a-google",
                "z-google"
            ]
        );
        let frame = tui.frame();
        let visible: Vec<_> = frame
            .lines()
            .filter(|line| line.contains(" · ") && !line.contains("navigate"))
            .collect();
        assert_eq!(visible.len(), 6, "{frame}");
        for (line, label) in visible.iter().zip([
            "OpenAI",
            "OpenRouter",
            "Google AI",
            "Anthropic",
            "Google AI",
            "Google AI",
        ]) {
            assert!(line.contains(label), "{frame}");
        }
        assert!(visible[2].contains("disabled in configuration"), "{frame}");
        tui.key(DomainKey::Esc);
    }
}
