//! Static provider protocol rules, adapted from oh-my-pi's Anthropic policy.
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
    url: "https://api.anthropic.com/v1/oauth/token",
    encoding: BodyEncoding::Json,
    headers: &[],
    extra_params: &[("state", Tpl::State)],
};
const REFRESH_REQUEST: TokenRequestPolicy = TokenRequestPolicy {
    url: "https://api.anthropic.com/v1/oauth/token",
    encoding: BodyEncoding::Json,
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

pub const fn org_scoped_identity(provider: &str) -> bool {
    matches!(provider.as_bytes(), b"anthropic")
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
    (provider == "anthropic").then_some(&ANTHROPIC_API)
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

    #[test]
    fn policies_keep_https_endpoints_and_paste_capable_absolute_callbacks() {
        for provider in crate::adapters::cli::auth::providers::all_providers() {
            for method in provider.auth_methods {
                if let AuthMethod::OAuthCode(policy) = method {
                    assert_eq!(
                        url::Url::parse(policy.authorize_url).unwrap().scheme(),
                        "https"
                    );
                    assert_eq!(
                        url::Url::parse(policy.exchange.url).unwrap().scheme(),
                        "https"
                    );
                    assert!(policy.callback_path.starts_with('/'));
                    assert!(policy.paste);
                }
            }
            if let Some(api) = provider.oauth_api {
                assert_eq!(url::Url::parse(api.api_origin).unwrap().scheme(), "https");
                assert_eq!(
                    url::Url::parse(api.refresh.request.url).unwrap().scheme(),
                    "https"
                );
            }
        }
    }
}
