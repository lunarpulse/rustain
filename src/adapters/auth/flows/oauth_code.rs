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

use super::{
    FlowCtx, FlowError,
    loopback::Loopback,
    token::{self, Values},
};
use crate::domain::models::{
    Credential, OAuthCredential,
    auth_policy::{OAuthClient, OAuthCodePolicy, PostExchangeHook},
};
use std::time::Duration;

/// The callback server's deadline bounds only the browser/paste wait; the exchange,
/// identity and post-exchange requests run after it under their own bounds.
const WAIT: Duration = Duration::from_secs(300);

pub(super) async fn run(
    policy: &OAuthCodePolicy,
    ctx: &FlowCtx,
) -> Result<(Credential, bool), FlowError> {
    let mut listener = Loopback::bind(
        crate::adapters::auth::policies::callback_port(policy.port),
        policy.port_fallback,
        policy.callback_host,
        policy.callback_path,
        ctx.ui.as_ref(),
    )
    .await?;
    let state = super::pkce::state();
    let (pkce, secret) = match policy.client {
        OAuthClient::PublicPkce => (Some(super::pkce::pair()), None),
        OAuthClient::Confidential { secret } => (None, Some(token::client_secret(secret)?)),
    };
    let (verifier, challenge) = pkce.as_ref().map_or(("", ""), |(verifier, challenge)| {
        (verifier.as_str(), challenge.as_str())
    });
    let client = token::client_id(policy.client_id)?;
    let values = Values {
        client_id: &client,
        redirect: &listener.redirect,
        scope: policy.scope,
        state: &state,
        challenge,
        ..Default::default()
    };
    let mut authorize = url::Url::parse(&crate::adapters::auth::policies::endpoint(
        policy.authorize_url,
    ))
    .map_err(|_| FlowError::configuration("Invalid OAuth authorization URL"))?;
    {
        let mut query = authorize.query_pairs_mut();
        query.extend_pairs([
            ("client_id", client.as_str()),
            ("response_type", "code"),
            ("redirect_uri", listener.redirect.as_str()),
            ("scope", policy.scope),
        ]);
        if pkce.is_some() {
            query.extend_pairs([
                ("code_challenge", challenge),
                ("code_challenge_method", "S256"),
            ]);
        }
        query.append_pair("state", &state);
        for (key, tpl) in policy.authorize_extra {
            query.append_pair(key, values.render(*tpl));
        }
    }
    listener.authorize = authorize.into();
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let serve = listener.serve(policy.callback_path, &state, tx);
    let exchange = async {
        ctx.ui
            .on_auth_url(&listener.authorize, &listener.launch, policy.instructions);
        let wait = async {
            let mut input = if policy.paste {
                ctx.ui.manual_input(policy.paste_prompt)
            } else {
                None
            };
            loop {
                tokio::select! {
                    callback=rx.recv()=>break callback.ok_or_else(||FlowError::configuration("OAuth callback listener stopped"))?,
                    pasted=async { match input.as_mut() {Some(input)=>input.await,None=>std::future::pending().await} }=>{
                        if let Some(pasted)=pasted {
                            if let Some(code)=super::paste::parse(&pasted,&state) {break Ok(code);}
                            ctx.ui.on_progress("Invalid authorization code or state; try again.");
                            input=ctx.ui.manual_input(policy.paste_prompt);
                        } else {input=None;}
                    }
                }
            }
        };
        let code = tokio::time::timeout(WAIT, wait).await.unwrap_or_else(|_| {
            Err(FlowError::configuration(
                "Login timed out after 300 seconds",
            ))
        })?;
        let (code, exchange_state) = code
            .split_once('#')
            .filter(|(_, s)| !s.is_empty())
            .unwrap_or((&code, &state));
        let values = Values {
            client_id: &client,
            client_secret: secret.as_ref().map(token::grant_secret),
            redirect: &listener.redirect,
            state: exchange_state,
            code,
            verifier,
            ..Default::default()
        };
        let url = token::resolve_url(&ctx.http, policy.exchange.url)
            .await
            .map_err(FlowError::Message)?;
        let response = token::request(
            &ctx.http,
            policy.exchange,
            &url,
            token::Grant::AuthorizationCode,
            &values,
        )
        .await
        .map_err(|e| FlowError::configuration(e.safe_message("OAuth token exchange failed")))?
        .body;
        let now = (ctx.clock)();
        let mut projected = token::project(&response, policy.projection, now)?;
        super::identity::fill(
            &ctx.http,
            policy.identity_hook,
            &projected.access,
            &response,
            &mut projected.identity,
            false,
        )
        .await?;
        match policy.post_exchange {
            Some(PostExchangeHook::GeminiCliProject) => {
                crate::adapters::auth::gemini_cli_project::run(
                    &ctx.http,
                    ctx,
                    &response,
                    &projected.access,
                    &mut projected.identity,
                )
                .await?;
            }
            None => {}
        }
        Ok((
            Credential::OAuth(OAuthCredential {
                access: projected.access,
                refresh: projected.refresh,
                expires_at_ms: projected.expires_at_ms,
                authorized_at_ms: Some(now),
                identity: projected.identity,
                routing: Default::default(),
            }),
            true,
        ))
    };
    tokio::pin!(serve);
    tokio::select! {
        result=exchange=>result,
        ()=&mut serve=>Err(FlowError::configuration("OAuth callback listener stopped")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::auth::flows::LoginUi;
    use crate::adapters::auth::policies::GEMINI_CLI_OAUTH;
    use crate::domain::models::auth_policy::{BodyEncoding, TokenRequestPolicy, TokenUrl};
    use futures::future::BoxFuture;
    use std::sync::Arc;
    use tokio::sync::{Mutex, oneshot};

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

    /// Pastes one code when the test delivers it.
    struct Paste(Mutex<Option<oneshot::Receiver<String>>>);
    impl LoginUi for Paste {
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
            let pasted = self.0.try_lock().ok()?.take()?;
            Some(Box::pin(async move { pasted.await.ok() }))
        }
        fn prompt_line(&self, _: &str) -> BoxFuture<'_, Option<String>> {
            Box::pin(async { None })
        }
        fn cancel_token(&self) -> tokio_util::sync::CancellationToken {
            tokio_util::sync::CancellationToken::new()
        }
    }

    /// Paused time: only the callback/paste wait is bounded, at exactly 300 s; the
    /// exchange that follows it is outside this deadline.
    #[tokio::test(start_paused = true)]
    async fn the_300_second_deadline_bounds_only_the_callback_wait() {
        static POLICY: OAuthCodePolicy = OAuthCodePolicy {
            port: 0,
            ..GEMINI_CLI_OAUTH
        };
        let ctx = FlowCtx::new(Arc::new(Silent), Arc::new(|| 0));
        let start = tokio::time::Instant::now();
        let error = run(&POLICY, &ctx).await.unwrap_err();
        assert_eq!(error.to_string(), "Login timed out after 300 seconds");
        assert_eq!(start.elapsed(), WAIT);
    }

    /// Paused time: a code pasted at 299 s is exchanged past the 300 s mark, and
    /// the exchange ends under its own 30 s request bound, never the wait deadline.
    #[tokio::test(start_paused = true)]
    #[serial_test::serial]
    async fn a_late_code_is_exchanged_past_the_300_second_deadline() {
        use std::sync::LazyLock;
        // Takes connections into its backlog and never answers them.
        static HELD: LazyLock<std::net::TcpListener> = LazyLock::new(|| {
            std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap()
        });
        static URL: LazyLock<String> =
            LazyLock::new(|| format!("http://{}/token", HELD.local_addr().unwrap()));
        static EXCHANGE: LazyLock<TokenRequestPolicy> = LazyLock::new(|| TokenRequestPolicy {
            url: TokenUrl::Static(URL.as_str()),
            encoding: BodyEncoding::Form,
            timeout_ms: 30_000,
            headers: &[],
            extra_params: &[],
        });
        let policy = OAuthCodePolicy {
            port: 0,
            exchange: &EXCHANGE,
            identity_hook: None,
            post_exchange: None,
            ..GEMINI_CLI_OAUTH
        };
        let (paste, pasted) = oneshot::channel();
        let ctx = FlowCtx::new(Arc::new(Paste(Mutex::new(Some(pasted)))), Arc::new(|| 0));
        let start = tokio::time::Instant::now();
        let deliver = async {
            tokio::time::advance(Duration::from_secs(299)).await;
            paste.send("LATE-CODE".to_owned()).unwrap();
        };
        let (result, ()) = tokio::join!(run(&policy, &ctx), deliver);
        let error = result.unwrap_err();
        assert_ne!(error.to_string(), "Login timed out after 300 seconds");
        assert!(start.elapsed() > WAIT, "{error}");
    }
}
