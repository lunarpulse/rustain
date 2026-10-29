//! Sign-in from the TUI: `/login [provider]`, Ctrl+X I and Enter on a locked
//! model-picker column. The event loop calls `command`, `on_action` and
//! `on_event` in one statement each; `handle_input` routes overlay keys to
//! `on_key`. The flow, store reads, validation and the live-layer update run
//! in spawned tasks and report back as `LoginEvent`s.
//!
//! MIT License
//!
//! Copyright (c) 2025 Mario Zechner
//! Copyright (c) 2025-2026 Can Bölük
//! Copyright (c) 2026 Stencil Labs, Inc.
//!
//! Permission is hereby granted, free of charge, to any person obtaining a copy
//! of this software and associated documentation files (the "Software"), to deal
//! in the Software without restriction, including without limitation the rights
//! to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
//! copies of the Software, and to permit persons to whom the Software is
//! furnished to do so, subject to the following conditions:
//!
//! The above copyright notice and this permission notice shall be included in all
//! copies or substantial portions of the Software.
//!
//! THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
//! IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
//! FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
//! AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
//! LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
//! OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
//! SOFTWARE.

use crate::adapters::auth::host::{self, LoginPlan};
use crate::adapters::tui::app::InputAction;
use crate::adapters::tui::login_runtime::{LoginCtx, spawn_targets};
use crate::adapters::tui::login_ui::safe_url;
use crate::adapters::tui::state::{
    FieldKind, LOGIN_PICKER_ROWS, LoginOrigin, LoginSection, LoginView, PendingField, RunningLogin,
    TuiState,
};
use crate::domain::events::{DomainInputEvent, DomainKey, LoginEvent, LoginFailure, LoginSummary};
use crate::domain::models::visual::OverlayType;
use crate::domain::models::{AppConfig, AuthMethod, FeedbackBlock, FeedbackLevel, FocusState};
use crate::domain::ports::ProviderInfoPort;

/// What the event loop's login arm performs for an overlay key or a picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginAction {
    /// The provider picker (`provider: None`), or the method choice / start
    /// for one provider.
    Open {
        provider: Option<String>,
        origin: LoginOrigin,
    },
    /// A provider chosen in the provider picker (Esc in its method picker
    /// returns to that picker).
    Choose { provider: String },
    /// Start `methods[method]` of the provider's plan.
    Start { provider: String, method: usize },
    /// Copy a link or code of the running view.
    Copy(String),
    /// The overlay closed without a login; a picker origin reopens the picker.
    Closed { origin: LoginOrigin },
}

/// Provider text shown in the overlay: terminal controls stripped, length capped.
pub(crate) fn display_text(text: &str) -> String {
    crate::adapters::auth::bounded_provider_text(text)
}

pub(crate) const LOGIN: FocusState = FocusState::Overlay(OverlayType::Login);

/// `/login [provider]` (`ExecuteCommand "login"`).
pub fn command(state: &mut TuiState, ctx: &LoginCtx, app: &AppConfig, args: Option<&str>) {
    let provider = args
        .map(str::trim)
        .filter(|arg| !arg.is_empty())
        .map(str::to_owned);
    open(state, ctx, app, provider, LoginOrigin::Slash);
}

/// The event loop's `InputAction::Login` arm.
pub fn on_action(
    state: &mut TuiState,
    ctx: &LoginCtx,
    app: &AppConfig,
    info: &dyn ProviderInfoPort,
    action: LoginAction,
) {
    match action {
        LoginAction::Open { provider, origin } => open(state, ctx, app, provider, origin),
        LoginAction::Choose { provider } => {
            let origin = state.login.origin.clone();
            begin(state, ctx, app, &provider, origin, true);
        }
        LoginAction::Start { provider, method } => {
            let origin = state.login.origin.clone();
            match host::plan(&provider, app) {
                Ok(plan) => match plan.methods.get(method) {
                    Some(method) => start(state, ctx, app, &plan, method, origin),
                    None => close(state),
                },
                Err(error) => failed_to_start(state, &error.to_string()),
            }
        }
        LoginAction::Copy(text) => {
            super::clipboard::flash_copy(state, &text, &*ctx.copier);
        }
        LoginAction::Closed { origin } => {
            if let LoginOrigin::Picker { column } = origin {
                super::model_switch::open_selector(state, info, app, Some(&column));
            }
        }
    }
}

