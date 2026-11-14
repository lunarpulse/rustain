//! K05 (AC2) device-code view, copy keys and hostile-text inertness (xAI,
//! production `XAI_DEVICE` rebased); K04 (AC4) Esc during a device poll;
//! K06 (AC3) the Copilot Enterprise `prompt_line` in a visible field.

use super::common::{Tui, World, assert_link_cells, decoded_text};
use crate::conformance_20_1d2_device_and_xai::device::xai_access;
use crate::conformance_20_1d3_copilot::common::{
    DEVICE_PATH as COPILOT_DEVICE_PATH, ENDPOINT_A, ENTERPRISE_CLIENT, PROMPT, PUBLIC_CLIENT,
    Recorder, SEED_IDS, device_form, mount_login, probe_reply,
};
use rustain::adapters::auth::flows::Sleeper;
use rustain::adapters::auth_store::FileAuthStore;
use rustain::adapters::github_copilot::{
    plan,
    policy::{self, PolicyReport},
};
use rustain::adapters::tui::notifications::{NoticeRecord, NoticeSeverity, NoticeSource};
use rustain::domain::events::{DomainInputEvent, DomainKey, LoginEvent};
use rustain::domain::models::{AppConfig, Credential};
use rustain::domain::ports::AuthStorePort;
use serde_json::{Value, json};
use serial_test::serial;
use std::sync::Arc;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const USER_CODE: &str = "WDJB-MJHT";
const VERIFY_COMPLETE: &str = "https://accounts.x.ai/device?user_code=WDJB-MJHT";

fn instant() -> Sleeper {
    Arc::new(|_| Box::pin(async {}))
}
/// A device wait the test never releases: the flow stays in its poll loop.
fn never() -> Sleeper {
    Arc::new(|_| Box::pin(std::future::pending()))
}

async fn mount(server: &MockServer, verb: &str, route: &str, reply: ResponseTemplate) {
    Mock::given(method(verb))
        .and(path(route))
        .respond_with(reply)
        .mount(server)
        .await;
}

async fn mount_xai(server: &MockServer, device: Value, token: ResponseTemplate) {
    mount(
        server,
        "POST",
        "/oauth2/device/code",
        ResponseTemplate::new(200).set_body_json(device),
    )
    .await;
    mount(
        server,
        "GET",
        "/.well-known/openid-configuration",
        ResponseTemplate::new(200).set_body_json(
            json!({"issuer": "https://auth.x.ai", "token_endpoint": "https://auth.x.ai/oauth2/token"}),
        ),
    )
    .await;
    mount(server, "POST", "/oauth2/token", token).await;
    mount(
        server,
        "GET",
        "/oauth2/userinfo",
        ResponseTemplate::new(200)
            .set_body_json(json!({"sub": "userinfo-sub", "email": "grok@example.test"})),
    )
    .await;
}

fn device_body(verify_complete: &str, user_code: &str) -> Value {
    json!({
        "device_code": "DEVICE-CODE-1",
        "user_code": user_code,
        "verification_uri": "https://accounts.x.ai/device",
        "verification_uri_complete": verify_complete,
        "interval": 5,
        "expires_in": 900,
    })
}

async fn polls(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.url.path() == "/oauth2/token")
        .count()
}

