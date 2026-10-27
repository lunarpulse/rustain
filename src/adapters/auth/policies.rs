//! Static provider protocol rules, adapted from oh-my-pi's authentication policies.
//! The xAI device and refresh rules are adapted from oh-my-pi's catalog rules.
//!
//! Portions adapted from NousResearch/hermes-agent, Copyright (c) 2025 Nous Research (MIT)
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

use crate::domain::models::auth_policy::*;

const CLIENT: ClientId = ClientId::Base64("OWQxYzI1MGEtZTYxYi00NGQ5LTg4ZWQtNTk0NGQxOTYyZjVl");
const EXCHANGE: TokenRequestPolicy = TokenRequestPolicy {
    url: TokenUrl::Static("https://api.anthropic.com/v1/oauth/token"),
    encoding: BodyEncoding::Json,
    timeout_ms: 30_000,
    headers: &[],
    extra_params: &[("state", Tpl::State)],
};
const REFRESH_REQUEST: TokenRequestPolicy = TokenRequestPolicy {
    url: TokenUrl::Static("https://api.anthropic.com/v1/oauth/token"),
    encoding: BodyEncoding::Json,
    timeout_ms: 30_000,
    headers: &[
        ("anthropic-beta", Tpl::Lit("oauth-2025-04-20")),
        (
            "User-Agent",
            Tpl::Lit("anthropic-sdk-typescript/0.112.1 userOAuthProvider"),
        ),
    ],
    extra_params: &[],
};
const LOGIN_PROJECTION: TokenProjection = TokenProjection {
    access: "/access_token",
    refresh: "/refresh_token",
    expires_in: "/expires_in",
    skew_ms: 300_000,
    account_id: Some("/account/uuid"),
    email: Some("/account/email_address"),
    project_id: None,
    org_id: Some("/organization/uuid"),
    org_name: Some("/organization/name"),
};
const REFRESH_PROJECTION: TokenProjection = TokenProjection {
    access: "/access_token",
    refresh: "/refresh_token",
    expires_in: "/expires_in",
    skew_ms: 300_000,
    account_id: Some("/account/uuid"),
    email: Some("/account/email_address"),
    project_id: None,
    org_id: None,
    org_name: None,
};
pub static ANTHROPIC_OAUTH: OAuthCodePolicy = OAuthCodePolicy {
    label: "Claude Pro/Max subscription (browser)",
    login_name: "Anthropic (Claude Pro/Max)",
    instructions: "Complete login in your browser. If the browser cannot reach this machine, paste the final redirect URL or authorization code when prompted.",
    paste_prompt: "Paste the authorization code (or full redirect URL):",
    client_id: CLIENT,
    authorize_url: "https://claude.ai/oauth/authorize",
    scope: "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload",
    authorize_extra: &[("code", Tpl::Lit("true"))],
    port: 54545,
    port_fallback: true,
    callback_path: "/callback",
    paste: true,
    exchange: &EXCHANGE,
    projection: &LOGIN_PROJECTION,
    identity_hook: Some(IdentityHook::AnthropicBootstrap),
};
pub static ANTHROPIC_REFRESH: RefreshPolicy = RefreshPolicy {
    client_id: CLIENT,
    request: &REFRESH_REQUEST,
    projection: &REFRESH_PROJECTION,
    identity_hook: Some(IdentityHook::AnthropicBootstrap),
};
pub static ANTHROPIC_API: OAuthApiPolicy = OAuthApiPolicy {
    api_origin: "https://api.anthropic.com",
    refresh: &ANTHROPIC_REFRESH,
};

pub const CODEX_ORIGINATOR: &str = "omp";
const CODEX_CLIENT: ClientId = ClientId::Literal("app_EMoamEEZ73f0CkXaXp7hrann");
const CODEX_TOKEN_REQUEST: TokenRequestPolicy = TokenRequestPolicy {
    url: TokenUrl::Static("https://auth.openai.com/oauth/token"),
    encoding: BodyEncoding::Form,
    timeout_ms: 15_000,
    headers: &[],
    extra_params: &[],
};
const CODEX_PROJECTION: TokenProjection = TokenProjection {
    access: "/access_token",
    refresh: "/refresh_token",
    expires_in: "/expires_in",
    skew_ms: 0,
    account_id: None,
    email: None,
    project_id: None,
    org_id: None,
    org_name: None,
};
pub static CODEX_OAUTH: OAuthCodePolicy = OAuthCodePolicy {
    label: "ChatGPT Plus/Pro subscription (browser)",
    login_name: "ChatGPT Plus/Pro (Codex Subscription)",
    instructions: "A browser window should open. Complete login to finish.",
    paste_prompt: "Paste the authorization code (or full redirect URL):",
    client_id: CODEX_CLIENT,
    authorize_url: "https://auth.openai.com/oauth/authorize",
    scope: "openid profile email offline_access api.connectors.read api.connectors.invoke",
    authorize_extra: &[
        ("id_token_add_organizations", Tpl::Lit("true")),
        ("codex_cli_simplified_flow", Tpl::Lit("true")),
        ("originator", Tpl::Lit(CODEX_ORIGINATOR)),
    ],
    port: 1455,
    port_fallback: false,
    callback_path: "/auth/callback",
    paste: true,
    exchange: &CODEX_TOKEN_REQUEST,
    projection: &CODEX_PROJECTION,
    identity_hook: Some(IdentityHook::CodexJwt),
};
pub static CODEX_REFRESH: RefreshPolicy = RefreshPolicy {
    client_id: CODEX_CLIENT,
    request: &CODEX_TOKEN_REQUEST,
    projection: &CODEX_PROJECTION,
    identity_hook: Some(IdentityHook::CodexJwt),
};
pub static CODEX_API: OAuthApiPolicy = OAuthApiPolicy {
    api_origin: "https://chatgpt.com",
    refresh: &CODEX_REFRESH,
};