/// The focus a login overlay returns to: the model picker is closed first when
/// the sign-in started there.
fn take_focus(state: &mut TuiState) -> FocusState {
    if state.focus == FocusState::Overlay(OverlayType::ModelSelector) {
        state.focus = state.model_selector.dismiss().unwrap_or(FocusState::Input);
    }
    state.focus.clone()
}

fn open(
    state: &mut TuiState,
    ctx: &LoginCtx,
    app: &AppConfig,
    provider: Option<String>,
    origin: LoginOrigin,
) {
    if state.login.running.is_some() {
        // One login at a time: the running view keeps (or regains) focus.
        state.focus = LOGIN;
        state.needs_redraw = true;
        return;
    }
    match provider {
        Some(provider) => begin(state, ctx, app, &provider, origin, false),
        None => {
            let previous = take_focus(state);
            let generation = state.login.open_picker(previous, origin);
            state.focus = LOGIN;
            state.needs_redraw = true;
            spawn_targets(ctx.clone(), app.clone(), generation);
        }
    }
}

fn begin(
    state: &mut TuiState,
    ctx: &LoginCtx,
    app: &AppConfig,
    provider: &str,
    origin: LoginOrigin,
    from_picker: bool,
) {
    let plan = match host::plan(provider, app) {
        Ok(plan) => plan,
        Err(error) => return failed_to_start(state, &error.to_string()),
    };
    if plan.keyless {
        close(state);
        feedback(
            state,
            FeedbackLevel::Info,
            host::keyless_line(&plan.display),
        );
        return;
    }
    let previous = take_focus(state);
    if let [method] = plan.methods {
        state.login.previous_focus.get_or_insert(previous);
        return start(state, ctx, app, &plan, method, origin);
    }
    state.login.open_methods(
        previous,
        origin,
        (plan.provider_id.clone(), plan.display.clone()),
        plan.methods,
        from_picker,
    );
    state.focus = LOGIN;
    state.needs_redraw = true;
}

fn failed_to_start(state: &mut TuiState, error: &str) {
    close(state);
    feedback(
        state,
        FeedbackLevel::Error,
        format!("Login failed: {}", display_text(error)),
    );
}

fn close(state: &mut TuiState) {
    if state.login.active {
        let previous = state.login.close();
        if state.focus == LOGIN {
            state.focus = previous.unwrap_or(FocusState::Input);
        }
        // A permission that arrived while the overlay owned focus was queued
        // behind it; surface it now, as closing any other overlay does.
        crate::adapters::tui::app::advance_deferred_permission(state);
    }
    state.needs_redraw = true;
}

fn feedback(state: &mut TuiState, level: FeedbackLevel, message: String) {
    let block = FeedbackBlock {
        id: "login-result".into(),
        level,
        message,
        actions: Vec::new(),
    };
    state.feedback_blocks.insert(block.id.clone(), block);
    state.needs_redraw = true;
}

fn start(
    state: &mut TuiState,
    ctx: &LoginCtx,
    app: &AppConfig,
    plan: &LoginPlan,
    method: &'static AuthMethod,
    origin: LoginOrigin,
) {
    let login_id = state.login.next_login_id();
    let cancel = crate::adapters::tui::login_runtime::spawn_login(ctx, app, login_id, plan, method);
    state.login.origin = origin.clone();
    state.login.start_running(RunningLogin {
        login_id,
        provider_id: plan.provider_id.clone(),
        display: display_text(&plan.display),
        origin,
        cancel: cancel.clone(),
        sections: Vec::new(),
        progress: Vec::new(),
        field: None,
    });
    state.focus = LOGIN;
    state.needs_redraw = true;
}

