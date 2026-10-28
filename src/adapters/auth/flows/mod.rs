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
mod codex_device;
#[cfg(any(feature = "anthropic", feature = "openai", feature = "ollama"))]
mod device_code;
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
use futures::future::BoxFuture;
use std::{sync::Arc, time::Duration};
pub use ui::LoginUi;

/// Injected wait used by device polling; tests record delays and advance `clock`.
pub type Sleeper = Arc<dyn Fn(Duration) -> BoxFuture<'static, ()> + Send + Sync>;

pub struct FlowCtx {
    pub ui: Arc<dyn LoginUi>,
    pub clock: Arc<dyn Fn() -> i64 + Send + Sync>,
    /// Every device-flow wait goes through here; deadlines are computed from `clock`.
    pub sleep: Sleeper,
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
            sleep: Arc::new(real_sleep),
            #[cfg(any(feature = "anthropic", feature = "openai", feature = "ollama"))]
            http: token::client(),
            validation_config: None,
        }
    }
    pub fn with_validation_config(mut self, config: ProviderConfig) -> Self {
        self.validation_config = Some(config);
        self
    }
    pub fn with_sleeper(mut self, sleep: Sleeper) -> Self {
        self.sleep = sleep;
        self
    }
}
fn real_sleep(duration: Duration) -> BoxFuture<'static, ()> {
    let duration = scaled(duration);
    Box::pin(async move {
        if !duration.is_zero() {
            tokio::time::sleep(duration).await;
        }
    })
}
/// Read per use, like the Responses retry knob: 0 removes every wait.
#[cfg(feature = "test-instrumentation")]
fn scaled(duration: Duration) -> Duration {
    let scale = crate::infrastructure::utils::env_var_trimmed("RUSTAIN_TEST_AUTH_SLEEP_SCALE")
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v >= 0.0)
        .unwrap_or(1.0);
    Duration::from_secs_f64((duration.as_secs_f64() * scale).min(86_400.0))
}
#[cfg(not(feature = "test-instrumentation"))]
fn scaled(duration: Duration) -> Duration {
    duration
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
                    // Its 300 s deadline bounds only the browser/paste wait; the
                    // exchange and post-exchange requests carry their own bounds.
                    oauth_code::run(policy, ctx).await?
                }
                #[cfg(not(any(feature = "anthropic", feature = "openai", feature = "ollama")))]
                {
                    let _ = policy;
                    return Err(FlowError::configuration(
                        "this build has no HTTP client; rebuild with default features",
                    ));
                }
            }
            // Self-bounded (Codex: 120 polls; RFC 8628: `expires_in`).
            AuthMethod::Device(policy) => {
                #[cfg(any(feature = "anthropic", feature = "openai", feature = "ollama"))]
                {
                    use crate::domain::models::auth_policy::DeviceFlow;
                    match policy.flow {
                        DeviceFlow::Rfc8628(rfc) => device_code::run(provider_id, rfc, ctx).await?,
                        DeviceFlow::OpenAiCodex => codex_device::run(ctx).await?,
                    }
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
                // The stored row is an OAuth credential for both subscription methods.
                AuthMethod::OAuthCode(_) | AuthMethod::Device(_) => "oauth",
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

#[cfg(all(test, not(feature = "test-instrumentation")))]
mod sleep_tests {
    use super::*;
    #[test]
    #[serial_test::serial]
    fn auth_sleep_knob_is_absent_in_normal_build() {
        struct Restore(Option<std::ffi::OsString>);
        impl Drop for Restore {
            fn drop(&mut self) {
                // SAFETY: the unkeyed serial guard outlives this restoration.
                unsafe {
                    match self.0.take() {
                        Some(v) => std::env::set_var("RUSTAIN_TEST_AUTH_SLEEP_SCALE", v),
                        None => std::env::remove_var("RUSTAIN_TEST_AUTH_SLEEP_SCALE"),
                    }
                }
            }
        }
        let _restore = Restore(std::env::var_os("RUSTAIN_TEST_AUTH_SLEEP_SCALE"));
        // SAFETY: all environment-mutating fixtures take the unkeyed serial lock.
        unsafe {
            std::env::set_var("RUSTAIN_TEST_AUTH_SLEEP_SCALE", "0");
        }
        assert_eq!(
            scaled(Duration::from_millis(5_000)),
            Duration::from_millis(5_000)
        );
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
        fn on_device_code(&self, _: &str, _: &str, _: &str) {
            panic!("no HTTP build must fail before publishing a device code");
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
