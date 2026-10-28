//! Browser PKCE key minting, adapted from oh-my-pi's OAuth-code engine
//! (`ai/src/registry/engine/oauth-code.ts`) and its OpenRouter rule
//! (`catalog/src/compat/rules/auth/openrouter.kdl`).
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

use super::{FlowCtx, FlowError, loopback::Loopback};
use crate::adapters::auth::policies::{callback_port, endpoint};
use crate::domain::models::{Credential, auth_policy::PkceKeyPolicy};
use std::time::Duration;

/// Bounds only the browser/paste wait, as for every browser login; the exchange
/// carries the policy's own timeout.
const WAIT: Duration = Duration::from_secs(300);

/// What the browser wait produced: an authorization code to exchange, or a key
/// the operator pasted instead.
enum Answer {
    Code(String),
    Key(String),
}

/// The exact exchange body, in omp's key order.
#[derive(serde::Serialize)]
struct Exchange<'a> {
    code: &'a str,
    code_verifier: &'a str,
    code_challenge_method: &'static str,
}

pub(super) async fn run(
    provider: &str,
    policy: &PkceKeyPolicy,
    ctx: &FlowCtx,
) -> Result<(Credential, bool), FlowError> {
    // Bind first so the advertised redirect names the port actually bound.
    let mut listener = Loopback::bind(
        callback_port(policy.port),
        policy.port_fallback,
        policy.callback_host,
        policy.callback_path,
        ctx.ui.as_ref(),
    )
    .await?;
    let (verifier, challenge) = super::pkce::pair();
    let mut authorize = url::Url::parse(&endpoint(policy.authorize_url))
        .map_err(|_| FlowError::configuration("Invalid OAuth authorization URL"))?;
    authorize.query_pairs_mut().extend_pairs([
        ("callback_url", listener.redirect.as_str()),
        ("code_challenge", challenge.as_str()),
        ("code_challenge_method", "S256"),
    ]);
    listener.authorize = authorize.into();
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    // No state is sent, so none is expected back.
    let serve = listener.serve(policy.callback_path, "", tx);
    let flow = async {
        ctx.ui
            .on_auth_url(&listener.authorize, &listener.launch, policy.instructions);
        let wait = async {
            let mut input = ctx.ui.manual_input(policy.paste_prompt);
            loop {
                tokio::select! {
                    callback = rx.recv() => {
                        let code = callback.ok_or_else(|| {
                            FlowError::configuration("OAuth callback listener stopped")
                        })??;
                        break Ok(Answer::Code(code));
                    }
                    pasted = async {
                        match input.as_mut() {
                            Some(input) => input.await,
                            None => std::future::pending().await,
                        }
                    } => {
                        let Some(pasted) = pasted else {
                            input = None;
                            continue;
                        };
                        let trimmed = pasted.trim();
                        if trimmed.starts_with(policy.paste_key_prefix) {
                            break Ok(Answer::Key(trimmed.to_owned()));
                        }
                        if let Some(code) = super::paste::parse(trimmed, "") {
                            break Ok(Answer::Code(code));
                        }
                        ctx.ui
                            .on_progress("Invalid authorization code or state; try again.");
                        input = ctx.ui.manual_input(policy.paste_prompt);
                    }
                }
            }
        };
        let answer = tokio::time::timeout(WAIT, wait).await.unwrap_or_else(|_| {
            Err(FlowError::configuration(
                "Login timed out after 300 seconds",
            ))
        })?;
        match answer {
            // A pasted key is validated like `--method api-key`; no exchange.
            Answer::Key(key) => {
                let validated = super::api_key::verify(provider, ctx, &key).await?;
                Ok((Credential::new_api_key(key), validated))
            }
            // The exchange answer is the key; omp does not validate it again.
            Answer::Code(code) => {
                let key = exchange(policy, ctx, &code, &verifier).await?;
                Ok((Credential::new_api_key(key), true))
            }
        }
    };
    tokio::pin!(serve);
    tokio::select! {
        result = flow => result,
        () = &mut serve => Err(FlowError::configuration("OAuth callback listener stopped")),
    }
}

