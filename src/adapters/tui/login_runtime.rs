//! Non-spawning TUI sign-in work. Handlers return data outcomes; event-loop
//! dispatch binds login cancellation/UI, starts work and independently launches
//! the validation checks returned after picker rows are emitted.

use crate::adapters::auth::flows::{self, FlowCtx, FlowError, Sleeper};
use crate::adapters::auth::host::{self, LoginPlan};
use crate::adapters::tui::clipboard::ClipboardResult;
use crate::adapters::tui::handlers::login::display_text;
use crate::adapters::tui::login_ui::{Opener, TuiLoginUi};
use crate::domain::events::{AppEvent, LoginEvent, LoginFailure, LoginSummary, LoginTargetRow};
use crate::domain::models::{AppConfig, AuthMethod};
use crate::domain::ports::AuthStorePort;
use crate::infrastructure::live_providers::LiveProviders;
use crate::infrastructure::runtime::event_bus::EventBus;
use std::sync::Arc;

/// Copies text to the clipboard (production: `tui::clipboard::copy_to_clipboard`).
pub type Copier = Arc<dyn Fn(&str) -> ClipboardResult + Send + Sync>;

/// Authentication worker collaborators, bound by dispatch before launching work.
#[derive(Clone)]
pub struct LoginCtx {
    pub events: Arc<EventBus>,
    pub live: Arc<LiveProviders>,
    pub store: Arc<dyn AuthStorePort>,
    pub opener: Opener,
    pub copier: Copier,
    pub clock: Arc<dyn Fn() -> i64 + Send + Sync>,
    /// Device-flow waits; `None` keeps the flow's real sleeper.
    pub sleeper: Option<Sleeper>,
}

impl LoginCtx {
    pub fn new(events: Arc<EventBus>, live: Arc<LiveProviders>) -> Self {
        Self {
            events,
            live,
            store: Arc::new(crate::adapters::auth_store::FileAuthStore::new()),
            opener: Arc::new(crate::adapters::auth::opener::open_url),
            copier: Arc::new(crate::adapters::tui::clipboard::copy_to_clipboard),
            clock: Arc::new(crate::infrastructure::utils::auth_now_ms),
            sleeper: None,
        }
    }
    pub fn with_opener(mut self, opener: Opener) -> Self {
        self.opener = opener;
        self
    }
    pub fn with_copier(mut self, copier: Copier) -> Self {
        self.copier = copier;
        self
    }
    pub fn with_clock(mut self, clock: Arc<dyn Fn() -> i64 + Send + Sync>) -> Self {
        self.clock = clock;
        self
    }
    pub fn with_sleeper(mut self, sleeper: Sleeper) -> Self {
        self.sleeper = Some(sleeper);
        self
    }
    fn emit(&self, event: LoginEvent) {
        let _ = self.events.emit_domain(AppEvent::Login(event));
    }
}
/// Runs the real login flow and publishes the existing live/result/discovery
/// sequence. Callers bind the UI and install running state before launching it.
pub async fn run_login(
    ctx: LoginCtx,
    app: AppConfig,
    login_id: u64,
    plan: LoginPlan,
    method: &'static AuthMethod,
    ui: Arc<TuiLoginUi>,
) {
    let LoginPlan {
        provider_id: provider,
        display,
        validation_config,
        ..
    } = plan;
    let before =
        crate::adapters::auth_store::FileAuthStore::try_snapshot_sync().unwrap_or_default();
    let mut flow = FlowCtx::new(ui, ctx.clock.clone()).with_validation_config(validation_config);
    if let Some(sleeper) = &ctx.sleeper {
        flow = flow.with_sleeper(sleeper.clone());
    }
    let result = match flows::login(&provider, method, &flow, &ctx.store).await {
        Ok(result) => result,
        Err(error) => {
            let outcome = match error {
                FlowError::Cancelled => LoginFailure::Cancelled,
                other => LoginFailure::Failed(display_text(&other.to_string())),
            };
            ctx.emit(LoginEvent::Finished {
                login_id,
                provider,
                outcome: Err(outcome),
            });
            return;
        }
    };
    // Phase 1 before `Finished`: the reopened picker finds the provider unlocked.
    let targets = ctx.live.register_change(&provider, &app, &before).await;
    let mut lines = vec![host::success_line(method, &result, &display)];
    if let Ok(saved) = host::credentials_saved_line() {
        lines.push(saved);
    }
    if let Ok(accounts) =
        crate::infrastructure::auth_accounts::accounts(&ctx.store, &app, &provider).await
        && accounts
            .rows
            .iter()
            .filter(|row| row.disabled_cause.is_none())
            .count()
            > 1
    {
        lines.push(host::active_line(&accounts, result.credential_id));
    }
    ctx.emit(LoginEvent::Finished {
        login_id,
        provider,
        outcome: Ok(LoginSummary {
            lines,
            credential_id: result.credential_id,
        }),
    });
    for (target, report) in ctx.live.refresh_changed(targets).await {
        if let Some(line) = host::discovery_line(&target.display_name, &report) {
            ctx.emit(LoginEvent::Discovered {
                provider: target.provider_id,
                line,
            });
        }
    }
}