/// K05 (M05): the device view renders the code and the linked sign-in page;
/// `c`/`u` copy them; the opener got the verification URL once.
#[tokio::test]
#[serial]
async fn k05_device_code_view_renders_links_and_copies() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let access = xai_access("jwt-sub");
    mount_xai(
        &server,
        device_body(VERIFY_COMPLETE, USER_CODE),
        ResponseTemplate::new(200).set_body_json(json!({
            "access_token": access, "refresh_token": "XAI-REFRESH", "expires_in": 3600, "token_type": "Bearer",
        })),
    )
    .await;
    let mut tui = Tui::new(AppConfig::default());
    tui.ctx = tui.ctx.clone().with_sleeper(instant());
    tui.submit("/login xai-oauth");
    tui.until_login("device code", |event| {
        matches!(event, LoginEvent::DeviceCode { .. })
    })
    .await;
    let frame = tui.frame_at(80, 24);
    for text in [
        "Enter this code on the sign-in page to connect your account.",
        USER_CODE,
        &format!("Sign-in page: {VERIFY_COMPLETE}"),
        "c copy code · u copy link · Esc cancel",
    ] {
        assert!(frame.contains(text), "{text}\n{frame}");
    }
    // The 48-cell page fits one row at 80 columns: label and URL on one line.
    assert_eq!(
        assert_link_cells(&tui, 80, 24, &[("Sign-in page: ", VERIFY_COMPLETE)]),
        [1]
    );
    assert_eq!(tui.opened.lock().unwrap().as_slice(), [VERIFY_COMPLETE]);
    tui.input(DomainInputEvent::KeyPress('c'));
    tui.input(DomainInputEvent::KeyPress('u'));
    assert_eq!(
        tui.copied.lock().unwrap().as_slice(),
        [USER_CODE, VERIFY_COMPLETE]
    );
    tui.until_login("login finished", |event| {
        matches!(event, LoginEvent::Finished { .. })
    })
    .await;
    assert!(
        tui.feedback()
            .starts_with("Logged in to xAI Grok OAuth (SuperGrok or X Premium+)"),
        "{}",
        tui.feedback()
    );
    // K5: the plan notice belongs to Copilot logins only.
    for record in login_notices(&tui) {
        assert!(
            !record.message.contains(plan::AVAILABILITY_HINT),
            "{}",
            record.message
        );
        assert_ne!(
            record.severity,
            NoticeSeverity::Warning,
            "{}",
            record.message
        );
    }
}

/// K05 (M05) + K04 (M04): a hostile verification URL and user code render
/// sanitized, unlinked, unopened and uncopyable; Esc during the poll cancels
/// and no poll follows.
#[tokio::test]
#[serial]
async fn k05_k04_hostile_device_text_is_inert_and_esc_cancels_the_poll() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    mount_xai(
        &server,
        device_body("https://x.ai/\u{1b}]52;c;ZXZpbA==\u{7}", "AB\u{9b}CD"),
        ResponseTemplate::new(400).set_body_json(json!({"error": "authorization_pending"})),
    )
    .await;
    let mut tui = Tui::new(AppConfig::default());
    tui.ctx = tui.ctx.clone().with_sleeper(never());
    tui.submit("/login xai-oauth");
    tui.until_login("device code", |event| {
        matches!(event, LoginEvent::DeviceCode { .. })
    })
    .await;
    let frame = tui.frame_at(80, 24);
    assert!(
        !frame.contains(['\u{1b}', '\u{7}', '\u{9b}']),
        "no terminal control reaches the buffer\n{frame:?}"
    );
    // The whole escape sequence is removed (not just its control bytes, which
    // the cell buffer would drop anyway): no OSC 52 payload is drawn, and the
    // C1 introducer in the code becomes a space.
    assert!(
        !frame.contains("52;c;") && !frame.contains("ZXZpbA"),
        "{frame}"
    );
    assert!(frame.contains("AB CD"), "{frame}");
    assert!(frame.contains("Sign-in page: "), "{frame}");
    assert_link_cells(&tui, 80, 24, &[]);
    assert!(tui.opened.lock().unwrap().is_empty(), "nor opened");
    tui.input(DomainInputEvent::KeyPress('u'));
    assert_eq!(tui.flash(), "Nothing to copy");
    tui.input(DomainInputEvent::KeyPress('c'));
    assert_eq!(tui.flash(), "Nothing to copy");
    assert!(tui.copied.lock().unwrap().is_empty());
    // The flow waits between polls (its wait is never released). Esc cancels it.
    let before = polls(&server).await;
    tui.key(DomainKey::Esc);
    tui.until_login("cancelled", |event| {
        matches!(event, LoginEvent::Finished { .. })
    })
    .await;
    assert_eq!(tui.flash(), "Login cancelled");
    assert_eq!(polls(&server).await, before, "no poll after Esc");
    assert!(!world.dir.path().join("auth.json").exists());
}

async fn copilot_row() -> rustain::domain::models::OAuthCredential {
    let store: Arc<dyn AuthStorePort> = Arc::new(FileAuthStore::new());
    let snapshot = store.snapshot().await.unwrap();
    let rows = snapshot.rows("github-copilot");
    match &rows.last().expect("a Copilot row").credential {
        Credential::OAuth(oauth) => oauth.clone(),
        _ => panic!("Copilot stores an OAuth row"),
    }
}

async fn device_starts(server: &MockServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.url.path() == COPILOT_DEVICE_PATH)
        .map(|request| String::from_utf8(request.body.clone()).unwrap())
        .collect()
}

