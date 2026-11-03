use crate::adapters::tui::clipboard::ClipboardResult;
use crate::adapters::tui::handlers::{AuthWork, HandlerOutcome, SpawnRequest, accounts, login};
use crate::adapters::tui::login_runtime::Copier;
use crate::adapters::tui::state::{LoginOrigin, LoginView, OverlayMode, RunningLogin, TuiState};
use crate::domain::models::{AppConfig, AuthMethod, FocusState};
use crate::domain::ports::{ProviderInfoPort, StreamingProvider};
use std::sync::{Arc, mpsc};
use tokio_util::sync::CancellationToken;

struct NoProviders;
impl ProviderInfoPort for NoProviders {
    fn active_delegate_id(&self) -> String {
        String::new()
    }
    fn get_model(
        &self,
        _: &str,
        _: &str,
    ) -> Option<crate::domain::models::provider::ModelDescriptor> {
        None
    }
    fn get_model_provider(&self, _: &str, _: Option<&str>) -> Option<String> {
        None
    }
    fn list_providers(&self) -> Vec<crate::domain::models::provider::ProviderDescriptor> {
        vec![]
    }
    fn list_models_by_provider(
        &self,
        _: &str,
    ) -> Vec<crate::domain::models::provider::ModelDescriptor> {
        vec![]
    }
    fn get_provider(&self, _: &str) -> Option<Arc<dyn StreamingProvider>> {
        None
    }
    fn set_active_provider(&self, _: &str) -> Result<(), crate::domain::errors::ProviderError> {
        Ok(())
    }
    fn now_unix(&self) -> i64 {
        0
    }
    fn today_start_unix_ms(&self) -> i64 {
        0
    }
}

#[test]
fn k1_outcomes_are_data_without_a_runtime() {
    let app = AppConfig::default();
    let mut state = TuiState::new(80, 24);
    let HandlerOutcome::RequestSpawn(SpawnRequest::Authentication(AuthWork::Targets {
        generation,
    })) = login::command(&mut state, &app, None)
    else {
        panic!("expected targets")
    };
    assert_eq!(generation, state.login.generation);
    assert!(state.login.loading);
    assert!(state.login.running.is_none());

    let mut state = TuiState::new(80, 24);
    let HandlerOutcome::RequestSpawn(SpawnRequest::Authentication(AuthWork::Login {
        login_id,
        plan,
        method,
        origin,
    })) = login::command(&mut state, &app, Some("deepseek"))
    else {
        panic!("expected login")
    };
    assert_eq!(login_id, 1);
    assert_eq!(plan.display, "DeepSeek");
    assert_eq!(plan.validation_config.api_key_env, "DEEPSEEK_API_KEY");
    assert_eq!(plan.methods.len(), 1);
    assert_eq!(plan.provider_id, "deepseek");
    assert!(!plan.keyless);
    assert_eq!(plan.validation_config.provider_id, "deepseek");
    assert!(matches!(method, AuthMethod::ApiKey));
    assert_eq!(origin, LoginOrigin::Slash);
    assert!(state.login.running.is_none());

    let mut state = TuiState::new(80, 24);
    let HandlerOutcome::RequestSpawn(SpawnRequest::Authentication(AuthWork::Providers {
        generation,
    })) = accounts::command(&mut state, "logout", None)
    else {
        panic!("expected providers")
    };
    assert_eq!(generation, state.login.generation);
    assert_eq!(state.login.mode, OverlayMode::Logout);

    let mut state = TuiState::new(80, 24);
    let HandlerOutcome::RequestSpawn(SpawnRequest::Authentication(AuthWork::Listing {
        generation,
        mode,
        provider,
    })) = accounts::command(&mut state, "accounts", Some(" openai "))
    else {
        panic!("expected listing")
    };
    assert_eq!(generation, state.login.generation);
    assert_eq!(mode, OverlayMode::Accounts);
    assert_eq!(provider, "openai");

    let (sink, copied) = mpsc::channel();
    let copier: Copier = Arc::new(move |text| {
        sink.send(text.to_owned()).unwrap();
        ClipboardResult::Osc52Success
    });
    let generation = state.login.generation;
    let HandlerOutcome::RequestSpawn(SpawnRequest::Authentication(AuthWork::Change {
        generation: actual,
        change,
        provider,
        id,
    })) = login::on_action(
        &mut state,
        &copier,
        &app,
        &NoProviders,
        login::LoginAction::Accounts(accounts::AccountAction::Select {
            provider: "openai".into(),
            id: 42,
        }),
    )
    else {
        panic!("expected forwarded change")
    };
    assert_eq!(actual, generation);
    assert!(matches!(
        change,
        crate::adapters::tui::accounts_runtime::Change::Select
    ));
    assert_eq!(provider, "openai");
    assert_eq!(id, 42);
    assert!(copied.try_recv().is_err());
    let HandlerOutcome::RequestSpawn(SpawnRequest::Authentication(AuthWork::Change {
        generation: actual,
        change,
        provider,
        id,
    })) = login::on_action(
        &mut state,
        &copier,
        &app,
        &NoProviders,
        login::LoginAction::Accounts(accounts::AccountAction::Logout {
            provider: "openai".into(),
            id: 7,
        }),
    )
    else {
        panic!("expected forwarded logout")
    };
    assert_eq!(actual, generation);
    assert!(matches!(
        change,
        crate::adapters::tui::accounts_runtime::Change::Logout
    ));
    assert_eq!(provider, "openai");
    assert_eq!(id, 7);
    let HandlerOutcome::RequestSpawn(SpawnRequest::Authentication(AuthWork::Listing {
        generation,
        mode,
        provider,
    })) = login::on_action(
        &mut state,
        &copier,
        &app,
        &NoProviders,
        login::LoginAction::Accounts(accounts::AccountAction::List {
            provider: "deepseek".into(),
        }),
    )
    else {
        panic!("expected forwarded listing")
    };
    assert_eq!(generation, state.login.generation);
    assert_eq!(mode, OverlayMode::Accounts);
    assert_eq!(provider, "deepseek");
}

