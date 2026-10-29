//! K03 (AC1) the Anthropic browser login in the overlay: method picker,
//! waiting view, OSC-8 link bytes, opener, copy keys, masked paste → exchange;
//! K02 for the authorization code; K04 (AC4) Esc cancels the waiting view,
//! a masked key field and a device poll; K10's login waiting view at 60×16.

use super::bloom::stub;
use super::common::{Tui, World, clipboard_file, config_with, stubai};
use ratatui::{Terminal, backend::CrosstermBackend};
use rustain::adapters::auth_store::FileAuthStore;
use rustain::adapters::tui::state::{LoginSection, LoginView};
use rustain::adapters::tui::theme::Theme;
use rustain::adapters::tui::widgets::{
    hyperlink::{LinkSpan, write_links},
    login_dialog,
};
use rustain::domain::events::{DomainInputEvent, DomainKey, LoginEvent};
use rustain::domain::models::{AppConfig, Credential, CredentialOrigin, FocusState};
use rustain::domain::ports::AuthStorePort;
use serde_json::json;
use serial_test::serial;
use std::sync::Arc;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_partial_json, method, path},
};

const CODE: &str = "AUTH-CODE-K03-SECRET";

/// A clonable byte sink behind the crossterm backend and the TRACE capture.
#[derive(Clone)]
struct Shared(Arc<std::sync::Mutex<Vec<u8>>>);
impl std::io::Write for Shared {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
async fn token_mock(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/v1/oauth/token"))
        .and(body_partial_json(
            json!({"grant_type": "authorization_code"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "LOGIN-ACCESS-CANARY",
            "refresh_token": "LOGIN-REFRESH-CANARY",
            "expires_in": 3600,
            "account": {"uuid": "acct-1", "email_address": "op@example.test"},
            "organization": {"uuid": "org-1", "name": "Ops"},
        })))
        .mount(server)
        .await;
}

fn callback(authorize: &str, code: &str) -> String {
    let auth = url::Url::parse(authorize).unwrap();
    let query: std::collections::HashMap<_, _> = auth.query_pairs().into_owned().collect();
    let mut cb = url::Url::parse(&query["redirect_uri"]).unwrap();
    cb.query_pairs_mut()
        .append_pair("code", code)
        .append_pair("state", &query["state"]);
    cb.into()
}

/// `/login anthropic` → first method → the waiting view with its paste field.
async fn open_browser_login(tui: &mut Tui) -> (String, String) {
    tui.submit("/login anthropic");
    assert_eq!(tui.state.login.view, LoginView::MethodPicker);
    tui.key(DomainKey::Enter);
    // Bounded and event-driven: the paste field follows the link at once.
    tui.until("paste field", Tui::field_pending).await;
    let running = tui.state.login.running.as_ref().unwrap();
    let LoginSection::AuthUrl {
        url, launch_url, ..
    } = &running.sections[0]
    else {
        panic!("a browser login shows its link first");
    };
    (url.clone(), launch_url.clone())
}

/// Every OSC 8 link sits exactly over the text the frame drew there, so the
/// bytes written after the frame never land on other content.
fn assert_links_over_text(frame: &str, links: &[LinkSpan]) {
    let rows: Vec<&str> = frame.lines().collect();
    for link in links {
        let drawn: String = rows[link.y as usize]
            .chars()
            .skip(link.x as usize)
            .take(link.text.chars().count())
            .collect();
        assert_eq!(
            drawn, link.text,
            "link at ({}, {})\n{frame}",
            link.x, link.y
        );
    }
}

/// K03 (M03) + K02: the waiting view, its link bytes, opener and copy keys,
/// then a masked paste of the redirect completes the exchange; a `TRACE`
/// capture of the whole login never carries the authorization code.
#[test]
#[serial]
fn k03_browser_waiting_view_links_copies_and_completes_by_paste() {
    let logs = Arc::new(std::sync::Mutex::new(Vec::new()));
    let writer = Shared(logs.clone());
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        tracing::trace!("k03 capture on");
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(k03_body());
    });
    let captured = String::from_utf8_lossy(&logs.lock().unwrap()).into_owned();
    assert!(
        captured.contains("k03 capture on"),
        "the TRACE capture is on"
    );
    assert!(!captured.contains(CODE), "a log line carries the code");
}

