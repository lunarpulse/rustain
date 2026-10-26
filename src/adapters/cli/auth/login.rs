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
        auth::flows::{self, FlowCtx, FlowError, LoginResult, LoginUi},
        cli::{
            auth::{providers, terminal_ui::TerminalUi},
            commands::AuthLoginMethod,
        },
    },
    domain::{
        models::{AppConfig, AuthMethod, CredentialIdentity, ProviderConfig},
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
async fn run(
    provider_id: &str,
    requested: Option<AuthLoginMethod>,
    json: bool,
    store: &Arc<dyn AuthStorePort>,
    app_config: &AppConfig,
) -> Result<()> {
    let (display, requires_key, config) = resolve_provider(provider_id, app_config)?;
    if !requires_key {
        if json {
            println!(
                "{}",
                serde_json::json!({"provider":provider_id,"status":"keyless","message":format!("{display} does not require an API key — no credentials to store.")})
            );
        } else {
            println!("{display} does not require an API key — no credentials to store.");
        }
        return Ok(());
    }
    let fallback = [AuthMethod::ApiKey];
    let methods = providers::lookup(provider_id).map_or(&fallback[..], |m| m.auth_methods);
    let method = select_method(methods, requested, std::io::stdin().is_terminal())?;
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
    match result {
        Ok(result) => {
            if json {
                println!("{}", result_json(&result));
            } else if result.method == "oauth" {
                println!("{}", oauth_success(&display, &result.identity));
            } else if result.validated {
                println!("✓ {display} credentials stored successfully.");
            } else {
                println!(
                    "⚠  Could not confirm key via /models endpoint — {display} credentials stored unvalidated."
                );
            }
            eprintln!(
                "Credentials saved to {}",
                crate::infrastructure::paths::data_dir()?
                    .join("auth.json")
                    .display()
            );
            Ok(())
        }
        Err(FlowError::Declined) => {
            eprintln!("Aborted — existing credential unchanged.");
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}
fn select_method(
    methods: &[AuthMethod],
    requested: Option<AuthLoginMethod>,
    tty: bool,
) -> Result<&AuthMethod> {
    select_method_with_input(methods, requested, tty, || {
        let mut value = String::new();
        std::io::stdin().read_line(&mut value)?;
        Ok(value)
    })
}
fn select_method_with_input(
    methods: &[AuthMethod],
    requested: Option<AuthLoginMethod>,
    tty: bool,
    read: impl FnOnce() -> std::io::Result<String>,
) -> Result<&AuthMethod> {
    if let Some(requested) = requested {
        return methods
            .iter()
            .find(|method| {
                matches!(
                    (requested, *method),
                    (AuthLoginMethod::ApiKey, AuthMethod::ApiKey)
                        | (AuthLoginMethod::Oauth, AuthMethod::OAuthCode(_))
                )
            })
            .ok_or_else(|| {
                anyhow::anyhow!("Requested --method is not available for this provider")
            });
    }
    if methods.len() == 1 {
        return Ok(&methods[0]);
    }
    if !tty {
        bail!("Multiple login methods are available; choose --method oauth or --method api-key.");
    }
    eprintln!("Choose a login method:");
    for (index, method) in methods.iter().enumerate() {
        eprintln!(
            "  {}. {}",
            index + 1,
            match method {
                AuthMethod::ApiKey => "API key",
                AuthMethod::OAuthCode(policy) => policy.label,
            }
        );
    }
    eprint!("Method [1-{}]: ", methods.len());
    std::io::stderr().flush()?;
    let value = read()?;
    let index = value
        .trim()
        .parse::<usize>()
        .ok()
        .and_then(|v| v.checked_sub(1))
        .filter(|i| *i < methods.len())
        .ok_or_else(|| anyhow::anyhow!("Invalid login method selection"))?;
    Ok(&methods[index])
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
    serde_json::json!({"provider":result.provider,"status":"authenticated","method":result.method,"validated":result.validated,"identity":identity})
}
pub(super) fn oauth_who(identity: &CredentialIdentity) -> Option<String> {
    let base = identity.email.as_ref().or(identity.account_id.as_ref());
    let org = identity.org_name.as_ref().or(identity.org_id.as_ref());
    let who = match (base, org) {
        (Some(base), Some(org)) => format!("{base} ({org})"),
        (Some(base), None) => base.clone(),
        (None, Some(org)) => org.clone(),
        _ => return None,
    };
    // Provider-supplied identity reaches the terminal; strip controls as other CLI labels do.
    Some(crate::adapters::cli::session::list::sanitize_title(&who))
}
fn oauth_success(display: &str, identity: &CredentialIdentity) -> String {
    let prefix = format!("Logged in to {display} (Claude Pro/Max)");
    oauth_who(identity).map_or_else(|| prefix.clone(), |who| format!("{prefix} as {who}"))
}
fn resolve_provider(provider: &str, app: &AppConfig) -> Result<(String, bool, ProviderConfig)> {
    if let Some(meta) = providers::lookup(provider) {
        // A configured built-in still keeps its custom origin and header policy.
        let config = app
            .provider
            .get(provider)
            .cloned()
            .unwrap_or_else(|| ProviderConfig {
                provider_id: provider.into(),
                model_id: String::new(),
                api_key_env: meta.api_key_env.into(),
                enabled: true,
                kind: Some(provider.into()),
                base_url: None,
                auth_header: None,
                context_window: None,
                supports_tools: None,
                discover_models: false,
                model_filter: vec![],
                cache_ttl_seconds: 0,
            });
        return Ok((meta.display_name.into(), meta.requires_key, config));
    }
    if let Some(config) = app.provider.get(provider) {
        return Ok((provider.into(), true, config.clone()));
    }
    let mut valid: Vec<_> = providers::all_provider_ids().map(str::to_owned).collect();
    for id in app.provider.keys() {
        if !valid.contains(id) {
            valid.push(id.clone());
        }
    }
    bail!(
        "unknown provider '{provider}'. Valid providers: {}",
        valid.join(", ")
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn method_override_and_non_tty_picker_boundary() {
        let methods = providers::lookup("anthropic").unwrap().auth_methods;
        assert!(
            select_method(methods, None, false)
                .unwrap_err()
                .to_string()
                .contains("--method")
        );
        assert!(matches!(
            select_method(methods, Some(AuthLoginMethod::Oauth), false).unwrap(),
            AuthMethod::OAuthCode(_)
        ));
        assert!(matches!(
            select_method(methods, Some(AuthLoginMethod::ApiKey), false).unwrap(),
            AuthMethod::ApiKey
        ));
        assert!(matches!(
            select_method_with_input(methods, None, true, || Ok("1\n".into())).unwrap(),
            AuthMethod::OAuthCode(_)
        ));
        assert!(matches!(
            select_method_with_input(methods, None, true, || Ok("2\n".into())).unwrap(),
            AuthMethod::ApiKey
        ));
        assert!(select_method_with_input(methods, None, true, || Ok("0\n".into())).is_err());
    }
    #[test]
    fn success_identity_and_json_never_contain_credentials() {
        let identity = CredentialIdentity {
            email: Some("me@example.com".into()),
            org_name: Some("My Org".into()),
            ..Default::default()
        };
        assert_eq!(
            oauth_success("Anthropic", &identity),
            "Logged in to Anthropic (Claude Pro/Max) as me@example.com (My Org)"
        );
        let json = result_json(&LoginResult {
            provider: "anthropic".into(),
            method: "oauth",
            validated: true,
            identity,
        });
        assert_eq!(json["method"], "oauth");
        assert!(json["identity"].get("account_id").is_none());
    }
    #[test]
    fn provider_identity_cannot_inject_terminal_controls() {
        let identity = CredentialIdentity {
            email: Some("me@example.com\u{1b}]0;title\u{7}".into()),
            org_name: Some("Org\n  ⚠ forged".into()),
            ..Default::default()
        };
        let line = oauth_success("Anthropic", &identity);
        assert!(!line.chars().any(char::is_control), "{line:?}");
    }
    #[test]
    fn custom_provider_configuration_is_preserved() {
        let app = AppConfig::default();
        assert!(resolve_provider("not-a-provider", &app).is_err());
        assert_eq!(resolve_provider("anthropic", &app).unwrap().0, "Anthropic");
    }
}
