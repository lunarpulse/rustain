//! A sign-in that fails inside the real flow: `run_login` turns the flow's error into the
//! two-line message the handler shows, so deleting the appended next step there turns this red.
//! The provider is the xAI device flow against the loopback stub, whose token endpoint refuses
//! the authorization; the failure travels `flows::login` -> `run_login` -> `LoginEvent::Finished`
//! -> `handlers::login::on_event` -> the notice store.

use super::common::{Tui, World};
use rustain::adapters::auth::flows::Sleeper;
use rustain::adapters::tui::notifications::{NoticeSeverity, NoticeSource};
use rustain::domain::events::LoginEvent;
use rustain::domain::models::AppConfig;
use serde_json::json;
use serial_test::serial;
use std::sync::Arc;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

fn instant() -> Sleeper {
    Arc::new(|_| Box::pin(async {}))
}

async fn mount(server: &MockServer, verb: &str, route: &str, reply: ResponseTemplate) {
    Mock::given(method(verb))
        .and(path(route))
        .respond_with(reply)
        .mount(server)
        .await;
}

#[tokio::test]
#[serial]
async fn a_refused_device_login_says_why_and_what_to_do_next() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    mount(
        &server,
        "POST",
        "/oauth2/device/code",
        ResponseTemplate::new(200).set_body_json(json!({
            "device_code": "DEVICE-CODE-1",
            "user_code": "WDJB-MJHT",
            "verification_uri": "https://accounts.x.ai/device",
            "verification_uri_complete": "https://accounts.x.ai/device?user_code=WDJB-MJHT",
            "interval": 5,
            "expires_in": 900,
        })),
    )
    .await;
    mount(
        &server,
        "GET",
        "/.well-known/openid-configuration",
        ResponseTemplate::new(200).set_body_json(
            json!({"issuer": "https://auth.x.ai", "token_endpoint": "https://auth.x.ai/oauth2/token"}),
        ),
    )
    .await;
    mount(
        &server,
        "POST",
        "/oauth2/token",
        ResponseTemplate::new(400).set_body_json(json!({"error": "access_denied"})),
    )
    .await;

    let mut tui = Tui::new(AppConfig::default());
    tui.ctx = tui.ctx.clone().with_sleeper(instant());
    tui.submit("/login xai-oauth");
    tui.until_login("login finished", |event| {
        matches!(event, LoginEvent::Finished { .. })
    })
    .await;

    let (severity, message) = tui
        .latest_notice(NoticeSource::Login)
        .expect("the failed sign-in raised a notice");
    assert_eq!(severity, NoticeSeverity::Error);
    let lines: Vec<&str> = message.lines().collect();
    assert_eq!(lines.len(), 2, "{message:?}");
    assert!(
        lines[0].starts_with("Login failed: ") && lines[0].len() > "Login failed: ".len(),
        "{message:?}"
    );
    assert!(
        lines[0].contains("device authorization was denied"),
        "the reason names what the provider refused: {message:?}"
    );
    assert_eq!(
        lines[1],
        "Run /login to try again, or check your provider settings."
    );
    assert_eq!(
        message,
        format!(
            "{}\nRun /login to try again, or check your provider settings.",
            lines[0]
        )
    );
}