async fn k03_body() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    token_mock(&server).await;
    let mut tui = Tui::new(AppConfig::default());
    let (authorize, launch) = open_browser_login(&mut tui).await;
    assert!(launch.starts_with("http://localhost:") && launch.ends_with("/launch"));
    assert_eq!(
        tui.opened.lock().unwrap().as_slice(),
        std::slice::from_ref(&launch)
    );
    let (frame, links) = tui.frame_at(80, 24);
    for text in [
        " Sign in to Anthropic ",
        "Continue in your browser. If it didn't open, open or copy the link below.",
        "Sign-in link: ",
        &format!("Local shortcut (this machine only): {launch}"),
        "› Preferred port 0 unavailable, using port",
        "Enter submit · Alt+C copy link · Alt+L copy local link · Esc cancel",
    ] {
        assert!(frame.contains(text), "{text}\n{frame}");
    }
    assert_eq!(links.len(), 2, "the sign-in link and the local shortcut");
    assert_links_over_text(&frame, &links);

    // The link is written over the drawn text as OSC 8 to the full URL.
    let full = url::Url::parse(&authorize).unwrap().to_string();
    let bytes = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let mut terminal = Terminal::with_options(
        CrosstermBackend::new(Shared(bytes.clone())),
        ratatui::TerminalOptions {
            viewport: ratatui::Viewport::Fixed(ratatui::layout::Rect::new(0, 0, 80, 24)),
        },
    )
    .unwrap();
    let mut links = Vec::new();
    terminal
        .draw(|frame| {
            links = login_dialog::render(frame, frame.area(), &tui.state.login, &Theme::dark());
        })
        .unwrap();
    write_links(terminal.backend_mut(), &links).unwrap();
    let bytes = String::from_utf8_lossy(&bytes.lock().unwrap()).into_owned();
    assert!(bytes.contains(&format!("\x1b]8;;{full}\x07")), "{bytes:?}");
    assert!(bytes.contains(&format!("\x1b]8;;{launch}\x07{launch}\x1b]8;;\x07")));

    // K10: the same waiting view at 60×16 keeps borders, link, field and footer.
    let (small, small_links) = tui.frame_at(60, 16);
    super::small_terminal::assert_fitted(&small, &["Sign-in link:", "> "], "Esc cancel");
    assert!(!small_links.is_empty(), "the sign-in link is still linked");
    assert_links_over_text(&small, &small_links);

    // Plain letters type into the focused field; Alt+L/Alt+C copy.
    tui.input(DomainInputEvent::KeyPress('l'));
    assert_eq!(
        tui.state
            .login
            .running
            .as_ref()
            .unwrap()
            .field
            .as_ref()
            .unwrap()
            .buffer
            .char_count(),
        1
    );
    tui.key(DomainKey::Backspace);
    tui.key(DomainKey::AltL);
    tui.key(DomainKey::AltC);
    assert_eq!(
        tui.copied.lock().unwrap().as_slice(),
        [launch.clone(), full]
    );
    assert_eq!(tui.flash(), "Copied to clipboard");

    // Paste the redirect; the field shows bullets only; Enter exchanges.
    let redirect = callback(&authorize, CODE);
    tui.paste(&redirect);
    let frame = tui.frame();
    assert!(!frame.contains(CODE), "{frame}");
    assert!(frame.contains("> •••"), "{frame}");
    tui.key(DomainKey::Enter);
    tui.until_login("login finished", |event| {
        matches!(event, LoginEvent::Finished { .. })
    })
    .await;
    assert_eq!(
        tui.feedback().lines().next(),
        Some("Logged in to Anthropic (Claude Pro/Max) as op@example.test (Ops)")
    );
    let tokens: Vec<_> = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| request.url.path() == "/v1/oauth/token")
        .collect();
    assert_eq!(tokens.len(), 1, "one token POST");
    let rows = FileAuthStore::new().snapshot().await.unwrap();
    let rows = rows.rows("anthropic");
    assert_eq!(rows.len(), 1);
    assert!(matches!(rows[0].credential, Credential::OAuth(_)));
    assert_eq!(rows[0].origin, CredentialOrigin::Login);

    // K02 for the authorization code.
    tui.drain();
    for frame in &tui.frames {
        assert!(!frame.contains(CODE));
    }
    for event in &tui.event_log {
        assert!(!event.contains(CODE), "{event}");
    }
    assert!(!tui.feedback().contains(CODE) && !tui.flash().contains(CODE));
    assert!(tui.copied.lock().unwrap().iter().all(|c| !c.contains(CODE)));
    assert!(!clipboard_file(&world).contains(CODE));
}

/// K04 (M04): Esc in the browser waiting view cancels the login: nothing is
/// stored, the callback port is closed, no token request is made.
#[tokio::test]
#[serial]
async fn k04_esc_in_the_waiting_view_cancels_and_closes_the_callback_port() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    token_mock(&server).await;
    let mut tui = Tui::new(AppConfig::default());
    let (_, launch) = open_browser_login(&mut tui).await;
    let port = url::Url::parse(&launch).unwrap().port().unwrap();
    tui.key(DomainKey::Esc);
    tui.until_login("cancelled", |event| {
        matches!(event, LoginEvent::Finished { .. })
    })
    .await;
    assert_eq!(tui.flash(), "Login cancelled");
    assert_eq!(tui.state.focus, FocusState::Input);
    assert!(
        !world.dir.path().join("auth.json").exists(),
        "nothing stored"
    );
    assert!(
        tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_err(),
        "the callback listener is closed"
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

/// K04 (M04): Esc in a masked key field cancels before any request.
#[tokio::test]
#[serial]
async fn k04_esc_in_a_masked_key_field_cancels_before_any_request() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    let (_listener, base) = stub().await;
    let mut tui = Tui::new(config_with(&[("stubai", stubai(&base))]));
    tui.submit("/login stubai");
    tui.until("masked key field", Tui::field_pending).await;
    tui.paste("sk-never-sent");
    // K10: the masked key view fits the smallest rendered terminal too.
    let (small, _) = tui.frame_at(60, 16);
    super::small_terminal::assert_fitted(&small, &["> •"], "Esc cancel");
    tui.key(DomainKey::CtrlC);
    tui.until_login("cancelled", |event| {
        matches!(event, LoginEvent::Finished { .. })
    })
    .await;
    assert_eq!(tui.flash(), "Login cancelled");
    assert!(!world.dir.path().join("auth.json").exists());
    let store: Arc<dyn AuthStorePort> = Arc::new(FileAuthStore::new());
    assert!(store.snapshot().await.unwrap().rows("stubai").is_empty());
}
