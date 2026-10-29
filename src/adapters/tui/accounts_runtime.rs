//! Non-spawning `/logout` and `/accounts` work: provider rows, one provider's
//! listing, and a change followed by the existing live-layer update. Handlers
//! return data outcomes; event-loop dispatch launches these asynchronous workers.

use crate::adapters::auth::host;
use crate::adapters::tui::handlers::login::display_text;
use crate::adapters::tui::login_runtime::LoginCtx;
use crate::adapters::tui::state::OverlayMode;
use crate::domain::events::{AccountListRow, AccountListing, AppEvent, LoginEvent, LoginTargetRow};
use crate::domain::models::{AppConfig, CredentialId, FeedbackLevel};
use crate::infrastructure::auth_accounts::{
    self, AccountError, AccountKind, ProviderAccounts, describe,
};

/// The change an account-step Enter performs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Logout,
    Select,
}

fn emit(ctx: &LoginCtx, event: LoginEvent) {
    let _ = ctx.events.emit_domain(AppEvent::Login(event));
}

/// The store could not be read: the overlay closes with an Error block (never
/// as if nothing were stored).
fn unreadable(generation: u64, error: &dyn std::fmt::Display) -> LoginEvent {
    LoginEvent::AccountsChanged {
        generation,
        level: FeedbackLevel::Error,
        lines: vec![format!(
            "Could not read stored accounts: {}",
            display_text(&error.to_string())
        )],
        listing: None,
    }
}

/// The account step's view of `accounts`: the `auth accounts` header and
/// `#<id>  <label>  <oauth|api key>  <state>` rows, served first, then by
/// label, then by id.
pub(crate) fn listing(accounts: &ProviderAccounts) -> AccountListing {
    let mut rows: Vec<_> = accounts.rows.iter().collect();
    rows.sort_by(|a, b| {
        b.served
            .cmp(&a.served)
            .then_with(|| a.label.cmp(&b.label))
            .then_with(|| a.id.0.cmp(&b.id.0))
    });
    AccountListing {
        provider: accounts.provider.clone(),
        display: display_text(&accounts.display_name),
        header: display_text(&host::account_header(accounts)),
        rows: rows
            .into_iter()
            .map(|row| {
                let badge = match row.kind {
                    AccountKind::OAuth => "oauth",
                    AccountKind::ApiKey => "api key",
                };
                let text = format!(
                    "#{}  {}  {badge}  {}",
                    row.id.0,
                    row.label,
                    host::row_state(row)
                );
                AccountListRow {
                    id: row.id.0,
                    text: text.trim_end().to_owned(),
                    served: row.served,
                }
            })
            .collect(),
    }
}

/// The provider step: every provider with stored rows, from one snapshot.
pub async fn run_providers(ctx: LoginCtx, app: AppConfig, generation: u64) {
    let snapshot = match ctx.store.snapshot().await {
        Ok(snapshot) => snapshot,
        Err(error) => return emit(&ctx, unreadable(generation, &error)),
    };
    let rows = host::stored_accounts(
        &snapshot,
        &app,
        &crate::infrastructure::utils::env_var_trimmed,
    )
    .into_iter()
    .map(|accounts| {
        let source = host::source_label(&accounts);
        LoginTargetRow {
            search: format!("{} {}", accounts.provider, source.as_deref().unwrap_or("")),
            display: display_text(&accounts.display_name),
            status: host::status_text(&accounts),
            id: accounts.provider,
            source,
        }
    })
    .collect();
    emit(&ctx, LoginEvent::Targets { generation, rows });
}

