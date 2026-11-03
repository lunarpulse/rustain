//! K12 (AC10): `/login`, `/login <id>` and `/models` reach their arms through
//! `submit_message`, open the matching overlay, and autocomplete lists them;
//! Ctrl+X, I opens the provider picker.

use super::common::{GUARD, Tui, World};
use rustain::adapters::command_registry::CommandRegistry;
use rustain::adapters::tui::app::InputAction;
use rustain::adapters::tui::presentation::Surface;
use rustain::adapters::tui::state::LoginView;
use rustain::domain::events::{DomainInputEvent, DomainKey};
use rustain::domain::models::AppConfig;
use rustain::domain::models::FocusState;
use serial_test::serial;
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

/// K12 (M12): every sign-in command routes and opens its overlay.
#[tokio::test]
#[serial]
async fn k12_login_and_models_commands_route_and_open_their_overlays() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let mut tui = Tui::new(AppConfig::default());

    // `/login` opens the provider picker, which fills from the store off-loop.
    let action = tui.submit("/login");
    assert_routed(&action, "login", None);
    assert_eq!(tui.state.modal_stack.top().unwrap().surface, Surface::Login);
    assert_eq!(tui.state.login.view, LoginView::ProviderPicker);
    tui.until("picker rows", |tui| !tui.state.login.loading)
        .await;
    let frame = tui.frame();
    assert!(frame.contains(" Sign in "), "{frame}");
    assert!(frame.contains("Anthropic · not signed in"), "{frame}");
    // K10: the provider picker fits the smallest rendered terminal.
    let small = tui.frame_at(60, 16);
    super::small_terminal::assert_fitted(
        &small,
        &["Type to search", "Anthropic · not signed in"],
        "Esc close",
    );
    tui.key(DomainKey::Esc);
    assert_eq!(tui.state.focus, FocusState::Input);

    // `/login anthropic` goes straight to its method picker, CLI order and labels.
    let action = tui.submit("/login anthropic");
    assert_routed(&action, "login", Some("anthropic"));
    assert_eq!(tui.state.login.view, LoginView::MethodPicker);
    let frame = tui.frame();
    let browser = frame
        .find("Claude Pro/Max subscription (browser)")
        .expect("browser method listed");
    let key = frame.find("API key").expect("API key method listed");
    assert!(browser < key, "CLI method order\n{frame}");
    let small = tui.frame_at(60, 16);
    super::small_terminal::assert_fitted(&small, &["Choose a login method:"], "Esc close");
    tui.key(DomainKey::Esc);

    // `/models` opens the model picker.
    let action = tui.submit("/models");
    assert_routed(&action, "models", None);
    assert_eq!(
        tui.state.modal_stack.top().unwrap().surface,
        Surface::ModelSelector
    );
    tui.key(DomainKey::Esc);

    // Ctrl+X, I opens the provider picker.
    tui.key(DomainKey::CtrlX);
    tui.input(DomainInputEvent::KeyPress('i'));
    assert_eq!(tui.state.modal_stack.top().unwrap().surface, Surface::Login);
    assert_eq!(tui.state.login.view, LoginView::ProviderPicker);

    // Autocomplete lists both built-ins.
    let registry = CommandRegistry::new();
    for (typed, name) in [("log", "login"), ("mod", "models")] {
        assert!(
            registry
                .filter(typed)
                .iter()
                .any(|command| command.name == name),
            "/{name} completes from `/{typed}`"
        );
    }
    let _ = GUARD;
}

/// A keyless provider reports the CLI's keyless line and opens nothing.
#[tokio::test]
#[serial]
async fn keyless_provider_reports_the_cli_line() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let mut tui = Tui::new(AppConfig::default());
    tui.submit("/login ollama");
    assert_eq!(tui.state.focus, FocusState::Input);
    assert_eq!(
        tui.feedback(),
        "Ollama does not require an API key — no credentials to store."
    );
}

/// Alt+C/L/U outside the login overlay are consumed no-ops (Story 21.3 AC12):
/// they never type `c`/`l`/`u` into the composer.
#[tokio::test]
#[serial]
async fn alt_keys_outside_the_overlay_are_inert() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let mut tui = Tui::new(AppConfig::default());
    for key in [DomainKey::AltC, DomainKey::AltL, DomainKey::AltU] {
        tui.key(key);
    }
    assert_eq!(tui.state.input_buffer, "");
}

/// Reopening a stacked Login keeps its picker data, but dismisses higher
/// frames. Back returns the untouched caller and its original base.
#[test]
fn login_reopen_retains_generation_payload_and_caller_from_both_bases() {
    use rustain::adapters::tui::handlers::{HandlerOutcome, login};
    use rustain::adapters::tui::presentation::reducer;
    use rustain::adapters::tui::quit_confirm::{QuitFacts, QuitHost};
    use rustain::adapters::tui::state::TuiState;
    for base in [FocusState::Input, FocusState::Chat] {
        let mut state = TuiState::new(80, 24);
        if base == FocusState::Chat {
            rustain::adapters::tui::app::handle_input(
                &mut state,
                &DomainInputEvent::SpecialKey(DomainKey::Esc),
            );
        }
        reducer::open(&mut state, Surface::Help);
        state.help_overlay.scroll_offset = 7;
        login::command(&mut state, &AppConfig::default(), None);
        state.login.query = "open".into();
        let generation = state.login.generation;
        let frame_id = state.modal_stack.top().unwrap().id;
        let mut facts = QuitFacts::idle(QuitHost::Local);
        facts.streams = 1;
        reducer::request_quit(&mut state, facts);
        assert!(matches!(
            login::command(&mut state, &AppConfig::default(), Some("anthropic")),
            HandlerOutcome::Quiet
        ));
        assert_eq!(state.modal_stack.top().unwrap().id, frame_id);
        assert_eq!(state.login.generation, generation);
        assert_eq!(state.login.query, "open");
        assert_eq!(state.focus, base);
        login::on_key(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Esc));
        assert_eq!(state.login.query, "");
        login::on_key(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Esc));
        assert_eq!(state.modal_stack.top().unwrap().surface, Surface::Help);
        assert_eq!(state.help_overlay.scroll_offset, 7);
        reducer::back(&mut state);
        assert!(state.modal_stack.is_empty());
        assert_eq!(state.focus, base);
    }
}