#[test]
fn k1_login_actions_preserve_origin_and_selected_method() {
    let app = AppConfig::default();
    let copier: Copier = Arc::new(|_| ClipboardResult::Osc52Success);
    let mut state = TuiState::new(80, 24);
    let origin = LoginOrigin::Picker {
        column: "deepseek".into(),
    };
    let HandlerOutcome::RequestSpawn(SpawnRequest::Authentication(AuthWork::Targets {
        generation,
    })) = login::on_action(
        &mut state,
        &copier,
        &app,
        &NoProviders,
        login::LoginAction::Open {
            provider: None,
            origin: origin.clone(),
        },
    )
    else {
        panic!("expected targets from open")
    };
    assert_eq!(generation, state.login.generation);
    assert_eq!(state.login.origin, origin);
    let HandlerOutcome::RequestSpawn(SpawnRequest::Authentication(AuthWork::Login {
        login_id,
        plan,
        method,
        origin: actual,
    })) = login::on_action(
        &mut state,
        &copier,
        &app,
        &NoProviders,
        login::LoginAction::Choose {
            provider: "deepseek".into(),
        },
    )
    else {
        panic!("expected login from choose")
    };
    assert_eq!(login_id, 1);
    assert_eq!(plan.provider_id, "deepseek");
    assert!(matches!(method, AuthMethod::ApiKey));
    assert_eq!(actual, origin);
    assert!(state.login.running.is_none());

    assert!(matches!(
        login::on_action(
            &mut state,
            &copier,
            &app,
            &NoProviders,
            login::LoginAction::Choose {
                provider: "openrouter".into()
            }
        ),
        HandlerOutcome::Quiet
    ));
    assert_eq!(state.login.view, LoginView::MethodPicker);
    let HandlerOutcome::RequestSpawn(SpawnRequest::Authentication(AuthWork::Login {
        login_id,
        plan,
        method,
        origin: actual,
    })) = login::on_action(
        &mut state,
        &copier,
        &app,
        &NoProviders,
        login::LoginAction::Start {
            provider: "openrouter".into(),
            method: 1,
        },
    )
    else {
        panic!("expected selected login method")
    };
    assert_eq!(login_id, 2);
    assert_eq!(plan.provider_id, "openrouter");
    assert!(matches!(method, AuthMethod::ApiKey));
    assert_eq!(actual, origin);
    assert!(state.login.running.is_none());
}