/// `POST exchange_url` with the JSON code/verifier body; a 2xx answer must carry a
/// non-empty string `key`. No response body is ever echoed.
async fn exchange(
    policy: &PkceKeyPolicy,
    ctx: &FlowCtx,
    code: &str,
    verifier: &str,
) -> Result<String, FlowError> {
    let name = policy.login_name;
    let unavailable =
        || FlowError::configuration(format!("{name} key exchange failed: transport unavailable"));
    let body = serde_json::to_vec(&Exchange {
        code,
        code_verifier: verifier,
        code_challenge_method: "S256",
    })
    .map_err(|_| unavailable())?;
    let response = ctx
        .http()
        .post(endpoint(policy.exchange_url))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body)
        .timeout(Duration::from_millis(policy.timeout_ms))
        .send()
        .await
        .map_err(|_| unavailable())?;
    let status = response.status();
    if !status.is_success() {
        return Err(FlowError::configuration(format!(
            "{name} key exchange failed (HTTP {})",
            status.as_u16()
        )));
    }
    let bytes = response.bytes().await.map_err(|_| unavailable())?;
    serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|value| {
            value
                .get("key")
                .and_then(serde_json::Value::as_str)
                .filter(|key| !key.is_empty())
                .map(str::to_owned)
        })
        .ok_or_else(|| FlowError::configuration(format!("{name} key exchange returned no key")))
}

#[cfg(test)]
mod tests {
    use super::super::paste;

    #[test]
    fn a_stateless_flow_ignores_a_pasted_state_and_a_stateful_one_still_checks_it() {
        assert_eq!(paste::parse("CODE#anything", "").as_deref(), Some("CODE"));
        assert_eq!(
            paste::parse("http://localhost:1/callback?code=C&state=x", "").as_deref(),
            Some("C")
        );
        assert_eq!(paste::parse("CODE#wrong", "expected"), None);
        assert_eq!(
            paste::parse("CODE#expected", "expected").as_deref(),
            Some("CODE")
        );
        assert_eq!(paste::parse("CODE", "expected").as_deref(), Some("CODE"));
    }

    #[test]
    fn the_exchange_body_is_exactly_the_three_omp_fields_in_order() {
        let body = serde_json::to_string(&super::Exchange {
            code: "C",
            code_verifier: "V",
            code_challenge_method: "S256",
        })
        .unwrap();
        assert_eq!(
            body,
            r#"{"code":"C","code_verifier":"V","code_challenge_method":"S256"}"#
        );
    }

    /// Paused time: the browser key mint bounds only its callback/paste wait,
    /// at exactly 300 s, with the landed wait-timeout text.
    #[tokio::test(start_paused = true)]
    async fn the_300_second_deadline_bounds_the_browser_wait() {
        use crate::adapters::auth::flows::{FlowCtx, LoginUi};
        use crate::adapters::auth::policies::OPENROUTER_KEY_MINT;
        use crate::domain::models::auth_policy::PkceKeyPolicy;
        use futures::future::BoxFuture;
        use std::sync::Arc;

        struct Silent;
        impl LoginUi for Silent {
            fn on_auth_url(&self, _: &str, _: &str, _: &str) {}
            fn on_device_code(&self, _: &str, _: &str, _: &str) {}
            fn on_progress(&self, _: &str) {}
            fn prompt_secret(
                &self,
                _: &str,
            ) -> BoxFuture<'_, Option<crate::domain::models::SecretString>> {
                Box::pin(async { None })
            }
            fn manual_input(&self, _: &str) -> Option<BoxFuture<'static, Option<String>>> {
                Some(Box::pin(std::future::pending()))
            }
            fn prompt_line(&self, _: &str) -> BoxFuture<'_, Option<String>> {
                Box::pin(async { None })
            }
            fn cancel_token(&self) -> tokio_util::sync::CancellationToken {
                tokio_util::sync::CancellationToken::new()
            }
        }
        static POLICY: PkceKeyPolicy = PkceKeyPolicy {
            port: 0,
            ..OPENROUTER_KEY_MINT
        };
        let ctx = FlowCtx::new(Arc::new(Silent), Arc::new(|| 0));
        let start = tokio::time::Instant::now();
        let error = super::run("openrouter", &POLICY, &ctx).await.unwrap_err();
        assert_eq!(error.to_string(), "Login timed out after 300 seconds");
        assert_eq!(start.elapsed(), super::WAIT);
    }
}