/// Owned credential-check data returned to dispatch; contains no executable work.
pub struct ValidationCheck {
    provider: String,
    env_vars: Vec<String>,
    policy: Option<crate::domain::services::key_cascade::RowPolicy>,
}

/// Reads the store once and emits rows before returning independent validation
/// checks. Dispatch, not this worker, launches each returned check.
pub async fn prepare_targets(
    ctx: LoginCtx,
    app: AppConfig,
    generation: u64,
) -> Vec<ValidationCheck> {
    let snapshot = ctx.store.snapshot().await.unwrap_or_default();
    let entries = crate::infrastructure::provider_availability::effective_providers(&app);
    let env = crate::infrastructure::utils::env_var_trimmed;
    let mut rows = Vec::new();
    let mut checks = Vec::new();
    for target in host::login_targets(&app) {
        let accounts =
            crate::infrastructure::auth_accounts::describe(&snapshot, &app, &target.id, &env);
        let source = host::source_label(&accounts);
        let entry = entries
            .iter()
            .find(|entry| entry.cfg.enabled && entry.rows_key() == target.id);
        let env_vars: Vec<String> = entry
            .map(|entry| entry.env_vars().into_iter().map(str::to_owned).collect())
            .unwrap_or_default();
        if source.is_some() {
            checks.push(ValidationCheck {
                provider: target.id.clone(),
                env_vars: env_vars.clone(),
                policy: entry.map(|entry| entry.row_policy(&env)),
            });
        }
        rows.push(LoginTargetRow {
            search: format!(
                "{} {} {}",
                target.id,
                source.as_deref().unwrap_or(""),
                env_vars.join(" ")
            ),
            id: target.id,
            display: display_text(&target.display),
            source,
            status: host::status_text(&accounts),
        });
    }
    ctx.emit(LoginEvent::Targets { generation, rows });
    checks
}

/// Validates one served credential through the handle its adapter would use.
/// Independent dispatch tasks allow another provider to complete while one waits.
pub async fn validate_target(ctx: LoginCtx, generation: u64, check: ValidationCheck) {
    let ValidationCheck {
        provider,
        env_vars,
        policy,
    } = check;
    let valid = match policy {
        Some(policy) => crate::infrastructure::key_resolver::CredentialHandle::new(
            crate::infrastructure::key_resolver::shared(),
            provider.clone(),
            env_vars,
            policy,
            None,
        )
        .current()
        .await
        .is_ok(),
        None => false,
    };
    ctx.emit(LoginEvent::Validated {
        generation,
        provider,
        valid,
    });
}

