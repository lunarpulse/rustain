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
    assert_eq!(state.focus, login::LOGIN);
    assert!(matches!(
        accounts::command(&mut state, "logout", None),
        HandlerOutcome::Quiet
    ));
}
