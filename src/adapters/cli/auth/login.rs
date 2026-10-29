//! CLI method selection and rendering; protocol and persistence live in flows.
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

use crate::{
    adapters::{
        auth::{
            flows::{self, FlowCtx, LoginResult, LoginUi},
            host::{
                LoginPlan, active_line, credentials_saved_line, keyless_line, plan, success_line,
            },
        },
        cli::{auth::terminal_ui::TerminalUi, commands::AuthLoginMethod},
    },
    domain::{
        models::{AppConfig, AuthMethod},
        ports::AuthStorePort,
    },
};
use anyhow::{Result, bail};
use std::{
    io::{IsTerminal, Write},
    sync::Arc,
};

pub async fn run_auth_login(
    provider_id: String,
    method: Option<AuthLoginMethod>,
    json: bool,
    store: &Arc<dyn AuthStorePort>,
    app_config: &AppConfig,
) -> Result<()> {
    let result = run(&provider_id, method, json, store, app_config).await;
    if let Err(error) = &result {
        eprintln!("{error}");
    }
    result
}
/// `rustain auth login` end to end, shared by its startup arm and the
/// `rustain init` sign-in step: the login (which prints its own error once),
/// then, on success, the provider's rediscovery and its discovery line.
pub async fn login_and_rediscover(
    provider: &str,
    method: Option<AuthLoginMethod>,
    json: bool,
    store: &Arc<dyn AuthStorePort>,
    app_config: &AppConfig,
) -> Result<()> {
    let before =
        crate::adapters::auth_store::FileAuthStore::try_snapshot_sync().unwrap_or_default();
    let result = run_auth_login(provider.to_owned(), method, json, store, app_config).await;
    if result.is_ok() {
        super::rediscover(provider, app_config, &before).await;
    }
    result
}
async fn run(
    provider_id: &str,
    requested: Option<AuthLoginMethod>,
    json: bool,
    store: &Arc<dyn AuthStorePort>,
    app_config: &AppConfig,
) -> Result<()> {
    let LoginPlan {
        display,
        keyless,
        methods,
        validation_config: config,
        ..
    } = plan(provider_id, app_config)?;
    if keyless {
        if json {
            println!(
                "{}",
                serde_json::json!({"provider":provider_id,"status":"keyless","message":keyless_line(&display)})
            );
        } else {
            println!("{}", keyless_line(&display));
        }
        return Ok(());
    }
    let method = select_method(methods, requested, std::io::stdin().is_terminal()).await?;
    if matches!(method, AuthMethod::ApiKey) && !std::io::stdin().is_terminal() {
        bail!(
            "auth login requires an interactive terminal (stdin is not a TTY). Set {} instead, or pipe through a TTY.",
            config.api_key_env
        );
    }
    let ui = TerminalUi::shared();
    let clock = Arc::new(crate::infrastructure::utils::auth_now_ms);
    let context = FlowCtx::new(ui.clone(), clock).with_validation_config(config);
    let login = flows::login(provider_id, method, &context, store);
    tokio::pin!(login);
    let result = tokio::select! {
        biased;
        _=tokio::signal::ctrl_c()=>{ui.cancel_token().cancel();login.await},
        result=&mut login=>result,
    };
    let result = result?;
    if json {
        println!("{}", result_json(&result));
    } else {
        println!("{}", success_line(method, &result, &display));
    }
    eprintln!("{}", credentials_saved_line()?);
    // Several accounts: name the one this login made active and how to change
    // it. The credential is already stored, so an unreadable store only skips
    // this hint and never fails the login.
    let Ok(accounts) =
        crate::infrastructure::auth_accounts::accounts(store, app_config, provider_id).await
    else {
        return Ok(());
    };
    if accounts
        .rows
        .iter()
        .filter(|row| row.disabled_cause.is_none())
        .count()
        > 1
    {
        eprintln!("{}", active_line(&accounts, result.credential_id));
        eprintln!(
            "Switch with `rustain auth use {provider_id} --id <N>`; remove one with `rustain auth logout {provider_id} --id <N>`."
        );
    }
    Ok(())
}
/// The method a `--method` flag or a single method decides; `None` when the
/// menu must ask.
fn decided_method(
    methods: &[AuthMethod],
    requested: Option<AuthLoginMethod>,
    tty: bool,
) -> Result<Option<&AuthMethod>> {
    if let Some(requested) = requested {
        return methods
            .iter()
            .find(|method| {
                matches!(
                    (requested, *method),
                    (AuthLoginMethod::ApiKey, AuthMethod::ApiKey)
                        | (
                            AuthLoginMethod::Oauth,
                            AuthMethod::OAuthCode(_) | AuthMethod::PkceKey(_)
                        )
                        | (AuthLoginMethod::Device, AuthMethod::Device(_))
                )
            })
            .map(Some)
            .ok_or_else(|| {
                anyhow::anyhow!("Requested --method is not available for this provider")
            });
    }
    if methods.len() == 1 {
        return Ok(Some(&methods[0]));
    }
    if !tty {
        bail!(
            "Multiple login methods are available; choose {}.",
            method_choices(methods)
        );
    }
    Ok(None)
}
/// The flag's or the only method, else the menu's choice read through the
/// process's one terminal-input owner.
async fn select_method(
    methods: &[AuthMethod],
    requested: Option<AuthLoginMethod>,
    tty: bool,
) -> Result<&AuthMethod> {
    if let Some(method) = decided_method(methods, requested, tty)? {
        return Ok(method);
    }
    print_method_menu(methods)?;
    let value = super::stdin_lines::line().await.unwrap_or_default();
    parse_method_choice(&value, methods)
}
fn print_method_menu(methods: &[AuthMethod]) -> Result<()> {
    eprintln!("Choose a login method:");
    for (index, method) in methods.iter().enumerate() {
        eprintln!("  {}. {}", index + 1, method.label());
    }
    eprint!("Method [1-{}]: ", methods.len());
    std::io::stderr().flush()?;
    Ok(())
}
/// A menu answer: `1`..`N`; anything else (empty or end of input included)
/// is `Invalid login method selection`.
fn parse_method_choice<'a>(value: &str, methods: &'a [AuthMethod]) -> Result<&'a AuthMethod> {
    let index = value
        .trim()
        .parse::<usize>()
        .ok()
        .and_then(|v| v.checked_sub(1))
        .filter(|i| *i < methods.len())
        .ok_or_else(|| anyhow::anyhow!("Invalid login method selection"))?;
    Ok(&methods[index])
}
/// `--method a or --method b` (`, ` before earlier choices), in `auth_methods` order.
fn method_choices(methods: &[AuthMethod]) -> String {
    let choices: Vec<_> = methods
        .iter()
        .map(|method| format!("--method {}", super::list::auth_method_token(method)))
        .collect();
    match choices.split_last() {
        Some((last, rest)) if !rest.is_empty() => format!("{} or {last}", rest.join(", ")),
        _ => choices.concat(),
    }
}
fn result_json(result: &LoginResult) -> serde_json::Value {
    let mut identity = serde_json::Map::new();
    for (name, value) in [
        ("account_id", &result.identity.account_id),
        ("email", &result.identity.email),
        ("project_id", &result.identity.project_id),
        ("org_id", &result.identity.org_id),
        ("org_name", &result.identity.org_name),
    ] {
        if let Some(value) = value {
            identity.insert(name.into(), value.clone().into());
        }
    }
    serde_json::json!({"provider":result.provider,"status":"authenticated","method":result.method,"validated":result.validated,"identity":identity,"id":result.credential_id.0})
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::auth::host::{oauth_success, resolve_provider};
    use crate::adapters::cli::auth::providers;
    use crate::domain::models::CredentialIdentity;
    use crate::domain::models::auth_policy::{DeviceFlow, DevicePolicy};
    static DEVICE: DevicePolicy = DevicePolicy {
        label: "Subscription (device code)",
        login_name: "Device Product",
        flow: DeviceFlow::OpenAiCodex,
    };
    /// The decision without a menu read (the menu path is `parse_method_choice`).
    fn select(
        methods: &[AuthMethod],
        requested: Option<AuthLoginMethod>,
        tty: bool,
    ) -> Result<&AuthMethod> {
        decided_method(methods, requested, tty)?.ok_or_else(|| anyhow::anyhow!("menu"))
    }
    #[test]
    fn method_override_and_non_tty_picker_boundary() {
        let methods = providers::lookup("anthropic").unwrap().auth_methods;
        assert_eq!(
            select(methods, None, false).unwrap_err().to_string(),
            "Multiple login methods are available; choose --method oauth or --method api-key."
        );
        assert!(select(methods, Some(AuthLoginMethod::Device), false).is_err());
        assert!(matches!(
            select(methods, Some(AuthLoginMethod::Oauth), false).unwrap(),
            AuthMethod::OAuthCode(_)
        ));
        assert!(matches!(
            select(methods, Some(AuthLoginMethod::ApiKey), false).unwrap(),
            AuthMethod::ApiKey
        ));
        assert!(decided_method(methods, None, true).unwrap().is_none());
        assert!(matches!(
            parse_method_choice("1\n", methods).unwrap(),
            AuthMethod::OAuthCode(_)
        ));
        assert!(matches!(
            parse_method_choice("2\n", methods).unwrap(),
            AuthMethod::ApiKey
        ));
        assert!(parse_method_choice("0\n", methods).is_err());
        assert_eq!(
            parse_method_choice("", methods).unwrap_err().to_string(),
            "Invalid login method selection"
        );
    }
    #[test]
    fn success_identity_and_json_never_contain_credentials() {
        let identity = CredentialIdentity {
            email: Some("me@example.com".into()),
            org_name: Some("My Org".into()),
            ..Default::default()
        };
        assert_eq!(
            oauth_success(
                crate::adapters::auth::policies::ANTHROPIC_OAUTH.login_name,
                &identity
            ),
            "Logged in to Anthropic (Claude Pro/Max) as me@example.com (My Org)"
        );
        let json = result_json(&LoginResult {
            provider: "anthropic".into(),
            method: "oauth",
            validated: true,
            identity,
            credential_id: crate::domain::models::CredentialId(5),
        });
        assert_eq!(json["method"], "oauth");
        assert_eq!(json["id"], 5);
        assert!(json["identity"].get("account_id").is_none());
    }
    #[test]
    fn codex_success_uses_product_name_and_single_method_needs_no_picker() {
        let methods = providers::lookup("openai-codex").unwrap().auth_methods;
        let method = select(methods, Some(AuthLoginMethod::Oauth), false).unwrap();
        assert!(matches!(method, AuthMethod::OAuthCode(_)));
        let login_name = method.login_name().unwrap();
        let identity = CredentialIdentity {
            email: Some("user@example.com".into()),
            org_name: Some("plus".into()),
            ..Default::default()
        };
        assert_eq!(
            oauth_success(login_name, &identity),
            "Logged in to ChatGPT Plus/Pro (Codex Subscription) as user@example.com (plus)"
        );
        assert_eq!(
            oauth_success(login_name, &CredentialIdentity::default()),
            "Logged in to ChatGPT Plus/Pro (Codex Subscription)"
        );
        assert!(select(methods, Some(AuthLoginMethod::ApiKey), false).is_err());
        let single = [AuthMethod::Device(&DEVICE)];
        assert!(matches!(
            select(&single, None, false).unwrap(),
            AuthMethod::Device(_)
        ));
        assert_eq!(AuthMethod::ApiKey.login_name(), None);
    }
    #[test]
    fn device_method_is_selected_by_flag_picker_label_and_named_on_success() {
        let methods = [
            providers::lookup("anthropic").unwrap().auth_methods[0].clone(),
            AuthMethod::Device(&DEVICE),
        ];
        assert!(matches!(
            select(&methods, Some(AuthLoginMethod::Device), false).unwrap(),
            AuthMethod::Device(_)
        ));
        assert!(matches!(
            select(&methods, Some(AuthLoginMethod::Oauth), false).unwrap(),
            AuthMethod::OAuthCode(_)
        ));
        assert_eq!(
            select(&methods, None, false).unwrap_err().to_string(),
            "Multiple login methods are available; choose --method oauth or --method device."
        );
        let picked = parse_method_choice("2\n", &methods).unwrap();
        assert_eq!(picked.label(), "Subscription (device code)");
        assert_eq!(
            oauth_success(picked.login_name().unwrap(), &CredentialIdentity::default()),
            "Logged in to Device Product"
        );
    }
    #[test]
    fn openrouter_offers_browser_minting_under_oauth_and_names_the_product_on_success() {
        let methods = providers::lookup("openrouter").unwrap().auth_methods;
        assert_eq!(
            select(methods, None, false).unwrap_err().to_string(),
            "Multiple login methods are available; choose --method oauth or --method api-key."
        );
        let minted = select(methods, Some(AuthLoginMethod::Oauth), false).unwrap();
        assert!(matches!(minted, AuthMethod::PkceKey(_)));
        assert_eq!(minted.label(), "OpenRouter account (browser sign-in)");
        assert!(matches!(
            select(methods, Some(AuthLoginMethod::ApiKey), false).unwrap(),
            AuthMethod::ApiKey
        ));
        assert!(select(methods, Some(AuthLoginMethod::Device), false).is_err());
        // Callback and pasted-key outcomes alike, validated or not.
        for validated in [true, false] {
            let result = LoginResult {
                provider: "openrouter".into(),
                method: "browser_key",
                validated,
                identity: CredentialIdentity::default(),
                credential_id: crate::domain::models::CredentialId(2),
            };
            assert_eq!(
                success_line(minted, &result, "OpenRouter"),
                "✓ Logged in to OpenRouter."
            );
        }
        let pasted = LoginResult {
            provider: "openrouter".into(),
            method: "api_key",
            validated: true,
            identity: CredentialIdentity::default(),
            credential_id: crate::domain::models::CredentialId(3),
        };
        assert_eq!(
            success_line(&AuthMethod::ApiKey, &pasted, "OpenRouter"),
            "✓ OpenRouter credentials stored successfully."
        );
    }
    #[test]
    fn provider_identity_cannot_inject_terminal_controls() {
        let identity = CredentialIdentity {
            email: Some("me@example.com\u{1b}]0;title\u{7}".into()),
            org_name: Some("Org\n  ⚠ forged".into()),
            ..Default::default()
        };
        let line = oauth_success(
            crate::adapters::auth::policies::ANTHROPIC_OAUTH.login_name,
            &identity,
        );
        assert!(!line.chars().any(char::is_control), "{line:?}");
    }
    #[test]
    fn custom_provider_configuration_is_preserved() {
        let app = AppConfig::default();
        assert!(resolve_provider("not-a-provider", &app).is_err());
        assert_eq!(resolve_provider("anthropic", &app).unwrap().0, "Anthropic");
    }
}