/// The event loop's `AppEvent::Login` arm.
pub fn on_event(
    state: &mut TuiState,
    app: &AppConfig,
    info: &dyn ProviderInfoPort,
    event: LoginEvent,
) {
    state.needs_redraw = true;
    match event {
        LoginEvent::Targets { generation, rows } => state.login.set_rows(generation, rows),
        LoginEvent::Validated {
            generation,
            provider,
            valid,
        } => state.login.set_validation(generation, &provider, valid),
        LoginEvent::AuthUrl {
            login_id,
            url,
            launch_url,
            instructions,
        } => {
            if let Some(running) = state.login.running_mut(login_id) {
                running.sections.push(LoginSection::AuthUrl {
                    url,
                    launch_url,
                    instructions,
                });
            }
        }
        LoginEvent::DeviceCode {
            login_id,
            verification_url,
            user_code,
            instructions,
        } => {
            if let Some(running) = state.login.running_mut(login_id) {
                running.sections.push(LoginSection::Device {
                    verification_url,
                    user_code,
                    instructions,
                });
            }
        }
        LoginEvent::Progress { login_id, message } => {
            if let Some(running) = state.login.running_mut(login_id) {
                running.progress.push(message);
            }
        }
        LoginEvent::PromptSecret {
            login_id,
            label,
            reply,
        } => {
            if let Some(running) = state.login.running_mut(login_id) {
                running.field = Some(PendingField::secret(label, reply));
            }
        }
        LoginEvent::ManualInput {
            login_id,
            prompt,
            reply,
        } => {
            if let Some(running) = state.login.running_mut(login_id) {
                running.field = Some(PendingField::text(FieldKind::Manual, prompt, reply));
            }
        }
        LoginEvent::PromptLine {
            login_id,
            prompt,
            reply,
        } => {
            if let Some(running) = state.login.running_mut(login_id) {
                running.field = Some(PendingField::text(FieldKind::Line, prompt, reply));
            }
        }
        LoginEvent::Finished {
            login_id,
            provider,
            outcome,
        } => finished(state, app, info, login_id, &provider, outcome),
        LoginEvent::Discovered { provider, line } => {
            if state.model_selector.active {
                state.model_selector.bloom = Some((provider, line));
            }
        }
    }
}

fn finished(
    state: &mut TuiState,
    app: &AppConfig,
    info: &dyn ProviderInfoPort,
    login_id: u64,
    provider: &str,
    outcome: Result<LoginSummary, LoginFailure>,
) {
    if state.login.running.as_ref().map(|r| r.login_id) != Some(login_id) {
        return;
    }
    let running = state.login.running.take();
    close(state);
    let picker_column = match running.map(|running| running.origin) {
        Some(LoginOrigin::Picker { column }) => Some(column),
        _ => None,
    };
    match outcome {
        // A stored credential is reported (and applied) even when the user
        // cancelled after it was persisted.
        Ok(summary) => {
            feedback(state, FeedbackLevel::Info, summary.lines.join("\n"));
            let column = picker_column.unwrap_or_else(|| {
                host::login_targets(app)
                    .into_iter()
                    .find(|target| target.id == provider)
                    .and_then(|target| target.entries.into_iter().next())
                    .unwrap_or_else(|| provider.to_owned())
            });
            super::model_switch::open_selector(state, info, app, Some(&column));
        }
        Err(failure) => {
            match failure {
                LoginFailure::Cancelled => {
                    super::clipboard::flash(state, "Login cancelled".into());
                }
                LoginFailure::Failed(message) => {
                    feedback(
                        state,
                        FeedbackLevel::Error,
                        format!("Login failed: {message}"),
                    );
                }
            }
            if let Some(column) = picker_column {
                super::model_switch::open_selector(state, info, app, Some(&column));
            }
        }
    }
}

/// The text a copy key takes from the latest link/code section: `None` when
/// that target is absent or unsafe (then `Nothing to copy`).
fn copy_target(state: &TuiState, key: char) -> String {
    let Some(section) = state
        .login
        .running
        .as_ref()
        .and_then(|running| running.sections.last())
    else {
        return String::new();
    };
    let target = match (section, key) {
        (LoginSection::AuthUrl { url, .. }, 'c') => safe_url(url),
        (LoginSection::AuthUrl { launch_url, .. }, 'l') => safe_url(launch_url),
        (LoginSection::Device { user_code, .. }, 'c') => (!user_code.is_empty()
            && !user_code.chars().any(char::is_control))
        .then(|| user_code.clone()),
        (
            LoginSection::Device {
                verification_url, ..
            },
            'u',
        ) => safe_url(verification_url),
        _ => None,
    };
    target.unwrap_or_default()
}

fn copy_key(event: &DomainInputEvent, field_focused: bool) -> Option<char> {
    match event {
        DomainInputEvent::SpecialKey(DomainKey::AltC) => Some('c'),
        DomainInputEvent::SpecialKey(DomainKey::AltL) => Some('l'),
        DomainInputEvent::SpecialKey(DomainKey::AltU) => Some('u'),
        DomainInputEvent::KeyPress(c @ ('c' | 'l' | 'u')) if !field_focused => Some(*c),
        _ => None,
    }
}

/// Overlay keys (called by `handle_input` while the overlay has focus).
pub fn on_key(state: &mut TuiState, event: &DomainInputEvent) -> InputAction {
    if matches!(event, DomainInputEvent::SpecialKey(DomainKey::CtrlQ)) {
        return InputAction::Quit;
    }
    state.needs_redraw = true;
    match state.login.view {
        LoginView::ProviderPicker => picker_key(state, event),
        LoginView::MethodPicker => method_key(state, event),
        LoginView::Running => running_key(state, event),
    }
}