/// The account step of `provider`, or the one line that closes the overlay.
pub async fn run_listing(
    ctx: LoginCtx,
    app: AppConfig,
    generation: u64,
    mode: OverlayMode,
    provider: String,
) {
    let closing = |level, line: String| LoginEvent::AccountsChanged {
        generation,
        level,
        lines: vec![line],
        listing: None,
    };
    let snapshot = match ctx.store.snapshot().await {
        Ok(snapshot) => snapshot,
        Err(error) => return emit(&ctx, unreadable(generation, &error)),
    };
    let accounts = describe(
        &snapshot,
        &app,
        &provider,
        &crate::infrastructure::utils::env_var_trimmed,
    );
    let event = if !accounts.rows.is_empty() {
        LoginEvent::Accounts {
            generation,
            listing: listing(&accounts),
        }
    } else if !host::known_provider(&app, &provider) {
        closing(
            FeedbackLevel::Warning,
            format!("Unknown provider: {}", display_text(&provider)),
        )
    } else {
        let display = display_text(&accounts.display_name);
        let line = if mode == OverlayMode::Logout {
            format!("No stored credentials for {display}. Remove env or config auth at its source.")
        } else {
            format!(
                "No stored credentials for {display}. Sign in with /login {}.",
                display_text(&provider)
            )
        };
        closing(FeedbackLevel::Info, line)
    };
    emit(&ctx, event);
}

/// One logout or selection: the change, then phase 1 of the live layer (the
/// router and registry relock or re-serve the provider and the locked warning
/// is posted) before the result closes the overlay, then phase 2 discovery.
pub async fn run_change(
    ctx: LoginCtx,
    app: AppConfig,
    generation: u64,
    change: Change,
    provider: String,
    id: u64,
) {
    let id = CredentialId(id);
    let before =
        crate::adapters::auth_store::FileAuthStore::try_snapshot_sync().unwrap_or_default();
    let env = crate::infrastructure::utils::env_var_trimmed;
    let result = match change {
        Change::Logout => auth_accounts::logout_account(&ctx.store, &app, &provider, id).await,
        Change::Select => auth_accounts::select_account(&ctx.store, &app, &provider, id).await,
    };
    let after = match result {
        Ok(after) => after,
        Err(error) => {
            return emit(&ctx, failed(&ctx, &app, generation, &provider, error).await);
        }
    };
    let lines = match change {
        // The label is read before the row went, as `auth logout --id` does.
        Change::Logout => host::logout_lines(
            &after,
            id,
            describe(&before, &app, &provider, &env)
                .row(id)
                .map(|row| row.label.clone()),
        ),
        Change::Select => host::select_lines(&after, id),
    };
    let targets = ctx.live.register_change(&provider, &app, &before).await;
    emit(
        &ctx,
        LoginEvent::AccountsChanged {
            generation,
            level: FeedbackLevel::Info,
            lines: lines.iter().map(|line| display_text(line)).collect(),
            listing: None,
        },
    );
    // Discovery runs silently; a report with a notice announces itself.
    let _ = ctx.live.refresh_changed(targets).await;
}

/// A stale or disabled row keeps the step open with fresh rows; a store
/// failure closes it.
async fn failed(
    ctx: &LoginCtx,
    app: &AppConfig,
    generation: u64,
    provider: &str,
    error: AccountError,
) -> LoginEvent {
    let fresh = auth_accounts::accounts(&ctx.store, app, provider)
        .await
        .ok();
    let display = display_text(
        fresh
            .as_ref()
            .map_or(provider, |accounts| accounts.display_name.as_str()),
    );
    let (level, line, keep) = match error {
        AccountError::NotFound { id, .. } => (
            FeedbackLevel::Warning,
            format!("Credential #{} for {display} no longer exists.", id.0),
            true,
        ),
        AccountError::Disabled { id, cause, .. } => (
            FeedbackLevel::Warning,
            format!(
                "Credential #{} for {display} is disabled ({}); log in again.",
                id.0,
                display_text(&cause)
            ),
            true,
        ),
        AccountError::Store(error) => (
            FeedbackLevel::Error,
            format!(
                "Account update failed: {}",
                display_text(&error.to_string())
            ),
            false,
        ),
    };
    LoginEvent::AccountsChanged {
        generation,
        level,
        lines: vec![line],
        listing: fresh.filter(|_| keep).map(|accounts| listing(&accounts)),
    }
}