#[test]
fn k1_quiet_paths_are_synchronous() {
    let app = AppConfig::default();
    let mut state = TuiState::new(80, 24);
    let (sink, copied) = mpsc::channel();
    let copier: Copier = Arc::new(move |text| {
        sink.send(text.to_owned()).unwrap();
        ClipboardResult::Osc52Success
    });
    assert!(matches!(
        login::command(&mut state, &app, Some("ollama")),
        HandlerOutcome::Quiet
    ));
    assert!(matches!(
        login::command(&mut state, &app, Some("not-a-provider")),
        HandlerOutcome::Quiet
    ));
    assert!(matches!(
        login::command(&mut state, &app, Some("anthropic")),
        HandlerOutcome::Quiet
    ));
    assert_eq!(state.login.view, LoginView::MethodPicker);
    assert!(matches!(
        login::on_action(
            &mut state,
            &copier,
            &app,
            &NoProviders,
            login::LoginAction::Start {
                provider: "anthropic".into(),
                method: usize::MAX
            }
        ),
        HandlerOutcome::Quiet
    ));
    assert!(matches!(
        login::on_action(
            &mut state,
            &copier,
            &app,
            &NoProviders,
            login::LoginAction::Copy("example".into())
        ),
        HandlerOutcome::Quiet
    ));
    assert_eq!(copied.try_recv().unwrap(), "example");
    assert!(copied.try_recv().is_err());
    assert!(matches!(
        login::on_action(
            &mut state,
            &copier,
            &app,
            &NoProviders,
            login::LoginAction::Closed {
                origin: LoginOrigin::Slash
            }
        ),
        HandlerOutcome::Quiet
    ));
    state.login.start_running(RunningLogin {
        login_id: 7,
        generation: state.login.generation,
        provider_id: "deepseek".into(),
        display: "DeepSeek".into(),
        origin: LoginOrigin::Slash,
        cancel: CancellationToken::new(),
        sections: vec![],
        progress: vec![],
        field: None,
    });
    state.focus = FocusState::Input;
    assert!(matches!(
        login::command(&mut state, &app, None),
        HandlerOutcome::Quiet
    ));
    assert!(crate::adapters::tui::presentation::is_open(
        &state,
        crate::adapters::tui::presentation::Surface::Login
    ));
    assert!(matches!(
        accounts::command(&mut state, "logout", None),
        HandlerOutcome::Quiet
    ));
}

#[test]
fn k7_ba_login_preserves_base_and_quit_returns_to_its_frame() {
    use crate::adapters::tui::{app, keymap::Context};
    use crate::domain::events::{DomainInputEvent as Event, DomainKey as Key};

    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Chat;
    let config = AppConfig::default();
    assert!(matches!(
        login::command(&mut state, &config, None),
        HandlerOutcome::RequestSpawn(_)
    ));
    assert_eq!(state.focus, FocusState::Chat, "Login must retain the base");
    let login_frame = state.modal_stack.top().expect("Login owns a frame").id;
    assert_eq!(
        state.modal_stack.top().unwrap().surface.context(),
        Context::Login
    );

    assert_eq!(
        app::handle_input(&mut state, &Event::SpecialKey(Key::CtrlQ)),
        app::InputAction::Quit
    );
    let mut facts = crate::adapters::tui::quit_confirm::QuitFacts::idle(
        crate::adapters::tui::quit_confirm::QuitHost::Local,
    );
    facts.composer_draft = true;
    crate::adapters::tui::quit_confirm::request(&mut state, facts);
    app::handle_input(&mut state, &Event::SpecialKey(Key::Esc));
    assert_eq!(state.modal_stack.top().unwrap().id, login_frame);
    assert_eq!(state.focus, FocusState::Chat);
    let app::InputAction::Login(action) =
        app::handle_input(&mut state, &Event::SpecialKey(Key::Esc))
    else {
        panic!("Login keys must still pass through its handler");
    };
    let copier: Copier = Arc::new(|_| ClipboardResult::Osc52Success);
    login::on_action(&mut state, &copier, &config, &NoProviders, action);
    assert!(state.modal_stack.is_empty());
    assert_eq!(state.focus, FocusState::Chat);
}