fn is_cancel(event: &DomainInputEvent) -> bool {
    matches!(
        event,
        DomainInputEvent::SpecialKey(DomainKey::Esc | DomainKey::CtrlC)
    )
}

fn move_cursor(selected: usize, len: usize, event: &DomainInputEvent) -> usize {
    if len == 0 {
        return 0;
    }
    match event {
        DomainInputEvent::SpecialKey(DomainKey::Up) => (selected + len - 1) % len,
        DomainInputEvent::SpecialKey(DomainKey::Down) => (selected + 1) % len,
        DomainInputEvent::SpecialKey(DomainKey::PageUp) => {
            selected.saturating_sub(LOGIN_PICKER_ROWS)
        }
        DomainInputEvent::SpecialKey(DomainKey::PageDown) => {
            (selected + LOGIN_PICKER_ROWS).min(len - 1)
        }
        _ => selected,
    }
}

fn closed(state: &mut TuiState) -> InputAction {
    let origin = state.login.origin.clone();
    close(state);
    InputAction::Login(LoginAction::Closed { origin })
}

fn picker_key(state: &mut TuiState, event: &DomainInputEvent) -> InputAction {
    let login = &mut state.login;
    match event {
        _ if is_cancel(event) => {
            if login.query.is_empty() {
                return closed(state);
            }
            login.query.clear();
            login.refilter();
        }
        DomainInputEvent::KeyPress(c) if !c.is_control() => {
            login.query.push(*c);
            login.refilter();
        }
        DomainInputEvent::Paste(text) => {
            login.query.extend(text.chars().filter(|c| !c.is_control()));
            login.refilter();
        }
        DomainInputEvent::SpecialKey(DomainKey::Backspace) => {
            login.query.pop();
            login.refilter();
        }
        DomainInputEvent::SpecialKey(DomainKey::Enter) => {
            if let Some(row) = login.selected_row() {
                return InputAction::Login(LoginAction::Choose {
                    provider: row.row.id.clone(),
                });
            }
        }
        _ => login.selected = move_cursor(login.selected, login.filtered.len(), event),
    }
    InputAction::Consumed
}

fn method_key(state: &mut TuiState, event: &DomainInputEvent) -> InputAction {
    let login = &mut state.login;
    match event {
        _ if is_cancel(event) => {
            if login.method_from_picker {
                login.view = LoginView::ProviderPicker;
                return InputAction::Consumed;
            }
            return closed(state);
        }
        DomainInputEvent::SpecialKey(DomainKey::Enter) => {
            if let Some((provider, _)) = &login.target {
                return InputAction::Login(LoginAction::Start {
                    provider: provider.clone(),
                    method: login.method_selected,
                });
            }
        }
        _ => login.method_selected = move_cursor(login.method_selected, login.methods.len(), event),
    }
    InputAction::Consumed
}

fn running_key(state: &mut TuiState, event: &DomainInputEvent) -> InputAction {
    if is_cancel(event) {
        // The flow's biased select ends with `Login cancelled`; its
        // `Finished` closes the overlay.
        state.login.cancel_running();
        return InputAction::Consumed;
    }
    let field_focused = state
        .login
        .running
        .as_ref()
        .is_some_and(|running| running.field.is_some());
    if let Some(key) = copy_key(event, field_focused) {
        return InputAction::Login(LoginAction::Copy(copy_target(state, key)));
    }
    let Some(running) = state.login.running.as_mut() else {
        return InputAction::Consumed;
    };
    let Some(field) = running.field.as_mut() else {
        return InputAction::Consumed;
    };
    match event {
        DomainInputEvent::KeyPress(c) if !c.is_control() => field.buffer.push(*c),
        DomainInputEvent::Paste(text) => {
            // Typed keys are control-free and so is a paste: a wrapped or
            // terminal-mangled paste never puts `\n`, a tab or ESC into a key.
            for c in text.chars().filter(|c| !c.is_control()) {
                field.buffer.push(c);
            }
        }
        DomainInputEvent::SpecialKey(DomainKey::Backspace) => field.buffer.pop(),
        DomainInputEvent::SpecialKey(DomainKey::Enter) => {
            if let Some(field) = running.field.take() {
                field.submit();
            }
        }
        // An image pasted while signing in is dropped, never attached.
        _ => {}
    }
    InputAction::Consumed
}
