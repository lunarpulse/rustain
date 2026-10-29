//! `/logout [provider]` and `/accounts [provider]`: the sign-in overlay's
//! provider step over every provider with stored rows, then one provider's
//! account step (Enter logs out of, or makes active, the row under the cursor;
//! no confirmation, as omp's logout selector). The event loop reaches this
//! module through data-only command/action outcomes; dispatch launches the
//! non-spawning workers in `tui::accounts_runtime`, which report `LoginEvent`s.
//!
//! Account step behaviour adapted from oh-my-pi's logout account selector.
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

use super::login::{LOGIN, LoginAction, close, closed, is_cancel, move_cursor, take_focus};
use super::{AuthWork, HandlerOutcome, SpawnRequest};
use crate::adapters::tui::accounts_runtime::Change;
use crate::adapters::tui::app::InputAction;
use crate::adapters::tui::state::{LoginOrigin, LoginView, OverlayMode, TuiState};
use crate::domain::events::{DomainInputEvent, DomainKey, LoginEvent};
use crate::domain::models::{FeedbackBlock, FeedbackLevel};

/// What the event loop's login arm performs for the account steps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountAction {
    /// Open `provider`'s account step.
    List { provider: String },
    /// Remove stored row `id` of `provider`.
    Logout { provider: String, id: u64 },
    /// Make stored row `id` the active account of `provider`.
    Select { provider: String, id: u64 },
}

/// `/logout [provider]` and `/accounts [provider]` (`ExecuteCommand`); `name`
/// is `logout` or `accounts`.
pub fn command(state: &mut TuiState, name: &str, args: Option<&str>) -> HandlerOutcome {
    if state.login.running.is_some() {
        // One overlay at a time: the running login keeps (or regains) focus.
        state.focus = LOGIN;
        state.needs_redraw = true;
        return HandlerOutcome::Quiet;
    }
    let mode = if name == "logout" {
        OverlayMode::Logout
    } else {
        OverlayMode::Accounts
    };
    let previous = take_focus(state);
    state.focus = LOGIN;
    state.needs_redraw = true;
    match args.map(str::trim).filter(|arg| !arg.is_empty()) {
        Some(provider) => {
            let generation = state
                .login
                .open_accounts(previous, mode, provider.to_owned(), false);
            HandlerOutcome::RequestSpawn(SpawnRequest::Authentication(AuthWork::Listing {
                generation,
                mode,
                provider: provider.to_owned(),
            }))
        }
        None => {
            let generation = state.login.open_picker(previous, LoginOrigin::Slash, mode);
            HandlerOutcome::RequestSpawn(SpawnRequest::Authentication(AuthWork::Providers {
                generation,
            }))
        }
    }
}

/// `LoginAction::Accounts` (delegated by `login::on_action`).
pub fn on_action(state: &mut TuiState, action: AccountAction) -> HandlerOutcome {
    let generation = state.login.generation;
    match action {
        AccountAction::List { provider } => {
            let mode = state.login.mode;
            let from_picker = state.login.view == LoginView::ProviderPicker;
            let previous = state.focus.clone();
            let generation =
                state
                    .login
                    .open_accounts(previous, mode, provider.clone(), from_picker);
            state.needs_redraw = true;
            HandlerOutcome::RequestSpawn(SpawnRequest::Authentication(AuthWork::Listing {
                generation,
                mode,
                provider,
            }))
        }
        AccountAction::Logout { provider, id } => {
            HandlerOutcome::RequestSpawn(SpawnRequest::Authentication(AuthWork::Change {
                generation,
                change: Change::Logout,
                provider,
                id,
            }))
        }
        AccountAction::Select { provider, id } => {
            HandlerOutcome::RequestSpawn(SpawnRequest::Authentication(AuthWork::Change {
                generation,
                change: Change::Select,
                provider,
                id,
            }))
        }
    }
}

