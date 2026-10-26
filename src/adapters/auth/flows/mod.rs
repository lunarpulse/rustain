//! Shared authentication engines and the sole login persistence boundary.
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

mod api_key;
#[cfg(any(feature = "anthropic", feature = "openai", feature = "ollama"))]
mod identity;
mod loopback;
#[cfg(any(feature = "anthropic", feature = "openai", feature = "ollama"))]
mod oauth_code;
mod paste;
mod pkce;
#[cfg(any(feature = "anthropic", feature = "openai", feature = "ollama"))]
pub mod refresh;
#[cfg(any(feature = "anthropic", feature = "openai", feature = "ollama"))]
mod token;
pub mod ui;
use crate::domain::{
    models::{AuthMethod, Credential, CredentialIdentity, CredentialOrigin, ProviderConfig},
    ports::AuthStorePort,
};
use std::sync::Arc;
pub use ui::LoginUi;

pub struct FlowCtx {
    pub ui: Arc<dyn LoginUi>,
    pub clock: Arc<dyn Fn() -> i64 + Send + Sync>,
    /// Private: every login host gets the same no-redirect client (`token::client`).
    #[cfg(any(feature = "anthropic", feature = "openai", feature = "ollama"))]
    http: reqwest::Client,
    pub validation_config: Option<ProviderConfig>,
}
impl FlowCtx {
    /// One HTTP client per flow, built here so CLI and TUI hosts share its policy.
    pub fn new(ui: Arc<dyn LoginUi>, clock: Arc<dyn Fn() -> i64 + Send + Sync>) -> Self {
        Self {
            ui,
            clock,
            #[cfg(any(feature = "anthropic", feature = "openai", feature = "ollama"))]
            http: token::client(),
            validation_config: None,
        }
    }
    pub fn with_validation_config(mut self, config: ProviderConfig) -> Self {
        self.validation_config = Some(config);
        self
    }
}
#[derive(Debug, thiserror::Error)]
pub enum FlowError {
    #[error("Login cancelled")]
    Cancelled,
    #[error("Aborted — existing credential unchanged.")]
    Declined,
    #[error("{0}")]
    Message(String),
}
impl FlowError {
    pub(super) fn configuration(message: impl Into<String>) -> Self {
        Self::Message(message.into())
    }
}
#[derive(Debug)]
pub struct LoginResult {
    pub provider: String,
    pub method: &'static str,
    pub validated: bool,
    pub identity: CredentialIdentity,
}

pub async fn login(
    provider_id: &str,
    method: &AuthMethod,
    ctx: &FlowCtx,
    store: &Arc<dyn AuthStorePort>,
) -> Result<LoginResult, FlowError> {
    let cancel = ctx.ui.cancel_token();
    let run = async {
        let (credential, validated) = match method {
            AuthMethod::ApiKey => api_key::run(provider_id, ctx, store).await?,
            AuthMethod::OAuthCode(policy) => {
                #[cfg(any(feature = "anthropic", feature = "openai", feature = "ollama"))]
                {
                    // Bounds the browser/paste wait and exchange, never masked key entry.
                    tokio::time::timeout(
                        std::time::Duration::from_secs(300),
                        oauth_code::run(policy, ctx),
                    )
                    .await
                    .unwrap_or_else(|_| {
                        Err(FlowError::configuration(
                            "Login timed out after 300 seconds",
                        ))
                    })?
                }
                #[cfg(not(any(feature = "anthropic", feature = "openai", feature = "ollama")))]
                {
                    let _ = policy;
                    return Err(FlowError::configuration(
                        "this build has no HTTP client; rebuild with default features",
                    ));
                }
            }
        };
        let identity = match &credential {
            Credential::OAuth(o) => o.identity.clone(),
            Credential::ApiKey(_) => CredentialIdentity::default(),
        };
        store
            .upsert(provider_id, credential, CredentialOrigin::Login, validated)
            .await
            .map_err(|_| FlowError::configuration("Failed to store credential"))?;
        Ok(LoginResult {
            provider: provider_id.into(),
            method: match method {
                AuthMethod::ApiKey => "api_key",
                AuthMethod::OAuthCode(_) => "oauth",
            },
            validated,
            identity,
        })
    };
    tokio::select! {
        biased;
        ()=cancel.cancelled()=>Err(FlowError::Cancelled),
        result=run=>result,
    }
}

#[cfg(all(
    test,
    not(any(feature = "anthropic", feature = "openai", feature = "ollama"))
))]
mod no_http_tests {
    use super::*;
    struct Ui;
    impl LoginUi for Ui {
        fn on_auth_url(&self, _: &str, _: &str, _: &str) {
            panic!("no HTTP build must fail before publishing a URL");
        }
        fn on_progress(&self, _: &str) {}
        fn prompt_secret(
            &self,
            _: &str,
        ) -> futures::future::BoxFuture<'_, Option<crate::domain::models::SecretString>> {
            Box::pin(async { Some("NO-HTTP-KEY".to_owned().into()) })
        }
        fn manual_input(
            &self,
            _: &str,
        ) -> Option<futures::future::BoxFuture<'static, Option<String>>> {
            None
        }
        fn cancel_token(&self) -> tokio_util::sync::CancellationToken {
            tokio_util::sync::CancellationToken::new()
        }
    }
    #[tokio::test]
    async fn no_http_oauth_fails_before_listener_and_api_key_stores_unvalidated() {
        let temp = tempfile::tempdir().unwrap();
        let store: Arc<dyn AuthStorePort> = Arc::new(
            crate::adapters::auth_store::FileAuthStore::at(temp.path().join("auth.json")),
        );
        let context = FlowCtx::new(Arc::new(Ui), Arc::new(|| 0));
        let error = login(
            "anthropic",
            &AuthMethod::OAuthCode(&crate::adapters::auth::policies::ANTHROPIC_OAUTH),
            &context,
            &store,
        )
        .await
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "this build has no HTTP client; rebuild with default features"
        );
        assert!(
            !login("anthropic", &AuthMethod::ApiKey, &context, &store)
                .await
                .unwrap()
                .validated
        );
        assert_eq!(
            store.snapshot().await.unwrap().rows("anthropic")[0].origin,
            CredentialOrigin::Login
        );
    }
}