/// K06 (M06): the Enterprise prompt shows verbatim in a visible field; the
/// typed domain selects the Enterprise client and is stored lowercased; a
/// blank answer selects github.com.
#[tokio::test]
#[serial]
async fn k06_copilot_enterprise_prompt_is_a_visible_field() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let mut tui = Tui::new(AppConfig::default());
    tui.ctx = tui.ctx.clone().with_sleeper(instant());

    let recorder = Recorder::new();
    let guards = mount_login(
        &server,
        &recorder,
        "t-k06-enterprise",
        probe_reply(ENDPOINT_A),
    )
    .await;
    tui.submit("/login github-copilot");
    tui.until("enterprise prompt", Tui::field_pending).await;
    let frame = tui.frame();
    assert!(frame.contains(PROMPT.trim_end()), "{frame}");
    assert!(frame.contains("> ▏"), "a visible field\n{frame}");
    tui.type_text("Company.GHE.com");
    assert!(tui.frame().contains("> Company.GHE.com▏"));
    tui.key(DomainKey::Enter);
    tui.until_login("login finished", |event| {
        matches!(event, LoginEvent::Finished { .. })
    })
    .await;
    assert_eq!(
        device_starts(&server).await,
        [device_form(ENTERPRISE_CLIENT)]
    );
    assert_eq!(
        copilot_row().await.routing.enterprise_domain.as_deref(),
        Some("company.ghe.com")
    );
    drop(guards);

    let recorder = Recorder::new();
    let _guards = mount_login(&server, &recorder, "t-k06-public", probe_reply(ENDPOINT_A)).await;
    tui.submit("/login github-copilot");
    tui.until("enterprise prompt", Tui::field_pending).await;
    tui.key(DomainKey::Enter);
    tui.until_login("login finished", |event| {
        matches!(event, LoginEvent::Finished { .. })
    })
    .await;
    assert_eq!(
        device_starts(&server).await,
        [device_form(ENTERPRISE_CLIENT), device_form(PUBLIC_CLIENT)]
    );
    assert_eq!(copilot_row().await.routing.enterprise_domain, None);
}