fn feedback(state: &mut TuiState, level: FeedbackLevel, message: String) {
    let block = FeedbackBlock {
        id: "accounts-result".into(),
        level,
        message,
        actions: Vec::new(),
    };
    state.feedback_blocks.insert(block.id.clone(), block);
    state.needs_redraw = true;
}

/// The provider step found no stored rows at all.
fn nothing_stored(mode: OverlayMode) -> &'static str {
    match mode {
        OverlayMode::Logout => {
            "No stored provider credentials to log out. Remove env or config auth at its source."
        }
        _ => "No stored provider credentials. Sign in with /login.",
    }
}

/// `Targets` of a logout/accounts provider step, `Accounts` and
/// `AccountsChanged` (delegated by `login::on_event`).
pub fn on_event(state: &mut TuiState, event: LoginEvent) {
    state.needs_redraw = true;
    let current = |state: &TuiState, generation: u64| {
        generation == state.login.generation && state.login.active
    };
    match event {
        LoginEvent::Targets { generation, rows } => {
            if !current(state, generation) {
                return;
            }
            if rows.is_empty() {
                let mode = state.login.mode;
                close(state);
                feedback(state, FeedbackLevel::Info, nothing_stored(mode).into());
                return;
            }
            state.login.set_rows(generation, rows);
        }
        LoginEvent::Accounts {
            generation,
            listing,
        } => state.login.set_accounts(generation, listing, None),
        LoginEvent::AccountsChanged {
            generation,
            level,
            lines,
            listing,
        } => {
            if !current(state, generation) {
                return;
            }
            match listing {
                Some(listing) => {
                    state
                        .login
                        .set_accounts(generation, listing, Some(lines.join("\n")));
                }
                None => {
                    close(state);
                    feedback(state, level, lines.join("\n"));
                }
            }
        }
        _ => {}
    }
}

/// Account-step keys (delegated by `login::on_key`; Ctrl+Q is handled there).
pub fn on_key(state: &mut TuiState, event: &DomainInputEvent) -> InputAction {
    let mode = state.login.mode;
    let Some(step) = state.login.account.as_mut() else {
        return InputAction::Consumed;
    };
    if step.busy {
        return InputAction::Consumed;
    }
    if is_cancel(event) {
        if step.from_picker {
            state.login.back_to_picker();
            return InputAction::Consumed;
        }
        return closed(state);
    }
    let Some(listing) = &step.listing else {
        return InputAction::Consumed;
    };
    match event {
        DomainInputEvent::SpecialKey(DomainKey::Enter) => {
            let Some(row) = listing.rows.get(step.selected) else {
                return InputAction::Consumed;
            };
            let (provider, id) = (listing.provider.clone(), row.id);
            step.busy = true;
            step.notice = None;
            let action = if mode == OverlayMode::Logout {
                AccountAction::Logout { provider, id }
            } else {
                AccountAction::Select { provider, id }
            };
            return InputAction::Login(LoginAction::Accounts(action));
        }
        _ => step.selected = move_cursor(step.selected, listing.rows.len(), event),
    }
    InputAction::Consumed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::FocusState;

    /// Esc on a step opened by id (`/accounts openai`) closes it, even after
    /// a login that began in the model picker: it never reopens the picker.
    #[test]
    fn esc_on_a_step_opened_by_id_never_reopens_the_model_picker() {
        let mut state = TuiState::new(80, 24);
        state.login.origin = LoginOrigin::Picker {
            column: "openai".into(),
        };
        state.login.open_accounts(
            FocusState::Input,
            OverlayMode::Accounts,
            "openai".into(),
            false,
        );
        state.focus = LOGIN;
        assert_eq!(
            crate::adapters::tui::app::handle_input(
                &mut state,
                &DomainInputEvent::SpecialKey(DomainKey::Esc)
            ),
            InputAction::Login(LoginAction::Closed {
                origin: LoginOrigin::Slash
            })
        );
        assert_eq!(state.focus, FocusState::Input);
    }
}