#[cfg(test)]
#[path = "auth_outcome_tests.rs"]
mod auth_outcome_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::tui::app::InputAction;
    use crate::adapters::tui::handlers::login::{LOGIN, LoginAction, on_event, on_key};
    use crate::adapters::tui::state::{
        LoginOrigin, LoginSection, PendingField, RunningLogin, SecretField, TuiState,
    };
    use crate::domain::events::{DomainInputEvent, DomainKey, LoginReply};
    use crate::domain::ports::ProviderInfoPort;
    use tokio_util::sync::CancellationToken;

    fn running(state: &mut TuiState, with_section: bool) -> CancellationToken {
        let cancel = CancellationToken::new();
        let login_id = state.login.next_login_id();
        state.login.start_running(RunningLogin {
            login_id,
            provider_id: "anthropic".into(),
            display: "Anthropic".into(),
            origin: LoginOrigin::Slash,
            cancel: cancel.clone(),
            sections: if with_section {
                vec![LoginSection::AuthUrl {
                    url: "https://claude.ai/oauth/authorize?x=1".into(),
                    launch_url: "http://localhost:1455/launch".into(),
                    instructions: "Paste the code".into(),
                }]
            } else {
                Vec::new()
            },
            progress: Vec::new(),
            field: None,
        });
        state.focus = LOGIN;
        cancel
    }

    fn key(state: &mut TuiState, key: DomainKey) -> InputAction {
        on_key(state, &DomainInputEvent::SpecialKey(key))
    }

    #[test]
    fn secret_field_debug_never_shows_the_text() {
        let mut field = SecretField::default();
        field.push_str("sk-ant-secret");
        assert_eq!(format!("{field:?}"), "SecretField(<redacted>)");
        assert_eq!(field.char_count(), 13);
    }

    #[tokio::test]
    async fn masked_field_takes_typing_and_paste_and_replies_on_enter() {
        let mut state = TuiState::new(80, 24);
        running(&mut state, true);
        let (tx, rx) = tokio::sync::oneshot::channel();
        state.login.running.as_mut().unwrap().field =
            Some(PendingField::secret("Enter key: ".into(), LoginReply(tx)));
        // Plain letters type into a focused field; they never copy.
        for c in "cl".chars() {
            assert_eq!(
                on_key(&mut state, &DomainInputEvent::KeyPress(c)),
                InputAction::Consumed
            );
        }
        on_key(&mut state, &DomainInputEvent::Paste("-pasted\r\n".into()));
        // An image pasted while signing in is dropped.
        on_key(&mut state, &DomainInputEvent::ImagePaste(vec![0x89, b'P']));
        assert!(state.pending_images.is_empty());
        assert!(state.input_buffer.is_empty(), "the chat input is untouched");
        // The Alt forms always copy while a field is focused.
        assert_eq!(
            key(&mut state, DomainKey::AltL),
            InputAction::Login(LoginAction::Copy("http://localhost:1455/launch".into()))
        );
        assert_eq!(key(&mut state, DomainKey::Enter), InputAction::Consumed);
        let reply = rx.await.unwrap().unwrap();
        assert_eq!(reply.expose_secret(), "cl-pasted");
        assert!(state.login.running.as_ref().unwrap().field.is_none());
    }

    #[tokio::test]
    async fn a_pasted_value_loses_every_control_character() {
        let mut state = TuiState::new(80, 24);
        running(&mut state, true);
        let (tx, rx) = tokio::sync::oneshot::channel();
        state.login.running.as_mut().unwrap().field = Some(PendingField::text(
            crate::adapters::tui::state::FieldKind::Manual,
            "Paste the code:".into(),
            LoginReply(tx),
        ));
        on_key(
            &mut state,
            &DomainInputEvent::Paste("co\tde\n42\u{1b}[0m\u{9b}\r\n".into()),
        );
        assert_eq!(key(&mut state, DomainKey::Enter), InputAction::Consumed);
        assert_eq!(rx.await.unwrap().as_deref(), Some("code42[0m"));
    }

    /// A permission that arrives while the overlay owns focus is queued behind
    /// it (as behind every overlay) and shown as soon as the overlay closes.
    #[test]
    fn a_permission_queued_behind_the_overlay_surfaces_when_it_closes() {
        use crate::domain::models::FocusState;
        use crate::domain::models::visual::{ConfirmationType, OverlayType};
        let mut state = TuiState::new(80, 24);
        state.login.open_picker(
            FocusState::Input,
            LoginOrigin::Slash,
            crate::adapters::tui::state::OverlayMode::Login,
        );
        state.focus = LOGIN;
        state
            .permission_queue
            .push(crate::adapters::tui::state::PendingPermission {
                id: crate::domain::models::RequestId::new(),
                source: crate::domain::models::ApprovalSource::ForegroundTurn {
                    conversation_id: "c1".into(),
                },
                tool_name: "Write".into(),
                input: Arc::new(serde_json::json!({})),
                risk: crate::domain::models::ToolRisk::Safe,
            });
        assert_eq!(
            crate::adapters::tui::app::handle_input(
                &mut state,
                &DomainInputEvent::SpecialKey(DomainKey::Esc)
            ),
            InputAction::Login(LoginAction::Closed {
                origin: LoginOrigin::Slash
            })
        );
        assert!(
            state.pending_permission.is_some(),
            "the queued permission is shown"
        );
        assert!(state.permission_queue.is_empty());
        assert_eq!(
            state.focus,
            FocusState::Overlay(OverlayType::Confirmation(ConfirmationType::Permission))
        );
    }

    #[test]
    fn without_a_field_plain_keys_copy_and_esc_cancels_the_token() {
        let mut state = TuiState::new(80, 24);
        let cancel = running(&mut state, true);
        assert_eq!(
            on_key(&mut state, &DomainInputEvent::KeyPress('c')),
            InputAction::Login(LoginAction::Copy(
                "https://claude.ai/oauth/authorize?x=1".into()
            ))
        );
        assert_eq!(key(&mut state, DomainKey::Esc), InputAction::Consumed);
        assert!(cancel.is_cancelled());
        // The overlay keeps focus until the flow reports `Login cancelled`.
        assert_eq!(state.focus, LOGIN);
        assert_eq!(key(&mut state, DomainKey::CtrlQ), InputAction::Quit);
    }

    #[test]
    fn quit_path_cancels_a_running_login() {
        let mut state = TuiState::new(80, 24);
        let cancel = running(&mut state, false);
        state.login.cancel_running();
        assert!(cancel.is_cancelled());
    }

    #[test]
    fn a_success_that_raced_a_cancel_is_still_reported() {
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
            fn get_provider(
                &self,
                _: &str,
            ) -> Option<Arc<dyn crate::domain::ports::StreamingProvider>> {
                None
            }
            fn set_active_provider(
                &self,
                _: &str,
            ) -> Result<(), crate::domain::errors::ProviderError> {
                Ok(())
            }
            fn now_unix(&self) -> i64 {
                0
            }
            fn today_start_unix_ms(&self) -> i64 {
                0
            }
        }
        let mut state = TuiState::new(80, 24);
        running(&mut state, true);
        key(&mut state, DomainKey::Esc);
        let login_id = state.login.running.as_ref().unwrap().login_id;
        on_event(
            &mut state,
            &AppConfig::default(),
            &NoProviders,
            LoginEvent::Finished {
                login_id,
                provider: "anthropic".into(),
                outcome: Ok(LoginSummary {
                    lines: vec!["Logged in to Anthropic (Claude Pro/Max)".into()],
                    credential_id: crate::domain::models::CredentialId(1),
                }),
            },
        );
        assert!(!state.login.active);
        assert_eq!(
            state.feedback_blocks["login-result"].message,
            "Logged in to Anthropic (Claude Pro/Max)"
        );
    }
}