/// K8 (20-9j, AC1-AC4): a verification URL too long for one row wraps under the
/// code, every row linked to the full URL (OSC 8 on) or scheme-less (plain);
/// `c` copies the code and `u` the full URL; the fitter keeps the whole link or
/// its label, and the code line when it cuts the header to its first line.
#[tokio::test]
#[serial]
async fn k8_long_device_url_wraps_all_or_none_and_copies_the_full_url() {
    let long = format!(
        "https://accounts.x.ai/device?user_code={USER_CODE}&next={}",
        "n".repeat(150)
    );
    let server = MockServer::start().await;
    let _world = World::new(&server);
    mount_xai(
        &server,
        device_body(&long, USER_CODE),
        ResponseTemplate::new(400).set_body_json(json!({"error": "authorization_pending"})),
    )
    .await;
    let mut tui = Tui::new(AppConfig::default());
    tui.ctx = tui.ctx.clone().with_sleeper(never());
    tui.submit("/login xai-oauth");
    tui.until_login("device code", |event| {
        matches!(event, LoginEvent::DeviceCode { .. })
    })
    .await;
    let flat = |text: &str| {
        text.replace('│', " ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    };
    let plain = flat(&tui.frame_at(80, 24));
    assert!(plain.contains(USER_CODE), "{plain}");
    assert!(
        !plain.contains("https://"),
        "plain rows drop the scheme\n{plain}"
    );
    let scheme_less = long.strip_prefix("https://").unwrap();
    assert!(plain.replace(' ', "").contains(scheme_less), "{plain}");
    let linked = flat(&decoded_text(&tui.login_buffer_at(80, 24, true)));
    assert!(linked.replace(' ', "").contains(&long), "{linked}");
    let rows = assert_link_cells(&tui, 80, 24, &[("Sign-in page: ", &long)]);
    assert!(rows[0] > 1, "the long page wraps: {rows:?}");

    // The device link shares the code header: fit keeps the whole header or its
    // first line (the code), so the link is whole or gone; the code always stays.
    // A full composer supplies the smallest sheet while every terminal stays
    // within the shared 60x16 minimum; retain both whole-link and code-only cases.
    tui.state.input_buffer = ["composer"; 10].join("\n");
    let (mut whole, mut code_only) = (false, false);
    for height in 16..=32 {
        let frame = tui.frame_at(60, height);
        assert!(frame.contains(USER_CODE), "{height}\n{frame}");
        assert!(frame.contains("Esc cancel"), "{height}\n{frame}");
        if frame.contains("Sign-in page:") {
            let rows = assert_link_cells(&tui, 60, height, &[("Sign-in page: ", &long)]);
            assert!(rows[0] > 1, "whole link at height {height}: {rows:?}");
            whole = true;
        } else {
            code_only = true;
            assert!(!frame.contains("accounts.x.ai"), "{frame}");
        }
    }
    assert!(whole && code_only, "{whole} {code_only}");

    // `c` copies the code and `u` the full URL, never the displayed rows.
    assert_eq!(tui.opened.lock().unwrap().as_slice(), [long.as_str()]);
    tui.input(DomainInputEvent::KeyPress('c'));
    tui.input(DomainInputEvent::KeyPress('u'));
    assert_eq!(
        tui.copied.lock().unwrap().as_slice(),
        [USER_CODE, long.as_str()]
    );
    tui.key(DomainKey::Esc);
    tui.until_login("cancelled", |event| {
        matches!(event, LoginEvent::Finished { .. })
    })
    .await;
    assert_eq!(tui.flash(), "Login cancelled");
}

/// The `Login` notices the store holds, oldest first.
fn login_notices(tui: &Tui) -> Vec<NoticeRecord> {
    tui.state
        .notifications
        .records()
        .filter(|record| record.source == NoticeSource::Login)
        .cloned()
        .collect()
}

/// K5: a Copilot login posts the `Success` summary, then the enabled count as
/// `Info` and the plan notice as `Warning`, each once; the notice is the
/// newest `Login` notice and raises the unseen badge on every login, a repeat
/// inside the suppression window included.
#[tokio::test]
#[serial]
async fn k5_copilot_login_posts_the_count_and_the_plan_notice_as_notices() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let mut tui = Tui::new(AppConfig::default());
    tui.ctx = tui.ctx.clone().with_sleeper(instant());
    let enabled = policy::enabled_line(PolicyReport {
        accepted: SEED_IDS,
        total: SEED_IDS,
    });
    let notice = plan::login_notice();

    let recorder = Recorder::new();
    let guards = mount_login(&server, &recorder, "t-k5-first", probe_reply(ENDPOINT_A)).await;
    let unseen = tui.state.notifications.unseen();
    tui.submit("/login github-copilot");
    tui.until("enterprise prompt", Tui::field_pending).await;
    tui.key(DomainKey::Enter);
    tui.until_login("login finished", |event| {
        matches!(event, LoginEvent::Finished { .. })
    })
    .await;
    let records = login_notices(&tui);
    let shown: Vec<(NoticeSeverity, &str, u32)> = records
        .iter()
        .map(|record| (record.severity, record.message.as_str(), record.count))
        .collect();
    assert_eq!(shown.len(), 3, "{shown:?}");
    assert_eq!(shown[0].0, NoticeSeverity::Success);
    assert!(
        shown[0].1.starts_with("Logged in to GitHub Copilot"),
        "{shown:?}"
    );
    assert_eq!(
        shown[1..],
        [
            (NoticeSeverity::Info, enabled.as_str(), 1),
            (NoticeSeverity::Warning, notice.as_str(), 1),
        ]
    );
    assert_eq!(
        tui.latest_notice(NoticeSource::Login),
        Some((NoticeSeverity::Warning, notice.clone()))
    );
    let after_first = tui.state.notifications.unseen();
    assert!(after_first > unseen, "{unseen} -> {after_first}");
    drop(guards);

    // A repeat inside the suppression window is counted silently, not shown
    // again, yet still raises the unseen badge.
    let recorder = Recorder::new();
    let _guards = mount_login(&server, &recorder, "t-k5-second", probe_reply(ENDPOINT_A)).await;
    tui.submit("/login github-copilot");
    tui.until("enterprise prompt", Tui::field_pending).await;
    tui.key(DomainKey::Enter);
    tui.until_login("login finished", |event| {
        matches!(event, LoginEvent::Finished { .. })
    })
    .await;
    assert!(tui.state.notifications.unseen() > after_first);
    let repeated: Vec<NoticeRecord> = login_notices(&tui)
        .into_iter()
        .filter(|record| record.message == notice)
        .collect();
    assert_eq!(repeated.len(), 1, "no second record");
    assert_eq!(repeated[0].count, 2);
}