/// OpenAI's custom device protocol; its endpoints and client are protocol-fixed in
/// the flow, and the stored row is refreshed by `CODEX_API.refresh`.
pub static CODEX_DEVICE: DevicePolicy = DevicePolicy {
    label: "ChatGPT Plus/Pro subscription (device code)",
    login_name: "ChatGPT Plus/Pro (Codex Subscription)",
    flow: DeviceFlow::OpenAiCodex,
};

const XAI_CLIENT: ClientId = ClientId::Literal("b1a00492-073a-47ea-816f-4c329264a828");
const XAI_PROJECTION: TokenProjection = TokenProjection {
    access: "/access_token",
    refresh: "/refresh_token",
    expires_in: "/expires_in",
    skew_ms: 300_000,
    // The access-JWT `sub` and userinfo fill identity (`IdentityHook::XaiUserinfo`).
    account_id: None,
    email: None,
    project_id: None,
    org_id: None,
    org_name: None,
};
/// Device-code polls; the endpoint is discovered and validated once per login.
pub static XAI_TOKEN_REQUEST: TokenRequestPolicy = TokenRequestPolicy {
    url: TokenUrl::XaiOidc,
    encoding: BodyEncoding::Form,
    timeout_ms: 30_000,
    headers: &[("Accept", Tpl::Lit("application/json"))],
    extra_params: &[],
};
/// Refresh sends no extra header; the endpoint is rediscovered per refresh.
const XAI_REFRESH_REQUEST: TokenRequestPolicy = TokenRequestPolicy {
    url: TokenUrl::XaiOidc,
    encoding: BodyEncoding::Form,
    timeout_ms: 30_000,
    headers: &[],
    extra_params: &[],
};
pub static XAI_RFC8628: Rfc8628Policy = Rfc8628Policy {
    client_id: XAI_CLIENT,
    scope: "openid profile email offline_access grok-cli:access api:access",
    device_url: "https://auth.x.ai/oauth2/device/code",
    device_headers: &[("Accept", Tpl::Lit("application/json"))],
    encoding: BodyEncoding::Form,
    timeout_ms: 30_000,
    token: &XAI_TOKEN_REQUEST,
    projection: &XAI_PROJECTION,
    identity_hook: Some(IdentityHook::XaiUserinfo),
    instructions: "Enter code: {user_code}",
};
pub static XAI_DEVICE: DevicePolicy = DevicePolicy {
    label: "xAI Grok subscription (device code)",
    login_name: "xAI Grok OAuth (SuperGrok or X Premium+)",
    flow: DeviceFlow::Rfc8628(&XAI_RFC8628),
};
pub static XAI_REFRESH: RefreshPolicy = RefreshPolicy {
    client_id: XAI_CLIENT,
    request: &XAI_REFRESH_REQUEST,
    projection: &XAI_PROJECTION,
    identity_hook: Some(IdentityHook::XaiUserinfo),
};
pub static XAI_API: OAuthApiPolicy = OAuthApiPolicy {
    api_origin: "https://api.x.ai",
    refresh: &XAI_REFRESH,
};

pub fn org_scoped_identity(provider: &str) -> bool {
    crate::adapters::cli::auth::providers::lookup(provider)
        .is_some_and(|meta| meta.org_scoped_identity)
}

/// Instrumented callers may replace pinned ports, never change fallback policy.
pub fn callback_port(policy: &OAuthCodePolicy) -> u16 {
    #[cfg(feature = "test-instrumentation")]
    if !policy.port_fallback
        && let Some(port) =
            crate::infrastructure::utils::env_var_trimmed("RUSTAIN_TEST_AUTH_CALLBACK_PORT")
                .and_then(|port| port.parse::<u16>().ok())
    {
        return port;
    }
    policy.port
}

