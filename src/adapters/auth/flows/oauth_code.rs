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
use crate::domain::models::{Credential, OAuthCredential, auth_policy::OAuthCodePolicy};

pub(super) async fn run(
    policy: &OAuthCodePolicy,
    ctx: &FlowCtx,
) -> Result<(Credential, bool), FlowError> {
    let mut listener = Loopback::bind(
        crate::adapters::auth::policies::callback_port(policy),
        policy.port_fallback,
        policy.callback_path,
        ctx.ui.as_ref(),
    )
    .await?;
    let (verifier, challenge, state) = super::pkce::generate();
    let client = token::client_id(policy.client_id)?;
    let values = Values {
        client_id: &client,
        redirect: &listener.redirect,
        scope: policy.scope,
        state: &state,
        challenge: &challenge,
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
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
            ("state", state.as_str()),
        ]);
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
        let mut input = if policy.paste {
            ctx.ui.manual_input(policy.paste_prompt)
        } else {
            None
        };
        let code = loop {
            tokio::select! {
                callback=rx.recv()=>break callback.ok_or_else(||FlowError::configuration("OAuth callback listener stopped"))??,
                pasted=async { match input.as_mut() {Some(input)=>input.await,None=>std::future::pending().await} }=>{
                    if let Some(pasted)=pasted {
                        if let Some(code)=super::paste::parse(&pasted,&state) {break code;}
                        ctx.ui.on_progress("Invalid authorization code or state; try again.");
                        input=ctx.ui.manual_input(policy.paste_prompt);
                    } else {input=None;}
                }
            }
        };
        let (code, exchange_state) = code
            .split_once('#')
            .filter(|(_, s)| !s.is_empty())
            .unwrap_or((&code, &state));
        let values = Values {
            client_id: &client,
            redirect: &listener.redirect,
            state: exchange_state,
            code,
            verifier: &verifier,
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
        Ok((
            Credential::OAuth(OAuthCredential {
                access: projected.access,
                refresh: projected.refresh,
                expires_at_ms: projected.expires_at_ms,
                authorized_at_ms: Some(now),
                identity: projected.identity,
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