mod ba_login_lifetime {
    use super::*;
    use crate::adapters::tui::{
        app,
        presentation::{self, Surface, reducer},
    };
    use crate::domain::events::{
        DomainInputEvent as Event, DomainKey as Key, LoginEvent, LoginFailure, LoginReply,
        LoginSummary,
    };
    use crate::domain::models::{CredentialId, SecretString};
    use tokio::sync::oneshot;

    type SecretReply = oneshot::Receiver<Option<SecretString>>;

    fn event(state: &mut TuiState, event: LoginEvent) {
        login::on_event(state, &AppConfig::default(), &NoProviders, event);
    }

    fn key(state: &mut TuiState, key: Key) -> app::InputAction {
        app::handle_input(state, &Event::SpecialKey(key))
    }

    fn field_len(state: &TuiState) -> Option<usize> {
        state
            .login
            .running
            .as_ref()
            .unwrap()
            .field
            .as_ref()
            .map(|f| f.buffer.char_count())
    }

    fn prompted(depth: usize) -> (TuiState, CancellationToken, SecretReply, u64) {
        let mut state = TuiState::new(80, 24);
        for surface in [
            Surface::Help,
            Surface::ModelSelector,
            Surface::ProfileSwitcher,
        ]
        .into_iter()
        .take(depth - 1)
        {
            reducer::open(&mut state, surface);
        }
        let HandlerOutcome::RequestSpawn(SpawnRequest::Authentication(AuthWork::Login {
            login_id,
            origin,
            ..
        })) = login::command(&mut state, &AppConfig::default(), Some("deepseek"))
        else {
            panic!("API-key login must be planned");
        };
        let cancel = CancellationToken::new();
        state.login.start_running(RunningLogin {
            login_id,
            generation: state.login.generation,
            provider_id: "deepseek".into(),
            display: "DeepSeek".into(),
            origin,
            cancel: cancel.clone(),
            sections: vec![],
            progress: vec![],
            field: None,
        });
        event(
            &mut state,
            LoginEvent::AuthUrl {
                login_id,
                url: "https://example.invalid/?code=private".into(),
                launch_url: "https://example.invalid/".into(),
                instructions: "link".into(),
            },
        );
        let (reply, received) = oneshot::channel();
        event(
            &mut state,
            LoginEvent::PromptSecret {
                login_id,
                label: "API key".into(),
                reply: LoginReply(reply),
            },
        );
        app::handle_input(&mut state, &Event::KeyPress('k'));
        app::handle_input(&mut state, &Event::Paste("e\ny".into()));
        assert_eq!(field_len(&state), Some(3));
        assert_eq!(state.modal_stack.len(), depth);
        (state, cancel, received, login_id)
    }

    fn late_prompt(state: &mut TuiState, login_id: u64) {
        let (reply, mut received) = oneshot::channel();
        event(
            state,
            LoginEvent::PromptSecret {
                login_id,
                label: "late API key".into(),
                reply: LoginReply(reply),
            },
        );
        assert_eq!(field_len(state), None);
        assert!(matches!(
            received.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        ));
        let (reply, mut received) = oneshot::channel();
        event(
            state,
            LoginEvent::ManualInput {
                login_id,
                prompt: "late code".into(),
                reply: LoginReply(reply),
            },
        );
        assert_eq!(field_len(state), None);
        assert!(matches!(
            received.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        ));
    }