pub fn claude_code_version() -> String {
    if let Some(version) =
        crate::infrastructure::utils::env_var_trimmed("RUSTAIN_CLAUDE_CODE_VERSION")
    {
        return version;
    }
    #[cfg(feature = "anthropic")]
    if let Some(version) = crate::adapters::anthropic::claude_code::adopted_version() {
        return version;
    }
    "2.1.280".to_owned()
}

/// Rebase only in instrumented test builds; resolve at each use, never cache.
pub fn endpoint(value: &str) -> String {
    #[cfg(feature = "test-instrumentation")]
    if let Some(origin) = crate::infrastructure::utils::env_var_trimmed("RUSTAIN_TEST_AUTH_ORIGIN")
    {
        if let (Ok(mut url), Ok(origin)) = (url::Url::parse(value), url::Url::parse(&origin)) {
            if matches!(
                origin.host_str(),
                Some("127.0.0.1" | "localhost" | "[::1]" | "::1")
            ) {
                let _ = url.set_scheme(origin.scheme());
                let _ = url.set_host(origin.host_str());
                let _ = url.set_port(origin.port());
                return url.into();
            }
        }
    }
    value.to_owned()
}

pub fn oauth_api(provider: &str) -> Option<&'static OAuthApiPolicy> {
    crate::adapters::cli::auth::providers::lookup(provider).and_then(|meta| meta.oauth_api)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(feature = "test-instrumentation"))]
    #[test]
    #[serial_test::serial]
    fn shipped_build_ignores_test_origin_override() {
        struct Restore(Option<std::ffi::OsString>);
        impl Drop for Restore {
            fn drop(&mut self) {
                // SAFETY: the unkeyed serial guard outlives this restoration.
                unsafe {
                    match &self.0 {
                        Some(value) => std::env::set_var("RUSTAIN_TEST_AUTH_ORIGIN", value),
                        None => std::env::remove_var("RUSTAIN_TEST_AUTH_ORIGIN"),
                    }
                }
            }
        }
        let _restore = Restore(std::env::var_os("RUSTAIN_TEST_AUTH_ORIGIN"));
        // SAFETY: all environment-mutating callers hold the unkeyed serial lock.
        unsafe {
            std::env::set_var("RUSTAIN_TEST_AUTH_ORIGIN", "http://127.0.0.1:9");
        }
        assert_eq!(
            endpoint("https://api.anthropic.com/v1/oauth/token"),
            "https://api.anthropic.com/v1/oauth/token"
        );
    }

    #[cfg(not(feature = "test-instrumentation"))]
    #[test]
    #[serial_test::serial]
    fn shipped_build_ignores_test_callback_override() {
        struct Restore(Option<std::ffi::OsString>);
        impl Drop for Restore {
            fn drop(&mut self) {
                // SAFETY: the serial guard outlives this restoration.
                unsafe {
                    match &self.0 {
                        Some(value) => std::env::set_var("RUSTAIN_TEST_AUTH_CALLBACK_PORT", value),
                        None => std::env::remove_var("RUSTAIN_TEST_AUTH_CALLBACK_PORT"),
                    }
                }
            }
        }
        let _restore = Restore(std::env::var_os("RUSTAIN_TEST_AUTH_CALLBACK_PORT"));
        // SAFETY: environment mutation is serialized with all other callers.
        unsafe {
            std::env::set_var("RUSTAIN_TEST_AUTH_CALLBACK_PORT", "1");
        }
        assert_eq!(callback_port(&CODEX_OAUTH), 1455);
        assert_eq!(callback_port(&ANTHROPIC_OAUTH), 54545);
    }

    #[test]
    fn policies_keep_https_endpoints_and_paste_capable_absolute_callbacks() {
        fn https(url: TokenUrl) {
            match url {
                TokenUrl::Static(url) => {
                    assert_eq!(url::Url::parse(url).unwrap().scheme(), "https");
                }
                // Discovered per use; its own validator admits only https `*.x.ai`.
                TokenUrl::XaiOidc => {}
            }
        }
        for provider in crate::adapters::cli::auth::providers::all_providers() {
            for method in provider.auth_methods {
                match method {
                    AuthMethod::OAuthCode(policy) => {
                        assert_eq!(
                            url::Url::parse(policy.authorize_url).unwrap().scheme(),
                            "https"
                        );
                        https(policy.exchange.url);
                        assert!(policy.callback_path.starts_with('/'));
                        assert!(policy.paste);
                    }
                    AuthMethod::Device(policy) => {
                        if let DeviceFlow::Rfc8628(rfc) = policy.flow {
                            assert_eq!(url::Url::parse(rfc.device_url).unwrap().scheme(), "https");
                            https(rfc.token.url);
                            assert!(rfc.instructions.contains("{user_code}"));
                        }
                    }
                    AuthMethod::ApiKey => {}
                }
            }
            if let Some(api) = provider.oauth_api {
                assert_eq!(url::Url::parse(api.api_origin).unwrap().scheme(), "https");
                https(api.refresh.request.url);
            }
        }
    }
}