    #[test]
    fn ba_login_discard_cancels_secret_and_ignores_late_callbacks() {
        let (mut state, cancel, mut received, login_id) = prompted(4);
        assert_eq!(
            state
                .modal_stack
                .iter()
                .map(|f| f.surface)
                .collect::<Vec<_>>(),
            [
                Surface::Help,
                Surface::ModelSelector,
                Surface::ProfileSwitcher,
                Surface::Login
            ]
        );
        key(&mut state, Key::CtrlP);
        assert_eq!(
            state.modal_stack.top().unwrap().surface,
            Surface::CommandPalette
        );
        assert!(!presentation::is_open(&state, Surface::Login));
        assert!(
            cancel.is_cancelled(),
            "discarding the frame must cancel its authentication"
        );
        assert_eq!(field_len(&state), None);
        assert!(matches!(
            received.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        ));
        let frames = state.modal_stack.iter().cloned().collect::<Vec<_>>();
        late_prompt(&mut state, login_id);
        event(
            &mut state,
            LoginEvent::AuthUrl {
                login_id,
                url: "https://example.invalid/?code=late".into(),
                launch_url: "https://example.invalid/".into(),
                instructions: "late link".into(),
            },
        );
        event(
            &mut state,
            LoginEvent::Progress {
                login_id,
                message: "late progress".into(),
            },
        );
        let running = state.login.running.as_ref().unwrap();
        assert!(running.sections.is_empty() && running.progress.is_empty());
        assert!(matches!(
            login::command(&mut state, &AppConfig::default(), None),
            HandlerOutcome::Quiet
        ));
        assert!(
            !presentation::is_open(&state, Surface::Login),
            "a retired flow cannot acquire a fresh completion lease while cancelling"
        );
        event(
            &mut state,
            LoginEvent::Finished {
                login_id,
                provider: "deepseek".into(),
                outcome: Ok(LoginSummary {
                    lines: vec!["Credential already persisted".into()],
                    credential_id: CredentialId(42),
                }),
            },
        );
        assert!(state.login.running.is_none());
        assert_eq!(
            state.modal_stack.iter().cloned().collect::<Vec<_>>(),
            frames
        );
        assert!(
            state
                .notifications
                .records()
                .any(|record| record.message.contains("Credential already persisted"))
        );
        assert!(matches!(
            login::command(&mut state, &AppConfig::default(), None),
            HandlerOutcome::RequestSpawn(_)
        ));
        assert!(presentation::is_open(&state, Surface::Login));
    }

    #[test]
    fn ba_login_release_cancels_at_every_depth_but_cover_preserves_the_flow() {
        for depth in 1..=4 {
            for release in ["close", "handoff", "truncate", "escape"] {
                if release == "truncate" && depth == 1 {
                    continue;
                }
                let (mut state, cancel, mut received, login_id) = prompted(depth);
                match release {
                    "close" => reducer::close(&mut state, Surface::Login),
                    "handoff" => {
                        reducer::handoff(&mut state, Surface::Login, Surface::UsagePanel);
                    }
                    "truncate" => {
                        reducer::open(&mut state, Surface::Help);
                    }
                    "escape" => {
                        key(&mut state, Key::Esc);
                    }
                    _ => unreachable!(),
                }
                assert!(cancel.is_cancelled(), "{release} at depth {depth}");
                assert_eq!(field_len(&state), None);
                assert!(matches!(
                    received.try_recv(),
                    Err(oneshot::error::TryRecvError::Closed)
                ));
                late_prompt(&mut state, login_id);
                event(
                    &mut state,
                    LoginEvent::Finished {
                        login_id,
                        provider: "deepseek".into(),
                        outcome: Err(LoginFailure::Cancelled),
                    },
                );
                assert!(!presentation::is_open(&state, Surface::Login));
                assert!(state.login.running.is_none());
            }
            for cover in [Surface::QuitConfirm, Surface::NotificationLog] {
                let (mut state, cancel, mut received, _) = prompted(depth);
                let login_frame = state.modal_stack.top().cloned().unwrap();
                if cover == Surface::QuitConfirm {
                    assert_eq!(key(&mut state, Key::CtrlQ), app::InputAction::Quit);
                    let mut facts = crate::adapters::tui::quit_confirm::QuitFacts::idle(
                        crate::adapters::tui::quit_confirm::QuitHost::Local,
                    );
                    facts.composer_draft = true;
                    crate::adapters::tui::quit_confirm::request(&mut state, facts);
                } else {
                    reducer::open(&mut state, cover);
                }
                assert!(!cancel.is_cancelled(), "cover/continuation is not release");
                assert!(matches!(
                    received.try_recv(),
                    Err(oneshot::error::TryRecvError::Empty)
                ));
                reducer::back(&mut state);
                assert_eq!(state.modal_stack.top(), Some(&login_frame));
                assert_eq!(field_len(&state), Some(3));
                assert_eq!(state.login.running.as_ref().unwrap().sections.len(), 1);
                if depth == 4 {
                    crate::adapters::tui::handlers::profile_switch::complete(&mut state);
                    assert_eq!(state.modal_stack.top(), Some(&login_frame));
                    assert!(!cancel.is_cancelled());
                    assert_eq!(field_len(&state), Some(3));
                }
            }
        }
    }
}
